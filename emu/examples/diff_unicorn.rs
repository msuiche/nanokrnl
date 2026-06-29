//! Differential **semantics** test: run each of the kernel's real instructions
//! through both Unicorn (QEMU's CPU core — the oracle) and ntemu from identical
//! register/flag/memory state, then diff the resulting GP registers and RFLAGS.
//! Catches wrong-result bugs (flags, shifts, arithmetic) that the length-only
//! conformance test can't see.
//!
//!   cargo run --release --example diff_unicorn
//!
//! AF (auxiliary carry) is excluded — ntemu does not model it.

use iced_x86::{Decoder, DecoderOptions, FlowControl, Instruction};
use ntemu::{Cpu, StepResult};
use unicorn_engine::unicorn_const::{Arch, Mode, Prot};
use unicorn_engine::{RegisterX86, Unicorn};

const MAP_BASE: u64 = 0;
const MAP_SIZE: u64 = 0x20_0000; // 2 MiB scratch, identity
const IP: u64 = 0x1000; // where each instruction is placed
// Compared flags: CF(0) PF(2) ZF(6) SF(7) OF(11) DF(10). AF(4) excluded.
const FLAG_MASK: u64 = (1 << 0) | (1 << 2) | (1 << 6) | (1 << 7) | (1 << 11) | (1 << 10);

const UREGS: [RegisterX86; 16] = [
    RegisterX86::RAX, RegisterX86::RCX, RegisterX86::RDX, RegisterX86::RBX,
    RegisterX86::RSP, RegisterX86::RBP, RegisterX86::RSI, RegisterX86::RDI,
    RegisterX86::R8, RegisterX86::R9, RegisterX86::R10, RegisterX86::R11,
    RegisterX86::R12, RegisterX86::R13, RegisterX86::R14, RegisterX86::R15,
];

/// Deterministic per-seed initial register file. Values are valid pointers into
/// the mapped scratch (so memory operands don't fault) but vary enough to
/// exercise flags.
fn init_regs(seed: u64) -> [u64; 16] {
    let mut r = [0u64; 16];
    let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15).wrapping_add(1);
    for v in r.iter_mut() {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        // Keep it in [0x40000, 0x1C0000) so [reg±disp] stays mapped.
        *v = 0x40000 + (x % 0x180000) & !0xF;
    }
    r[4] = 0x100000; // RSP mid-region, 16-aligned
    r
}

fn run_unicorn(code: &[u8], regs: &[u64; 16], rflags: u64) -> Option<([u64; 16], u64)> {
    let mut uc = Unicorn::new(Arch::X86, Mode::MODE_64).ok()?;
    uc.mem_map(MAP_BASE, MAP_SIZE, Prot::ALL).ok()?;
    uc.mem_write(IP, code).ok()?;
    for (i, &r) in regs.iter().enumerate() {
        uc.reg_write(UREGS[i], r).ok()?;
    }
    uc.reg_write(RegisterX86::RFLAGS, rflags | 0x2).ok()?;
    uc.reg_write(RegisterX86::RIP, IP).ok()?;
    uc.emu_start(IP, IP + code.len() as u64, 0, 1).ok()?;
    let mut out = [0u64; 16];
    for (i, slot) in out.iter_mut().enumerate() {
        *slot = uc.reg_read(UREGS[i]).ok()?;
    }
    let fl = uc.reg_read(RegisterX86::RFLAGS).ok()?;
    Some((out, fl))
}

fn run_ntemu(code: &[u8], regs: &[u64; 16], rflags: u64) -> Option<([u64; 16], u64)> {
    let mut mem = vec![0u8; (MAP_SIZE) as usize];
    mem[IP as usize..IP as usize + code.len()].copy_from_slice(code);
    let mut cpu = Cpu::new();
    cpu.regs = *regs;
    cpu.rflags = rflags | 0x2;
    cpu.rip = IP;
    match cpu.step(&mut mem) {
        StepResult::Ok => Some((cpu.regs, cpu.rflags)),
        _ => None,
    }
}

fn main() {
    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "../target/x86_64-unknown-none/debug/kernel".to_string());
    let image = std::fs::read(&path).expect("read kernel ELF");
    let phoff = u64::from_le_bytes(image[32..40].try_into().unwrap()) as usize;
    let phentsize = u16::from_le_bytes(image[54..56].try_into().unwrap()) as usize;
    let phnum = u16::from_le_bytes(image[56..58].try_into().unwrap()) as usize;
    let (mut text_off, mut text_va, mut text_size) = (0usize, 0u64, 0usize);
    for i in 0..phnum {
        let ph = phoff + i * phentsize;
        let typ = u32::from_le_bytes(image[ph..ph + 4].try_into().unwrap());
        let flags = u32::from_le_bytes(image[ph + 4..ph + 8].try_into().unwrap());
        if typ == 1 && flags & 1 != 0 {
            text_off = u64::from_le_bytes(image[ph + 8..ph + 16].try_into().unwrap()) as usize;
            text_va = u64::from_le_bytes(image[ph + 16..ph + 24].try_into().unwrap());
            text_size = u64::from_le_bytes(image[ph + 32..ph + 40].try_into().unwrap()) as usize;
        }
    }
    let code = &image[text_off..text_off + text_size];

    let seeds = [1u64, 2, 3];
    let rflag_seeds = [0u64, 1 /*CF*/];
    let mut decoder = Decoder::with_ip(64, code, text_va, DecoderOptions::NONE);
    let mut instr = Instruction::default();
    let mut diverged: std::collections::BTreeMap<String, (u64, String)> = std::collections::BTreeMap::new();
    let mut checked = 0u64;

    while decoder.can_decode() {
        let ip = decoder.ip();
        decoder.decode_out(&mut instr);
        if instr.is_invalid() || instr.flow_control() != FlowControl::Next {
            continue;
        }
        let off = (ip - text_va) as usize;
        if off + instr.len() > code.len() {
            break;
        }
        let bytes = &code[off..off + instr.len()];
        let mn = format!("{:?}", instr.mnemonic());
        for &s in &seeds {
            for &fs in &rflag_seeds {
                let regs = init_regs(s);
                let (Some((ur, uf)), Some((nr, nf))) =
                    (run_unicorn(bytes, &regs, fs), run_ntemu(bytes, &regs, fs))
                else {
                    continue;
                };
                checked += 1;
                let regs_ok = ur == nr;
                let flags_ok = (uf & FLAG_MASK) == (nf & FLAG_MASK);
                if !regs_ok || !flags_ok {
                    let hex = bytes.iter().map(|b| format!("{:02x}", b)).collect::<Vec<_>>().join(" ");
                    let detail = if !flags_ok {
                        format!("FLAGS uc={:#06x} nt={:#06x} (masked {:#06x} vs {:#06x}) [{}]",
                            uf & FLAG_MASK, nf & FLAG_MASK, uf & FLAG_MASK, nf & FLAG_MASK, hex)
                    } else {
                        let i = (0..16).find(|&i| ur[i] != nr[i]).unwrap();
                        format!("REG {:?} uc={:#x} nt={:#x} [{}]", UREGS[i], ur[i], nr[i], hex)
                    };
                    diverged.entry(mn.clone()).or_insert_with(|| (0, detail)).0 += 1;
                    break;
                }
            }
        }
    }

    println!("checked {} (instruction, seed) pairs", checked);
    println!("DIVERGENCES by mnemonic: {}", diverged.len());
    for (mn, (count, sample)) in &diverged {
        println!("  {:<12} x{:<6} e.g. {}", mn, count, sample);
    }
}
