//! Boot the real ntoskrnl-rs ELF under ntemu via the bootloader_api handoff.
//!
//!   cargo run --example inspect_kernel -- ../target/x86_64-unknown-none/debug/kernel
//!
//! Loads + relocates the kernel high-half, builds the page tables + BootInfo,
//! enters `_start`, and runs — reporting UART output and where it stops (the
//! next opcode to implement, if any).

use ntemu::elf::Elf;
use ntemu::machine::{Machine, RunStop};

fn main() {
    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "../target/x86_64-unknown-none/debug/kernel".to_string());
    let image = std::fs::read(&path).expect("read kernel ELF");
    let elf = Elf::parse(&image).expect("parse ELF");
    println!("loaded {} ({} bytes), entry={:#x}", path, image.len(), elf.entry);

    // DUMP=0x1f84b2 prints the raw bytes at a kernel link-vaddr.
    if let Ok(d) = std::env::var("DUMP") {
        let va = u64::from_str_radix(d.trim_start_matches("0x"), 16).unwrap();
        for s in elf.segments.iter() {
            if va >= s.vaddr && va < s.vaddr + s.file_size as u64 {
                let off = s.file_off + (va - s.vaddr) as usize;
                let bytes = &image[off..off + 24];
                print!("bytes @ {:#x}:", va);
                for b in bytes {
                    print!(" {:02x}", b);
                }
                println!();
            }
        }
        return;
    }

    let mut m = Machine::new(128 * 1024 * 1024);
    m.boot_kernel(&image).expect("boot_kernel");
    println!(
        "booted: rip={:#x} rsp={:#x} rdi={:#x}",
        m.cpu.rip, m.cpu.regs[ntemu::RSP], m.cpu.regs[ntemu::RDI]
    );

    m.trace_on = std::env::var("TRACE").is_ok();
    let stop = m.run(50_000_000);
    if m.trace_on {
        println!("--- last {} rips ---", m.trace_log.len());
        for r in &m.trace_log {
            println!("  {:#x}", r);
        }
    }
    let out = m.take_uart_output();
    println!("\n--- UART ({} bytes) ---", out.len());
    println!("{}", String::from_utf8_lossy(&out));
    println!("--- stop: {:?} (rip={:#x}) ---", stop, m.cpu.rip);
    let a = &m.cpu.dev.apic;
    println!(
        "APIC: lvt_timer={:#x} initial={} current={} divide={:#x}  IF={}",
        a.lvt_timer,
        a.initial_count,
        a.current_count,
        a.divide_config,
        m.cpu.rflags & (1 << 9) != 0
    );
    println!("timer IRQs delivered: {}   hlts: {}", m.irqs_delivered, m.hlts);
    if let RunStop::Unknown { rip, byte } = stop {
        // Show the bytes around the faulting RIP for context.
        println!("next opcode to implement: {:#04x} at rip {:#x}", byte, rip);
    }
}
