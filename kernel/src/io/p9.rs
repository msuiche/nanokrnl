//! Minimal 9P2000.L client over the port-mapped `p9` transport (see
//! `docs/9p-over-nanox.md`). It talks to a host-side 9P server (the JS server in
//! `web/nanox/`, or a test server) that exports a file tree, and is used to back
//! the `H:\` "host" drive so real host files reach `NtCreateFile`/`NtReadFile`.
//!
//! Transport: the guest writes a size-prefixed T-message a byte at a time to the
//! DATA port; the host drains it between run slices, serves it, and pushes the
//! R-message into the read queue, which the guest reads back through DATA once
//! STATUS reports data is ready. 9P messages are self-framing (a 4-byte size
//! prefix), so no packet boundaries are needed.
//!
//! We deliberately implement only the handful of messages needed to read a file
//! (version, attach, walk, lopen, read, clunk); `Tgetattr` is skipped by reading
//! in a loop until a short read signals end of file.

use crate::hal::port::{inb, outb};
use alloc::vec::Vec;

const DATA: u16 = 0x9F0; // read pops a response byte; write appends a request byte
const STATUS: u16 = 0x9F1; // bit0 = a response byte is ready

// 9P2000.L message types (only the ones we send/expect).
const TVERSION: u8 = 100;
const RVERSION: u8 = 101;
const TATTACH: u8 = 104;
const RATTACH: u8 = 105;
const TWALK: u8 = 110;
const RWALK: u8 = 111;
const TLOPEN: u8 = 12;
const RLOPEN: u8 = 13;
const TREAD: u8 = 116;
const RREAD: u8 = 117;
const TCLUNK: u8 = 120;

const NOTAG: u16 = 0xFFFF;
const NOFID: u32 = 0xFFFF_FFFF;
const ROOT_FID: u32 = 0;
const FILE_FID: u32 = 1;
const MSIZE: u32 = 8192;
const READ_CHUNK: u32 = 4096;

/// Spin until the transport has a response byte, then read it. Bounded so a
/// missing/wedged host server cannot hang the kernel forever.
fn read_byte() -> Option<u8> {
    let mut spins: u64 = 0;
    loop {
        if unsafe { inb(STATUS) } & 1 != 0 {
            return Some(unsafe { inb(DATA) });
        }
        spins += 1;
        if spins > 2_000_000_000 {
            return None; // host never answered
        }
        core::hint::spin_loop();
    }
}

/// Send a framed T-message and read the framed R-message it produces.
fn rpc(req: &[u8]) -> Option<Vec<u8>> {
    for &b in req {
        unsafe { outb(DATA, b) };
    }
    let mut hdr = [0u8; 4];
    for h in hdr.iter_mut() {
        *h = read_byte()?;
    }
    let size = u32::from_le_bytes(hdr) as usize;
    if !(7..=1 << 20).contains(&size) {
        return None;
    }
    let mut buf = Vec::with_capacity(size);
    buf.extend_from_slice(&hdr);
    while buf.len() < size {
        buf.push(read_byte()?);
    }
    Some(buf)
}

// --- message builders (little-endian, no padding) ------------------------

fn begin(v: &mut Vec<u8>, typ: u8, tag: u16) {
    v.extend_from_slice(&[0, 0, 0, 0]); // size, backpatched by finish()
    v.push(typ);
    v.extend_from_slice(&tag.to_le_bytes());
}
fn finish(mut v: Vec<u8>) -> Vec<u8> {
    let n = v.len() as u32;
    v[0..4].copy_from_slice(&n.to_le_bytes());
    v
}
fn pstr(v: &mut Vec<u8>, s: &str) {
    v.extend_from_slice(&(s.len() as u16).to_le_bytes());
    v.extend_from_slice(s.as_bytes());
}

fn tversion() -> Vec<u8> {
    let mut v = Vec::new();
    begin(&mut v, TVERSION, NOTAG);
    v.extend_from_slice(&MSIZE.to_le_bytes());
    pstr(&mut v, "9P2000.L");
    finish(v)
}
fn tattach() -> Vec<u8> {
    let mut v = Vec::new();
    begin(&mut v, TATTACH, 0);
    v.extend_from_slice(&ROOT_FID.to_le_bytes());
    v.extend_from_slice(&NOFID.to_le_bytes());
    pstr(&mut v, ""); // uname
    pstr(&mut v, ""); // aname
    v.extend_from_slice(&0u32.to_le_bytes()); // n_uname (9P2000.L)
    finish(v)
}
fn twalk(path: &str) -> Vec<u8> {
    let mut v = Vec::new();
    begin(&mut v, TWALK, 0);
    v.extend_from_slice(&ROOT_FID.to_le_bytes());
    v.extend_from_slice(&FILE_FID.to_le_bytes());
    let names: Vec<&str> = path.split(['/', '\\']).filter(|s| !s.is_empty()).collect();
    v.extend_from_slice(&(names.len() as u16).to_le_bytes());
    for n in names {
        pstr(&mut v, n);
    }
    finish(v)
}
fn tlopen() -> Vec<u8> {
    let mut v = Vec::new();
    begin(&mut v, TLOPEN, 0);
    v.extend_from_slice(&FILE_FID.to_le_bytes());
    v.extend_from_slice(&0u32.to_le_bytes()); // flags = O_RDONLY
    finish(v)
}
fn tread(offset: u64, count: u32) -> Vec<u8> {
    let mut v = Vec::new();
    begin(&mut v, TREAD, 0);
    v.extend_from_slice(&FILE_FID.to_le_bytes());
    v.extend_from_slice(&offset.to_le_bytes());
    v.extend_from_slice(&count.to_le_bytes());
    finish(v)
}
fn tclunk() -> Vec<u8> {
    let mut v = Vec::new();
    begin(&mut v, TCLUNK, 0);
    v.extend_from_slice(&FILE_FID.to_le_bytes());
    finish(v)
}

fn is_type(reply: &Option<Vec<u8>>, typ: u8) -> bool {
    matches!(reply, Some(r) if r.len() >= 5 && r[4] == typ)
}

/// Read a whole host file by path (e.g. "readme.txt") over 9P. Returns the file
/// bytes, or `None` on any protocol/transport failure or a missing file.
pub fn read(path: &str) -> Option<Vec<u8>> {
    if !is_type(&rpc(&tversion()), RVERSION) {
        return None;
    }
    if !is_type(&rpc(&tattach()), RATTACH) {
        return None;
    }
    if !is_type(&rpc(&twalk(path)), RWALK) {
        return None;
    }
    if !is_type(&rpc(&tlopen()), RLOPEN) {
        return None;
    }
    let mut out = Vec::new();
    let mut offset = 0u64;
    loop {
        let reply = rpc(&tread(offset, READ_CHUNK));
        let r = match reply {
            Some(ref r) if r.len() >= 11 && r[4] == RREAD => r,
            _ => break,
        };
        let count = u32::from_le_bytes(r[7..11].try_into().ok()?) as usize;
        if count == 0 || 11 + count > r.len() {
            break;
        }
        out.extend_from_slice(&r[11..11 + count]);
        offset += count as u64;
        if count < READ_CHUNK as usize {
            break;
        }
    }
    let _ = rpc(&tclunk());
    Some(out)
}
