//! The full machine: physical RAM + a CPU in long mode + the device set, with
//! a run loop that retires instructions, advances the APIC timer, and delivers
//! interrupts and page faults through the guest IDT.
//!
//! Unlike qemu-wasm/v86 we do not emulate a PC from the reset vector. We **boot
//! directly in long mode**: [`Machine::boot_long_mode`] builds an identity map,
//! an IDT, and the control registers a freshly-paged 64-bit kernel expects, then
//! [`Machine::run`] executes from a given entry point. This is the whole reason
//! the emulator is small — no real mode, no BIOS, no chipset bring-up.

extern crate alloc;
use alloc::vec;
use alloc::vec::Vec;

use crate::mmu;
use crate::{Cpu, StepResult, IF};

// Physical layout the bootstrap builds. All well below where a kernel image or
// the stack would sit.
const PML4_ADDR: u64 = 0x1000;
const PDPT_ADDR: u64 = 0x2000;
const PD0_ADDR: u64 = 0x3000; // four PD tables (0x3000..0x7000) → identity-map 4 GiB
const IDT_ADDR: u64 = 0x1_0000;

// Page-table entry flags.
const P: u64 = 1 << 0;
const RW: u64 = 1 << 1;
const US: u64 = 1 << 2;
const PS: u64 = 1 << 7;

/// Why the run loop stopped.
#[derive(Debug, PartialEq, Eq)]
pub enum RunStop {
    /// `hlt` with no way to wake (interrupts disabled, or no timer armed).
    Halted,
    /// Hit the instruction budget.
    MaxSteps,
    /// An unimplemented opcode — the trace-driven signal for what to add next.
    Unknown { rip: u64, byte: u8 },
    /// A page fault with no usable IDT handler (double fault territory).
    UnhandledFault { addr: u64 },
    /// `syscall` reached the host trap (only in non-machine mode).
    Syscall,
}

/// Physical-frame address mask (bits 51:12).
const ADDR_MASK: u64 = 0x000F_FFFF_FFFF_F000;

pub struct Machine {
    pub cpu: Cpu,
    pub ram: Vec<u8>,
    /// Bump pointer for page-table frame allocation (used by `boot_kernel`).
    pt_next: u64,
    /// Optional ring buffer of the most recent instruction pointers (debug).
    pub trace_on: bool,
    pub trace_log: Vec<u64>,
    /// Counters (debug): timer/IRQ deliveries and `hlt` idles serviced.
    pub irqs_delivered: u64,
    pub hlts: u64,
    /// Debug watchpoints: rips to flag the first time they execute.
    pub watch: Vec<u64>,
    pub watch_hits: Vec<u64>,
}

impl Machine {
    /// Create a machine with `ram_bytes` of physical memory.
    pub fn new(ram_bytes: usize) -> Self {
        let mut cpu = Cpu::new();
        cpu.machine_mode = true;
        Machine {
            cpu,
            ram: vec![0u8; ram_bytes],
            pt_next: 0,
            trace_on: false,
            trace_log: Vec::new(),
            irqs_delivered: 0,
            hlts: 0,
            watch: Vec::new(),
            watch_hits: Vec::new(),
        }
    }

    /// Write bytes into physical memory (e.g. a kernel image or test program).
    pub fn write_phys(&mut self, phys: u64, bytes: &[u8]) {
        let a = phys as usize;
        self.ram[a..a + bytes.len()].copy_from_slice(bytes);
    }
    fn put64(&mut self, phys: u64, val: u64) {
        self.write_phys(phys, &val.to_le_bytes());
    }

