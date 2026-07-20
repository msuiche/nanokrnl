//! Virtual address translation — reading the page tables.
//!
//! x86_64 4-level translation (PML4 → PDPT → PD → PT), each level 512
//! 8-byte entries indexed by 9 bits of the virtual address:
//!
//! ```text
//! 47        39 38       30 29       21 20       12 11          0
//! +-----------+-----------+-----------+-----------+-------------+
//! |   PML4    |   PDPT    |    PD     |    PT     | page offset |
//! +-----------+-----------+-----------+-----------+-------------+
//! ```
//!
//! Tables live in physical memory; we read them through the
//! physical-memory window (`phys_to_virt`), which is what lets this be a
//! plain loop instead of recursive-mapping gymnastics.
//!
//! Write-side mapping (`MmMapIoSpace` proper, allocating intermediate
//! tables) is deliberately deferred until something needs a mapping the
//! bootloader didn't provide; the read-side walker below already covers
//! diagnostics and `MmGetPhysicalAddress`.

use super::{phys::mm_allocate_page, phys::mm_free_contiguous_pages, phys_to_virt, PhysAddr};
use core::arch::asm;

const ENTRY_PRESENT: u64 = 1 << 0;
const ENTRY_RW: u64 = 1 << 1; // writable
const ENTRY_USER: u64 = 1 << 2; // U/S: 1 = user-accessible
const ENTRY_LARGE: u64 = 1 << 7; // PS bit: 1 GiB (PDPT) / 2 MiB (PD) page
const ENTRY_NX: u64 = 1 << 63; // No-eXecute (enforced only when EFER.NXE=1)
const ADDR_MASK: u64 = 0x000F_FFFF_FFFF_F000;
/// Flag bits to carry across a large-page split (everything but the address).
const FLAG_MASK: u64 = !ADDR_MASK;

/// Read CR3 — physical base of the current PML4.
fn current_pml4() -> PhysAddr {
    let cr3: u64;
    unsafe { asm!("mov {}, cr3", out(reg) cr3, options(nomem, nostack, preserves_flags)) };
    PhysAddr(cr3 & ADDR_MASK)
}

/// The kernel's own PML4 (the boot address space), saved once in phase 0.
/// Every per-process address space clones this one's high half.
static KERNEL_PML4: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// Whether SMAP is enabled. When set, the kernel must bracket every access to
/// a user (U/S) page with [`user_access_begin`]/[`user_access_end`].
static SMAP_ON: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// Record that SMAP is active (called after CR4.SMAP is set in phase 0).
pub fn mm_set_smap(on: bool) {
    SMAP_ON.store(on, core::sync::atomic::Ordering::Release);
}

/// Permit kernel access to user pages (set RFLAGS.AC via `stac`). Pair with
/// [`user_access_end`]. A no-op when SMAP is off. Keep the bracketed region
/// tiny — an interrupt in between runs with AC set.
#[inline]
pub fn user_access_begin() {
    if SMAP_ON.load(core::sync::atomic::Ordering::Relaxed) {
        // `stac` modifies RFLAGS.AC, so flags are NOT preserved.
        unsafe { asm!("stac", options(nomem, nostack)) };
    }
}

/// End a [`user_access_begin`] region (clear RFLAGS.AC via `clac`).
#[inline]
pub fn user_access_end() {
    if SMAP_ON.load(core::sync::atomic::Ordering::Relaxed) {
        unsafe { asm!("clac", options(nomem, nostack)) };
    }
}

/// NT's nominal user/kernel boundary on x64 (`MM_USER_PROBE_ADDRESS`). Real
/// Windows confines all user memory below this and probing is just a bounds
/// check. This kernel does not honour that invariant — it maps some
/// user-accessible memory (the shared `kernel32`/`ntdll` stubs, window-backed
/// `NtAllocateVirtualMemory` ranges) in the high half — so the probe instead
/// inspects each page's actual U/S bit (see [`probe_user_buffer`]). The
/// constant is kept for reference and tests.
pub const MM_USER_PROBE_ADDRESS: u64 = 0x0000_7FFF_FFFF_0000;

