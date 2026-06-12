//! # Ob — the Object Manager
//!
//! Everything long-lived in NT is an *object*: a reference-counted,
//! type-tagged allocation whose body is prefixed by an `OBJECT_HEADER`.
//! User mode reaches objects through handles; kernel mode through
//! referenced pointers. The manager's contract is simple and strict:
//!
//! * every object knows its type (so a wait on a "file" can be rejected),
//! * pointer references (`ObReferenceObject`) and handles each hold one
//!   reference,
//! * the object dies — type-specific delete procedure, then pool free —
//!   exactly when the last reference drops.
//!
//! Layout compatibility note: as in NT, the header sits immediately
//! *before* the object body, so `body_to_header` is constant pointer
//! arithmetic, and code holding a body pointer never sees the header.

pub mod handle;

use crate::mm::pool::{pool_free, pool_tag};
use crate::rtl::string::UnicodeString;
use crate::rtl::NtStatus;
use core::sync::atomic::{AtomicI64, Ordering};

const TAG_OBJECT: u32 = pool_tag(b"Obje");

/// Type-specific behavior — a trimmed `OBJECT_TYPE_INITIALIZER`.
pub struct ObjectType {
    /// Type name for diagnostics ("Event", "Thread", "Device", …).
    pub name: UnicodeString,
    /// Called when the last reference drops, before the pool free, to
    /// tear down type-specific state (close hardware, unlink lists…).
    pub delete: Option<fn(body: *mut u8)>,
}

/// `OBJECT_HEADER` — lives immediately before every object body.
#[repr(C)]
pub struct ObjectHeader {
    /// Pointer references held on this object. Starts at 1 (creator's).
    pub ref_count: AtomicI64,
    /// The object's type descriptor (static registration).
    pub object_type: &'static ObjectType,
    /// Total allocation size (header + body) for the final free.
    pub total_size: u32,
    _pad: u32,
}

const HEADER_SIZE: usize = core::mem::size_of::<ObjectHeader>();

/// Recover the header from a body pointer (`OBJECT_TO_OBJECT_HEADER`).
#[inline]
unsafe fn body_to_header(body: *mut u8) -> *mut ObjectHeader {
    unsafe { body.sub(HEADER_SIZE) as *mut ObjectHeader }
}

/// `ObCreateObject` — allocate header + body in one pool block and move
/// `body` into it. Returns the *body* pointer, holding the initial
/// reference.
pub fn ob_create_object<T>(
    object_type: &'static ObjectType,
    body: T,
) -> Result<*mut T, NtStatus> {
    debug_assert!(core::mem::align_of::<T>() <= 16);
    let total = HEADER_SIZE + core::mem::size_of::<T>();
    let raw = crate::mm::pool::pool_alloc_checked(total, TAG_OBJECT)?;
    unsafe {
        let hdr = raw as *mut ObjectHeader;
        hdr.write(ObjectHeader {
            ref_count: AtomicI64::new(1),
            object_type,
            total_size: total as u32,
            _pad: 0,
        });
        let body_ptr = raw.add(HEADER_SIZE) as *mut T;
        body_ptr.write(body);
        Ok(body_ptr)
    }
}

/// `ObReferenceObject` — take an additional reference.
///
/// # Safety
/// `body` must be a live pointer returned by [`ob_create_object`].
pub unsafe fn ob_reference_object(body: *mut u8) {
    unsafe {
        let prev = (*body_to_header(body)).ref_count.fetch_add(1, Ordering::Relaxed);
        debug_assert!(prev > 0, "referencing a dead object");
    }
}

/// `ObDereferenceObject` — drop a reference; on the last one, run the
/// type's delete procedure and free the allocation.
///
/// # Safety
/// `body` must be a live pointer; the caller's reference is consumed.
pub unsafe fn ob_dereference_object(body: *mut u8) {
    unsafe {
        let hdr = body_to_header(body);
        // Release ordering so all writes to the object happen-before the
        // destruction observed by whichever thread does the final drop.
        let prev = (*hdr).ref_count.fetch_sub(1, Ordering::Release);
        debug_assert!(prev > 0, "over-dereference");
        if prev == 1 {
            core::sync::atomic::fence(Ordering::Acquire);
            if let Some(delete) = (*hdr).object_type.delete {
                delete(body);
            }
            pool_free(hdr as *mut u8, TAG_OBJECT);
        }
    }
}

/// `ObGetObjectType`-equivalent: check a body against an expected type,
/// the guard `KeWaitForSingleObject`-style APIs use before trusting casts.
///
/// # Safety
/// `body` must be a live object pointer.
pub unsafe fn ob_check_type(body: *mut u8, expected: &'static ObjectType) -> Result<(), NtStatus> {
    unsafe {
        if core::ptr::eq((*body_to_header(body)).object_type, expected) {
            Ok(())
        } else {
            Err(NtStatus::OBJECT_TYPE_MISMATCH)
        }
    }
}

/// Convenience: current reference count (diagnostics/self tests only).
///
/// # Safety
/// `body` must be a live object pointer.
pub unsafe fn ob_ref_count(body: *mut u8) -> i64 {
    unsafe { (*body_to_header(body)).ref_count.load(Ordering::Relaxed) }
}
