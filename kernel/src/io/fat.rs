//! FAT32 read support over the virtio-blk device — the `D:\` drive.
//!
//! A superfloppy layout (no partition table): the BPB at sector 0 gives the
//! geometry, the FAT gives cluster chains, and 32-byte 8.3 directory
//! entries give names. Everything is read-only for now — mount, enumerate,
//! open, read — which is the half a filesystem demo needs before write
//! support and a pagefile.

use super::virtblk;
use crate::ke::spinlock::SpinLock;
use crate::ob;
use crate::rtl::string::UnicodeString;
use crate::w;
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

/// The one-sector FAT window, shared by reads and writes (a write through
/// it forces the next read to re-fetch, so allocation and traversal never
/// see a stale "free" entry).
static FAT_CACHE: SpinLock<(u32, [u8; 512])> = SpinLock::new((u32::MAX, [0u8; 512]));

/// Read one FAT entry, caching the current FAT sector (chains are usually
/// short and cluster-local, so a one-sector window is nearly free).
fn fat_entry(g: &Geometry, clus: u32) -> Option<u32> {
    let sec = g.reserved + clus / ENTRIES_PER_SECTOR;
    let mut c = FAT_CACHE.lock();
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
/// bytes, or `None` when absent / not a file / a chain error. An empty file
/// (first cluster 0) reads as an empty vector.
pub fn read(path: &str) -> Option<Vec<u8>> {
    let ent = lookup(path)?;
    if ent.attr & ATTR_DIRECTORY != 0 {
        return None; // a directory is not readable as a file
    }
    if ent.cluster < 2 {
        return Some(Vec::new()); // empty file: no cluster chain
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

// ---------------------------------------------------------------------------
// Write support: cluster allocation, chain management, directory updates.
// ---------------------------------------------------------------------------

/// First cluster to probe for free-space allocation (skip the reserved
/// bookkeeping clusters our generator uses).
const FIRST_DATA_CLUSTER: u32 = 7;

/// Allocate a free cluster: mark it EOC and zero its sectors. Scans the FAT
/// forward from a rolling hint (allocated clusters are never reused until
/// freed, so the hint only moves forward).
fn alloc_cluster(g: &Geometry) -> Option<u32> {
    static HINT: SpinLock<u32> = SpinLock::new(FIRST_DATA_CLUSTER);
    let mut hint = HINT.lock();
    let max = (g.total_secs - g.data_start) / g.spc as u32 + 2;
    let mut c = *hint;
    while c < max {
        if fat_entry(g, c)? == 0 {
            set_fat_entry(g, c, EOC);
            zero_cluster(g, c);
            *hint = c + 1;
            return Some(c);
        }
        c += 1;
    }
    None
}

/// Write one FAT entry (through the shared window, invalidated after so the
/// next read re-fetches the sector).
fn set_fat_entry(g: &Geometry, clus: u32, val: u32) {
    let sec = g.reserved + clus / ENTRIES_PER_SECTOR;
    let off = ((clus % ENTRIES_PER_SECTOR) * 4) as usize;
    let mut c = FAT_CACHE.lock();
    if c.0 != sec {
        if !virtblk::read_sector(sec as u64, &mut c.1) {
            return;
        }
        c.0 = sec;
    }
    c.1[off..off + 4].copy_from_slice(&(val & 0x0FFF_FFFF).to_le_bytes());
    if virtblk::write_sector(sec as u64, &c.1) {
        c.0 = u32::MAX; // force a re-read next time
    }
}

/// Zero every sector of a cluster.
fn zero_cluster(g: &Geometry, clus: u32) {
    let buf = [0u8; 512];
    for s in 0..g.spc as u32 {
        virtblk::write_sector(cluster_sector(g, clus) + s as u64, &buf);
    }
}

/// Free a whole chain (zero the FAT entries; data sectors are left as-is).
fn free_chain(g: &Geometry, first: u32) {
    let mut c = first;
    while c >= 2 && c < EOC {
        let Some(next) = fat_entry(g, c) else { break };
        set_fat_entry(g, c, 0);
        if next >= EOC {
            break;
        }
        c = next;
    }
}

/// Write `spc` sectors of `buf` (must be exactly `spc * 512` bytes) to `clus`.
fn write_cluster(g: &Geometry, clus: u32, buf: &[u8]) -> bool {
    for (s, chunk) in buf.chunks(512).enumerate() {
        let chunk: &[u8; 512] = chunk.try_into().expect("cluster write is sector-sized");
        if !virtblk::write_sector(cluster_sector(g, clus) + s as u64, chunk) {
            return false;
        }
    }
    true
}

/// One 8.3 directory entry, packed for writing.
fn pack_dirent(name83: &[u8; 11], attr: u8, cluster: u32, size: u32) -> [u8; 32] {
    let mut d = [0u8; 32];
    d[..11].copy_from_slice(name83);
    d[11] = attr;
    d[20..22].copy_from_slice(&((cluster >> 16) as u16).to_le_bytes());
    d[26..28].copy_from_slice(&(cluster as u16).to_le_bytes());
    d[28..32].copy_from_slice(&size.to_le_bytes());
    d
}

/// Add or replace the entry for `name` in the directory at `dir_clus`
/// (0 = root). `cluster` is the file's first cluster (0 for an empty file).
fn dir_add(g: &Geometry, dir_clus: u32, name: &str, attr: u8, cluster: u32, size: u32) -> bool {
    let dir_clus = if dir_clus == 0 { g.root_clus } else { dir_clus };
    let Some(name83) = to_83(name) else { return false };
    let mut chain_vec = Vec::new();
    if !chain(g, dir_clus, &mut chain_vec) {
        return false;
    }
    let mut buf = [0u8; 512];
    for c in chain_vec {
        for s in 0..g.spc as u32 {
            let sec = cluster_sector(g, c) + s as u64;
            if !virtblk::read_sector(sec, &mut buf) {
                return false;
            }
            for off in (0..512).step_by(32) {
                let e = &mut buf[off..off + 32];
                // Replace an existing entry with the same name, or claim a
                // free slot (0 = never used, 0xE5 = deleted).
                if e[0] == 0 || e[0] == 0xE5 || (e[11] != ATTR_LFN && e[..11] == name83) {
                    let packed = pack_dirent(&name83, attr, cluster, size);
                    e.copy_from_slice(&packed);
                    return virtblk::write_sector(sec, &buf);
                }
            }
        }
    }
    false // directory is full (no growth yet)
}

/// Create or truncate `path` to exactly `data` (a whole-file write). The
/// final component must be 8.3; the parent directory must exist.
pub fn create_file(path: &str, data: &[u8]) -> bool {
    let g = match geo() {
        Some(g) => g,
        None => return false,
    };
    let (dir_part, name) = match path.rsplit(['/', '\\']).next() {
        Some(last) if !last.is_empty() => (&path[..path.len() - last.len()], last),
        _ => return false,
    };
    let dir_clus = if dir_part.is_empty() || dir_part.chars().all(|c| c == '\\' || c == '/') {
        0
    } else {
        match lookup(dir_part) {
            Some(ent) if ent.attr & ATTR_DIRECTORY != 0 => ent.cluster,
            _ => return false,
        }
    };
    // Drop any previous contents of this file.
    if let Some(old) = find_in_dir(&g, dir_clus, name) {
        if old.cluster >= 2 {
            free_chain(&g, old.cluster);
        }
    }
    // Allocate the new chain and fill it.
    let mut first = 0u32;
    if !data.is_empty() {
        let nclusters = data.len().div_ceil(512 * g.spc as usize);
        let mut prev = 0u32;
        for i in 0..nclusters {
            let Some(c) = alloc_cluster(&g) else { return false };
            if i == 0 {
                first = c;
            } else {
                set_fat_entry(&g, prev, c);
            }
            let start = i * 512 * g.spc as usize;
            let end = (start + 512 * g.spc as usize).min(data.len());
            let mut buf = alloc::vec![0u8; 512 * g.spc as usize];
            buf[..end - start].copy_from_slice(&data[start..end]);
            if !write_cluster(&g, c, &buf) {
                return false;
            }
            prev = c;
        }
    }
    dir_add(&g, dir_clus, name, 0x20, first, data.len() as u32)
}

// ---------------------------------------------------------------------------
// Writable FAT files (write-back on close)
// ---------------------------------------------------------------------------

/// An open writable FAT file: an in-memory buffer flushed back to the FAT
/// on last close (the write-back model — the files here are small, and it
/// keeps the block layer sector-level while Win32 paths stay byte-level).
#[repr(C)]
pub struct FatWritable {
    shared: *const FatShared,
    pos: core::sync::atomic::AtomicUsize,
}
// SAFETY: `shared` is `'static` and internally synchronized.
unsafe impl Send for FatWritable {}
unsafe impl Sync for FatWritable {}

struct FatShared {
    path: alloc::string::String,
    data: SpinLock<Vec<u8>>,
}

/// Object-manager type for writable FAT files (its delete procedure is the
/// flush: last close writes the buffer back through `create_file`).
pub static FAT_WRITABLE_TYPE: ob::ObjectType = ob::ObjectType {
    name: UnicodeString::from_units(w!("FatWritable")),
    delete: Some(fat_writable_deleted),
    on_reference: None,
    on_dereference: None,
};

fn fat_writable_deleted(body: *mut u8) {
    let file = body as *mut FatWritable;
    unsafe {
        let sh = &*(*file).shared;
        let data = sh.data.lock();
        if !create_file(&sh.path, &data) {
            crate::kd_println!("FAT: write-back of {:?} FAILED", sh.path);
        }
    }
}

/// Create or truncate `path` on the FAT drive and return an open writable
/// object positioned at 0 (buffered; content flushes on close).
pub fn create_writable(path: &str) -> Option<*mut FatWritable> {
    if !create_file(path, b"") {
        return None;
    }
    let shared: &'static FatShared = alloc::boxed::Box::leak(alloc::boxed::Box::new(FatShared {
        path: alloc::string::String::from(path),
        data: SpinLock::new(Vec::new()),
    }));
    ob::ob_create_object(&FAT_WRITABLE_TYPE, FatWritable {
        shared,
        pos: core::sync::atomic::AtomicUsize::new(0),
    })
    .ok()
}

