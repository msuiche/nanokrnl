//! DPCs — Deferred Procedure Calls.
//!
//! Interrupt service routines must be short: they run at a device IRQL
//! masking everything below. A DPC is the standard NT escape hatch — the
//! ISR queues a callback and the processor runs it later at
//! `DISPATCH_LEVEL`, when device interrupts are open again but the
//! scheduler is still held off.
//!
//! ## Locking
//!
//! The DPC queue is touched from *any* IRQL (the clock ISR at
//! `CLOCK_LEVEL` queues DPCs), so a `DISPATCH_LEVEL` spinlock cannot
//! protect it — the clock interrupt could interrupt the lock holder on
//! the same CPU and spin forever. Like NT, the queue lock is therefore
//! taken with **interrupts disabled** (effectively `HIGH_LEVEL`), held
//! only for the queue link/unlink instructions.

use crate::ke::irql;
use crate::rtl::list::ListEntry;
use core::sync::atomic::{AtomicBool, Ordering};

/// DPC callback: receives the DPC itself plus the caller's context.
pub type DpcRoutine = fn(dpc: *mut Kdpc, context: *mut core::ffi::c_void);

/// `KDPC` — a queueable deferred call. Embedded in driver/kernel
/// structures and pinned, like every intrusive object here.
#[repr(C)]
pub struct Kdpc {
    pub list_entry: ListEntry,
    /// In-kernel callback (Rust ABI). Used by kernel-internal DPCs.
    pub routine: Option<DpcRoutine>,
    /// Driver callback (`KDEFERRED_ROUTINE`, Microsoft x64 ABI). Set when a
    /// loaded driver initializes this DPC via the `KeInitializeDpc` export;
    /// takes precedence over `routine`.
    pub win64_routine: Option<ntabi::KdeferredRoutine>,
    pub context: *mut core::ffi::c_void,
    /// System arguments passed to a win64 routine (set by KeInsertQueueDpc).
    pub system_arg1: *mut core::ffi::c_void,
    pub system_arg2: *mut core::ffi::c_void,
    /// Guards against double-queueing (KeInsertQueueDpc returns FALSE on
    /// an already-queued DPC rather than corrupting the list).
    pub inserted: bool,
}

impl Kdpc {
    /// `KeInitializeDpc` for an in-kernel (Rust ABI) callback.
    pub const fn new(routine: DpcRoutine, context: *mut core::ffi::c_void) -> Self {
        Kdpc {
            list_entry: ListEntry::new(),
            routine: Some(routine),
            win64_routine: None,
            context,
            system_arg1: core::ptr::null_mut(),
            system_arg2: core::ptr::null_mut(),
            inserted: false,
        }
    }

    /// `KeInitializeDpc` for a driver (Microsoft x64 ABI) callback.
    pub const fn new_win64(
        routine: ntabi::KdeferredRoutine,
        context: *mut core::ffi::c_void,
    ) -> Self {
        Kdpc {
            list_entry: ListEntry::new(),
            routine: None,
            win64_routine: Some(routine),
            context,
            system_arg1: core::ptr::null_mut(),
            system_arg2: core::ptr::null_mut(),
            inserted: false,
        }
    }
}

/// The boot processor's DPC queue head + its any-IRQL lock.
/// (Per-PRCB on MP NT; single queue while we are single-processor.)
static QUEUE_LOCK: AtomicBool = AtomicBool::new(false);
static mut QUEUE_HEAD: ListEntry = ListEntry::new();
static mut QUEUE_INIT: bool = false;

/// Run `f` with the queue locked and interrupts disabled. The closure gets
/// the (lazily self-linked) queue head.
fn with_queue<R>(f: impl FnOnce(&mut ListEntry) -> R) -> R {
    let rflags = irql::disable_interrupts();
    while QUEUE_LOCK
        .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
        .is_err()
    {
        core::hint::spin_loop();
    }
    // SAFETY: the flag lock + masked interrupts give exclusive access on
    // this single-processor configuration.
    let r = unsafe {
        let head = &mut *(&raw mut QUEUE_HEAD);
        if !QUEUE_INIT {
            head.init();
            QUEUE_INIT = true;
        }
        f(head)
    };
    QUEUE_LOCK.store(false, Ordering::Release);
    irql::restore_interrupts(rflags);
    r
}

/// `KeInsertQueueDpc` — queue the DPC and request a dispatch interrupt.
/// Returns false if it was already queued.
///
/// Callable from any IRQL, including ISRs — that is its entire purpose.
///
/// # Safety
/// `dpc` must be pinned and initialized.
pub unsafe fn ke_insert_queue_dpc(dpc: *mut Kdpc) -> bool {
    let inserted = with_queue(|head| unsafe {
        if (*dpc).inserted {
            return false;
        }
        (*dpc).inserted = true;
        head.insert_tail(&raw mut (*dpc).list_entry);
        true
    });
    if inserted {
        // Ask for the software interrupt that drains the queue once IRQL
        // falls below DISPATCH_LEVEL.
        crate::hal::apic::request_dispatch_interrupt();
    }
    inserted
}

/// Drain the DPC queue. Called from the dispatch interrupt at
/// `DISPATCH_LEVEL` (see `ke::traps`), mirroring `KiRetireDpcList`:
/// pop-with-lock, run *without* the lock so routines may queue more DPCs.
pub fn ki_retire_dpcs() {
    loop {
        let next = with_queue(|head| unsafe {
            head.remove_head().map(|entry| {
                let dpc = crate::container_of!(entry, Kdpc, list_entry);
                (*dpc).inserted = false;
                dpc
            })
        });
        let Some(dpc) = next else { break };
        // Run the callback without the queue lock so it may queue more DPCs.
        // Prefer the driver (win64) routine if present.
        unsafe {
            if let Some(win64) = (*dpc).win64_routine {
                win64(
                    dpc as *mut ntabi::KDpc,
                    (*dpc).context,
                    (*dpc).system_arg1,
                    (*dpc).system_arg2,
                );
            } else if let Some(routine) = (*dpc).routine {
                routine(dpc, (*dpc).context);
            }
        }
    }
}
