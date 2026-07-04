//! The dispatcher — ready queues, waits, timers, and preemption.
//!
//! This module is NT's `ki` scheduling core: the single place threads
//! block, wake, and processors pick what to run next.
//!
//! ## The dispatcher lock travels across context switches
//!
//! All scheduling state is guarded by one global spinlock held at
//! `DISPATCH_LEVEL` (classic NT design; modern NT shards it, same
//! semantics). The subtle part: a thread acquires the lock, decides to
//! block, and calls [`ki_swap_context`] *while still holding it* — the CPU
//! resumes some other thread, which finds itself owning the lock and is
//! responsible for releasing it. The invariant is:
//!
//! > **Whoever wakes up out of `ki_swap_context` owns the dispatcher lock
//! > and must release it.**
//!
//! Threads parked in a blocking call release it in their own frame right
//! after `ki_swap_context` returns; brand-new threads (whose first "wake"
//! is the forged startup frame) release it in
//! [`ki_finish_switch_to_new_thread`]. This is why the lock here is a
//! manual acquire/release pair rather than the RAII [`SpinLock`] — a guard
//! object cannot change owners mid-flight.
//!
//! ## Interrupt-level division of labor (same as NT)
//!
//! * **Clock ISR** (`CLOCK_LEVEL`, vector 0xD1): bump `KeTickCount`, burn
//!   the running thread's quantum, peek the earliest timer deadline — and
//!   if anything needs the scheduler, *request a dispatch interrupt*.
//!   Never takes the dispatcher lock (it could be interrupting the lock
//!   holder on this very CPU).
//! * **Dispatch ISR** (`DISPATCH_LEVEL`-class, vector 0x2F): drain DPCs,
//!   expire timers, handle quantum end. May take the lock — by the IRQL
//!   rules nobody it interrupted can be holding it.
//!
//! [`SpinLock`]: crate::ke::spinlock::SpinLock

use crate::ke::dispatcher::{DispatcherHeader, DispatcherObjectType, Kmutant, Ksemaphore, Ktimer};
use crate::ke::irql::{self, Kirql, DISPATCH_LEVEL, PASSIVE_LEVEL};
use crate::ke::pcr;
use crate::ke::thread::{
    ki_swap_context, Kthread, ThreadState, WaitType, DEFAULT_QUANTUM, THREAD_WAIT_OBJECTS,
    TIMER_WAIT_BLOCK,
};
use crate::ke::traps::KtrapFrame;
use crate::rtl::list::ListEntry;
use crate::rtl::NtStatus;
use crate::container_of;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// `KeTickCount` — monotone clock ticks since boot (~1 ms granularity).
pub static KE_TICK_COUNT: AtomicU64 = AtomicU64::new(0);

/// Cached earliest timer deadline so the clock ISR can test "anything
/// due?" with one load instead of taking the dispatcher lock.
static EARLIEST_TIMER_DUE: AtomicU64 = AtomicU64::new(u64::MAX);

// ---------------------------------------------------------------------------
// The dispatcher lock (manual, hand-off capable — see module docs)
// ---------------------------------------------------------------------------

static DISPATCHER_LOCK: AtomicBool = AtomicBool::new(false);

/// Raise to DISPATCH_LEVEL and take the dispatcher lock.
/// Returns the IRQL to restore at the matching [`release`].
fn acquire() -> Kirql {
    let old = irql::ke_raise_irql(DISPATCH_LEVEL);
    while DISPATCHER_LOCK
        .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
        .is_err()
    {
        core::hint::spin_loop();
    }
    old
}

/// Release the dispatcher lock and lower IRQL.
fn release(old_irql: Kirql) {
    DISPATCHER_LOCK.store(false, Ordering::Release);
    irql::ke_lower_irql(old_irql);
}

// ---------------------------------------------------------------------------
// Scheduling state (guarded by the dispatcher lock)
// ---------------------------------------------------------------------------

