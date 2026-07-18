//! Configuration Manager (the registry).
//!
//! A dynamic in-memory hive: a forest of keys (HKCR/HKCU/HKLM/HKU/…) each
//! with named subkeys and named values. This is the kernel-side store; the
//! `kernel32` `Reg*` shims translate the Win32 ABI onto the syscalls in
//! `syscalls.rs` that call into here. It serves a modern CLI (cmd.exe reads
//! its `Command Processor` configuration here, creates/queries keys,
//! enumerates), and — via [`crate::cm::hive`] — loads real Windows hives
//! from file bytes.
//!
//! Handles: the predefined roots are the well-known `HKEY_*` constants
//! (`0x8000_000x`, which arrive sign-extended as `0xFFFFFFFF_8000_000x`); an
//! opened subkey is returned as `HANDLE_BASE + key_index`. Indices are
//! stable (the store grows, slots never move), so handles stay valid for
//! the session; `RegCloseKey` is a no-op (keys live for the session).

pub mod hive;

use crate::ke::spinlock::SpinLock;
use alloc::vec::Vec;

/// Opened-subkey handles start here (predefined roots use the `HKEY_*` values).
pub const HANDLE_BASE: u64 = 0x2000_0000;

struct Key {
    /// Parent key index, or -1 for a forest root.
    parent: i32,
    /// Key name (UTF-16 units).
    name: Vec<u16>,
}

struct Value {
    /// Owning key index.
    key: i32,
    name: Vec<u16>,
    vtype: u32,
    data: Vec<u8>,
}

struct Hive {
    /// Index-stable slots: `Some` = live, `None` = free for reuse.
    keys: Vec<Option<Key>>,
    values: Vec<Option<Value>>,
    initialized: bool,
}

static HIVE: SpinLock<Hive> = SpinLock::new(Hive {
    keys: Vec::new(),
    values: Vec::new(),
    initialized: false,
});

/// REG_SZ value type (a NUL-terminated UTF-16 string).
pub const REG_SZ: u32 = 1;
/// REG_DWORD value type.
pub const REG_DWORD: u32 = 4;

fn lc(c: u16) -> u16 {
    if (b'A' as u16..=b'Z' as u16).contains(&c) {
        c + 32
    } else {
        c
    }
}

fn name_eq(a: &[u16], b: &[u16]) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(&x, &y)| lc(x) == lc(y))
}

impl Hive {
    fn alloc_key(&mut self) -> usize {
        if let Some(i) = self.keys.iter().position(|k| k.is_none()) {
            i
        } else {
            self.keys.push(None);
            self.keys.len() - 1
        }
    }

    /// Create a forest root (parent = -1) with the given name; returns its index.
    fn make_root(&mut self, name: &[u16]) -> usize {
        let i = self.alloc_key();
        self.keys[i] = Some(Key { parent: -1, name: name.to_vec() });
        i
    }

    fn find_child(&self, parent: usize, seg: &[u16]) -> Option<usize> {
        self.keys.iter().position(|k| {
            matches!(k, Some(k) if k.parent == parent as i32 && name_eq(&k.name, seg))
        })
    }

    /// Walk `path` (backslash-delimited) from `parent`; if `create`, make
    /// missing keys. Returns the final key index.
    fn walk(&mut self, parent: usize, path: &[u16], create: bool) -> Option<usize> {
        let mut cur = parent;
        let mut i = 0;
        while i < path.len() {
            // Skip separators.
            while i < path.len() && path[i] == b'\\' as u16 {
                i += 1;
            }
            let start = i;
            while i < path.len() && path[i] != b'\\' as u16 {
                i += 1;
            }
            if i == start {
                break; // trailing separator / empty
            }
            let seg = &path[start..i];
            match self.find_child(cur, seg) {
                Some(c) => cur = c,
                None => {
                    if !create {
                        return None;
                    }
                    let ni = self.alloc_key();
                    self.keys[ni] = Some(Key { parent: cur as i32, name: seg.to_vec() });
                    cur = ni;
                }
            }
        }
        Some(cur)
    }

