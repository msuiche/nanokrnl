//! Attach a debugger to a nanokrnl kernel running in nanox, over the GDB Remote
//! Serial Protocol on TCP :3333.
//!
//!   cargo run --release --example gdb_server -- [kernel]
//!
//! Then, in another terminal:
//!   lldb -o "gdb-remote 3333" -o "register read" -o "continue"
//!   # or gdb:  target remote :3333
//!
//! The kernel is booted to its prompt first; on attach the target is halted and
//! the debugger drives it (read registers/memory, set breakpoints, step,
//! continue). Console output goes to stdout so you can watch the shell too.

use nanox::gdb::GdbStub;
use nanox::machine::{Machine, RunStop};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::time::Duration;

fn main() {
    let kernel = std::env::args().nth(1).unwrap_or_else(|| {
        let rel = "../target/x86_64-unknown-none/release/kernel";
        if std::path::Path::new(rel).exists() { rel.into() }
        else { "../target/x86_64-unknown-none/debug/kernel".into() }
    });
    let image = std::fs::read(&kernel).expect("read kernel");
    let mut m = Machine::new(128 * 1024 * 1024);
    m.boot_kernel(&image).expect("boot");

    // Boot to the prompt so there is a live kernel to inspect.
    let mut booted = false;
    let mut banner = String::new();
    for _ in 0..2000 {
        m.run(20_000_000);
        for b in m.take_uart_output() {
            banner.push(b as char);
            print!("{}", b as char);
        }
        if banner.contains("C:\\>") {
            booted = true;
            break;
        }
    }
    let _ = std::io::stdout().flush();
    eprintln!("\n[gdb_server] kernel {}", if booted { "reached prompt" } else { "did not reach prompt" });

    let listener = TcpListener::bind("127.0.0.1:3333").expect("bind :3333");
    eprintln!("[gdb_server] listening on 127.0.0.1:3333 (lldb: gdb-remote 3333)");
    let (mut sock, peer) = listener.accept().expect("accept");
    sock.set_nonblocking(true).unwrap();
    eprintln!("[gdb_server] debugger attached from {peer}");
    m.cpu.debug_break = true; // int3 (e.g. a bugcheck) traps to the debugger

    let mut stub = GdbStub::new();
    let mut buf = [0u8; 4096];
    loop {
        // Pull whatever the debugger sent.
        match sock.read(&mut buf) {
            Ok(0) => {
                eprintln!("[gdb_server] debugger disconnected");
                break;
            }
            Ok(n) => stub.on_input(&mut m, &buf[..n]),
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(e) => {
                eprintln!("[gdb_server] socket error: {e}");
                break;
            }
        }

        // Advance the machine while the debugger has it running; report the stop.
        if stub.running {
            match m.run(200_000) {
                RunStop::Breakpoint { .. }
                | RunStop::Unknown { .. }
                | RunStop::UnhandledFault { .. } => stub.report_stop(5),
                RunStop::Halted | RunStop::MaxSteps | RunStop::Syscall => {}
            }
            for b in m.take_uart_output() {
                print!("{}", b as char);
            }
            let _ = std::io::stdout().flush();
        } else {
            std::thread::sleep(Duration::from_millis(2));
        }

        // Flush replies.
        if !stub.out.is_empty() {
            let bytes: Vec<u8> = stub.out.drain(..).collect();
            if sock.write_all(&bytes).is_err() {
                break;
            }
        }
    }
}