/// All mutable dispatcher state, in one bag so the `unsafe` access pattern
/// is uniform: touch only via [`state`], only with the lock held.
struct KiDispatcher {
    /// One FIFO ready queue per priority, like NT's `DispatcherReadyListHead[32]`.
    ready_queues: [ListEntry; 32],
    /// Bit *p* set ⇔ ready_queues[p] non-empty (`KiReadySummary`); makes
    /// "highest ready priority" a single leading-zero count.
    ready_summary: u32,
    /// Active (armed) `Ktimer`s, unsorted — fine at our timer counts;
    /// NT's timer wheel is the scalable version of exactly this list.
    timer_list: ListEntry,
    initialized: bool,
}

static mut KI_DISPATCHER: KiDispatcher = KiDispatcher {
    ready_queues: [const { ListEntry::new() }; 32],
    ready_summary: 0,
    timer_list: ListEntry::new(),
    initialized: false,
};

/// Access the dispatcher state.
///
/// # Safety
/// Caller must hold the dispatcher lock (or be in single-threaded phase-0).
unsafe fn state() -> &'static mut KiDispatcher {
    unsafe { &mut *(&raw mut KI_DISPATCHER) }
}

// ---------------------------------------------------------------------------
// Initialization
// ---------------------------------------------------------------------------

/// Adopt the boot context as this processor's idle thread and bring the
/// dispatcher online. Phase-1, single-threaded, interrupts still off.
///
/// NT does the same trick: the stack that carried `KiSystemStartup`
/// *becomes* the idle thread rather than being discarded.
///
/// # Safety
/// Call exactly once per processor, before interrupts are enabled.
pub unsafe fn ki_initialize(boot_thread: *mut Kthread) {
    unsafe {
        let s = state();
        for q in &mut s.ready_queues {
            q.init();
        }
        s.timer_list.init();
        s.initialized = true;

        (*boot_thread).state = ThreadState::Running;
        let prcb = pcr::ke_get_prcb();
        prcb.idle_thread = boot_thread;
        prcb.current_thread = boot_thread;
    }
}

// ---------------------------------------------------------------------------
// Ready queues
// ---------------------------------------------------------------------------

/// Insert `thread` at the tail of its priority's ready queue.
/// Lock held by caller.
unsafe fn ready_thread_locked(thread: *mut Kthread) {
    unsafe {
        let s = state();
        let pri = (*thread).priority as usize & 31;
        (*thread).state = ThreadState::Ready;
        s.ready_queues[pri].insert_tail(&raw mut (*thread).ready_list_entry);
        s.ready_summary |= 1 << pri;
    }
}

/// Pop the highest-priority ready thread, or null if none.
/// Lock held by caller.
unsafe fn select_next_locked() -> *mut Kthread {
    unsafe {
        let s = state();
        if s.ready_summary == 0 {
            return core::ptr::null_mut();
        }
        let pri = 31 - s.ready_summary.leading_zeros() as usize;
        let entry = s.ready_queues[pri]
            .remove_head()
            .expect("ready_summary bit set but queue empty");
        if s.ready_queues[pri].is_empty() {
            s.ready_summary &= !(1 << pri);
        }
        container_of!(entry, Kthread, ready_list_entry)
    }
}

/// `KeReadyThread` — make a thread runnable and let preemption sort it out.
///
/// # Safety
/// `thread` must be pinned, initialized (forged stack), and not queued.
pub unsafe fn ki_ready_thread(thread: *mut Kthread) {
    let old = acquire();
    unsafe { ready_thread_locked(thread) };
    release(old);
    // The new thread may outrank the running one; the dispatch interrupt
    // will run ki_check_preemption once IRQL allows.
    crate::hal::apic::request_dispatch_interrupt();
}

// ---------------------------------------------------------------------------
// Context-switch core
// ---------------------------------------------------------------------------

