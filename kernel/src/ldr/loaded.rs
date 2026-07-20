//! Loaded user-mode modules and cross-module import resolution.
//!
//! A console app imports from `kernel32.dll`, which we satisfy by loading a
//! shim DLL and resolving the app's imports against its export table. This
//! is the kernel's tiny user-mode dynamic linker: load the support module(s),
//! then resolve each imported name against (a) the `ntdll` syscall
//! trampoline and (b) the exports of any loaded module.
//!
//! Single support module for now (`kernel32`); generalizes to a list when
//! more DLLs appear.

use super::{ntdll, pe};
use crate::ke::spinlock::SpinLock;
use crate::rtl::NtStatus;
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

static KERNEL32_BASE: AtomicU64 = AtomicU64::new(0);
static KERNEL32_SIZE: AtomicUsize = AtomicUsize::new(0);
static MSVCRT_BASE: AtomicU64 = AtomicU64::new(0);
static MSVCRT_SIZE: AtomicUsize = AtomicUsize::new(0);
static ULIB_BASE: AtomicU64 = AtomicU64::new(0);
static ULIB_SIZE: AtomicUsize = AtomicUsize::new(0);
/// ulib.dll's entry point (its `DllMain`) VA. A process that imports ulib must
/// run this with `DLL_PROCESS_ATTACH` before its own entry, so ulib's one-time
/// init (standard-stream objects, heap) runs — `PROGRAM::Initialize` fails if it
/// hasn't. 0 until ulib is loaded.
static ULIB_ENTRY: AtomicU64 = AtomicU64::new(0);

/// Load the `kernel32` shim DLL into user-accessible memory. It has no
/// imports of its own (its functions issue syscalls inline), so loading is a
/// plain `load_user`. Phase-1, before any console app is loaded.
pub fn load_kernel32(image: &[u8]) -> Result<(), NtStatus> {
    let loaded = pe::load_user(image)?;
    KERNEL32_BASE.store(loaded.base as u64, Ordering::Release);
    KERNEL32_SIZE.store(loaded.size, Ordering::Release);
    register_shim_data(image, loaded.base as u64);
    // Point the ntdll-page exception-dispatcher thunk at the shim's
    // KiUserExceptionDispatcher, so user-mode exceptions (ke::exception)
    // reach the vectored-handler machinery instead of the terminate stub.
    if let Some(va) = resolve_export_in(loaded.base as u64, loaded.size, "KiUserExceptionDispatcher") {
        ntdll::set_exception_dispatcher(va as u64);
    }
    crate::kd_println!(
        "LDR: loaded kernel32.dll @ {:p} ({} bytes)",
        loaded.base,
        loaded.size
    );
    Ok(())
}

/// Load the `msvcrt` C-runtime shim DLL. Like `kernel32` it issues syscalls
/// inline (no imports), so a plain `load_user` suffices. Phase-1, after
/// `kernel32`. Lets a real classic-CRT console binary bind its `msvcrt`
/// imports to our implementation.
pub fn load_msvcrt(image: &[u8]) -> Result<(), NtStatus> {
    let loaded = pe::load_user(image)?;
    MSVCRT_BASE.store(loaded.base as u64, Ordering::Release);
    MSVCRT_SIZE.store(loaded.size, Ordering::Release);
    register_shim_data(image, loaded.base as u64);
    crate::kd_println!(
        "LDR: loaded msvcrt.dll @ {:p} ({} bytes)",
        loaded.base,
        loaded.size
    );
    Ok(())
}