    fn find_value(&self, key: usize, name: &[u16]) -> Option<usize> {
        self.values.iter().position(|v| {
            matches!(v, Some(v) if v.key == key as i32 && name_eq(&v.name, name))
        })
    }

    fn set_value(&mut self, key: usize, name: &[u16], vtype: u32, data: &[u8]) -> bool {
        let idx = match self.find_value(key, name) {
            Some(i) => i,
            None => {
                let i = if let Some(i) = self.values.iter().position(|v| v.is_none()) {
                    i
                } else {
                    self.values.push(None);
                    self.values.len() - 1
                };
                self.values[i] = Some(Value { key: key as i32, name: name.to_vec(), vtype, data: data.to_vec() });
                return true;
            }
        };
        if let Some(v) = &mut self.values[idx] {
            v.vtype = vtype;
            v.data.clear();
            v.data.extend_from_slice(data);
        }
        true
    }

    /// The nth (0-based) subkey of `key`; returns (index).
    fn enum_key(&self, key: usize, n: usize) -> Option<usize> {
        let mut count = 0;
        for (i, k) in self.keys.iter().enumerate() {
            if matches!(k, Some(k) if k.parent == key as i32) {
                if count == n {
                    return Some(i);
                }
                count += 1;
            }
        }
        None
    }
}

/// Seed the predefined roots and a small amount of real content so the hive is
/// genuinely functional. Idempotent.
pub fn init() {
    let mut h = HIVE.lock();
    if h.initialized {
        return;
    }
    // Roots, in HKEY order: index i == (HKEY_* & 7).
    let hkcr = h.make_root(crate::w!("HKCR")); // 0x80000000
    let hkcu = h.make_root(crate::w!("HKCU")); // 0x80000001
    let hklm = h.make_root(crate::w!("HKLM")); // 0x80000002
    let hku = h.make_root(crate::w!("HKU")); // 0x80000003
    debug_assert!(hkcr == 0 && hkcu == 1 && hklm == 2 && hku == 3);

    // A couple of real values under HKLM so queries return live data:
    // HKLM\Software\Microsoft\Command Processor\{CompletionChar, EnableExtensions}.
    if let Some(cp) = h.walk(hklm, crate::w!("Software\\Microsoft\\Command Processor"), true) {
        let tab = [0x09u8, 0, 0, 0]; // TAB completion
        h.set_value(cp, crate::w!("CompletionChar"), REG_DWORD, &tab);
        let one = [1u8, 0, 0, 0];
        h.set_value(cp, crate::w!("EnableExtensions"), REG_DWORD, &one);
    }
    // HKLM\Software\Microsoft\Windows NT\CurrentVersion — the placeholder OS
    // version (1.0.1.1). Tools (e.g. cmd's banner) read the build/UBR here.
    if let Some(cv) = h.walk(hklm, crate::w!("Software\\Microsoft\\Windows NT\\CurrentVersion"), true) {
        let one = [1u8, 0, 0, 0];
        let zero = [0u8, 0, 0, 0];
        h.set_value(cv, crate::w!("CurrentMajorVersionNumber"), REG_DWORD, &one);
        h.set_value(cv, crate::w!("CurrentMinorVersionNumber"), REG_DWORD, &one);
        h.set_value(cv, crate::w!("UBR"), REG_DWORD, &one);
        // CurrentBuildNumber = REG_SZ "31337" (nanokrnl 1.1.31337), UTF-16 + NUL.
        h.set_value(
            cv,
            crate::w!("CurrentBuildNumber"),
            REG_SZ,
            &[0x33, 0x00, 0x31, 0x00, 0x33, 0x00, 0x33, 0x00, 0x37, 0x00, 0x00, 0x00],
        );
        let _ = zero;
    }
    h.initialized = true;
    drop(h);

    // Mount the system hive, preferring the host's copy: if H:\system.hive
    // exists it holds state from previous boots (this is what makes the
    // registry actually persistent); the embedded hive (tools/gen_hive.py)
    // is the first-boot seed. Under transports with no 9P server the probe
    // fails fast and we fall back.
    let host_bytes = crate::io::p9::read("system.hive");
    let image = match &host_bytes {
        Some(b) => {
            crate::kd_println!("CM: mounting host hive H:\\system.hive ({} bytes)", b.len());
            b
        }
        None => crate::init::HIVE_IMAGE,
    };
    if !image.is_empty() {
        match hive::load(image, crate::w!("SYSTEM")) {
            Ok(n) => crate::kd_println!("CM: hive loaded — {} keys under HKLM\\SYSTEM", n),
            Err(e) => crate::kd_println!("CM: hive load failed: {:?}", e),
        }
    }

    // The cross-boot persistence proof: bump HKLM\SYSTEM\PersistTest\BootCount
    // on every boot and flush the hive back to the host, so the next boot
    // reads what this one wrote.
    const HKLM: u64 = 0x8000_0002;
    let key = create_key(HKLM, crate::w!("SYSTEM\\PersistTest"));
    if key != 0 {
        let mut t = 0u32;
        let mut b = [0u8; 4];
        let prev = if query_value(key, crate::w!("BootCount"), &mut t, &mut b) == 4 {
            u32::from_le_bytes(b)
        } else {
            0
        };
        let cur = prev + 1;
        set_value(key, crate::w!("BootCount"), REG_DWORD, &cur.to_le_bytes());
        if cur > 1 {
            crate::kd_println!("CM: boot #{} from the persisted hive", cur);
        }
        flush_to_host();
    }
}