/// Switch from `cur` (already re-stated by the caller: Waiting/Ready/
/// Terminated) to the best ready thread, or to the idle thread if none.
/// Called and returns with the dispatcher lock held (hand-off; see module
/// docs). No-op if the chosen thread *is* `cur`.
unsafe fn switch_away_locked(cur: *mut Kthread) {
    unsafe {
        let prcb = pcr::ke_get_prcb();
        let mut next = select_next_locked();
        if next.is_null() {
            next = prcb.idle_thread;
        }
        if next == cur {
            // cur was requeued and is still the best choice: keep running.
            (*cur).state = ThreadState::Running;
            return;
        }
        (*next).state = ThreadState::Running;
        (*next).quantum = DEFAULT_QUANTUM;
        prcb.current_thread = next;
        // Future user-mode entries from `next` must land on its stack: both
        // the TSS RSP0 (used by interrupts) and the per-CPU syscall stack.
        crate::ke::gdt::set_kernel_stack((*next).stack_top);
        pcr::set_syscall_kernel_stack((*next).stack_top);
        // Switch into the resuming thread's address space. The kernel half is
        // shared by every address space, so the kernel stacks of both `cur`
        // and `next` (and the code executing here) stay mapped across the
        // load. Only switch when the target AS actually differs, to avoid a
        // needless TLB flush when staying within one address space.
        // Emulated per-process DLL data: give the resuming process its private
        // copy of the shim C-runtime state (fd table, cached std handles) by
        // swapping it into the shared shim `.data` pages, saving the outgoing
        // process's first. No-op unless an isolated process is involved. Done
        // while `cur`'s address space is still active; the shim pages are in the
        // shared high half (same physical frames in every space), so the copy
        // sees the same bytes either side of the CR3 load.
        let out_cr3 = (*cur).cr3;
        let in_cr3 = (*next).cr3;
        if out_cr3 != in_cr3 && (out_cr3 != 0 || in_cr3 != 0) {
            crate::ldr::loaded::swap_shim_data(out_cr3, in_cr3);
        }
        let target_cr3 = if (*next).cr3 != 0 {
            (*next).cr3
        } else {
            crate::mm::virt::mm_kernel_address_space().0
        };
        if target_cr3 != crate::mm::virt::mm_current_address_space().0 {
            crate::mm::virt::mm_switch_address_space(crate::mm::PhysAddr(target_cr3));
        }
        // Restore the resuming thread's user GS base (its TEB). Per-thread, so
        // when `next` next returns to ring 3 its `swapgs` lands on its own GS.
        // Kernel threads have gs_base 0 and run on the KPCR (GS_BASE), so we
        // leave KERNEL_GS_BASE untouched for them.
        if (*next).gs_base != 0 {
            const IA32_KERNEL_GS_BASE: u32 = 0xC000_0102;
            pcr::wrmsr(IA32_KERNEL_GS_BASE, (*next).gs_base);
        }
        ki_swap_context(cur, next);
        // ...time passes; somebody switched back to us. We own the lock.
    }
}

/// New-thread epilogue (see `ke::thread::ki_thread_begin`): the forged
/// frame has no enclosing function to release the dispatcher lock the
/// switch handed us, so do it here, then open interrupts at PASSIVE.
pub fn ki_finish_switch_to_new_thread() {
    release(PASSIVE_LEVEL);
    unsafe { core::arch::asm!("sti", options(nomem, nostack)) };
}

// ---------------------------------------------------------------------------
// Waiting and signaling
// ---------------------------------------------------------------------------

/// Result of testing whether a thread's whole wait can complete now.
enum Satisfy {
    /// Not satisfiable yet — keep blocking.
    NotYet,
    /// The timeout timer fired first → `STATUS_TIMEOUT`.
    Timeout,
    /// WaitAny: real object at this index is signaled → `WAIT_0 + key`.
    Any(usize),
    /// WaitAll: every real object is signaled → `WAIT_0`.
    All,
}

/// Is `hdr` signaled *for this thread*? Mutants add the owner exception:
/// the holder may always re-acquire recursively even when the count is 0.
/// Lock held.
unsafe fn signaled_for(thread: *mut Kthread, hdr: *mut DispatcherHeader) -> bool {
    unsafe {
        if (*hdr).signal_state > 0 {
            return true;
        }
        if (*hdr).object_type == DispatcherObjectType::Mutant {
            let m = container_of!(hdr, Kmutant, header);
            return (*m).owner == thread;
        }
        false
    }
}