/// Load `ulib.dll` — a real dependent DLL (unlike the shims, it has imports of
/// its own). `load_user` binds those imports against the already-loaded shims
/// (kernel32/msvcrt/ntdll), maps it user-executable in the shared high half,
/// and we record its export table so a consumer's `ulib` imports resolve. Must
/// run after kernel32/msvcrt. Skips cleanly if `ulib.dll` wasn't staged.
pub fn load_ulib(image: &[u8]) -> Result<(), NtStatus> {
    if image.is_empty() {
        return Ok(());
    }
    let loaded = pe::load_user(image)?;
    ULIB_BASE.store(loaded.base as u64, Ordering::Release);
    ULIB_SIZE.store(loaded.size, Ordering::Release);
    ULIB_ENTRY.store(loaded.entry_va, Ordering::Release);
    // ulib's writable sections are per-process C-runtime state (CRT guards,
    // standard streams, heap) exactly like the shims': register them so every
    // process gets its own private pages for them.
    register_shim_data(image, loaded.base as u64);
    crate::kd_println!(
        "LDR: loaded ulib.dll @ {:p} ({} bytes)",
        loaded.base,
        loaded.size
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Per-process shim data (private DLL .data pages)
// ---------------------------------------------------------------------------
//
// The shim DLLs (`kernel32`, `msvcrt`, `ulib`) are shared code in the high
// half, so a single physical copy of their writable `.data` is visible to
// every process. But that data holds *per-process* C-runtime state —
// msvcrt's fd table and cached standard handles, ulib's CRT guards and
// standard streams. With one shared copy, a concurrent child's CRT init
// clobbers the parent's fd table mid-pipe-setup (`dir | sort` feeds `sort`
// the console instead of the pipe), and on SMP two isolated processes on
// two CPUs fight over the same pages.
//
// NT gives each process a private, copy-on-write copy of a DLL's data. We
// do the equivalent eagerly: at process creation every writable shim page
// is *privatized* into the process's address space — the page-table chain
// for the range is cloned and each leaf gets a fresh frame (see
// `mm::virt::mm_privatize_pages`). The shared pages then serve only as the
// pristine template: nothing ever writes them again, so the per-process
// copy starts from post-load state every time. (This replaced a
// context-switch swap of per-process buffers — correct only on one CPU.)

/// A writable region of a shim, captured post-load, page-aligned.
/// `snap_off` is its offset into the flat pristine snapshot.
#[derive(Clone, Copy)]
struct ShimRegion {
    va: u64,
    len: usize,
    snap_off: usize,
}

const MAX_SHIM_REGIONS: usize = 8;
/// Total writable bytes tracked across the shims (kernel32/msvcrt/ulib).
const SHIM_DATA_MAX: usize = 128 * 1024;

struct ShimData {
    regions: [ShimRegion; MAX_SHIM_REGIONS],
    n: usize,
    total: usize,
    /// Pristine post-load bytes of every region, concatenated by `snap_off`.
    /// This is the *seed* for per-process privatization: the shared page
    /// itself is live state for kernel-AS apps (their VEH lists, heap
    /// arena, fd table), so copying it would leak one app's registrations
    /// into every new process.
    snapshot: [u8; SHIM_DATA_MAX],
}

static SHIM_DATA: SpinLock<ShimData> = SpinLock::new(ShimData {
    regions: [ShimRegion { va: 0, len: 0, snap_off: 0 }; MAX_SHIM_REGIONS],
    n: 0,
    total: 0,
    snapshot: [0u8; SHIM_DATA_MAX],
});

/// Per-process privatization records, keyed by address space.
const MAX_SHIM_SLOTS: usize = 16;
struct ShimSlot {
    cr3: u64,
    in_use: bool,
    pages: crate::mm::virt::Privatized,
}
static SHIM_SLOTS: SpinLock<[ShimSlot; MAX_SHIM_SLOTS]> = SpinLock::new(
    [const { ShimSlot { cr3: 0, in_use: false, pages: crate::mm::virt::Privatized::new() } };
        MAX_SHIM_SLOTS],
);

/// Record a shim's writable sections (page-aligned spans) and snapshot their
/// pristine post-load bytes, so [`privatize_shim_data`] can seed every
/// process's private pages from them. Call once per shim, right after it is
/// loaded and before any process runs it.
pub fn register_shim_data(image: &[u8], base: u64) {
    let mut secs = [(0u32, 0u32); MAX_SHIM_REGIONS];
    let n = pe::writable_sections(image, &mut secs);
    let mut sd = SHIM_DATA.lock();
    // The shim image is mapped user-accessible; a supervisor read traps under
    // SMAP, so bracket the snapshot copy.
    crate::mm::virt::user_access_begin();
    for &(rva, vsize) in secs.iter().take(n) {
        // Page-align the span (privatization works per 4 KiB page).
        let lo = rva as u64 & !0xFFF;
        let hi = (rva as u64 + vsize as u64 + 0xFFF) & !0xFFF;
        let len = (hi - lo) as usize;
        if sd.n >= MAX_SHIM_REGIONS || sd.total + len > SHIM_DATA_MAX {
            break;
        }
        let off = sd.total;
        let va = base + lo;
        unsafe {
            core::ptr::copy_nonoverlapping(va as *const u8, sd.snapshot[off..].as_mut_ptr(), len);
        }
        let i = sd.n;
        sd.regions[i] = ShimRegion { va, len, snap_off: off };
        sd.n += 1;
        sd.total += len;
    }
    crate::mm::virt::user_access_end();
}

/// Give address space `cr3` private pages for every registered shim region.
/// Idempotent per `cr3`. No-op for the kernel (`cr3 == 0`: kernel-AS apps
/// share the pages, as they always have) or before any shim registered.
pub fn privatize_shim_data(cr3: u64) {
    if cr3 == 0 {
        return;
    }
    // Hold SHIM_DATA across the loop: the pristine snapshot is the seed for
    // every private page (never the shared page, which kernel-AS apps keep
    // mutating — see SHIM_DATA's doc). Lock order SHIM_DATA → SHIM_SLOTS →
    // PFN (inside mm_privatize_pages); no one nests the other way.
    let sd = SHIM_DATA.lock();
    if sd.n == 0 {
        return;
    }
    let mut sl = SHIM_SLOTS.lock();
    let idx = sl
        .iter()
        .position(|s| s.in_use && s.cr3 == cr3)
        .or_else(|| sl.iter().position(|s| !s.in_use));
    let Some(i) = idx else {
        crate::kd_println!("LDR: no shim slot for cr3 {:#X} — sharing shim data", cr3);
        return;
    };
    let rec = &mut sl[i].pages;
    for r in &sd.regions[..sd.n] {
        let seed = &sd.snapshot[r.snap_off..r.snap_off + r.len];
        if !crate::mm::virt::mm_privatize_pages(crate::mm::PhysAddr(cr3), r.va, r.len, rec, seed) {
            crate::kd_println!("LDR: shim privatization partial for cr3 {:#X}", cr3);
            break;
        }
    }
    sl[i].cr3 = cr3;
    sl[i].in_use = true;
}

/// Release address space `cr3`'s private shim pages on process exit.
pub fn free_shim_pages(cr3: u64) {
    if cr3 == 0 {
        return;
    }
    let mut sl = SHIM_SLOTS.lock();
    for s in sl.iter_mut() {
        if s.in_use && s.cr3 == cr3 {
            crate::mm::virt::mm_free_privatized(&mut s.pages);
            s.in_use = false;
            s.cr3 = 0;
            break;
        }
    }
}

/// The VA of the first registered shim writable region (0 when none) —
/// self-test surface for proving per-process privatization.
pub fn first_shim_data_va() -> u64 {
    let sd = SHIM_DATA.lock();
    if sd.n == 0 {
        return 0;
    }
    sd.regions[0].va
}

/// `(base, size)` of the loaded ulib.dll (for the debugger's module map).
pub fn ulib_range() -> (u64, usize) {
    (ULIB_BASE.load(Ordering::Acquire), ULIB_SIZE.load(Ordering::Acquire))
}

/// `(base, entry)` of ulib.dll — its load address (the `HINSTANCE` DllMain
/// expects) and its `DllMain` VA. `(0, 0)` if ulib isn't loaded.
pub fn ulib_base_and_entry() -> (u64, u64) {
    (ULIB_BASE.load(Ordering::Acquire), ULIB_ENTRY.load(Ordering::Acquire))
}

/// `(base, size)` of the loaded kernel32 shim (for the debugger's module map).
pub fn kernel32_range() -> (u64, usize) {
    (KERNEL32_BASE.load(Ordering::Acquire), KERNEL32_SIZE.load(Ordering::Acquire))
}
/// `(base, size)` of the loaded msvcrt shim.
pub fn msvcrt_range() -> (u64, usize) {
    (MSVCRT_BASE.load(Ordering::Acquire), MSVCRT_SIZE.load(Ordering::Acquire))
}

/// Case-insensitive module-name match, tolerating an optional `.dll` suffix
/// on the query (so `GetModuleHandleA("KERNEL32.DLL")` and `"kernel32"` both
/// match the `kernel32` module).
fn module_name_matches(query: &str, name: &str) -> bool {
    let q = if query.len() >= 4 && query[query.len() - 4..].eq_ignore_ascii_case(".dll") {
        &query[..query.len() - 4]
    } else {
        query
    };
    q.eq_ignore_ascii_case(name)
}

/// `GetModuleHandleA` backend: map a module name to its loaded base VA (the
/// value Win32 treats as an `HMODULE`). Returns 0 for an unknown module.
/// A NULL/empty query (the caller's own image) is not tracked yet → 0.
pub fn module_base(name: &str) -> u64 {
    if name.is_empty() {
        return 0;
    }
    if module_name_matches(name, "kernel32") {
        return KERNEL32_BASE.load(Ordering::Acquire);
    }
    if module_name_matches(name, "ntdll") {
        return ntdll::trampoline_base();
    }
    if module_name_matches(name, "msvcrt") {
        return MSVCRT_BASE.load(Ordering::Acquire);
    }
    if module_name_matches(name, "ulib") {
        return ULIB_BASE.load(Ordering::Acquire);
    }
    0
}

/// Address of the generic by-ordinal import fallback (`kernel32!__ordinal_stub`),
/// used by the loader when binding an import referenced by ordinal rather than
/// name. `None` until kernel32 is loaded.
pub fn ordinal_stub() -> Option<usize> {
    resolve_export_in(
        KERNEL32_BASE.load(Ordering::Acquire),
        KERNEL32_SIZE.load(Ordering::Acquire),
        "__ordinal_stub",
    )
    .map(|va| va as usize)
}

// --- Per-name unresolved-import stubs (instrumentation) --------------------
// Unimplemented by-name imports each get their OWN return-0 stub at a distinct
// address (instead of all sharing __ordinal_stub), and we log name->address.
// Behaviour is identical (return 0), but the API tracer's call target now
// uniquely identifies WHICH missing import a binary actually calls — essential
// for finding the next function to implement (e.g. cmd.exe's command dispatch).
const UNRESOLVED_MAX: usize = 256; // 256 * 16-byte stubs == one page
const UNRESOLVED_STRIDE: usize = 16;
static UNRESOLVED_PAGE: AtomicU64 = AtomicU64::new(0);
static UNRESOLVED_COUNT: AtomicUsize = AtomicUsize::new(0);

/// Assign a distinct stub to an unresolved import `name`, logging the mapping.
/// Each unimplemented by-name import gets its OWN return-0 stub at a unique
/// address (rather than all sharing `__ordinal_stub`), so the API tracer's call
/// target — cross-referenced with the boot-time "unresolved import" log below —
/// identifies exactly WHICH missing import a binary calls. Returns the stub VA.
pub fn unresolved_stub(name: &str) -> Option<usize> {
    let mut base = UNRESOLVED_PAGE.load(Ordering::Acquire);
    if base == 0 {
        let pa = crate::mm::phys::mm_allocate_page()?;
        let va = crate::mm::phys_to_virt(pa) as u64;
        // Fill the page with identical `xor eax,eax ; ret` stubs (return 0)
        // while it is still writable, then mark it user-executable (read-only).
        unsafe {
            for i in 0..UNRESOLVED_MAX {
                let s = (va as *mut u8).add(i * UNRESOLVED_STRIDE);
                *s = 0x31; // xor eax, eax
                *s.add(1) = 0xC0;
                *s.add(2) = 0xC3; // ret
            }
            crate::mm::virt::mm_set_user_executable(va, crate::mm::PAGE_SIZE);
        }
        UNRESOLVED_PAGE.store(va, Ordering::Release);
        base = va;
    }
    let i = UNRESOLVED_COUNT.fetch_add(1, Ordering::AcqRel);
    if i >= UNRESOLVED_MAX {
        return ordinal_stub();
    }
    let addr = base + (i * UNRESOLVED_STRIDE) as u64;
    crate::kd_println!("LDR: unresolved import {} -> stub {:#x}", name, addr);
    Some(addr as usize)
}

/// Base VA + size of the unresolved-stub page (for the debug tracer's module
/// map, so calls into a missing import are labelled). 0 if not built yet.
pub fn unresolved_range() -> (u64, usize) {
    (UNRESOLVED_PAGE.load(Ordering::Acquire), crate::mm::PAGE_SIZE)
}

/// Read an export `name` from an already-loaded user-accessible image at
/// `(base, size)`, bracketing the read for SMAP (the image is U/S).
fn resolve_export_in(base: u64, size: usize, name: &str) -> Option<u64> {
    if base == 0 {
        return None;
    }
    crate::mm::virt::user_access_begin();
    let resolved = unsafe { pe::resolve_export(base as *const u8, size, name) };
    crate::mm::virt::user_access_end();
    resolved
}

/// `GetProcAddress` backend: resolve `name` within the module identified by
/// `module_base` (an `HMODULE` returned by [`module_base`]). kernel32 names
/// are resolved by parsing its PE export directory; ntdll names map to the
/// syscall-trampoline stubs. Returns 0 if the module or name is unknown.
pub fn proc_address(module_base: u64, name: &str) -> usize {
    if module_base == 0 {
        return 0;
    }
    let k32 = KERNEL32_BASE.load(Ordering::Acquire);
    if module_base == k32 && k32 != 0 {
        let size = KERNEL32_SIZE.load(Ordering::Acquire);
        return resolve_export_in(module_base, size, name).map(|va| va as usize).unwrap_or(0);
    }
    let crt = MSVCRT_BASE.load(Ordering::Acquire);
    if module_base == crt && crt != 0 {
        let size = MSVCRT_SIZE.load(Ordering::Acquire);
        return resolve_export_in(module_base, size, name).map(|va| va as usize).unwrap_or(0);
    }
    let ulib = ULIB_BASE.load(Ordering::Acquire);
    if module_base == ulib && ulib != 0 {
        let size = ULIB_SIZE.load(Ordering::Acquire);
        return resolve_export_in(module_base, size, name).map(|va| va as usize).unwrap_or(0);
    }
    if module_base == ntdll::trampoline_base() {
        return ntdll::resolve_import(name).unwrap_or(0);
    }
    0
}

/// Resolve a user-mode imported symbol against the loaded support modules:
/// the `ntdll` syscall trampoline (the `Nt*` names), then the `kernel32`
/// shim's exports, then the `msvcrt` shim's exports. This is the resolver
/// `load_user`/`load_user_process` hand to the import binder — it lets a
/// console app (or a real classic-CRT binary) bind cross-module imports.
pub fn resolve(name: &str) -> Option<usize> {
    // ucrt exposes many functions under an `_o_<name>` indirection alias
    // (`_o_malloc` == `malloc`, …). Strip the prefix and resolve the real name.
    if let Some(real) = name.strip_prefix("_o_") {
        return resolve(real);
    }
    if let Some(addr) = ntdll::resolve_import(name) {
        return Some(addr);
    }
    if let Some(va) = resolve_export_in(
        KERNEL32_BASE.load(Ordering::Acquire),
        KERNEL32_SIZE.load(Ordering::Acquire),
        name,
    ) {
        return Some(va as usize);
    }
    if let Some(va) = resolve_export_in(
        MSVCRT_BASE.load(Ordering::Acquire),
        MSVCRT_SIZE.load(Ordering::Acquire),
        name,
    ) {
        return Some(va as usize);
    }
    // Dependent DLLs (ulib.dll, …) loaded after the shims. Their exports —
    // e.g. ulib's mangled C++ class methods — resolve here by name.
    if let Some(va) = resolve_export_in(
        ULIB_BASE.load(Ordering::Acquire),
        ULIB_SIZE.load(Ordering::Acquire),
        name,
    ) {
        return Some(va as usize);
    }
    None
}