    /// Build identity page tables (4 GiB via 2 MiB pages), an IDT, and the
    /// control registers for an active long mode. After this the CPU is paging,
    /// in ring 0, ready to execute at `entry` with `rsp` as the stack.
    pub fn boot_long_mode(&mut self, entry: u64, rsp: u64) {
        // PML4[0] → PDPT.
        self.put64(PML4_ADDR, PDPT_ADDR | P | RW | US);
        // PDPT[0..4] → four PDs; PD_k[i] maps (k GiB + i*2 MiB) as a 2 MiB page.
        for k in 0..4u64 {
            let pd = PD0_ADDR + k * 0x1000;
            self.put64(PDPT_ADDR + k * 8, pd | P | RW | US);
            for i in 0..512u64 {
                let frame = k * 0x4000_0000 + i * 0x20_0000;
                self.put64(pd + i * 8, frame | P | RW | US | PS);
            }
        }
        self.cpu.paging.cr3 = PML4_ADDR;
        self.cpu.paging.cr4 = mmu::CR4_PAE;
        self.cpu.paging.efer = mmu::EFER_LME | mmu::EFER_LMA | mmu::EFER_NXE;
        self.cpu.paging.cr0 = mmu::CR0_PG | 1 /* PE */;
        self.cpu.idtr_base = IDT_ADDR;
        self.cpu.idtr_limit = 256 * 16 - 1;
        self.cpu.cpl = 0;
        self.cpu.rip = entry;
        self.cpu.regs[crate::RSP] = rsp;
        // A sane default RSP0 for ring3→ring0 interrupts (used once user mode
        // exists); harmless otherwise.
        self.cpu.tss_rsp0 = rsp;
    }

    /// Install a 64-bit interrupt gate: `vector` → `handler` (ring-0 code
    /// selector 0x08, present, DPL 0, type 0xE).
    pub fn set_idt_gate(&mut self, vector: u8, handler: u64) {
        let desc = IDT_ADDR + vector as u64 * 16;
        let selector: u64 = 0x08;
        let type_attr: u64 = 0x8E; // present | DPL0 | interrupt gate
        let lo = (handler & 0xFFFF)
            | (selector << 16)
            | (type_attr << 40)
            | (((handler >> 16) & 0xFFFF) << 48);
        let hi = (handler >> 32) & 0xFFFF_FFFF;
        self.put64(desc, lo);
        self.put64(desc + 8, hi);
    }

    fn get64(&self, phys: u64) -> u64 {
        let a = phys as usize;
        let mut v = [0u8; 8];
        v.copy_from_slice(&self.ram[a..a + 8]);
        u64::from_le_bytes(v)
    }

    /// Allocate (and zero) a 4 KiB page-table frame from the bump area.
    fn alloc_frame(&mut self) -> u64 {
        let f = self.pt_next;
        self.pt_next += 0x1000;
        for b in &mut self.ram[f as usize..f as usize + 0x1000] {
            *b = 0;
        }
        f
    }

    /// Return the next-level table's physical base for `idx` in `table`,
    /// creating it (present + writable) if absent.
    fn ensure_table(&mut self, table: u64, idx: u64) -> u64 {
        let e = self.get64(table + idx * 8);
        if e & 1 != 0 {
            return e & ADDR_MASK;
        }
        let f = self.alloc_frame();
        self.put64(table + idx * 8, f | P | RW);
        f
    }

    /// Map one page (4 KiB, or 2 MiB if `large`) `virt → phys` in the tree
    /// rooted at `pml4`. Supervisor, writable, executable (no NX).
    fn map_page(&mut self, pml4: u64, virt: u64, phys: u64, large: bool) {
        let pml4i = (virt >> 39) & 0x1FF;
        let pdpti = (virt >> 30) & 0x1FF;
        let pdi = (virt >> 21) & 0x1FF;
        let pti = (virt >> 12) & 0x1FF;
        let pdpt = self.ensure_table(pml4, pml4i);
        let pd = self.ensure_table(pdpt, pdpti);
        if large {
            self.put64(pd + pdi * 8, (phys & !0x1F_FFFF) | P | RW | PS);
            return;
        }
        let pt = self.ensure_table(pd, pdi);
        self.put64(pt + pti * 8, (phys & ADDR_MASK) | P | RW);
    }

    fn map_range(&mut self, pml4: u64, virt: u64, phys: u64, len: u64, large: bool) {
        let step = if large { 0x20_0000 } else { 0x1000 };
        let mut o = 0;
        while o < len {
            self.map_page(pml4, virt + o, phys + o, large);
            o += step;
        }
    }

