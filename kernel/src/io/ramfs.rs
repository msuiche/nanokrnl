//! A tiny in-memory (RAM-backed) filesystem.
//!
//! Real Windows binaries open files by path (`CreateFileA`), read them, and
//! enumerate directories. We have no disk, so this module provides a small
//! fixed set of files held in kernel memory, reached through the normal
//! `NtCreateFile`/`NtReadFile` path: a path that isn't a device name resolves
//! to a [`FileObject`] here.
//!
//! A `FileObject` is a first-class object-manager object (so a handle owns a
//! reference and `NtClose` frees it) carrying the file's bytes and a read
//! cursor — the moral equivalent of NT's `FILE_OBJECT` + `SECTION` for a
//! read-only memory file. Files are read-only for now.

use crate::ob;
use crate::rtl::string::UnicodeString;
use crate::w;
use core::sync::atomic::{AtomicUsize, Ordering};

/// A file in the RAM filesystem: a canonical path and its bytes.
pub struct RamFile {
    pub path: &'static str,
    pub data: &'static [u8],
}

/// The filesystem contents, all in the `C:\` root. Demonstration text files
/// plus the embedded console programs — exposing the `.exe`s here lets them be
/// enumerated (`dir`, `where`) and launched by path (`CreateProcessW`). The
/// executable bytes are shared with the loader's `const` images (no second
/// copy). Entries with empty data (an image that wasn't built) are skipped by
/// lookups since their path still matches but length is zero.
pub static FILES: &[RamFile] = &[
    RamFile {
        path: "C:\\hello.txt",
        data: b"hello from the ntoskrnl-rs RAM filesystem\n",
    },
    RamFile {
        path: "C:\\readme.txt",
        data: b"This file lives in kernel memory, opened via NtCreateFile.\n",
    },
    // A compute app, so CreateProcessW("C:\\child.exe") can load + launch it.
    RamFile { path: "C:\\child.exe", data: crate::init::USERAPP2_IMAGE },
    // The real Windows console programs we run against our shims.
    RamFile { path: "C:\\sort.exe", data: crate::init::SORT_IMAGE },
    RamFile { path: "C:\\choice.exe", data: crate::init::CHOICE_IMAGE },
    RamFile { path: "C:\\where.exe", data: crate::init::WHERE_IMAGE },
    RamFile { path: "C:\\cmd.exe", data: crate::init::CMD_IMAGE },
];

/// Object-manager type for RAM files, distinct from `DEVICE_TYPE` so the read
/// path can tell a file handle from a device handle via `ob_check_type`.
pub static FILE_TYPE: ob::ObjectType = ob::ObjectType {
    name: UnicodeString::from_units(w!("File")),
    delete: None,
};

/// A read-only open file: its bytes plus a read cursor.
#[repr(C)]
pub struct FileObject {
    pub data: *const u8,
    pub len: usize,
    pub pos: AtomicUsize,
}

// SAFETY: the data pointer targets immutable static bytes; pos is atomic.
unsafe impl Send for FileObject {}
unsafe impl Sync for FileObject {}

/// Case-insensitive path comparison treating `/` and `\` as equivalent.
fn path_eq(a: &str, b: &str) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.bytes().zip(b.bytes()).all(|(x, y)| {
        let norm = |c: u8| {
            let c = if c == b'/' { b'\\' } else { c };
            if c.is_ascii_uppercase() {
                c + 32
            } else {
                c
            }
        };
        norm(x) == norm(y)
    })
}

/// Collapse a path to its meaningful segments — dropping empty and `.`
/// (current-directory) segments — joined by `\`, into `out`; returns the byte
/// length written. cmd builds paths like `C:\.\where.exe`, so the `.` must be
/// normalized away before any comparison.
fn normalize_path(s: &str, out: &mut [u8]) -> usize {
    let mut n = 0;
    for seg in s.split(|c| c == '\\' || c == '/') {
        if seg.is_empty() || seg == "." {
            continue;
        }
        if n > 0 && n < out.len() {
            out[n] = b'\\';
            n += 1;
        }
        for &c in seg.as_bytes() {
            if n < out.len() {
                out[n] = c;
                n += 1;
            }
        }
    }
    n
}

/// Compare two paths for equality after normalizing `.` segments and separator
/// style, case-insensitively.
fn path_eq_norm(a: &str, b: &str) -> bool {
    let mut ba = [0u8; 280];
    let mut bb = [0u8; 280];
    let na = normalize_path(a, &mut ba);
    let nb = normalize_path(b, &mut bb);
    match (
        core::str::from_utf8(&ba[..na]),
        core::str::from_utf8(&bb[..nb]),
    ) {
        (Ok(sa), Ok(sb)) => path_eq(sa, sb),
        _ => false,
    }
}

/// Look up a file's bytes by path (tolerating `.` segments and separator style).
pub fn lookup(path: &str) -> Option<&'static [u8]> {
    FILES
        .iter()
        .find(|f| path_eq_norm(f.path, path))
        .map(|f| f.data)
}