/// Test a blocked thread's wait against current object states. Objects are
/// checked before the timeout so a just-in-time signal wins over a
/// simultaneous expiry (NT prefers success to `STATUS_TIMEOUT`). Lock held.
unsafe fn evaluate_wait(thread: *mut Kthread) -> Satisfy {
    unsafe {
        let count = (*thread).wait_count;
        match (*thread).wait_type {
            WaitType::Any => {
                for i in 0..count {
                    if signaled_for(thread, (*thread).wait_blocks[i].object) {
                        return Satisfy::Any(i);
                    }
                }
            }
            WaitType::All => {
                if count > 0 && (0..count).all(|i| signaled_for(thread, (*thread).wait_blocks[i].object)) {
                    return Satisfy::All;
                }
            }
        }
        if (*thread).wait_timed {
            let timer_hdr = (*thread).wait_blocks[TIMER_WAIT_BLOCK].object;
            if (*timer_hdr).signal_state > 0 {
                return Satisfy::Timeout;
            }
        }
        Satisfy::NotYet
    }
}

/// Apply the "satisfy" side effect to one object as a wait consumes it:
/// auto-reset events clear, semaphores/mutants decrement, the mutant gains
/// an owner. Notification events, timers and threads stay signaled. Lock held.
unsafe fn satisfy_object_for(hdr: *mut DispatcherHeader, thread: *mut Kthread) {
    unsafe {
        match (*hdr).object_type {
            DispatcherObjectType::SynchronizationEvent => (*hdr).signal_state = 0,
            DispatcherObjectType::Semaphore => (*hdr).signal_state -= 1,
            DispatcherObjectType::Mutant => {
                (*hdr).signal_state -= 1;
                let m = container_of!(hdr, Kmutant, header);
                (*m).owner = thread;
            }
            _ => {}
        }
    }
}

/// Consume the objects a satisfied wait acquired and compute its status.
/// Lock held.
unsafe fn apply_consume(thread: *mut Kthread, result: &Satisfy) -> NtStatus {
    unsafe {
        match *result {
            Satisfy::Any(i) => {
                let key = (*thread).wait_blocks[i].wait_key;
                satisfy_object_for((*thread).wait_blocks[i].object, thread);
                NtStatus(NtStatus::WAIT_0.0 + key)
            }
            Satisfy::All => {
                for i in 0..(*thread).wait_count {
                    satisfy_object_for((*thread).wait_blocks[i].object, thread);
                }
                NtStatus::WAIT_0
            }
            Satisfy::Timeout => NtStatus::TIMEOUT,
            Satisfy::NotYet => NtStatus::UNSUCCESSFUL, // never reached
        }
    }
}

/// Unlink a thread from every object it was waiting on and cancel its
/// timeout timer. Called exactly once when a wait is satisfied (by an
/// object, by timeout, or on the entry fast path it is a no-op). Lock held.
unsafe fn unlink_thread_waits(thread: *mut Kthread) {
    unsafe {
        for i in 0..(*thread).wait_count {
            if (*thread).wait_blocks[i].active {
                ListEntry::remove(&raw mut (*thread).wait_blocks[i].wait_list_entry);
                (*thread).wait_blocks[i].active = false;
            }
        }
        if (*thread).wait_timed {
            if (*thread).wait_blocks[TIMER_WAIT_BLOCK].active {
                ListEntry::remove(&raw mut (*thread).wait_blocks[TIMER_WAIT_BLOCK].wait_list_entry);
                (*thread).wait_blocks[TIMER_WAIT_BLOCK].active = false;
            }
            if (*thread).wait_timer.inserted {
                ListEntry::remove(&raw mut (*thread).wait_timer.timer_list_entry);
                (*thread).wait_timer.inserted = false;
            }
            (*thread).wait_timed = false;
        }
    }
}

