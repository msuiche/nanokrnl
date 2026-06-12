//! Handle table — the user-mode → kernel-object indirection.
//!
//! User mode never holds kernel pointers; it holds **handles** — small
//! opaque integers the object manager maps to referenced objects. A handle
//! owns one reference on its object, released when the handle is closed.
//!
//! NT keeps a per-process handle table (`EPROCESS.ObjectTable`). We have a
//! single shared address space and no real processes yet, so this is one
//! system-wide table; it becomes per-process when `Ps` grows real
//! processes. Handle values follow the NT convention of being multiples of
//! 4 (the low bits are reserved), and 0 is the NULL/invalid handle.

use crate::ke::spinlock::SpinLock;
use crate::ob;
use crate::rtl::NtStatus;

/// Maximum simultaneously-open handles in the system table.
const MAX_HANDLES: usize = 256;

#[derive(Clone, Copy)]
struct HandleEntry {
    object: *mut u8,
    #[allow(dead_code)]
    granted_access: u32,
}

struct HandleTable {
    entries: [Option<HandleEntry>; MAX_HANDLES],
}

// SAFETY: the table's raw pointers are only touched under its spinlock.
unsafe impl Send for HandleTable {}

static TABLE: SpinLock<HandleTable> = SpinLock::new(HandleTable {
    entries: [None; MAX_HANDLES],
});

/// Map a table index to a handle value (NT-style: a multiple of 4).
#[inline]
fn index_to_handle(i: usize) -> u64 {
    (i as u64) << 2
}
#[inline]
fn handle_to_index(h: u64) -> usize {
    (h >> 2) as usize
}

/// `ObCreateHandle`-equivalent: insert `object` into the table and return a
/// new handle, taking one reference on the object. Returns 0 (NULL) if the
/// table is full.
pub fn ob_create_handle(object: *mut u8, granted_access: u32) -> u64 {
    let mut t = TABLE.lock();
    // Index 0 is reserved for the NULL handle; allocate from 1 up.
    for i in 1..MAX_HANDLES {
        if t.entries[i].is_none() {
            t.entries[i] = Some(HandleEntry { object, granted_access });
            // The handle owns a reference for as long as it is open.
            unsafe { ob::ob_reference_object(object) };
            return index_to_handle(i);
        }
    }
    0
}

/// `ObReferenceObjectByHandle` (peek form): resolve `handle` to its object.
/// Does *not* add a reference — the handle's own reference keeps the object
/// alive for the duration of the syscall using it. Returns
/// `STATUS_INVALID_HANDLE` for an unknown handle.
pub fn ob_reference_object_by_handle(handle: u64) -> Result<*mut u8, NtStatus> {
    let i = handle_to_index(handle);
    let t = TABLE.lock();
    if i >= 1 && i < MAX_HANDLES {
        if let Some(e) = t.entries[i] {
            return Ok(e.object);
        }
    }
    Err(NtStatus::INVALID_HANDLE)
}

/// `NtClose`: remove the handle and drop the reference it held.
pub fn ob_close_handle(handle: u64) -> NtStatus {
    let i = handle_to_index(handle);
    let mut t = TABLE.lock();
    if i >= 1 && i < MAX_HANDLES {
        if let Some(e) = t.entries[i].take() {
            // Drop the table lock before dereferencing: the object's delete
            // routine could itself touch the object manager.
            drop(t);
            unsafe { ob::ob_dereference_object(e.object) };
            return NtStatus::SUCCESS;
        }
    }
    NtStatus::INVALID_HANDLE
}