/// Open an existing FAT file for append (buffer seeded with its content;
/// flushed on close). Returns None if the file doesn't exist.
pub fn open_writable(path: &str) -> Option<*mut FatWritable> {
    let existing = read(path)?;
    let shared: &'static FatShared = alloc::boxed::Box::leak(alloc::boxed::Box::new(FatShared {
        path: alloc::string::String::from(path),
        data: SpinLock::new(existing),
    }));
    ob::ob_create_object(&FAT_WRITABLE_TYPE, FatWritable {
        shared,
        pos: core::sync::atomic::AtomicUsize::new(0),
    })
    .ok()
}

/// Whether `body` is a writable FAT file.
///
/// # Safety
/// `body` must be a live object-manager object.
pub unsafe fn is_fat_writable(body: *mut u8) -> bool {
    unsafe { ob::ob_check_type(body, &FAT_WRITABLE_TYPE).is_ok() }
}

/// Append `src` to a writable FAT file (buffered; flushed on close).
///
/// # Safety
/// `file` is a live `FatWritable`; `src` valid for `len` bytes.
pub unsafe fn write(file: *mut FatWritable, src: *const u8, len: usize) -> usize {
    let chunk = unsafe { core::slice::from_raw_parts(src, len) }.to_vec();
    let sh = unsafe { &*(*file).shared };
    sh.data.lock().extend_from_slice(&chunk);
    len
}

/// Read from a writable FAT file at its cursor (buffered content).
///
/// # Safety
/// `file` is a live `FatWritable`; `dst` valid for `max` bytes.
pub unsafe fn read_writable(file: *mut FatWritable, dst: *mut u8, max: usize) -> usize {
    let sh = unsafe { &*(*file).shared };
    let (chunk, n) = {
        let d = sh.data.lock();
        let pos = unsafe { (*file).pos.load(core::sync::atomic::Ordering::Acquire) };
        let n = d.len().saturating_sub(pos).min(max);
        (d[pos..pos + n].to_vec(), n)
    };
    unsafe {
        core::ptr::copy_nonoverlapping(chunk.as_ptr(), dst, n);
        (*file).pos.fetch_add(n, core::sync::atomic::Ordering::AcqRel);
    }
    n
}

/// Current buffered size of a writable FAT file.
///
/// # Safety
/// `file` is a live `FatWritable`.
pub unsafe fn writable_size(file: *mut FatWritable) -> usize {
    unsafe { (*(*file).shared).data.lock().len() }
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
