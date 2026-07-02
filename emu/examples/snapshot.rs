//! Boot the interactive kernel to the `C:\>` prompt, then dump a machine
//! snapshot the browser can restore for an instant boot.
//!
//!   cargo run --release --example snapshot -- <kernel> <out.bin>
//!
//! Defaults: kernel = ../target/x86_64-unknown-none/release/kernel (falls back
//! to debug), out = ../web/nanox/snapshot.bin. build-wasm.sh gzips the result.

use nanox::machine::Machine;

fn main() {
    let mut args = std::env::args().skip(1);
    let kernel = args.next().unwrap_or_else(|| {
        let rel = "../target/x86_64-unknown-none/release/kernel";
        if std::path::Path::new(rel).exists() { rel.into() }
        else { "../target/x86_64-unknown-none/debug/kernel".into() }
    });
    let out = args.next().unwrap_or_else(|| "../web/nanox/snapshot.bin".into());

    let image = std::fs::read(&kernel).expect("read kernel ELF");
    let mut m = Machine::new(128 * 1024 * 1024);
    m.boot_kernel(&image).expect("boot");

    let mut text = String::new();
    let mut steps = 0;
    for _ in 0..200 {
        m.run(20_000_000);
        for b in m.take_uart_output() { text.push(b as char); }
        steps += 1;
        if text.contains("C:\\>") { break; }
    }
    assert!(text.contains("C:\\>"), "kernel did not reach the prompt");

    let snap = m.snapshot();
    std::fs::write(&out, &snap).expect("write snapshot");
    eprintln!(
        "snapshot: reached prompt after {} slices; {} bytes ({:.1} MiB) -> {}",
        steps, snap.len(), snap.len() as f64 / (1024.0 * 1024.0), out
    );
}
