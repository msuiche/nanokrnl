//! Page-out / page-in — the modified-page writer and its fault path.
//!
//! The memory manager keeps a **working-set registry**: every
//! demand-committed user page is queued (process, VA) oldest-first. When
//! the physical allocator runs dry — or the self-test forces it — the
//! engine evicts the oldest backed pages: their content goes to a slot in
//! the pagefile region of the block device, their PTE is cleared (the VAD
//! stays committed), and the frame is freed. The next touch of that VA
//! faults in the page-fault handler, which finds the `(process, VA) → slot`
//! record and pages the content back in instead of zero-filling — real
//! demand paging, in both directions.
//!
//! The pagefile is a dedicated raw region of the scratch disk (sectors
//! `base..base + slots * 8`), a deliberate simplification over a
//! filesystem-backed `pagefile.sys`: paging must never recurse into the
//! filesystem it might be paging out.
//!
//! Locking: registry/map/slot-map each have their own lock; block I/O
//! happens under the BLK lock; page-table edits go through the physical
//! window with no lock. Order: VADS → (registry/map) → BLK; PFN lock is
//! leaf-most.

use super::phys::{mm_allocate_page, mm_free_contiguous_pages};
use super::virt;
use super::PhysAddr;
use crate::io::virtblk;
use crate::ke::spinlock::SpinLock;
use alloc::collections::VecDeque;
use alloc::vec::Vec;

const SECTORS_PER_PAGE: u32 = 8;

/// Pagefile geometry (set by [`init`]).
static GEOM: SpinLock<(u64, u32)> = SpinLock::new((0, 0));
/// Total pagefile slots.
fn slot_count() -> u32 {
    GEOM.lock().1
}
fn base_sector() -> u64 {
    GEOM.lock().0
}

/// Slot allocation bitmap.
static SLOT_MAP: SpinLock<Vec<u64>> = SpinLock::new(Vec::new());
/// Rolling allocation hint (last freed/allocated slot).
static SLOT_HINT: SpinLock<u32> = SpinLock::new(0);

fn slot_alloc() -> Option<u32> {
    let n = slot_count();
    if n == 0 {
        return None;
    }
    let mut map = SLOT_MAP.lock();
    let mut hint = SLOT_HINT.lock();
    for i in 0..n {
        let s = (*hint + i) % n;
        if map[(s / 64) as usize] & (1 << (s % 64)) == 0 {
            map[(s / 64) as usize] |= 1 << (s % 64);
            *hint = s + 1;
            return Some(s);
        }
    }
    None
}

fn slot_free(slot: u32) {
    SLOT_MAP.lock()[(slot / 64) as usize] &= !(1 << (slot % 64));
    let mut hint = SLOT_HINT.lock();
    *hint = (*hint).min(slot);
}

/// Write one 4 KiB page (physical `pa`) to a pagefile slot. Sector-at-a-time
/// straight from the physical window — no bounce buffer.
fn write_page(slot: u32, pa: PhysAddr) -> bool {
    let va = super::phys_to_virt(pa) as u64;
    for i in 0..SECTORS_PER_PAGE {
        let src = (va + (i as u64) * 512) as *const [u8; 512];
        if !virtblk::write_sector(
            base_sector() + slot as u64 * SECTORS_PER_PAGE as u64 + i as u64,
            unsafe { &*src },
        ) {
            return false;
        }
    }
    true
}

/// Read one 4 KiB pagefile slot back into physical `pa`.
fn read_page(slot: u32, pa: PhysAddr) -> bool {
    let va = super::phys_to_virt(pa) as u64;
    for i in 0..SECTORS_PER_PAGE {
        let dst = (va + (i as u64) * 512) as *mut [u8; 512];
        if !virtblk::read_sector(
            base_sector() + slot as u64 * SECTORS_PER_PAGE as u64 + i as u64,
            unsafe { &mut *dst },
        ) {
            return false;
        }
    }
    true
}

// ---------------------------------------------------------------------------
// The working-set registry and the paged-out map
// ---------------------------------------------------------------------------

/// Registry capacity; on overflow the oldest entry is evicted immediately
/// (which is exactly what the engine is for, so nothing is ever dropped).
const REG_CAP: usize = 8192;
/// Paged-out record capacity (bounded so eviction never allocates memory
/// under memory pressure — the reserve is done once, up front).
const MAP_CAP: usize = 2048;

/// Working-set FIFO: (process CR3, page VA), pushed on every
/// demand-commit and page-in.
static REGISTRY: SpinLock<VecDeque<(u64, u64)>> = SpinLock::new(VecDeque::new());
/// `(process CR3, page VA) -> pagefile slot`.
static PAGED_OUT: SpinLock<Vec<(u64, u64, u32)>> = SpinLock::new(Vec::new());

/// The last slot written (self-test surface).
static LAST_SLOT: SpinLock<Option<u32>> = SpinLock::new(None);

/// Initialize the pagefile region: `base` = first sector, `slots` pages.
/// Reserves the slot bitmap and the record map's capacity.
pub fn init(base: u64, slots: u32) {
    *GEOM.lock() = (base, slots);
    *SLOT_MAP.lock() = alloc::vec![0u64; (slots as usize).div_ceil(64)];
    PAGED_OUT.lock().reserve(MAP_CAP);
    crate::kd_println!("MM: pagefile online — {} pages at sector {}", slots, base);
}

/// Whether the pagefile is initialized.
pub fn online() -> bool {
    slot_count() != 0
}

