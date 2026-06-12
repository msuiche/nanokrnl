//! Configuration Manager (the registry).
//!
//! A compact in-memory hive: a forest of keys (HKCR/HKCU/HKLM/HKU/…) each with
//! named subkeys and named values. This is the kernel-side store; the
//! `kernel32` `Reg*` shims translate the Win32 ABI onto the syscalls in
//! `syscalls.rs` that call into here. It is enough for a modern CLI (cmd.exe
//! reads its `Command Processor` configuration here, creates/queries keys,
//! enumerates), without yet persisting to disk.
//!
//! Handles: the predefined roots are the well-known `HKEY_*` constants
//! (`0x8000_000x`, which arrive sign-extended as `0xFFFFFFFF_8000_000x`); an
//! opened subkey is returned as `HANDLE_BASE + key_index`. `RegCloseKey` is a
//! no-op (keys live for the session), so no handle table is needed.

use crate::ke::spinlock::SpinLock;

const MAX_KEYS: usize = 64;
const MAX_VALUES: usize = 128;
const NAME_MAX: usize = 48; // UTF-16 units
const DATA_MAX: usize = 128; // bytes

/// Opened-subkey handles start here (predefined roots use the `HKEY_*` values).
pub const HANDLE_BASE: u64 = 0x2000_0000;

#[derive(Clone, Copy)]
struct Key {
    in_use: bool,
    parent: i32, // key index, or -1 for a forest root
    name: [u16; NAME_MAX],
    name_len: usize,
}

#[derive(Clone, Copy)]
struct Value {
    in_use: bool,
    key: i32,
    name: [u16; NAME_MAX],
    name_len: usize,
    vtype: u32,
    data: [u8; DATA_MAX],
    data_len: usize,
}

struct Hive {
    keys: [Key; MAX_KEYS],
    values: [Value; MAX_VALUES],
    initialized: bool,
}

const EMPTY_KEY: Key = Key { in_use: false, parent: -1, name: [0; NAME_MAX], name_len: 0 };
const EMPTY_VALUE: Value = Value {
    in_use: false,
    key: -1,
    name: [0; NAME_MAX],
    name_len: 0,
    vtype: 0,
    data: [0; DATA_MAX],
    data_len: 0,
};

