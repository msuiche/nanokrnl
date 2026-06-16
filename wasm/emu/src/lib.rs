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

#[derive(Clone)]
pub struct Cpu {
    pub regs: [u64; 16],
    pub rip: u64,
    pub rflags: u64,
}

#[derive(Debug, PartialEq, Eq)]
pub enum StepResult {
    Ok,
    Halt,
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
        Cpu { regs: [0; 16], rip: 0, rflags: 0 }
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
    fn read_rm(&self, mem: &[u8], rm: Rm, size: u8) -> Option<u64> {
        match rm {
            Rm::Reg(r) => Some(if size == 4 { self.regs[r] & 0xFFFF_FFFF } else { self.regs[r] }),
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

    /// Decode a ModRM byte at `pc`. Returns (reg field index, r/m operand, pc
    /// after any SIB/displacement bytes).
    fn decode_modrm(
        &self,
        mem: &[u8],
        mut pc: usize,
        rex_r: bool,
        rex_x: bool,
        rex_b: bool,
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
            // RIP-relative (approximate: relative to pc after the disp32).
            let d = rd32(pc);
            pc += 4;
            addr = (pc as u64).wrapping_add(d as u64);
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
        (reg, Rm::Mem(addr), pc)
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
        let size: u8 = if rex_w { 8 } else { 4 };

        match b {
            // ALU/MOV with ModRM: 0x01 add, 0x29 sub, 0x31 xor, 0x39 cmp,
            // 0x89 mov  — all "r/m, reg". 0x03/0x2B/0x8B/0x3B are "reg, r/m".
            0x01 | 0x29 | 0x31 | 0x39 | 0x89 | 0x03 | 0x2b | 0x3b | 0x8b => {
                let op = b;
                pc += 1;
                let (reg, rm, npc) = self.decode_modrm(mem, pc, rex_r, rex_x, rex_b);
                pc = npc;
                let to_reg = matches!(op, 0x03 | 0x2b | 0x3b | 0x8b);
                let rmv = match self.read_rm(mem, rm, size) {
                    Some(v) => v,
                    None => return self.fault(rm),
                };
                let regv = if size == 4 { self.regs[reg] & 0xFFFF_FFFF } else { self.regs[reg] };
                let (a, src) = if to_reg { (regv, rmv) } else { (rmv, regv) };
                let res = match op {
                    0x89 | 0x8b => src,                    // mov
                    0x01 | 0x03 => a.wrapping_add(src),    // add
                    0x29 | 0x2b => a.wrapping_sub(src),    // sub
                    0x31 => a ^ src,                       // xor
                    0x39 | 0x3b => a.wrapping_sub(src),    // cmp (result discarded)
                    _ => unreachable!(),
                };
                match op {
                    0x01 | 0x03 => {
                        self.set_flag(CF, a.checked_add(src).is_none() || a.wrapping_add(src) < a);
                        self.set_flag(OF, ((a ^ res) & (src ^ res)) >> 63 & 1 == 1);
                        self.set_zsp(res, size);
                    }
                    0x29 | 0x2b | 0x39 | 0x3b => {
                        self.set_flag(CF, a < src);
                        self.set_flag(OF, ((a ^ src) & (a ^ res)) >> 63 & 1 == 1);
                        self.set_zsp(res, size);
                    }
                    0x31 => {
                        self.set_flag(CF, false);
                        self.set_flag(OF, false);
                        self.set_zsp(res, size);
                    }
                    _ => {}
                }
                let ok = if op == 0x39 || op == 0x3b {
                    true // cmp: flags only
                } else if to_reg {
                    self.write_rm(mem, Rm::Reg(reg), res, size)
                } else {
                    self.write_rm(mem, rm, res, size)
                };
                if !ok {
                    return self.fault(rm);
                }
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
            // mov r/m, imm32  (0xC7 /0)
            0xc7 => {
                pc += 1;
                let (_, rm, npc) = self.decode_modrm(mem, pc, rex_r, rex_x, rex_b);
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
            other => StepResult::Unknown { rip: start, byte: other },
        }
    }

    fn fault(&self, rm: Rm) -> StepResult {
        match rm {
            Rm::Mem(a) => StepResult::Fault { addr: a },
            Rm::Reg(_) => StepResult::Fault { addr: 0 },
        }
    }

    /// Evaluate a condition code (low nibble of a 0x7x jcc opcode).
    fn cond(&self, cc: u8) -> bool {
        match cc {
            0x4 => self.flag(ZF),                          // je/jz
            0x5 => !self.flag(ZF),                         // jne/jnz
            0xc => self.flag(SF) != self.flag(OF),         // jl
            0xd => self.flag(SF) == self.flag(OF),         // jge
            0xe => self.flag(ZF) || (self.flag(SF) != self.flag(OF)), // jle
            0xf => !self.flag(ZF) && (self.flag(SF) == self.flag(OF)), // jg
            0x2 => self.flag(CF),                          // jb
            0x3 => !self.flag(CF),                         // jae
            _ => false,
        }
    }

    /// Set up an initial frame (entry + stack with a HALT_ADDR return), then run
    /// until Halt/Unknown/Fault, capped at `max_steps`.
    pub fn run_program(&mut self, mem: &mut [u8], entry: u64, stack_top: u64, max_steps: usize) -> StepResult {
        self.rip = entry;
        self.regs[RSP] = stack_top;
        self.push64(mem, HALT_ADDR);
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
    fn unknown_opcode_reported() {
        let mut mem = vec![0x0f, 0x0b]; // ud2
        let mut cpu = Cpu::new();
        assert_eq!(cpu.step(&mut mem), StepResult::Unknown { rip: 0, byte: 0x0f });
    }
}
