//! `mm::pool` — the kernel's pool API, backed by a static arena in WASM linear
//! memory. The x86 kernel's pool sits on real RAM behind page tables; here that
//! region of linear memory *is* "physical memory" (no MMU). Same function
//! surface (`pool_tag`, `pool_alloc`, `pool_alloc_checked`, `pool_free`) so the
//! real `ob` object manager allocates its headers/bodies through this unchanged.
//!
//! Phase 1 is a bump allocator with no reclaim — `pool_free` is a no-op. That is
//! enough to exercise the object manager's logic (ref counting, delete
//! procedures); a real free list comes with a later phase.
use crate::rtl::NtStatus;
use core::sync::atomic::{AtomicUsize, Ordering};

const ARENA_SIZE: usize = 1 << 20; // 1 MiB of "physical memory"
static mut ARENA: [u8; ARENA_SIZE] = [0; ARENA_SIZE];
static BUMP: AtomicUsize = AtomicUsize::new(0);

/// Make a pool tag from a 4-char ASCII literal: `pool_tag(b"Obje")`.
pub const fn pool_tag(tag: &[u8; 4]) -> u32 {
    u32::from_le_bytes(*tag)
}

/// Allocate `payload` 16-byte-aligned bytes; null on exhaustion.
pub fn pool_alloc(payload: usize, _tag: u32) -> *mut u8 {
    let aligned = (payload + 15) & !15;
    let off = BUMP.fetch_add(aligned, Ordering::Relaxed);
    if off + aligned > ARENA_SIZE {
        return core::ptr::null_mut();
    }
    unsafe { (&raw mut ARENA as *mut u8).add(off) }
}

/// As [`pool_alloc`] but reports exhaustion as `STATUS_INSUFFICIENT_RESOURCES`.
pub fn pool_alloc_checked(size: usize, tag: u32) -> Result<*mut u8, NtStatus> {
    let p = pool_alloc(size, tag);
    if p.is_null() {
        Err(NtStatus::INSUFFICIENT_RESOURCES)
    } else {
        Ok(p)
    }
}

/// Free a block — a no-op for the phase-1 bump arena (no reclaim yet).
pub fn pool_free(_ptr: *mut u8, _tag: u32) {}

/// Bytes of arena handed out (diagnostics / self tests).
pub fn pool_used() -> usize {
    BUMP.load(Ordering::Relaxed)
}
