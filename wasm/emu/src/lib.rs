//! `x86emu` — a minimal x86-64 interpreter (Track B, phase B0).
//!
//! The seed of running the real x86-64 PE binaries and drivers in the browser
//! without a platform emulator: this executes the *instruction stream* over a
//! flat linear-memory address space; later phases bridge `syscall` (ring 3) and
//! `ntoskrnl` export calls (ring 0) into the kernel's existing Rust services.
//! No MMU, no chipset, no device emulation — just a CPU.
//!
//! Decoding grows opcode-by-opcode, trace-driven: [`Cpu::step`] returns
//! [`StepResult::Unknown`] with the offending byte so the next opcode to
//! implement is obvious. This file implements just enough — REX-prefixed reg/reg
//! ALU and `mov r, imm` — to prove the register file, decoder spine, and flags,
//! exercised by the tests below.
#![cfg_attr(not(test), no_std)]

/// x86-64 general-purpose register indices (encoding order rax..r15).
pub const RAX: usize = 0;
pub const RCX: usize = 1;
pub const RDX: usize = 2;
pub const RBX: usize = 3;

/// RFLAGS bits we maintain.
const CF: u64 = 1 << 0;
const PF: u64 = 1 << 2;
const ZF: u64 = 1 << 6;
const SF: u64 = 1 << 7;
const OF: u64 = 1 << 11;

/// Architectural CPU state. Flat 64-bit address space (memory is supplied to
/// [`Cpu::step`] as a byte slice — the binary's RAM; no paging).
#[derive(Clone)]
pub struct Cpu {
    pub regs: [u64; 16],
    pub rip: u64,
    pub rflags: u64,
}

/// What a single [`Cpu::step`] did.
#[derive(Debug, PartialEq, Eq)]
pub enum StepResult {
    /// Executed one instruction; keep going.
    Ok,
    /// Hit `ret`/`hlt` with no frame — stop (B0 has no call stack yet).
    Halt,
    /// Undecoded opcode `byte` at this rip — the next thing to implement.
    Unknown { rip: u64, byte: u8 },
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

    /// Set ZF/SF/PF from a result of `size` bytes (1/2/4/8).
    fn set_zsp(&mut self, val: u64, size: u8) {
        let bits = size as u32 * 8;
        let masked = if bits == 64 { val } else { val & ((1u64 << bits) - 1) };
        self.set_flag(ZF, masked == 0);
        self.set_flag(SF, (masked >> (bits - 1)) & 1 == 1);
        self.set_flag(PF, (masked as u8).count_ones() % 2 == 0);
    }