/// `KiWaitTest` — after an object becomes signaled, wake every blocked
/// thread whose *entire* wait is now satisfiable, in FIFO order, consuming
/// signal state as it goes (so an auto-reset event or a semaphore stops
/// waking once its budget is spent). Returns the number woken. Lock held.
unsafe fn ki_wait_test(hdr: *mut DispatcherHeader) -> usize {
    unsafe {
        (*hdr).ensure_wait_list();
        let mut woke = 0;
        // Re-scan from the head after each wake: waking unlinks the thread
        // from this list and mutates signal state, so the list and the
        // satisfiability of the remaining waiters both change underfoot.
        loop {
            let mut found: *mut Kthread = core::ptr::null_mut();
            (*hdr).wait_list.for_each(|entry| {
                if !found.is_null() {
                    return;
                }
                let wb = container_of!(entry, crate::ke::thread::KwaitBlock, wait_list_entry);
                let thr = (*wb).thread;
                if !matches!(evaluate_wait(thr), Satisfy::NotYet) {
                    found = thr;
                }
            });
            if found.is_null() {
                break;
            }
            let result = evaluate_wait(found);
            (*found).wait_status = apply_consume(found, &result);
            unlink_thread_waits(found);
            ready_thread_locked(found);
            woke += 1;
        }
        woke
    }
}

/// `KiWaitForObjects` — the general blocking core behind
/// `KeWaitForSingleObject` and `KeWaitForMultipleObjects`.
///
/// Sets up the current thread's wait blocks for `objects` under `wait_type`
/// (WaitAll/WaitAny), checks for immediate satisfaction (the fast path that
/// never blocks), and otherwise links into each object's wait list, arms an
/// optional timeout timer, and switches away. Returns `WAIT_0 + index` (the
/// satisfying object for WaitAny, or `WAIT_0` for WaitAll) or
/// `STATUS_TIMEOUT`.
///
/// # Safety
/// Each object pinned & initialized; `objects.len() <= THREAD_WAIT_OBJECTS`;
/// IRQL < DISPATCH (asserted by the public wrappers).
pub unsafe fn ki_wait_for_objects(
    objects: &[*mut DispatcherHeader],
    wait_type: WaitType,
    timeout_ticks: Option<u64>,
) -> NtStatus {
    debug_assert!(objects.len() <= THREAD_WAIT_OBJECTS, "too many wait objects");
    unsafe {
        let old = acquire();
        let cur = pcr::ke_get_current_thread();

        // Lay out the wait blocks (not yet linked).
        (*cur).wait_type = wait_type;
        (*cur).wait_count = objects.len();
        (*cur).wait_timed = false;
        for (i, &obj) in objects.iter().enumerate() {
            (*obj).ensure_wait_list();
            (*cur).wait_blocks[i].thread = cur;
            (*cur).wait_blocks[i].object = obj;
            (*cur).wait_blocks[i].wait_key = i as u32;
            (*cur).wait_blocks[i].active = false;
        }

        // Fast path: already satisfiable — consume and return without blocking.
        let result = evaluate_wait(cur);
        if !matches!(result, Satisfy::NotYet) {
            let status = apply_consume(cur, &result);
            release(old);
            return status;
        }
        // A zero timeout means "poll": never block.
        if timeout_ticks == Some(0) {
            release(old);
            return NtStatus::TIMEOUT;
        }

        // Slow path: link each object's wait block into its wait list.
        for i in 0..(*cur).wait_count {
            let obj = (*cur).wait_blocks[i].object;
            (*obj)
                .wait_list
                .insert_tail(&raw mut (*cur).wait_blocks[i].wait_list_entry);
            (*cur).wait_blocks[i].active = true;
        }

        // Arm the timeout timer, if any, as an extra waited object.
        if let Some(ticks) = timeout_ticks {
            (*cur).wait_timer = Ktimer::new();
            (*cur).wait_timer.due_tick = KE_TICK_COUNT.load(Ordering::Relaxed) + ticks.max(1);
            (*cur).wait_timer.header.ensure_wait_list();
            (*cur).wait_timer.inserted = true;
            state()
                .timer_list
                .insert_tail(&raw mut (*cur).wait_timer.timer_list_entry);
            let tb = TIMER_WAIT_BLOCK;
            (*cur).wait_blocks[tb].thread = cur;
            (*cur).wait_blocks[tb].object = &raw mut (*cur).wait_timer.header;
            (*cur).wait_blocks[tb].active = true;
            let twl = &raw mut (*cur).wait_timer.header.wait_list;
            (*twl).insert_tail(&raw mut (*cur).wait_blocks[tb].wait_list_entry);
            (*cur).wait_timed = true;
            update_earliest_locked();
        }

        (*cur).state = ThreadState::Waiting;
        switch_away_locked(cur);

        // Woken: status was set by whoever satisfied (or timed out) the wait.
        let status = (*cur).wait_status;
        release(old);
        status
    }
}

