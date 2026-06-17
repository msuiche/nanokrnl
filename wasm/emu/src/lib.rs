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
#![cfg_attr(not(test), no_std)]

pub mod pe;

pub const RAX: usize = 0;
pub const RCX: usize = 1;
pub const RDX: usize = 2;
pub const RBX: usize = 3;
pub const RSP: usize = 4;
pub const RBP: usize = 5;

const CF: u64 = 1 << 0;
const PF: u64 = 1 << 2;
const ZF: u64 = 1 << 6;
const SF: u64 = 1 << 7;
const OF: u64 = 1 << 11;

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
    /// Out-of-bounds memory access at `addr` — the program faulted.
    Fault { addr: u64 },
}

/// A decoded ModRM r/m: a register index or an effective memory address.
#[derive(Clone, Copy)]
enum Rm {
    Reg(usize),
    Mem(u64),
}

impl Default for Cpu {
    fn default() -> Self {
        Cpu { regs: [0; 16], rip: 0, rflags: 0, gs_base: 0, fs_base: 0, seg_base: 0, xmm: [0; 16] }
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
        let cin = if self.flag(CF) { 1 } else { 0 };
        let (res, writeback) = match sel {
            0 => (a.wrapping_add(b), true),                 // add
            1 => (a | b, true),                             // or
            2 => (a.wrapping_add(b).wrapping_add(cin), true), // adc
            3 => (a.wrapping_sub(b).wrapping_sub(cin), true), // sbb
            4 => (a & b, true),                             // and
            5 => (a.wrapping_sub(b), true),                 // sub
            6 => (a ^ b, true),                             // xor
            7 => (a.wrapping_sub(b), false),                // cmp
            _ => (a, false),
        };
        match sel {
            0 | 2 => {
                self.set_flag(CF, res < a || (sel == 2 && res == a && b != 0));
                self.set_flag(OF, ((a ^ res) & (b ^ res)) >> 63 & 1 == 1);
            }
            5 | 7 | 3 => {
                self.set_flag(CF, a < b.wrapping_add(if sel == 3 { cin } else { 0 }));
                self.set_flag(OF, ((a ^ b) & (a ^ res)) >> 63 & 1 == 1);
            }
            _ => {
                // logical ops clear CF/OF
                self.set_flag(CF, false);
                self.set_flag(OF, false);
            }
        }
        self.set_zsp(res, size);
        (res, writeback)
    }