/// Walk the current address space and report whether `va`'s page is **present
/// and user-accessible** — the U/S bit must be set at *every* level (x86
/// ANDs U/S down the hierarchy). Large pages short-circuit at their level.
fn page_present_and_user(va: u64) -> bool {
    let idx = |shift: u64| ((va >> shift) & 0x1FF) as usize;
    let ok = |e: u64| e & ENTRY_PRESENT != 0 && e & ENTRY_USER != 0;

    let pml4e = entry(current_pml4(), idx(39));
    if !ok(pml4e) {
        return false;
    }
    let pdpte = entry(PhysAddr(pml4e & ADDR_MASK), idx(30));
    if !ok(pdpte) {
        return false;
    }
    if pdpte & ENTRY_LARGE != 0 {
        return true; // 1 GiB page
    }
    let pde = entry(PhysAddr(pdpte & ADDR_MASK), idx(21));
    if !ok(pde) {
        return false;
    }
    if pde & ENTRY_LARGE != 0 {
        return true; // 2 MiB page
    }
    let pte = entry(PhysAddr(pde & ADDR_MASK), idx(12));
    ok(pte)
}

/// Validate that `[address, address + length)` is a well-formed user-mode
/// buffer aligned to `alignment` — the kernel's first line of defence against
/// a ring-3 caller handing a syscall a bogus or kernel pointer (the
/// confused-deputy case). Mirrors NT's `ProbeForRead` contract — returning
/// `STATUS_DATATYPE_MISALIGNMENT` / `STATUS_ACCESS_VIOLATION` instead of
/// faulting, with a zero `length` a no-op — but enforces it by checking every
/// spanned page is present and **user-accessible** (U/S set) rather than by a
/// fixed address boundary, because this kernel maps user memory in both
/// halves. A kernel/supervisor page (U/S clear) or an unmapped page is
/// therefore rejected.
pub fn probe_user_buffer(
    address: u64,
    length: usize,
    alignment: u64,
) -> Result<(), crate::rtl::NtStatus> {
    if length == 0 {
        return Ok(());
    }
    // Alignment must be a power of two; the address must satisfy it.
    debug_assert!(alignment.is_power_of_two());
    if address & (alignment - 1) != 0 {
        return Err(crate::rtl::NtStatus::DATATYPE_MISALIGNMENT);
    }
    // No wraparound past the top of the address space.
    let end = address
        .checked_add(length as u64)
        .ok_or(crate::rtl::NtStatus::ACCESS_VIOLATION)?;
    // Every page the range touches must be present and user-accessible — or,
    // failing that, covered by a committed VAD: a committed page that simply
    // hasn't been touched yet is a valid buffer whose first access faults in
    // (demand commit, `mm::vad`). Outside every VAD it's the access
    // violation it always was.
    let mut page = address & !0xFFF;
    let mut all_present = true;
    while page < end {
        if !page_present_and_user(page) {
            all_present = false;
            break;
        }
        page += 0x1000;
    }
    if !all_present && !crate::mm::vad::vad_covers(current_pml4(), address, length) {
        return Err(crate::rtl::NtStatus::ACCESS_VIOLATION);
    }
    Ok(())
}

/// `ProbeForRead(Address, Length, Alignment)` semantics. See
/// [`probe_user_buffer`].
#[inline]
pub fn probe_for_read(address: u64, length: usize, alignment: u64) -> Result<(), crate::rtl::NtStatus> {
    probe_user_buffer(address, length, alignment)
}

/// `ProbeForWrite(Address, Length, Alignment)` semantics. We validate range
/// and alignment exactly as for reads; NT additionally touches each page to
/// fault in / test writability, which our demand-fault path does not yet need
/// (documented).
#[inline]
pub fn probe_for_write(address: u64, length: usize, alignment: u64) -> Result<(), crate::rtl::NtStatus> {
    probe_user_buffer(address, length, alignment)
}

/// Record the kernel address space. Phase-0, while CR3 is the boot PML4.
pub fn mm_save_kernel_address_space() {
    KERNEL_PML4.store(current_pml4().0, core::sync::atomic::Ordering::Release);
}

/// The kernel address space (PML4 physical base).
pub fn mm_kernel_address_space() -> PhysAddr {
    PhysAddr(KERNEL_PML4.load(core::sync::atomic::Ordering::Acquire))
}

/// Read the current address space (CR3).
pub fn mm_current_address_space() -> PhysAddr {
    current_pml4()
}

/// Switch the active address space (load CR3). The kernel half is shared by
/// every address space, so the kernel code/stack executing this remain
/// mapped across the switch.
///
/// # Safety
/// `pml4` must be a valid address space whose high half is the kernel's.
pub unsafe fn mm_switch_address_space(pml4: PhysAddr) {
    unsafe { asm!("mov cr3, {}", in(reg) pml4.0, options(nostack, preserves_flags)) };
}

