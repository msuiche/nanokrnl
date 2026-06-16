//! `ke` — kernel core for the WASM build. Phase 1 provides the `SpinLock`
//! surface (see [`spinlock`]); the x86 machinery (GDT/IDT, traps, the register
//! context switch, syscall entry) has no WASM analogue and is out of scope until
//! a later phase.
pub mod spinlock;
