//! Windows registry hive (`.dat` / `.hiv`) parser — the `regf` file format.
//!
//! The format in one breath: a 4096-byte base block ("regf", with the root
//! cell offset and total hive size), then `hbin` blocks of **cells**. Every
//! record lives in a cell at `4096 + index` (an `i32` size, negative when
//! allocated) and cross-references others by that index: `nk` key nodes
//! (with subkey lists `lf`/`lh`/`li`/`ri` and value lists), and `vk` values
//! (with inline data for ≤ 4 bytes, big-data `db` out of scope here).
//!
//! Everything is bounds-checked against the input: a corrupt hive (or a
//! hostile one, if it ever comes from the host drive) degrades to an error,
//! never to a bad pointer. The tree is grafted under a caller-chosen key
//! (e.g. `HKLM\SYSTEM`) and bounded by a cell budget so a runaway file
//! can't eat the pool.

/// Why a load failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoadError {
    /// Base block missing/malformed (no `regf`, truncated, bad sizes).
    BadBase,
    /// A cell points outside the hive or past its own hbin span.
    BadCell,
    /// A record signature doesn't match its expected type.
    BadSignature,
    /// Nesting past the recursion cap.
    TooDeep,
    /// The cell budget was exhausted (pathological input).
    BudgetExhausted,
}

/// Recursion cap for subkey walks.
const MAX_DEPTH: usize = 32;
/// Hard budget of cells to visit per load.
const CELL_BUDGET: usize = 4096;

fn u16le(b: &[u8], o: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(o..o + 2)?.try_into().ok()?))
}
fn u32le(b: &[u8], o: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(o..o + 4)?.try_into().ok()?))
}
fn i32le(b: &[u8], o: usize) -> Option<i32> {
    Some(i32::from_le_bytes(b.get(o..o + 4)?.try_into().ok()?))
}

/// Bounds-checked accessor for the cell at index `idx` (file offset
/// `4096 + idx`), returning the payload after its size prefix and the
/// allocated size. `hbins_size` caps the index range.
fn cell<'a>(b: &'a [u8], hbins_size: usize, idx: i32) -> Option<&'a [u8]> {
    if idx < 0 || (idx as usize) >= hbins_size {
        return None;
    }
    let off = 4096usize.checked_add(idx as usize)?;
    let size = i32le(b, off)?;
    let size = size.checked_neg()?.max(4) as usize; // negative = allocated
    let payload = off.checked_add(4)?;
    b.get(payload..payload + size - 4)
}

struct Parser<'a> {
    b: &'a [u8],
    hbins_size: usize,
    visited: usize,
}