/// `GetFileAttributesW` backend: Win32 file attributes for `path`, or
/// `INVALID_FILE_ATTRIBUTES` (0xFFFF_FFFF) if it doesn't exist.
///
/// A drive root (`C:`, `C:\`) or any path ending in a separator is reported as
/// a directory so a shell's current-directory validation succeeds. A path that
/// matches a [`FILES`] entry is a normal file. Everything else is "not found".
pub fn attributes(path: &str) -> u32 {
    const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
    const FILE_ATTRIBUTE_NORMAL: u32 = 0x80;
    const INVALID: u32 = 0xFFFF_FFFF;

    let bytes = path.as_bytes();
    // Drive root: "X:" or "X:\" / "X:/".
    let is_drive_root = match bytes {
        [_, b':'] => true,
        [_, b':', b'\\'] | [_, b':', b'/'] => true,
        _ => false,
    };
    let trailing_sep = matches!(bytes.last(), Some(b'\\') | Some(b'/'));
    if is_drive_root || trailing_sep {
        return FILE_ATTRIBUTE_DIRECTORY;
    }
    if lookup(path).is_some() {
        FILE_ATTRIBUTE_NORMAL
    } else {
        INVALID
    }
}

/// One result of a directory enumeration: the bare file name (no directory),
/// its Win32 attributes, and its size in bytes.
pub struct DirEntry {
    pub name: &'static str,
    pub attributes: u32,
    pub size: u64,
}

/// `FindFirstFile`/`FindNextFile` backend: the `index`-th [`FILES`] entry whose
/// path matches the wildcard `pattern` (`C:\*`, `C:\*.exe`, `C:\cmd.exe`, …).
/// The pattern's directory part (everything up to the last separator) must
/// equal the file's directory; the trailing name part is glob-matched (`*` and
/// `?`, case-insensitive). Returns `None` once `index` is past the last match,
/// which is how the caller detects `ERROR_NO_MORE_FILES`.
pub fn find(pattern: &str, index: usize) -> Option<DirEntry> {
    let (pat_dir, pat_name) = split_path(pattern);
    // A bare directory pattern ("C:\", empty name part) enumerates the whole
    // directory. cmd's `dir` calls FindFirstFile on the bare root and does not
    // retry with an explicit "C:\*", so treat the empty name as a "*" wildcard.
    let pat_name = if pat_name.is_empty() { "*" } else { pat_name };
    let mut n = 0;
    for f in FILES {
        let (dir, name) = split_path(f.path);
        if dir_eq(dir, pat_dir) && glob_match(pat_name.as_bytes(), name.as_bytes()) {
            if n == index {
                return Some(DirEntry {
                    name,
                    attributes: 0x80, // FILE_ATTRIBUTE_NORMAL
                    size: f.data.len() as u64,
                });
            }
            n += 1;
        }
    }
    None
}

/// Split a path into `(directory-with-trailing-separator, file-name)`. A path
/// with no separator has an empty directory.
fn split_path(p: &str) -> (&str, &str) {
    match p.bytes().rposition(|b| b == b'\\' || b == b'/') {
        Some(i) => (&p[..=i], &p[i + 1..]),
        None => ("", p),
    }
}

/// Case-insensitive directory comparison (`/`≡`\`) tolerating a missing
/// trailing separator on either side and `.` (current-directory) segments, so
/// `C:\`, `C:`, and `C:\.\` all compare equal. cmd builds current-directory
/// search patterns like `C:\.\sort.*`, so the `.` must be normalized away.
fn dir_eq(a: &str, b: &str) -> bool {
    path_eq_norm(a, b)
}

/// Case-insensitive ASCII byte equality.
fn eq_ci(a: u8, b: u8) -> bool {
    a.to_ascii_lowercase() == b.to_ascii_lowercase()
}

/// Minimal case-insensitive glob: `*` matches any run (including empty), `?`
/// matches one character, everything else is literal. Iterative on `*` so a
/// pattern full of stars can't blow the stack.
fn glob_match(pat: &[u8], name: &[u8]) -> bool {
    match pat.split_first() {
        None => name.is_empty(),
        Some((&b'*', rest)) => {
            let mut tail = name;
            loop {
                if glob_match(rest, tail) {
                    return true;
                }
                match tail.split_first() {
                    Some((_, t)) => tail = t,
                    None => return false,
                }
            }
        }
        Some((&b'?', rest)) => !name.is_empty() && glob_match(rest, &name[1..]),
        Some((&p, rest)) => match name.split_first() {
            Some((&c, t)) if eq_ci(p, c) => glob_match(rest, t),
            _ => false,
        },
    }
}

/// Open `path` as a referenced `FileObject` (the body pointer doubles as the
/// object the handle stores). Returns `None` if no such file.
pub fn open(path: &str) -> Option<*mut FileObject> {
    let data = lookup(path)?;
    ob::ob_create_object(
        &FILE_TYPE,
        FileObject {
            data: data.as_ptr(),
            len: data.len(),
            pos: AtomicUsize::new(0),
        },
    )
    .ok()
}

/// Whether a body pointer is a `FileObject` (vs a device).
///
/// # Safety
/// `body` must be a live object-manager object pointer.
pub unsafe fn is_file_object(body: *mut u8) -> bool {
    unsafe { ob::ob_check_type(body, &FILE_TYPE).is_ok() }
}

/// Read up to `max` bytes from the file at its current cursor into `dst`,
/// advancing the cursor. Returns the byte count (0 at end of file).
///
/// # Safety
/// `file` must be a live `FileObject`; `dst` valid for `max` bytes.
pub unsafe fn read(file: *mut FileObject, dst: *mut u8, max: usize) -> usize {
    unsafe {
        let pos = (*file).pos.load(Ordering::Acquire);
        let remaining = (*file).len.saturating_sub(pos);
        let n = remaining.min(max);
        core::ptr::copy_nonoverlapping((*file).data.add(pos), dst, n);
        (*file).pos.store(pos + n, Ordering::Release);
        n
    }
}

/// Total size of the file in bytes.
///
/// # Safety
/// `file` must be a live `FileObject`.
pub unsafe fn size(file: *mut FileObject) -> usize {
    unsafe { (*file).len }
}
