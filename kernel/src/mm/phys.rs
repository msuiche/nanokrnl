//! Physical page allocator — the PFN database, bitmap edition.
//!
//! NT tracks every physical page with an `MMPFN` record (~48 bytes each)
//! holding state, share counts, and working-set linkage. Until we page,
//! one bit per frame ("free"/"in use") carries the same allocator
//! decisions at 1/384th the memory; the structure here is deliberately
//! shaped so the bitmap can later be *replaced* by a real MMPFN array
//! without touching callers (they only see allocate/free of frames).
//!
//! ## Bootstrapping trick
//!
//! The bitmap needs storage proportional to RAM, but the allocator that
//! would provide storage *is* this allocator. Solution (same one NT's
//! loader uses for the PFN database): carve the bitmap out of the largest
//! usable region reported by the boot memory map, then mark that carve-out
//! as in-use in the bitmap it now backs.

use super::{set_phys_offset, PhysAddr, PAGE_SHIFT, PAGE_SIZE};
use crate::kd_println;
use crate::ke::spinlock::SpinLock;
use crate::rtl::bitmap::RtlBitmap;
use bootloader_api::info::{MemoryRegionKind, MemoryRegions};

/// Allocator state behind one spinlock: the bitmap plus an allocation
/// cursor (`hint`) that turns sequential allocations into forward scans.
struct PfnDatabase {
    bitmap: Option<RtlBitmap<'static>>,
    hint: usize,
    /// Statistics, NT-style (`MmNumberOfPhysicalPages` etc.).
    total_pages: u64,
    free_pages: u64,
}

static PFN_DB: SpinLock<PfnDatabase> = SpinLock::new(PfnDatabase {
    bitmap: None,
    hint: 0,
    total_pages: 0,
    free_pages: 0,
});

/// Bring the physical allocator online from the boot memory map.
/// Phase 0, single-threaded.
pub fn init(regions: &MemoryRegions, phys_offset: u64) {
    set_phys_offset(phys_offset);

    // Pass 1: find the highest usable frame (bitmap size) and the largest
    // usable region (bitmap home).
    let mut max_pfn = 0u64;
    let mut largest: Option<(u64, u64)> = None; // (start, end)
    for r in regions.iter() {
        if r.kind == MemoryRegionKind::Usable {
            max_pfn = max_pfn.max(r.end >> PAGE_SHIFT);
            if largest.map_or(0, |(s, e)| e - s) < r.end - r.start {
                largest = Some((r.start, r.end));
            }
        }
    }
    let (home_start, home_end) = largest.expect("no usable physical memory");

    // Carve the bitmap words from the start of the largest region.
    let bits = max_pfn as usize;
    let words = bits.div_ceil(64);
    let carve_bytes = (words * 8 + PAGE_SIZE - 1) & !(PAGE_SIZE - 1);
    assert!(home_start + carve_bytes as u64 <= home_end, "bitmap larger than region");
    // SAFETY: the region is RAM, unused, and addressable via the window.
    let storage: &'static mut [u64] = unsafe {
        let va = super::phys_to_virt(PhysAddr(home_start)) as *mut u64;
        core::ptr::write_bytes(va as *mut u8, 0, carve_bytes);
        core::slice::from_raw_parts_mut(va, words)
    };

    // Build the map: everything starts in-use, then usable regions are
    // cleared, then the carve-out is re-reserved.
    let mut bm = RtlBitmap::new(storage, bits);
    bm.set_bits(0, bits);
    let mut total = 0u64;
    for r in regions.iter() {
        if r.kind == MemoryRegionKind::Usable {
            let first = (r.start >> PAGE_SHIFT) as usize;
            let count = ((r.end - r.start) >> PAGE_SHIFT) as usize;
            bm.clear_bits(first, count);
            total += count as u64;
        }
    }
    let carve_pages = (carve_bytes >> PAGE_SHIFT as usize) as usize;
    bm.set_bits((home_start >> PAGE_SHIFT) as usize, carve_pages);
    // Never hand out frame 0: too many things treat PFN 0 / address 0 as
    // a null sentinel (NT reserves low memory similarly).
    if !bm.test_bit(0) {
        bm.set_bits(0, 1);
        total -= 1;
    }

    let free = total - carve_pages as u64;
    let mut db = PFN_DB.lock();
    db.bitmap = Some(bm);
    db.total_pages = total;
    db.free_pages = free;
    drop(db);

    kd_println!(
        "MM: PFN bitmap @ {:#X} ({} KiB) — {} MiB usable RAM",
        home_start,
        carve_bytes / 1024,
        free * 4 / 1024
    );
}

/// Allocate `count` physically *contiguous* frames. Returns the first
/// frame's physical address, zero-filled (kernel allocations must never
/// leak prior contents — NT's zero-page thread exists for this; we zero
/// inline until we have one).
pub fn mm_allocate_contiguous_pages(count: usize) -> Option<PhysAddr> {
    let mut db = PFN_DB.lock();
    let hint = db.hint;
    let bm = db.bitmap.as_mut()?;
    let first = bm.find_clear_bits_and_set(count, hint)?;
    db.hint = first + count;
    db.free_pages -= count as u64;
    drop(db);

    let pa = PhysAddr::from_pfn(first as u64);
    // SAFETY: frames are ours now and reachable through the window.
    unsafe {
        core::ptr::write_bytes(super::phys_to_virt(pa), 0, count * PAGE_SIZE);
    }
    Some(pa)
}

/// Allocate a single zeroed frame — `MiRemoveAnyPage`, more or less.
pub fn mm_allocate_page() -> Option<PhysAddr> {
    mm_allocate_contiguous_pages(1)
}

/// Return frames to the free pool. Double-free is a bugcheck-grade error
/// caught by the debug assertion (PFN_LIST_CORRUPT in spirit).
pub fn mm_free_contiguous_pages(pa: PhysAddr, count: usize) {
    let mut db = PFN_DB.lock();
    if let Some(bm) = db.bitmap.as_mut() {
        let first = pa.pfn() as usize;
        debug_assert!(
            (first..first + count).all(|i| bm.test_bit(i)),
            "freeing a page that is not allocated"
        );
        bm.clear_bits(first, count);
        // Pull the hint back so freed space is found again promptly.
        db.hint = db.hint.min(first);
        db.free_pages += count as u64;
    }
}

/// `MmGetNumberOfFreePages` — for diagnostics and the self tests.
pub fn mm_free_page_count() -> u64 {
    PFN_DB.lock().free_pages
}
