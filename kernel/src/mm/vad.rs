//! VADs — per-address-space Virtual Address Descriptors, and demand commit.
//!
//! NT records every user virtual allocation as an `MMVAD` (an AVL node off
//! `EPROCESS`): the page tables say what *is* mapped, the VAD tree says what
//! *may be* mapped, and the page-fault handler (`MmAccessFault`) reconciles
//! the two — a not-present fault inside a committed VAD gets a zeroed page
//! and a retry; anything else is an access violation.
//!
//! This is that, at our scale: a sorted, non-overlapping list of committed
//! ranges per address space (keyed by PML4, since threads carry `cr3` and
//! there is no `EPROCESS` yet). `NtAllocateVirtualMemory` inserts a VAD
//! *without touching a page table*; the first access faults and
//! [`vad_resolve`] backs the page on demand (see `ke::traps`) — zero-filled,
//! or paged back in from the pagefile when [`super::pageout`] evicted it
//! earlier. `NtFree` / `NtProtect` split, shrink, and drop VADs and apply to
//! whatever pages happen to be backed.
//!
//! Locking: one global lock (same shape as the dispatcher's — the list is
//! short and faults are rare). Lock order is **VADS → PFN**: resolve and
//! free allocate/free frames while holding it. All entry points must run at
//! `PASSIVE_LEVEL`; the #PF caller enforces that.
//!
//! Address-space teardown does *not* go through here: `mm_free_user_address_space`
//! walks the page tables and frees every low-half leaf (which covers
//! VAD-backed pages), so [`vad_teardown`] just drops the bookkeeping.

use super::phys::{mm_allocate_page, mm_free_contiguous_pages};
use super::virt;
use super::{PhysAddr, PAGE_SIZE};
use crate::ke::spinlock::SpinLock;
use crate::rtl::NtStatus;
use alloc::vec::Vec;

/// Base of the per-process VirtualAlloc arena: high enough to clear the
/// image (~0x140000000) with room to grow, far below the stack/TEB region
/// (0x0000_7FFF_FFxx_xxxx) the loader carves out.
const USER_VAD_BASE: u64 = 0x0000_7000_0000_0000;
/// Top of the arena — `NtAllocateVirtualMemory` never hands out VAs at or
/// above this, keeping the loader's fixed regions unreachable.
const USER_VAD_LIMIT: u64 = 0x0000_7FFF_0000_0000;

/// One committed virtual range. All VADs are committed — "reserved but not
/// committed" is a future `MEM_RESERVE` split; today every descriptor is
/// backed-on-first-touch.
#[derive(Clone, Copy)]
struct Vad {
    /// First virtual address of the range (page-aligned).
    start: u64,
    /// One past the last virtual address (page-aligned).
    end: u64,
    /// Writable on demand-fault mapping (`PAGE_READWRITE` vs `PAGE_READONLY`).
    writable: bool,
    /// Executable on demand-fault mapping (the `PAGE_EXECUTE*` family).
    executable: bool,
}

/// The VAD list of one address space, sorted by `start`, non-overlapping.
struct SpaceVads {
    pml4: PhysAddr,
    vads: Vec<Vad>,
}

static VADS: SpinLock<Vec<SpaceVads>> = SpinLock::new(Vec::new());

/// Find the entry for `pml4`, creating an empty one when `create` is set.
fn space_mut<'a>(spaces: &'a mut Vec<SpaceVads>, pml4: PhysAddr, create: bool) -> Option<&'a mut SpaceVads> {
    let pos = match spaces.iter().position(|s| s.pml4 == pml4) {
        Some(i) => i,
        None if create => {
            spaces.push(SpaceVads { pml4, vads: Vec::new() });
            spaces.len() - 1
        }
        None => return None,
    };
    Some(&mut spaces[pos])
}

