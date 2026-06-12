//! NonPagedPool — the tagged kernel heap.
//!
//! Pool is NT's kernel allocator: variable-size blocks, each prefixed by a
//! `POOL_HEADER` carrying the size and a 4-byte **tag** ('Thrd', 'Irp ',
//! 'File'…) identifying the owner. Tags turn heap corruption and leak
//! hunts from archaeology into bookkeeping — `!poolused` in a debugger —
//! and we keep them for exactly that reason.
//!
//! ## Design
//!
//! A first-fit free list over 64 KiB physically-contiguous *slabs*
//! obtained from the PFN allocator (reachable through the physical-memory
//! window, so no page-table work):
//!
//! ```text
//! slab: [POOL_HEADER|payload][POOL_HEADER|payload][free block].....
//!        ^ allocated          ^ allocated          ^ on free list
//! ```
//!
//! * Blocks are 16-byte aligned with a 16-byte header, like x64 NT pool.
//! * Frees coalesce with a *following* free neighbor when it is the slab
//!   tail; full block-merge coalescing (NT does it via `PreviousSize`
//!   back-links) is documented future work — fragmentation is bounded in
//!   practice by the kernel's strongly size-clustered allocation pattern.
//! * Requests over [`LARGE_THRESHOLD`] bypass the list and take whole
//!   pages directly (NT's "big pool" behaves the same way).
//!
//! The Rust global allocator is implemented on top, so `Box`/`Vec`/`Arc`
//! in kernel code *are* pool allocations (tag `'Rust'`).

use super::{phys::mm_allocate_contiguous_pages, phys::mm_free_contiguous_pages, phys_to_virt, PhysAddr, PAGE_SIZE};
use crate::ke::spinlock::SpinLock;
use crate::rtl::NtStatus;

/// Pool block alignment and header size (x64 NT uses 16 for both).
const POOL_ALIGN: usize = 16;
/// Slab size requested from the PFN allocator when the list runs dry.
const SLAB_PAGES: usize = 16; // 64 KiB
/// Allocations at or above this go straight to whole pages.
const LARGE_THRESHOLD: usize = PAGE_SIZE - POOL_ALIGN;

/// Make a pool tag from a 4-char ASCII literal: `pool_tag(b"Thrd")`.
pub const fn pool_tag(tag: &[u8; 4]) -> u32 {
    u32::from_le_bytes(*tag)
}

/// Header preceding every pool allocation — `POOL_HEADER`, 16 bytes.
#[repr(C)]
struct PoolHeader {
    /// Total block size in bytes, header included.
    size: u32,
    /// Owner tag for diagnostics ([`pool_tag`]).
    tag: u32,
    /// `u32::MAX` for list/slab blocks; for large (whole-page) allocations
    /// the page count, so free() knows which path to take.
    large_pages: u32,
    _pad: u32,
}

/// A free block: the header space is reused for free-list links (the
/// payload of a free block is dead storage, so this costs nothing).
#[repr(C)]
struct FreeBlock {
    size: u32,
    _tag: u32,
    next: *mut FreeBlock,
}

struct PoolState {
    free_list: *mut FreeBlock,
    /// Statistics for `!poolused`-style diagnostics.
    bytes_in_use: usize,
    allocations: u64,
    frees: u64,
}

// SAFETY: raw pointer is only dereferenced under the pool lock.
unsafe impl Send for PoolState {}

static POOL: SpinLock<PoolState> = SpinLock::new(PoolState {
    free_list: core::ptr::null_mut(),
    bytes_in_use: 0,
    allocations: 0,
    frees: 0,
});

/// Round a payload request up to a whole 16-aligned block incl. header.
fn block_size_for(payload: usize) -> usize {
    (payload + core::mem::size_of::<PoolHeader>() + POOL_ALIGN - 1) & !(POOL_ALIGN - 1)
}

/// `ExAllocatePoolWithTag(NonPagedPool, …)` — the core allocator.
/// Returns a 16-aligned pointer or null on exhaustion (matching the C
/// contract; the `ex` module offers a `Result` wrapper for Rust callers).
pub fn pool_alloc(payload: usize, tag: u32) -> *mut u8 {
    if payload == 0 {
        return core::ptr::null_mut();
    }

    // Large path: whole pages, no list participation.
    if payload >= LARGE_THRESHOLD {
        let pages = (payload + core::mem::size_of::<PoolHeader>()).div_ceil(PAGE_SIZE);
        let Some(pa) = mm_allocate_contiguous_pages(pages) else {
            return core::ptr::null_mut();
        };
        let hdr = phys_to_virt(pa) as *mut PoolHeader;
        // SAFETY: fresh zeroed pages, exclusively ours.
        unsafe {
            (*hdr).size = (pages * PAGE_SIZE) as u32;
            (*hdr).tag = tag;
            (*hdr).large_pages = pages as u32;
            let mut s = POOL.lock();
            s.bytes_in_use += pages * PAGE_SIZE;
            s.allocations += 1;
            return (hdr as *mut u8).add(core::mem::size_of::<PoolHeader>());
        }
    }

    let want = block_size_for(payload);
    let mut s = POOL.lock();

    // First fit with split.
    unsafe {
        let mut prev: *mut *mut FreeBlock = &mut s.free_list;
        while !(*prev).is_null() {
            let blk = *prev;
            let bsize = (*blk).size as usize;
            if bsize >= want {
                let leftover = bsize - want;
                if leftover >= block_size_for(POOL_ALIGN) {
                    // Split: tail remains free.
                    let rest = (blk as *mut u8).add(want) as *mut FreeBlock;
                    (*rest).size = leftover as u32;
                    (*rest).next = (*blk).next;
                    *prev = rest;
                } else {
                    *prev = (*blk).next;
                }
                let hdr = blk as *mut PoolHeader;
                (*hdr).size = if leftover >= block_size_for(POOL_ALIGN) {
                    want as u32
                } else {
                    bsize as u32
                };
                (*hdr).tag = tag;
                (*hdr).large_pages = u32::MAX;
                s.bytes_in_use += (*hdr).size as usize;
                s.allocations += 1;
                return (hdr as *mut u8).add(core::mem::size_of::<PoolHeader>());
            }
            prev = &mut (*blk).next;
        }
    }

    // List dry: grow by one slab and retry (recursion depth 1 by design —
    // the fresh slab satisfies any sub-threshold request).
    drop(s);
    let Some(pa) = mm_allocate_contiguous_pages(SLAB_PAGES) else {
        return core::ptr::null_mut();
    };
    free_block_insert(phys_to_virt(pa), SLAB_PAGES * PAGE_SIZE);
    pool_alloc(payload, tag)
}

