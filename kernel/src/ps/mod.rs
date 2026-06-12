//! # Ps — Process and Thread management
//!
//! Ps wraps Ke's raw scheduling objects (`KPROCESS`/`KTHREAD`) into the
//! executive objects the rest of the system sees (`EPROCESS`/`ETHREAD`),
//! adding identity (CIDs), and — eventually — handle tables, address
//! spaces and security tokens.
//!
//! What exists today: the System process skeleton and
//! [`ps_create_system_thread`], the kernel-thread factory every other
//! subsystem uses (NT's `PsCreateSystemThread` minus the object-handle
//! plumbing, which arrives with Ob handle tables).

use crate::ex;
use crate::ke::scheduler;
use crate::ke::thread::{Kthread, DEFAULT_PRIORITY};
use crate::mm::pool::pool_tag;
use crate::rtl::NtStatus;
use core::sync::atomic::{AtomicU64, Ordering};

/// Pool tags, WinDbg-style.
const TAG_THREAD: u32 = pool_tag(b"Thrd");
const TAG_STACK: u32 = pool_tag(b"Stak");

/// Kernel thread stacks: 32 KiB, matching NT's x64 kernel stack budget
/// (3 pages guaranteed + expansion; we simply allocate the full budget).
const KERNEL_STACK_SIZE: usize = 32 * 1024;

/// Monotone thread-ID source (CID allocation, simplified).
static NEXT_THREAD_ID: AtomicU64 = AtomicU64::new(4); // NT CIDs start low, 4 = System

/// `ETHREAD` — executive thread. The `KTHREAD` (named `tcb`, exactly as
/// in NT) must stay the first member: Ke code freely casts between the
/// two, and `CONTAINING_RECORD`-style recovery depends on it.
#[repr(C)]
pub struct Ethread {
    /// Thread control block — the Ke-visible part.
    pub tcb: Kthread,
    /// Entry point recorded for diagnostics.
    pub start_address: u64,
}

/// `PsCreateSystemThread` — create and start a kernel-mode thread running
/// `entry(context)`.
///
/// The thread runs at [`DEFAULT_PRIORITY`] and is immediately ready; on a
/// single CPU it actually runs when the caller next blocks, lowers IRQL,
/// or takes a dispatch interrupt. Returns the `ETHREAD` pointer (handle
/// plumbing arrives with the Ob handle table; kernel callers in NT
/// usually convert the handle straight back to a pointer anyway).
pub fn ps_create_system_thread(
    entry: extern "C" fn(*mut core::ffi::c_void) -> !,
    context: *mut core::ffi::c_void,
) -> Result<*mut Ethread, NtStatus> {
    // Stack first: a thread without a stack is nothing.
    let stack = crate::mm::pool::pool_alloc_checked(KERNEL_STACK_SIZE, TAG_STACK)?;
    let tid = NEXT_THREAD_ID.fetch_add(4, Ordering::Relaxed);

    let thread = ex::ex_allocate_object(
        Ethread {
            tcb: Kthread::new(tid, stack as u64, KERNEL_STACK_SIZE, DEFAULT_PRIORITY),
            start_address: entry as usize as u64,
        },
        TAG_THREAD,
    )?;

    // SAFETY: thread/stack freshly allocated and exclusively ours; the
    // forged frame hands control to `entry` on first switch-in.
    unsafe {
        (*thread).tcb.initialize_stack(entry, context);
        scheduler::ki_ready_thread(&raw mut (*thread).tcb);
    }
    Ok(thread)
}

/// `PsTerminateSystemThread` — called by a system thread to end itself.
/// Signals joiners (the ETHREAD is a dispatcher object) and never returns.
///
/// The thread's stack and ETHREAD are *not* freed here — it is standing
/// on that stack. NT parks dead threads on a reaper queue that a worker
/// thread drains; until our reaper exists, terminated-thread memory is
/// intentionally leaked (bounded by thread churn, zero in the current
/// system-thread-only workload).
pub fn ps_terminate_system_thread() -> ! {
    unsafe { scheduler::ki_terminate_current_thread() }
}
