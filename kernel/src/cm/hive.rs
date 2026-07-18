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

// ---------------------------------------------------------------------------
// Serializer — the write half: cm's model back to a valid `regf` file.
// ---------------------------------------------------------------------------

/// Why a save failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SaveError {
    /// The root key doesn't exist.
    NoSuchKey,
    /// A key/value name isn't ASCII-representable (compressed-name cells
    /// hold ASCII; non-ASCII names need the wide-name form, unimplemented).
    NonAsciiName,
}

fn to_ascii(name: &[u16]) -> Result<alloc::vec::Vec<u8>, SaveError> {
    name.iter()
        .map(|&c| if c <= 0xFF { Ok(c as u8) } else { Err(SaveError::NonAsciiName) })
        .collect()
}

/// Wrap a payload in a cell: negative size prefix, 8-byte alignment.
fn wrap_cell(payload: &[u8], out: &mut alloc::vec::Vec<u8>) {
    let size = (4 + payload.len() + 7) & !7;
    out.extend_from_slice(&(-(size as i32)).to_le_bytes());
    out.extend_from_slice(payload);
    out.resize(out.len() + size - 4 - payload.len(), 0);
}

/// `nk` record payload (compressed name; indices may be -1 for "none").
fn nk_payload(flags: u16, parent: i32, sub_count: u32, sub_list: i32, val_count: u32, val_list: i32, name: &[u8]) -> alloc::vec::Vec<u8> {
    let mut r = alloc::vec::Vec::with_capacity(76 + name.len());
    r.extend_from_slice(b"nk");
    r.extend_from_slice(&flags.to_le_bytes());
    r.extend_from_slice(&[0; 8]); // last-write timestamp
    r.extend_from_slice(&[0; 4]); // spare
    r.extend_from_slice(&parent.to_le_bytes());
    r.extend_from_slice(&sub_count.to_le_bytes());
    r.extend_from_slice(&[0; 4]); // volatile subkey count
    r.extend_from_slice(&sub_list.to_le_bytes());
    r.extend_from_slice(&(-1i32).to_le_bytes()); // volatile subkey list
    r.extend_from_slice(&val_count.to_le_bytes());
    r.extend_from_slice(&val_list.to_le_bytes());
    r.extend_from_slice(&(-1i32).to_le_bytes()); // security
    r.extend_from_slice(&(-1i32).to_le_bytes()); // class
    r.extend_from_slice(&[0; 16]); // max name/class/value-name/value-data
    r.extend_from_slice(&[0; 4]); // workvar
    r.extend_from_slice(&(name.len() as u16).to_le_bytes());
    r.extend_from_slice(&[0; 2]); // class length
    r.extend_from_slice(name);
    r
}

/// The fast-leaf name hint: the first 4 uppercase ASCII bytes of the name,
/// zero-padded (what Windows writes in `lf`/`lh` entries).
fn name_hint(name: &[u8]) -> u32 {
    let mut h = [0u8; 4];
    for (i, &c) in name.iter().take(4).enumerate() {
        h[i] = c.to_ascii_uppercase();
    }
    u32::from_le_bytes(h)
}

