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

pub struct Machine {
    pub cpu: Cpu,
    pub ram: Vec<u8>,
}

impl Machine {
    /// Create a machine with `ram_bytes` of physical memory.
    pub fn new(ram_bytes: usize) -> Self {
        let mut cpu = Cpu::new();
        cpu.machine_mode = true;
        Machine { cpu, ram: vec![0u8; ram_bytes] }
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

            match self.cpu.step(&mut self.ram) {
                StepResult::Ok => continue,
                StepResult::Hlt => {
                    // Idle. If a vector is already pending and enabled, take it.
                    if self.cpu.flag(IF) {
                        if self.cpu.dev.apic.pending_vector.is_some() {
                            self.service_pending_irq();
                            continue;
                        }
                        // Fast-forward an armed one-shot/periodic timer to wake.
                        if self.cpu.dev.apic.expire().is_some() {
                            self.service_pending_irq();
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

    /// If the APIC has a pending vector and interrupts are enabled, deliver it.
    fn service_pending_irq(&mut self) {
        if self.cpu.flag(IF) {
            if let Some(vec) = self.cpu.dev.apic.pending_vector.take() {
                self.cpu.deliver_interrupt(&mut self.ram, vec, None);
            }
        }
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