/// Serialize `HKLM\SYSTEM` and stream it to `H:\system.hive` on the host 9P
/// drive, finalizing the persistence loop. Returns false when there is no
/// hive to save or no live 9P server (plain QEMU: registry stays in RAM).
pub fn flush_to_host() -> bool {
    let Some(bytes) = save_hive(crate::w!("SYSTEM")) else { return false };
    let Some(mut w) = crate::io::p9::create("system.hive") else { return false };
    let ok = w.write(&bytes);
    w.close();
    ok
}

/// Resolve an `HKEY` to a key index. Handles predefined roots (sign-extended
/// `0x8000_000x`) and opened-subkey handles (`HANDLE_BASE + index`).
fn resolve(h: &Hive, hkey: u64) -> Option<usize> {
    // Predefined: low 32 bits are 0x8000_000x.
    if (hkey as u32) & 0xFFFF_FFF8 == 0x8000_0000 {
        let r = (hkey & 0x7) as usize;
        if r < h.keys.len() && h.keys[r].is_some() {
            return Some(r);
        }
        return None;
    }
    if hkey >= HANDLE_BASE {
        let i = (hkey - HANDLE_BASE) as usize;
        if i < h.keys.len() && h.keys[i].is_some() {
            return Some(i);
        }
    }
    None
}

/// `RegOpenKeyEx` backend: open an existing subkey. Returns its handle or 0.
pub fn open_key(parent: u64, path: &[u16]) -> u64 {
    let mut h = HIVE.lock();
    let Some(p) = resolve(&h, parent) else { return 0 };
    match h.walk(p, path, false) {
        Some(i) => HANDLE_BASE + i as u64,
        None => 0,
    }
}

/// `RegCreateKeyEx` backend: open or create. Returns the handle or 0.
pub fn create_key(parent: u64, path: &[u16]) -> u64 {
    let mut h = HIVE.lock();
    let Some(p) = resolve(&h, parent) else { return 0 };
    match h.walk(p, path, true) {
        Some(i) => HANDLE_BASE + i as u64,
        None => 0,
    }
}

/// `RegQueryValueEx`/`RegGetValue` backend. Copies the value's data into `out`
/// (up to `out_cap` bytes), writes its type to `out_type`. Returns the byte
/// length (always the true length, even if it didn't fit), or `-1` if absent.
pub fn query_value(hkey: u64, name: &[u16], out_type: &mut u32, out: &mut [u8]) -> i64 {
    let h = HIVE.lock();
    let Some(k) = resolve(&h, hkey) else { return -1 };
    let Some(vi) = h.find_value(k, name) else { return -1 };
    let Some(v) = &h.values[vi] else { return -1 };
    *out_type = v.vtype;
    let n = v.data.len().min(out.len());
    out[..n].copy_from_slice(&v.data[..n]);
    v.data.len() as i64
}