    // --- flat little-endian memory ----------------------------------------
    fn load(mem: &[u8], addr: u64, size: u8) -> Option<u64> {
        let a = addr as usize;
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
    fn store(mem: &mut [u8], addr: u64, val: u64, size: u8) -> bool {
        let a = addr as usize;
        let n = size as usize;
        if a + n > mem.len() {
            return false;
        }
        for i in 0..n {
            mem[a + i] = (val >> (i * 8)) as u8;
        }
        true
    }
    fn load128(mem: &[u8], addr: u64) -> Option<u128> {
        let a = addr as usize;
        if a + 16 > mem.len() {
            return None;
        }
        Some(u128::from_le_bytes(mem[a..a + 16].try_into().ok()?))
    }
    fn store128(mem: &mut [u8], addr: u64, val: u128) -> bool {
        let a = addr as usize;
        if a + 16 > mem.len() {
            return false;
        }
        mem[a..a + 16].copy_from_slice(&val.to_le_bytes());
        true
    }
    /// Read a 128-bit SSE operand (an XMM register or a 16-byte memory operand).
    fn read_xmm_rm(&self, mem: &[u8], rm: Rm) -> Option<u128> {
        match rm {
            Rm::Reg(r) => Some(self.xmm[r]),
            Rm::Mem(a) => Self::load128(mem, a),
        }
    }
    fn write_xmm_rm(&mut self, mem: &mut [u8], rm: Rm, val: u128) -> bool {
        match rm {
            Rm::Reg(r) => {
                self.xmm[r] = val;
                true
            }
            Rm::Mem(a) => Self::store128(mem, a, val),
        }
    }

    fn read_rm(&self, mem: &[u8], rm: Rm, size: u8) -> Option<u64> {
        match rm {
            Rm::Reg(r) => {
                let v = self.regs[r];
                Some(if size >= 8 { v } else { v & ((1u64 << (size as u32 * 8)) - 1) })
            }
            Rm::Mem(a) => Self::load(mem, a, size),
        }
    }
    fn write_rm(&mut self, mem: &mut [u8], rm: Rm, val: u64, size: u8) -> bool {
        match rm {
            Rm::Reg(r) => {
                self.regs[r] = if size == 4 { val & 0xFFFF_FFFF } else { val };
                true
            }
            Rm::Mem(a) => Self::store(mem, a, val, size),
        }
    }

    /// Decode a ModRM byte at `pc` for an instruction with no trailing immediate.
    fn decode_modrm(
        &self,
        mem: &[u8],
        pc: usize,
        rex_r: bool,
        rex_x: bool,
        rex_b: bool,
    ) -> (usize, Rm, usize) {
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
        mut pc: usize,
        rex_r: bool,
        rex_x: bool,
        rex_b: bool,
        imm_len: usize,
    ) -> (usize, Rm, usize) {
        let modrm = *mem.get(pc).unwrap_or(&0);
        pc += 1;
        let md = modrm >> 6;
        let reg = ((modrm >> 3) & 7) as usize + if rex_r { 8 } else { 0 };
        let rm = (modrm & 7) as usize;
        if md == 3 {
            return (reg, Rm::Reg(rm + if rex_b { 8 } else { 0 }), pc);
        }
        let rd32 = |p: usize| -> i64 {
            let mut v = 0u32;
            for i in 0..4 {
                v |= (*mem.get(p + i).unwrap_or(&0) as u32) << (i * 8);
            }
            v as i32 as i64
        };
        let mut addr: u64;
        if rm == 4 {
            // SIB byte
            let sib = *mem.get(pc).unwrap_or(&0);
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
            let d = *mem.get(pc).unwrap_or(&0) as i8 as i64;
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
        Self::store(mem, self.regs[RSP], val, 8)
    }
    fn pop64(&mut self, mem: &[u8]) -> Option<u64> {
        let v = Self::load(mem, self.regs[RSP], 8)?;
        self.regs[RSP] = self.regs[RSP].wrapping_add(8);
        Some(v)
    }

    /// Decode and execute one instruction.
    pub fn step(&mut self, mem: &mut [u8]) -> StepResult {
        // An import trap address (bound into IAT slots by the loader) isn't real
        // code — surface it so the host can service the imported function.
        if self.rip >= IMPORT_BASE && self.rip < IMPORT_BASE + IMPORT_MAX * 8 {
            return StepResult::Import { index: ((self.rip - IMPORT_BASE) / 8) as u32 };
        }
        let start = self.rip;
        let mut pc = self.rip as usize;
        let fetch = |i: usize| -> u8 { *mem.get(i).unwrap_or(&0) };
        let imm32 = |p: usize| -> i64 {
            let mut v = 0u32;
            for i in 0..4 {
                v |= (fetch(p + i) as u32) << (i * 8);
            }
            v as i32 as i64
        };

        // Legacy prefixes (segment override sets the operand segment base; the
        // rest are consumed — size/rep semantics handled per-opcode as needed).
        self.seg_base = 0;
        let mut opsize16 = false;
        loop {
            match fetch(pc) {
                0x65 => self.seg_base = self.gs_base,
                0x64 => self.seg_base = self.fs_base,
                0x66 => opsize16 = true,
                // 0xF2/0xF3 are REP/mandatory-SSE prefixes; 0x67/0xF0/seg are
                // consumed. (SSE move variants are handled identically here.)
                0x67 | 0xf0 | 0xf2 | 0xf3 | 0x2e | 0x36 | 0x3e | 0x26 => {}
                _ => break,
            }
            pc += 1;
        }
        // REX prefix (must immediately precede the opcode).
        let mut rex_w = false;
        let (mut rex_r, mut rex_x, mut rex_b) = (false, false, false);
        let mut b = fetch(pc);
        if (0x40..=0x4f).contains(&b) {
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
                            if !Self::store(mem, a, v, 1) {
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
                let imm = imm32(pc) as u64; // sign-extended to 64
                pc += 4;
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
                let imm = imm32(pc) as u64;
                pc += 4;
                let a = if size == 4 { self.regs[RAX] & 0xFFFF_FFFF } else { self.regs[RAX] };
                self.apply_alu(4, a, imm, size);
                self.rip = pc as u64;
                StepResult::Ok
            }
            // immediate-group ALU: 0x81 r/m, imm32 ; 0x83 r/m, imm8 (sign-ext).
            // ModRM.reg is the op selector (add/or/adc/sbb/and/sub/xor/cmp).
            0x81 | 0x83 => {
                pc += 1;
                let il = if b == 0x83 { 1 } else { 4 };
                let (sel, rm, npc) = self.decode_modrm_imm(mem, pc, rex_r, rex_x, rex_b, il);
                pc = npc;
                let imm = if b == 0x83 {
                    let v = fetch(pc) as i8 as i64 as u64;
                    pc += 1;
                    v
                } else {
                    let v = imm32(pc) as u64; // sign-extended to 64
                    pc += 4;
                    v
                };
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
            // mov reg, imm  (0xB8+r): imm64 with REX.W else imm32 zero-extended.
            0xb8..=0xbf => {
                let reg = (b - 0xb8) as usize + if rex_b { 8 } else { 0 };
                pc += 1;
                let imm = if rex_w {
                    let mut v = 0u64;
                    for i in 0..8 {
                        v |= (fetch(pc + i) as u64) << (i * 8);
                    }
                    pc += 8;
                    v
                } else {
                    let v = imm32(pc) as u32 as u64;
                    pc += 4;
                    v
                };
                self.regs[reg] = imm;
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
                        if !Self::store(mem, a, imm, 1) {
                            return StepResult::Fault { addr: a };
                        }
                    }
                }
                self.rip = pc as u64;
                StepResult::Ok
            }
            // mov r/m, imm32  (0xC7 /0)
            0xc7 => {
                pc += 1;
                let (_, rm, npc) = self.decode_modrm_imm(mem, pc, rex_r, rex_x, rex_b, 4);
                pc = npc;
                let imm = imm32(pc) as u64 & if size == 4 { 0xFFFF_FFFF } else { u64::MAX };
                pc += 4;
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
                        // mul/imul: AX = AL * r/m8
                        let al = self.regs[RAX] & 0xff;
                        let prod = if sub & 7 == 4 {
                            (al * a) & 0xffff
                        } else {
                            (((al as i8 as i64) * (a as i8 as i64)) as u64) & 0xffff
                        };
                        self.regs[RAX] = (self.regs[RAX] & !0xffff) | prod;
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
                        // test r/m, imm32 (sign-extended)
                        let imm = imm32(pc) as u64;
                        pc += 4;
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
                        // mul/imul: rdx:rax = rax * r/m
                        let prod: u128 = if sub & 7 == 4 {
                            (self.regs[RAX] as u128 & mask128(size)) * (a as u128 & mask128(size))
                        } else {
                            let x = sign_ext(self.regs[RAX], size) as i128;
                            let y = sign_ext(a, size) as i128;
                            (x * y) as u128
                        };
                        if size == 4 {
                            self.regs[RAX] = (prod as u64) & 0xFFFF_FFFF;
                            self.regs[RDX] = (prod >> 32) as u64 & 0xFFFF_FFFF;
                        } else {
                            self.regs[RAX] = prod as u64;
                            self.regs[RDX] = (prod >> 64) as u64;
                        }
                    }
                    6 | 7 => {
                        // div/idiv: (rdx:rax) / r/m
                        if a == 0 {
                            return StepResult::Fault { addr: 0 }; // #DE
                        }
                        if sub & 7 == 6 {
                            let num: u128 = if size == 4 {
                                ((self.regs[RDX] & 0xFFFF_FFFF) << 32 | (self.regs[RAX] & 0xFFFF_FFFF))
                                    as u128
                            } else {
                                ((self.regs[RDX] as u128) << 64) | self.regs[RAX] as u128
                            };
                            let d = (a & if size == 4 { 0xFFFF_FFFF } else { u64::MAX }) as u128;
                            let q = num / d;
                            let r = num % d;
                            self.regs[RAX] = q as u64 & if size == 4 { 0xFFFF_FFFF } else { u64::MAX };
                            self.regs[RDX] = r as u64 & if size == 4 { 0xFFFF_FFFF } else { u64::MAX };
                        } else {
                            let num: i128 = if size == 4 {
                                (((self.regs[RDX] & 0xFFFF_FFFF) << 32
                                    | (self.regs[RAX] & 0xFFFF_FFFF)) as i64)
                                    as i128
                            } else {
                                (((self.regs[RDX] as i128) << 64) | self.regs[RAX] as i128) as i128
                            };
                            let d = sign_ext(a, size) as i128;
                            self.regs[RAX] = (num / d) as u64 & if size == 4 { 0xFFFF_FFFF } else { u64::MAX };
                            self.regs[RDX] = (num % d) as u64 & if size == 4 { 0xFFFF_FFFF } else { u64::MAX };
                        }
                    }
                    _ => return StepResult::Unknown { rip: start, byte: 0xf7 },
                }
                self.rip = pc as u64;
                StepResult::Ok
            }
            // shift/rotate group: 0xC1 r/m,imm8 ; 0xD1 r/m,1 ; 0xD3 r/m,CL.
            // ModRM.reg selects: /0 rol /1 ror /4 shl /5 shr /7 sar.
            0xc1 | 0xd1 | 0xd3 => {
                pc += 1;
                let il = if b == 0xc1 { 1 } else { 0 };
                let (sel, rm, npc) = self.decode_modrm_imm(mem, pc, rex_r, rex_x, rex_b, il);
                pc = npc;
                let raw_count = match b {
                    0xc1 => {
                        let c = fetch(pc);
                        pc += 1;
                        c as u32
                    }
                    0xd1 => 1,
                    _ => (self.regs[RCX] & 0xff) as u32, // 0xD3: count = CL
                };
                let bits = size as u32 * 8;
                let count = raw_count & (if size == 8 { 63 } else { 31 });
                let a = match self.read_rm(mem, rm, size) {
                    Some(v) => v,
                    None => return self.fault(rm),
                };
                let mask = if bits == 64 { u64::MAX } else { (1u64 << bits) - 1 };
                let av = a & mask;
                let res = if count == 0 {
                    av
                } else {
                    match sel & 7 {
                        4 => {
                            // shl
                            self.set_flag(CF, (av >> (bits - count)) & 1 == 1);
                            (av << count) & mask
                        }
                        5 => {
                            // shr (logical)
                            self.set_flag(CF, (av >> (count - 1)) & 1 == 1);
                            av >> count
                        }
                        7 => {
                            // sar (arithmetic)
                            self.set_flag(CF, (av >> (count - 1)) & 1 == 1);
                            let sign = (av >> (bits - 1)) & 1 == 1;
                            let mut r = av >> count;
                            if sign {
                                r |= mask & !(mask >> count);
                            }
                            r
                        }
                        0 => ((av << count) | (av >> (bits - count))) & mask, // rol
                        1 => ((av >> count) | (av << (bits - count))) & mask, // ror
                        _ => return StepResult::Unknown { rip: start, byte: b },
                    }
                };
                if matches!(sel & 7, 4 | 5 | 7) {
                    self.set_zsp(res, size);
                }
                if !self.write_rm(mem, rm, res, size) {
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
                        // syscall — advance past the 2-byte opcode; the caller
                        // services it from the register state and resumes.
                        self.rip = (pc + 2) as u64;
                        StepResult::Syscall
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
                            self.regs[reg] = if size == 4 { v & 0xFFFF_FFFF } else { v };
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
                                if !Self::store(mem, a, val, 1) {
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
                        self.xmm[reg] = v;
                        self.rip = pc as u64;
                        StepResult::Ok
                    }
                    0x11 | 0x29 | 0x7f => {
                        pc += 2;
                        let (reg, rm, npc) = self.decode_modrm(mem, pc, rex_r, rex_x, rex_b);
                        pc = npc;
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
                    _ => StepResult::Unknown { rip: start, byte: 0x0f },
                }
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
