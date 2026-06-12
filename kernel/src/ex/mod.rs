//! # Ex — Executive support routines
//!
//! In NT, `Ex` is the grab-bag service layer above Ke: pool allocation,
//! fast mutexes, worker threads, lookaside lists. Right now it hosts the
//! public pool API; the underlying allocator lives in [`crate::mm::pool`]
//! (same split as NT, where `ExAllocatePoolWithTag` fronts Mm's pool
//! manager).

use crate::rtl::NtStatus;

/// `POOL_TYPE` — only the non-paged flavor exists until Mm learns to page.
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PoolType {
    NonPaged = 0,
}

/// `ExAllocatePoolWithTag` — allocate `size` bytes of kernel heap, owned
/// by `tag` (build tags with [`crate::mm::pool::pool_tag`]). Returns null
/// on exhaustion, exactly like the C export.
#[cfg(target_arch = "x86_64")]
pub fn ex_allocate_pool_with_tag(_pool_type: PoolType, size: usize, tag: u32) -> *mut u8 {
    crate::mm::pool::pool_alloc(size, tag)
}

/// `ExFreePoolWithTag`.
#[cfg(target_arch = "x86_64")]
pub fn ex_free_pool_with_tag(ptr: *mut u8, tag: u32) {
    crate::mm::pool::pool_free(ptr, tag)
}

/// Rust-flavored typed allocation: place `value` in pool under `tag`,
/// returning a raw pointer with the value's destructor responsibility
/// transferred to the caller (kernel objects are reference-counted by Ob,
/// not borrow-checked — raw pointers are the honest type here).
#[cfg(target_arch = "x86_64")]
pub fn ex_allocate_object<T>(value: T, tag: u32) -> Result<*mut T, NtStatus> {
    let p = crate::mm::pool::pool_alloc_checked(core::mem::size_of::<T>(), tag)? as *mut T;
    // SAFETY: freshly allocated, sized and 16-aligned (T's alignment must
    // be <= 16, which holds for all kernel objects; checked in debug).
    debug_assert!(core::mem::align_of::<T>() <= 16);
    unsafe { p.write(value) };
    Ok(p)
}