/// `RegSetValueEx` backend: create or replace a value. Returns true on success.
pub fn set_value(hkey: u64, name: &[u16], vtype: u32, data: &[u8]) -> bool {
    let mut h = HIVE.lock();
    let Some(k) = resolve(&h, hkey) else { return false };
    h.set_value(k, name, vtype, data)
}

/// `RegEnumKeyEx` backend: copy the `n`th subkey's name into `out` (UTF-16,
/// NUL-terminated). Returns the name's char count (excluding NUL), or -1.
pub fn enum_key(hkey: u64, n: usize, out: &mut [u16]) -> i64 {
    let h = HIVE.lock();
    let Some(k) = resolve(&h, hkey) else { return -1 };
    let Some(i) = h.enum_key(k, n) else { return -1 };
    let Some(key) = &h.keys[i] else { return -1 };
    let n = key.name.len().min(out.len().saturating_sub(1));
    out[..n].copy_from_slice(&key.name[..n]);
    if n < out.len() {
        out[n] = 0;
    }
    key.name.len() as i64
}

// --- Hive-loading surface (used by cm::hive) --------------------------------

/// Graft point for a loaded hive: create (or open) a key path from a root,
/// returning its index. `root` is a predefined-root index (0..3).
pub(crate) fn graft_root(root: usize, path: &[u16]) -> Option<usize> {
    let mut h = HIVE.lock();
    if root >= h.keys.len() || h.keys[root].is_none() {
        return None;
    }
    h.walk(root, path, true)
}

/// Populate a key with a loaded subkey (name → new child index). cm-internal.
pub(crate) fn add_key(parent: usize, name: &[u16]) -> Option<usize> {
    let mut h = HIVE.lock();
    if parent >= h.keys.len() || h.keys[parent].is_none() {
        return None;
    }
    let i = h.alloc_key();
    h.keys[i] = Some(Key { parent: parent as i32, name: name.to_vec() });
    Some(i)
}

/// Populate a loaded value. cm-internal.
pub(crate) fn add_value(key: usize, name: &[u16], vtype: u32, data: &[u8]) -> bool {
    let mut h = HIVE.lock();
    if key >= h.keys.len() || h.keys[key].is_none() {
        return false;
    }
    h.set_value(key, name, vtype, data)
}

// --- Serialization surface (used by cm::hive::save) -------------------------

/// Resolve a path from HKLM to a key index (read side of save).
pub(crate) fn find_key(path: &[u16]) -> Option<usize> {
    let mut h = HIVE.lock();
    h.walk(2 /* HKLM */, path, false)
}

/// `RegSaveFile` in spirit: serialize the subtree at `path` (from HKLM)
/// into a valid `regf` hive file. None if the path is missing or a name
/// isn't ASCII-representable.
pub fn save_hive(path: &[u16]) -> Option<alloc::vec::Vec<u8>> {
    let root = find_key(path)?;
    hive::save(root).ok()
}

/// A key's name, children, and values, for serialization. cm-internal.
pub(crate) fn key_contents(key: usize) -> Option<(Vec<u16>, Vec<usize>, Vec<(Vec<u16>, u32, Vec<u8>)>)> {
    let h = HIVE.lock();
    let k = h.keys.get(key)?.as_ref()?;
    let name = k.name.clone();
    let children: Vec<usize> = h
        .keys
        .iter()
        .enumerate()
        .filter_map(|(i, c)| c.as_ref().and_then(|c| (c.parent == key as i32).then_some(i)))
        .collect();
    let values: Vec<(Vec<u16>, u32, Vec<u8>)> = h
        .values
        .iter()
        .filter_map(|v| v.as_ref())
        .filter(|v| v.key == key as i32)
        .map(|v| (v.name.clone(), v.vtype, v.data.clone()))
        .collect();
    Some((name, children, values))
}