    /// Boot the real kernel: load + relocate it high-half, build the page
    /// tables and the `bootloader_api` `BootInfo`, and enter `_start` (BootInfo
    /// pointer in RDI) exactly as the `bootloader` crate would. This replaces
    /// the BIOS/real-mode bring-up entirely — we hand the kernel the paged,
    /// long-mode environment it expects directly.
    pub fn boot_kernel(&mut self, image: &[u8]) -> Result<(), crate::elf::ElfError> {
        use crate::bootinfo::{self, HandoffParams, Region};
        let elf = crate::elf::Elf::parse(image)?;

        const KIMG_PHYS: u64 = 0x80_0000; // 8 MiB
        const KERNEL_VIRT: u64 = 0xFFFF_8000_0000_0000;
        const PHYS_OFFSET: u64 = 0xFFFF_FF00_0000_0000;
        const STACK_VIRT: u64 = 0xFFFF_8000_4000_0000;
        const STACK_LEN: u64 = 256 * 1024;
        const BOOTINFO_VIRT: u64 = 0xFFFF_8000_5000_0000;
        const REGIONS_VIRT: u64 = 0xFFFF_8000_5001_0000;

        let mut span_end = 0u64;
        for s in elf.segments.iter() {
            span_end = span_end.max(s.vaddr + s.mem_size as u64);
        }
        let img_span = (span_end + 0xFFF) & !0xFFF;
        let stack_phys = (KIMG_PHYS + img_span + 0xFFF) & !0xFFF;
        let bootinfo_phys = stack_phys + STACK_LEN;
        let regions_phys = bootinfo_phys + 0x1000;
        let high_water = (regions_phys + 0x1000 + 0x1F_FFFF) & !0x1F_FFFF;
        let ramsize = self.ram.len() as u64;

        // 1. Load segments at their physical home, then apply PIE relocations.
        for s in elf.segments.iter() {
            let bytes = elf.segment_bytes(s);
            self.write_phys(KIMG_PHYS + s.vaddr, bytes);
        }
        elf.apply_relative_relocs(KERNEL_VIRT, |off, val| {
            self.put64(KIMG_PHYS + off, val);
        });

        // 2. Page tables: PT frames live at 1 MiB (below the kernel image).
        self.pt_next = 0x10_0000;
        let pml4 = self.alloc_frame();
        self.map_range(pml4, KERNEL_VIRT, KIMG_PHYS, img_span, false);
        self.map_range(pml4, STACK_VIRT, stack_phys, STACK_LEN, false);
        self.map_page(pml4, BOOTINFO_VIRT, bootinfo_phys, false);
        self.map_page(pml4, REGIONS_VIRT, regions_phys, false);
        // Physical-memory window: the bootloader maps the whole physical address
        // space (RAM *and* MMIO holes such as the Local APIC at 0xFEE00000), so
        // map the low 4 GiB. Accesses to the APIC page are intercepted as MMIO;
        // other non-RAM frames simply fault if the kernel ever touches them.
        let window = core::cmp::max(ramsize, 0x1_0000_0000);
        self.map_range(pml4, PHYS_OFFSET, 0, window, true);

        // 3. Control registers for an active long mode.
        self.cpu.paging.cr3 = pml4;
        self.cpu.paging.cr4 = mmu::CR4_PAE;
        self.cpu.paging.efer = mmu::EFER_LME | mmu::EFER_LMA | mmu::EFER_NXE;
        self.cpu.paging.cr0 = mmu::CR0_PG | 1; // PG | PE
        self.cpu.cpl = 0;

        // 4. BootInfo: reserve everything we built; the rest is usable RAM.
        let regions = [
            Region { start: 0x1000, end: high_water, usable: false },
            Region { start: high_water, end: ramsize, usable: true },
        ];
        let params = HandoffParams {
            physical_memory_offset: PHYS_OFFSET,
            kernel_image_offset: KERNEL_VIRT,
            kernel_addr: KIMG_PHYS,
            kernel_len: image.len() as u64,
            kernel_stack_bottom: STACK_VIRT,
            kernel_stack_len: STACK_LEN,
            rsdp_addr: None,
            regions_vaddr: REGIONS_VIRT,
        };
        let (bi_bytes, reg_bytes) = bootinfo::build(&params, &regions);
        self.write_phys(bootinfo_phys, &bi_bytes);
        self.write_phys(regions_phys, &reg_bytes);

        // 5. Enter _start(boot_info): pointer in RDI, a 16-byte-aligned stack.
        self.cpu.regs[crate::RDI] = BOOTINFO_VIRT;
        let top = (STACK_VIRT + STACK_LEN) & !0xF;
        self.cpu.regs[crate::RSP] = top - 8;
        self.cpu.tss_rsp0 = top;
        self.cpu.rip = KERNEL_VIRT + elf.entry;
        Ok(())
    }

