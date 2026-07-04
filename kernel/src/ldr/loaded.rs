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

/// Pristine post-load snapshot of ulib.dll's image. ulib lives once in the
/// shared high half, so its writable `.data`/`.bss` (the C-runtime's per-process
/// state: the `/GS` security cookie, the CRT startup-state machine, the on-exit
/// tables, standard-stream/heap pointers) is shared across every process that
/// runs it. On real Windows each process gets a private, copy-on-write copy of a
/// DLL's data; here we emulate that by restoring this snapshot before each
/// process spawn so ulib's `DllMain` re-initializes cleanly. Without it the
/// *second* ulib-based program (e.g. `more.com` run twice) sees "already
/// initialized" CRT guards and aborts during startup. See [`reset_ulib_data`].
const ULIB_SNAPSHOT_MAX: usize = 256 * 1024;
static mut ULIB_SNAPSHOT: [u8; ULIB_SNAPSHOT_MAX] = [0u8; ULIB_SNAPSHOT_MAX];
static ULIB_SNAPSHOT_LEN: AtomicUsize = AtomicUsize::new(0);

/// Load the `kernel32` shim DLL into user-accessible memory. It has no
/// imports of its own (its functions issue syscalls inline), so loading is a
/// plain `load_user`. Phase-1, before any console app is loaded.
pub fn load_kernel32(image: &[u8]) -> Result<(), NtStatus> {
    let loaded = pe::load_user(image)?;
    KERNEL32_BASE.store(loaded.base as u64, Ordering::Release);
    KERNEL32_SIZE.store(loaded.size, Ordering::Release);
    register_shim_data(image, loaded.base as u64);
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
    // Snapshot the pristine post-load image (relocations applied, imports bound,
    // CRT data at its initial values) so every process can start from it.
    let snap_len = loaded.size.min(ULIB_SNAPSHOT_MAX);
    // `load_user` already marked the image user-accessible, so reading it from
    // the kernel traps under SMAP — bracket the copy.
    crate::mm::virt::user_access_begin();
    unsafe {
        core::ptr::copy_nonoverlapping(loaded.base, (&raw mut ULIB_SNAPSHOT) as *mut u8, snap_len);
    }
    crate::mm::virt::user_access_end();
    ULIB_SNAPSHOT_LEN.store(snap_len, Ordering::Release);
    crate::kd_println!(
        "LDR: loaded ulib.dll @ {:p} ({} bytes)",
        loaded.base,
        loaded.size
    );
    Ok(())
}

/// Restore ulib.dll's image to its pristine post-load state. Called before
/// spawning each user process so the shared ulib's C-runtime re-initializes for
/// the new process instead of seeing a previous process's "already initialized"
/// guards. No-op if ulib isn't loaded. Safe because user processes run serially
/// (the creator blocks in `NtWaitForSingleObject`), so no ulib code is executing
/// when this runs.
pub fn reset_ulib_data() {
    let base = ULIB_BASE.load(Ordering::Acquire);
    let len = ULIB_SNAPSHOT_LEN.load(Ordering::Acquire);
    if base == 0 || len == 0 {
        return;
    }
    // ulib's image is mapped user-accessible (it executes in ring 3), so a
    // supervisor write to it traps under SMAP — bracket it like any user access.
    crate::mm::virt::user_access_begin();
    unsafe {
        core::ptr::copy_nonoverlapping((&raw const ULIB_SNAPSHOT) as *const u8, base as *mut u8, len);
    }
    crate::mm::virt::user_access_end();
}

// ---------------------------------------------------------------------------
// Per-process shim data (emulated copy-on-write DLL .data)
// ---------------------------------------------------------------------------
//
// The shim DLLs (`kernel32`, `msvcrt`) are shared code in the high half, so a
// single physical copy of their writable `.data` is visible to every process.
// But that data holds *per-process* C-runtime state - most importantly msvcrt's
// fd table and cached standard handles. With one shared copy, a concurrent
// child's CRT init clobbers the parent's fd table mid-pipe-setup, so the parent
// hands the wrong handle to the next stage (`dir | sort` feeds `sort` the
// console instead of the pipe). Real Windows gives each process a private,
// copy-on-write copy of a DLL's data; we emulate that by keeping a per-process
// buffer of these regions and swapping it in/out of the shared pages on every
// context switch between address spaces. The regions are small (a few KB), so
// the per-switch copy is cheap, and `SHIM_ACTIVE` skips it entirely until at
// least one isolated process exists (so boot and the self-tests pay nothing).

/// A writable region of a shim, captured post-load. `snap_off` is its offset
/// into the flat pristine snapshot / per-process buffers.
#[derive(Clone, Copy)]
struct ShimRegion {
    va: u64,
    len: usize,
    snap_off: usize,
}

const MAX_SHIM_REGIONS: usize = 8;
/// Total writable bytes we track across the shims (msvcrt+kernel32 `.data`).
const SHIM_DATA_MAX: usize = 24 * 1024;

struct ShimData {
    regions: [ShimRegion; MAX_SHIM_REGIONS],
    n: usize,
    total: usize,
    /// Pristine post-load bytes of every region, concatenated by `snap_off`.
    snapshot: [u8; SHIM_DATA_MAX],
}

