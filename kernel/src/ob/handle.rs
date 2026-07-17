//! Handle tables — the user-mode → kernel-object indirection, per process.
//!
//! User mode never holds kernel pointers; it holds **handles** — small opaque
//! integers the object manager maps to referenced objects. A handle owns one
//! reference on its object, released when the handle is closed.
//!
//! Like NT (`EPROCESS.ObjectTable`), each process gets its **own** handle table,
//! keyed here by its address space (`CR3`). Kernel threads (`CR3 == 0`) share a
//! dedicated kernel table. Per-process tables are what make cross-process handle
//! inheritance correct: a child gets its *own* handle to an inherited object (a
//! pipe end, a file) via [`ob_create_handle_in`], so the parent closing its copy
//! leaves the object alive for the child — exactly `bInheritHandles` semantics,
//! and what `dir | sort` needs. Handle values follow the NT convention of being
//! multiples of 4; 0 is the NULL/invalid handle, and values are per-table (two
//! processes can each have a handle `0x10` naming different objects).

use crate::ke::spinlock::SpinLock;
use crate::ob;
use crate::rtl::NtStatus;

/// Maximum simultaneously-open handles per process.
const MAX_HANDLES: usize = 256;
/// Maximum distinct live handle tables (the kernel plus concurrent processes).
const MAX_TABLES: usize = 16;

#[derive(Clone, Copy)]
struct HandleEntry {
    object: *mut u8,
    #[allow(dead_code)]
    granted_access: u32,
    /// Open copies of this handle value. A `DuplicateHandle` shares the
    /// value and bumps this; `NtClose` drops it, and the entry (with the
    /// object reference it owns) dies only when the last copy closes.
    refs: u32,
}

struct TableSlot {
    /// Address space this table belongs to (`CR3`); 0 is the kernel table.
    cr3: u64,
    in_use: bool,
    entries: [Option<HandleEntry>; MAX_HANDLES],
}

struct Tables {
    slots: [TableSlot; MAX_TABLES],
}

// SAFETY: the raw object pointers are only touched under the spinlock.
unsafe impl Send for Tables {}