/// `NtAllocateVirtualMemory` (commit): reserve `pages` of user VA space in
/// address space `pml4` and record them committed. No page is mapped — the
/// first touch faults one in via [`vad_resolve`]. Returns the range base,
/// or `None` when the arena is exhausted.
pub fn vad_allocate(pml4: PhysAddr, pages: usize, writable: bool, executable: bool) -> Option<u64> {
    let len = (pages as u64).checked_mul(PAGE_SIZE as u64)?;
    if len == 0 {
        return None;
    }
    let mut spaces = VADS.lock();
    let s = space_mut(&mut spaces, pml4, true)?;
    // First-fit over [USER_VAD_BASE, USER_VAD_LIMIT).
    let mut cursor = USER_VAD_BASE;
    for v in s.vads.iter() {
        if v.start >= cursor && v.start - cursor >= len {
            break; // the gap in front of v fits
        }
        cursor = cursor.max(v.end);
    }
    let end = cursor.checked_add(len)?;
    if end > USER_VAD_LIMIT {
        return None;
    }
    let pos = s.vads.partition_point(|v| v.start < cursor);
    s.vads.insert(pos, Vad { start: cursor, end, writable, executable });
    Some(cursor)
}

/// Is every page of `[va, va+len)` inside committed VADs of address space
/// `pml4`? This is the demand-paging half of `ProbeForRead`/`ProbeForWrite`:
/// a committed page that simply hasn't faulted in yet is a *valid* user
/// buffer (the access will fault and resolve), while anything outside every
/// VAD is the access violation it always was.
pub fn vad_covers(pml4: PhysAddr, va: u64, len: usize) -> bool {
    let Some(end) = va.checked_add(len as u64) else { return false };
    let spaces = VADS.lock();
    let Some(s) = spaces.iter().find(|s| s.pml4 == pml4) else { return false };
    let mut cursor = va;
    for v in s.vads.iter() {
        if v.end <= cursor {
            continue;
        }
        if v.start > cursor {
            return false; // uncovered gap
        }
        cursor = v.end;
        if cursor >= end {
            return true;
        }
    }
    cursor >= end
}

/// The #PF half of demand commit: if `va` lies in a committed VAD of the
/// **current** address space, back its page and report success (the trap
/// returns and the CPU retries the faulting instruction). The backing is a
/// zeroed frame for a never-touched page, or the pagefile content when
/// [`super::pageout`] evicted this page earlier. Anything else — no VAD, out
/// of frames — returns false and the trap treats it as a fault to report.
///
/// Only meaningful for not-present faults at `PASSIVE_LEVEL`; `ke::traps`
/// checks both. A *protection* fault (page present) is never resolvable
/// here, which is exactly how a write to a read-only VAD page becomes an
/// access violation instead of a silent permission upgrade.
pub fn vad_resolve(va: u64) -> bool {
    let page = va & !(PAGE_SIZE as u64 - 1);
    let pml4 = virt::mm_current_address_space();
    let mut spaces = VADS.lock();
    let Some(s) = space_mut(&mut spaces, pml4, false) else { return false };
    let Some(v) = s.vads.iter().find(|v| v.start <= page && page < v.end) else { return false };
    let (writable, executable) = (v.writable, v.executable);
    // Two CPUs can fault on the same page concurrently; the loser of the
    // VADS-lock race finds the page already mapped and is done.
    if virt::mm_debug_pte(page).is_some() {
        return true;
    }
    // Paged out earlier? Read the content back instead of zero-filling.
    if super::pageout::page_in(pml4.0, page, writable, executable) {
        return true;
    }
    let phys = match mm_allocate_page() {
        Some(p) => p, // zeroed — NT's zero-page rule
        None => return false,
    };
    // SAFETY: `pml4` is the live address space; `phys` is ours; the page is
    // VAD-committed user VA space with no current mapping.
    unsafe { virt::mm_map_user_range(pml4, page, phys, 1, writable, executable) };
    super::pageout::register_page(pml4.0, page);
    true
}

