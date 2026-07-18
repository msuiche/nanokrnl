//! FAT32 read support over the virtio-blk device — the `D:\` drive.
//!
//! A superfloppy layout (no partition table): the BPB at sector 0 gives the
//! geometry, the FAT gives cluster chains, and 32-byte 8.3 directory
//! entries give names. Everything is read-only for now — mount, enumerate,
//! open, read — which is the half a filesystem demo needs before write
//! support and a pagefile.

use super::virtblk;
use crate::ke::spinlock::SpinLock;
use alloc::string::String;
use alloc::vec::Vec;

const EOC: u32 = 0x0FFF_FFF8;
const ATTR_DIRECTORY: u8 = 0x10;
const ATTR_LFN: u8 = 0x0F;
const ATTR_VOLUME_ID: u8 = 0x08;
/// FAT entries per 512-byte sector.
const ENTRIES_PER_SECTOR: u32 = 128;

#[derive(Clone, Copy)]
struct Geometry {
    reserved: u32,
    fat_secs: u32,
    root_clus: u32,
    data_start: u32,
    spc: u8,
    total_secs: u32,
}

static GEO: SpinLock<Option<Geometry>> = SpinLock::new(None);

/// A snapshot of the mounted geometry (static after mount; copying it out
/// avoids holding the lock across directory walks, which would deadlock
/// against `lookup`'s own lock acquisition).
fn geo() -> Option<Geometry> {
    GEO.lock().as_ref().copied()
}

fn u16le(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes(b[o..o + 2].try_into().unwrap())
}
fn u32le(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}

/// Mount the filesystem if the block device holds a plausible FAT32 BPB.
pub fn init() -> bool {
    if virtblk::capacity_sectors() == 0 {
        return false;
    }
    let mut b0 = [0u8; 512];
    if !virtblk::read_sector(0, &mut b0) {
        return false;
    }
    if b0[510] != 0x55 || b0[511] != 0xAA {
        return false;
    }
    let (bps, spc, reserved, fats, fat_secs, root_clus, total_secs) = (
        u16le(&b0, 11) as u32,
        b0[13],
        u16le(&b0, 14) as u32,
        b0[16],
        u32le(&b0, 36),
        u32le(&b0, 44),
        u32le(&b0, 32),
    );
    if bps != 512 || spc == 0 || fats == 0 || fat_secs == 0 || total_secs == 0 || root_clus < 2 {
        return false;
    }
    let g = Geometry {
        reserved,
        fat_secs,
        root_clus,
        data_start: reserved + fats as u32 * fat_secs,
        spc,
        total_secs,
    };
    crate::kd_println!(
        "FAT: D:\\ mounted (FAT32, {} MiB, spc {}, root cluster {})",
        total_secs / 2048,
        spc,
        root_clus
    );
    *GEO.lock() = Some(g);
    true
}

/// Whether a FAT32 filesystem is mounted.
pub fn mounted() -> bool {
    GEO.lock().is_some()
}

fn cluster_sector(g: &Geometry, clus: u32) -> u64 {
    (g.data_start + (clus - 2) * g.spc as u32) as u64
}

/// Read one FAT entry, caching the current FAT sector (chains are usually
/// short and cluster-local, so a one-sector window is nearly free).
fn fat_entry(g: &Geometry, clus: u32) -> Option<u32> {
    static CACHE: SpinLock<(u32, [u8; 512])> = SpinLock::new((u32::MAX, [0u8; 512]));
    let sec = g.reserved + clus / ENTRIES_PER_SECTOR;
    let mut c = CACHE.lock();
    if c.0 != sec {
        if !virtblk::read_sector(sec as u64, &mut c.1) {
            return None;
        }
        c.0 = sec;
    }
    let e = u32le(&c.1, ((clus % ENTRIES_PER_SECTOR) * 4) as usize) & 0x0FFF_FFFF;
    Some(e)
}

/// Follow a cluster chain into `out` (first cluster included, the EOC
/// marker excluded), with a cap so a corrupt FAT can't loop forever.
fn chain(g: &Geometry, first: u32, out: &mut Vec<u32>) -> bool {
    let mut c = first;
    loop {
        out.push(c);
        let Some(next) = fat_entry(g, c) else { return false };
        if next >= EOC {
            return true;
        }
        if next < 2 || out.len() >= 4096 {
            return false;
        }
        c = next;
    }
}