/// Create a fresh per-process address space: a new PML4 that **shares the
/// kernel's high half** (entries 256..512, copied so kernel code, stack, and
/// the physical-memory window stay mapped) and has an **empty low half**
/// (entries 0..256) for per-process user mappings. Returns the new PML4's
/// physical base.
///
/// # Safety
/// Must be called after [`mm_save_kernel_address_space`].
pub unsafe fn mm_create_address_space() -> PhysAddr {
    unsafe {
        let new = mm_allocate_page().expect("PML4 allocation"); // zeroed
        let new_t = phys_to_virt(new) as *mut u64;
        let kern_t = phys_to_virt(mm_kernel_address_space()) as *const u64;
        for i in 256..512 {
            *new_t.add(i) = *kern_t.add(i);
        }
        new
    }
}

/// Map `pages` 4 KiB pages at user virtual address `va` to physical `phys`
/// in address space `pml4`, allocating intermediate page tables as needed.
/// The pages are user-accessible; `writable`/`exec` set RW/NX. Intended for
/// low-half (user) addresses.
///
/// # Safety
/// `pml4` must be a valid address space; `va` should be in the low half;
/// `phys` must cover `pages` frames the caller owns.
pub unsafe fn mm_map_user_range(
    pml4: PhysAddr,
    va: u64,
    phys: PhysAddr,
    pages: usize,
    writable: bool,
    exec: bool,
) {
    unsafe {
        for i in 0..pages {
            let v = va + (i as u64) * 0x1000;
            let p = phys.0 + (i as u64) * 0x1000;
            let idx = |shift: u64| ((v >> shift) & 0x1FF) as usize;
            let pdpt = get_or_create_table(pml4, idx(39));
            let pd = get_or_create_table(pdpt, idx(30));
            let pt = get_or_create_table(pd, idx(21));
            let pte = entry_ptr(pt, idx(12));
            *pte = (p & ADDR_MASK)
                | ENTRY_PRESENT
                | ENTRY_USER
                | if writable { ENTRY_RW } else { 0 }
                | if exec { 0 } else { ENTRY_NX };
        }
    }
}

/// Return the child table of `table[idx]`, creating a present, writable,
/// user-accessible intermediate entry (and a zeroed table) if absent.
unsafe fn get_or_create_table(table: PhysAddr, idx: usize) -> PhysAddr {
    unsafe {
        let e = entry_ptr(table, idx);
        if *e & ENTRY_PRESENT != 0 {
            return PhysAddr(*e & ADDR_MASK);
        }
        let child = mm_allocate_page().expect("page table allocation"); // zeroed
        *e = (child.0 & ADDR_MASK) | ENTRY_PRESENT | ENTRY_RW | ENTRY_USER;
        child
    }
}

/// Fetch entry `index` of the table at physical `table`.
fn entry(table: PhysAddr, index: usize) -> u64 {
    // SAFETY: page tables are valid RAM covered by the physical window.
    unsafe { (phys_to_virt(table) as *const u64).add(index).read_volatile() }
}

/// Mutable pointer to entry `index` of the table at physical `table`.
unsafe fn entry_ptr(table: PhysAddr, index: usize) -> *mut u64 {
    unsafe { (phys_to_virt(table) as *mut u64).add(index) }
}

/// Make the virtual range `[va, va+len)` executable by clearing the NX bit
/// at **every** level of the paging hierarchy along the walk (NX at any
/// level disables execution for the whole sub-tree), keeping `EFER.NXE`
/// enabled so data pages stay non-executable.
///
/// This is what lets freshly loaded driver code run: pool memory (where the
/// loader maps images) lives in the physical-memory window, which the
/// bootloader marks NX. Note the granularity caveat — if that window is
/// mapped with large pages, clearing NX on the containing 2 MiB/1 GiB page
/// makes that whole page executable. A finer scheme (remap the image onto
/// dedicated 4 KiB pages) is future work; documented as a deliberate
/// coarsening.
///
/// # Safety
/// `va`/`len` must describe kernel-owned memory; the caller intends it to
/// hold code. Modifies live page tables and flushes the TLB.
pub unsafe fn mm_set_executable(va: u64, len: usize) {
    unsafe {
        let mut addr = va & !0xFFF;
        let end = va + len as u64;
        while addr < end {
            split_to_4k(addr);
            clear_nx_path(addr);
            addr += 0x1000;
        }
        flush_tlb();
    }
}