/// `NtFreeVirtualMemory` (`MEM_RELEASE`): drop `[base, base+len)` from the
/// VADs of `pml4`, unmapping and freeing whatever pages are currently
/// backed. Ranges may cut into VADs — a surviving head/tail is kept (NT
/// requires the exact allocation base; being permissive is simpler and the
/// CRT heap only ever releases whole chunks anyway).
///
/// Must be called with `pml4` the active address space (the unmap half
/// walks the live tables).
pub fn vad_free(pml4: PhysAddr, base: u64, len: u64) -> Result<(), NtStatus> {
    let Some(end) = base.checked_add(len) else { return Err(NtStatus::INVALID_PARAMETER) };
    // Release the pagefile slots of any paged-out pages in the range —
    // their content dies with the allocation.
    super::pageout::drop_range(pml4.0, base, len);
    let mut spaces = VADS.lock();
    let Some(s) = space_mut(&mut spaces, pml4, false) else { return Err(NtStatus::INVALID_PARAMETER) };
    let mut i = 0;
    while i < s.vads.len() {
        let v = s.vads[i];
        if v.end <= base {
            i += 1;
            continue;
        }
        if v.start >= end {
            break;
        }
        let (lo, hi) = (v.start.max(base), v.end.min(end));
        // Unmap + free the backed pages of the cut.
        let mut page = lo;
        while page < hi {
            // SAFETY: `pml4` is active; `page` is inside this VAD.
            if let Some(pa) = unsafe { virt::mm_unmap_user_page(page) } {
                mm_free_contiguous_pages(pa, 1);
            }
            page += PAGE_SIZE as u64;
        }
        match (v.start < lo, hi < v.end) {
            (true, true) => {
                // Middle cut: shrink this VAD to the head, insert the tail.
                s.vads[i].end = lo;
                s.vads.insert(i + 1, Vad { start: hi, ..v });
                i += 1;
            }
            (true, false) => s.vads[i].end = lo,
            (false, true) => s.vads[i].start = hi,
            (false, false) => {
                s.vads.remove(i);
                continue;
            }
        }
        i += 1;
    }
    Ok(())
}

/// `NtProtectVirtualMemory`: change the protection of `[base, base+len)`,
/// splitting VADs on partial overlaps, and apply the new bits to already
/// backed pages (unbacked ones pick them up at fault time via the VAD).
pub fn vad_protect(pml4: PhysAddr, base: u64, len: u64, writable: bool, executable: bool) -> Result<(), NtStatus> {
    let Some(end) = base.checked_add(len) else { return Err(NtStatus::INVALID_PARAMETER) };
    if end <= base {
        return Err(NtStatus::INVALID_PARAMETER);
    }
    let mut spaces = VADS.lock();
    let Some(s) = space_mut(&mut spaces, pml4, false) else { return Err(NtStatus::INVALID_PARAMETER) };
    // Any overlap at all? NT answers STATUS_NOT_COMMITTED for a range with
    // no committed VAD behind it.
    if !s.vads.iter().any(|v| v.start < end && base < v.end) {
        return Err(NtStatus::INVALID_PARAMETER);
    }
    let mut i = 0;
    while i < s.vads.len() {
        let v = s.vads[i];
        if v.end <= base {
            i += 1;
            continue;
        }
        if v.start >= end {
            break;
        }
        let (lo, hi) = (v.start.max(base), v.end.min(end));
        // Split so [lo, hi) is its own VAD, then set its protection.
        if v.start < lo {
            s.vads[i].end = lo;
            s.vads.insert(i + 1, Vad { start: lo, ..v });
            i += 1;
        }
        if hi < s.vads[i].end {
            let tail = s.vads[i];
            s.vads.insert(i + 1, Vad { start: hi, ..tail });
            s.vads[i].end = hi;
        }
        s.vads[i].writable = writable;
        s.vads[i].executable = executable;
        let mut page = lo;
        while page < hi {
            // SAFETY: `pml4` is active; page inside this VAD.
            unsafe { virt::mm_protect_user_page(page, writable, executable) };
            page += PAGE_SIZE as u64;
        }
        i += 1;
    }
    Ok(())
}

/// Drop the VAD bookkeeping of a dying address space. The pages themselves
/// are reclaimed by `mm_free_user_address_space`'s page-table walk, which
/// also covers the loader's eager (non-VAD) mappings; paged-out pages have
/// no frame to reclaim, so their pagefile slots are released here.
pub fn vad_teardown(pml4: PhysAddr) {
    super::pageout::drop_process(pml4.0);
    let mut spaces = VADS.lock();
    if let Some(i) = spaces.iter().position(|s| s.pml4 == pml4) {
        spaces.remove(i);
    }
}

/// Number of VADs registered for `pml4` — self-test/diagnostic surface.
#[cfg(target_arch = "x86_64")]
pub fn vad_count(pml4: PhysAddr) -> usize {
    let spaces = VADS.lock();
    spaces.iter().find(|s| s.pml4 == pml4).map_or(0, |s| s.vads.len())
}