    /// Decode and execute one instruction against `mem`. The B0 subset:
    ///   REX.W? [0x01 ADD | 0x89 MOV | 0x29 SUB] /r   (reg,reg form, mod=11)
    ///   REX.W? 0xB8+r imm32/imm64 (MOV reg, imm)
    ///   0xC3 RET  -> Halt
    pub fn step(&mut self, mem: &[u8]) -> StepResult {
        let start = self.rip;
        let mut pc = self.rip as usize;
        let fetch = |i: usize| -> u8 { *mem.get(i).unwrap_or(&0) };

        // REX prefix (0x40..=0x4F): W=8, R=4, X=2, B=1.
        let mut rex_w = false;
        let mut rex_r = false;
        let mut rex_b = false;
        let mut b = fetch(pc);
        if (0x40..=0x4f).contains(&b) {
            rex_w = b & 8 != 0;
            rex_r = b & 4 != 0;
            rex_b = b & 1 != 0;
            pc += 1;
            b = fetch(pc);
        }

        match b {
            // ADD/SUB/MOV r/m64, r64  (reg,reg when ModRM.mod == 0b11)
            0x01 | 0x29 | 0x89 => {
                let op = b;
                pc += 1;
                let modrm = fetch(pc);
                pc += 1;
                if modrm >> 6 != 0b11 {
                    return StepResult::Unknown { rip: start, byte: b }; // memory form: later
                }
                let reg = ((modrm >> 3) & 7) as usize + if rex_r { 8 } else { 0 };
                let rm = (modrm & 7) as usize + if rex_b { 8 } else { 0 };
                let size: u8 = if rex_w { 8 } else { 4 };
                let dst = self.regs[rm];
                let src = self.regs[reg];
                let res = match op {
                    0x89 => src,                       // MOV  r/m, r
                    0x01 => dst.wrapping_add(src),     // ADD  r/m, r
                    0x29 => dst.wrapping_sub(src),     // SUB  r/m, r
                    _ => unreachable!(),
                };
                if op == 0x01 {
                    let (_, carry) = dst.overflowing_add(src);
                    self.set_flag(CF, carry);
                    self.set_flag(OF, ((dst ^ res) & (src ^ res)) >> 63 & 1 == 1);
                    self.set_zsp(res, size);
                } else if op == 0x29 {
                    self.set_flag(CF, dst < src);
                    self.set_flag(OF, ((dst ^ src) & (dst ^ res)) >> 63 & 1 == 1);
                    self.set_zsp(res, size);
                }
                self.regs[rm] = if size == 4 { res & 0xFFFF_FFFF } else { res };
                self.rip = pc as u64;
                StepResult::Ok
            }
            // MOV reg, imm  (0xB8+r). imm64 with REX.W, else imm32 zero-extended.
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
                    let mut v = 0u32;
                    for i in 0..4 {
                        v |= (fetch(pc + i) as u32) << (i * 8);
                    }
                    pc += 4;
                    v as u64
                };
                self.regs[reg] = imm;
                self.rip = pc as u64;
                StepResult::Ok
            }
            0xc3 => StepResult::Halt, // RET (no call stack yet)
            other => StepResult::Unknown { rip: start, byte: other },
        }
    }

    /// Run until `Halt` or `Unknown`, capped at `max_steps` (runaway guard).
    pub fn run(&mut self, mem: &[u8], max_steps: usize) -> StepResult {
        for _ in 0..max_steps {
            match self.step(mem) {
                StepResult::Ok => continue,
                done => return done,
            }
        }
        StepResult::Unknown { rip: self.rip, byte: 0 }
    }

    pub fn zf(&self) -> bool {
        self.flag(ZF)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mov_add_sub() {
        // mov rax, 5 ; mov rcx, 7 ; add rax, rcx ; sub rax, rcx ; ret
        let code = [
            0x48, 0xb8, 5, 0, 0, 0, 0, 0, 0, 0, // mov rax, 5
            0x48, 0xb9, 7, 0, 0, 0, 0, 0, 0, 0, // mov rcx, 7
            0x48, 0x01, 0xc8, // add rax, rcx   (rax=12)
            0x48, 0x29, 0xc8, // sub rax, rcx   (rax=5)
            0xc3, // ret
        ];
        let mut cpu = Cpu::new();
        assert_eq!(cpu.run(&code, 100), StepResult::Halt);
        assert_eq!(cpu.regs[RAX], 5);
        assert_eq!(cpu.regs[RCX], 7);
    }

    #[test]
    fn add_sets_zero_flag() {
        // mov rax,0 ; mov rcx,0 ; add rax,rcx ; ret
        let code = [
            0x48, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0x48, 0xb9, 0, 0, 0, 0, 0, 0, 0, 0, 0x48, 0x01,
            0xc8, 0xc3,
        ];
        let mut cpu = Cpu::new();
        cpu.run(&code, 100);
        assert!(cpu.zf(), "ZF should be set when add yields 0");
    }

    #[test]
    fn unknown_opcode_is_reported() {
        let code = [0x0f, 0x0b]; // ud2 — not implemented
        let mut cpu = Cpu::new();
        assert_eq!(cpu.step(&code), StepResult::Unknown { rip: 0, byte: 0x0f });
    }
}
