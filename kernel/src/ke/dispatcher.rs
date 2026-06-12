//! Dispatcher objects — events, semaphores, timers, and waiting.
//!
//! Anything a thread can wait on in NT begins with a `DISPATCHER_HEADER`:
//! a type tag, a signal state, and a list of waiting threads. `KEVENT`,
//! `KSEMAPHORE`, `KTIMER`, `KTHREAD`, `KMUTANT`… all share this prefix,
//! which is what lets one routine — `KeWaitForSingleObject` — wait on any
//! of them.
//!
//! All signal-state and wait-list manipulation happens under the single
//! global **dispatcher lock** at `DISPATCH_LEVEL` (NT pre-Win7 semantics;
//! modern NT shards the lock per-object, an optimization that does not
//! change this API). The lock and the block/unblock machinery live in
//! [`super::scheduler`]; this module is the object layer on top.

use crate::ke::irql;
use crate::ke::scheduler;
use crate::rtl::list::ListEntry;
use crate::rtl::NtStatus;

/// Discriminates what a `DispatcherHeader` is embedded in, and with it the
/// signal/satisfy semantics (auto-reset vs sticky vs counted).
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DispatcherObjectType {
    /// Sticky: stays signaled until explicitly reset; wakes *all* waiters.
    NotificationEvent = 0,
    /// Auto-reset: wakes *one* waiter and consumes the signal.
    SynchronizationEvent = 1,
    /// Mutual exclusion with recursive ownership: signaled (free) when the
    /// count is +1, owned when <= 0. Only the owner may release.
    Mutant = 2,
    /// Counted: each release adds, each satisfied wait subtracts.
    Semaphore = 5,
    /// Signaled forever once the thread terminates.
    Thread = 6,
    /// Signaled when the due tick arrives (notification semantics).
    Timer = 8,
}

/// The common prefix of every waitable object — `DISPATCHER_HEADER`.
#[repr(C)]
pub struct DispatcherHeader {
    pub object_type: DispatcherObjectType,
    /// >0 means signaled. Events use 0/1, semaphores their count.
    pub signal_state: i32,
    /// Threads blocked on this object (links `KwaitBlock`s).
    pub wait_list: ListEntry,
    /// Wait lists are lazily initialized on first use (a `const fn` cannot
    /// self-link); this tracks that.
    pub wait_list_init: bool,
}

impl DispatcherHeader {
    pub const fn new(object_type: DispatcherObjectType, signal_state: i32) -> Self {
        DispatcherHeader {
            object_type,
            signal_state,
            wait_list: ListEntry::new(),
            wait_list_init: false,
        }
    }

    /// Self-link the wait list head on first use. Caller holds the
    /// dispatcher lock, and the header must be pinned.
    pub(super) unsafe fn ensure_wait_list(&mut self) {
        if !self.wait_list_init {
            unsafe { self.wait_list.init() };
            self.wait_list_init = true;
        }
    }
}

// ---------------------------------------------------------------------------
// KEVENT
// ---------------------------------------------------------------------------

/// `KEVENT` — the workhorse synchronization object.
#[repr(C)]
pub struct Kevent {
    pub header: DispatcherHeader,
}

impl Kevent {
    /// `KeInitializeEvent`.
    pub const fn new(object_type: DispatcherObjectType, initially_signaled: bool) -> Self {
        Kevent {
            header: DispatcherHeader::new(object_type, if initially_signaled { 1 } else { 0 }),
        }
    }

    /// `KeSetEvent` — signal, waking waiters per the event's semantics.
    /// Returns the previous signal state, like the NT API.
    ///
    /// # Safety
    /// `self` must be pinned (lists hold its address) — guaranteed for
    /// pool/static allocations, which is where events live.
    pub unsafe fn set(&mut self) -> i32 {
        unsafe { scheduler::ki_signal_object(&mut self.header) }
    }

