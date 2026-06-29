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
    const KV: u64 = 0xFFFF_8000_0000_0000;
    // (name, link-vaddr) of the post-scheduler dispatch path.
    let watch = [
        ("ki_dispatch_trap", 0x209a80u64),
        ("switch_away_locked", 0x1e32d0),
        ("ki_swap_context", 0x1ee338),
        ("ki_finish_switch_to_new_thread", 0x1e50f0),
        ("smoke_test_thread", 0x1d4840),
    ];
    m.watch = watch.iter().map(|(_, a)| KV + a).collect();
    let steps: usize = std::env::var("STEPS").ok().and_then(|s| s.parse().ok()).unwrap_or(2_000_000_000);
    let stop = m.run(steps);
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
    println!("--- dispatch-path watchpoints ---");
    for (name, a) in &watch {
        let hit = m.watch_hits.contains(&(KV + a));
        println!("  [{}] {}", if hit { "HIT " } else { "miss" }, name);
    }
    if let RunStop::Unknown { rip, .. } = stop {
        // Translate the faulting rip through the current page tables (works for
        // kernel, phys-window, and user addresses) and dump the bytes there.
        print!("bytes at faulting rip:");
        for i in 0..12u64 {
            match ntemu::mmu::translate(&m.ram, &m.cpu.paging, rip + i, ntemu::mmu::Access::Execute, false) {
                Ok(p) if (p as usize) < m.ram.len() => print!(" {:02x}", m.ram[p as usize]),
                _ => print!(" ??"),
            }
        }
        println!();
    }
    if let RunStop::Unknown { rip, byte } = stop {
        // Show the bytes around the faulting RIP for context.
        println!("next opcode to implement: {:#04x} at rip {:#x}", byte, rip);
    }
}
