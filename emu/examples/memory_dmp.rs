//! End-to-end test of the kernel-authored Windows crash dump: boot, type `crash`,
//! and run an in-process writable 9P server that captures the `MEMORY.DMP` the
//! kernel streams to `H:\`. Writes it to /tmp/MEMORY.DMP for inspection, then
//! sanity-checks the DUMP_HEADER64. Pairs with `core_dump` (the ELF core).
//!
//!   cargo run --release --example memory_dmp -- ../target/x86_64-unknown-none/debug/kernel

use nanox::machine::{Machine, RunStop};
use std::collections::HashMap;

fn main() {
    let kernel = std::env::args().nth(1).unwrap_or_else(|| {
        let rel = "../target/x86_64-unknown-none/release/kernel";
        if std::path::Path::new(rel).exists() { rel.into() } else { "../target/x86_64-unknown-none/debug/kernel".into() }
    });
    let image = std::fs::read(&kernel).expect("read kernel");
    let mut m = Machine::new(128 * 1024 * 1024);
    m.boot_kernel(&image).expect("boot");

    let mut srv = P9Server::new();
    let mut out = String::new();
    for _ in 0..4000 {
        m.run(20_000_000);
        for b in m.take_uart_output() { out.push(b as char); }
        srv.pump(&mut m);
        if out.contains("C:\\>") { break; }
    }
    assert!(out.contains("C:\\>"), "no prompt");

    for &b in b"crash\r" { m.cpu.dev.uart.push_rx(b); }
    let base = out.len();
    for i in 0..400_000 {
        let st = m.run(300_000);
        for b in m.take_uart_output() { out.push(b as char); }
        srv.pump(&mut m);
        if srv.dmp_done { break; } // MEMORY.DMP fully written + clunked
        if i % 2000 == 0 {
            if let Some(f) = srv.files.get("MEMORY.DMP") {
                eprint!("\r[memory_dmp] {} KB written...", f.len() / 1024);
            }
        }
        if matches!(st, RunStop::Unknown { .. } | RunStop::UnhandledFault { .. }) { break; }
    }
    eprintln!();

    println!("--- output after `crash` ---\n{}", out[base..].replace('\r', ""));
    println!("9P messages served: {}", srv.served);
    match srv.files.get("MEMORY.DMP") {
        Some(dmp) => {
            std::fs::write("/tmp/MEMORY.DMP", dmp).unwrap();
            println!("wrote /tmp/MEMORY.DMP ({} bytes)", dmp.len());
            assert!(&dmp[0..4] == b"PAGE", "bad Signature");
            assert!(&dmp[4..8] == b"DU64", "bad ValidDump");
            let rd = |o: usize| u64::from_le_bytes(dmp[o..o + 8].try_into().unwrap());
            let rd32 = |o: usize| u32::from_le_bytes(dmp[o..o + 4].try_into().unwrap());
            println!("  DirectoryTableBase = {:#x}", rd(0x10));
            println!("  PsLoadedModuleList = {:#x}", rd(0x20));
            println!("  PsActiveProcessHead= {:#x}", rd(0x28));
            println!("  KdDebuggerDataBlock= {:#x}", rd(0x80));
            println!("  MachineImageType   = {:#x}", rd32(0x30));
            println!("  BugCheckCode       = {:#x}", rd32(0x38));
            println!("  DumpType           = {}", rd32(0xf98));
            println!("PASS: kernel wrote a DUMP_HEADER64 MEMORY.DMP to H:\\ over 9P");
        }
        None => panic!("kernel did not write MEMORY.DMP"),
    }
}

/// A minimal writable 9P2000.L server: version/attach/walk + lcreate/write/clunk.
struct P9Server {
    inbuf: Vec<u8>,
    files: HashMap<String, Vec<u8>>,
    wname: Option<String>,
    served: u32,
    dmp_done: bool,
}
impl P9Server {
    fn new() -> Self { P9Server { inbuf: Vec::new(), files: HashMap::new(), wname: None, served: 0, dmp_done: false } }
    fn pump(&mut self, m: &mut Machine) {
        while let Some(b) = m.cpu.dev.p9.tx.pop_front() { self.inbuf.push(b); }
        loop {
            if self.inbuf.len() < 7 { return; }
            let size = u32::from_le_bytes(self.inbuf[0..4].try_into().unwrap()) as usize;
            if size < 7 || self.inbuf.len() < size { return; }
            let msg: Vec<u8> = self.inbuf.drain(0..size).collect();
            let reply = self.serve(&msg);
            for b in reply { m.cpu.dev.p9.rx.push_back(b); }
            self.served += 1;
        }
    }
    fn serve(&mut self, msg: &[u8]) -> Vec<u8> {
        let typ = msg[4];
        let tag = u16::from_le_bytes(msg[5..7].try_into().unwrap());
        let body = &msg[7..];
        match typ {
            100 => { let ms = u32::from_le_bytes(body[0..4].try_into().unwrap()); let mut r = R::new(101, tag); r.u32(ms); r.s("9P2000.L"); r.done() }
            104 => { let mut r = R::new(105, tag); r.qid(); r.done() }
            110 => {
                let mut r = R::new(111, tag);
                let nw = u16::from_le_bytes(body[8..10].try_into().unwrap());
                r.u16(nw);
                for _ in 0..nw { r.qid(); }
                r.done()
            }
            14 => {
                let l = u16::from_le_bytes(body[4..6].try_into().unwrap()) as usize;
                let name = String::from_utf8_lossy(&body[6..6 + l]).into_owned();
                self.files.insert(name.clone(), Vec::new());
                self.wname = Some(name);
                let mut r = R::new(15, tag); r.qid(); r.u32(0); r.done()
            }
            118 => {
                let offset = u64::from_le_bytes(body[4..12].try_into().unwrap()) as usize;
                let count = u32::from_le_bytes(body[12..16].try_into().unwrap()) as usize;
                let data = &body[16..16 + count];
                if let Some(name) = &self.wname {
                    let f = self.files.get_mut(name).unwrap();
                    if f.len() < offset + count { f.resize(offset + count, 0); }
                    f[offset..offset + count].copy_from_slice(data);
                }
                let mut r = R::new(119, tag); r.u32(count as u32); r.done()
            }
            120 => {
                if self.wname.as_deref() == Some("MEMORY.DMP") { self.dmp_done = true; }
                self.wname = None; R::new(121, tag).done()
            }
            _ => { let mut r = R::new(7, tag); r.u32(22); r.done() }
        }
    }
}
struct R(Vec<u8>);
impl R {
    fn new(typ: u8, tag: u16) -> Self { let mut v = vec![0u8; 4]; v.push(typ); v.extend_from_slice(&tag.to_le_bytes()); R(v) }
    fn u16(&mut self, v: u16) { self.0.extend_from_slice(&v.to_le_bytes()); }
    fn u32(&mut self, v: u32) { self.0.extend_from_slice(&v.to_le_bytes()); }
    fn s(&mut self, s: &str) { self.u16(s.len() as u16); self.0.extend_from_slice(s.as_bytes()); }
    fn qid(&mut self) { self.0.push(0); self.u32(0); self.0.extend_from_slice(&0u64.to_le_bytes()); }
    fn done(mut self) -> Vec<u8> { let n = self.0.len() as u32; self.0[0..4].copy_from_slice(&n.to_le_bytes()); self.0 }
}
