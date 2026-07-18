//! Cross-boot registry persistence proof: boot the kernel three times against
//! the same in-process 9P server (extended from `p9_host` with `Tlcreate`/
//! `Twrite`, so `H:\system.hive` is a real writable file on the host). Each
//! boot must mount the hive the previous boot flushed and bump
//! `HKLM\SYSTEM\PersistTest\BootCount`:
//!
//!   boot 1: loads the embedded seed hive, BootCount := 1, flushes to host
//!   boot 2: "CM: mounting host hive" + "CM: boot #2 from the persisted hive"
//!   boot 3: "CM: boot #3 from the persisted hive"
//!
//!   cargo run --release --example p9_persist

use nanox::machine::Machine;
use std::collections::HashMap;

fn main() {
    let kernel = std::env::args().nth(1).unwrap_or_else(|| {
        let rel = "../target/x86_64-unknown-none/release/kernel";
        if std::path::Path::new(rel).exists() { rel.into() }
        else { "../target/x86_64-unknown-none/debug/kernel".into() }
    });
    let image = std::fs::read(&kernel).expect("read kernel");
    let mut server = P9Server::new();

    for boot in 1..=3 {
        let mut m = Machine::new(128 * 1024 * 1024);
        m.boot_kernel(&image).expect("boot");
        let mut out = String::new();
        for _ in 0..3000 {
            m.run(20_000_000);
            for b in m.take_uart_output() {
                out.push(b as char);
            }
            server.pump(&mut m);
            if out.contains("system idle") || out.contains("SELF TESTS FAILED") {
                break;
            }
        }
        assert!(
            out.contains("ALL SELF TESTS PASSED"),
            "boot {boot}: self-tests failed:\n{}",
            &out[out.len().saturating_sub(4000)..].replace('\r', "")
        );
        if boot == 1 {
            // The seed hive was mounted, counted, and flushed to the host.
            let hive = server
                .files
                .get("system.hive")
                .expect("boot 1: kernel did not create system.hive on the host");
            assert!(
                hive.starts_with(b"regf"),
                "boot 1: flushed system.hive is not a regf file"
            );
            println!("boot 1: flushed H:\\system.hive ({} bytes, regf ok)", hive.len());
        } else {
            assert!(
                out.contains("CM: mounting host hive"),
                "boot {boot}: did not mount the host hive:\n{}",
                &out[out.len().saturating_sub(2000)..].replace('\r', "")
            );
            let want = format!("CM: boot #{boot} from the persisted hive");
            assert!(
                out.contains(&want),
                "boot {boot}: missing banner {want:?}:\n{}",
                &out[out.len().saturating_sub(2000)..].replace('\r', "")
            );
            println!("boot {boot}: {want}");
        }
        // Stop the idle machine before the next boot.
        drop(m);
    }
    println!("PASS: registry persists across boots over 9P (BootCount 1 -> 2 -> 3)");
}

/// A tiny 9P2000.L server with read + create/write: drains complete
/// T-messages from the guest's p9.tx, serves them from an in-memory file
/// map, and pushes R-messages into p9.rx.
struct P9Server {
    inbuf: Vec<u8>,
    files: HashMap<String, Vec<u8>>,
    /// fid -> backing file name (None = a directory).
    fids: HashMap<u32, Option<String>>,
    served: u32,
}