static TABLES: SpinLock<Tables> = SpinLock::new(Tables {
    slots: [const {
        TableSlot { cr3: 0, in_use: false, entries: [None; MAX_HANDLES] }
    }; MAX_TABLES],
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

/// The address space of the calling thread — its handle table's key. Kernel
/// threads (no per-process `CR3`) map to the kernel table (0).
fn current_cr3() -> u64 {
    let t = crate::ke::pcr::ke_get_current_thread();
    if t.is_null() {
        0
    } else {
        unsafe { (*t).cr3 }
    }
}

/// Find the slot index for `cr3`, allocating one if none exists. The kernel
/// table (`cr3 == 0`) is slot 0, created on first use. Returns `None` only if
/// every table slot is taken.
fn slot_for(tables: &mut Tables, cr3: u64) -> Option<usize> {
    for i in 0..MAX_TABLES {
        if tables.slots[i].in_use && tables.slots[i].cr3 == cr3 {
            return Some(i);
        }
    }
    for i in 0..MAX_TABLES {
        if !tables.slots[i].in_use {
            tables.slots[i].in_use = true;
            tables.slots[i].cr3 = cr3;
            tables.slots[i].entries = [None; MAX_HANDLES];
            return Some(i);
        }
    }
    None
}

/// Insert `object` into the table for `cr3`, returning a new handle and taking a
/// reference. 0 (NULL) if the table (or the table set) is full.
fn create_in(cr3: u64, object: *mut u8, granted_access: u32) -> u64 {
    let mut t = TABLES.lock();
    let Some(s) = slot_for(&mut t, cr3) else {
        return 0;
    };
    // Index 0 is reserved for the NULL handle; allocate from 1 up.
    for i in 1..MAX_HANDLES {
        if t.slots[s].entries[i].is_none() {
            t.slots[s].entries[i] = Some(HandleEntry { object, granted_access, refs: 1 });
            drop(t);
            // The handle owns a reference for as long as it is open.
            unsafe { ob::ob_reference_object(object) };
            return index_to_handle(i);
        }
    }
    0
}

/// `ObCreateHandle` in the **calling process's** table.
pub fn ob_create_handle(object: *mut u8, granted_access: u32) -> u64 {
    create_in(current_cr3(), object, granted_access)
}

/// `ObCreateHandle` in a **specific** address space's table. Used at process
/// creation to seed a child's standard handles and to duplicate inherited
/// handles into the child's own table.
pub fn ob_create_handle_in(cr3: u64, object: *mut u8, granted_access: u32) -> u64 {
    create_in(cr3, object, granted_access)
}

/// `ObReferenceObjectByHandle` (peek form) in the calling process's table.
/// Does *not* add a reference — the handle's own reference keeps the object
/// alive for the duration of the syscall using it.
pub fn ob_reference_object_by_handle(handle: u64) -> Result<*mut u8, NtStatus> {
    let cr3 = current_cr3();
    let i = handle_to_index(handle);
    let t = TABLES.lock();
    if i >= 1 && i < MAX_HANDLES {
        for s in 0..MAX_TABLES {
            if t.slots[s].in_use && t.slots[s].cr3 == cr3 {
                if let Some(e) = t.slots[s].entries[i] {
                    return Ok(e.object);
                }
                break;
            }
        }
    }
    Err(NtStatus::INVALID_HANDLE)
}

/// Create a second reference to the handle **value** itself: share
/// semantics. The duplicate names the same value (with the entry's
/// reference count bumped), so a handle value is a stable identity of its
/// object for as long as any copy is open — the property cmd's redirect
/// save/restore choreography depends on (`saved = _dup(1); ...;
/// _dup2(saved, 1); _close(saved)`), where a *fresh* value per dup would
/// invalidate every copy of the old value a CRT fd table or a cmd local
/// still holds. Closing any single copy keeps the object alive for the
/// rest; the last close drops the object reference. Backs `DuplicateHandle`.
pub fn duplicate_handle(handle: u64) -> u64 {
    let cr3 = current_cr3();
    let i = handle_to_index(handle);
    let mut t = TABLES.lock();
    if i >= 1 && i < MAX_HANDLES {
        for s in 0..MAX_TABLES {
            if t.slots[s].in_use && t.slots[s].cr3 == cr3 {
                if let Some(e) = &mut t.slots[s].entries[i] {
                    e.refs += 1;
                    // The new copy's reference on the object.
                    unsafe { ob::ob_reference_object(e.object) };
                    return handle;
                }
                break;
            }
        }
    }
    0
}

/// `NtClose`: close one copy of the handle. Each copy holds an object
/// reference (taken at dup time), so every close drops one — the entry dies
/// with the last copy, and with it the object's type-level accounting (a
/// pipe's writer count) stays balanced.
pub fn ob_close_handle(handle: u64) -> NtStatus {
    let cr3 = current_cr3();
    let i = handle_to_index(handle);
    let mut t = TABLES.lock();
    if i >= 1 && i < MAX_HANDLES {
        for s in 0..MAX_TABLES {
            if t.slots[s].in_use && t.slots[s].cr3 == cr3 {
                let object = if let Some(e) = &mut t.slots[s].entries[i] {
                    if e.refs > 1 {
                        e.refs -= 1;
                        Some(e.object)
                    } else {
                        None
                    }
                } else {
                    None
                };
                if let Some(object) = object {
                    // A dup copy closed: the entry lives on for the others.
                    drop(t);
                    unsafe { ob::ob_dereference_object(object) };
                    return NtStatus::SUCCESS;
                }
                if let Some(e) = t.slots[s].entries[i].take() {
                    // Last copy: drop the lock before dereferencing — the
                    // object's delete routine could itself touch the manager.
                    drop(t);
                    unsafe { ob::ob_dereference_object(e.object) };
                    return NtStatus::SUCCESS;
                }
                break;
            }
        }
    }
    NtStatus::INVALID_HANDLE
}

/// Tear down the handle table for address space `cr3` when its process exits:
/// close every open handle (dropping each object reference) and free the slot.
/// The kernel table (`cr3 == 0`) is never freed. Reclaiming per-process tables
/// keeps a long shell session from exhausting the table set.
pub fn ob_free_table(cr3: u64) {
    if cr3 == 0 {
        return;
    }
    // Collect the objects to dereference, clearing the slot under the lock, then
    // dereference outside it (delete routines may re-enter the object manager).
    let mut objs: [*mut u8; MAX_HANDLES] = [core::ptr::null_mut(); MAX_HANDLES];
    let mut n = 0;
    {
        let mut t = TABLES.lock();
        for s in 0..MAX_TABLES {
            if t.slots[s].in_use && t.slots[s].cr3 == cr3 {
                for i in 1..MAX_HANDLES {
                    if let Some(e) = t.slots[s].entries[i].take() {
                        objs[n] = e.object;
                        n += 1;
                    }
                }
                t.slots[s].in_use = false;
                t.slots[s].cr3 = 0;
                break;
            }
        }
    }
    for &obj in objs.iter().take(n) {
        unsafe { ob::ob_dereference_object(obj) };
    }
}
