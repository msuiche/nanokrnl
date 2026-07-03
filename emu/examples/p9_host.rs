//! End-to-end test of the 9P host filesystem: boot the interactive kernel, run a
//! tiny in-process 9P2000.L server against the `p9` transport, and confirm that
//! `more H:\readme.txt` prints a file the server holds (not a baked-in ramfs
//! file). Proves the kernel client (io::p9), the transport, and the H:\ wiring.
//!
//!   cargo run --release --example p9_host

use nanox::machine::{Machine, RunStop};
use std::collections::HashMap;


fn main() {
    let kernel = std::env::args().nth(1).unwrap_or_else(|| {
        let rel = "../target/x86_64-unknown-none/release/kernel";
        if std::path::Path::new(rel).exists() { rel.into() }
        else { "../target/x86_64-unknown-none/debug/kernel".into() }
    });
    let image = std::fs::read(&kernel).expect("read kernel");
    let mut m = Machine::new(128 * 1024 * 1024);
    m.boot_kernel(&image).expect("boot");

    let mut server = P9Server::new();
    server.files.insert("readme.txt".into(), b"hello from the host filesystem over 9P\n".to_vec());

    let mut out = String::new();
    // Boot to the prompt (pumping the server harmlessly along the way).
    for _ in 0..2000 {
        m.run(20_000_000);
        for b in m.take_uart_output() { out.push(b as char); }
        server.pump(&mut m);
        if out.contains("C:\\>") { break; }
    }
    if !out.contains("C:\\>") {
        eprintln!("--- boot output ({} bytes) ---\n{}", out.len(), out.replace('\r', ""));
        panic!("no prompt");
    }

    // Ask more.com to read the host file.
    for &b in b"more H:\\readme.txt\r" { m.cpu.dev.uart.push_rx(b); }
    let base = out.len();
    for _ in 0..200 {
        let stop = m.run(2_000_000);
        for b in m.take_uart_output() { out.push(b as char); }
        server.pump(&mut m);
        if matches!(stop, RunStop::Unknown { .. } | RunStop::UnhandledFault { .. }) {
            out.push_str(&format!("\n[stop {:?}]", stop));
            break;
        }
        if out[base..].contains("host filesystem over 9P") { break; }
    }

    let tail = &out[base..];
    println!("--- output after `more H:\\readme.txt` ---\n{}", tail.replace('\r', ""));
    println!("served {} 9P requests", server.served);
    assert!(tail.contains("hello from the host filesystem over 9P"),
        "did not read the host file over 9P");
    println!("PASS: read a host file over 9P");
}

/// A tiny 9P2000.L server: drains complete T-messages from the guest's p9.tx,
/// serves them from an in-memory file map, and pushes R-messages into p9.rx.
struct P9Server {
    inbuf: Vec<u8>,
    files: HashMap<String, Vec<u8>>,
    fids: HashMap<u32, Option<Vec<u8>>>, // fid -> file bytes (None = a directory)
    served: u32,
}

impl P9Server {
    fn new() -> Self {
        P9Server { inbuf: Vec::new(), files: HashMap::new(), fids: HashMap::new(), served: 0 }
    }
    fn pump(&mut self, m: &mut Machine) {
        while let Some(b) = m.cpu.dev.p9.tx.pop_front() { self.inbuf.push(b); }
        loop {
            if self.inbuf.len() < 7 { return; }
            let size = u32::from_le_bytes(self.inbuf[0..4].try_into().unwrap()) as usize;
            if self.inbuf.len() < size { return; }
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
            100 => { // Tversion -> Rversion (echo msize, version 9P2000.L)
                let msize = u32::from_le_bytes(body[0..4].try_into().unwrap());
                let mut r = R::new(101, tag);
                r.u32(msize); r.s("9P2000.L"); r.done()
            }
            104 => { // Tattach fid afid uname aname n_uname -> Rattach qid
                let fid = u32::from_le_bytes(body[0..4].try_into().unwrap());
                self.fids.insert(fid, None); // root directory
                let mut r = R::new(105, tag); r.qid(0); r.done()
            }
            110 => { // Twalk fid newfid nwname names -> Rwalk qids
                let newfid = u32::from_le_bytes(body[4..8].try_into().unwrap());
                let nw = u16::from_le_bytes(body[8..10].try_into().unwrap()) as usize;
                let mut off = 10;
                let mut name = String::new();
                for _ in 0..nw {
                    let l = u16::from_le_bytes(body[off..off + 2].try_into().unwrap()) as usize;
                    off += 2;
                    name = String::from_utf8_lossy(&body[off..off + l]).into_owned();
                    off += l;
                }
                match self.files.get(&name) {
                    Some(bytes) => {
                        self.fids.insert(newfid, Some(bytes.clone()));
                        let mut r = R::new(111, tag);
                        r.u16(nw as u16);
                        for _ in 0..nw { r.qid(0); }
                        r.done()
                    }
                    None => rlerror(tag, 2), // ENOENT
                }
            }
            12 => { // Tlopen fid flags -> Rlopen qid iounit
                let mut r = R::new(13, tag); r.qid(0); r.u32(0); r.done()
            }
            116 => { // Tread fid offset count -> Rread count data
                let fid = u32::from_le_bytes(body[0..4].try_into().unwrap());
                let offset = u64::from_le_bytes(body[4..12].try_into().unwrap()) as usize;
                let count = u32::from_le_bytes(body[12..16].try_into().unwrap()) as usize;
                let data = self.fids.get(&fid).and_then(|f| f.as_ref());
                let mut r = R::new(117, tag);
                match data {
                    Some(bytes) if offset < bytes.len() => {
                        let end = (offset + count).min(bytes.len());
                        let slice = &bytes[offset..end];
                        r.u32(slice.len() as u32);
                        r.bytes(slice);
                    }
                    _ => r.u32(0),
                }
                r.done()
            }
            120 => { let mut r = R::new(121, tag); r.done() } // Tclunk -> Rclunk
            _ => rlerror(tag, 22), // EINVAL
        }
    }
}

fn rlerror(tag: u16, ecode: u32) -> Vec<u8> {
    let mut r = R::new(7, tag); r.u32(ecode); r.done()
}

/// Little-endian reply builder.
struct R(Vec<u8>);
impl R {
    fn new(typ: u8, tag: u16) -> Self {
        let mut v = vec![0u8; 4];
        v.push(typ);
        v.extend_from_slice(&tag.to_le_bytes());
        R(v)
    }
    fn u16(&mut self, v: u16) { self.0.extend_from_slice(&v.to_le_bytes()); }
    fn u32(&mut self, v: u32) { self.0.extend_from_slice(&v.to_le_bytes()); }
    fn bytes(&mut self, b: &[u8]) { self.0.extend_from_slice(b); }
    fn s(&mut self, s: &str) { self.u16(s.len() as u16); self.0.extend_from_slice(s.as_bytes()); }
    fn qid(&mut self, typ: u8) { self.0.push(typ); self.u32(0); self.0.extend_from_slice(&0u64.to_le_bytes()); }
    fn done(mut self) -> Vec<u8> {
        let n = self.0.len() as u32;
        self.0[0..4].copy_from_slice(&n.to_le_bytes());
        self.0
    }
}
