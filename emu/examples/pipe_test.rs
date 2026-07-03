//! Exercise cmd.exe pipes and redirection end to end: boot the interactive
//! kernel, reach the prompt, and run `dir`, `dir | sort`, `echo`-redirection,
//! printing what comes back so we can see whether `|` and `>` actually work
//! (not just render). Pairs with `cmd_session` but drives the pipe path.
//!
//!   cargo run --release --example pipe_test -- ../target/x86_64-unknown-none/debug/kernel

use nanox::machine::{Machine, RunStop};

fn main() {
    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "../target/x86_64-unknown-none/debug/kernel".to_string());
    let image = std::fs::read(&path).expect("read kernel ELF");
    let mut m = Machine::new(128 * 1024 * 1024);
    m.boot_kernel(&image).expect("boot");

    let mut out = String::new();
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

    for _ in 0..120 {
        pump(&mut m, &mut out, 1);
        if out.contains("C:\\>") {
            break;
        }
    }
    let mark = |out: &mut String, label: &str| out.push_str(&format!("\n===== {label} =====\n"));

    let run_cmd = |m: &mut Machine, out: &mut String, cmd: &str| {
        mark(out, cmd);
        for &b in cmd.as_bytes() {
            m.cpu.dev.uart.push_rx(b);
        }
        m.cpu.dev.uart.push_rx(b'\r');
        pump(m, out, 12);
    };

    let seq: &[&str] = if std::env::args().any(|a| a == "--plain") {
        // Regression check: only non-pipe commands. cmd must survive all of them
        // (no "CMD: wait" — that would mean cmd exited early).
        &["dir", "echo hello", "more hello.txt", "ver"]
    } else if std::env::args().any(|a| a == "--slashc") {
        // Is the `cmd /c <builtin>` child path (which `|` relies on) working?
        &["cmd /c echo hi", "cmd /c dir"]
    } else if std::env::args().any(|a| a == "--tools") {
        // Which shipped Windows console tools work as interactive commands?
        &["whoami", "where cmd.exe", "where cmd", "ver", "vol"]
    } else {
        &["dir", "dir | sort", "dir > out.txt", "more out.txt"]
    };
    let trace = std::env::args().any(|a| a == "--trace");
    for cmd in seq {
        if trace && cmd.contains('|') {
            m.cpu.sys_log.clear();
            m.cpu.trace_sys = true;
        }
        run_cmd(&mut m, &mut out, cmd);
        if trace && cmd.contains('|') {
            m.cpu.trace_sys = false;
            out.push_str("\n--- syscall trace ---\n");
            for &(svc, val) in &m.cpu.sys_log {
                if svc == 0xFFFF_FFFF {
                    out.push_str(&format!("   -> {val:#x}\n"));
                } else {
                    out.push_str(&format!("svc {svc:>3} arg1={val:#x}\n"));
                }
            }
        }
    }

    println!("{out}");
}