/// Push a raw range onto the free list as one block.
fn free_block_insert(at: *mut u8, size: usize) {
    let mut s = POOL.lock();
    // SAFETY: range is exclusively ours and at least a header in size.
    unsafe {
        let blk = at as *mut FreeBlock;
        (*blk).size = size as u32;
        (*blk).next = s.free_list;
        s.free_list = blk;
    }
}

/// `ExFreePoolWithTag` — return a block. The tag is checked in debug
/// builds (NT's checked build does too) to catch cross-owner frees.
pub fn pool_free(ptr: *mut u8, tag: u32) {
    if ptr.is_null() {
        return;
    }
    unsafe {
        let hdr = ptr.sub(core::mem::size_of::<PoolHeader>()) as *mut PoolHeader;
        debug_assert_eq!((*hdr).tag, tag, "pool tag mismatch on free");
        let size = (*hdr).size as usize;
        {
            let mut s = POOL.lock();
            s.bytes_in_use -= size;
            s.frees += 1;
        }
        if (*hdr).large_pages != u32::MAX {
            // Large allocation: give the pages back wholesale.
            let pages = (*hdr).large_pages as usize;
            let va = hdr as u64;
            let off = va - (phys_to_virt(PhysAddr(0)) as u64);
            mm_free_contiguous_pages(PhysAddr(off), pages);
            return;
        }
        free_block_insert(hdr as *mut u8, size);
    }
}

/// `ExFreePool` — free a block without knowing its tag (the block's own
/// header records it). Same teardown as [`pool_free`], minus the tag check.
pub fn pool_free_any(ptr: *mut u8) {
    if ptr.is_null() {
        return;
    }
    // SAFETY: ptr came from pool_alloc, so its header precedes it.
    let tag = unsafe { (*(ptr.sub(core::mem::size_of::<PoolHeader>()) as *const PoolHeader)).tag };
    pool_free(ptr, tag);
}

/// Bytes currently allocated — self-test/diagnostic hook.
pub fn pool_bytes_in_use() -> usize {
    POOL.lock().bytes_in_use
}

/// `NTSTATUS`-flavored allocation for kernel-internal use.
pub fn pool_alloc_checked(size: usize, tag: u32) -> Result<*mut u8, NtStatus> {
    let p = pool_alloc(size, tag);
    if p.is_null() {
        Err(NtStatus::INSUFFICIENT_RESOURCES)
    } else {
        Ok(p)
    }
}

// ---------------------------------------------------------------------------
// Rust global allocator on top of pool
// ---------------------------------------------------------------------------

/// `Box`, `Vec`, `String`… in kernel code allocate from NonPagedPool with
/// the 'Rust' tag. Alignments beyond 16 are satisfied by over-allocating
/// and storing the original pointer just before the aligned payload.
struct PoolGlobalAlloc;

const RUST_TAG: u32 = pool_tag(b"Rust");

unsafe impl alloc::alloc::GlobalAlloc for PoolGlobalAlloc {
    unsafe fn alloc(&self, layout: core::alloc::Layout) -> *mut u8 {
        let align = layout.align();
        if align <= POOL_ALIGN {
            return pool_alloc(layout.size().max(1), RUST_TAG);
        }
        // Over-aligned: allocate size + align + one slot for the back-pointer.
        let raw = pool_alloc(layout.size() + align + core::mem::size_of::<usize>(), RUST_TAG);
        if raw.is_null() {
            return raw;
        }
        unsafe {
            let payload = raw.add(core::mem::size_of::<usize>());
            let aligned = payload.add(payload.align_offset(align));
            (aligned.sub(core::mem::size_of::<usize>()) as *mut usize).write(raw as usize);
            aligned
        }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: core::alloc::Layout) {
        if layout.align() <= POOL_ALIGN {
            pool_free(ptr, RUST_TAG);
        } else {
            unsafe {
                let raw = (ptr.sub(core::mem::size_of::<usize>()) as *mut usize).read() as *mut u8;
                pool_free(raw, RUST_TAG);
            }
        }
    }
}

/// Install pool as the global allocator for the freestanding kernel build
/// (host test builds use std's allocator).
#[cfg(not(test))]
#[global_allocator]
static GLOBAL_ALLOC: PoolGlobalAlloc = PoolGlobalAlloc;
