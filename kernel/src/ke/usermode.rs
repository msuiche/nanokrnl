//! Ring-3 entry — crossing from kernel into user mode.
//!
//! The CPU enters user mode by `iretq`-ing to a frame that names a ring-3
//! code segment (`CS` with RPL 3). [`ki_enter_user_mode`] builds that frame
//! and makes the jump. Coming *back* is the `syscall` path in
//! [`super::syscall`] (or an interrupt/fault).
//!
//! ## GS handling
//!
//! `swapgs` toggles the active GS base with `IA32_KERNEL_GS_BASE`. The
//! invariant the whole boundary relies on: **in kernel mode the active GS
//! base is the KPCR; in user mode it is the user value, with the KPCR
//! parked in `IA32_KERNEL_GS_BASE`.** So just before dropping to ring 3 we
//! stash the KPCR in `KERNEL_GS_BASE` and `swapgs` (active GS becomes the
//! user value), and the first `syscall`'s `swapgs` brings the KPCR back.
//!
//! Interrupts are masked across the `swapgs;iretq` pair (a `swapgs` is not
//! atomic with `iretq`, and an interrupt arriving in between would `swapgs`
//! from the wrong state); `iretq` restores the user RFLAGS — which has IF
//! set — re-enabling them atomically on the user side.

use crate::ke::pcr;
use crate::ke::selectors::{KGDT64_R3_CODE, KGDT64_R3_DATA};
use core::arch::asm;

const IA32_KERNEL_GS_BASE: u32 = 0xC000_0102;

/// Enter ring 3 at `user_rip` with stack `user_rsp`. Never returns to the
/// caller — control leaves for user mode and comes back only via `syscall`
/// (or a trap). The current thread's kernel stack top must already be
/// recorded as the per-CPU syscall stack (see
/// [`pcr::set_syscall_kernel_stack`]) so the first syscall can switch onto it.
///
/// # Safety
/// `user_rip`/`user_rsp` must point into user-accessible memory (mark it
/// with [`crate::mm::virt::mm_set_user_executable`]). The current thread
/// becomes a user thread; it must reach a terminating syscall to be
/// reclaimed.
pub unsafe fn ki_enter_user_mode(user_rip: u64, user_rsp: u64, teb: u64) -> ! {
    unsafe {
        // Decide the user-mode GS base: a real Windows binary expects GS to
        // point at its TEB (it reads `gs:[0x30]` self, `gs:[0x60]` PEB, ...).
        // Threads without a TEB (our minimal apps) keep GS = KPCR, which they
        // never read. We park that value in KERNEL_GS_BASE and `swapgs`, so
        // the active GS base becomes it and the (kernel) KPCR moves into
        // KERNEL_GS_BASE for the first syscall's swapgs to restore.
        //
        // KERNEL_GS_BASE is per-CPU; we record this thread's user GS in its
        // KTHREAD so the scheduler restores it on every switch-in (see
        // `switch_away_locked`). That lets multiple TEB user threads coexist —
        // e.g. a CreateProcess parent and its child.
        let kpcr = pcr::ke_get_pcr() as *mut _ as u64;
        let user_gs = if teb != 0 { teb } else { kpcr };
        (*pcr::ke_get_current_thread()).gs_base = user_gs;
        pcr::wrmsr(IA32_KERNEL_GS_BASE, user_gs);

        let user_cs = (KGDT64_R3_CODE | 3) as u64;
        let user_ss = (KGDT64_R3_DATA | 3) as u64;
        let mut rflags: u64 = 0x202; // IF set, reserved bit 1 set
        // If the debugger armed tracing, set the Trap Flag so the CPU raises a
        // #DB after each user instruction (single-stepping).
        if crate::ke::debug::take_armed() {
            rflags |= 1 << 8; // TF
        }

        asm!(
            "cli",          // no interrupts across the swapgs;iretq window
            "swapgs",       // active GS base -> user value (KPCR parked)
            "push {ss}",    // iretq frame, top-down: SS, RSP, RFLAGS, CS, RIP
            "push {rsp}",
            "push {flags}",
            "push {cs}",
            "push {rip}",
            "iretq",
            ss = in(reg) user_ss,
            rsp = in(reg) user_rsp,
            flags = in(reg) rflags,
            cs = in(reg) user_cs,
            rip = in(reg) user_rip,
            options(noreturn),
        );
    }
}