    /// `KeResetEvent` — return to non-signaled; nobody is woken.
    pub fn reset(&mut self) -> i32 {
        scheduler::ki_reset_object(&mut self.header)
    }

    /// `KeReadStateEvent`.
    pub fn read_state(&self) -> i32 {
        // Racy-read by design, exactly like the NT export: the value may
        // be stale the instant it is returned; only useful for heuristics.
        unsafe { core::ptr::addr_of!(self.header.signal_state).read_volatile() }
    }
}

// ---------------------------------------------------------------------------
// KSEMAPHORE
// ---------------------------------------------------------------------------

/// `KSEMAPHORE` — counted dispatcher object.
#[repr(C)]
pub struct Ksemaphore {
    pub header: DispatcherHeader,
    /// Maximum count; releasing beyond it is a caller bug (NT raises
    /// `STATUS_SEMAPHORE_LIMIT_EXCEEDED`; we return it).
    pub limit: i32,
}

impl Ksemaphore {
    /// `KeInitializeSemaphore`.
    pub const fn new(initial: i32, limit: i32) -> Self {
        Ksemaphore {
            header: DispatcherHeader::new(DispatcherObjectType::Semaphore, initial),
            limit,
        }
    }

    /// `KeReleaseSemaphore` — add `adjustment` to the count, waking up to
    /// that many waiters.
    ///
    /// # Safety
    /// `self` must be pinned, as with [`Kevent::set`].
    pub unsafe fn release(&mut self, adjustment: i32) -> Result<i32, NtStatus> {
        unsafe { scheduler::ki_release_semaphore(self, adjustment) }
    }
}

// ---------------------------------------------------------------------------
// KMUTANT (a.k.a. KMUTEX)
// ---------------------------------------------------------------------------

/// `KMUTANT` — recursive mutual-exclusion dispatcher object.
///
/// Unlike a binary semaphore, a mutant tracks an *owner* and a recursion
/// count: the owning thread may acquire it again without blocking, and must
/// release it once per acquire. `signal_state == 1` means free; each
/// acquire decrements (0, -1, …) and each release increments back. Only the
/// owner may release (`STATUS_MUTANT_NOT_OWNED` otherwise).
#[repr(C)]
pub struct Kmutant {
    pub header: DispatcherHeader,
    /// The thread that currently owns it, or null when free.
    pub owner: *mut crate::ke::thread::Kthread,
}

// The owner pointer is only read/written under the dispatcher lock, so the
// mutant is safe to place in a `static` (same reasoning as `ListEntry`).
unsafe impl Send for Kmutant {}
unsafe impl Sync for Kmutant {}

impl Kmutant {
    /// `KeInitializeMutant` — created free (signaled), unowned.
    pub const fn new() -> Self {
        Kmutant {
            header: DispatcherHeader::new(DispatcherObjectType::Mutant, 1),
            owner: core::ptr::null_mut(),
        }
    }

    /// `KeReleaseMutant` — give up one level of ownership.
    ///
    /// # Safety
    /// `self` must be pinned.
    pub unsafe fn release(&mut self) -> Result<i32, NtStatus> {
        unsafe { scheduler::ki_release_mutant(self as *mut Kmutant) }
    }
}

// ---------------------------------------------------------------------------
// KTIMER
// ---------------------------------------------------------------------------

/// `KTIMER` — signaled when its due tick is reached. One-shot for now
/// (periodic timers re-arm via their expiry DPC on NT; straightforward
/// extension once needed).
#[repr(C)]
pub struct Ktimer {
    pub header: DispatcherHeader,
    /// Absolute `KeTickCount` value at which the timer fires.
    pub due_tick: u64,
    /// Linkage in the active-timer list (scheduler-owned).
    pub timer_list_entry: ListEntry,
    /// True while linked in the active list.
    pub inserted: bool,
    /// Optional DPC queued when the timer expires (`KeSetTimer`'s Dpc arg).
    /// Null for a plain waitable timer (`KeWaitForSingleObject(timer)`).
    pub dpc: *mut crate::ke::dpc::Kdpc,
}

