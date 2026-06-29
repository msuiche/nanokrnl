//! Differential decoder conformance: disassemble the real kernel's `.text` with
//! iced-x86 (an authoritative x86 decoder) and check that nanox decodes every
//! instruction to the **same length**. A length mismatch is a decode/operand-size
//! bug that would desync nanox mid-stream and corrupt the boot — exactly the
//! class of bug we keep hitting. This finds them automatically, with no
//! single-stepping of the kernel.
//!
//!   cargo run --release --example conformance
//!
//! Reports: length mismatches (BUGS), and opcodes nanox doesn't implement yet
//! (GAPS). It only checks instruction *length* here (the desync class); it does
//! not execute semantics.

use iced_x86::{Decoder, DecoderOptions, FlowControl, Instruction};
use nanox::{Cpu, StepResult};

/// Decode-length of the instruction at the start of `bytes` per nanox, by
/// executing one step over a flat buffer with registers pointing at valid
/// memory. Returns Ok(len) only for sequential instructions (so rip-delta is
/// the length); Err for unknown opcodes or non-sequential/﹣faulting steps.
fn nanox_len(bytes: &[u8]) -> Result<usize, &'static str> {
    const BASE: u64 = 0x80_0000; // 8 MiB into a 16 MiB buffer
    let mut mem = vec![0u8; 16 << 20];
    let n = bytes.len().min(15);
    mem[BASE as usize..BASE as usize + n].copy_from_slice(&bytes[..n]);
    let mut cpu = Cpu::new();
    // Every GPR points at valid in-bounds memory so operands don't fault.
    for r in cpu.regs.iter_mut() {
        *r = BASE;
    }
    cpu.rip = BASE;
    match cpu.step(&mut mem) {
        StepResult::Unknown { .. } => Err("unimplemented"),
        StepResult::Ok if cpu.rip >= BASE && cpu.rip < BASE + 16 => Ok((cpu.rip - BASE) as usize),
        _ => Err("non-sequential"),
    }
}

fn main() {
    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "../target/x86_64-unknown-none/debug/kernel".to_string());
    let image = std::fs::read(&path).expect("read kernel ELF");

    // Find the executable PT_LOAD segment (R-X) of the ELF64.
    let phoff = u64::from_le_bytes(image[32..40].try_into().unwrap()) as usize;
    let phentsize = u16::from_le_bytes(image[54..56].try_into().unwrap()) as usize;
    let phnum = u16::from_le_bytes(image[56..58].try_into().unwrap()) as usize;
    let (mut text_off, mut text_va, mut text_size) = (0usize, 0u64, 0usize);
    for i in 0..phnum {
        let ph = phoff + i * phentsize;
        let typ = u32::from_le_bytes(image[ph..ph + 4].try_into().unwrap());
        let flags = u32::from_le_bytes(image[ph + 4..ph + 8].try_into().unwrap());
        if typ == 1 && flags & 1 != 0 {
            // PT_LOAD + executable
            text_off = u64::from_le_bytes(image[ph + 8..ph + 16].try_into().unwrap()) as usize;
            text_va = u64::from_le_bytes(image[ph + 16..ph + 24].try_into().unwrap());
            text_size = u64::from_le_bytes(image[ph + 32..ph + 40].try_into().unwrap()) as usize;
        }
    }
    let code = &image[text_off..text_off + text_size];
    println!(".text: va={:#x} size={} bytes", text_va, text_size);

    let mut decoder = Decoder::with_ip(64, code, text_va, DecoderOptions::NONE);
    let mut instr = Instruction::default();
    let (mut total, mut checked, mut bugs, mut gaps) = (0u64, 0u64, 0u64, 0u64);
    let mut bug_samples: Vec<(u64, usize, usize, String, String)> = Vec::new();
    let mut gap_opcodes: std::collections::BTreeMap<String, u64> = std::collections::BTreeMap::new();

    while decoder.can_decode() {
        let ip = decoder.ip();
        decoder.decode_out(&mut instr);
        if instr.is_invalid() {
            continue;
        }
        total += 1;
        // Only length-check sequential instructions (rip-delta == length).
        if instr.flow_control() != FlowControl::Next {
            continue;
        }
        let off = (ip - text_va) as usize;
        let bytes = &code[off..(off + instr.len()).min(code.len())];
        match nanox_len(bytes) {
            Ok(len) => {
                checked += 1;
                if len != instr.len() {
                    bugs += 1;
                    if bug_samples.len() < 25 {
                        let hex = bytes.iter().map(|b| format!("{:02x}", b)).collect::<Vec<_>>().join(" ");
                        bug_samples.push((ip, instr.len(), len, format!("{:?}", instr.mnemonic()), hex));
                    }
                }
            }
            Err("unimplemented") => {
                gaps += 1;
                *gap_opcodes.entry(format!("{:?}", instr.mnemonic())).or_insert(0) += 1;
            }
            Err(_) => {}
        }
    }

    println!("\n{} instructions, {} length-checked", total, checked);
    println!("LENGTH MISMATCHES (decode bugs): {}", bugs);
    for (ip, want, got, mn, hex) in &bug_samples {
        println!("  {:#x} {:<10} iced_len={} nanox_len={}  [{}]", ip, mn, want, got, hex);
    }
    println!("\nUNIMPLEMENTED (gaps): {} sites across {} mnemonics", gaps, gap_opcodes.len());
    for (mn, count) in gap_opcodes.iter() {
        println!("  {:<14} x{}", mn, count);
    }
}
