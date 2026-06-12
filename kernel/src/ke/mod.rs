//! # Ke — the Kernel core
//!
//! The lowest layer of the executive: everything the rest of the system
//! needs to *not know* it is running on shared CPUs. `Ke` owns
//!
//! * the **IRQL** model ([`irql`]) — software interrupt priorities mapped
//!   onto the APIC task-priority register, exactly as on x64 Windows;
//! * **spinlocks** ([`spinlock`]) — `KSPIN_LOCK`, acquired at
//!   `DISPATCH_LEVEL`;
//! * **dispatcher objects** ([`dispatcher`]) — events, semaphores, and the
//!   wait machinery (`KeWaitForSingleObject`);
//! * **DPCs** ([`dpc`]) — deferred procedure calls run at `DISPATCH_LEVEL`;
//! * **threads and the scheduler** ([`thread`], [`scheduler`]) — `KTHREAD`,
//!   32 priority ready queues, context switching;
//! * the **KPCR** ([`pcr`]) — per-processor control region reached through
//!   `GS`, holding current-thread state like the real `KPCR`/`KPRCB`;
//! * **bugchecks** ([`bugcheck`]) — `KeBugCheckEx`, the controlled crash.

pub mod bugcheck;
pub mod irql;
pub mod selectors;
pub mod spinlock;

// The dispatcher/scheduler complex blocks and switches real CPU contexts,
// so it only builds for the kernel target; host tests cover the
// arch-independent layers beneath it (lists, IRQL rules, spinlocks).
#[cfg(target_arch = "x86_64")]
pub mod debug;
#[cfg(target_arch = "x86_64")]
pub mod dispatcher;
#[cfg(target_arch = "x86_64")]
pub mod dpc;
#[cfg(target_arch = "x86_64")]
pub mod gdt;
#[cfg(target_arch = "x86_64")]
pub mod idt;
#[cfg(target_arch = "x86_64")]
pub mod pcr;
#[cfg(target_arch = "x86_64")]
pub mod scheduler;
#[cfg(target_arch = "x86_64")]
pub mod syscall;
#[cfg(target_arch = "x86_64")]
pub mod thread;
#[cfg(target_arch = "x86_64")]
pub mod traps;
#[cfg(target_arch = "x86_64")]
pub mod usermode;