/// Serialize the subtree rooted at cm key `root` into a valid `regf` hive
/// file: base block with checksum, one page-rounded hbin, and `nk`/`vk`/`lh`
/// cells for every key and value — a file Windows' own tools would open.
pub fn save(root: usize) -> Result<alloc::vec::Vec<u8>, SaveError> {
    // Collect the subtree pre-order (parents before children, as indices
    // require), recording each child's position.
    struct Node {
        parent_pos: i32,
        name: alloc::vec::Vec<u16>,
        values: alloc::vec::Vec<(alloc::vec::Vec<u16>, u32, alloc::vec::Vec<u8>)>,
        child_positions: alloc::vec::Vec<usize>,
    }
    fn collect(cm_idx: usize, parent_pos: i32, nodes: &mut alloc::vec::Vec<Node>) -> Result<(), SaveError> {
        let Some((name, children, values)) = super::key_contents(cm_idx) else {
            return Err(SaveError::NoSuchKey);
        };
        let pos = nodes.len();
        nodes.push(Node { parent_pos, name, values, child_positions: alloc::vec::Vec::new() });
        for c in children {
            let child_pos = nodes.len();
            nodes[pos].child_positions.push(child_pos);
            collect(c, pos as i32, nodes)?;
        }
        Ok(())
    }
    let mut nodes = alloc::vec::Vec::new();
    collect(root, -1, &mut nodes)?;

    // Layout: every nk (in node order), then per node its lh (if children),
    // value list (if values), vk cells, and data cells for long values.
    let mut cur = 0x20usize; // hbin header
    let mut nk_off = alloc::vec::Vec::with_capacity(nodes.len());
    let mut name_ascii: alloc::vec::Vec<alloc::vec::Vec<u8>> = alloc::vec::Vec::with_capacity(nodes.len());
    for n in &nodes {
        let a = to_ascii(&n.name)?;
        nk_off.push(cur);
        cur += (4 + 76 + a.len() + 7) & !7;
        name_ascii.push(a);
    }
    let mut lh_off: alloc::vec::Vec<i32> = alloc::vec::Vec::new();
    let mut vlist_off: alloc::vec::Vec<i32> = alloc::vec::Vec::new();
    let mut vk_offs: alloc::vec::Vec<alloc::vec::Vec<i32>> = alloc::vec::Vec::new();
    let mut data_offs: alloc::vec::Vec<alloc::vec::Vec<i32>> = alloc::vec::Vec::new();
    for n in &nodes {
        lh_off.push(if n.child_positions.is_empty() { -1 } else {
            let o = cur as i32;
            cur += (4 + 4 + 8 * n.child_positions.len() + 7) & !7;
            o
        });
        vlist_off.push(if n.values.is_empty() { -1 } else {
            let o = cur as i32;
            cur += (4 + 4 * n.values.len() + 7) & !7;
            o
        });
        let mut vks = alloc::vec::Vec::new();
        let mut datas = alloc::vec::Vec::new();
        for (vname, _t, data) in &n.values {
            let a = to_ascii(vname)?;
            vks.push(cur as i32);
            cur += (4 + 20 + a.len() + 7) & !7;
            if data.len() > 4 {
                datas.push(cur as i32);
                cur += (4 + data.len() + 7) & !7;
            } else {
                datas.push(-1);
            }
        }
        vk_offs.push(vks);
        data_offs.push(datas);
    }

    // Emit cells.
    let mut body: alloc::vec::Vec<u8> = alloc::vec::Vec::with_capacity(cur - 0x20);
    for (i, n) in nodes.iter().enumerate() {
        let flags = if i == 0 { 0x2C } else { 0x20 };
        let parent = if n.parent_pos < 0 { -1 } else { nk_off[n.parent_pos as usize] as i32 };
        let rec = nk_payload(
            flags,
            parent,
            n.child_positions.len() as u32,
            lh_off[i],
            n.values.len() as u32,
            vlist_off[i],
            &name_ascii[i],
        );
        wrap_cell(&rec, &mut body);
    }
    for (i, n) in nodes.iter().enumerate() {
        if lh_off[i] >= 0 {
            let mut rec = alloc::vec::Vec::with_capacity(4 + 8 * n.child_positions.len());
            rec.extend_from_slice(b"lh");
            rec.extend_from_slice(&(n.child_positions.len() as u16).to_le_bytes());
            for &cpos in &n.child_positions {
                rec.extend_from_slice(&(nk_off[cpos] as i32).to_le_bytes());
                rec.extend_from_slice(&name_hint(&name_ascii[cpos]).to_le_bytes());
            }
            wrap_cell(&rec, &mut body);
        }
        if vlist_off[i] >= 0 {
            let mut rec = alloc::vec::Vec::with_capacity(4 * n.values.len());
            for &vo in &vk_offs[i] {
                rec.extend_from_slice(&vo.to_le_bytes());
            }
            wrap_cell(&rec, &mut body);
        }
        for (vi, (vname, vtype, data)) in n.values.iter().enumerate() {
            let a = to_ascii(vname)?;
            let mut rec = alloc::vec::Vec::with_capacity(20 + a.len());
            rec.extend_from_slice(b"vk");
            rec.extend_from_slice(&(a.len() as u16).to_le_bytes());
            if data.len() <= 4 {
                rec.extend_from_slice(&(0x8000_0000u32 | data.len() as u32).to_le_bytes());
                let mut d = [0u8; 4];
                d[..data.len()].copy_from_slice(data);
                rec.extend_from_slice(&d);
            } else {
                rec.extend_from_slice(&(data.len() as u32).to_le_bytes());
                rec.extend_from_slice(&data_offs[i][vi].to_le_bytes());
            }
            rec.extend_from_slice(&vtype.to_le_bytes());
            rec.extend_from_slice(&1u16.to_le_bytes()); // named
            rec.extend_from_slice(&[0; 2]);
            rec.extend_from_slice(&a);
            wrap_cell(&rec, &mut body);
            if data.len() > 4 {
                wrap_cell(data, &mut body);
            }
        }
    }

    // hbin (page-rounded) + base block with checksum.
    let hbin_size = (0x20 + body.len() + 4095) & !4095;
    let mut out = alloc::vec::Vec::with_capacity(4096 + hbin_size);
    out.resize(4096, 0);
    out[0..4].copy_from_slice(b"regf");
    out[4..8].copy_from_slice(&1u32.to_le_bytes()); // sequence1
    out[8..12].copy_from_slice(&1u32.to_le_bytes()); // sequence2
    out[0x1C..0x20].copy_from_slice(&1u32.to_le_bytes()); // major
    out[0x20..0x24].copy_from_slice(&1u32.to_le_bytes()); // minor
    out[0x24..0x28].copy_from_slice(&(nk_off[0] as u32).to_le_bytes()); // root cell
    out[0x28..0x2C].copy_from_slice(&(hbin_size as u32).to_le_bytes()); // hbins size
    out[0x2C..0x30].copy_from_slice(&1u32.to_le_bytes()); // cluster factor
    let mut csum = 0u32;
    for i in (0..0x1F8).step_by(4) {
        csum ^= u32::from_le_bytes(out[i..i + 4].try_into().unwrap());
    }
    out[0x1FC..0x200].copy_from_slice(&csum.to_le_bytes());
    out.extend_from_slice(b"hbin");
    out.extend_from_slice(&[0; 4]); // hbin file offset
    out.extend_from_slice(&(hbin_size as u32).to_le_bytes());
    out.extend_from_slice(&[0; 20]); // reserved/timestamp/spare
    out.extend_from_slice(&body);
    out.resize(4096 + hbin_size, 0);
    Ok(out)
}

