//! # Mm — the Memory Manager
//!
//! Phase-1 scope of NT's largest subsystem:
//!
//! * [`phys`] — the physical page allocator over a PFN bitmap (our
//!   compact stand-in for the `MMPFN` database).
//! * [`pool`] — the NonPagedPool: tagged variable-size kernel heap,
//!   also wired up as Rust's `#[global_allocator]` so `alloc::` types
//!   (Box/Vec/String) draw from pool exactly like `ExAllocatePoolWithTag`.
//! * [`virt`] — page-table introspection (`MmGetPhysicalAddress`).
//!
//! ## The physical-memory window
//!
//! The bootloader maps **all** physical memory at one virtual offset
//! (`BootInfo::physical_memory_offset`). NT keeps an equivalent direct
//! window for the kernel. Two consequences we lean on:
//!
//! 1. The PFN bitmap and pool blocks need no page-table edits — physical
//!    pages are already addressable at `offset + pa`.
//! 2. Page tables themselves are readable the same way, which is what
//!    makes [`virt::mm_get_physical_address`] a simple walk.
//!
//! Paged pool, VADs, working sets and the fault-driven paths are future
//! work; everything here is the resident, never-paged core that the rest
//! of the kernel boots on.

#[cfg(target_arch = "x86_64")]
pub mod phys;
#[cfg(target_arch = "x86_64")]
pub mod pool;
#[cfg(target_arch = "x86_64")]
pub mod virt;

/// x86_64 small page size. (Large pages are a future optimization.)
pub const PAGE_SIZE: usize = 4096;
pub const PAGE_SHIFT: u64 = 12;

/// Physical address newtype — keeps physical and virtual addresses from
/// crossing accidentally (the classic C kernel bug class).
#[repr(transparent)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct PhysAddr(pub u64);

impl PhysAddr {
    /// Page frame number — the index of the 4 KiB frame.
    pub const fn pfn(self) -> u64 {
        self.0 >> PAGE_SHIFT
    }
    pub const fn from_pfn(pfn: u64) -> Self {
        PhysAddr(pfn << PAGE_SHIFT)
    }
}

#[cfg(target_arch = "x86_64")]
mod offset {
    use super::PhysAddr;
    use core::sync::atomic::{AtomicU64, Ordering};

    /// Virtual base of the all-of-physical-memory window. Set once during
    /// `mm::phys::init` from BootInfo, read everywhere after.
    static PHYS_OFFSET: AtomicU64 = AtomicU64::new(0);

    pub(crate) fn set_phys_offset(offset: u64) {
        PHYS_OFFSET.store(offset, Ordering::Release);
    }

    /// Translate a physical address into the direct window.
    /// The kernel-wide way to touch a physical page.
    pub fn phys_to_virt(pa: PhysAddr) -> *mut u8 {
        let off = PHYS_OFFSET.load(Ordering::Acquire);
        debug_assert!(off != 0, "mm not initialized");
        (off + pa.0) as *mut u8
    }
}

#[cfg(target_arch = "x86_64")]
pub use offset::phys_to_virt;
#[cfg(target_arch = "x86_64")]
pub(crate) use offset::set_phys_offset;