impl P9Server {
    fn new() -> Self {
        P9Server { inbuf: Vec::new(), files: HashMap::new(), fids: HashMap::new(), served: 0 }
    }
    fn pump(&mut self, m: &mut Machine) {
        while let Some(b) = m.cpu.dev.p9.tx.pop_front() {
            self.inbuf.push(b);
        }
        loop {
            if self.inbuf.len() < 7 {
                return;
            }
            let size = u32::from_le_bytes(self.inbuf[0..4].try_into().unwrap()) as usize;
            if self.inbuf.len() < size {
                return;
            }
            let msg: Vec<u8> = self.inbuf.drain(0..size).collect();
            let reply = self.serve(&msg);
            for b in reply {
                m.cpu.dev.p9.rx.push_back(b);
            }
            self.served += 1;
        }
    }
    fn serve(&mut self, msg: &[u8]) -> Vec<u8> {
        let typ = msg[4];
        let tag = u16::from_le_bytes(msg[5..7].try_into().unwrap());
        let body = &msg[7..];
        match typ {
            100 => {
                let msize = u32::from_le_bytes(body[0..4].try_into().unwrap());
                let mut r = R::new(101, tag);
                r.u32(msize);
                r.s("9P2000.L");
                r.done()
            }
            104 => {
                let fid = u32::from_le_bytes(body[0..4].try_into().unwrap());
                self.fids.insert(fid, None); // root directory
                let mut r = R::new(105, tag);
                r.qid(0);
                r.done()
            }
            110 => {
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
                if nw == 0 {
                    // Zero-name walk: clone the (root) fid — a directory.
                    self.fids.insert(newfid, None);
                    let mut r = R::new(111, tag);
                    r.u16(0);
                    r.done()
                } else if self.files.contains_key(&name) {
                    self.fids.insert(newfid, Some(name));
                    let mut r = R::new(111, tag);
                    r.u16(nw as u16);
                    for _ in 0..nw {
                        r.qid(0);
                    }
                    r.done()
                } else {
                    rlerror(tag, 2) // ENOENT
                }
            }
            12 => {
                let mut r = R::new(13, tag);
                r.qid(0);
                r.u32(0);
                r.done()
            }
            14 => {
                // Tlcreate fid name flags mode gid: create the (truncated) file
                // and make the fid refer to it, open for write.
                let fid = u32::from_le_bytes(body[0..4].try_into().unwrap());
                let nl = u16::from_le_bytes(body[4..6].try_into().unwrap()) as usize;
                let name = String::from_utf8_lossy(&body[6..6 + nl]).into_owned();
                self.files.insert(name.clone(), Vec::new());
                self.fids.insert(fid, Some(name));
                let mut r = R::new(15, tag);
                r.qid(0);
                r.u32(0); // iounit
                r.0.extend_from_slice(&[0, 0]); // exclusive, reserved
                r.done()
            }
            116 => {
                let fid = u32::from_le_bytes(body[0..4].try_into().unwrap());
                let offset = u64::from_le_bytes(body[4..12].try_into().unwrap()) as usize;
                let count = u32::from_le_bytes(body[12..16].try_into().unwrap()) as usize;
                let data = self.fids.get(&fid).and_then(|n| n.as_ref()).and_then(|n| self.files.get(n));
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
            118 => {
                // Twrite fid offset count data: grow/overwrite the file.
                let fid = u32::from_le_bytes(body[0..4].try_into().unwrap());
                let offset = u64::from_le_bytes(body[4..12].try_into().unwrap()) as usize;
                let count = u32::from_le_bytes(body[12..16].try_into().unwrap()) as usize;
                let data = &body[16..16 + count];
                let name = self.fids.get(&fid).and_then(|n| n.clone());
                let mut r = R::new(119, tag);
                match name {
                    Some(name) => {
                        let bytes = self.files.entry(name).or_default();
                        if bytes.len() < offset + count {
                            bytes.resize(offset + count, 0);
                        }
                        bytes[offset..offset + count].copy_from_slice(data);
                        r.u32(count as u32);
                    }
                    None => r.u32(0),
                }
                r.done()
            }
            120 => {
                let mut r = R::new(121, tag);
                r.done()
            }
            _ => rlerror(tag, 22), // EINVAL
        }
    }
}

fn rlerror(tag: u16, ecode: u32) -> Vec<u8> {
    let mut r = R::new(7, tag);
    r.u32(ecode);
    r.done()
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
    fn u16(&mut self, v: u16) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn u32(&mut self, v: u32) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn bytes(&mut self, b: &[u8]) {
        self.0.extend_from_slice(b);
    }
    fn s(&mut self, s: &str) {
        self.u16(s.len() as u16);
        self.0.extend_from_slice(s.as_bytes());
    }
    fn qid(&mut self, typ: u8) {
        self.0.push(typ);
        self.u32(0);
        self.0.extend_from_slice(&0u64.to_le_bytes());
    }
    fn done(mut self) -> Vec<u8> {
        let n = self.0.len() as u32;
        self.0[0..4].copy_from_slice(&n.to_le_bytes());
        self.0
    }
}