/// Clear the NX bit on every present entry on the path that maps `va`,
/// stopping at the leaf (large page or 4 KiB PTE).
unsafe fn clear_nx_path(va: u64) {
    unsafe {
        let idx = |shift: u64| ((va >> shift) & 0x1FF) as usize;

        let p4 = entry_ptr(current_pml4(), idx(39));
        if *p4 & ENTRY_PRESENT == 0 {
            return;
        }
        *p4 &= !ENTRY_NX;

        let p3 = entry_ptr(PhysAddr(*p4 & ADDR_MASK), idx(30));
        if *p3 & ENTRY_PRESENT == 0 {
            return;
        }
        *p3 &= !ENTRY_NX;
        if *p3 & ENTRY_LARGE != 0 {
            return; // 1 GiB leaf
        }

        let p2 = entry_ptr(PhysAddr(*p3 & ADDR_MASK), idx(21));
        if *p2 & ENTRY_PRESENT == 0 {
            return;
        }
        *p2 &= !ENTRY_NX;
        if *p2 & ENTRY_LARGE != 0 {
            return; // 2 MiB leaf
        }

        let p1 = entry_ptr(PhysAddr(*p2 & ADDR_MASK), idx(12));
        if *p1 & ENTRY_PRESENT == 0 {
            return;
        }
        *p1 &= !ENTRY_NX;
    }
}

/// Make `[va, va+len)` user-accessible and executable: set the U/S bit and
/// clear NX at **every** level of the walk (both attributes are governed by
/// the whole path — a page is user-accessible only if U/S is set at every
/// level, and non-executable if NX is set at any). This hosts ring-3 code
/// and stacks in pages the kernel allocated.
///
/// Large pages on the path are split to 4 KiB first ([`split_to_4k`]), so
/// only the targeted pages become user-accessible — a neighboring supervisor
/// page in the same 2 MiB/1 GiB region is unaffected (essential under SMEP).
///
/// # Safety
/// `va`/`len` must describe kernel-owned memory intended to back user code
/// or stack. Modifies live page tables and flushes the TLB.
pub unsafe fn mm_set_user_executable(va: u64, len: usize) {
    unsafe {
        let mut addr = va & !0xFFF;
        let end = va + len as u64;
        while addr < end {
            split_to_4k(addr);
            set_user_exec_path(addr);
            addr += 0x1000;
        }
        flush_tlb();
    }
}

/// Set U/S and clear NX on every present entry on the path mapping `va`,
/// down to the leaf (large page or 4 KiB PTE).
unsafe fn set_user_exec_path(va: u64) {
    unsafe {
        let idx = |shift: u64| ((va >> shift) & 0x1FF) as usize;
        let touch = |e: *mut u64| {
            *e |= ENTRY_USER;
            *e &= !ENTRY_NX;
        };

        let p4 = entry_ptr(current_pml4(), idx(39));
        if *p4 & ENTRY_PRESENT == 0 {
            return;
        }
        touch(p4);
        let p3 = entry_ptr(PhysAddr(*p4 & ADDR_MASK), idx(30));
        if *p3 & ENTRY_PRESENT == 0 {
            return;
        }
        touch(p3);
        if *p3 & ENTRY_LARGE != 0 {
            return;
        }
        let p2 = entry_ptr(PhysAddr(*p3 & ADDR_MASK), idx(21));
        if *p2 & ENTRY_PRESENT == 0 {
            return;
        }
        touch(p2);
        if *p2 & ENTRY_LARGE != 0 {
            return;
        }
        let p1 = entry_ptr(PhysAddr(*p2 & ADDR_MASK), idx(12));
        if *p1 & ENTRY_PRESENT == 0 {
            return;
        }
        touch(p1);
    }
}

/// Ensure the 4 KiB page containing `va` has its own leaf PTE by splitting
/// any large page on the path down to 4 KiB granularity. This is what makes
/// per-page protection (NX, U/S) precise: without it, changing one page's
/// bits would change the whole enclosing 2 MiB/1 GiB region, which (under
/// SMEP) lets a U/S marking on a user image contaminate a neighboring
/// supervisor driver image sharing the large page.
///
/// Splits one level per call as needed: 1 GiB PDPTE → a PD of 2 MiB pages,
/// then 2 MiB PDE → a PT of 4 KiB pages. New tables inherit the original
/// entry's flags so the mapping is unchanged until a protect routine adjusts
/// the specific 4 KiB leaf.
unsafe fn split_to_4k(va: u64) {
    unsafe {
        let idx = |shift: u64| ((va >> shift) & 0x1FF) as usize;

        let p4 = entry_ptr(current_pml4(), idx(39));
        if *p4 & ENTRY_PRESENT == 0 {
            return;
        }
        let p3 = entry_ptr(PhysAddr(*p4 & ADDR_MASK), idx(30));
        if *p3 & ENTRY_PRESENT == 0 {
            return;
        }
        if *p3 & ENTRY_LARGE != 0 {
            // 1 GiB → 512 × 2 MiB. Each child keeps PS (still a large page).
            let base = *p3 & ADDR_MASK & !0x3FFF_FFFF;
            let flags = *p3 & FLAG_MASK; // includes PS, U/S, RW, NX, …
            let table = new_table();
            let t = phys_to_virt(table) as *mut u64;
            for i in 0..512u64 {
                *t.add(i as usize) = (base + i * 0x20_0000) | flags;
            }
            *p3 = (table.0 & ADDR_MASK)
                | ENTRY_PRESENT
                | (*p3 & (ENTRY_RW | ENTRY_USER | ENTRY_NX));
        }
        let p2 = entry_ptr(PhysAddr(*p3 & ADDR_MASK), idx(21));
        if *p2 & ENTRY_PRESENT == 0 {
            return;
        }
        if *p2 & ENTRY_LARGE != 0 {
            // 2 MiB → 512 × 4 KiB. Children drop PS (real leaf PTEs).
            let base = *p2 & ADDR_MASK & !0x1F_FFFF;
            let flags = *p2 & FLAG_MASK & !ENTRY_LARGE;
            let table = new_table();
            let t = phys_to_virt(table) as *mut u64;
            for i in 0..512u64 {
                *t.add(i as usize) = (base + i * 0x1000) | flags;
            }
            *p2 = (table.0 & ADDR_MASK)
                | ENTRY_PRESENT
                | (*p2 & (ENTRY_RW | ENTRY_USER | ENTRY_NX));
        }
    }
}

