//! Kernel APCs — `KAPC`, `KeInitializeApc`, `KeInsertQueueApc`, and delivery.
//!
//! NT's asynchronous procedure call targets a *specific thread*: the object
//! queues onto that thread's APC list and runs its routines in that thread's
//! context the next time it runs at `APC_LEVEL` — the machinery behind
//! thread suspension, `SetThreadContext`, and special I/O-completion APCs.
//! This is the normal-kernel-APC half of the model: a `KernelRoutine` (runs
//! first and may free the object) and a `NormalRoutine(context, arg1, arg2)`,
//! both delivered at `APC_LEVEL` on the target thread.
//!
//! Delivery rides the dispatch interrupt: `ki_dispatch_interrupt` runs in
//! the interrupted thread's context at `DISPATCH_LEVEL` on every preemption
//! tick, so pending KAPCs deliver there (lowered to `APC_LEVEL`, dispatcher
//! lock untouched). On a single CPU that is exactly NT's "next time the
//! thread runs at `APC_LEVEL`" — reentrancy from a nested clock tick is cut
//! by a per-CPU flag.

use super::irql::{self, APC_LEVEL};
use super::pcr;
use super::thread::Kthread;
use crate::container_of;
use crate::rtl::list::ListEntry;
use core::sync::atomic::{AtomicBool, Ordering};

/// `PKERNEL_ROUTINE` — the teardown hook, runs first at `APC_LEVEL`. (NT's
/// five-argument freeform is collapsed to just the APC; everything else is
/// reachable from it.)
pub type KapcKernelRoutine = unsafe fn(*mut Kapc);
/// `PKNORMAL_ROUTINE` — the payload: `(context, arg1, arg2)`.
pub type KapcNormalRoutine = unsafe fn(u64, u64, u64);

/// `KAPC` — a normal kernel APC, NT's `KAPC` in spirit (the full NT struct
/// adds the rundown routine, APC mode and state index, which arrive with
/// suspension and special APCs).
#[repr(C)]
pub struct Kapc {
    /// Linkage in the target thread's pending queue (`Kthread::kapc_queue`).
    pub entry: ListEntry,
    /// Set while queued (guards double-insert; `KAPC.Inserted`).
    inserted: AtomicBool,
    kernel_routine: Option<KapcKernelRoutine>,
    normal_routine: Option<KapcNormalRoutine>,
    normal_context: u64,
    arg1: u64,
    arg2: u64,
    /// The thread this APC targets, recorded at initialize for the insert.
    thread: *mut Kthread,
}

impl Kapc {
    /// A zeroed APC, ready for [`ke_initialize_apc`]. (Static or pool storage;
    /// NT lets the caller own the object either way.)
    pub const fn new() -> Self {
        Kapc {
            entry: ListEntry::new(),
            inserted: AtomicBool::new(false),
            kernel_routine: None,
            normal_routine: None,
            normal_context: 0,
            arg1: 0,
            arg2: 0,
            thread: core::ptr::null_mut(),
        }
    }
}

/// The thread's queue head, lazily self-linked on first touch: a
/// `ListEntry::new()` field starts null-linked, and `init` needs `&mut
/// self`, which `Kthread::new` doesn't have for its own fields.
///
/// # Safety
/// `thread` live.
unsafe fn queue_of(thread: *mut Kthread) -> *mut ListEntry {
    let q = &raw mut (*thread).kapc_queue;
    unsafe {
        if (*q).flink.is_null() {
            (*q).init();
        }
    }
    q
}

