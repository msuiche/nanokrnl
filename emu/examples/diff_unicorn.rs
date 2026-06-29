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

/// Extract the executable code bytes + their base VA from an ELF (R-X PT_LOAD)
/// or a PE/PE32+ (the executable section). The diff places each instruction at a
/// fixed address in both engines, so the VA only affects RIP-relative reporting.
fn extract_code(image: &[u8]) -> (Vec<u8>, u64) {
    let u16le = |o: usize| u16::from_le_bytes(image[o..o + 2].try_into().unwrap());
    let u32le = |o: usize| u32::from_le_bytes(image[o..o + 4].try_into().unwrap());
    let u64le = |o: usize| u64::from_le_bytes(image[o..o + 8].try_into().unwrap());
    if &image[0..2] == b"MZ" {
        // PE: DOS -> e_lfanew@0x3C -> COFF -> optional header -> sections.
        let pe = u32le(0x3C) as usize;
        let nsec = u16le(pe + 6) as usize;
        let opt = u16le(pe + 20) as usize; // SizeOfOptionalHeader
        let image_base = u64le(pe + 24 + 24); // PE32+ ImageBase
        let sec0 = pe + 24 + opt;
        for i in 0..nsec {
            let s = sec0 + i * 40;
            let chars = u32le(s + 36);
            if chars & 0x2000_0000 != 0 {
                // IMAGE_SCN_MEM_EXECUTE
                let va = image_base + u32le(s + 12) as u64;
                let off = u32le(s + 20) as usize;
                let size = u32le(s + 16) as usize;
                return (image[off..off + size].to_vec(), va);
            }
        }
        return (Vec::new(), 0);
    }
    // ELF64: R-X PT_LOAD.
    let phoff = u64le(32) as usize;
    let phentsize = u16le(54) as usize;
    let phnum = u16le(56) as usize;
    for i in 0..phnum {
        let ph = phoff + i * phentsize;
        if u32le(ph) == 1 && u32le(ph + 4) & 1 != 0 {
            let off = u64le(ph + 8) as usize;
            let va = u64le(ph + 16);
            let size = u64le(ph + 32) as usize;
            return (image[off..off + size].to_vec(), va);
        }
    }
    (Vec::new(), 0)
}

fn main() {
    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "../target/x86_64-unknown-none/debug/kernel".to_string());
    let image = std::fs::read(&path).expect("read image");
    let (code_vec, text_va) = extract_code(&image);
    let code = &code_vec[..];
    eprintln!("{}: code {} bytes @ {:#x}", path, code.len(), text_va);

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
        // REP string ops can't be compared 1:1: Unicorn's count=1 runs a single
        // iteration while ntemu runs the whole rep in one step. Same final state,
        // different per-step — skip to avoid false positives.
        if matches!(mn.as_str(), "Movsb" | "Movsw" | "Movsd" | "Movsq" | "Stosb" | "Stosw"
            | "Stosd" | "Stosq" | "Lodsb" | "Lodsw" | "Lodsd" | "Lodsq"
            | "Scasb" | "Scasw" | "Scasd" | "Scasq" | "Cmpsb" | "Cmpsw" | "Cmpsd" | "Cmpsq") {
            continue;
        }
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
