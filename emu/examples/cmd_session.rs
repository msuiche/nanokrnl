//! Drive the interactive cmd.exe shell: boot the `--features interactive`
//! kernel, reach the prompt, type commands over the emulated UART, and print
//! what comes back. Proves the shell actually executes, not just renders.
//!
//!   cargo run --release --example cmd_session -- ../target/x86_64-unknown-none/debug/kernel

use ntemu::machine::{Machine, RunStop};

fn main() {
    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "../target/x86_64-unknown-none/debug/kernel".to_string());
    let image = std::fs::read(&path).expect("read kernel ELF");
    let mut m = Machine::new(128 * 1024 * 1024);
    m.boot_kernel(&image).expect("boot");

    let mut out = String::new();
    // Pump: run a slice, drain the UART; repeat until output goes quiet.
    let mut pump = |m: &mut Machine, out: &mut String, slices: usize| {
        for _ in 0..slices {
            let stop = m.run(20_000_000);
            for b in m.take_uart_output() {
                out.push(b as char);
            }
            if matches!(stop, RunStop::Unknown { .. } | RunStop::UnhandledFault { .. }) {
                out.push_str(&format!("\n[stop: {:?}]\n", stop));
                break;
            }
        }
    };

    // The interactive kernel runs its self-test suite first, then loads cmd.
    // Pump until the prompt appears (or give up after a large budget).
    for _ in 0..120 {
        pump(&mut m, &mut out, 1);
        if out.contains("C:\\>") {
            break;
        }
    }
    let banner_len = out.len();

    // Type a few commands. cmd reads COM1; feed each line + CR.
    for cmd in ["ver", "echo hello from ntemu", "exit"] {
        for &byte in cmd.as_bytes() {
            m.cpu.dev.uart.push_rx(byte);
        }
        m.cpu.dev.uart.push_rx(b'\r');
        pump(&mut m, &mut out, 8);
    }

    println!("===== full cmd.exe session =====");
    println!("{}", out);
    println!("===== ({} bytes total, {} after banner) =====", out.len(), out.len() - banner_len);
}