impl<'a> Parser<'a> {
    fn cell(&mut self, idx: i32) -> Result<&'a [u8], LoadError> {
        if self.visited >= CELL_BUDGET {
            return Err(LoadError::BudgetExhausted);
        }
        self.visited += 1;
        cell(self.b, self.hbins_size, idx).ok_or(LoadError::BadCell)
    }

    /// Decode an `nk` name: ASCII when flag 0x20 (`KEY_COMP_NAME`), UTF-16LE
    /// otherwise. Returns the units (not NUL-terminated).
    fn nk_name(&self, c: &[u8], flags: u16, off: usize, len: usize) -> Option<alloc::vec::Vec<u16>> {
        let raw = c.get(off..off + len)?;
        let mut out = alloc::vec::Vec::with_capacity(len);
        if flags & 0x20 != 0 {
            for &x in raw {
                out.push(x as u16);
            }
        } else {
            for ch in raw.chunks_exact(2) {
                out.push(u16::from_le_bytes([ch[0], ch[1]]));
            }
        }
        Some(out)
    }

    /// Enumerate the subkey *cell indices* of an `nk` (following lf/lh/li/ri).
    fn subkey_indices(&mut self, list_idx: i32, out: &mut alloc::vec::Vec<i32>) -> Result<(), LoadError> {
        if list_idx < 0 {
            return Ok(()); // no list
        }
        let c = self.cell(list_idx)?;
        let sig = c.first_chunk::<2>().copied().unwrap_or([0, 0]);
        let count = u16le(c, 2).ok_or(LoadError::BadCell)? as usize;
        match &sig {
            b"lf" | b"lh" => {
                for i in 0..count {
                    let o = 4 + i * 8;
                    out.push(i32le(c, o).ok_or(LoadError::BadCell)?);
                }
            }
            b"li" | b"ri" => {
                for i in 0..count {
                    let o = 4 + i * 4;
                    let idx = i32le(c, o).ok_or(LoadError::BadCell)?;
                    if sig == *b"li" {
                        out.push(idx);
                    } else {
                        // ri: each entry is another list; recurse one level.
                        self.subkey_indices(idx, out)?;
                    }
                }
            }
            _ => return Err(LoadError::BadSignature),
        }
        Ok(())
    }

    /// Load one value into `key` (cm index). `idx` names the `vk` cell.
    fn load_value(&mut self, key: usize, idx: i32) -> Result<(), LoadError> {
        let c = self.cell(idx)?;
        if c.first_chunk::<2>().copied().unwrap_or([0, 0]) != *b"vk" {
            return Err(LoadError::BadSignature);
        }
        let name_len = u16le(c, 2).ok_or(LoadError::BadCell)? as usize;
        let data_size = u32le(c, 4).ok_or(LoadError::BadCell)?;
        let inline = data_size & 0x8000_0000 != 0;
        let dlen = (data_size & 0x7FFF_FFFF) as usize;
        let data_idx = i32le(c, 8).ok_or(LoadError::BadCell)?;
        let vtype = u32le(c, 12).ok_or(LoadError::BadCell)?;
        let flags = u16le(c, 16).ok_or(LoadError::BadCell)?;

        // The value's bytes: inline in the index field (≤ 4 bytes) or in the
        // referenced data cell.
        let data: alloc::vec::Vec<u8> = if inline {
            if dlen > 4 {
                return Err(LoadError::BadCell);
            }
            let raw = c.get(8..8 + 4).ok_or(LoadError::BadCell)?;
            raw[..dlen].to_vec()
        } else {
            let dc = self.cell(data_idx)?;
            if dlen > dc.len() {
                return Err(LoadError::BadCell);
            }
            dc[..dlen].to_vec()
        };

        // Value name (ASCII for named values in modern hives).
        let name: alloc::vec::Vec<u16> = if flags & 1 != 0 {
            let raw = c.get(20..20 + name_len).ok_or(LoadError::BadCell)?;
            raw.iter().map(|&x| x as u16).collect()
        } else {
            alloc::vec::Vec::new() // (Default) value
        };

        if !super::add_value(key, &name, vtype, &data) {
            return Err(LoadError::BudgetExhausted);
        }
        Ok(())
    }

    /// Load the `nk` at `idx` as a child of `parent` (cm index), then recurse
    /// into its subkeys and values.
    fn load_key(&mut self, idx: i32, parent: usize, depth: usize) -> Result<(), LoadError> {
        if depth > MAX_DEPTH {
            return Err(LoadError::TooDeep);
        }
        let c = self.cell(idx)?;
        if c.first_chunk::<2>().copied().unwrap_or([0, 0]) != *b"nk" {
            return Err(LoadError::BadSignature);
        }
        let flags = u16le(c, 2).ok_or(LoadError::BadCell)?;
        let sub_count = u32le(c, 20).ok_or(LoadError::BadCell)? as usize;
        let sub_list = i32le(c, 28).ok_or(LoadError::BadCell)?;
        let val_count = u32le(c, 36).ok_or(LoadError::BadCell)? as usize;
        let val_list = i32le(c, 40).ok_or(LoadError::BadCell)?;
        let name_len = u16le(c, 72).ok_or(LoadError::BadCell)? as usize;
        let name = self.nk_name(c, flags, 76, name_len).ok_or(LoadError::BadCell)?;

        let me = super::add_key(parent, &name).ok_or(LoadError::BudgetExhausted)?;

        // Values: the list is a plain run of cell indices.
        if val_count > 0 && val_list >= 0 {
            let vl = self.cell(val_list)?;
            for i in 0..val_count {
                let vi = i32le(vl, i * 4).ok_or(LoadError::BadCell)?;
                self.load_value(me, vi)?;
            }
        }

        // Subkeys.
        if sub_count > 0 {
            let mut indices = alloc::vec::Vec::with_capacity(sub_count.min(64));
            self.subkey_indices(sub_list, &mut indices)?;
            for si in indices {
                self.load_key(si, me, depth + 1)?;
            }
        }
        Ok(())
    }
}

/// Load the hive file at `bytes` and graft its root key's **children** under
/// `graft` (a path from HKLM, e.g. `w!("SYSTEM")`). Returns the number of
/// keys (including the graft root) loaded.
pub fn load(bytes: &[u8], graft: &[u16]) -> Result<usize, LoadError> {
    // Base block: "regf", hbins size at 0x28, root cell at 0x24.
    if bytes.len() < 4096 || bytes.first_chunk::<4>() != Some(b"regf") {
        return Err(LoadError::BadBase);
    }
    let root_idx = u32le(bytes, 0x24).ok_or(LoadError::BadBase)? as i32;
    let hbins_size = u32le(bytes, 0x28).ok_or(LoadError::BadBase)? as usize;
    if hbins_size == 0 || 4096 + hbins_size > bytes.len() {
        return Err(LoadError::BadBase);
    }

    let graft_root = super::graft_root(2 /* HKLM */, graft).ok_or(LoadError::BudgetExhausted)?;

    let mut p = Parser { b: bytes, hbins_size, visited: 0 };
    // The hive root `nk` itself is a container (its name is the hive's own
    // name); load its children under the graft point.
    let c = p.cell(root_idx)?;
    if c.first_chunk::<2>().copied().unwrap_or([0, 0]) != *b"nk" {
        return Err(LoadError::BadSignature);
    }
    let sub_count = u32le(c, 20).ok_or(LoadError::BadCell)? as usize;
    let sub_list = i32le(c, 28).ok_or(LoadError::BadCell)?;
    let val_count = u32le(c, 36).ok_or(LoadError::BadCell)? as usize;
    let val_list = i32le(c, 40).ok_or(LoadError::BadCell)?;

    if val_count > 0 && val_list >= 0 {
        let vl = p.cell(val_list)?;
        for i in 0..val_count {
            let vi = i32le(vl, i * 4).ok_or(LoadError::BadCell)?;
            p.load_value(graft_root, vi)?;
        }
    }
    let mut loaded = 1usize;
    if sub_count > 0 {
        let mut indices = alloc::vec::Vec::with_capacity(sub_count.min(64));
        p.subkey_indices(sub_list, &mut indices)?;
        for si in indices {
            p.load_key(si, graft_root, 1)?;
            loaded += 1;
        }
    }
    Ok(loaded)
}