/// Allocate a zeroed physical page to serve as a new page table.
unsafe fn new_table() -> PhysAddr {
    mm_allocate_page().expect("page table allocation for split")
}

/// Flush the entire (non-global) TLB by reloading CR3, so the cleared NX
/// bits take effect.
unsafe fn flush_tlb() {
    unsafe {
        let cr3: u64;
        asm!("mov {}, cr3", out(reg) cr3, options(nomem, nostack, preserves_flags));
        asm!("mov cr3, {}", in(reg) cr3, options(nostack, preserves_flags));
    }
}

/// `MmGetPhysicalAddress` — translate a virtual address by walking the
/// live page tables. Returns `None` for unmapped addresses. Handles the
/// 1 GiB and 2 MiB large-page short-circuits.
pub fn mm_get_physical_address(va: u64) -> Option<PhysAddr> {
    let idx = |shift: u64| ((va >> shift) & 0x1FF) as usize;

    let pml4e = entry(current_pml4(), idx(39));
    if pml4e & ENTRY_PRESENT == 0 {
        return None;
    }
    let pdpte = entry(PhysAddr(pml4e & ADDR_MASK), idx(30));
    if pdpte & ENTRY_PRESENT == 0 {
        return None;
    }
    if pdpte & ENTRY_LARGE != 0 {
        return Some(PhysAddr((pdpte & ADDR_MASK & !0x3FFF_FFFF) | (va & 0x3FFF_FFFF)));
    }
    let pde = entry(PhysAddr(pdpte & ADDR_MASK), idx(21));
    if pde & ENTRY_PRESENT == 0 {
        return None;
    }
    if pde & ENTRY_LARGE != 0 {
        return Some(PhysAddr((pde & ADDR_MASK & !0x1F_FFFF) | (va & 0x1F_FFFF)));
    }
    let pte = entry(PhysAddr(pde & ADDR_MASK), idx(12));
    if pte & ENTRY_PRESENT == 0 {
        return None;
    }
    Some(PhysAddr((pte & ADDR_MASK) | (va & 0xFFF)))
}


// ---------------------------------------------------------------------------
// Write-side single-page operations (VAD demand-commit / free / protect)
// ---------------------------------------------------------------------------

/// Invalidate the TLB entry for the page containing `va` (current AS).
unsafe fn invlpg(va: u64) {
    unsafe { asm!("invlpg [{}]", in(reg) va, options(nostack)) };
}

/// Walk the current address space to the 4 KiB leaf PTE for `va`. Returns
/// `None` on any not-present level, and on a large-page leaf: the user low
/// half is only ever mapped at 4 KiB granularity by us, so a large leaf here
/// is a supervisor mapping (identity/window) we must not touch.
unsafe fn leaf_pte(va: u64) -> Option<*mut u64> {
    unsafe {
        let idx = |shift: u64| ((va >> shift) & 0x1FF) as usize;
        let p4 = entry(current_pml4(), idx(39));
        if p4 & ENTRY_PRESENT == 0 {
            return None;
        }
        let p3 = entry(PhysAddr(p4 & ADDR_MASK), idx(30));
        if p3 & ENTRY_PRESENT == 0 || p3 & ENTRY_LARGE != 0 {
            return None;
        }
        let p2 = entry(PhysAddr(p3 & ADDR_MASK), idx(21));
        if p2 & ENTRY_PRESENT == 0 || p2 & ENTRY_LARGE != 0 {
            return None;
        }
        Some(entry_ptr(PhysAddr(p2 & ADDR_MASK), idx(12)))
    }
}