/// One directory entry (8.3, LFN entries skipped).
pub struct Dirent {
    pub name: String,
    pub attr: u8,
    pub cluster: u32,
    pub size: u32,
}

/// Parse the `index`-th valid entry of the directory at `clus` (skipping
/// LFN, volume-id, deleted, and `.`/`..` entries). Cluster 0 means the root.
fn dir_entry(g: &Geometry, clus: u32, index: usize) -> Option<Dirent> {
    let clus = if clus == 0 { g.root_clus } else { clus };
    let mut chain_vec = Vec::new();
    if !chain(g, clus, &mut chain_vec) {
        return None;
    }
    let mut n = 0;
    let mut buf = [0u8; 512];
    for c in chain_vec {
        for s in 0..g.spc as u32 {
            if !virtblk::read_sector(cluster_sector(g, c) + s as u64, &mut buf) {
                return None;
            }
            for off in (0..512).step_by(32) {
                let e = &buf[off..off + 32];
                if e[0] == 0 {
                    return None; // end of directory
                }
                if e[0] == 0xE5 || e[11] == ATTR_LFN || e[11] & ATTR_VOLUME_ID != 0 {
                    continue;
                }
                // Skip "." and "..".
                if e[0] == b'.' {
                    continue;
                }
                if n == index {
                    let cluster = (u16le(e, 20) as u32) << 16 | u16le(e, 26) as u32;
                    return Some(Dirent { name: display_name(e), attr: e[11], cluster, size: u32le(e, 28) });
                }
                n += 1;
            }
        }
    }
    None
}

/// Find `name` (an 8.3 component) in the directory at `clus` (0 = root).
fn find_in_dir(g: &Geometry, clus: u32, name: &str) -> Option<Dirent> {
    let want = to_83(name)?;
    let mut chain_vec = Vec::new();
    let clus = if clus == 0 { g.root_clus } else { clus };
    if !chain(g, clus, &mut chain_vec) {
        return None;
    }
    let mut buf = [0u8; 512];
    for c in chain_vec {
        for s in 0..g.spc as u32 {
            if !virtblk::read_sector(cluster_sector(g, c) + s as u64, &mut buf) {
                return None;
            }
            for off in (0..512).step_by(32) {
                let e = &buf[off..off + 32];
                if e[0] == 0 {
                    return None;
                }
                if e[0] == 0xE5 || e[11] == ATTR_LFN || e[11] & ATTR_VOLUME_ID != 0 || e[0] == b'.' {
                    continue;
                }
                if e[..11] == want {
                    let cluster = (u16le(e, 20) as u32) << 16 | u16le(e, 26) as u32;
                    let name = display_name(e);
                    return Some(Dirent { name, attr: e[11], cluster, size: u32le(e, 28) });
                }
            }
        }
    }
    None
}

/// Render an 8.3 entry's name as "NAME.EXT" (dot only with an extension).
fn display_name(e: &[u8]) -> String {
    let nlen = e[..8].iter().rposition(|&b| b != b' ').map_or(0, |p| p + 1);
    let xlen = e[8..11].iter().rposition(|&b| b != b' ').map_or(0, |p| p + 1);
    let mut name = String::new();
    for &b in &e[..nlen] {
        name.push(b as char);
    }
    if xlen > 0 {
        name.push('.');
        for &b in &e[8..8 + xlen] {
            name.push(b as char);
        }
    }
    name
}

/// Format a path component as an 8.3 name ("HELLO.TXT" -> "HELLO   TXT",
/// "SUB" -> "SUB        "). Component-longer-than-8.3 -> None (no LFN yet).
fn to_83(name: &str) -> Option<[u8; 11]> {
    let mut out = [b' '; 11];
    let (stem, ext) = match name.split_once('.') {
        Some((s, e)) => (s, e),
        None => (name, ""),
    };
    if stem.len() > 8 || ext.len() > 3 || stem.is_empty() {
        return None;
    }
    for (i, b) in stem.bytes().enumerate() {
        out[i] = b.to_ascii_uppercase();
    }
    for (i, b) in ext.bytes().enumerate() {
        out[8 + i] = b.to_ascii_uppercase();
    }
    Some(out)
}