static HIVE: SpinLock<Hive> = SpinLock::new(Hive {
    keys: [EMPTY_KEY; MAX_KEYS],
    values: [EMPTY_VALUE; MAX_VALUES],
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

fn name_eq(a: &[u16], al: usize, b: &[u16]) -> bool {
    if al != b.len() {
        return false;
    }
    for i in 0..al {
        if lc(a[i]) != lc(b[i]) {
            return false;
        }
    }
    true
}

impl Hive {
    fn alloc_key(&mut self) -> Option<usize> {
        (0..MAX_KEYS).find(|&i| !self.keys[i].in_use)
    }

    /// Create a forest root (parent = -1) with the given name; returns its index.
    fn make_root(&mut self, name: &[u16]) -> usize {
        let i = self.alloc_key().expect("registry root capacity");
        let mut k = EMPTY_KEY;
        k.in_use = true;
        k.parent = -1;
        k.name_len = name.len().min(NAME_MAX);
        k.name[..k.name_len].copy_from_slice(&name[..k.name_len]);
        self.keys[i] = k;
        i
    }

    fn find_child(&self, parent: usize, seg: &[u16]) -> Option<usize> {
        (0..MAX_KEYS).find(|&i| {
            let k = &self.keys[i];
            k.in_use && k.parent == parent as i32 && name_eq(&k.name, k.name_len, seg)
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
                    let ni = self.alloc_key()?;
                    let mut k = EMPTY_KEY;
                    k.in_use = true;
                    k.parent = cur as i32;
                    k.name_len = seg.len().min(NAME_MAX);
                    k.name[..k.name_len].copy_from_slice(&seg[..k.name_len]);
                    self.keys[ni] = k;
                    cur = ni;
                }
            }
        }
        Some(cur)
    }

    fn find_value(&self, key: usize, name: &[u16]) -> Option<usize> {
        (0..MAX_VALUES).find(|&i| {
            let v = &self.values[i];
            v.in_use && v.key == key as i32 && name_eq(&v.name, v.name_len, name)
        })
    }

    fn set_value(&mut self, key: usize, name: &[u16], vtype: u32, data: &[u8]) -> bool {
        let idx = self.find_value(key, name).or_else(|| {
            let slot = (0..MAX_VALUES).find(|&i| !self.values[i].in_use)?;
            self.values[slot] = EMPTY_VALUE;
            self.values[slot].in_use = true;
            self.values[slot].key = key as i32;
            self.values[slot].name_len = name.len().min(NAME_MAX);
            let nl = self.values[slot].name_len;
            self.values[slot].name[..nl].copy_from_slice(&name[..nl]);
            Some(slot)
        });
        let Some(idx) = idx else { return false };
        self.values[idx].vtype = vtype;
        self.values[idx].data_len = data.len().min(DATA_MAX);
        let dl = self.values[idx].data_len;
        self.values[idx].data[..dl].copy_from_slice(&data[..dl]);
        true
    }

    /// The nth (0-based) subkey of `key`; returns (index).
    fn enum_key(&self, key: usize, n: usize) -> Option<usize> {
        let mut count = 0;
        for i in 0..MAX_KEYS {
            let k = &self.keys[i];
            if k.in_use && k.parent == key as i32 {
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
        h.set_value(cv, crate::w!("CurrentMinorVersionNumber"), REG_DWORD, &zero);
        h.set_value(cv, crate::w!("UBR"), REG_DWORD, &one);
        // CurrentBuildNumber = REG_SZ "1" (UTF-16: '1' 0x0031 then NUL).
        h.set_value(cv, crate::w!("CurrentBuildNumber"), REG_SZ, &[0x31, 0x00, 0x00, 0x00]);
    }
    h.initialized = true;
}

/// Resolve an `HKEY` to a key index. Handles predefined roots (sign-extended
/// `0x8000_000x`) and opened-subkey handles (`HANDLE_BASE + index`).
fn resolve(h: &Hive, hkey: u64) -> Option<usize> {
    // Predefined: low 32 bits are 0x8000_000x.
    if (hkey as u32) & 0xFFFF_FFF8 == 0x8000_0000 {
        let r = (hkey & 0x7) as usize;
        if r < MAX_KEYS && h.keys[r].in_use {
            return Some(r);
        }
        return None;
    }
    if hkey >= HANDLE_BASE {
        let i = (hkey - HANDLE_BASE) as usize;
        if i < MAX_KEYS && h.keys[i].in_use {
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
    let v = &h.values[vi];
    *out_type = v.vtype;
    let n = v.data_len.min(out.len());
    out[..n].copy_from_slice(&v.data[..n]);
    v.data_len as i64
}

/// `RegSetValueEx` backend. Returns true on success.
pub fn set_value(hkey: u64, name: &[u16], vtype: u32, data: &[u8]) -> bool {
    let mut h = HIVE.lock();
    let Some(k) = resolve(&h, hkey) else { return false };
    h.set_value(k, name, vtype, data)
}

/// `RegEnumKeyEx` backend: name of the nth subkey into `out` (UTF-16). Returns
/// the name length in chars, or `-1` past the end.
pub fn enum_key(hkey: u64, index: usize, out: &mut [u16]) -> i64 {
    let h = HIVE.lock();
    let Some(k) = resolve(&h, hkey) else { return -1 };
    let Some(ci) = h.enum_key(k, index) else { return -1 };
    let key = &h.keys[ci];
    let n = key.name_len.min(out.len());
    out[..n].copy_from_slice(&key.name[..n]);
    key.name_len as i64
}