/// Unmap the 4 KiB page at `va` from the **current** address space,
/// returning the frame it mapped (`None` when it was never backed).
/// Intermediate tables are left in place — they are shared with neighboring
/// mappings and reclaimed wholesale at address-space teardown.
///
/// # Safety
/// `va` must be a user page this address space owns (a VAD-backed range).
pub unsafe fn mm_unmap_user_page(va: u64) -> Option<PhysAddr> {
    unsafe {
        let pte = leaf_pte(va)?;
        if *pte & ENTRY_PRESENT == 0 {
            return None;
        }
        let pa = PhysAddr(*pte & ADDR_MASK);
        *pte = 0;
        invlpg(va);
        // Threads of this address space may be running on other CPUs.
        crate::ke::smp::tlb_shootdown(current_pml4().0, va);
        Some(pa)
    }
}

/// Change the write/execute bits of the page at `va` in the **current**
/// address space. A no-op when the page isn't backed — an unbacked VAD page
/// picks up its protection from the descriptor at fault time instead.
///
/// # Safety
/// `va` must be a user page this address space owns.
pub unsafe fn mm_protect_user_page(va: u64, writable: bool, executable: bool) {
    unsafe {
        let Some(pte) = leaf_pte(va) else { return };
        if *pte & ENTRY_PRESENT == 0 {
            return;
        }
        *pte = (*pte & !ENTRY_RW & !ENTRY_NX)
            | if writable { ENTRY_RW } else { 0 }
            | if executable { 0 } else { ENTRY_NX };
        invlpg(va);
        crate::ke::smp::tlb_shootdown(current_pml4().0, va);
    }
}

/// Raw PTE for `va` in the current address space — self-test surface for
/// asserting protection bits (`NtProtectVirtualMemory` made it read-only?).
pub fn mm_debug_pte(va: u64) -> Option<u64> {
    unsafe {
        let pte = leaf_pte(va)?;
        if *pte & ENTRY_PRESENT == 0 {
            return None;
        }
        Some(*pte)
    }
}

/// Raw PTE for `va` in the address space `pml4` (present or not). Read-only
/// walk through the physical window — the page-out engine uses this to
/// validate and unmap pages in an address space that isn't the current one.
pub fn mm_debug_pte_in(pml4: PhysAddr, va: u64) -> Option<u64> {
    let idx = |shift: u64| ((va >> shift) & 0x1FF) as usize;
    unsafe {
        let p4 = entry(pml4, idx(39));
        if p4 & ENTRY_PRESENT == 0 {
            return None;
        }
        let p3 = entry(PhysAddr(p4 & ADDR_MASK), idx(30));
        if p3 & ENTRY_PRESENT == 0 || p3 & ENTRY_LARGE != 0 {
            return None;
        }
        let p2 = entry(PhysAddr(p3 & ADDR_MASK), idx(21));
        if p2 & ENTRY_PRESENT == 0 || p2 & ENTRY_LARGE != 0 {
            return None;
        }
        Some(entry(PhysAddr(p2 & ADDR_MASK), idx(12)))
    }
}

/// Clear the 4 KiB leaf PTE of `va` in the address space `pml4`, returning
/// the frame it mapped (`None` when it was never backed). Flushes the TLB
/// entry locally when `pml4` is the current address space, and on every
/// other CPU currently running `pml4` via a shootdown IPI.
///
/// # Safety
/// `va` must be a user page owned by `pml4`.
pub unsafe fn mm_unmap_user_page_in(pml4: PhysAddr, va: u64) -> Option<PhysAddr> {
    unsafe {
        let idx = |shift: u64| ((va >> shift) & 0x1FF) as usize;
        let p4 = entry(pml4, idx(39));
        if p4 & ENTRY_PRESENT == 0 {
            return None;
        }
        let p3 = entry(PhysAddr(p4 & ADDR_MASK), idx(30));
        if p3 & ENTRY_PRESENT == 0 || p3 & ENTRY_LARGE != 0 {
            return None;
        }
        let p2 = entry(PhysAddr(p3 & ADDR_MASK), idx(21));
        if p2 & ENTRY_PRESENT == 0 || p2 & ENTRY_LARGE != 0 {
            return None;
        }
        let pte = entry_ptr(PhysAddr(p2 & ADDR_MASK), idx(12));
        if *pte & ENTRY_PRESENT == 0 {
            return None;
        }
        let pa = PhysAddr(*pte & ADDR_MASK);
        *pte = 0;
        if pml4 == current_pml4() {
            invlpg(va);
        }
        crate::ke::smp::tlb_shootdown(pml4.0, va);
        Some(pa)
    }
}