/// The slot the last eviction wrote (self-test).
pub fn last_evicted_slot() -> Option<u32> {
    *LAST_SLOT.lock()
}

/// Record a demand-committed page on the working set. On registry overflow
/// the oldest entry is evicted immediately — the registry never drops a
/// page without paying for it.
pub fn register_page(cr3: u64, va: u64) {
    let mut reg = REGISTRY.lock();
    if reg.len() < REG_CAP {
        reg.push_back((cr3, va));
        return;
    }
    // Overflow: evict the oldest entry, then re-acquire to push. (The lock
    // is not reentrant — never lock twice while `reg` is alive.)
    let Some(victim) = reg.pop_front() else { return };
    drop(reg);
    evict(victim.0, victim.1);
    REGISTRY.lock().push_back((cr3, va));
}

/// Drop all records and registry entries of a dying address space (called
/// from `vad_teardown`, before the page tables go away).
pub fn drop_process(cr3: u64) {
    REGISTRY.lock().retain(|&(c, _)| c != cr3);
    let mut map = PAGED_OUT.lock();
    let mut i = 0;
    while i < map.len() {
        if map[i].0 == cr3 {
            let (_, _, slot) = map.remove(i);
            slot_free(slot);
        } else {
            i += 1;
        }
    }
}

/// Drop records overlapping `[base, base+len)` of `cr3` (a VAD free): their
/// slots are released; their content is discarded, as freeing implies.
pub fn drop_range(cr3: u64, base: u64, len: u64) {
    let end = base.saturating_add(len);
    let mut map = PAGED_OUT.lock();
    let mut i = 0;
    while i < map.len() {
        let (c, va, _) = map[i];
        if c == cr3 && va >= base && va < end {
            let (_, _, slot) = map.remove(i);
            slot_free(slot);
        } else {
            i += 1;
        }
    }
}

/// If `(cr3, va)` is paged out, take its slot off the map (the caller now
/// owns it — read it back and free it). Returns the slot.
pub fn paged_out_take(cr3: u64, va: u64) -> Option<u32> {
    let mut map = PAGED_OUT.lock();
    let i = map.iter().position(|&(c, v, _)| c == cr3 && v == va)?;
    let (_, _, slot) = map.remove(i);
    Some(slot)
}

/// Evict one working-set page `(cr3, va)` to the pagefile. Returns 1 on a
/// real eviction, 0 when the entry was stale (already unmapped/freed) or no
/// slot was available.
fn evict(cr3: u64, va: u64) -> u32 {
    let pml4 = PhysAddr(cr3);
    let Some(pte) = virt::mm_debug_pte_in(pml4, va) else {
        return 0; // stale: freed/unmapped since registration
    };
    if pte & 1 == 0 {
        return 0; // not present (already paged out or freed)
    }
    let pa = PhysAddr(pte & 0x000F_FFFF_FFFF_F000);
    let Some(slot) = slot_alloc() else {
        return 0; // pagefile full: leave the page resident
    };
    if !write_page(slot, pa) {
        slot_free(slot);
        return 0;
    }
    // Unmap in the target address space, record the slot, free the frame.
    let unmapped = unsafe { virt::mm_unmap_user_page_in(pml4, va) };
    if unmapped != Some(pa) {
        // Lost the race with a free/unmap: the frame is gone from the table;
        // the slot content is valid but ownerless — discard the record.
        slot_free(slot);
        return 0;
    }
    let mut map = PAGED_OUT.lock();
    if map.len() >= MAP_CAP {
        // Records full: put the page back rather than lose the mapping
        // semantics. (The map is sized generously; hitting this is a bug.)
        slot_free(slot);
        return 0;
    }
    map.push((cr3, va, slot));
    *LAST_SLOT.lock() = Some(slot);
    mm_free_contiguous_pages(pa, 1);
    1
}

/// `evict_some(n)`: free up to `n` frames by evicting the oldest
/// working-set pages. Returns how many frames were actually freed.
pub fn evict_some(n: usize) -> usize {
    let mut freed = 0;
    while freed < n {
        let victim = {
            let mut reg = REGISTRY.lock();
            reg.pop_front()
        };
        let Some((cr3, va)) = victim else { break };
        freed += evict(cr3, va) as usize;
    }
    freed
}

/// Force one eviction of `(cr3, va)` — the self-test's deterministic hook.
pub fn evict_for_test(cr3: u64, va: u64) -> u32 {
    evict(cr3, va)
}

/// Number of currently paged-out pages (diagnostics/self-test).
pub fn paged_out_count() -> usize {
    PAGED_OUT.lock().len()
}

// ---------------------------------------------------------------------------
// The fault path
// ---------------------------------------------------------------------------

/// The #PF half of page-in: if `(cr3, va)` is paged out, read it back into
/// a fresh frame, map it with the VAD's protection, and return true. The
/// VAD itself handles the never-touched (zero-fill) case.
pub fn page_in(cr3: u64, va: u64, writable: bool, executable: bool) -> bool {
    let Some(slot) = paged_out_take(cr3, va) else { return false };
    let Some(pa) = mm_allocate_page() else {
        // No frame for the page-in itself: put the record back.
        PAGED_OUT.lock().push((cr3, va, slot));
        return false;
    };
    let ok = read_page(slot, pa);
    if !ok {
        PAGED_OUT.lock().push((cr3, va, slot));
        mm_free_contiguous_pages(pa, 1);
        return false;
    }
    slot_free(slot);
    unsafe { virt::mm_map_user_range(PhysAddr(cr3), va, pa, 1, writable, executable) };
    register_page(cr3, va);
    true
}