/// `KiWaitForObject` — single-object wait wrapper.
///
/// # Safety
/// As [`ki_wait_for_objects`], for one object.
pub unsafe fn ki_wait_for_object(
    object: *mut DispatcherHeader,
    timeout_ticks: Option<u64>,
) -> NtStatus {
    unsafe { ki_wait_for_objects(&[object], WaitType::Any, timeout_ticks) }
}

/// `KiSignalObject` — generic "set this object" used by KeSetEvent and
/// thread termination. Returns the previous signal state.
///
/// # Safety
/// `header` pinned & initialized.
pub unsafe fn ki_signal_object(header: *mut DispatcherHeader) -> i32 {
    let woke;
    let prev;
    unsafe {
        let old = acquire();
        let hdr = &mut *header;
        prev = hdr.signal_state;
        // Latch signaled. Auto-reset (synchronization) events are consumed
        // back to 0 by the single waiter ki_wait_test wakes; notification
        // events/threads/timers stay signaled and wake everyone eligible.
        hdr.signal_state = 1;
        woke = ki_wait_test(header);
        release(old);
    }
    if woke > 0 {
        crate::hal::apic::request_dispatch_interrupt();
    }
    prev
}

/// `KeResetEvent` core. Returns previous state.
pub fn ki_reset_object(header: &mut DispatcherHeader) -> i32 {
    let old = acquire();
    let prev = header.signal_state;
    header.signal_state = 0;
    release(old);
    prev
}

/// `KeReleaseSemaphore` core.
///
/// # Safety
/// `sem` pinned & initialized.
pub unsafe fn ki_release_semaphore(sem: *mut Ksemaphore, adjustment: i32) -> Result<i32, NtStatus> {
    let woke;
    let prev;
    unsafe {
        let old = acquire();
        let s = &mut *sem;
        prev = s.header.signal_state;
        let new = prev.checked_add(adjustment).unwrap_or(i32::MAX);
        if new > s.limit || adjustment <= 0 {
            release(old);
            // STATUS_SEMAPHORE_LIMIT_EXCEEDED
            return Err(NtStatus(0xC000_0047));
        }
        s.header.signal_state = new;
        woke = ki_wait_test(&raw mut s.header);
        release(old);
    }
    if woke > 0 {
        crate::hal::apic::request_dispatch_interrupt();
    }
    Ok(prev)
}

/// `KeReleaseMutant` — release one level of mutant ownership. The caller
/// must own it (else `STATUS_MUTANT_NOT_OWNED`); when the recursion count
/// returns to free the owner is cleared and waiters are tested. Returns the
/// previous signal state.
///
/// # Safety
/// `mutant` pinned & initialized.
pub unsafe fn ki_release_mutant(mutant: *mut Kmutant) -> Result<i32, NtStatus> {
    let woke;
    let prev;
    unsafe {
        let old = acquire();
        let m = &mut *mutant;
        let cur = pcr::ke_get_current_thread();
        if m.owner != cur {
            release(old);
            return Err(NtStatus(0xC000_0046)); // STATUS_MUTANT_NOT_OWNED
        }
        prev = m.header.signal_state;
        // Each acquire decremented below 1; each release adds one back.
        m.header.signal_state += 1;
        if m.header.signal_state > 0 {
            m.owner = core::ptr::null_mut();
            woke = ki_wait_test(&raw mut m.header);
        } else {
            woke = 0;
        }
        release(old);
    }
    if woke > 0 {
        crate::hal::apic::request_dispatch_interrupt();
    }
    Ok(prev)
}