/// Free an entire per-process user address space: every low-half leaf's
/// backing frame, the intermediate page tables, and the PML4 itself.
///
/// The low half of a per-process space only ever holds mappings this kernel
/// created — the loaded image, the user stack, the TEB/PEB block, and
/// VAD-backed heap — all 4 KiB leaves, all owned by the process, so every
/// present leaf frame is reclaimed here (VAD-backed or not).
///
/// # Safety
/// The caller must **not** be running on `pml4` — switch to the kernel
/// address space first; the PML4 is freed. No TLB flush is needed: the
/// space is never loaded into CR3 again.
pub unsafe fn mm_free_user_address_space(pml4: PhysAddr) {
    unsafe {
        let t = phys_to_virt(pml4) as *mut u64;
        for i in 0..256 {
            let e = *t.add(i);
            if e & ENTRY_PRESENT != 0 {
                free_table_level(PhysAddr(e & ADDR_MASK), 3);
            }
        }
        mm_free_contiguous_pages(pml4, 1);
    }
}

/// Recursive walk-and-free under a table (`level` 3 = PDPT, 2 = PD, 1 = PT):
/// frees every present leaf's backing frame, then the table page itself.
unsafe fn free_table_level(table: PhysAddr, level: u8) {
    unsafe {
        let t = phys_to_virt(table) as *mut u64;
        for i in 0..512 {
            let e = *t.add(i);
            if e & ENTRY_PRESENT == 0 {
                continue;
            }
            debug_assert!(e & ENTRY_LARGE == 0, "large page in user low half");
            let child = PhysAddr(e & ADDR_MASK);
            if level > 1 {
                free_table_level(child, level - 1);
            } else {
                mm_free_contiguous_pages(child, 1);
            }
        }
        mm_free_contiguous_pages(table, 1);
    }
}

// ---------------------------------------------------------------------------
// Per-process privatization of shared (phys-window) pages
// ---------------------------------------------------------------------------

/// Record of one process's privatized shared-range state: the cloned page
/// tables and the fresh frames backing the private copies. Returned by
/// [`mm_privatize_pages`], freed by [`mm_free_privatized`].
///
/// The clone works at whatever granularity the shared mapping uses: PDPT/PD
/// entries pointing at shared tables are cloned (copied), large leaves are
/// split into finer tables replicating the mapping — then the leaf PTE gets
/// a new frame with the shared content. Everything else in the window keeps
/// pointing at shared tables/frames, so non-shim window data is untouched.
pub struct Privatized {
    /// The process's private PDPT for the window's PML4 slot (0 = none yet).
    pub pdpt: PhysAddr,
    /// Cloned/split table frames (PDs, PTs) — freed as tables.
    pub tables: [PhysAddr; MAX_PRIV_TABLES],
    pub n_tables: usize,
    /// Private data frames (leaf content copies) — freed as frames.
    pub frames: [PhysAddr; MAX_PRIV_FRAMES],
    pub n_frames: usize,
}

const MAX_PRIV_TABLES: usize = 16;
const MAX_PRIV_FRAMES: usize = 256;

impl Privatized {
    pub const fn new() -> Self {
        Privatized {
            pdpt: PhysAddr(0),
            tables: [PhysAddr(0); MAX_PRIV_TABLES],
            n_tables: 0,
            frames: [PhysAddr(0); MAX_PRIV_FRAMES],
            n_frames: 0,
        }
    }

    fn push_table(&mut self, t: PhysAddr) -> Option<()> {
        if self.n_tables >= MAX_PRIV_TABLES {
            return None;
        }
        self.tables[self.n_tables] = t;
        self.n_tables += 1;
        Some(())
    }

    fn push_frame(&mut self, f: PhysAddr) -> Option<()> {
        if self.n_frames >= MAX_PRIV_FRAMES {
            return None;
        }
        self.frames[self.n_frames] = f;
        self.n_frames += 1;
        Some(())
    }

    fn is_ours(&self, t: PhysAddr) -> bool {
        self.tables[..self.n_tables].contains(&t)
    }
}

