//! Inspect (and attempt to boot) the real ntoskrnl-rs ELF under ntemu.
//!
//!   cargo run --example inspect_kernel -- ../target/x86_64-unknown-none/debug/kernel
//!
//! Prints the ELF entry + load segments, then tries a direct long-mode boot and
//! reports where it stops. This is the trace-driven signal for what's still
//! missing to run the real kernel (an opcode, or the bootloader handoff).

use ntemu::elf::Elf;
use ntemu::machine::{Machine, RunStop};

fn main() {
    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "../target/x86_64-unknown-none/debug/kernel".to_string());
    let image = std::fs::read(&path).expect("read kernel ELF");
    println!("loaded {} ({} bytes)", path, image.len());

    let elf = Elf::parse(&image).expect("parse ELF");
    println!("entry = {:#x}", elf.entry);
    let mut max_end = 0u64;
    for (i, s) in elf.segments.iter().enumerate() {
        println!(
            "  seg[{i}] vaddr={:#x} paddr={:#x} filesz={:#x} memsz={:#x} flags={}{}{}",
            s.vaddr,
            s.paddr,
            s.file_size,
            s.mem_size,
            if s.flags & 4 != 0 { "R" } else { "-" },
            if s.flags & 2 != 0 { "W" } else { "-" },
            if s.flags & 1 != 0 { "X" } else { "-" },
        );
        max_end = max_end.max(s.vaddr + s.mem_size as u64).max(s.paddr + s.mem_size as u64);
    }
    println!("highest load end = {:#x}", max_end);

    // Attempt a direct boot. Size RAM to the image if plausible; otherwise this
    // documents the gap (a high-half kernel needs the bootloader handoff).
    if max_end > 0x4000_0000 {
        println!(
            "\nNOTE: load addresses exceed 1 GiB ({:#x}); a direct identity boot \
             can't place them. This is the bootloader-handoff gap (SPEC.md).",
            max_end
        );
        return;
    }
    let ram = (max_end as usize + 0x10_0000).next_power_of_two().max(0x100_0000);
    let mut m = Machine::new(ram);
    let entry = m.load_elf(&image).unwrap();
    m.boot_long_mode(entry, (ram as u64 / 2) & !0xFFF);
    let stop = m.run(2_000_000);
    let out = m.take_uart_output();
    println!("\nran: stop = {:?}", stop);
    if !out.is_empty() {
        println!("UART output: {:?}", String::from_utf8_lossy(&out));
    }
    if let RunStop::Unknown { rip, byte } = stop {
        println!("=> next opcode to implement: {:#04x} at rip {:#x}", byte, rip);
    }
}