    /// Load an ELF64 image's `PT_LOAD` segments into physical memory (by their
    /// physical address) and return the entry point. Segments that fall outside
    /// physical RAM are skipped (the caller sizes RAM for the image).
    pub fn load_elf(&mut self, image: &[u8]) -> Result<u64, crate::elf::ElfError> {
        let e = crate::elf::Elf::parse(image)?;
        for seg in e.segments.iter() {
            let end = seg.paddr as usize + seg.mem_size;
            if end > self.ram.len() {
                continue; // out of range for this RAM size
            }
            let bytes = e.segment_bytes(seg);
            self.write_phys(seg.paddr, bytes);
            // BSS (mem_size > file_size) is already zero from RAM init.
        }
        Ok(e.entry)
    }

    /// Drain the UART transmit buffer (what the guest has printed).
    pub fn take_uart_output(&mut self) -> Vec<u8> {
        self.cpu.dev.uart.tx.drain(..).collect()
    }

    /// Execute up to `max_steps` instructions, servicing the APIC timer, device
    /// interrupts, and page faults along the way.
    pub fn run(&mut self, max_steps: usize) -> RunStop {
        for _ in 0..max_steps {
            // Advance the timer one instruction's worth; a fired vector is
            // injected before the next instruction if interrupts are enabled.
            self.cpu.dev.apic.tick(1);
            self.service_pending_irq();

            if self.trace_on {
                if self.trace_log.len() >= 64 {
                    self.trace_log.remove(0);
                }
                self.trace_log.push(self.cpu.rip);
            }
            if !self.watch.is_empty() {
                let r = self.cpu.rip;
                if self.watch.contains(&r) && !self.watch_hits.contains(&r) {
                    self.watch_hits.push(r);
                }
            }

            match self.cpu.step(&mut self.ram) {
                StepResult::Ok => continue,
                StepResult::Hlt => {
                    self.hlts += 1;
                    // Idle. Deliver an eligible pending interrupt, or fast-forward
                    // the armed timer to its next fire and deliver that.
                    if self.cpu.flag(IF) {
                        if self.service_pending_irq() {
                            continue;
                        }
                        if self.cpu.dev.apic.expire().is_some() && self.service_pending_irq() {
                            continue;
                        }
                    }
                    return RunStop::Halted;
                }
                StepResult::Fault { addr } => {
                    // Deliver #PF (vector 14) with a best-effort error code.
                    self.cpu.cr2 = addr;
                    let err = mmu::PageFault::P; // present-ish; refined later
                    if !self.cpu.deliver_interrupt(&mut self.ram, 14, Some(err as u64)) {
                        return RunStop::UnhandledFault { addr };
                    }
                }
                StepResult::Unknown { rip, byte } => return RunStop::Unknown { rip, byte },
                StepResult::Syscall => return RunStop::Syscall,
                StepResult::Halt => return RunStop::Halted,
                StepResult::Import { .. } => return RunStop::Halted,
            }
        }
        RunStop::MaxSteps
    }

