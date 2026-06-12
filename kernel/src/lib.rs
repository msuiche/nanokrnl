//! # ntoskrnl-rs — an NT-compatible kernel written in Rust
//!
//! This crate implements the core of a Windows-NT-architecture kernel:
//! the same subsystem decomposition, the same fundamental abstractions
//! (IRQLs, dispatcher objects, the object manager, tagged pool, IRPs),
//! and — where it matters for compatibility — the same constants and
//! structure layouts (NTSTATUS values, `LIST_ENTRY`, `UNICODE_STRING`,
//! the x64 GDT selector layout, the IRQL↔TPR mapping).
//!
//! ## Subsystem map (mirrors the real ntoskrnl source layout)
//!
//! | Module | NT prefix | Responsibility |
//! |--------|-----------|----------------|
//! | [`rtl`] | `Rtl*` | Run-time library: lists, strings, bitmaps, NTSTATUS |
//! | [`ke`]  | `Ke*`/`Ki*` | Kernel core: IRQL, spinlocks, dispatcher objects, DPCs, threads, scheduler |
//! | [`mm`]  | `Mm*` | Memory manager: physical pages (PFN), page tables, pool |
//! | [`ex`]  | `Ex*` | Executive: tagged pool API, support routines |
//! | [`ob`]  | `Ob*` | Object manager: object headers, types, handles |
//! | [`ps`]  | `Ps*` | Process/thread management: EPROCESS/ETHREAD |
//! | [`io`]  | `Io*` | I/O manager: driver/device objects, IRPs |
//! | [`hal`] | `Hal*` | Hardware abstraction: serial, PIC/APIC, timers |
//!
//! ## Boot flow
//!
//! The `bootloader` crate (our winload stand-in) drops us in 64-bit long
//! mode with all physical memory mapped at a fixed virtual offset and hands
//! us a `BootInfo` — the moral equivalent of NT's `LOADER_PARAMETER_BLOCK`.
//! From there:
//!
//! 1. **Phase 0** (`init::ki_system_startup`): serial debug output,
//!    GDT/TSS, IDT, KPCR, memory manager, interrupt controllers.
//!    Single-threaded, interrupts disabled.
//! 2. **Phase 1**: scheduler starts, system threads are created, the
//!    built-in self tests run, and the boot processor becomes the idle
//!    thread.
//!
//! ## Host-side testing
//!
//! Architecture-independent subsystems (`rtl`, the dispatcher logic in
//! `ke`, the pool internals in `mm`) build and unit-test on the host
//! (`cargo test`); everything that touches x86_64 hardware is gated behind
//! `#[cfg(target_arch = "x86_64")]`.

// Freestanding when built for the kernel target; normal std crate when the
// host runs `cargo test` so the data-structure tests can use libtest.
#![cfg_attr(not(test), no_std)]
#![allow(dead_code)]

extern crate alloc;

pub mod ex;
pub mod hal;
pub mod ke;
pub mod mm;
pub mod rtl;

// Ob/Ps/Io/Ldr sit on top of the scheduler and pool, which only build for
// the kernel target; the layers beneath them are host-testable.
#[cfg(target_arch = "x86_64")]
pub mod cm;
#[cfg(target_arch = "x86_64")]
pub mod io;
#[cfg(target_arch = "x86_64")]
pub mod ldr;
#[cfg(target_arch = "x86_64")]
pub mod ob;
#[cfg(target_arch = "x86_64")]
pub mod ps;
#[cfg(target_arch = "x86_64")]
pub mod syscalls;

#[cfg(target_arch = "x86_64")]
pub mod init;

// ---------------------------------------------------------------------------
// Panic handler: route Rust panics through KeBugCheck
// ---------------------------------------------------------------------------

/// Rust panics are kernel bugs; treat them exactly like NT treats a fatal
/// inconsistency: raise to HIGH_LEVEL, dump state on the debug port, halt.
/// Only compiled into the freestanding kernel build — host test builds get
/// std's unwinding panics, which is what libtest needs.
#[cfg(all(target_arch = "x86_64", not(test)))]
#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    ke::bugcheck::ke_bug_check_panic(info)
}
