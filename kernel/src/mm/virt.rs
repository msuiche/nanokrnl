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

use super::{phys::mm_allocate_page, phys_to_virt, PhysAddr};
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
    // Every page the range touches must be present and user-accessible.
    let mut page = address & !0xFFF;
    while page < end {
        if !page_present_and_user(page) {
            return Err(crate::rtl::NtStatus::ACCESS_VIOLATION);
        }
        page += 0x1000;
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
