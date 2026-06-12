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
use crate::rtl::NtStatus;
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

static KERNEL32_BASE: AtomicU64 = AtomicU64::new(0);
static KERNEL32_SIZE: AtomicUsize = AtomicUsize::new(0);
static MSVCRT_BASE: AtomicU64 = AtomicU64::new(0);
static MSVCRT_SIZE: AtomicUsize = AtomicUsize::new(0);

/// Load the `kernel32` shim DLL into user-accessible memory. It has no
/// imports of its own (its functions issue syscalls inline), so loading is a
/// plain `load_user`. Phase-1, before any console app is loaded.
pub fn load_kernel32(image: &[u8]) -> Result<(), NtStatus> {
    let loaded = pe::load_user(image)?;
    KERNEL32_BASE.store(loaded.base as u64, Ordering::Release);
    KERNEL32_SIZE.store(loaded.size, Ordering::Release);
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
    crate::kd_println!(
        "LDR: loaded msvcrt.dll @ {:p} ({} bytes)",
        loaded.base,
        loaded.size
    );
    Ok(())
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
const UNRESOLVED_MAX: usize = 384;
const UNRESOLVED_STRIDE: usize = 8;
static UNRESOLVED_PAGE: AtomicU64 = AtomicU64::new(0);
static UNRESOLVED_COUNT: AtomicUsize = AtomicUsize::new(0);

/// Assign a distinct return-0 stub to an unresolved import `name`, logging the
/// mapping. Returns the stub's VA (or `None` if the page is full / unallocated).
pub fn unresolved_stub(name: &str) -> Option<usize> {
    // Lazily build a user-executable page of `xor eax,eax; ret` stubs.
    let mut base = UNRESOLVED_PAGE.load(Ordering::Acquire);
    if base == 0 {
        let pa = crate::mm::phys::mm_allocate_page()?;
        let va = crate::mm::phys_to_virt(pa) as u64;
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
        // Out of slots: fall back to the shared stub.
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
    None
}
