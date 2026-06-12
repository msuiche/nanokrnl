//! MUI (Multilingual User Interface) string resolution.
//!
//! Modern Windows binaries keep almost no strings in the executable itself —
//! their UI text lives in a side-by-side resource-only DLL, `<exe>.mui`
//! (e.g. `en-US\choice.exe.mui`). When a program calls `LoadStringW` the
//! Windows resource loader transparently falls back to that `.mui`. We mirror
//! that: a `.mui` is **registered against a module's base address** at load
//! time, and [`load_string`] parses its `RT_STRING` resources on demand
//! (the `NtLoadMuiString` service backs the `LoadStringW` fallback).
//!
//! The `.mui` is raw file bytes (not a mapped image), so resource RVAs are
//! translated to file offsets through the section table.

use crate::ke::spinlock::SpinLock;

/// Module-base → `.mui` bytes registrations. Small fixed table; a process's
/// HINSTANCE (image base) is the key, matching what `GetModuleHandle(NULL)`
/// returns. (Distinct processes can share a base value but run one at a time,
/// so a base key suffices here.)
const MAX_MUI: usize = 8;
struct MuiTable {
    entries: [(u64, &'static [u8]); MAX_MUI],
    count: usize,
}
static TABLE: SpinLock<MuiTable> = SpinLock::new(MuiTable {
    entries: [(0, &[]); MAX_MUI],
    count: 0,
});

/// Register a `.mui` resource module for `module_base`.
pub fn register(module_base: u64, mui: &'static [u8]) {
    let mut t = TABLE.lock();
    // Replace an existing entry for this base, else append.
    for i in 0..t.count {
        if t.entries[i].0 == module_base {
            t.entries[i].1 = mui;
            return;
        }
    }
    if t.count < MAX_MUI {
        let n = t.count;
        t.entries[n] = (module_base, mui);
        t.count += 1;
    }
}

fn lookup(module_base: u64) -> Option<&'static [u8]> {
    let t = TABLE.lock();
    (0..t.count)
        .find(|&i| t.entries[i].0 == module_base)
        .map(|i| t.entries[i].1)
}

// Bounds-checked little-endian reads (return None on out-of-range).
fn u16le(b: &[u8], o: usize) -> Option<u16> {
    b.get(o..o + 2).map(|s| u16::from_le_bytes([s[0], s[1]]))
}
fn u32le(b: &[u8], o: usize) -> Option<u32> {
    b.get(o..o + 4).map(|s| u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}

/// Translate a resource RVA to a file offset within the raw `.mui`, using the
/// section table (the file is not mapped, so RVA != file offset).
fn rva_to_off(mui: &[u8], rva: u32) -> Option<usize> {
    let e = u32le(mui, 0x3C)? as usize;
    let nsec = u16le(mui, e + 6)? as usize;
    let opt_size = u16le(mui, e + 20)? as usize;
    let sec = e + 24 + opt_size;
    for i in 0..nsec {
        let o = sec + i * 40;
        let va = u32le(mui, o + 12)?;
        let vsz = u32le(mui, o + 8)?;
        let rsz = u32le(mui, o + 16)?;
        let rptr = u32le(mui, o + 20)?;
        let span = vsz.max(rsz);
        if rva >= va && rva < va + span {
            return Some((rptr + (rva - va)) as usize);
        }
    }
    None
}

/// Find the `.rsrc` section's RVA (resource directory base). `.mui` files put
/// the resource directory there even when the data-directory entry is 0.
fn rsrc_rva(mui: &[u8]) -> Option<u32> {
    let e = u32le(mui, 0x3C)? as usize;
    let nsec = u16le(mui, e + 6)? as usize;
    let opt_size = u16le(mui, e + 20)? as usize;
    let sec = e + 24 + opt_size;
    for i in 0..nsec {
        let o = sec + i * 40;
        if &mui.get(o..o + 5)? == b".rsrc" {
            return u32le(mui, o + 12);
        }
    }
    None
}

const RT_STRING: u32 = 6;

/// Find the sub-directory entry with id `want_id` in the resource directory
/// at `dir_rel` (relative to the resource base offset `res_off`). Returns the
/// sub-directory's relative offset.
fn dir_find(mui: &[u8], res_off: usize, dir_rel: usize, want_id: u32) -> Option<usize> {
    let base = res_off + dir_rel;
    let named = u16le(mui, base + 12)? as usize;
    let ids = u16le(mui, base + 14)? as usize;
    let ent = base + 16 + named * 8;
    for i in 0..ids {
        let o = ent + i * 8;
        if u32le(mui, o)? == want_id {
            return Some((u32le(mui, o + 4)? & 0x7FFF_FFFF) as usize);
        }
    }
    None
}

/// Load string `id` from the `.mui` registered for `module_base` into `out`
/// (UTF-16). Returns the number of code units, 0 if not found. Windows packs
/// strings in bundles of 16: string `id` is item `id & 15` of bundle
/// `(id >> 4) + 1`, each item a `u16` length then that many `u16` chars.
pub fn load_string(module_base: u64, id: u32, out: &mut [u16]) -> usize {
    let mui = match lookup(module_base) {
        Some(m) => m,
        None => return 0,
    };
    (|| -> Option<usize> {
        let res_rva = rsrc_rva(mui)?;
        let res_off = rva_to_off(mui, res_rva)?;
        let type_dir = dir_find(mui, res_off, 0, RT_STRING)?;
        let bundle_id = (id >> 4) + 1;
        let name_dir = dir_find(mui, res_off, type_dir, bundle_id)?;
        // First language entry → its data entry (a leaf, offset not a subdir).
        let data_rel = (u32le(mui, res_off + name_dir + 16 + 4)? & 0x7FFF_FFFF) as usize;
        // IMAGE_RESOURCE_DATA_ENTRY: OffsetToData (an RVA), Size, ...
        let blob_rva = u32le(mui, res_off + data_rel)?;
        let mut p = rva_to_off(mui, blob_rva)?;
        // Walk to item (id & 15) within the 16-string bundle.
        for _ in 0..(id & 15) {
            let len = u16le(mui, p)? as usize;
            p += 2 + len * 2;
        }
        let len = u16le(mui, p)? as usize;
        let n = len.min(out.len());
        for i in 0..n {
            out[i] = u16le(mui, p + 2 + i * 2)?;
        }
        Some(n)
    })()
    .unwrap_or(0)
}

const RT_MESSAGETABLE: u32 = 11;

/// Load message `id` from the registered `.mui`'s `RT_MESSAGETABLE` into `out`
/// (UTF-16). Returns the number of code units, 0 if not found. Backs
/// `FormatMessageW(FORMAT_MESSAGE_FROM_HMODULE)`. The message-table data is a
/// `MESSAGE_RESOURCE_DATA`: a `u32` block count, then `{LowId, HighId, Offset}`
/// blocks; entries at `Offset` are `{u16 Length, u16 Flags, text[Length-4]}`
/// (Unicode when `Flags & 1`).
pub fn load_message(module_base: u64, id: u32, out: &mut [u16]) -> usize {
    let mui = match lookup(module_base) {
        Some(m) => m,
        None => return 0,
    };
    (|| -> Option<usize> {
        let res_rva = rsrc_rva(mui)?;
        let res_off = rva_to_off(mui, res_rva)?;
        let type_dir = dir_find(mui, res_off, 0, RT_MESSAGETABLE)?;
        // First name entry under the type dir, then its first language entry.
        let named = u16le(mui, res_off + type_dir + 12)? as usize;
        let name_dir = (u32le(mui, res_off + type_dir + 16 + named * 8 + 4)? & 0x7FFF_FFFF) as usize;
        let data_rel = (u32le(mui, res_off + name_dir + 16 + 4)? & 0x7FFF_FFFF) as usize;
        let blob_rva = u32le(mui, res_off + data_rel)?;
        let mdo = rva_to_off(mui, blob_rva)?; // MESSAGE_RESOURCE_DATA start
        let nblocks = u32le(mui, mdo)? as usize;
        for b in 0..nblocks {
            let bo = mdo + 4 + b * 12;
            let low = u32le(mui, bo)?;
            let high = u32le(mui, bo + 4)?;
            let off = u32le(mui, bo + 8)? as usize;
            if id >= low && id <= high {
                let mut p = mdo + off;
                for _ in low..id {
                    p += u16le(mui, p)? as usize; // skip whole entries by Length
                }
                let len = u16le(mui, p)? as usize;
                let flags = u16le(mui, p + 2)?;
                let text_off = p + 4;
                let text_bytes = len.saturating_sub(4);
                // Entries are NUL-terminated within their padded Length; stop
                // at the first NUL so the returned text is just the message.
                return Some(if flags & 1 != 0 {
                    let nchars = text_bytes / 2;
                    let cap = nchars.min(out.len());
                    let mut n = 0;
                    while n < cap {
                        let c = u16le(mui, text_off + n * 2)?;
                        if c == 0 {
                            break;
                        }
                        out[n] = c;
                        n += 1;
                    }
                    n
                } else {
                    let cap = text_bytes.min(out.len());
                    let mut n = 0;
                    while n < cap {
                        let c = *mui.get(text_off + n)?;
                        if c == 0 {
                            break;
                        }
                        out[n] = c as u16;
                        n += 1;
                    }
                    n
                });
            }
        }
        None
    })()
    .unwrap_or(0)
}