/// Give address space `pml4` a private copy of every 4 KiB page of
/// `[va, va+len)`: the range's page-table chain is cloned (so the process
/// stops sharing the intermediate tables for it) and each leaf gets a fresh
/// frame initialized from `seed` (the pristine bytes for the range,
/// `seed.len() == len`; pass the shared content itself when no separate
/// seed exists). The classic use is per-process DLL data — NT's
/// copy-on-write, done eagerly at our scale.
///
/// Returns false (leaving shared mappings in place) if the range isn't
/// mapped or the record caps are exceeded.
pub fn mm_privatize_pages(pml4: PhysAddr, va: u64, len: usize, rec: &mut Privatized, seed: &[u8]) -> bool {
    let win_idx = (((phys_to_virt(PhysAddr(0)) as u64) >> 39) & 0x1FF) as usize;
    if seed.len() < len {
        return false;
    }
    unsafe {
        // Ensure the process's PML4 slot for the phys window holds a private
        // PDPT clone rather than the shared one (copied at AS creation).
        if rec.pdpt.0 == 0 {
            let p4 = entry_ptr(pml4, win_idx);
            let cur = *p4;
            if cur & ENTRY_PRESENT == 0 {
                return false;
            }
            let clone = new_table();
            core::ptr::copy_nonoverlapping(
                phys_to_virt(PhysAddr(cur & ADDR_MASK)) as *const u64,
                phys_to_virt(clone) as *mut u64,
                512,
            );
            if rec.push_table(clone).is_none() {
                mm_free_contiguous_pages(clone, 1);
                return false;
            }
            *p4 = clone.0 | (cur & !ADDR_MASK);
            rec.pdpt = clone;
        }
        let base = va & !0xFFF;
        let mut page = base;
        let end = va + len as u64;
        while page < end {
            let s = (page - base) as usize;
            if !privatize_leaf(rec, page, &seed[s..]) {
                return false;
            }
            page += 0x1000;
        }
    }
    true
}

/// Clone the chain from `rec.pdpt` down to `va`'s leaf and give the leaf a
/// fresh frame initialized from `seed` (at least one page long). See
/// [`mm_privatize_pages`].
unsafe fn privatize_leaf(rec: &mut Privatized, va: u64, seed: &[u8]) -> bool {
    let idx = |shift: u64| ((va >> shift) & 0x1FF) as usize;
    unsafe {
        let mut parent = rec.pdpt;
        let mut shift = 30u64; // PDPT entry index bits
        loop {
            let e = entry_ptr(parent, idx(shift));
            let cur = *e;
            if cur & ENTRY_PRESENT == 0 {
                return false; // range not mapped in the shared space
            }
            let child = PhysAddr(cur & ADDR_MASK);
            if shift == 12 {
                // Leaf: swap in a fresh frame seeded with the pristine bytes.
                let Some(frame) = mm_allocate_page() else { return false };
                core::ptr::copy_nonoverlapping(
                    seed.as_ptr(),
                    phys_to_virt(frame) as *mut u8,
                    super::PAGE_SIZE,
                );
                if rec.push_frame(frame).is_none() {
                    mm_free_contiguous_pages(frame, 1);
                    return false;
                }
                *e = (cur & !ADDR_MASK) | frame.0;
                return true;
            }
            if cur & ENTRY_LARGE != 0 {
                // Large leaf: split into a finer table replicating the mapping
                // (1 GiB → 512 × 2 MiB, or 2 MiB → 512 × 4 KiB).
                let child_span = 1u64 << (shift - 9);
                let base = cur & ADDR_MASK & !(child_span * 512 - 1);
                let flags = cur & FLAG_MASK & !ENTRY_LARGE;
                let table = new_table();
                let t = phys_to_virt(table) as *mut u64;
                for i in 0..512u64 {
                    *t.add(i as usize) = (base + i * child_span) | flags;
                }
                if rec.push_table(table).is_none() {
                    mm_free_contiguous_pages(table, 1);
                    return false;
                }
                *e = table.0 | ENTRY_PRESENT | (cur & (ENTRY_RW | ENTRY_USER | ENTRY_NX));
                parent = table;
            } else if rec.is_ours(child) {
                parent = child; // already cloned on an earlier page
            } else {
                // Shared intermediate table: clone it (entries and all).
                let table = new_table();
                core::ptr::copy_nonoverlapping(
                    phys_to_virt(child) as *const u64,
                    phys_to_virt(table) as *mut u64,
                    512,
                );
                if rec.push_table(table).is_none() {
                    mm_free_contiguous_pages(table, 1);
                    return false;
                }
                *e = table.0 | (cur & !ADDR_MASK);
                parent = table;
            }
            shift -= 9;
        }
    }
}

/// Free every frame and cloned table recorded in `rec` (process teardown).
/// The process's PML4 is reclaimed separately by `mm_free_user_address_space`.
pub fn mm_free_privatized(rec: &mut Privatized) {
    for &f in &rec.frames[..rec.n_frames] {
        mm_free_contiguous_pages(f, 1);
    }
    for &t in &rec.tables[..rec.n_tables] {
        mm_free_contiguous_pages(t, 1);
    }
    *rec = Privatized::new();
}