/// Walk a '/' or '\\' separated path from the root to a file or directory.
fn lookup(path: &str) -> Option<Dirent> {
    let g = geo()?;
    let mut clus = 0u32;
    let mut found = None;
    let mut components = path.split(['/', '\\']).filter(|s| !s.is_empty()).peekable();
    if components.peek().is_none() {
        return None;
    }
    while let Some(comp) = components.next() {
        let e = find_in_dir(&g, clus, comp)?;
        // A file mid-path is only OK as the last component.
        if components.peek().is_some() && e.attr & ATTR_DIRECTORY == 0 {
            return None;
        }
        clus = e.cluster;
        found = Some(e);
    }
    found
}

/// Read a whole file by path ("HELLO.TXT", "SUB\\NESTED.TXT"). Returns the
/// bytes, or `None` when absent / not a file / a chain error.
pub fn read(path: &str) -> Option<Vec<u8>> {
    let ent = lookup(path)?;
    if ent.attr & ATTR_DIRECTORY != 0 {
        return None; // a directory is not readable as a file
    }
    let g = geo()?;
    let mut chain_vec = Vec::new();
    if !chain(&g, ent.cluster, &mut chain_vec) {
        return None;
    }
    let mut out = Vec::with_capacity(ent.size as usize);
    let mut buf = [0u8; 512];
    for c in chain_vec {
        for s in 0..g.spc as u32 {
            if !virtblk::read_sector(cluster_sector(&g, c) + s as u64, &mut buf) {
                return None;
            }
            let take = (ent.size as usize - out.len()).min(512);
            out.extend_from_slice(&buf[..take]);
            if out.len() >= ent.size as usize {
                return Some(out);
            }
        }
    }
    Some(out)
}

/// Enumerate: the `index`-th entry of the directory at `path` ("" or "\\" =
/// root, "SUB" = a subdirectory), or the `index`-th glob match when the
/// last component carries a wildcard. Returns `(name, attributes, size)`.
pub fn find(path: &str, index: usize) -> Option<(String, u32, u64)> {
    let g = geo()?;
    // Split off a trailing glob component, if any.
    let (dir_part, glob) = match path.rsplit(['/', '\\']).next() {
        Some(last) if last.contains('*') || last.contains('?') => {
            let dir = &path[..path.len() - last.len()];
            (dir, last)
        }
        _ => (path, "*"),
    };
    let dir_clus = if dir_part.is_empty() || dir_part.chars().all(|c| c == '\\' || c == '/') {
        0
    } else {
        let ent = lookup(dir_part)?;
        if ent.attr & ATTR_DIRECTORY == 0 {
            return None;
        }
        ent.cluster
    };
    let mut n = 0usize;
    let mut i = 0usize;
    loop {
        let e = dir_entry(&g, dir_clus, i)?;
        i += 1;
        if glob_match(glob.as_bytes(), e.name.as_bytes()) {
            if n == index {
                let attr = if e.attr & ATTR_DIRECTORY != 0 { 0x10u32 } else { 0x80u32 };
                return Some((e.name, attr, e.size as u64));
            }
            n += 1;
        }
    }
}

/// `GetFileAttributesW` for the FAT drive: drive root and directories report
/// DIRECTORY, files report NORMAL, everything else "not found".
pub fn attributes(path: &str) -> u32 {
    const DIR: u32 = 0x10;
    const NORMAL: u32 = 0x80;
    const INVALID: u32 = 0xFFFF_FFFF;
    let bytes = path.as_bytes();
    let is_root = bytes.is_empty()
        || bytes == b"\\"
        || bytes == b"/"
        || matches!(bytes, [_, b':'] | [_, b':', b'\\'] | [_, b':', b'/']);
    if is_root {
        return DIR;
    }
    match lookup(path) {
        Some(ent) if ent.attr & ATTR_DIRECTORY != 0 => DIR,
        Some(_) => NORMAL,
        None => INVALID,
    }
}

/// Case-insensitive glob: `*` any run, `?` one character, else literal.
fn glob_match(pat: &[u8], name: &[u8]) -> bool {
    match pat.split_first() {
        None => name.is_empty(),
        Some((&b'*', rest)) => {
            for skip in 0..=name.len() {
                if glob_match(rest, &name[skip..]) {
                    return true;
                }
            }
            false
        }
        Some((&b'?', rest)) => !name.is_empty() && glob_match(rest, &name[1..]),
        Some((&b, rest)) => {
            !name.is_empty() && name[0].eq_ignore_ascii_case(&b) && glob_match(rest, &name[1..])
        }
    }
}
