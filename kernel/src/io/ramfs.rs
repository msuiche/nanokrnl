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

/// The filesystem contents. A couple of demonstration files for now; the set
/// grows as real programs need specific paths (e.g. `.mui` resources).
pub static FILES: &[RamFile] = &[
    RamFile {
        path: "C:\\hello.txt",
        data: b"hello from the ntoskrnl-rs RAM filesystem\n",
    },
    RamFile {
        path: "C:\\readme.txt",
        data: b"This file lives in kernel memory, opened via NtCreateFile.\n",
    },
    // A real executable on the filesystem, so CreateProcessW("C:\\child.exe")
    // can load and launch it (the compute app — reports 5050 then exits).
    RamFile {
        path: "C:\\child.exe",
        data: include_bytes!(env!("NTOS_USERAPP2_IMAGE")),
    },
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

/// Look up a file's bytes by path.
pub fn lookup(path: &str) -> Option<&'static [u8]> {
    FILES.iter().find(|f| path_eq(f.path, path)).map(|f| f.data)
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
