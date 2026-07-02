//! `x86emu` — a minimal x86-64 interpreter (Track B).
//!
//! The seed of running the real x86-64 PE binaries and drivers in the browser
//! without a platform emulator: this executes the *instruction stream* over a
//! flat linear-memory address space; later phases bridge `syscall` (ring 3) and
//! `ntoskrnl` export calls (ring 0) into the kernel's existing Rust services.
//! No MMU, no chipset, no device emulation — just a CPU.
//!
//! Decoding grows opcode-by-opcode, trace-driven: [`Cpu::step`] returns
//! [`StepResult::Unknown`] with the offending byte so the next opcode to
//! implement is obvious.
//!
//! Implemented so far (B0+B1 core): REX-prefixed reg/reg + reg/mem ALU
//! (`add`/`sub`/`mov`/`cmp`/`xor`), `mov r,imm`, `mov r/m,imm32`, the stack
//! (`push`/`pop`), and control flow (`call`/`ret`/`jmp`/`jcc`). ModRM addressing
//! covers register-direct, `[base]`, `[base+disp8/32]`, and SIB; RIP-relative is
//! approximate (needs full-instruction length — refined later). Enough to run
//! hand-assembled functions with calls and loops; grows toward `whoami`.
// `no_std` only for the wasm build (the browser target); native host builds
// (used for the test suite and tooling) link std so they need no panic handler
// or allocator. The wasm module supplies both.
#![cfg_attr(target_arch = "wasm32", no_std)]

extern crate alloc;

pub mod bootinfo;
pub mod devices;
#[cfg(target_arch = "wasm32")]
pub mod wasm;
pub mod elf;
pub mod machine;
pub mod mmu;
pub mod pe;

pub const RAX: usize = 0;
pub const RCX: usize = 1;
pub const RDX: usize = 2;
pub const RBX: usize = 3;
pub const RSP: usize = 4;
pub const RBP: usize = 5;
pub const RSI: usize = 6;
pub const RDI: usize = 7;

const CF: u64 = 1 << 0;
const PF: u64 = 1 << 2;
const ZF: u64 = 1 << 6;
const SF: u64 = 1 << 7;
const IF: u64 = 1 << 9; // interrupt-enable flag
const DF: u64 = 1 << 10; // direction flag (string ops)
const OF: u64 = 1 << 11;
const AC: u64 = 1 << 18; // alignment-check / SMAP access (stac/clac)

/// Return address that means "the program returned to its entry caller" — the
/// interpreter stops (mirrors the native loader's NtTerminateThread return stub).
pub const HALT_ADDR: u64 = u64::MAX;

/// IAT slots are bound to `IMPORT_BASE + index*8`. A call/jmp to such an address
/// traps to the host as [`StepResult::Import`]. The region sits *inside* the
/// flat memory buffer (real, zeroed storage) so that **data** imports — IAT
/// entries the program dereferences as variables rather than calls (e.g. the
/// CRT's `_commode`/`_fmode`) — read/write harmlessly instead of faulting, while
/// **function** imports are still trapped by address before any fetch. The host
/// must size its buffer to cover `[IMPORT_BASE, IMPORT_BASE + IMPORT_MAX*8)`.
pub const IMPORT_BASE: u64 = 0x0200_0000; // 32 MiB
/// Max distinct imports (bounds the trap range; 64 KiB of slots).
pub const IMPORT_MAX: u64 = 8192;

#[derive(Clone)]
pub struct Cpu {
    pub regs: [u64; 16],
    pub rip: u64,
    pub rflags: u64,
    /// GS/FS segment bases. Windows user code reaches the TEB via `gs:[...]`
    /// (gs:[0x30]=self, gs:[0x60]=PEB, gs:[0x58]=TLS array). The host points
    /// `gs_base` at a TEB it builds in the interpreter's memory.
    pub gs_base: u64,
    pub fs_base: u64,
    /// Transient: the active segment base for the current instruction's memory
    /// operands (0 unless a 0x64/0x65 prefix selected FS/GS). Reset each step.
    seg_base: u64,
    /// SSE/XMM registers (128-bit). The MSVC CRT uses these to zero/copy stack
    /// buffers (`xorps`/`movaps`/`movdqa`).
    pub xmm: [u128; 16],

    // --- full-machine (long-mode) state -----------------------------------
    // Below this point the fields are only meaningful when `machine_mode` is
    // set: the seed's usermode interpreter (flat memory, host-serviced
    // syscalls, IAT import traps) leaves them at their defaults and behaves
    // exactly as before.
    /// When true, run as a full machine: memory accesses translate through the
    /// guest page tables (`paging`), `syscall`/`sysret` are architectural, and
    /// IAT import traps are disabled. When false (default), behave as the
    /// usermode interpreter the seed shipped.
    pub machine_mode: bool,
    /// Paging control registers (CR0/CR3/CR4) + EFER, consulted by the MMU.
    pub paging: mmu::Paging,
    /// CR2 — the linear address of the most recent page fault.
    pub cr2: u64,
    /// CR8 — the task priority register, aliased to IRQL on x86-64. A pending
    /// interrupt vector `v` is delivered only when `v >> 4 > cr8`.
    pub cr8: u64,
    /// Current privilege level (0 = kernel, 3 = user). Drives U/S page checks
    /// and the syscall/sysret/interrupt ring transitions.
    pub cpl: u8,
    /// The emulated device set (UART/APIC/PS2). MMIO and port I/O route here.
    pub dev: devices::Devices,
    /// IDTR (interrupt descriptor table) base/limit — set by `lidt`.
    pub idtr_base: u64,
    pub idtr_limit: u16,
    /// GDTR base/limit — set by `lgdt`.
    pub gdtr_base: u64,
    pub gdtr_limit: u16,
    /// syscall MSRs: STAR (segment selectors), LSTAR (entry RIP), SFMASK
    /// (RFLAGS bits cleared on entry), and the KernelGSBase swapped by `swapgs`.
    pub star: u64,
    pub lstar: u64,
    pub sfmask: u64,
    pub kernel_gs_base: u64,
    /// Ring-0 stack pointer loaded on an interrupt that changes privilege
    /// (TSS.RSP0). Set by the machine when building the TSS.
    pub tss_rsp0: u64,
    /// Base of the live TSS (decoded from the GDT when `ltr` loads TR). On a
    /// privilege-raising interrupt the CPU loads RSP0 from `[tr_base + 4]` — the
    /// kernel updates that field in memory on every context switch, so we must
    /// read it dynamically, not cache a boot-time value.
    pub tr_base: u64,
    /// Retired-instruction counter — drives the APIC timer's countdown.
    pub icount: u64,
    /// Debug: when set, record (service-number-in-EAX, arg1-in-R10) at each
    /// `syscall` and (0xFFFF_FFFF, RAX) at each `sysret`, so a host tool can see
    /// the syscall/return sequence.
    pub trace_sys: bool,
    pub sys_log: alloc::vec::Vec<(u32, u64)>,
}

/// Low-`size`-bytes mask as a u128 (size 8 -> full 64-bit mask).
fn mask128(size: u8) -> u128 {
    if size >= 8 {
        u64::MAX as u128
    } else {
        (1u128 << (size as u32 * 8)) - 1
    }
}