// ---------------------------------------------------------------------------
// Timers & sleeping
// ---------------------------------------------------------------------------

/// Recompute the clock ISR's earliest-deadline hint. Lock held.
unsafe fn update_earliest_locked() {
    unsafe {
        let mut earliest = u64::MAX;
        state().timer_list.for_each(|e| {
            let t = container_of!(e, Ktimer, timer_list_entry);
            earliest = earliest.min((*t).due_tick);
        });
        EARLIEST_TIMER_DUE.store(earliest, Ordering::Release);
    }
}

/// `KeDelayExecutionThread` core: a pure timeout wait — zero objects, so
/// only the thread's embedded timeout timer can satisfy it. Always returns
/// `STATUS_TIMEOUT` after `ticks` clock ticks.
pub fn ki_delay_thread(ticks: u64) -> NtStatus {
    unsafe { ki_wait_for_objects(&[], WaitType::All, Some(ticks)) }
}

/// Expire due timers: unlink, latch signaled, wake waiters. Runs in the
/// dispatch interrupt (DISPATCH_LEVEL), the NT timer-expiry DPC's job.
fn ki_expire_timers() {
    let now = KE_TICK_COUNT.load(Ordering::Relaxed);
    if EARLIEST_TIMER_DUE.load(Ordering::Acquire) > now {
        return;
    }
    let mut woke = 0;
    // DPCs to queue once the dispatcher lock is dropped (the DPC queue uses
    // its own any-IRQL lock; keep the two locks from nesting).
    let mut dpcs: [*mut crate::ke::dpc::Kdpc; 16] = [core::ptr::null_mut(); 16];
    let mut dpc_n = 0;
    unsafe {
        let old = acquire();
        let mut due: [*mut Ktimer; 16] = [core::ptr::null_mut(); 16];
        let mut n = 0;
        state().timer_list.for_each(|e| {
            let t = container_of!(e, Ktimer, timer_list_entry);
            if (*t).due_tick <= now && n < due.len() {
                due[n] = t;
                n += 1;
            }
        });
        for &t in &due[..n] {
            ListEntry::remove(&raw mut (*t).timer_list_entry);
            (*t).inserted = false;
            (*t).header.signal_state = 1; // timers are notification-type: latch
            woke += ki_wait_test(&raw mut (*t).header);
            if !(*t).dpc.is_null() && dpc_n < dpcs.len() {
                dpcs[dpc_n] = (*t).dpc;
                dpc_n += 1;
            }
        }
        update_earliest_locked();
        release(old);
    }
    // Queue the expiry DPCs; they run later in this same dispatch pass.
    for &dpc in &dpcs[..dpc_n] {
        unsafe { crate::ke::dpc::ke_insert_queue_dpc(dpc) };
    }
    let _ = woke; // already inside the dispatch interrupt: rescheduling follows
}

// ---------------------------------------------------------------------------
// Driver-facing timer API (KeSetTimer / KeCancelTimer)
// ---------------------------------------------------------------------------

/// `KeSetTimer` — arm `timer` to expire at absolute tick `due_tick`, queuing
/// `dpc` (may be null) on expiry. Returns true if the timer was already in
/// the queue (it is re-armed). Lock-managed internally.
///
/// # Safety
/// `timer` (and `dpc`, if non-null) must be pinned & initialized.
pub unsafe fn ke_set_timer(timer: *mut Ktimer, due_tick: u64, dpc: *mut crate::ke::dpc::Kdpc) -> bool {
    unsafe {
        let old = acquire();
        let was_set = (*timer).inserted;
        if was_set {
            ListEntry::remove(&raw mut (*timer).timer_list_entry);
        }
        (*timer).header.ensure_wait_list();
        (*timer).header.signal_state = 0; // re-arm: not yet signaled
        (*timer).due_tick = due_tick;
        (*timer).dpc = dpc;
        (*timer).inserted = true;
        state().timer_list.insert_tail(&raw mut (*timer).timer_list_entry);
        update_earliest_locked();
        release(old);
        was_set
    }
}