    /// Inject the highest-priority pending APIC interrupt if interrupts are
    /// enabled (RFLAGS.IF) and its priority class outranks the current IRQL
    /// (`v >> 4 > CR8`) — the x86-64 hardware delivery rule. Returns whether one
    /// was delivered.
    fn service_pending_irq(&mut self) -> bool {
        if !self.cpu.flag(IF) {
            return false;
        }
        if let Some(vec) = self.cpu.dev.apic.highest_pending() {
            if (vec >> 4) as u64 > self.cpu.cr8 {
                self.cpu.dev.apic.ack(vec);
                self.cpu.deliver_interrupt(&mut self.ram, vec, None);
                self.irqs_delivered += 1;
                return true;
            }
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // End-to-end long-mode boot: paging on, port I/O to the UART, APIC MMIO to
    // arm the timer, sti/hlt, an interrupt delivered through the IDT to a
    // handler that prints and iretq's back. Exercises every M1–M4 subsystem.
    #[test]
    fn boots_long_mode_prints_and_takes_timer_interrupt() {
        const ENTRY: u64 = 0x40_0000;
        const HANDLER: u64 = 0x41_0000;
        const STACK: u64 = 0x3F_0000;

        // mov edx, 0x3F8  (UART base)
        let uart_dx = [0xBA, 0xF8, 0x03, 0x00, 0x00];
        // out dx, al
        let out = [0xEE];

        let mut prog: Vec<u8> = Vec::new();
        // print 'O','K'
        prog.extend_from_slice(&uart_dx);
        prog.extend_from_slice(&[0xB0, b'O']);
        prog.extend_from_slice(&out);
        prog.extend_from_slice(&[0xB0, b'K']);
        prog.extend_from_slice(&out);
        // rbx = APIC base 0xFEE00000
        prog.extend_from_slice(&[0x48, 0xBB, 0x00, 0x00, 0xE0, 0xFE, 0x00, 0x00, 0x00, 0x00]);
        // mov dword [rbx+0x3E0], 0xB   (divide = 1)
        prog.extend_from_slice(&[0xC7, 0x83, 0xE0, 0x03, 0x00, 0x00, 0x0B, 0x00, 0x00, 0x00]);
        // mov dword [rbx+0x320], 0xD1  (LVT timer: vector 0xD1, one-shot, unmasked)
        prog.extend_from_slice(&[0xC7, 0x83, 0x20, 0x03, 0x00, 0x00, 0xD1, 0x00, 0x00, 0x00]);
        // mov dword [rbx+0x380], 0x14  (initial count = 20)
        prog.extend_from_slice(&[0xC7, 0x83, 0x80, 0x03, 0x00, 0x00, 0x14, 0x00, 0x00, 0x00]);
        // sti ; hlt   (enable interrupts, idle → timer fires)
        prog.extend_from_slice(&[0xFB, 0xF4]);
        // after the handler iretq's back: print 'D','!'
        prog.extend_from_slice(&uart_dx);
        prog.extend_from_slice(&[0xB0, b'D']);
        prog.extend_from_slice(&out);
        prog.extend_from_slice(&[0xB0, b'!']);
        prog.extend_from_slice(&out);
        // hlt for good (timer is one-shot, now stopped → run loop halts)
        prog.extend_from_slice(&[0xF4]);

        // Handler: print 'T', EOI, iretq.
        let mut handler: Vec<u8> = Vec::new();
        handler.extend_from_slice(&uart_dx);
        handler.extend_from_slice(&[0xB0, b'T']);
        handler.extend_from_slice(&out);
        // rbx = APIC base, then mov dword [rbx+0xB0], 0  (EOI)
        handler.extend_from_slice(&[0x48, 0xBB, 0x00, 0x00, 0xE0, 0xFE, 0x00, 0x00, 0x00, 0x00]);
        handler.extend_from_slice(&[0xC7, 0x83, 0xB0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]);
        // iretq (REX.W + CF)
        handler.extend_from_slice(&[0x48, 0xCF]);

        let mut m = Machine::new(0x100_0000); // 16 MiB
        m.boot_long_mode(ENTRY, STACK);
        m.set_idt_gate(0xD1, HANDLER);
        m.write_phys(ENTRY, &prog);
        m.write_phys(HANDLER, &handler);

        let stop = m.run(10_000);
        let out = m.take_uart_output();
        assert_eq!(
            core::str::from_utf8(&out).unwrap(),
            "OKTD!",
            "stop={:?} cr2={:#x}",
            stop,
            m.cpu.cr2
        );
        assert_eq!(stop, RunStop::Halted);
    }

    // syscall/sysret round-trip in machine mode: user code at ring 3 issues a
    // syscall, the kernel handler (at LSTAR) runs in ring 0 and sysrets back.
    #[test]
    fn syscall_sysret_round_trip() {
        const USER: u64 = 0x40_0000;
        const KERNEL: u64 = 0x42_0000;
        const STACK: u64 = 0x3F_0000;

        let mut m = Machine::new(0x100_0000);
        m.boot_long_mode(USER, STACK);
        m.cpu.lstar = KERNEL;
        m.cpu.cpl = 3; // start in user mode

        // user: mov eax, 0x1234 ; syscall ; hlt
        let user = [
            0xB8, 0x34, 0x12, 0x00, 0x00, // mov eax, 0x1234
            0x0F, 0x05, // syscall
            0xF4, // hlt
        ];
        // kernel: mov ebx, 0x99 ; sysret   (proves we entered ring0 and returned)
        let kernel = [
            0xBB, 0x99, 0x00, 0x00, 0x00, // mov ebx, 0x99
            0x0F, 0x07, // sysret
        ];
        m.write_phys(USER, &user);
        m.write_phys(KERNEL, &kernel);

        let stop = m.run(100);
        assert_eq!(stop, RunStop::Halted);
        assert_eq!(m.cpu.regs[crate::RAX] & 0xFFFF, 0x1234);
        assert_eq!(m.cpu.regs[crate::RBX], 0x99); // kernel ran
        assert_eq!(m.cpu.cpl, 3); // sysret returned to user
    }

    // ELF path: load a freestanding long-mode image and run its entry. The
    // image prints 'E' to the UART then halts.
    #[test]
    fn loads_and_runs_elf_image() {
        // mov edx,0x3F8 ; mov al,'E' ; out dx,al ; hlt
        let code = [
            0xBA, 0xF8, 0x03, 0x00, 0x00, 0xB0, b'E', 0xEE, 0xF4,
        ];
        let img = make_elf(0x40_0000, 0x40_0000, &code);

        let mut m = Machine::new(0x100_0000);
        let entry = m.load_elf(&img).expect("parse elf");
        assert_eq!(entry, 0x40_0000);
        m.boot_long_mode(entry, 0x3F_0000);
        let stop = m.run(1000);
        assert_eq!(stop, RunStop::Halted);
        assert_eq!(m.take_uart_output(), b"E");
    }

    // Minimal ELF64 builder mirroring elf::tests::minimal_elf.
    fn make_elf(entry: u64, paddr: u64, payload: &[u8]) -> Vec<u8> {
        let (ehsize, phentsize) = (64usize, 56usize);
        let data_off = ehsize + phentsize;
        let mut b = vec![0u8; data_off + payload.len()];
        b[0..4].copy_from_slice(b"\x7fELF");
        b[4] = 2;
        b[5] = 1;
        b[6] = 1;
        b[16..18].copy_from_slice(&2u16.to_le_bytes());
        b[18..20].copy_from_slice(&0x3Eu16.to_le_bytes());
        b[24..32].copy_from_slice(&entry.to_le_bytes());
        b[32..40].copy_from_slice(&(ehsize as u64).to_le_bytes());
        b[54..56].copy_from_slice(&(phentsize as u16).to_le_bytes());
        b[56..58].copy_from_slice(&1u16.to_le_bytes());
        let ph = ehsize;
        b[ph..ph + 4].copy_from_slice(&1u32.to_le_bytes()); // PT_LOAD
        b[ph + 4..ph + 8].copy_from_slice(&5u32.to_le_bytes());
        b[ph + 8..ph + 16].copy_from_slice(&(data_off as u64).to_le_bytes());
        b[ph + 16..ph + 24].copy_from_slice(&paddr.to_le_bytes());
        b[ph + 24..ph + 32].copy_from_slice(&paddr.to_le_bytes());
        b[ph + 32..ph + 40].copy_from_slice(&(payload.len() as u64).to_le_bytes());
        b[ph + 40..ph + 48].copy_from_slice(&(payload.len() as u64).to_le_bytes());
        b[data_off..].copy_from_slice(payload);
        b
    }

    // Paging actually translates: map nothing extra, write through a higher-half
    // canonical address that is NOT identity-covered → expect an unhandled fault
    // (no IDT handler for #PF), proving translation isn't a passthrough.
    #[test]
    fn unmapped_high_address_faults() {
        const ENTRY: u64 = 0x40_0000;
        let mut m = Machine::new(0x40_0000 + 0x1000);
        m.boot_long_mode(ENTRY, 0x3F_0000);
        // mov rax, 0xFFFF_8000_0000_0000 ; mov [rax], 0  → access unmapped VA
        let prog = [
            0x48, 0xB8, 0x00, 0x00, 0x00, 0x00, 0x00, 0x80, 0xFF, 0xFF, // mov rax, imm64
            0x48, 0xC7, 0x00, 0x00, 0x00, 0x00, 0x00, // mov qword [rax], 0
        ];
        m.write_phys(ENTRY, &prog);
        let stop = m.run(100);
        // The PD only maps the first 4 GiB; 0xFFFF_8000_… is unmapped → #PF, and
        // with no handler installed the run loop reports it.
        assert!(matches!(stop, RunStop::UnhandledFault { .. }), "got {:?}", stop);
    }
}