/// Sign-extend the low `size` bytes of `v` to i64.
fn sign_ext(v: u64, size: u8) -> i64 {
    match size {
        1 => v as u8 as i8 as i64,
        2 => v as u16 as i16 as i64,
        4 => v as u32 as i32 as i64,
        _ => v as i64,
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum StepResult {
    Ok,
    Halt,
    /// Executed a `syscall` (rip already advanced past it). The caller inspects
    /// the registers (per our ABI: `eax` = service number, args in the integer
    /// regs), services it against the kernel, and resumes by calling `step`.
    Syscall,
    /// Called an imported function (the IAT slot held an import trap address).
    /// `index` is the import's IAT-order index (map it to a name via the PE
    /// import table). The return address is already on the stack; the caller
    /// services the import, sets the return value in `rax`, and resumes — the
    /// interpreter then executes the pending `ret`.
    Import { index: u32 },
    Unknown { rip: u64, byte: u8 },
    /// Out-of-bounds / unmapped memory access at `addr` — the program faulted.
    /// In machine mode the run loop turns this into a #PF through the IDT.
    Fault { addr: u64 },
    /// Executed `hlt`: the CPU idles until the next interrupt. The machine run
    /// loop services pending device interrupts and resumes.
    Hlt,
}

/// A decoded ModRM r/m: a register index or an effective memory address.
#[derive(Clone, Copy)]
enum Rm {
    Reg(usize),
    Mem(u64),
}

impl Default for Cpu {
    fn default() -> Self {
        Cpu {
            regs: [0; 16],
            rip: 0,
            rflags: 0,
            gs_base: 0,
            fs_base: 0,
            seg_base: 0,
            xmm: [0; 16],
            machine_mode: false,
            paging: mmu::Paging::default(),
            cr2: 0,
            cr8: 0,
            cpl: 0,
            dev: devices::Devices::new(),
            idtr_base: 0,
            idtr_limit: 0,
            gdtr_base: 0,
            gdtr_limit: 0,
            star: 0,
            lstar: 0,
            sfmask: 0,
            kernel_gs_base: 0,
            tss_rsp0: 0,
            tr_base: 0,
            icount: 0,
            trace_sys: false,
            sys_log: alloc::vec::Vec::new(),
        }
    }
}

impl Cpu {
    pub fn new() -> Self {
        Self::default()
    }

    fn flag(&self, bit: u64) -> bool {
        self.rflags & bit != 0
    }
    fn set_flag(&mut self, bit: u64, on: bool) {
        if on {
            self.rflags |= bit;
        } else {
            self.rflags &= !bit;
        }
    }
    pub fn zf(&self) -> bool {
        self.flag(ZF)
    }

    fn set_zsp(&mut self, val: u64, size: u8) {
        let bits = size as u32 * 8;
        let masked = if bits == 64 { val } else { val & ((1u64 << bits) - 1) };
        self.set_flag(ZF, masked == 0);
        self.set_flag(SF, (masked >> (bits - 1)) & 1 == 1);
        self.set_flag(PF, (masked as u8).count_ones() % 2 == 0);
    }

    /// Apply ALU op `sel` (the `/digit` of the 0x80/0x81/0x83 groups, also used
    /// for the corresponding two-operand opcodes) to `a` op `b`, set flags, and
    /// return `(result, writeback)`. `writeback` is false for `cmp` (sel 7).
    /// sel: 0 add, 1 or, 2 adc, 3 sbb, 4 and, 5 sub, 6 xor, 7 cmp.
    fn apply_alu(&mut self, sel: u8, a: u64, b: u64, size: u8) -> (u64, bool) {
        // All flag math is done at the operand width, not 64-bit: CF is the
        // carry/borrow out of bit (bits-1), OF the signed overflow at that bit.
        let bits = size as u32 * 8;
        let mask = if bits >= 64 { u64::MAX } else { (1u64 << bits) - 1 };
        let msb = 1u64 << (bits - 1);
        let cin: u64 = if self.flag(CF) { 1 } else { 0 };
        let (am, bm) = (a & mask, b & mask);
        let (res, writeback) = match sel {
            0 | 2 => {
                // add / adc
                let c = if sel == 2 { cin } else { 0 };
                let wide = am as u128 + bm as u128 + c as u128;
                let res = (wide as u64) & mask;
                self.set_flag(CF, wide > mask as u128);
                self.set_flag(OF, (!(am ^ bm) & (am ^ res)) & msb != 0);
                (res, true)
            }
            3 | 5 | 7 => {
                // sbb / sub / cmp
                let c = if sel == 3 { cin } else { 0 };
                let res = am.wrapping_sub(bm).wrapping_sub(c) & mask;
                self.set_flag(CF, (am as u128) < bm as u128 + c as u128);
                self.set_flag(OF, ((am ^ bm) & (am ^ res)) & msb != 0);
                (res, sel != 7)
            }
            1 => (self.logic_flags((a | b) & mask), true), // or
            4 => (self.logic_flags((a & b) & mask), true), // and
            6 => (self.logic_flags((a ^ b) & mask), true), // xor
            _ => (am, false),
        };
        self.set_zsp(res, size);
        (res, writeback)
    }

    /// Logical ops clear CF and OF; returns the result unchanged for chaining.
    fn logic_flags(&mut self, res: u64) -> u64 {
        self.set_flag(CF, false);
        self.set_flag(OF, false);
        res
    }

    // --- translated little-endian memory ----------------------------------
    // `addr` is a *virtual* address. In machine mode it is translated through
    // the guest page tables and may land on device MMIO (the Local APIC); below
    // long mode (and in the seed's usermode path) translation is the identity,
    // so these behave exactly like the original flat accessors.

    /// Translate a virtual address to physical (identity unless long-mode
    /// paging is active). Records CR2 on fault.
    fn xlate(&mut self, mem: &[u8], vaddr: u64, access: mmu::Access) -> Option<u64> {
        match mmu::translate(mem, &self.paging, vaddr, access, self.cpl == 3) {
            Ok(p) => Some(p),
            Err(f) => {
                self.cr2 = f.addr;
                None
            }
        }
    }

    fn load(&mut self, mem: &[u8], addr: u64, size: u8) -> Option<u64> {
        let phys = self.xlate(mem, addr, mmu::Access::Read)?;
        if self.machine_mode && self.dev.is_apic_mmio(phys) {
            return Some(self.dev.apic_read(phys) & mask128(size) as u64);
        }
        let a = phys as usize;
        let n = size as usize;
        if a + n > mem.len() {
            return None;
        }
        let mut v = 0u64;
        for i in 0..n {
            v |= (mem[a + i] as u64) << (i * 8);
        }
        Some(v)
    }
    fn store(&mut self, mem: &mut [u8], addr: u64, val: u64, size: u8) -> bool {
        let phys = match self.xlate(mem, addr, mmu::Access::Write) {
            Some(p) => p,
            None => return false,
        };
        if self.machine_mode && self.dev.is_apic_mmio(phys) {
            self.dev.apic_write(phys, val);
            return true;
        }
        let a = phys as usize;
        let n = size as usize;
        if a + n > mem.len() {
            return false;
        }
        for i in 0..n {
            mem[a + i] = (val >> (i * 8)) as u8;
        }
        true
    }
    fn load128(&mut self, mem: &[u8], addr: u64) -> Option<u128> {
        let phys = self.xlate(mem, addr, mmu::Access::Read)?;
        let a = phys as usize;
        if a + 16 > mem.len() {
            return None;
        }
        Some(u128::from_le_bytes(mem[a..a + 16].try_into().ok()?))
    }
    fn store128(&mut self, mem: &mut [u8], addr: u64, val: u128) -> bool {
        let phys = match self.xlate(mem, addr, mmu::Access::Write) {
            Some(p) => p,
            None => return false,
        };
        let a = phys as usize;
        if a + 16 > mem.len() {
            return false;
        }
        mem[a..a + 16].copy_from_slice(&val.to_le_bytes());
        true
    }
    /// Read a 128-bit SSE operand (an XMM register or a 16-byte memory operand).
    fn read_xmm_rm(&mut self, mem: &[u8], rm: Rm) -> Option<u128> {
        match rm {
            Rm::Reg(r) => Some(self.xmm[r]),
            Rm::Mem(a) => self.load128(mem, a),
        }
    }
    fn write_xmm_rm(&mut self, mem: &mut [u8], rm: Rm, val: u128) -> bool {
        match rm {
            Rm::Reg(r) => {
                self.xmm[r] = val;
                true
            }
            Rm::Mem(a) => self.store128(mem, a, val),
        }
    }

    fn read_rm(&mut self, mem: &[u8], rm: Rm, size: u8) -> Option<u64> {
        match rm {
            Rm::Reg(r) => {
                let v = self.regs[r];
                Some(if size >= 8 { v } else { v & ((1u64 << (size as u32 * 8)) - 1) })
            }
            Rm::Mem(a) => self.load(mem, a, size),
        }
    }
    fn write_rm(&mut self, mem: &mut [u8], rm: Rm, val: u64, size: u8) -> bool {
        match rm {
            Rm::Reg(r) => {
                // 8/16-bit writes preserve the upper register bits; a 32-bit
                // write zero-extends to 64; a 64-bit write is full-width.
                self.regs[r] = match size {
                    1 => (self.regs[r] & !0xff) | (val & 0xff),
                    2 => (self.regs[r] & !0xffff) | (val & 0xffff),
                    4 => val & 0xFFFF_FFFF,
                    _ => val,
                };
                true
            }
            Rm::Mem(a) => self.store(mem, a, val, size),
        }
    }

    /// Decode a ModRM byte at `pc` for an instruction with no trailing immediate.
    fn decode_modrm(
        &self,
        mem: &[u8],
        pc: u64,
        rex_r: bool,
        rex_x: bool,
        rex_b: bool,
    ) -> (usize, Rm, u64) {
        self.decode_modrm_imm(mem, pc, rex_r, rex_x, rex_b, 0)
    }

    /// Decode a ModRM byte. `imm_len` is the number of immediate bytes that
    /// follow the ModRM/SIB/displacement — needed so RIP-relative addressing
    /// (`[rip+disp32]`) resolves against the address of the *next instruction*
    /// (end of the whole instruction, immediate included), not just the end of
    /// the displacement. Returns (reg field index, r/m operand, pc after the
    /// ModRM/SIB/disp).
    fn decode_modrm_imm(
        &self,
        mem: &[u8],
        mut pc: u64,
        rex_r: bool,
        rex_x: bool,
        rex_b: bool,
        imm_len: usize,
    ) -> (usize, Rm, u64) {
        // Code bytes (ModRM/SIB/displacement) are at virtual addresses; translate
        // each through a local paging copy so this stays `&self` (no borrow of
        // the mutable CPU). Identity below long mode / in usermode. `pc` is a
        // u64 (a 64-bit virtual address — `usize` is only 32-bit on wasm32).
        let pg = self.paging;
        let cu = self.cpl == 3;
        let cb = move |p: u64| -> u8 {
            let ph = mmu::translate(mem, &pg, p, mmu::Access::Execute, cu).unwrap_or(p);
            *mem.get(ph as usize).unwrap_or(&0)
        };
        let modrm = cb(pc);
        pc += 1;
        let md = modrm >> 6;
        let reg = ((modrm >> 3) & 7) as usize + if rex_r { 8 } else { 0 };
        let rm = (modrm & 7) as usize;
        if md == 3 {
            return (reg, Rm::Reg(rm + if rex_b { 8 } else { 0 }), pc);
        }
        let rd32 = |p: u64| -> i64 {
            let mut v = 0u32;
            for i in 0..4 {
                v |= (cb(p + i) as u32) << (i * 8);
            }
            v as i32 as i64
        };
        let mut addr: u64;
        if rm == 4 {
            // SIB byte
            let sib = cb(pc);
            pc += 1;
            let scale = 1u64 << (sib >> 6);
            let index = ((sib >> 3) & 7) as usize + if rex_x { 8 } else { 0 };
            let base_reg = (sib & 7) as usize;
            let idx_val = if ((sib >> 3) & 7) == 4 && !rex_x { 0 } else { self.regs[index] };
            if md == 0 && base_reg == 5 {
                addr = idx_val.wrapping_mul(scale).wrapping_add(rd32(pc) as u64);
                pc += 4;
            } else {
                addr = self.regs[base_reg + if rex_b { 8 } else { 0 }]
                    .wrapping_add(idx_val.wrapping_mul(scale));
            }
        } else if md == 0 && rm == 5 {
            // RIP-relative: base is the address of the *next* instruction, i.e.
            // end of the displacement plus any trailing immediate bytes.
            let d = rd32(pc);
            pc += 4;
            addr = (pc as u64 + imm_len as u64).wrapping_add(d as u64);
        } else {
            addr = self.regs[rm + if rex_b { 8 } else { 0 }];
        }
        if md == 1 {
            let d = cb(pc) as i8 as i64;
            pc += 1;
            addr = addr.wrapping_add(d as u64);
        } else if md == 2 {
            let d = rd32(pc);
            pc += 4;
            addr = addr.wrapping_add(d as u64);
        }
        // Apply an active FS/GS segment base (0 when no override prefix).
        (reg, Rm::Mem(addr.wrapping_add(self.seg_base)), pc)
    }

    fn push64(&mut self, mem: &mut [u8], val: u64) -> bool {
        self.regs[RSP] = self.regs[RSP].wrapping_sub(8);
        let sp = self.regs[RSP];
        self.store(mem, sp, val, 8)
    }
    fn pop64(&mut self, mem: &[u8]) -> Option<u64> {
        let v = self.load(mem, self.regs[RSP], 8)?;
        self.regs[RSP] = self.regs[RSP].wrapping_add(8);
        Some(v)
    }

    /// Decode and execute one instruction.
    pub fn step(&mut self, mem: &mut [u8]) -> StepResult {
        self.icount = self.icount.wrapping_add(1);
        // An import trap address (bound into IAT slots by the loader) isn't real
        // code — surface it so the host can service the imported function. Only
        // in the usermode path; a full machine has no IAT traps.
        if !self.machine_mode
            && self.rip >= IMPORT_BASE
            && self.rip < IMPORT_BASE + IMPORT_MAX * 8
        {
            return StepResult::Import { index: ((self.rip - IMPORT_BASE) / 8) as u32 };
        }
        let start = self.rip;
        // `pc` is a 64-bit virtual address. It must be u64, not usize: on wasm32
        // usize is 32-bit and would truncate high-half kernel addresses
        // (0xFFFF_8000_…) to garbage.
        let mut pc: u64 = self.rip;
        // Code fetch translates virtual→physical (identity below long mode). A
        // local paging copy keeps these closures free of any `self` borrow so
        // the dispatch body can still take `&mut self`.
        let pg = self.paging;
        let cu = self.cpl == 3;
        let fetch = |i: u64| -> u8 {
            let ph = mmu::translate(mem, &pg, i, mmu::Access::Execute, cu).unwrap_or(i);
            *mem.get(ph as usize).unwrap_or(&0)
        };
        let imm32 = |p: u64| -> i64 {
            let mut v = 0u32;
            for i in 0..4 {
                v |= (fetch(p + i) as u32) << (i * 8);
            }
            v as i32 as i64
        };
        let imm16 = |p: u64| -> i64 {
            (fetch(p) as u16 | ((fetch(p + 1) as u16) << 8)) as i16 as i64
        };

        // Legacy prefixes (segment override sets the operand segment base; the
        // rest are consumed — size/rep semantics handled per-opcode as needed).
        self.seg_base = 0;
        let mut opsize16 = false;
        let mut rep = false; // F3 (rep/repe) prefix seen
        let mut repne = false; // F2 (repne) prefix seen
        loop {
            match fetch(pc) {
                0x65 => self.seg_base = self.gs_base,
                0x64 => self.seg_base = self.fs_base,
                0x66 => opsize16 = true,
                0xf3 => rep = true,
                0xf2 => repne = true,
                // 0x67/0xF0/seg are consumed.
                0x67 | 0xf0 | 0x2e | 0x36 | 0x3e | 0x26 => {}
                _ => break,
            }
            pc += 1;
        }
        let _ = repne;
        // REX prefix (must immediately precede the opcode).
        let mut rex_w = false;
        let (mut rex_r, mut rex_x, mut rex_b) = (false, false, false);
        let mut rex_present = false;
        let mut b = fetch(pc);
        if (0x40..=0x4f).contains(&b) {
            rex_present = true; // even a bare 0x40 (no bits) changes 8-bit reg encoding
            rex_w = b & 8 != 0;
            rex_r = b & 4 != 0;
            rex_x = b & 2 != 0;
            rex_b = b & 1 != 0;
            pc += 1;
            b = fetch(pc);
        }
        let size: u8 = if rex_w {
            8
        } else if opsize16 {
            2
        } else {
            4
        };

        match b {
            // two-operand integer ALU with ModRM: add/or/adc/sbb/and/sub/xor/cmp.
            // Opcode bits encode op = (opcode>>3)&7 and direction = opcode&2
            // (0 => r/m,reg ; 2 => reg,r/m). Covers x1 (r/m,r) and x3 (r,r/m) of
            // each group. `mov` (0x88..0x8B) is handled separately below.
            0x01 | 0x03 | 0x09 | 0x0b | 0x11 | 0x13 | 0x19 | 0x1b | 0x21 | 0x23 | 0x29 | 0x2b
            | 0x31 | 0x33 | 0x39 | 0x3b => {
                let sel = (b >> 3) & 7;
                let to_reg = b & 2 != 0;
                pc += 1;
                let (reg, rm, npc) = self.decode_modrm(mem, pc, rex_r, rex_x, rex_b);
                pc = npc;
                let rmv = match self.read_rm(mem, rm, size) {
                    Some(v) => v,
                    None => return self.fault(rm),
                };
                let regv = if size == 4 { self.regs[reg] & 0xFFFF_FFFF } else { self.regs[reg] };
                let (a, src) = if to_reg { (regv, rmv) } else { (rmv, regv) };
                let (res, writeback) = self.apply_alu(sel as u8, a, src, size);
                if writeback {
                    let ok = if to_reg {
                        self.write_rm(mem, Rm::Reg(reg), res, size)
                    } else {
                        self.write_rm(mem, rm, res, size)
                    };
                    if !ok {
                        return self.fault(rm);
                    }
                }
                self.rip = pc as u64;
                StepResult::Ok
            }
            // xchg r/m, reg (0x87): swap the two operands (no flags).
            0x87 => {
                pc += 1;
                let (reg, rm, npc) = self.decode_modrm(mem, pc, rex_r, rex_x, rex_b);
                pc = npc;
                let rmv = match self.read_rm(mem, rm, size) {
                    Some(v) => v,
                    None => return self.fault(rm),
                };
                let regv = if size == 4 { self.regs[reg] & 0xFFFF_FFFF } else { self.regs[reg] };
                if !self.write_rm(mem, rm, regv, size) {
                    return self.fault(rm);
                }
                self.regs[reg] = if size == 4 { rmv & 0xFFFF_FFFF } else { rmv };
                self.rip = pc as u64;
                StepResult::Ok
            }
            // movsxd r64, r/m32 (0x63): sign-extend a 32-bit source to 64 bits.
            0x63 => {
                pc += 1;
                let (reg, rm, npc) = self.decode_modrm(mem, pc, rex_r, rex_x, rex_b);
                pc = npc;
                let v = match self.read_rm(mem, rm, 4) {
                    Some(v) => v as u32 as i32 as i64 as u64,
                    None => return self.fault(rm),
                };
                self.regs[reg] = if rex_w { v } else { v & 0xFFFF_FFFF };
                self.rip = pc as u64;
                StepResult::Ok
            }
            // 8-bit mov: 0x88 r/m8,r8 ; 0x8A r8,r/m8. Byte writes preserve the
            // rest of a destination register (no zero-extension).
            0x88 | 0x8a => {
                let to_reg = b == 0x8a;
                pc += 1;
                let (reg, rm, npc) = self.decode_modrm(mem, pc, rex_r, rex_x, rex_b);
                pc = npc;
                if to_reg {
                    let v = match self.read_rm(mem, rm, 1) {
                        Some(v) => v & 0xff,
                        None => return self.fault(rm),
                    };
                    self.regs[reg] = (self.regs[reg] & !0xff) | v;
                } else {
                    let v = self.regs[reg] & 0xff;
                    match rm {
                        Rm::Reg(r) => self.regs[r] = (self.regs[r] & !0xff) | v,
                        Rm::Mem(a) => {
                            if !self.store(mem, a, v, 1) {
                                return StepResult::Fault { addr: a };
                            }
                        }
                    }
                }
                self.rip = pc as u64;
                StepResult::Ok
            }
            // mov r/m,reg (0x89) and mov reg,r/m (0x8B).
            0x89 | 0x8b => {
                let to_reg = b == 0x8b;
                pc += 1;
                let (reg, rm, npc) = self.decode_modrm(mem, pc, rex_r, rex_x, rex_b);
                pc = npc;
                let ok = if to_reg {
                    match self.read_rm(mem, rm, size) {
                        Some(v) => self.write_rm(mem, Rm::Reg(reg), v, size),
                        None => return self.fault(rm),
                    }
                } else {
                    let v = if size == 4 { self.regs[reg] & 0xFFFF_FFFF } else { self.regs[reg] };
                    self.write_rm(mem, rm, v, size)
                };
                if !ok {
                    return self.fault(rm);
                }
                self.rip = pc as u64;
                StepResult::Ok
            }
            // accumulator-immediate ALU: AL,imm8 (0x04/0C/14/1C/24/2C/34/3C) and
            // eAX,imm32 (0x05/0D/15/1D/25/2D/35/3D). op = (opcode>>3)&7.
            0x04 | 0x0c | 0x14 | 0x1c | 0x24 | 0x2c | 0x34 | 0x3c => {
                let sel = (b >> 3) & 7;
                pc += 1;
                let imm = fetch(pc) as u64;
                pc += 1;
                let al = self.regs[RAX] & 0xff;
                let (res, wb) = self.apply_alu(sel, al, imm, 1);
                if wb {
                    self.regs[RAX] = (self.regs[RAX] & !0xff) | (res & 0xff);
                }
                self.rip = pc as u64;
                StepResult::Ok
            }
            0x05 | 0x0d | 0x15 | 0x1d | 0x25 | 0x2d | 0x35 | 0x3d => {
                let sel = (b >> 3) & 7;
                pc += 1;
                let imm = if opsize16 { imm16(pc) as u64 } else { imm32(pc) as u64 };
                pc += if opsize16 { 2 } else { 4 };
                let a = if size == 4 { self.regs[RAX] & 0xFFFF_FFFF } else { self.regs[RAX] };
                let (res, wb) = self.apply_alu(sel, a, imm, size);
                if wb {
                    self.regs[RAX] = if size == 4 { res & 0xFFFF_FFFF } else { res };
                }
                self.rip = pc as u64;
                StepResult::Ok
            }
            // test AL,imm8 (0xA8) and test eAX,imm32 (0xA9) — flags only.
            0xa8 => {
                pc += 1;
                let imm = fetch(pc) as u64;
                pc += 1;
                self.apply_alu(4, self.regs[RAX] & 0xff, imm, 1);
                self.rip = pc as u64;
                StepResult::Ok
            }
            0xa9 => {
                pc += 1;
                let imm = if opsize16 { imm16(pc) as u64 } else { imm32(pc) as u64 };
                pc += if opsize16 { 2 } else { 4 };
                let a = if size == 4 { self.regs[RAX] & 0xFFFF_FFFF } else { self.regs[RAX] };
                self.apply_alu(4, a, imm, size);
                self.rip = pc as u64;
                StepResult::Ok
            }
            // immediate-group ALU: 0x81 r/m, imm32 ; 0x83 r/m, imm8 (sign-ext).
            // ModRM.reg is the op selector (add/or/adc/sbb/and/sub/xor/cmp).
            0x81 | 0x83 => {
                pc += 1;
                let il = if b == 0x83 { 1 } else if opsize16 { 2 } else { 4 };
                let (sel, rm, npc) = self.decode_modrm_imm(mem, pc, rex_r, rex_x, rex_b, il);
                pc = npc;
                let imm = if b == 0x83 {
                    fetch(pc) as i8 as i64 as u64
                } else if opsize16 {
                    imm16(pc) as u64
                } else {
                    imm32(pc) as u64 // sign-extended to 64
                };
                pc += il as u64;
                let a = match self.read_rm(mem, rm, size) {
                    Some(v) => v,
                    None => return self.fault(rm),
                };
                let (res, writeback) = self.apply_alu((sel & 7) as u8, a, imm, size);
                if writeback && !self.write_rm(mem, rm, res, size) {
                    return self.fault(rm);
                }
                self.rip = pc as u64;
                StepResult::Ok
            }
            // mov sreg, r/m16 (0x8E) and mov r/m16, sreg (0x8C). In long mode the
            // DS/ES/SS/CS bases are flat (ignored) and FS/GS bases are set via
            // MSRs (wrmsr/swapgs), so the selector load itself is a no-op here.
            // We still consume the operand so decoding stays in sync.
            0x8c | 0x8e => {
                pc += 1;
                let (_sreg, rm, npc) = self.decode_modrm(mem, pc, rex_r, rex_x, rex_b);
                pc = npc;
                if b == 0x8c {
                    // store selector → r/m16 (we don't track selectors: write 0).
                    if !self.write_rm(mem, rm, 0, 2) {
                        return self.fault(rm);
                    }
                } else {
                    // load selector ← r/m16: consume (read) it, no architectural
                    // effect in our flat long-mode model.
                    if self.read_rm(mem, rm, 2).is_none() {
                        return self.fault(rm);
                    }
                }
                self.rip = pc as u64;
                StepResult::Ok
            }
            // imul reg, r/m, imm (0x69 imm32 / 0x6B imm8, sign-extended).
            0x69 | 0x6b => {
                let il = if b == 0x6b { 1 } else if opsize16 { 2 } else { 4 };
                pc += 1;
                let (reg, rm, npc) = self.decode_modrm_imm(mem, pc, rex_r, rex_x, rex_b, il);
                pc = npc;
                let src = match self.read_rm(mem, rm, size) {
                    Some(v) => v,
                    None => return self.fault(rm),
                };
                let imm: i64 = if b == 0x6b {
                    let v = fetch(pc) as i8 as i64;
                    pc += 1;
                    v
                } else if opsize16 {
                    let v = (fetch(pc) as u16 | ((fetch(pc + 1) as u16) << 8)) as i16 as i64;
                    pc += 2;
                    v
                } else {
                    let v = imm32(pc);
                    pc += 4;
                    v
                };
                let full = sign_ext(src, size) as i128 * imm as i128;
                let prod = full as u64;
                self.regs[reg] = if size == 4 { prod & 0xFFFF_FFFF } else { prod };
                // CF=OF when the true product does not fit in the destination.
                let lo = sign_ext((full as u128 & mask128(size)) as u64, size) as i128;
                let ovf = full != lo;
                self.set_flag(CF, ovf);
                self.set_flag(OF, ovf);
                self.rip = pc as u64;
                StepResult::Ok
            }
            // push imm32 (0x68, sign-extended to 64) / push imm8 (0x6A).
            0x68 | 0x6a => {
                let v = if b == 0x6a {
                    let v = fetch(pc + 1) as i8 as i64 as u64;
                    pc += 2;
                    v
                } else {
                    let v = imm32(pc + 1) as u64;
                    pc += 5;
                    v
                };
                if !self.push64(mem, v) {
                    return StepResult::Fault { addr: self.regs[RSP] };
                }
                self.rip = pc as u64;
                StepResult::Ok
            }
            // lea reg, [mem]  (0x8D): load the effective address (no memory read).
            0x8d => {
                pc += 1;
                let (reg, rm, npc) = self.decode_modrm(mem, pc, rex_r, rex_x, rex_b);
                pc = npc;
                let addr = match rm {
                    Rm::Mem(a) => a,
                    Rm::Reg(_) => return StepResult::Unknown { rip: start, byte: 0x8d },
                };
                self.regs[reg] = if size == 4 { addr & 0xFFFF_FFFF } else { addr };
                self.rip = pc as u64;
                StepResult::Ok
            }
            // test r/m, reg  (0x85): set flags from (r/m & reg), no writeback.
            0x85 => {
                pc += 1;
                let (reg, rm, npc) = self.decode_modrm(mem, pc, rex_r, rex_x, rex_b);
                pc = npc;
                let a = match self.read_rm(mem, rm, size) {
                    Some(v) => v,
                    None => return self.fault(rm),
                };
                let regv = if size == 4 { self.regs[reg] & 0xFFFF_FFFF } else { self.regs[reg] };
                self.apply_alu(4, a, regv, size); // AND sets ZF/SF/PF, clears CF/OF
                self.rip = pc as u64;
                StepResult::Ok
            }
            // mov r8, imm8  (0xB0+r). Without a REX prefix, regs 4..7 are the
            // high-byte registers AH/CH/DH/BH; with REX they're SPL..DIL (+8 →
            // R8B..R15B). Either way only the selected byte is written.
            0xb0..=0xb7 => {
                let sel = (b - 0xb0) as usize;
                pc += 1;
                let imm = fetch(pc) as u64 & 0xff;
                pc += 1;
                if !rex_present && sel >= 4 {
                    // AH/CH/DH/BH: bits 8..15 of RAX/RCX/RDX/RBX. Only when NO
                    // REX byte is present — a bare 0x40 selects SPL..DIL instead.
                    let r = sel - 4;
                    self.regs[r] = (self.regs[r] & !0xff00) | (imm << 8);
                } else {
                    let r = sel + if rex_b { 8 } else { 0 };
                    self.regs[r] = (self.regs[r] & !0xff) | imm;
                }
                self.rip = pc as u64;
                StepResult::Ok
            }
            // mov reg, imm  (0xB8+r): imm64 with REX.W else imm32 zero-extended.
            0xb8..=0xbf => {
                let reg = (b - 0xb8) as usize + if rex_b { 8 } else { 0 };
                pc += 1;
                if rex_w {
                    let mut v = 0u64;
                    for i in 0..8 {
                        v |= (fetch(pc + i) as u64) << (i * 8);
                    }
                    pc += 8;
                    self.regs[reg] = v;
                } else if opsize16 {
                    // mov r16, imm16 — preserve the upper 48 bits.
                    let imm = fetch(pc) as u64 | ((fetch(pc + 1) as u64) << 8);
                    pc += 2;
                    self.regs[reg] = (self.regs[reg] & !0xffff) | imm;
                } else {
                    let v = imm32(pc) as u32 as u64;
                    pc += 4;
                    self.regs[reg] = v;
                }
                self.rip = pc as u64;
                StepResult::Ok
            }
            // mov r/m8, imm8  (0xC6 /0)
            0xc6 => {
                pc += 1;
                let (_, rm, npc) = self.decode_modrm_imm(mem, pc, rex_r, rex_x, rex_b, 1);
                pc = npc;
                let imm = fetch(pc) as u64 & 0xff;
                pc += 1;
                match rm {
                    Rm::Reg(r) => self.regs[r] = (self.regs[r] & !0xff) | imm,
                    Rm::Mem(a) => {
                        if !self.store(mem, a, imm, 1) {
                            return StepResult::Fault { addr: a };
                        }
                    }
                }
                self.rip = pc as u64;
                StepResult::Ok
            }
            // mov r/m, imm  (0xC7 /0): imm16 with a 0x66 override, else imm32
            // (sign-extended for a 64-bit operand). The immediate length also
            // feeds RIP-relative addressing, so it must match.
            0xc7 => {
                pc += 1;
                let il = if opsize16 { 2 } else { 4 };
                let (_, rm, npc) = self.decode_modrm_imm(mem, pc, rex_r, rex_x, rex_b, il);
                pc = npc;
                let imm = if opsize16 { imm16(pc) as u64 } else { imm32(pc) as u64 };
                pc += il as u64;
                let imm = imm & mask128(size) as u64;
                if !self.write_rm(mem, rm, imm, size) {
                    return self.fault(rm);
                }
                self.rip = pc as u64;
                StepResult::Ok
            }
            // push/pop reg
            0x50..=0x57 => {
                let reg = (b - 0x50) as usize + if rex_b { 8 } else { 0 };
                let v = self.regs[reg];
                if !self.push64(mem, v) {
                    return StepResult::Fault { addr: self.regs[RSP] };
                }
                self.rip = (pc + 1) as u64;
                StepResult::Ok
            }
            0x58..=0x5f => {
                let reg = (b - 0x58) as usize + if rex_b { 8 } else { 0 };
                match self.pop64(mem) {
                    Some(v) => self.regs[reg] = v,
                    None => return StepResult::Fault { addr: self.regs[RSP] },
                }
                self.rip = (pc + 1) as u64;
                StepResult::Ok
            }
            // call rel32
            0xe8 => {
                let rel = imm32(pc + 1);
                let ret = (pc + 5) as u64;
                if !self.push64(mem, ret) {
                    return StepResult::Fault { addr: self.regs[RSP] };
                }
                self.rip = ret.wrapping_add(rel as u64);
                StepResult::Ok
            }
            // ret
            0xc3 => match self.pop64(mem) {
                Some(HALT_ADDR) => StepResult::Halt,
                Some(target) => {
                    self.rip = target;
                    StepResult::Ok
                }
                None => StepResult::Halt,
            },
            // ret imm16 (0xC2): pop return address, then add imm16 to RSP.
            0xc2 => {
                let imm = fetch(pc + 1) as u64 | ((fetch(pc + 2) as u64) << 8);
                match self.pop64(mem) {
                    Some(HALT_ADDR) => StepResult::Halt,
                    Some(target) => {
                        self.regs[RSP] = self.regs[RSP].wrapping_add(imm);
                        self.rip = target;
                        StepResult::Ok
                    }
                    None => StepResult::Halt,
                }
            }
            // jmp rel32 / rel8
            0xe9 => {
                let rel = imm32(pc + 1);
                self.rip = (pc as u64 + 5).wrapping_add(rel as u64);
                StepResult::Ok
            }
            0xeb => {
                let rel = fetch(pc + 1) as i8 as i64;
                self.rip = (pc as u64 + 2).wrapping_add(rel as u64);
                StepResult::Ok
            }
            // jcc rel8 (0x70..0x7F)
            0x70..=0x7f => {
                let take = self.cond(b & 0x0f);
                let rel = fetch(pc + 1) as i8 as i64;
                self.rip = (pc as u64 + 2).wrapping_add(if take { rel as u64 } else { 0 });
                StepResult::Ok
            }
            0x90 => {
                self.rip = (pc + 1) as u64; // nop
                StepResult::Ok
            }
            // cbw/cwde/cdqe (0x98): sign-extend AL/AX/EAX into AX/EAX/RAX.
            0x98 => {
                self.regs[RAX] = match size {
                    8 => self.regs[RAX] as u32 as i32 as i64 as u64,
                    2 => (self.regs[RAX] & !0xffff) | (self.regs[RAX] as u8 as i8 as i16 as u16 as u64),
                    _ => (self.regs[RAX] as u16 as i16 as i32 as u32) as u64,
                };
                self.rip = (pc + 1) as u64;
                StepResult::Ok
            }
            // cwd/cdq/cqo (0x99): sign-extend the accumulator into DX/EDX/RDX.
            0x99 => {
                let neg = match size {
                    8 => self.regs[RAX] >> 63 & 1 == 1,
                    2 => self.regs[RAX] >> 15 & 1 == 1,
                    _ => self.regs[RAX] >> 31 & 1 == 1,
                };
                self.regs[RDX] = match size {
                    8 => {
                        if neg {
                            u64::MAX
                        } else {
                            0
                        }
                    }
                    2 => (self.regs[RDX] & !0xffff) | if neg { 0xffff } else { 0 },
                    _ => {
                        if neg {
                            0xFFFF_FFFF
                        } else {
                            0
                        }
                    }
                };
                self.rip = (pc + 1) as u64;
                StepResult::Ok
            }
            // test r/m8, r8 (0x84): 8-bit AND for flags only.
            0x84 => {
                pc += 1;
                let (reg, rm, npc) = self.decode_modrm(mem, pc, rex_r, rex_x, rex_b);
                pc = npc;
                let a = match self.read_rm(mem, rm, 1) {
                    Some(v) => v & 0xff,
                    None => return self.fault(rm),
                };
                let regv = self.regs[reg] & 0xff;
                self.apply_alu(4, a, regv, 1);
                self.rip = pc as u64;
                StepResult::Ok
            }
            // 8-bit unary group (0xF6 /digit): test imm8 / not / neg / mul / div.
            0xf6 => {
                pc += 1;
                let (sub, rm, npc) = self.decode_modrm(mem, pc, rex_r, rex_x, rex_b);
                pc = npc;
                let a = match self.read_rm(mem, rm, 1) {
                    Some(v) => v & 0xff,
                    None => return self.fault(rm),
                };
                match sub & 7 {
                    0 | 1 => {
                        let imm = fetch(pc) as u64;
                        pc += 1;
                        self.apply_alu(4, a, imm, 1);
                    }
                    2 => {
                        if !self.write_rm(mem, rm, !a & 0xff, 1) {
                            return self.fault(rm);
                        }
                    }
                    3 => {
                        let (r, _) = self.apply_alu(5, 0, a, 1);
                        if !self.write_rm(mem, rm, r, 1) {
                            return self.fault(rm);
                        }
                    }
                    4 | 5 => {
                        // mul/imul: AX = AL * r/m8. CF=OF is set when the result
                        // does not fit in AL: the high byte is significant (mul),
                        // or AX is not the sign-extension of AL (imul).
                        let al = self.regs[RAX] & 0xff;
                        let (prod, ovf) = if sub & 7 == 4 {
                            let p = (al * a) & 0xffff;
                            (p, p >> 8 != 0)
                        } else {
                            let p = (((al as i8 as i64) * (a as i8 as i64)) as u64) & 0xffff;
                            (p, (p as u8 as i8 as i16) != p as u16 as i16)
                        };
                        self.regs[RAX] = (self.regs[RAX] & !0xffff) | prod;
                        self.set_flag(CF, ovf);
                        self.set_flag(OF, ovf);
                    }
                    6 | 7 => {
                        // div/idiv: AL = AX / r/m8, AH = AX % r/m8
                        if a == 0 {
                            return StepResult::Fault { addr: 0 };
                        }
                        let ax = self.regs[RAX] & 0xffff;
                        let (q, r) = if sub & 7 == 6 {
                            (ax / a, ax % a)
                        } else {
                            let n = ax as i16 as i64;
                            let d = a as i8 as i64;
                            ((n / d) as u64 & 0xff, (n % d) as u64 & 0xff)
                        };
                        self.regs[RAX] = (self.regs[RAX] & !0xffff) | (q & 0xff) | ((r & 0xff) << 8);
                    }
                    _ => return StepResult::Unknown { rip: start, byte: 0xf6 },
                }
                self.rip = pc as u64;
                StepResult::Ok
            }
            // unary group 3 (0xF7 /digit): test imm / not / neg / mul / imul /
            // div / idiv on r/m (mul-family use rdx:rax).
            0xf7 => {
                pc += 1;
                let (sub, rm, npc) = self.decode_modrm(mem, pc, rex_r, rex_x, rex_b);
                pc = npc;
                let a = match self.read_rm(mem, rm, size) {
                    Some(v) => v,
                    None => return self.fault(rm),
                };
                match sub & 7 {
                    0 | 1 => {
                        // test r/m, imm (imm16 with 0x66, else imm32 sign-extended)
                        let imm = if opsize16 { imm16(pc) as u64 } else { imm32(pc) as u64 };
                        pc += if opsize16 { 2 } else { 4 };
                        self.apply_alu(4, a, imm, size); // AND -> flags only
                    }
                    2 => {
                        // not (no flags)
                        let res = !a & if size == 4 { 0xFFFF_FFFF } else { u64::MAX };
                        if !self.write_rm(mem, rm, res, size) {
                            return self.fault(rm);
                        }
                    }
                    3 => {
                        // neg = 0 - a
                        let (res, _) = self.apply_alu(5, 0, a, size);
                        if !self.write_rm(mem, rm, res, size) {
                            return self.fault(rm);
                        }
                    }
                    4 | 5 => {
                        // mul/imul: rdx:rax = rax * r/m. CF=OF is set when the
                        // upper half is significant (mul) or the product is not
                        // the sign-extension of the lower half (imul).
                        let bits = size as u32 * 8;
                        let (prod, ovf): (u128, bool) = if sub & 7 == 4 {
                            let p = (self.regs[RAX] as u128 & mask128(size)) * (a as u128 & mask128(size));
                            (p, p >> bits != 0)
                        } else {
                            let x = sign_ext(self.regs[RAX], size) as i128;
                            let y = sign_ext(a, size) as i128;
                            let full = x * y;
                            let lo = sign_ext((full as u128 & mask128(size)) as u64, size) as i128;
                            (full as u128, full != lo)
                        };
                        if size == 4 {
                            self.regs[RAX] = (prod as u64) & 0xFFFF_FFFF;
                            self.regs[RDX] = (prod >> 32) as u64 & 0xFFFF_FFFF;
                        } else {
                            self.regs[RAX] = prod as u64;
                            self.regs[RDX] = (prod >> 64) as u64;
                        }
                        self.set_flag(CF, ovf);
                        self.set_flag(OF, ovf);
                    }
                    6 | 7 => {
                        // div/idiv: dividend is (RDX:RAX) at the operand width
                        // (DX:AX for 16-bit, EDX:EAX for 32-bit, RDX:RAX for 64).
                        if a == 0 {
                            return StepResult::Fault { addr: 0 }; // #DE
                        }
                        // Writeback rule: 16-bit preserves upper bits, 32-bit
                        // zero-extends, 64-bit is full-width.
                        let wr = |cur: u64, v: u64| match size {
                            2 => (cur & !0xFFFF) | (v & 0xFFFF),
                            4 => v & 0xFFFF_FFFF,
                            _ => v,
                        };
                        let (dlo, alo) = (self.regs[RDX], self.regs[RAX]);
                        if sub & 7 == 6 {
                            let num: u128 = match size {
                                2 => ((dlo & 0xFFFF) << 16 | (alo & 0xFFFF)) as u128,
                                4 => ((dlo & 0xFFFF_FFFF) << 32 | (alo & 0xFFFF_FFFF)) as u128,
                                _ => ((dlo as u128) << 64) | alo as u128,
                            };
                            let d = (a & mask128(size) as u64) as u128;
                            self.regs[RAX] = wr(alo, (num / d) as u64);
                            self.regs[RDX] = wr(dlo, (num % d) as u64);
                        } else {
                            let num: i128 = match size {
                                2 => (((dlo & 0xFFFF) << 16 | (alo & 0xFFFF)) as i32) as i128,
                                4 => (((dlo & 0xFFFF_FFFF) << 32 | (alo & 0xFFFF_FFFF)) as i64) as i128,
                                _ => ((dlo as i128) << 64) | alo as i128,
                            };
                            let d = sign_ext(a, size) as i128;
                            self.regs[RAX] = wr(alo, (num / d) as u64);
                            self.regs[RDX] = wr(dlo, (num % d) as u64);
                        }
                    }
                    _ => return StepResult::Unknown { rip: start, byte: 0xf7 },
                }
                self.rip = pc as u64;
                StepResult::Ok
            }
            // shift/rotate group. 8-bit: C0 imm8 / D0 by 1 / D2 by CL.
            // v-bit: C1 imm8 / D1 by 1 / D3 by CL. ModRM.reg selects:
            // /0 rol /1 ror /2 rcl /3 rcr /4 shl /5 shr (/6 sal=shl) /7 sar.
            0xc0 | 0xc1 | 0xd0 | 0xd1 | 0xd2 | 0xd3 => {
                pc += 1;
                let eight = matches!(b, 0xc0 | 0xd0 | 0xd2);
                let sz: u8 = if eight { 1 } else { size };
                let il = if matches!(b, 0xc0 | 0xc1) { 1 } else { 0 };
                let (sel, rm, npc) = self.decode_modrm_imm(mem, pc, rex_r, rex_x, rex_b, il);
                pc = npc;
                let raw_count = match b {
                    0xc0 | 0xc1 => {
                        let c = fetch(pc);
                        pc += 1;
                        c as u32
                    }
                    0xd0 | 0xd1 => 1,
                    _ => (self.regs[RCX] & 0xff) as u32, // D2/D3: count = CL
                };
                let bits = sz as u32 * 8;
                let count = raw_count & (if sz == 8 { 63 } else { 31 }); // 6-bit count for 64-bit operands, else 5-bit
                let a = match self.read_rm(mem, rm, sz) {
                    Some(v) => v,
                    None => return self.fault(rm),
                };
                let mask = if bits == 64 { u64::MAX } else { (1u64 << bits) - 1 };
                let av = a & mask;
                let op = sel & 7;
                // A shift/rotate by zero changes nothing — flags included.
                if count == 0 {
                    self.rip = pc as u64;
                    return StepResult::Ok;
                }
                let cf0 = self.flag(CF);
                let res = match op {
                    4 | 6 => {
                        // shl / sal
                        self.set_flag(CF, count <= bits && (av >> (bits - count)) & 1 == 1);
                        (av.wrapping_shl(count)) & mask
                    }
                    5 => {
                        // shr (logical)
                        self.set_flag(CF, (av >> (count - 1).min(bits - 1)) & 1 == 1);
                        if count >= bits { 0 } else { av >> count }
                    }
                    7 => {
                        // sar (arithmetic)
                        let sign = (av >> (bits - 1)) & 1 == 1;
                        self.set_flag(CF, (av >> (count - 1).min(bits - 1)) & 1 == 1);
                        if count >= bits {
                            if sign { mask } else { 0 }
                        } else {
                            let mut r = av >> count;
                            if sign {
                                r |= mask & !(mask >> count);
                            }
                            r
                        }
                    }
                    0 => {
                        // rol
                        let c = count % bits;
                        let r = if c == 0 { av } else { ((av << c) | (av >> (bits - c))) & mask };
                        self.set_flag(CF, r & 1 == 1);
                        r
                    }
                    1 => {
                        // ror
                        let c = count % bits;
                        let r = if c == 0 { av } else { ((av >> c) | (av << (bits - c))) & mask };
                        self.set_flag(CF, (r >> (bits - 1)) & 1 == 1);
                        r
                    }
                    2 | 3 => {
                        // rcl / rcr — rotate through CF.
                        let mut val = av;
                        let mut cf = if cf0 { 1u64 } else { 0 };
                        for _ in 0..count {
                            if op == 2 {
                                let nc = (val >> (bits - 1)) & 1;
                                val = ((val << 1) | cf) & mask;
                                cf = nc;
                            } else {
                                let nc = val & 1;
                                val = (val >> 1) | (cf << (bits - 1));
                                cf = nc;
                            }
                        }
                        self.set_flag(CF, cf == 1);
                        val & mask
                    }
                    _ => return StepResult::Unknown { rip: start, byte: b },
                };
                // OF is architecturally defined only for a count of 1.
                if count == 1 {
                    let cf = self.flag(CF);
                    let msb = (res >> (bits - 1)) & 1 == 1;
                    let of = match op {
                        4 | 6 => msb ^ cf,                       // shl
                        5 => (av >> (bits - 1)) & 1 == 1,        // shr: original MSB
                        7 => false,                              // sar
                        0 => msb ^ cf,                           // rol
                        1 => msb ^ ((res >> (bits - 2)) & 1 == 1), // ror
                        2 => msb ^ cf,                           // rcl
                        _ => false,
                    };
                    self.set_flag(OF, of);
                }
                if matches!(op, 4 | 5 | 6 | 7) {
                    self.set_zsp(res, sz);
                }
                if !self.write_rm(mem, rm, res, sz) {
                    return self.fault(rm);
                }
                self.rip = pc as u64;
                StepResult::Ok
            }
            // group 4 (0xFE /digit): 8-bit inc (/0) / dec (/1). Affect ZF/SF/PF/OF,
            // not CF.
            0xfe => {
                pc += 1;
                let (sub, rm, npc) = self.decode_modrm(mem, pc, rex_r, rex_x, rex_b);
                pc = npc;
                let v = match self.read_rm(mem, rm, 1) {
                    Some(v) => v & 0xff,
                    None => return self.fault(rm),
                };
                let res = if sub & 7 == 0 { v.wrapping_add(1) } else { v.wrapping_sub(1) } & 0xff;
                self.set_zsp(res, 1);
                if !self.write_rm(mem, rm, res, 1) {
                    return self.fault(rm);
                }
                self.rip = pc as u64;
                StepResult::Ok
            }
            // group 5 (0xFF /digit): inc, dec, call, jmp, push of r/m.
            0xff => {
                pc += 1;
                let (sub, rm, npc) = self.decode_modrm(mem, pc, rex_r, rex_x, rex_b);
                pc = npc;
                match sub & 7 {
                    0 | 1 => {
                        // inc / dec — affect ZF/SF/OF/PF but not CF.
                        let v = match self.read_rm(mem, rm, size) {
                            Some(v) => v,
                            None => return self.fault(rm),
                        };
                        let res = if sub & 7 == 0 { v.wrapping_add(1) } else { v.wrapping_sub(1) };
                        self.set_zsp(res, size);
                        if !self.write_rm(mem, rm, res, size) {
                            return self.fault(rm);
                        }
                        self.rip = pc as u64;
                        StepResult::Ok
                    }
                    2 => {
                        // call r/m64 — push return address, jump indirectly.
                        let target = match self.read_rm(mem, rm, 8) {
                            Some(v) => v,
                            None => return self.fault(rm),
                        };
                        if !self.push64(mem, pc as u64) {
                            return StepResult::Fault { addr: self.regs[RSP] };
                        }
                        self.rip = target;
                        StepResult::Ok
                    }
                    4 => {
                        // jmp r/m64
                        match self.read_rm(mem, rm, 8) {
                            Some(t) => self.rip = t,
                            None => return self.fault(rm),
                        }
                        StepResult::Ok
                    }
                    6 => {
                        // push r/m64
                        let v = match self.read_rm(mem, rm, 8) {
                            Some(v) => v,
                            None => return self.fault(rm),
                        };
                        if !self.push64(mem, v) {
                            return StepResult::Fault { addr: self.regs[RSP] };
                        }
                        self.rip = pc as u64;
                        StepResult::Ok
                    }
                    _ => StepResult::Unknown { rip: start, byte: 0xff },
                }
            }
            // two-byte opcodes (0x0F ...)
            0x0f => {
                let b2 = fetch(pc + 1);
                match b2 {
                    0x05 => {
                        // syscall. In machine mode with LSTAR programmed, do the
                        // architectural thing: save the return RIP in RCX and
                        // RFLAGS in R11, mask RFLAGS by SFMASK, enter ring 0 at
                        // LSTAR. Otherwise (usermode/seed path) trap to the host.
                        let next = (pc + 2) as u64;
                        if self.machine_mode && self.lstar != 0 {
                            if self.trace_sys {
                                let svc = self.regs[RAX] as u32;
                                self.sys_log.push((svc, self.regs[10]));
                                #[cfg(not(target_arch = "wasm32"))]
                                if svc == 3 {
                                    let (ptr, len) = (self.regs[10], (self.regs[RDX] as usize).min(64));
                                    let mut s = alloc::string::String::new();
                                    for i in 0..len {
                                        if let Ok(p) = mmu::translate(mem, &self.paging, ptr + i as u64, mmu::Access::Read, false) {
                                            s.push(*mem.get(p as usize).unwrap_or(&b'?') as char);
                                        }
                                    }
                                    eprintln!("[svc3 NtCreateFile ptr={:#x} len={} name={:?}]", ptr, self.regs[RDX], s);
                                }
                            }
                            self.regs[RCX] = next;
                            self.regs[11] = self.rflags;
                            self.rflags &= !self.sfmask;
                            self.cpl = 0;
                            self.rip = self.lstar;
                            StepResult::Ok
                        } else {
                            self.rip = next;
                            StepResult::Syscall
                        }
                    }
                    // cmovcc reg, r/m (0F 40..4F): move r/m to reg if condition.
                    0x40..=0x4f => {
                        pc += 2;
                        let (reg, rm, npc) = self.decode_modrm(mem, pc, rex_r, rex_x, rex_b);
                        pc = npc;
                        let v = match self.read_rm(mem, rm, size) {
                            Some(v) => v,
                            None => return self.fault(rm),
                        };
                        if self.cond(b2 & 0x0f) {
                            // write_rm applies the right width rule: 16-bit preserves
                            // the upper 48 bits, 32-bit zero-extends, 64-bit full.
                            self.write_rm(mem, Rm::Reg(reg), v, size);
                        }
                        self.rip = pc as u64;
                        StepResult::Ok
                    }
                    // cmpxchg r/m, reg (0F B1): compare RAX with r/m; if equal,
                    // store reg into r/m and set ZF; else load r/m into RAX.
                    0xb1 => {
                        pc += 2;
                        let (reg, rm, npc) = self.decode_modrm(mem, pc, rex_r, rex_x, rex_b);
                        pc = npc;
                        let dst = match self.read_rm(mem, rm, size) {
                            Some(v) => v,
                            None => return self.fault(rm),
                        };
                        let acc = if size == 4 { self.regs[RAX] & 0xFFFF_FFFF } else { self.regs[RAX] };
                        self.apply_alu(7, acc, dst, size); // cmp -> flags incl. ZF
                        if acc == dst {
                            let src = if size == 4 { self.regs[reg] & 0xFFFF_FFFF } else { self.regs[reg] };
                            if !self.write_rm(mem, rm, src, size) {
                                return self.fault(rm);
                            }
                        } else {
                            self.regs[RAX] = dst;
                        }
                        self.rip = pc as u64;
                        StepResult::Ok
                    }
                    // jcc rel32 (near conditional jump): 0F 80..0F 8F.
                    0x80..=0x8f => {
                        let take = self.cond(b2 & 0x0f);
                        let rel = imm32(pc + 2);
                        let next = (pc as u64 + 6).wrapping_add(if take { rel as u64 } else { 0 });
                        self.rip = next;
                        StepResult::Ok
                    }
                    // setcc r/m8 (0F 90..0F 9F): set the byte to 0/1 by condition.
                    0x90..=0x9f => {
                        let val = if self.cond(b2 & 0x0f) { 1u64 } else { 0 };
                        pc += 2;
                        let (_, rm, npc) = self.decode_modrm(mem, pc, rex_r, rex_x, rex_b);
                        pc = npc;
                        match rm {
                            Rm::Reg(r) => self.regs[r] = (self.regs[r] & !0xff) | val,
                            Rm::Mem(a) => {
                                if !self.store(mem, a, val, 1) {
                                    return StepResult::Fault { addr: a };
                                }
                            }
                        }
                        self.rip = pc as u64;
                        StepResult::Ok
                    }
                    // movzx/movsx reg, r/m8 (B6/BE) and r/m16 (B7/BF).
                    0xb6 | 0xb7 | 0xbe | 0xbf => {
                        pc += 2;
                        let (reg, rm, npc) = self.decode_modrm(mem, pc, rex_r, rex_x, rex_b);
                        pc = npc;
                        let src_size: u8 = if b2 == 0xb6 || b2 == 0xbe { 1 } else { 2 };
                        let raw = match self.read_rm(mem, rm, src_size) {
                            Some(v) => v,
                            None => return self.fault(rm),
                        };
                        let bits = src_size as u32 * 8;
                        let low = raw & ((1u64 << bits) - 1);
                        let val = if b2 == 0xbe || b2 == 0xbf {
                            // sign-extend: flip the sign bit then subtract it.
                            let m = 1u64 << (bits - 1);
                            (low ^ m).wrapping_sub(m)
                        } else {
                            low // zero-extend
                        };
                        self.regs[reg] = if size == 4 { val & 0xFFFF_FFFF } else { val };
                        self.rip = pc as u64;
                        StepResult::Ok
                    }
                    // bit-test group (0F BA /digit, imm8): /4 bt /5 bts /6 btr
                    // /7 btc. CF = the tested bit; set/reset/complement write back.
                    0xba => {
                        pc += 2;
                        let (sel, rm, npc) = self.decode_modrm_imm(mem, pc, rex_r, rex_x, rex_b, 1);
                        pc = npc;
                        let imm = fetch(pc) as u32;
                        pc += 1;
                        let bit = imm & (size as u32 * 8 - 1);
                        let v = match self.read_rm(mem, rm, size) {
                            Some(v) => v,
                            None => return self.fault(rm),
                        };
                        self.set_flag(CF, (v >> bit) & 1 == 1);
                        let res = match sel & 7 {
                            5 => Some(v | (1u64 << bit)),  // bts
                            6 => Some(v & !(1u64 << bit)), // btr
                            7 => Some(v ^ (1u64 << bit)),  // btc
                            _ => None,                     // bt (4): no writeback
                        };
                        if let Some(r) = res {
                            if !self.write_rm(mem, rm, r, size) {
                                return self.fault(rm);
                            }
                        }
                        self.rip = pc as u64;
                        StepResult::Ok
                    }
                    // multi-byte NOP (0F 1F /0) — consume the ModRM operand, do
                    // nothing. Used by compilers for alignment padding.
                    0x1f => {
                        pc += 2;
                        let (_, _, npc) = self.decode_modrm(mem, pc, rex_r, rex_x, rex_b);
                        self.rip = npc as u64;
                        StepResult::Ok
                    }
                    // SSE 128-bit moves: load to xmm (0F 10 movups, 0F 28 movaps,
                    // 66/F3 0F 6F movdqa/u) and store from xmm (0F 11, 0F 29,
                    // 66/F3 0F 7F). We move the full 128 bits (enough for the
                    // CRT's buffer zero/copy).
                    0x10 | 0x28 | 0x6f => {
                        pc += 2;
                        let (reg, rm, npc) = self.decode_modrm(mem, pc, rex_r, rex_x, rex_b);
                        pc = npc;
                        let v = match self.read_xmm_rm(mem, rm) {
                            Some(v) => v,
                            None => return self.fault(rm),
                        };
                        // movss (F3 0F 10) / movsd (F2 0F 10): scalar load — only
                        // the low 32/64 bits move. A memory source zeroes the rest;
                        // a register source preserves the destination's high bits.
                        if b2 == 0x10 && (rep || repne) {
                            let from_mem = matches!(rm, Rm::Mem(_));
                            let width = if repne { 64 } else { 32 };
                            let lomask: u128 = if width == 64 { u64::MAX as u128 } else { 0xFFFF_FFFF };
                            let low = v & lomask;
                            self.xmm[reg] = if from_mem { low } else { (self.xmm[reg] & !lomask) | low };
                        } else {
                            self.xmm[reg] = v;
                        }
                        self.rip = pc as u64;
                        StepResult::Ok
                    }
                    0x11 | 0x29 | 0x7f => {
                        pc += 2;
                        let (reg, rm, npc) = self.decode_modrm(mem, pc, rex_r, rex_x, rex_b);
                        pc = npc;
                        // movss/movsd store: only the low 32/64 bits to memory.
                        if b2 == 0x11 && (rep || repne) {
                            if let Rm::Mem(a) = rm {
                                let sz = if repne { 8 } else { 4 };
                                let v = self.xmm[reg] as u64;
                                if !self.store(mem, a, v, sz) {
                                    return StepResult::Fault { addr: a };
                                }
                                self.rip = pc as u64;
                                return StepResult::Ok;
                            }
                            // reg dest: scalar move preserving high bits.
                            if let Rm::Reg(r) = rm {
                                let lomask: u128 = if repne { u64::MAX as u128 } else { 0xFFFF_FFFF };
                                self.xmm[r] = (self.xmm[r] & !lomask) | (self.xmm[reg] & lomask);
                                self.rip = pc as u64;
                                return StepResult::Ok;
                            }
                        }
                        let v = self.xmm[reg];
                        if !self.write_xmm_rm(mem, rm, v) {
                            return self.fault(rm);
                        }
                        self.rip = pc as u64;
                        StepResult::Ok
                    }
                    // xorps (0F 57) / xorpd (66 0F 57) / pxor (66 0F EF): 128-bit
                    // bitwise xor — `xorps xmm,xmm` is the idiom for zeroing.
                    0x57 | 0xef => {
                        pc += 2;
                        let (reg, rm, npc) = self.decode_modrm(mem, pc, rex_r, rex_x, rex_b);
                        pc = npc;
                        let v = match self.read_xmm_rm(mem, rm) {
                            Some(v) => v,
                            None => return self.fault(rm),
                        };
                        self.xmm[reg] ^= v;
                        self.rip = pc as u64;
                        StepResult::Ok
                    }
                    // movd/movq xmm, r/m (66 0F 6E): zero-extend a 32/64-bit GPR
                    // or memory operand into the low bits of xmm, clearing the rest.
                    0x6e => {
                        pc += 2;
                        let (reg, rm, npc) = self.decode_modrm(mem, pc, rex_r, rex_x, rex_b);
                        pc = npc;
                        let sz = if rex_w { 8 } else { 4 };
                        let v = match self.read_rm(mem, rm, sz) {
                            Some(v) => v,
                            None => return self.fault(rm),
                        };
                        self.xmm[reg] = v as u128;
                        self.rip = pc as u64;
                        StepResult::Ok
                    }
                    // 0F 7E: F3 prefix → movq xmm, xmm/m64 (load low 64, zero high);
                    // otherwise (66) → movd/movq r/m, xmm (store xmm low 32/64).
                    0x7e => {
                        pc += 2;
                        let (reg, rm, npc) = self.decode_modrm(mem, pc, rex_r, rex_x, rex_b);
                        pc = npc;
                        if rep {
                            let v = match rm {
                                Rm::Reg(r) => self.xmm[r] as u64,
                                Rm::Mem(a) => match self.load(mem, a, 8) {
                                    Some(v) => v,
                                    None => return self.fault(rm),
                                },
                            };
                            self.xmm[reg] = v as u128;
                        } else {
                            let sz = if rex_w { 8 } else { 4 };
                            let v = self.xmm[reg] as u64;
                            if !self.write_rm(mem, rm, v, sz) {
                                return self.fault(rm);
                            }
                        }
                        self.rip = pc as u64;
                        StepResult::Ok
                    }
                    // SSE 64-bit lane moves. 0F 12 movlps/movhlps, 0F 13 movlps
                    // store, 0F 16 movhps/movlhps, 0F 17 movhps store. The 64-bit
                    // move semantics are the same for the ps/pd variants we need.
                    0x12 | 0x16 => {
                        let high = b2 == 0x16; // 0x16 targets the high lane
                        pc += 2;
                        let (reg, rm, npc) = self.decode_modrm(mem, pc, rex_r, rex_x, rex_b);
                        pc = npc;
                        let src64 = match rm {
                            // reg form: 0F12 movhlps (use src high), 0F16 movlhps (src low).
                            Rm::Reg(r) => {
                                if high { self.xmm[r] as u64 } else { (self.xmm[r] >> 64) as u64 }
                            }
                            Rm::Mem(a) => match self.load(mem, a, 8) {
                                Some(v) => v,
                                None => return self.fault(rm),
                            },
                        };
                        let lo = self.xmm[reg] as u64;
                        let hi = (self.xmm[reg] >> 64) as u64;
                        self.xmm[reg] = if high {
                            (lo as u128) | ((src64 as u128) << 64)
                        } else {
                            (src64 as u128) | ((hi as u128) << 64)
                        };
                        self.rip = pc as u64;
                        StepResult::Ok
                    }
                    0x13 | 0x17 => {
                        let high = b2 == 0x17;
                        pc += 2;
                        let (reg, rm, npc) = self.decode_modrm(mem, pc, rex_r, rex_x, rex_b);
                        pc = npc;
                        let v = if high { (self.xmm[reg] >> 64) as u64 } else { self.xmm[reg] as u64 };
                        match rm {
                            Rm::Mem(a) => {
                                if !self.store(mem, a, v, 8) {
                                    return StepResult::Fault { addr: a };
                                }
                            }
                            Rm::Reg(_) => return StepResult::Unknown { rip: start, byte: 0x0f },
                        }
                        self.rip = pc as u64;
                        StepResult::Ok
                    }
                    // pcmpeqb/w/d (66 0F 74/75/76): packed equality compare,
                    // setting each element to all-ones on match. Used by SSE
                    // string scans (strlen/memchr).
                    0x74 | 0x75 | 0x76 => {
                        let esz = match b2 {
                            0x74 => 1,
                            0x75 => 2,
                            _ => 4,
                        };
                        pc += 2;
                        let (reg, rm, npc) = self.decode_modrm(mem, pc, rex_r, rex_x, rex_b);
                        pc = npc;
                        let src = match self.read_xmm_rm(mem, rm) {
                            Some(v) => v,
                            None => return self.fault(rm),
                        };
                        let dst = self.xmm[reg];
                        let mut res = 0u128;
                        let mut i = 0;
                        while i < 16 {
                            let sh = i * 8;
                            let m: u128 = if esz == 1 {
                                0xff
                            } else if esz == 2 {
                                0xffff
                            } else {
                                0xffff_ffff
                            } << sh;
                            if dst & m == src & m {
                                res |= m;
                            }
                            i += esz;
                        }
                        self.xmm[reg] = res;
                        self.rip = pc as u64;
                        StepResult::Ok
                    }
                    // pmovmskb (66 0F D7): gather the high bit of each of the 16
                    // bytes of an xmm register into a GPR.
                    0xd7 => {
                        pc += 2;
                        let (reg, rm, npc) = self.decode_modrm(mem, pc, rex_r, rex_x, rex_b);
                        pc = npc;
                        let src = match rm {
                            Rm::Reg(r) => self.xmm[r],
                            Rm::Mem(_) => return StepResult::Unknown { rip: start, byte: 0x0f },
                        };
                        let mut mask = 0u64;
                        for i in 0..16 {
                            if (src >> (i * 8 + 7)) & 1 == 1 {
                                mask |= 1 << i;
                            }
                        }
                        self.regs[reg] = mask; // zero-extended into the 64-bit GPR
                        self.rip = pc as u64;
                        StepResult::Ok
                    }
                    // --- SSE2 scalar floating-point -------------------------
                    // Prefix selects width: F2 (repne) = double, F3 (rep) =
                    // single. xmm low 64/32 bits hold the scalar; the rest is
                    // preserved on register writes.
                    //
                    // cvtsi2sd/ss (F2/F3 0F 2A): integer (GPR/mem) → scalar float.
                    0x2a => {
                        pc += 2;
                        let (reg, rm, npc) = self.decode_modrm(mem, pc, rex_r, rex_x, rex_b);
                        pc = npc;
                        let isz = if rex_w { 8 } else { 4 };
                        let iv = match self.read_rm(mem, rm, isz) {
                            Some(v) => v,
                            None => return self.fault(rm),
                        };
                        let signed = if isz == 8 { iv as i64 } else { iv as i32 as i64 };
                        if repne {
                            let f = signed as f64;
                            self.xmm[reg] = (self.xmm[reg] & !0xFFFF_FFFF_FFFF_FFFF) | f.to_bits() as u128;
                        } else {
                            let f = signed as f32;
                            self.xmm[reg] = (self.xmm[reg] & !0xFFFF_FFFF) | f.to_bits() as u128;
                        }
                        self.rip = pc as u64;
                        StepResult::Ok
                    }
                    // cvttsd2si/cvtsd2si + ss (F2/F3 0F 2C/2D): scalar float → int.
                    0x2c | 0x2d => {
                        pc += 2;
                        let (reg, rm, npc) = self.decode_modrm(mem, pc, rex_r, rex_x, rex_b);
                        pc = npc;
                        let src = match self.read_xmm_rm(mem, rm) {
                            Some(v) => v,
                            None => return self.fault(rm),
                        };
                        // (2C truncates; 2D rounds. We truncate for both — the CRT
                        // paths that matter use the truncating form.)
                        let val: i64 = if repne {
                            f64::from_bits(src as u64) as i64
                        } else {
                            f32::from_bits(src as u32) as f32 as i64
                        };
                        self.regs[reg] = if rex_w { val as u64 } else { val as u32 as u64 };
                        self.rip = pc as u64;
                        StepResult::Ok
                    }
                    // ucomisd/comisd + ss (66/none 0F 2E/2F): compare scalars,
                    // setting ZF/PF/CF (OF/SF/AF cleared); unordered → ZF=PF=CF=1.
                    0x2e | 0x2f => {
                        pc += 2;
                        let (reg, rm, npc) = self.decode_modrm(mem, pc, rex_r, rex_x, rex_b);
                        pc = npc;
                        let src = match self.read_xmm_rm(mem, rm) {
                            Some(v) => v,
                            None => return self.fault(rm),
                        };
                        let (a, b) = if opsize16 {
                            (f64::from_bits(self.xmm[reg] as u64), f64::from_bits(src as u64))
                        } else {
                            (f32::from_bits(self.xmm[reg] as u32) as f64, f32::from_bits(src as u32) as f64)
                        };
                        self.set_flag(OF, false);
                        self.set_flag(SF, false);
                        let (zf, pf, cf) = if a.is_nan() || b.is_nan() {
                            (true, true, true)
                        } else if a < b {
                            (false, false, true)
                        } else if a > b {
                            (false, false, false)
                        } else {
                            (true, false, false)
                        };
                        self.set_flag(ZF, zf);
                        self.set_flag(PF, pf);
                        self.set_flag(CF, cf);
                        self.rip = pc as u64;
                        StepResult::Ok
                    }
                    // Scalar arithmetic: add/mul/sub/div/min/max ss/sd
                    // (F3/F2 0F 58/59/5C/5E/5D/5F).
                    0x58 | 0x59 | 0x5c | 0x5e | 0x5d | 0x5f => {
                        pc += 2;
                        let (reg, rm, npc) = self.decode_modrm(mem, pc, rex_r, rex_x, rex_b);
                        pc = npc;
                        let src = match self.read_xmm_rm(mem, rm) {
                            Some(v) => v,
                            None => return self.fault(rm),
                        };
                        let dst = self.xmm[reg];
                        if repne {
                            let a = f64::from_bits(dst as u64);
                            let s = f64::from_bits(src as u64);
                            let r = match b2 {
                                0x58 => a + s,
                                0x59 => a * s,
                                0x5c => a - s,
                                0x5e => a / s,
                                0x5d => if s < a { s } else { a },
                                _ => if s > a { s } else { a },
                            };
                            self.xmm[reg] = (dst & !0xFFFF_FFFF_FFFF_FFFF) | r.to_bits() as u128;
                        } else if rep {
                            let a = f32::from_bits(dst as u32);
                            let s = f32::from_bits(src as u32);
                            let r = match b2 {
                                0x58 => a + s,
                                0x59 => a * s,
                                0x5c => a - s,
                                0x5e => a / s,
                                0x5d => if s < a { s } else { a },
                                _ => if s > a { s } else { a },
                            };
                            self.xmm[reg] = (dst & !0xFFFF_FFFF) | r.to_bits() as u128;
                        } else {
                            return StepResult::Unknown { rip: start, byte: 0x0f }; // packed: later
                        }
                        self.rip = pc as u64;
                        StepResult::Ok
                    }
                    // cvtsd2ss / cvtss2sd (F2/F3 0F 5A): convert between double/single.
                    0x5a => {
                        pc += 2;
                        let (reg, rm, npc) = self.decode_modrm(mem, pc, rex_r, rex_x, rex_b);
                        pc = npc;
                        let src = match self.read_xmm_rm(mem, rm) {
                            Some(v) => v,
                            None => return self.fault(rm),
                        };
                        if repne {
                            // sd -> ss
                            let f = f64::from_bits(src as u64) as f32;
                            self.xmm[reg] = (self.xmm[reg] & !0xFFFF_FFFF) | f.to_bits() as u128;
                        } else if rep {
                            // ss -> sd
                            let f = f32::from_bits(src as u32) as f64;
                            self.xmm[reg] = (self.xmm[reg] & !0xFFFF_FFFF_FFFF_FFFF) | f.to_bits() as u128;
                        } else {
                            return StepResult::Unknown { rip: start, byte: 0x0f };
                        }
                        self.rip = pc as u64;
                        StepResult::Ok
                    }
                    // 0F 00 group 6: sldt/str (/0,/1 → store 0), lldt/ltr
                    // (/2,/3 → consume; TR/LDT aren't modeled, TSS RSP0 is set at
                    // boot), verr/verw (/4,/5 → mark accessible via ZF).
                    0x00 => {
                        pc += 2;
                        let (ext, rm, npc) = self.decode_modrm(mem, pc, rex_r, rex_x, rex_b);
                        pc = npc;
                        match ext & 7 {
                            0 | 1 => {
                                if !self.write_rm(mem, rm, 0, 2) {
                                    return self.fault(rm);
                                }
                            }
                            2 | 3 => {
                                let sel = match self.read_rm(mem, rm, 2) {
                                    Some(v) => v,
                                    None => return self.fault(rm),
                                };
                                // ltr (/3): decode the 64-bit TSS descriptor from
                                // the GDT to find the TSS base, so interrupts can
                                // read the live RSP0. (lldt /2 is left a no-op.)
                                if ext & 7 == 3 {
                                    let d = self.gdtr_base + (sel & 0xFFF8);
                                    let lo = self.load(mem, d, 8).unwrap_or(0);
                                    let hi = self.load(mem, d + 8, 8).unwrap_or(0);
                                    self.tr_base = ((lo >> 16) & 0xFF_FFFF)        // base 0..23
                                        | (((lo >> 56) & 0xFF) << 24)             // base 24..31
                                        | ((hi & 0xFFFF_FFFF) << 32); // base 32..63
                                }
                            }
                            4 | 5 => self.set_flag(ZF, true),
                            _ => return StepResult::Unknown { rip: start, byte: 0x00 },
                        }
                        self.rip = pc as u64;
                        StepResult::Ok
                    }
                    // 0F 01 group: lgdt (/2), lidt (/3), and swapgs (modrm F8).
                    0x01 => {
                        let sub = fetch(pc + 2);
                        if sub == 0xf8 {
                            // swapgs: exchange GS base with KernelGSBase.
                            core::mem::swap(&mut self.gs_base, &mut self.kernel_gs_base);
                            self.rip = (pc + 3) as u64;
                            return StepResult::Ok;
                        }
                        if sub == 0xcb || sub == 0xca {
                            // stac (CB) / clac (CA): set/clear RFLAGS.AC, the
                            // SMAP "kernel may touch user pages" override.
                            self.set_flag(AC, sub == 0xcb);
                            self.rip = (pc + 3) as u64;
                            return StepResult::Ok;
                        }
                        pc += 2;
                        let (ext, rm, npc) = self.decode_modrm(mem, pc, rex_r, rex_x, rex_b);
                        pc = npc;
                        let addr = match rm {
                            Rm::Mem(a) => a,
                            Rm::Reg(_) => return StepResult::Unknown { rip: start, byte: 0x01 },
                        };
                        // m16&64: a 16-bit limit followed by a 64-bit base.
                        let limit = match self.load(mem, addr, 2) {
                            Some(v) => v as u16,
                            None => return StepResult::Fault { addr },
                        };
                        let base = match self.load(mem, addr + 2, 8) {
                            Some(v) => v,
                            None => return StepResult::Fault { addr: addr + 2 },
                        };
                        match ext & 7 {
                            2 => {
                                self.gdtr_base = base;
                                self.gdtr_limit = limit;
                            }
                            3 => {
                                self.idtr_base = base;
                                self.idtr_limit = limit;
                            }
                            _ => return StepResult::Unknown { rip: start, byte: 0x01 },
                        }
                        self.rip = pc as u64;
                        StepResult::Ok
                    }
                    // sysret: return to ring 3 — RIP from RCX, RFLAGS from R11.
                    0x07 => {
                        if self.trace_sys {
                            self.sys_log.push((0xFFFF_FFFF, self.regs[RAX]));
                        }
                        self.rip = self.regs[RCX];
                        self.rflags = self.regs[11];
                        self.cpl = 3;
                        StepResult::Ok
                    }
                    // mov reg, cr (0F 20) and mov cr, reg (0F 22). 64-bit operand.
                    0x20 | 0x22 => {
                        let to_cr = b2 == 0x22;
                        pc += 2;
                        let modrm = fetch(pc);
                        pc += 1;
                        let cr_num = ((modrm >> 3) & 7) as usize + if rex_r { 8 } else { 0 };
                        let gpr = (modrm & 7) as usize + if rex_b { 8 } else { 0 };
                        if to_cr {
                            let v = self.regs[gpr];
                            match cr_num {
                                0 => self.paging.cr0 = v,
                                2 => self.cr2 = v,
                                3 => self.paging.cr3 = v,
                                4 => self.paging.cr4 = v,
                                8 => self.cr8 = v & 0xF, // IRQL / task priority class
                                _ => {}
                            }
                            self.recompute_lma();
                        } else {
                            self.regs[gpr] = match cr_num {
                                0 => self.paging.cr0,
                                2 => self.cr2,
                                3 => self.paging.cr3,
                                4 => self.paging.cr4,
                                8 => self.cr8,
                                _ => 0,
                            };
                        }
                        self.rip = pc as u64;
                        StepResult::Ok
                    }
                    // wrmsr (0F 30): MSR[ECX] = EDX:EAX.
                    0x30 => {
                        let idx = (self.regs[RCX] & 0xFFFF_FFFF) as u32;
                        let val = ((self.regs[RDX] & 0xFFFF_FFFF) << 32)
                            | (self.regs[RAX] & 0xFFFF_FFFF);
                        self.write_msr(idx, val);
                        self.rip = (pc + 2) as u64;
                        StepResult::Ok
                    }
                    // rdmsr (0F 32): EDX:EAX = MSR[ECX].
                    0x32 => {
                        let idx = (self.regs[RCX] & 0xFFFF_FFFF) as u32;
                        let val = self.read_msr(idx);
                        self.regs[RAX] = val & 0xFFFF_FFFF;
                        self.regs[RDX] = val >> 32;
                        self.rip = (pc + 2) as u64;
                        StepResult::Ok
                    }
                    // rdtsc (0F 31): EDX:EAX = a monotonic counter (use icount).
                    0x31 => {
                        let tsc = self.icount;
                        self.regs[RAX] = tsc & 0xFFFF_FFFF;
                        self.regs[RDX] = tsc >> 32;
                        self.rip = (pc + 2) as u64;
                        StepResult::Ok
                    }
                    // clts (0F 06), wbinvd (0F 09), invd (0F 08): treat as no-ops.
                    0x06 | 0x08 | 0x09 => {
                        self.rip = (pc + 2) as u64;
                        StepResult::Ok
                    }
                    // lfence/sfence/mfence/clflush (0F AE /r): memory barriers — nop.
                    0xae => {
                        pc += 2;
                        let (_, _, npc) = self.decode_modrm(mem, pc, rex_r, rex_x, rex_b);
                        self.rip = npc as u64;
                        StepResult::Ok
                    }
                    // cpuid (0F A2): report a minimal but plausible 64-bit CPU.
                    0xa2 => {
                        let leaf = self.regs[RAX] as u32;
                        let (a, b, c, d) = self.cpuid(leaf, self.regs[RCX] as u32);
                        self.regs[RAX] = a as u64;
                        self.regs[RBX] = b as u64;
                        self.regs[RCX] = c as u64;
                        self.regs[RDX] = d as u64;
                        self.rip = (pc + 2) as u64;
                        StepResult::Ok
                    }
                    // imul reg, r/m (0F AF): two-operand signed multiply.
                    0xaf => {
                        pc += 2;
                        let (reg, rm, npc) = self.decode_modrm(mem, pc, rex_r, rex_x, rex_b);
                        pc = npc;
                        let src = match self.read_rm(mem, rm, size) {
                            Some(v) => v,
                            None => return self.fault(rm),
                        };
                        let a = sign_ext(self.regs[reg], size) as i128;
                        let bb = sign_ext(src, size) as i128;
                        let full = a * bb;
                        let prod = full as u64;
                        self.regs[reg] = if size == 4 { prod & 0xFFFF_FFFF } else { prod };
                        // CF=OF when the true product does not fit in the result.
                        let lo = sign_ext((full as u128 & mask128(size)) as u64, size) as i128;
                        let ovf = full != lo;
                        self.set_flag(CF, ovf);
                        self.set_flag(OF, ovf);
                        self.rip = pc as u64;
                        StepResult::Ok
                    }
                    // bsf/bsr (0F BC/BD): bit scan forward/reverse.
                    0xbc | 0xbd => {
                        pc += 2;
                        let (reg, rm, npc) = self.decode_modrm(mem, pc, rex_r, rex_x, rex_b);
                        pc = npc;
                        let v = match self.read_rm(mem, rm, size) {
                            Some(v) => v,
                            None => return self.fault(rm),
                        };
                        let mask = if size == 4 { 0xFFFF_FFFF } else { u64::MAX };
                        let v = v & mask;
                        if v == 0 {
                            self.set_flag(ZF, true);
                        } else {
                            self.set_flag(ZF, false);
                            let idx =
                                if b2 == 0xbc { v.trailing_zeros() } else { 63 - v.leading_zeros() };
                            self.regs[reg] =
                                if size == 4 { idx as u64 & 0xFFFF_FFFF } else { idx as u64 };
                        }
                        self.rip = pc as u64;
                        StepResult::Ok
                    }
                    // bt/bts/btr/btc with a register bit index (0F A3/AB/B3/BB).
                    0xa3 | 0xab | 0xb3 | 0xbb => {
                        pc += 2;
                        let (reg, rm, npc) = self.decode_modrm(mem, pc, rex_r, rex_x, rex_b);
                        pc = npc;
                        let bit = self.regs[reg] & (size as u64 * 8 - 1);
                        let v = match self.read_rm(mem, rm, size) {
                            Some(v) => v,
                            None => return self.fault(rm),
                        };
                        self.set_flag(CF, (v >> bit) & 1 == 1);
                        let res = match b2 {
                            0xab => Some(v | (1u64 << bit)),  // bts
                            0xb3 => Some(v & !(1u64 << bit)), // btr
                            0xbb => Some(v ^ (1u64 << bit)),  // btc
                            _ => None,                        // bt
                        };
                        if let Some(r) = res {
                            if !self.write_rm(mem, rm, r, size) {
                                return self.fault(rm);
                            }
                        }
                        self.rip = pc as u64;
                        StepResult::Ok
                    }
                    // cmpxchg r/m8, r8 (0F B0).
                    0xb0 => {
                        pc += 2;
                        let (reg, rm, npc) = self.decode_modrm(mem, pc, rex_r, rex_x, rex_b);
                        pc = npc;
                        let dst = match self.read_rm(mem, rm, 1) {
                            Some(v) => v & 0xff,
                            None => return self.fault(rm),
                        };
                        let acc = self.regs[RAX] & 0xff;
                        self.apply_alu(7, acc, dst, 1);
                        if acc == dst {
                            let src = self.regs[reg] & 0xff;
                            if !self.write_rm(mem, rm, src, 1) {
                                return self.fault(rm);
                            }
                        } else {
                            self.regs[RAX] = (self.regs[RAX] & !0xff) | dst;
                        }
                        self.rip = pc as u64;
                        StepResult::Ok
                    }
                    // xadd (0F C0 r/m8,r8 ; 0F C1 r/m,reg): exchange-and-add.
                    0xc0 | 0xc1 => {
                        let sz = if b2 == 0xc0 { 1 } else { size };
                        pc += 2;
                        let (reg, rm, npc) = self.decode_modrm(mem, pc, rex_r, rex_x, rex_b);
                        pc = npc;
                        let dst = match self.read_rm(mem, rm, sz) {
                            Some(v) => v,
                            None => return self.fault(rm),
                        };
                        let src = self.regs[reg] & mask128(sz) as u64;
                        let (sum, _) = self.apply_alu(0, dst, src, sz);
                        if !self.write_rm(mem, rm, sum, sz) {
                            return self.fault(rm);
                        }
                        self.write_rm(mem, Rm::Reg(reg), dst, sz);
                        self.rip = pc as u64;
                        StepResult::Ok
                    }
                    // movnti (0F C3): non-temporal store r/m, reg.
                    0xc3 => {
                        pc += 2;
                        let (reg, rm, npc) = self.decode_modrm(mem, pc, rex_r, rex_x, rex_b);
                        pc = npc;
                        let v = self.regs[reg];
                        if !self.write_rm(mem, rm, v, size) {
                            return self.fault(rm);
                        }
                        self.rip = pc as u64;
                        StepResult::Ok
                    }
                    // prefetch (0F 18 /r) and friends: consume ModRM, no-op.
                    0x18 => {
                        pc += 2;
                        let (_, _, npc) = self.decode_modrm(mem, pc, rex_r, rex_x, rex_b);
                        self.rip = npc as u64;
                        StepResult::Ok
                    }
                    _ => StepResult::Unknown { rip: start, byte: 0x0f },
                }
            }
            // --- one-byte system / I/O instructions ----------------------
            // in al,dx (EC) / in eax,dx (ED) / in al,imm8 (E4) / in eax,imm8 (E5)
            0xe4 | 0xe5 | 0xec | 0xed => {
                let (port, isz) = match b {
                    0xe4 => {
                        let p = fetch(pc + 1) as u16;
                        pc += 2;
                        (p, 1u8)
                    }
                    0xe5 => {
                        let p = fetch(pc + 1) as u16;
                        pc += 2;
                        (p, if opsize16 { 2 } else { 4 })
                    }
                    0xec => {
                        pc += 1;
                        ((self.regs[RDX] & 0xFFFF) as u16, 1)
                    }
                    _ => {
                        pc += 1;
                        ((self.regs[RDX] & 0xFFFF) as u16, if opsize16 { 2 } else { 4 })
                    }
                };
                let v = self.dev.port_in(port, isz);
                let m = if isz >= 8 { u64::MAX } else { (1u64 << (isz as u32 * 8)) - 1 };
                self.regs[RAX] = (self.regs[RAX] & !m) | (v & m);
                if isz == 4 {
                    self.regs[RAX] &= 0xFFFF_FFFF; // 32-bit writes zero the high half
                }
                self.rip = pc as u64;
                StepResult::Ok
            }
            // out dx,al (EE) / out dx,eax (EF) / out imm8,al (E6) / out imm8,eax (E7)
            0xe6 | 0xe7 | 0xee | 0xef => {
                let (port, isz) = match b {
                    0xe6 => {
                        let p = fetch(pc + 1) as u16;
                        pc += 2;
                        (p, 1u8)
                    }
                    0xe7 => {
                        let p = fetch(pc + 1) as u16;
                        pc += 2;
                        (p, if opsize16 { 2 } else { 4 })
                    }
                    0xee => {
                        pc += 1;
                        ((self.regs[RDX] & 0xFFFF) as u16, 1)
                    }
                    _ => {
                        pc += 1;
                        ((self.regs[RDX] & 0xFFFF) as u16, if opsize16 { 2 } else { 4 })
                    }
                };
                let m = if isz >= 8 { u64::MAX } else { (1u64 << (isz as u32 * 8)) - 1 };
                self.dev.port_out(port, self.regs[RAX] & m, isz);
                self.rip = pc as u64;
                StepResult::Ok
            }
            // cli (FA) / sti (FB): clear / set the interrupt flag (RFLAGS.IF).
            0xfa => {
                self.set_flag(IF, false);
                self.rip = (pc + 1) as u64;
                StepResult::Ok
            }
            0xfb => {
                self.set_flag(IF, true);
                self.rip = (pc + 1) as u64;
                StepResult::Ok
            }
            // hlt (F4): idle until an interrupt — the run loop handles wake-up.
            0xf4 => {
                self.rip = (pc + 1) as u64;
                StepResult::Hlt
            }
            // int imm8 (CD): software interrupt through the IDT.
            0xcd => {
                let vector = fetch(pc + 1);
                self.rip = (pc + 2) as u64;
                if self.deliver_interrupt(mem, vector, None) {
                    StepResult::Ok
                } else {
                    StepResult::Fault { addr: self.idtr_base }
                }
            }
            // far return (CB = retfq, CA = retf imm16): pop RIP then CS. Used to
            // reload CS after the kernel installs its own GDT.
            0xca | 0xcb => {
                let new_rip = match self.pop64(mem) {
                    Some(v) => v,
                    None => return StepResult::Fault { addr: self.regs[RSP] },
                };
                let cs = self.pop64(mem).unwrap_or(0);
                if b == 0xca {
                    let imm = fetch(pc + 1) as u64 | ((fetch(pc + 2) as u64) << 8);
                    self.regs[RSP] = self.regs[RSP].wrapping_add(imm);
                }
                self.rip = new_rip;
                self.cpl = (cs & 3) as u8;
                StepResult::Ok
            }
            // iretq (CF): pop RIP/CS/RFLAGS/RSP/SS and return.
            0xcf => {
                let rip = match self.pop64(mem) {
                    Some(v) => v,
                    None => return StepResult::Fault { addr: self.regs[RSP] },
                };
                let cs = self.pop64(mem).unwrap_or(0);
                let rflags = self.pop64(mem).unwrap_or(self.rflags);
                let rsp = self.pop64(mem).unwrap_or(self.regs[RSP]);
                let _ss = self.pop64(mem).unwrap_or(0);
                self.rip = rip;
                self.rflags = rflags;
                self.regs[RSP] = rsp;
                self.cpl = (cs & 3) as u8;
                StepResult::Ok
            }
            // 8-bit two-operand ALU (the even-opcode siblings of 0x01/0x03…):
            // 0x00 r/m8,r8 ; 0x02 r8,r/m8 ; …; 0x38/0x3A cmp. op=(b>>3)&7,
            // direction = b&2.
            0x00 | 0x02 | 0x08 | 0x0a | 0x10 | 0x12 | 0x18 | 0x1a | 0x20 | 0x22 | 0x28 | 0x2a
            | 0x30 | 0x32 | 0x38 | 0x3a => {
                let sel = (b >> 3) & 7;
                let to_reg = b & 2 != 0;
                pc += 1;
                let (reg, rm, npc) = self.decode_modrm(mem, pc, rex_r, rex_x, rex_b);
                pc = npc;
                let rmv = match self.read_rm(mem, rm, 1) {
                    Some(v) => v & 0xff,
                    None => return self.fault(rm),
                };
                let regv = self.regs[reg] & 0xff;
                let (a, src) = if to_reg { (regv, rmv) } else { (rmv, regv) };
                let (res, wb) = self.apply_alu(sel, a, src, 1);
                if wb {
                    let ok = if to_reg {
                        self.write_rm(mem, Rm::Reg(reg), res, 1)
                    } else {
                        self.write_rm(mem, rm, res, 1)
                    };
                    if !ok {
                        return self.fault(rm);
                    }
                }
                self.rip = pc as u64;
                StepResult::Ok
            }
            // immediate-group ALU, 8-bit: 0x80 r/m8, imm8.
            0x80 => {
                pc += 1;
                let (sel, rm, npc) = self.decode_modrm_imm(mem, pc, rex_r, rex_x, rex_b, 1);
                pc = npc;
                let imm = fetch(pc) as u64 & 0xff;
                pc += 1;
                let a = match self.read_rm(mem, rm, 1) {
                    Some(v) => v & 0xff,
                    None => return self.fault(rm),
                };
                let (res, wb) = self.apply_alu((sel & 7) as u8, a, imm, 1);
                if wb && !self.write_rm(mem, rm, res, 1) {
                    return self.fault(rm);
                }
                self.rip = pc as u64;
                StepResult::Ok
            }
            // xchg r/m8, r8 (0x86).
            0x86 => {
                pc += 1;
                let (reg, rm, npc) = self.decode_modrm(mem, pc, rex_r, rex_x, rex_b);
                pc = npc;
                let rmv = match self.read_rm(mem, rm, 1) {
                    Some(v) => v & 0xff,
                    None => return self.fault(rm),
                };
                let regv = self.regs[reg] & 0xff;
                if !self.write_rm(mem, rm, regv, 1) {
                    return self.fault(rm);
                }
                self.regs[reg] = (self.regs[reg] & !0xff) | rmv;
                self.rip = pc as u64;
                StepResult::Ok
            }
            // pushfq (0x9C) / popfq (0x9D): push/pop RFLAGS (64-bit).
            0x9c => {
                let f = (self.rflags & 0x0021_4FD5) | 0x2; // keep defined bits, bit1 reads 1
                if !self.push64(mem, f) {
                    return StepResult::Fault { addr: self.regs[RSP] };
                }
                self.rip = (pc + 1) as u64;
                StepResult::Ok
            }
            0x9d => {
                match self.pop64(mem) {
                    Some(v) => self.rflags = (v & 0x0021_4FD5) | 0x2,
                    None => return StepResult::Fault { addr: self.regs[RSP] },
                }
                self.rip = (pc + 1) as u64;
                StepResult::Ok
            }
            // sahf (0x9E) / lahf (0x9F): AH ↔ low byte of flags.
            0x9e => {
                let ah = (self.regs[RAX] >> 8) & 0xff;
                self.rflags = (self.rflags & !0xD5) | (ah & 0xD5) | 0x2;
                self.rip = (pc + 1) as u64;
                StepResult::Ok
            }
            0x9f => {
                let lo = (self.rflags & 0xD5) | 0x2;
                self.regs[RAX] = (self.regs[RAX] & !0xff00) | (lo << 8);
                self.rip = (pc + 1) as u64;
                StepResult::Ok
            }
            // clc/stc/cmc (0xF8/0xF9/0xF5): clear/set/complement carry.
            0xf8 => {
                self.set_flag(CF, false);
                self.rip = (pc + 1) as u64;
                StepResult::Ok
            }
            0xf9 => {
                self.set_flag(CF, true);
                self.rip = (pc + 1) as u64;
                StepResult::Ok
            }
            0xf5 => {
                let c = self.flag(CF);
                self.set_flag(CF, !c);
                self.rip = (pc + 1) as u64;
                StepResult::Ok
            }
            // cld (0xFC) / std (0xFD): clear / set the direction flag.
            0xfc => {
                self.set_flag(DF, false);
                self.rip = (pc + 1) as u64;
                StepResult::Ok
            }
            0xfd => {
                self.set_flag(DF, true);
                self.rip = (pc + 1) as u64;
                StepResult::Ok
            }
            // String ops with optional REP. movs (A4/A5), stos (AA/AB),
            // lods (AC/AD), scas (AE/AF), cmps (A6/A7). 8-bit when b is even.
            0xa4 | 0xa5 | 0xaa | 0xab | 0xac | 0xad | 0xae | 0xaf | 0xa6 | 0xa7 => {
                pc += 1;
                let sz: u8 = if b & 1 == 0 { 1 } else { size };
                let delta = if self.flag(DF) {
                    (sz as u64).wrapping_neg()
                } else {
                    sz as u64
                };
                // Number of iterations: REP uses RCX, otherwise a single op.
                let mut count = if rep || repne { self.regs[RCX] } else { 1 };
                let is_cmp = matches!(b, 0xa6 | 0xa7 | 0xae | 0xaf);
                let result = loop {
                    if count == 0 {
                        break StepResult::Ok;
                    }
                    let r = match b {
                        0xa4 | 0xa5 => {
                            // movs: [RDI] = [RSI]
                            match self.load(mem, self.regs[RSI], sz) {
                                Some(v) => {
                                    if !self.store(mem, self.regs[RDI], v, sz) {
                                        break StepResult::Fault { addr: self.regs[RDI] };
                                    }
                                    self.regs[RSI] = self.regs[RSI].wrapping_add(delta);
                                    self.regs[RDI] = self.regs[RDI].wrapping_add(delta);
                                    None
                                }
                                None => break StepResult::Fault { addr: self.regs[RSI] },
                            }
                        }
                        0xaa | 0xab => {
                            // stos: [RDI] = AL/eAX
                            let v = self.regs[RAX];
                            if !self.store(mem, self.regs[RDI], v, sz) {
                                break StepResult::Fault { addr: self.regs[RDI] };
                            }
                            self.regs[RDI] = self.regs[RDI].wrapping_add(delta);
                            None
                        }
                        0xac | 0xad => {
                            // lods: AL/eAX = [RSI]
                            match self.load(mem, self.regs[RSI], sz) {
                                Some(v) => {
                                    let m = mask128(sz) as u64;
                                    self.regs[RAX] = (self.regs[RAX] & !m) | (v & m);
                                    self.regs[RSI] = self.regs[RSI].wrapping_add(delta);
                                    None
                                }
                                None => break StepResult::Fault { addr: self.regs[RSI] },
                            }
                        }
                        0xae | 0xaf => {
                            // scas: cmp AL/eAX, [RDI]
                            match self.load(mem, self.regs[RDI], sz) {
                                Some(v) => {
                                    let a = self.regs[RAX] & mask128(sz) as u64;
                                    self.apply_alu(7, a, v, sz);
                                    self.regs[RDI] = self.regs[RDI].wrapping_add(delta);
                                    None
                                }
                                None => break StepResult::Fault { addr: self.regs[RDI] },
                            }
                        }
                        _ => {
                            // cmps (A6/A7): cmp [RSI], [RDI]
                            let lhs = self.load(mem, self.regs[RSI], sz);
                            let rhs = self.load(mem, self.regs[RDI], sz);
                            match (lhs, rhs) {
                                (Some(a), Some(c)) => {
                                    self.apply_alu(7, a, c, sz);
                                    self.regs[RSI] = self.regs[RSI].wrapping_add(delta);
                                    self.regs[RDI] = self.regs[RDI].wrapping_add(delta);
                                    None
                                }
                                _ => break StepResult::Fault { addr: self.regs[RSI] },
                            }
                        }
                    };
                    if let Some(stop) = r {
                        break stop;
                    }
                    count -= 1;
                    if rep || repne {
                        self.regs[RCX] = count;
                        // repe/repne on cmps/scas also stop on the ZF condition.
                        if is_cmp {
                            let zf = self.flag(ZF);
                            if (rep && !zf) || (repne && zf) {
                                break StepResult::Ok;
                            }
                        }
                    }
                };
                if result == StepResult::Ok {
                    self.rip = pc as u64;
                }
                result
            }
            other => StepResult::Unknown { rip: start, byte: other },
        }
    }

    fn fault(&self, rm: Rm) -> StepResult {
        match rm {
            Rm::Mem(a) => StepResult::Fault { addr: a },
            Rm::Reg(_) => StepResult::Fault { addr: 0 },
        }
    }

    /// Recompute EFER.LMA: long mode becomes *active* once it is *enabled*
    /// (EFER.LME) and paging is on (CR0.PG). The CPU sets this latch; software
    /// reads it back via rdmsr.
    fn recompute_lma(&mut self) {
        let lme = self.paging.efer & mmu::EFER_LME != 0;
        let pg = self.paging.cr0 & mmu::CR0_PG != 0;
        if lme && pg {
            self.paging.efer |= mmu::EFER_LMA;
        } else {
            self.paging.efer &= !mmu::EFER_LMA;
        }
    }

    fn read_msr(&self, idx: u32) -> u64 {
        // x2APIC register access (MSR 0x800..=0x83F → APIC MMIO offset).
        if (0x800..=0x83F).contains(&idx) {
            return self.dev.apic.read(((idx - 0x800) * 16) as u64) as u64;
        }
        match idx {
            0x0000_001B => 0x0000_0000_FEE0_0D00, // IA32_APIC_BASE: enabled + x2APIC + BSP
            0x0000_0080 => self.paging.efer,
            0xC000_0080 => self.paging.efer, // IA32_EFER
            0xC000_0081 => self.star,
            0xC000_0082 => self.lstar,
            0xC000_0084 => self.sfmask,
            0xC000_0100 => self.fs_base,
            0xC000_0101 => self.gs_base,
            0xC000_0102 => self.kernel_gs_base,
            _ => 0,
        }
    }
    fn write_msr(&mut self, idx: u32, val: u64) {
        // x2APIC register access (MSR 0x800..=0x83F → APIC MMIO offset).
        if (0x800..=0x83F).contains(&idx) {
            self.dev.apic.write(((idx - 0x800) * 16) as u64, val as u32);
            return;
        }
        match idx {
            0x0000_001B => {} // IA32_APIC_BASE: accept (x2APIC enable etc.)
            0x0000_0080 | 0xC000_0080 => {
                self.paging.efer = val;
                self.recompute_lma();
            }
            0xC000_0081 => self.star = val,
            0xC000_0082 => self.lstar = val,
            0xC000_0084 => self.sfmask = val,
            0xC000_0100 => self.fs_base = val,
            0xC000_0101 => self.gs_base = val,
            0xC000_0102 => self.kernel_gs_base = val,
            _ => {}
        }
    }

    /// Minimal CPUID: vendor + the feature bits a long-mode kernel checks
    /// (PAE/APIC/MSR/TSC/SSE/SSE2, and in the extended leaf SYSCALL/NX/LM).
    /// Returns (eax, ebx, ecx, edx).
    fn cpuid(&self, leaf: u32, _subleaf: u32) -> (u32, u32, u32, u32) {
        match leaf {
            0 => (0x10, 0x756e6547, 0x6c65746e, 0x49656e69), // max leaf + "GenuineIntel"
            1 => {
                let edx = (1 << 0)  // FPU
                    | (1 << 3)      // PSE
                    | (1 << 4)      // TSC
                    | (1 << 5)      // MSR
                    | (1 << 6)      // PAE
                    | (1 << 8)      // CX8
                    | (1 << 9)      // APIC
                    | (1 << 13)     // PGE
                    | (1 << 15)     // CMOV
                    | (1 << 19)     // CLFSH
                    | (1 << 23)     // MMX
                    | (1 << 24)     // FXSR
                    | (1 << 25)     // SSE
                    | (1 << 26); // SSE2
                let ecx = (1 << 0)  // SSE3
                    | (1 << 23); // POPCNT
                (0x0006_03A9, 0, ecx, edx)
            }
            // Structured extended features. EBX: SMEP (bit 7), SMAP (bit 20) —
            // the kernel enables CR4.SMEP/SMAP only when these are advertised.
            7 => (0, (1 << 7) | (1 << 20), 0, 0),
            0x8000_0000 => (0x8000_0008, 0, 0, 0),
            0x8000_0001 => {
                let edx = (1 << 11)  // SYSCALL
                    | (1 << 20)      // NX
                    | (1 << 26)      // 1 GiB pages
                    | (1 << 27)      // RDTSCP
                    | (1 << 29); // Long Mode
                (0, 0, 1 /* LAHF */, edx)
            }
            0x8000_0008 => (0x3028, 0, 0, 0), // 40-bit phys, 48-bit virt
            _ => (0, 0, 0, 0),
        }
    }

    /// Code/stack selector conventions used when building/reversing interrupt
    /// frames. We don't model the GDT's descriptor cache; these are the standard
    /// long-mode selectors (ring0 0x08/0x10, ring3 0x33/0x2B) so that
    /// deliver→iretq round-trips the privilege level.
    fn cs_for(cpl: u8) -> u64 {
        if cpl == 3 { 0x33 } else { 0x08 }
    }
    fn ss_for(cpl: u8) -> u64 {
        if cpl == 3 { 0x2B } else { 0x10 }
    }

    /// Deliver an interrupt/exception through the long-mode IDT: read the 16-byte
    /// gate for `vector`, switch stacks if the privilege level drops, push the
    /// interrupt frame (SS, RSP, RFLAGS, CS, RIP [, error code]), and jump to the
    /// handler. Returns false if the gate or a stack push could not be accessed.
    pub fn deliver_interrupt(&mut self, mem: &mut [u8], vector: u8, error: Option<u64>) -> bool {
        // Every memory access made *by delivery itself* — reading the IDT gate
        // and TSS, pushing the interrupt frame — is a supervisor access, so drop
        // to CPL 0 up front. (Otherwise, on a ring-3 fault, the MMU's U/S check
        // rejects the read of the supervisor IDT/stack and delivery fails.)
        let old_cpl = self.cpl;
        self.cpl = 0;
        let desc = self.idtr_base + vector as u64 * 16;
        let lo = match self.load(mem, desc, 8) {
            Some(v) => v,
            None => return false,
        };
        let hi = match self.load(mem, desc + 8, 8) {
            Some(v) => v,
            None => return false,
        };
        let selector = (lo >> 16) & 0xFFFF;
        let type_attr = (lo >> 40) & 0xFF;
        let offset = (lo & 0xFFFF) | (((lo >> 48) & 0xFFFF) << 16) | ((hi & 0xFFFF_FFFF) << 32);
        if type_attr & 0x80 == 0 {
            return false; // gate not present
        }
        let target_cpl = (selector & 3) as u8;
        let old_rsp = self.regs[RSP];
        if target_cpl < old_cpl {
            // Privilege increased (ring3 → ring0): switch to the kernel stack.
            // Read RSP0 from the live TSS (the kernel rewrites it on every
            // context switch); fall back to the boot value if TR isn't loaded.
            self.regs[RSP] = if self.tr_base != 0 {
                self.load(mem, self.tr_base + 4, 8).unwrap_or(self.tss_rsp0)
            } else {
                self.tss_rsp0
            };
        }
        let ok = self.push64(mem, Self::ss_for(old_cpl))
            && self.push64(mem, old_rsp)
            && self.push64(mem, self.rflags)
            && self.push64(mem, Self::cs_for(old_cpl))
            && self.push64(mem, self.rip)
            && error.map_or(true, |e| self.push64(mem, e));
        if !ok {
            return false;
        }
        self.cpl = target_cpl;
        self.rip = offset;
        // An interrupt gate (type 0xE) clears IF; a trap gate (0xF) leaves it.
        if type_attr & 0xF == 0xE {
            self.set_flag(IF, false);
        }
        true
    }

    /// Evaluate an x86 condition code (low nibble of a jcc/setcc/cmovcc opcode).
    fn cond(&self, cc: u8) -> bool {
        let (cf, zf, sf, of, pf) =
            (self.flag(CF), self.flag(ZF), self.flag(SF), self.flag(OF), self.flag(PF));
        match cc & 0x0f {
            0x0 => of,                 // o
            0x1 => !of,                // no
            0x2 => cf,                 // b/c
            0x3 => !cf,                // ae/nc
            0x4 => zf,                 // e/z
            0x5 => !zf,                // ne/nz
            0x6 => cf || zf,           // be
            0x7 => !cf && !zf,         // a
            0x8 => sf,                 // s
            0x9 => !sf,                // ns
            0xa => pf,                 // p
            0xb => !pf,                // np
            0xc => sf != of,           // l
            0xd => sf == of,           // ge
            0xe => zf || (sf != of),   // le
            _ => !zf && (sf == of),    // g (0xf)
        }
    }

    /// Set up an initial call frame: entry point in `rip`, `rsp` at `stack_top`,
    /// and a `HALT_ADDR` return address pushed so a final `ret` stops the
    /// interpreter (mirrors the native loader's NtTerminateThread return stub).
    pub fn setup_frame(&mut self, mem: &mut [u8], entry: u64, stack_top: u64) {
        self.rip = entry;
        self.regs[RSP] = stack_top;
        self.push64(mem, HALT_ADDR);
    }

    /// Set up a frame and run until Halt/Syscall/Unknown/Fault, capped at
    /// `max_steps`. (A caller that needs to service syscalls should drive
    /// [`Cpu::step`] directly after [`Cpu::setup_frame`].)
    pub fn run_program(&mut self, mem: &mut [u8], entry: u64, stack_top: u64, max_steps: usize) -> StepResult {
        self.setup_frame(mem, entry, stack_top);
        for _ in 0..max_steps {
            match self.step(mem) {
                StepResult::Ok => continue,
                done => return done,
            }
        }
        StepResult::Unknown { rip: self.rip, byte: 0 }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alu_reg() {
        // mov rax,5 ; mov rcx,7 ; add rax,rcx ; sub rax,rcx ; ret
        let mut mem = vec![0u8; 256];
        let code = [
            0x48, 0xb8, 5, 0, 0, 0, 0, 0, 0, 0, 0x48, 0xb9, 7, 0, 0, 0, 0, 0, 0, 0, 0x48, 0x01,
            0xc8, 0x48, 0x29, 0xc8, 0xc3,
        ];
        mem[..code.len()].copy_from_slice(&code);
        assert_eq!(self_run(&mut mem), StepResult::Halt);
    }

    fn self_run(mem: &mut [u8]) -> StepResult {
        let mut cpu = Cpu::new();
        cpu.run_program(mem, 0, 240, 1000)
    }

    #[test]
    fn call_ret() {
        // entry: call +6 (to fn) ; ret      fn: mov rax,42 ; ret
        // layout: [0]=call rel32(=1) -> 0+5+1=6 ; [5]=ret ; [6]=mov rax,42 ; ret
        let mut mem = vec![0u8; 256];
        let code = [
            0xe8, 0x01, 0x00, 0x00, 0x00, // call +1 -> 6
            0xc3, // ret (back to HALT)
            0x48, 0xb8, 42, 0, 0, 0, 0, 0, 0, 0, // mov rax,42
            0xc3, // ret
        ];
        mem[..code.len()].copy_from_slice(&code);
        let mut cpu = Cpu::new();
        assert_eq!(cpu.run_program(&mut mem, 0, 240, 1000), StepResult::Halt);
        assert_eq!(cpu.regs[RAX], 42);
    }

    #[test]
    fn countdown_loop() {
        // mov rax,0 ; mov rcx,3 ; L: add rax,rcx ; sub rcx,1(via dec-ish) ...
        // Simpler: mov rcx,3 ; mov rax,0 ; L: add rax,rcx ; mov rdx,1 ; sub rcx,rdx ; cmp rcx,...
        // Use: rcx counts 3->0, rax += rcx each iter (3+2+1=6).
        // mov rcx,3; mov rax,0; mov rbx,1;
        // L(@idx): add rax,rcx; sub rcx,rbx; jne L; ret
        let mut mem = vec![0u8; 256];
        let mut c = vec![];
        c.extend_from_slice(&[0x48, 0xb9, 3, 0, 0, 0, 0, 0, 0, 0]); // mov rcx,3
        c.extend_from_slice(&[0x48, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0]); // mov rax,0
        c.extend_from_slice(&[0x48, 0xbb, 1, 0, 0, 0, 0, 0, 0, 0]); // mov rbx,1
        let l = c.len();
        c.extend_from_slice(&[0x48, 0x01, 0xc8]); // add rax,rcx
        c.extend_from_slice(&[0x48, 0x29, 0xd9]); // sub rcx,rbx
        // jne rel8 back to L
        let after = c.len() + 2;
        let rel = (l as i64 - after as i64) as i8 as u8;
        c.extend_from_slice(&[0x75, rel]); // jne L
        c.push(0xc3); // ret
        mem[..c.len()].copy_from_slice(&c);
        let mut cpu = Cpu::new();
        assert_eq!(cpu.run_program(&mut mem, 0, 240, 1000), StepResult::Halt);
        assert_eq!(cpu.regs[RAX], 6); // 3+2+1
    }

    #[test]
    fn mem_store_load_via_stack() {
        // push 0xdead-ish via reg, pop into another reg.
        // mov rax,0x1234 ; push rax ; pop rcx ; ret
        let mut mem = vec![0u8; 256];
        let code = [
            0x48, 0xb8, 0x34, 0x12, 0, 0, 0, 0, 0, 0, // mov rax,0x1234
            0x50, // push rax
            0x59, // pop rcx
            0xc3, // ret
        ];
        mem[..code.len()].copy_from_slice(&code);
        let mut cpu = Cpu::new();
        assert_eq!(cpu.run_program(&mut mem, 0, 240, 1000), StepResult::Halt);
        assert_eq!(cpu.regs[RCX], 0x1234);
    }

    #[test]
    fn syscall_traps_with_service_in_eax() {
        // mov eax,7 ; syscall ; ret
        let mut mem = vec![0u8; 256];
        let code = [0xb8, 7, 0, 0, 0, 0x0f, 0x05, 0xc3];
        mem[..code.len()].copy_from_slice(&code);
        let mut cpu = Cpu::new();
        // run_program stops at the syscall (first non-Ok result).
        assert_eq!(cpu.run_program(&mut mem, 0, 240, 1000), StepResult::Syscall);
        assert_eq!(cpu.regs[RAX], 7);
        // resuming continues to the ret -> Halt.
        assert_eq!(cpu.step(&mut mem), StepResult::Halt);
    }

    #[test]
    fn imm_group_alu() {
        // mov rax,5 ; add rax, 0x10 (0x83 /0) ; sub rax, 1 (0x83 /5) ; ret  => 20
        let mut mem = vec![0u8; 256];
        let prog = [
            0x48, 0xb8, 5, 0, 0, 0, 0, 0, 0, 0, // mov rax,5
            0x48, 0x83, 0xc0, 0x10, // add rax, 0x10  -> 21
            0x48, 0x83, 0xe8, 0x01, // sub rax, 1     -> 20
            0xc3,
        ];
        mem[..prog.len()].copy_from_slice(&prog);
        let mut cpu = Cpu::new();
        assert_eq!(cpu.run_program(&mut mem, 0, 240, 1000), StepResult::Halt);
        assert_eq!(cpu.regs[RAX], 20);
    }

    #[test]
    fn cmp_imm_and_jne_rel32_not_taken() {
        // mov rax,7 ; cmp rax,7 (0x83 /7) ; jne rel32 +5 ; mov rcx,1 ; ret
        // equal -> jne not taken -> rcx=1.
        let mut mem = vec![0u8; 256];
        let prog = [
            0x48, 0xb8, 7, 0, 0, 0, 0, 0, 0, 0, // mov rax,7
            0x48, 0x83, 0xf8, 0x07, // cmp rax,7
            0x0f, 0x85, 5, 0, 0, 0, // jne +5 (skips the mov rcx,? if taken)
            0x48, 0xb9, 1, 0, 0, 0, 0, 0, 0, 0, // mov rcx,1
            0xc3,
        ];
        mem[..prog.len()].copy_from_slice(&prog);
        let mut cpu = Cpu::new();
        cpu.run_program(&mut mem, 0, 240, 1000);
        assert_eq!(cpu.regs[RCX], 1); // jne not taken, so rcx set
    }

    #[test]
    fn lea_and_movzx() {
        // mov rbx, 0x1100 ; lea rax, [rbx+0x10] ; ret   -> rax = 0x1110
        // then movzx via a byte in memory.
        let mut mem = vec![0u8; 256];
        let prog = [
            0x48, 0xbb, 0x00, 0x11, 0, 0, 0, 0, 0, 0, // mov rbx, 0x1100
            0x48, 0x8d, 0x43, 0x10, // lea rax, [rbx+0x10]
            0xc3,
        ];
        mem[..prog.len()].copy_from_slice(&prog);
        let mut cpu = Cpu::new();
        assert_eq!(cpu.run_program(&mut mem, 0, 240, 1000), StepResult::Halt);
        assert_eq!(cpu.regs[RAX], 0x1110);
    }

    #[test]
    fn movsx_sign_extends() {
        // mov rax, 0xFF ; movsx rcx, al ... we lack r/m8 reg encoding nuance, so
        // test the decoder path: movzx rcx, al (0F B6 C8) zero-extends 0xFF -> 0xFF;
        // movsx rcx, al (0F BE C8) sign-extends 0xFF -> 0xFFFF...FF.
        let mut mem = vec![0u8; 64];
        // mov rax,0xFF ; movsx rcx, al ; ret
        let prog = [
            0x48, 0xb8, 0xff, 0, 0, 0, 0, 0, 0, 0, // mov rax,0xFF
            0x48, 0x0f, 0xbe, 0xc8, // movsx rcx, al
            0xc3,
        ];
        mem[..prog.len()].copy_from_slice(&prog);
        let mut cpu = Cpu::new();
        cpu.run_program(&mut mem, 0, 56, 1000);
        assert_eq!(cpu.regs[RCX], u64::MAX); // 0xFF sign-extended
    }

    #[test]
    fn unknown_opcode_reported() {
        let mut mem = vec![0x0f, 0x0b]; // ud2
        let mut cpu = Cpu::new();
        assert_eq!(cpu.step(&mut mem), StepResult::Unknown { rip: 0, byte: 0x0f });
    }
}