/// `KeInitializeApc` — prepare `apc` for `thread`. The kernel routine runs
/// first at delivery and may free the object; the normal routine is the
/// payload. (NT also takes the APC environment/state index and a rundown
/// routine — normal kernel APCs don't need them.)
///
/// # Safety
/// `apc` must be valid until delivered (or cancelled by the owner).
pub unsafe fn ke_initialize_apc(
    apc: *mut Kapc,
    thread: *mut Kthread,
    kernel_routine: Option<KapcKernelRoutine>,
    normal_routine: Option<KapcNormalRoutine>,
    normal_context: u64,
    arg1: u64,
    arg2: u64,
) {
    unsafe {
        (*apc).entry = ListEntry::new();
        (*apc).inserted.store(false, Ordering::Release);
        (*apc).kernel_routine = kernel_routine;
        (*apc).normal_routine = normal_routine;
        (*apc).normal_context = normal_context;
        (*apc).arg1 = arg1;
        (*apc).arg2 = arg2;
        (*apc).thread = thread;
    }
}

/// `KeInsertQueueApc` — queue a prepared APC to its target thread. Returns
/// false on a double-insert (the APC is already queued somewhere) or a null
/// target; the caller keeps ownership in that case.
///
/// # Safety
/// `apc` initialized; `thread` live.
pub unsafe fn ke_insert_queue_apc(apc: *mut Kapc, thread: *mut Kthread) -> bool {
    if thread.is_null() || unsafe { (*apc).thread != thread } {
        return false;
    }
    // Guard the queue with the dispatcher lock, as every thread-state
    // structure here is.
    let old = crate::ke::scheduler::dispatcher_lock();
    let queued = unsafe {
        if (*apc).inserted.swap(true, Ordering::AcqRel) {
            false
        } else {
            let q = queue_of(thread);
            (*q).insert_tail(&raw mut (*apc).entry);
            true
        }
    };
    crate::ke::scheduler::dispatcher_unlock(old);
    queued
}

/// Drain the **current** thread's pending kernel APCs at `APC_LEVEL`. Runs
/// each APC's kernel routine (if any) then its normal routine, in queue
/// order. Called from the dispatch interrupt's return path — thread context,
/// `DISPATCH_LEVEL`, no locks held. Reentrancy (a clock tick landing
/// mid-delivery and re-entering through a nested dispatch) is cut by the
/// per-CPU guard: the inner call simply leaves the queue for the outer one.
pub fn ki_deliver_apcs() {
    // Re-entrancy guard, **per CPU**: delivery can nest on one processor
    // (an APC routine that blocks re-enters delivery on its resume), while
    // concurrent deliveries on different CPUs are legitimate and must not
    // suppress each other — under SMP a global guard drops deliveries.
    static DELIVERING: [AtomicBool; crate::ke::smp::MAX_CPUS] =
        [const { AtomicBool::new(false) }; crate::ke::smp::MAX_CPUS];
    let cpu = pcr::ke_get_prcb().number as usize;
    let Some(slot) = DELIVERING.get(cpu) else { return };
    if slot.swap(true, Ordering::AcqRel) {
        return;
    }
    // Move to APC_LEVEL from either side: the dispatch vector enters with
    // CR8 wherever the interrupted code left it (PASSIVE in practice), and
    // the raise/lower asserts are strict about direction.
    let entry = irql::ke_get_current_irql();
    if entry < APC_LEVEL {
        irql::ke_raise_irql(APC_LEVEL);
    } else if entry > APC_LEVEL {
        irql::ke_lower_irql(APC_LEVEL);
    }
    loop {
        let cur = pcr::ke_get_current_thread();
        let old = crate::ke::scheduler::dispatcher_lock();
        let entry = unsafe {
            let q = queue_of(cur);
            (*q).remove_head()
        };
        crate::ke::scheduler::dispatcher_unlock(old);
        let Some(entry) = entry else { break };
        let apc = unsafe { container_of!(entry, Kapc, entry) };
        unsafe {
            (*apc).inserted.store(false, Ordering::Release);
            if let Some(kr) = (*apc).kernel_routine {
                kr(apc);
            }
            if let Some(nr) = (*apc).normal_routine {
                nr((*apc).normal_context, (*apc).arg1, (*apc).arg2);
            }
        }
    }
    // Restore the entry level.
    if entry < APC_LEVEL {
        irql::ke_lower_irql(entry);
    } else if entry > APC_LEVEL {
        irql::ke_raise_irql(entry);
    }
    slot.store(false, Ordering::Release);
}
