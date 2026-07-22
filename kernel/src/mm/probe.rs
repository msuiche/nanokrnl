//! Guarded user-memory access — NT's probe semantics, with recovery.
//!
//! `mm::virt::probe_for_read`/`probe_for_write` validate a user range up
//! front, but validation alone has the holes NT closes by wrapping the
//! actual copy in `__try/__except`: a read-only page passes a *presence*
//! probe and faults on the write, and a page can always vanish between
//! the probe and the touch. A fault there arrives as a kernel-mode page
//! fault — without recovery it is a bugcheck that takes the whole machine
//! down for one bad user pointer.
//!
//! This module is the recovery half: [`guard`] arms a per-thread landing
//! pad (setjmp-style), the page-fault path ([`take_recovery`], called
//! from `ke::traps`) rewinds the trap frame into it, and the guarded copy
//! reports `ACCESS_VIOLATION` instead of stopping the machine. The
//! [`copy_to_user`]/[`copy_from_user`] primitives package the whole
//! pattern — probe, SMAP bracket, guarded copy — for syscall paths.
//!
//! The guard contract, exactly like NT's probes: the guarded region must
//! be short, non-blocking, and must not touch scheduler state — the
//! landing abandons the region's stack frames, so anything it held (a
//! lock, an allocation with a live Drop) would leak. The region is a raw
//! copy and nothing else.

use crate::ke::thread::FaultRecovery;
use crate::rtl::NtStatus;

/// setjmp-style capture for the fault-recovery guard. Returns 0 on the
/// initial call; the page-fault path can rewind the faulting thread's
/// trap frame so control resumes *here again* with the nonvolatile
/// registers restored and 1 in rax — the guard's fault exit. `ctx`
/// receives the resume state: return address, post-return RSP, and the
/// callee-saved set (the registers `iretq` cannot otherwise recover).
#[unsafe(naked)]
unsafe extern "C" fn recovery_capture(ctx: *mut FaultRecovery) -> u64 {
    core::arch::naked_asm!(
        "mov rax, [rsp]",
        "mov [rdi + 0x00], rax", // rip = our return address
        "lea rax, [rsp + 8]",
        "mov [rdi + 0x08], rax", // rsp as it will be after that return
        "mov [rdi + 0x10], rbp",
        "mov [rdi + 0x18], rbx",
        "mov [rdi + 0x20], r12",
        "mov [rdi + 0x28], r13",
        "mov [rdi + 0x30], r14",
        "mov [rdi + 0x38], r15",
        "xor eax, eax",
        "ret",
    )
}

/// Arm the current thread's recovery slot with the captured state.
unsafe fn arm(ctx: &FaultRecovery) {
    unsafe {
        let t = crate::ke::pcr::ke_get_current_thread();
        (*t).fault_recovery = *ctx;
        (*t).recovery_armed = true;
    }
}

/// Disarm the current thread's recovery slot (guarded region completed).
unsafe fn disarm() {
    unsafe {
        let t = crate::ke::pcr::ke_get_current_thread();
        (*t).recovery_armed = false;
    }
}

/// The page-fault path's half: if the faulting thread has an armed
/// recovery, disarm and hand the landing pad over (the trap handler
/// rewrites the frame with it). Any thread without one faults the old
/// way — a kernel bug is a bugcheck, guard or no guard.
pub fn take_recovery() -> Option<FaultRecovery> {
    unsafe {
        let t = crate::ke::pcr::ke_get_current_thread();
        if t.is_null() || !(*t).recovery_armed {
            return None;
        }
        (*t).recovery_armed = false;
        Some((*t).fault_recovery)
    }
}

/// Run `f` with the recovery guard armed: a kernel-mode page fault inside
/// `f` rewinds control to the capture point and `guard` reports
/// `ACCESS_VIOLATION` — the machine keeps running. The slot is per-thread
/// and the landing abandons `f`'s frames, so `f` must be a bounded,
/// non-blocking, borrow-only region (a raw copy and nothing else).
#[inline(never)]
pub fn guard(f: impl FnOnce()) -> Result<(), NtStatus> {
    let mut ctx = FaultRecovery::default();
    // recovery_capture returns twice: 0 now, 1 if the fault path rewound
    // here (the trap handler already disarmed the slot).
    if unsafe { recovery_capture(&mut ctx) } != 0 {
        return Err(NtStatus::ACCESS_VIOLATION);
    }
    unsafe { arm(&ctx) };
    f();
    unsafe { disarm() };
    Ok(())
}

/// Copy into a user buffer: probe the range, then copy inside the
/// recovery guard (and the SMAP bracket) so a page the probe cannot see —
/// read-only, or gone since — fails the copy instead of the machine.
pub fn copy_to_user(dst: u64, src: *const u8, len: usize) -> Result<(), NtStatus> {
    crate::mm::virt::probe_for_write(dst, len, 1)?;
    crate::mm::virt::user_access_begin();
    let r = guard(|| unsafe {
        core::ptr::copy_nonoverlapping(src, dst as *mut u8, len);
    });
    crate::mm::virt::user_access_end();
    r
}

/// Copy out of a user buffer — the read mirror of [`copy_to_user`].
pub fn copy_from_user(dst: *mut u8, src: u64, len: usize) -> Result<(), NtStatus> {
    crate::mm::virt::probe_for_read(src, len, 1)?;
    crate::mm::virt::user_access_begin();
    let r = guard(|| unsafe {
        core::ptr::copy_nonoverlapping(src as *const u8, dst, len);
    });
    crate::mm::virt::user_access_end();
    r
}