impl Ktimer {
    /// `KeInitializeTimer`.
    pub const fn new() -> Self {
        Ktimer {
            header: DispatcherHeader::new(DispatcherObjectType::Timer, 0),
            due_tick: 0,
            timer_list_entry: ListEntry::new(),
            inserted: false,
            dpc: core::ptr::null_mut(),
        }
    }
}

// ---------------------------------------------------------------------------
// Waiting
// ---------------------------------------------------------------------------

/// `KeWaitForSingleObject` — block until `object` is signaled.
///
/// Must be called at IRQL < DISPATCH_LEVEL (you cannot block the
/// dispatcher itself — same rule, same reason as NT). Satisfying the wait
/// consumes the signal according to the object type (auto-reset events
/// clear, semaphores decrement, notification events/threads stay).
///
/// # Safety
/// `object` must be a pinned, initialized dispatcher object.
pub unsafe fn ke_wait_for_single_object(object: *mut DispatcherHeader) -> NtStatus {
    debug_assert!(
        irql::ke_get_current_irql() < irql::DISPATCH_LEVEL,
        "KeWaitForSingleObject at DISPATCH_LEVEL or above"
    );
    unsafe { scheduler::ki_wait_for_object(object, None) }
}

/// `KeWaitForSingleObject` with a timeout in clock ticks. Returns
/// `STATUS_TIMEOUT` if the object did not signal in time; `Some(0)` polls.
///
/// # Safety
/// `object` must be a pinned, initialized dispatcher object.
pub unsafe fn ke_wait_for_single_object_timeout(
    object: *mut DispatcherHeader,
    timeout_ticks: Option<u64>,
) -> NtStatus {
    debug_assert!(irql::ke_get_current_irql() < irql::DISPATCH_LEVEL);
    unsafe { scheduler::ki_wait_for_object(object, timeout_ticks) }
}

/// `KeWaitForMultipleObjects` — block until *all* (`wait_all`) or *any*
/// (`!wait_all`) of `objects` are signaled, or the optional timeout fires.
///
/// Returns `WAIT_0 + index` of the satisfying object for a "wait any",
/// `WAIT_0` for a satisfied "wait all", or `STATUS_TIMEOUT`. At most
/// [`THREAD_WAIT_OBJECTS`](crate::ke::thread::THREAD_WAIT_OBJECTS) objects.
///
/// # Safety
/// Every object pinned & initialized.
pub unsafe fn ke_wait_for_multiple_objects(
    objects: &[*mut DispatcherHeader],
    wait_all: bool,
    timeout_ticks: Option<u64>,
) -> NtStatus {
    debug_assert!(irql::ke_get_current_irql() < irql::DISPATCH_LEVEL);
    let wait_type = if wait_all {
        crate::ke::thread::WaitType::All
    } else {
        crate::ke::thread::WaitType::Any
    };
    unsafe { scheduler::ki_wait_for_objects(objects, wait_type, timeout_ticks) }
}

/// `KeWaitForMutexObject` / wait-on-mutant: acquire the mutant, blocking
/// until it is free (or already owned by this thread, which acquires
/// recursively). Pair each successful wait with a [`Kmutant::release`].
///
/// # Safety
/// `mutant` must be pinned & initialized.
pub unsafe fn ke_wait_for_mutant(mutant: *mut Kmutant) -> NtStatus {
    debug_assert!(irql::ke_get_current_irql() < irql::DISPATCH_LEVEL);
    unsafe { scheduler::ki_wait_for_object(&raw mut (*mutant).header, None) }
}

/// `KeDelayExecutionThread` — sleep for `ticks` clock ticks (~1 ms each).
pub fn ke_delay_execution_thread(ticks: u64) -> NtStatus {
    debug_assert!(irql::ke_get_current_irql() < irql::DISPATCH_LEVEL);
    scheduler::ki_delay_thread(ticks)
}