/// `KeCancelTimer` — remove `timer` from the active list if present. Returns
/// true if it was queued (i.e. the cancel actually did something).
///
/// # Safety
/// `timer` must be pinned & initialized.
pub unsafe fn ke_cancel_timer(timer: *mut Ktimer) -> bool {
    unsafe {
        let old = acquire();
        let was_set = (*timer).inserted;
        if was_set {
            ListEntry::remove(&raw mut (*timer).timer_list_entry);
            (*timer).inserted = false;
            update_earliest_locked();
        }
        release(old);
        was_set
    }
}

// ---------------------------------------------------------------------------
// Interrupt-driven scheduling
// ---------------------------------------------------------------------------

/// Clock ISR body (vector 0xD1, CLOCK_LEVEL). Keep it tiny; defer
/// everything that needs the dispatcher lock. See module docs.
pub fn ki_clock_tick(_frame: &mut KtrapFrame) {
    let now = KE_TICK_COUNT.fetch_add(1, Ordering::Relaxed) + 1;

    let prcb = pcr::ke_get_prcb();
    let cur = prcb.current_thread;
    if !cur.is_null() && cur != prcb.idle_thread {
        // Quantum accounting: only this CPU touches its running thread's
        // quantum, so a plain decrement at CLOCK_LEVEL is race-free.
        unsafe {
            (*cur).quantum -= 1;
            if (*cur).quantum <= 0 {
                prcb.quantum_end = true;
                crate::hal::apic::request_dispatch_interrupt();
            }
        }
    }
    if EARLIEST_TIMER_DUE.load(Ordering::Acquire) <= now {
        crate::hal::apic::request_dispatch_interrupt();
    }
}

/// Dispatch ISR body (vector 0x2F): retire DPCs, expire timers, then
/// resolve quantum end / preemption — `KiDispatchInterrupt`.
pub fn ki_dispatch_interrupt() {
    crate::ke::dpc::ki_retire_dpcs();
    if unsafe { !state().initialized } {
        return; // boot-time stray dispatch IPI before phase 1
    }
    ki_expire_timers();

    let prcb = pcr::ke_get_prcb();
    let quantum_end = core::mem::replace(&mut prcb.quantum_end, false);
    unsafe {
        let old = acquire();
        let cur = prcb.current_thread;
        let resched = if cur == prcb.idle_thread {
            // Idle cedes to anything ready.
            state().ready_summary != 0
        } else if quantum_end {
            // Round-robin within the priority: requeue at tail, then pick.
            (*cur).state = ThreadState::Ready;
            (*cur).quantum = DEFAULT_QUANTUM;
            ready_thread_locked(cur);
            true
        } else {
            // Preemption check: someone readied a higher-priority thread.
            let best = 31 - state().ready_summary.leading_zeros().min(31) as u8;
            state().ready_summary != 0 && best > (*cur).priority
        };
        if resched {
            if cur != prcb.idle_thread && (*cur).state == ThreadState::Running {
                (*cur).state = ThreadState::Ready;
                ready_thread_locked(cur);
            }
            switch_away_locked(cur);
        }
        release(old);
    }
}

/// `KeQueryTickCount`.
pub fn ke_query_tick_count() -> u64 {
    KE_TICK_COUNT.load(Ordering::Relaxed)
}

/// Thread self-termination: signal joiners, mark terminated, switch away
/// forever. The stack is intentionally not freed here — the thread is
/// standing on it; reclamation belongs to a reaper thread (future work,
/// noted in ps).
///
/// # Safety
/// Must be called by the terminating thread itself, at PASSIVE_LEVEL.
pub unsafe fn ki_terminate_current_thread() -> ! {
    unsafe {
        let _old = acquire();
        let cur = pcr::ke_get_current_thread();
        // Join semantics: a terminated thread is a signaled dispatcher object.
        (*cur).header.signal_state = 1;
        ki_wait_test(&raw mut (*cur).header);
        (*cur).state = ThreadState::Terminated;
        switch_away_locked(cur);
    }
    unreachable!("terminated thread was rescheduled");
}
