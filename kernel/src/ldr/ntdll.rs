//! User-mode `ntdll` shim — the ring-3 system-call trampoline.
//!
//! On real Windows, a user program doesn't issue `syscall` itself: it calls
//! `ntdll!NtWriteFile`, a thin exported stub that loads the service number
//! and executes `syscall`. We provide the same shape so a console app can be
//! built against an `ntdll` import library and run **unmodified** (no inline
//! assembly): the loader binds the app's imports to the stubs built here.
//!
//! The stubs live in a single user-accessible, executable page the kernel
//! builds at init. Each is the canonical Windows syscall thunk:
//!
//! ```text
//!   mov r10, rcx     ; arg1 (RCX is clobbered by `syscall`, so move it)
//!   mov eax, <svc>   ; service number
//!   syscall
//!   ret
//! ```
//!
//! The app calls them with the normal Microsoft x64 convention
//! (`rcx, rdx, r8, r9`); the stub turns that into our syscall ABI
//! (`r10, rdx, r8, r9` + `eax`). [`resolve_import`] maps an imported name to
//! its stub address, for the loader's import binding.

use crate::syscalls;
use core::sync::atomic::{AtomicU64, Ordering};

/// Bytes per stub slot (the 11-byte thunk, padded to 16 for clean indexing).
const STUB_STRIDE: usize = 16;

/// Exported name → system-service number. The PE loader resolves a user
/// app's imports through this table; the stub for service `n` lives at
/// `base + n * STUB_STRIDE`.
static EXPORTS: &[(&str, usize)] = &[
    ("NtTerminateThread", syscalls::SVC_EXIT_THREAD),
    ("NtWriteFile", syscalls::SVC_NT_WRITE_FILE),
    ("NtCreateFile", syscalls::SVC_NT_CREATE_FILE),
    ("NtClose", syscalls::SVC_NT_CLOSE),
    ("NtReadFile", syscalls::SVC_NT_READ_FILE),
    ("NtAllocateVirtualMemory", syscalls::SVC_NT_ALLOCATE_VIRTUAL_MEMORY),
    ("NtFreeVirtualMemory", syscalls::SVC_NT_FREE_VIRTUAL_MEMORY),
    ("NtProtectVirtualMemory", syscalls::SVC_NT_PROTECT_VIRTUAL_MEMORY),
];

/// Base VA of the trampoline page (0 until [`init`] runs).
static TRAMPOLINE_BASE: AtomicU64 = AtomicU64::new(0);

/// Build the ring-3 syscall-stub page. Phase-1, single-threaded. Allocates a
/// user-accessible executable page and writes one stub per service number
/// used by [`EXPORTS`].
pub fn init() {
    let pa = crate::mm::phys::mm_allocate_page().expect("ntdll trampoline page");
    let va = crate::mm::phys_to_virt(pa);

    // Highest service index we need a stub for.
    let max_svc = EXPORTS.iter().map(|&(_, n)| n).max().unwrap_or(0);
    assert!((max_svc + 1) * STUB_STRIDE <= crate::mm::PAGE_SIZE, "too many stubs for one page");

    for svc in 0..=max_svc {
        let off = svc * STUB_STRIDE;
        // mov r10, rcx ; mov eax, <svc> ; syscall ; ret  (3 + 5 + 2 + 1 = 11 bytes)
        let stub: [u8; 10] = [
            0x49, 0x89, 0xCA, // mov r10, rcx
            0xB8, // mov eax, imm32
            (svc & 0xFF) as u8,
            ((svc >> 8) & 0xFF) as u8,
            ((svc >> 16) & 0xFF) as u8,
            ((svc >> 24) & 0xFF) as u8,
            0x0F, 0x05, // syscall
        ];
        // SAFETY: page is freshly allocated and exclusively ours.
        unsafe {
            core::ptr::copy_nonoverlapping(stub.as_ptr(), va.add(off), stub.len());
            *va.add(off + 10) = 0xC3; // ret
        }
    }

    // Make the whole page user-accessible + executable.
    unsafe { crate::mm::virt::mm_set_user_executable(va as u64, crate::mm::PAGE_SIZE) };
    TRAMPOLINE_BASE.store(va as u64, Ordering::Release);
}

/// Base VA of the trampoline page (the value Win32 treats as ntdll's
/// `HMODULE`); 0 until [`init`] runs.
pub fn trampoline_base() -> u64 {
    TRAMPOLINE_BASE.load(Ordering::Acquire)
}

/// Resolve an imported `ntdll` name to its ring-3 stub address. Used by the
/// PE loader (`load_user`) to bind a console app's imports.
pub fn resolve_import(name: &str) -> Option<usize> {
    let base = TRAMPOLINE_BASE.load(Ordering::Acquire);
    if base == 0 {
        return None; // init() hasn't run
    }
    EXPORTS
        .iter()
        .find(|&&(n, _)| n == name)
        .map(|&(_, svc)| (base as usize) + svc * STUB_STRIDE)
}

/// Names this shim exports — used to generate the driver-side `ntdll.lib`
/// import library in the build script.
pub fn export_names() -> impl Iterator<Item = &'static str> {
    EXPORTS.iter().map(|&(n, _)| n)
}
