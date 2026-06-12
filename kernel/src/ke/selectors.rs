//! NT x64 GDT selector constants — the canonical, architecture-neutral
//! definitions (`KGDT64_*`).
//!
//! These are *ABI*: NT's trap frames, `syscall`/`sysret` MSR programming,
//! and the debugger all assume this exact arrangement. They live in their
//! own un-gated module (rather than inside the x86_64-only `gdt`) for one
//! concrete reason — the layout conformance tests assert them on the host,
//! and the host target is not x86_64. `gdt` re-exports them.
//!
//! See `ke::gdt` for how each descriptor is built and why the ordering of
//! the user selectors (0x20/0x28/0x30) is mandated by `syscall`/`sysret`.

/// Null descriptor.
pub const KGDT64_NULL: u16 = 0x00;
/// Kernel-mode 64-bit code (CS in kernel).
pub const KGDT64_R0_CODE: u16 = 0x10;
/// Kernel-mode data (SS in kernel).
pub const KGDT64_R0_DATA: u16 = 0x18;
/// User-mode 32-bit code (WoW64 compatibility-mode CS).
pub const KGDT64_R3_CMCODE: u16 = 0x20;
/// User-mode data (user SS, RPL 3).
pub const KGDT64_R3_DATA: u16 = 0x28;
/// User-mode 64-bit code (user CS, RPL 3).
pub const KGDT64_R3_CODE: u16 = 0x30;
/// The TSS (a 16-byte system descriptor, so it consumes two GDT slots).
pub const KGDT64_SYS_TSS: u16 = 0x40;

/// Requested Privilege Level OR'd into user selectors when loaded.
pub const RPL_USER: u16 = 3;