static SHIM_DATA: SpinLock<ShimData> = SpinLock::new(ShimData {
    regions: [ShimRegion { va: 0, len: 0, snap_off: 0 }; MAX_SHIM_REGIONS],
    n: 0,
    total: 0,
    snapshot: [0u8; SHIM_DATA_MAX],
});

const MAX_SHIM_SLOTS: usize = 16;
struct ShimSlot {
    cr3: u64,
    in_use: bool,
    data: [u8; SHIM_DATA_MAX],
}
static SHIM_SLOTS: SpinLock<[ShimSlot; MAX_SHIM_SLOTS]> = SpinLock::new(
    [const { ShimSlot { cr3: 0, in_use: false, data: [0u8; SHIM_DATA_MAX] } }; MAX_SHIM_SLOTS],
);
/// Number of live per-process buffers. When 0, the context-switch swap is a
/// pure no-op (the common case during boot and self-tests).
static SHIM_ACTIVE: AtomicUsize = AtomicUsize::new(0);

/// Record a shim's writable sections and snapshot their pristine (post-load,
/// pre-run) bytes, so each process can start its private copy from them. Call
/// once per shim, right after it is loaded and before any process runs it.
pub fn register_shim_data(image: &[u8], base: u64) {
    let mut secs = [(0u32, 0u32); MAX_SHIM_REGIONS];
    let n = pe::writable_sections(image, &mut secs);
    let mut sd = SHIM_DATA.lock();
    // The shim image is mapped user-accessible; a supervisor read traps under
    // SMAP, so bracket the snapshot copy.
    crate::mm::virt::user_access_begin();
    for &(rva, vsize) in secs.iter().take(n) {
        let len = vsize as usize;
        if sd.n >= MAX_SHIM_REGIONS || sd.total + len > SHIM_DATA_MAX {
            break;
        }
        let off = sd.total;
        let va = base + rva as u64;
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

/// Give address space `cr3` a private copy of the shim data, initialized to the
/// pristine snapshot. Idempotent per `cr3`. No-op for the kernel (`cr3 == 0`)
/// or before any shim registered.
pub fn alloc_shim_data(cr3: u64) {
    if cr3 == 0 {
        return;
    }
    let sd = SHIM_DATA.lock();
    if sd.n == 0 {
        return;
    }
    let total = sd.total;
    let mut sl = SHIM_SLOTS.lock();
    let idx = sl
        .iter()
        .position(|s| s.in_use && s.cr3 == cr3)
        .or_else(|| sl.iter().position(|s| !s.in_use));
    if let Some(i) = idx {
        let was_free = !sl[i].in_use;
        sl[i].cr3 = cr3;
        sl[i].in_use = true;
        sl[i].data[..total].copy_from_slice(&sd.snapshot[..total]);
        if was_free {
            SHIM_ACTIVE.fetch_add(1, Ordering::AcqRel);
        }
    }
}

/// Release address space `cr3`'s shim-data buffer on process exit.
pub fn free_shim_data(cr3: u64) {
    if cr3 == 0 {
        return;
    }
    let mut sl = SHIM_SLOTS.lock();
    for s in sl.iter_mut() {
        if s.in_use && s.cr3 == cr3 {
            s.in_use = false;
            s.cr3 = 0;
            SHIM_ACTIVE.fetch_sub(1, Ordering::AcqRel);
            break;
        }
    }
}

/// On a context switch between address spaces, save the running (shared) shim
/// data into the outgoing process's buffer and restore the incoming process's.
/// A process without a buffer (the kernel, self-test workers) simply shares the
/// pages, exactly as before. Called from the scheduler at DISPATCH_LEVEL.
pub fn swap_shim_data(out_cr3: u64, in_cr3: u64) {
    if out_cr3 == in_cr3 || SHIM_ACTIVE.load(Ordering::Acquire) == 0 {
        return;
    }
    let sd = SHIM_DATA.lock();
    if sd.n == 0 {
        return;
    }
    let mut sl = SHIM_SLOTS.lock_at_dpc_level();
    let out_idx = if out_cr3 != 0 {
        sl.iter().position(|s| s.in_use && s.cr3 == out_cr3)
    } else {
        None
    };
    let in_idx = if in_cr3 != 0 {
        sl.iter().position(|s| s.in_use && s.cr3 == in_cr3)
    } else {
        None
    };
    if out_idx.is_none() && in_idx.is_none() {
        return;
    }
    // The shim pages are user-accessible; bracket the supervisor copies.
    crate::mm::virt::user_access_begin();
    if let Some(oi) = out_idx {
        for k in 0..sd.n {
            let r = sd.regions[k];
            unsafe {
                core::ptr::copy_nonoverlapping(
                    r.va as *const u8,
                    sl[oi].data[r.snap_off..].as_mut_ptr(),
                    r.len,
                );
            }
        }
    }
    if let Some(ii) = in_idx {
        for k in 0..sd.n {
            let r = sd.regions[k];
            unsafe {
                core::ptr::copy_nonoverlapping(
                    sl[ii].data[r.snap_off..].as_ptr(),
                    r.va as *mut u8,
                    r.len,
                );
            }
        }
    }
    crate::mm::virt::user_access_end();
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
