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
use core::arch::{asm, naked_asm};

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

/// Resume user mode with a complete register state — the tail of
/// `NtContinue`. Unlike [`ki_enter_user_mode`] (which starts a fresh thread
/// with zeroed GPRs), this restores all fifteen GPRs from `ctx` before the
/// `iretq`, so an exception handler can resume the faulting instruction
/// stream with modified registers.
///
/// The kernel stack is **reset** to the current thread's stack top first:
/// the syscall that led here never returns, and abandoning its frames is
/// exactly how the per-thread stack stays bounded across arbitrarily many
/// dispatch/continue round trips.
///
/// # Safety
/// `ctx` must be validated and sanitized by the caller (ring-3 segments,
/// user RIP/RSP, masked RFLAGS — see `ke::exception`).
pub unsafe fn ki_continue_user_mode(ctx: &super::exception::UserResume) -> ! {
    unsafe {
        let top = (*pcr::ke_get_current_thread()).stack_top;
        ki_continue_asm(ctx, top)
    }
}

/// The register-restore half of [`ki_continue_user_mode`]. Naked with a
/// fixed convention (`rdi` = context, `rsi` = kernel stack top) because the
/// compiler cannot be allowed to choose the registers: every one of them is
/// repurposed by the restore, with `rdi` itself restored last.
#[unsafe(naked)]
unsafe extern "C" fn ki_continue_asm(ctx: &super::exception::UserResume, stack_top: u64) -> ! {
    use super::exception::UserResume as U;
    naked_asm!(
        "cli",                    // the swapgs;iretq window must stay atomic
        "mov rsp, rsi",           // reset the kernel stack (syscall frames die)
        // iretq frame, top-down: SS, RSP, RFLAGS, CS, RIP
        "push qword ptr [rdi + {ss}]",
        "push qword ptr [rdi + {rsp}]",
        "push qword ptr [rdi + {rflags}]",
        "push qword ptr [rdi + {cs}]",
        "push qword ptr [rdi + {rip}]",
        // Restore the GPRs; rdi (the context pointer) goes last.
        "mov r15, [rdi + {r15}]",
        "mov r14, [rdi + {r14}]",
        "mov r13, [rdi + {r13}]",
        "mov r12, [rdi + {r12}]",
        "mov r11, [rdi + {r11}]",
        "mov r10, [rdi + {r10}]",
        "mov r9,  [rdi + {r9}]",
        "mov r8,  [rdi + {r8}]",
        "mov rbp, [rdi + {rbp}]",
        "mov rdx, [rdi + {rdx}]",
        "mov rcx, [rdi + {rcx}]",
        "mov rbx, [rdi + {rbx}]",
        "mov rax, [rdi + {rax}]",
        "mov rsi, [rdi + {rsi}]",
        "mov rdi, [rdi + {rdi}]",
        "swapgs",                 // active GS base -> user value (KPCR parked)
        "iretq",
        ss = const core::mem::offset_of!(U, ss),
        rsp = const core::mem::offset_of!(U, rsp),
        rflags = const core::mem::offset_of!(U, rflags),
        cs = const core::mem::offset_of!(U, cs),
        rip = const core::mem::offset_of!(U, rip),
        r15 = const core::mem::offset_of!(U, r15),
        r14 = const core::mem::offset_of!(U, r14),
        r13 = const core::mem::offset_of!(U, r13),
        r12 = const core::mem::offset_of!(U, r12),
        r11 = const core::mem::offset_of!(U, r11),
        r10 = const core::mem::offset_of!(U, r10),
        r9 = const core::mem::offset_of!(U, r9),
        r8 = const core::mem::offset_of!(U, r8),
        rbp = const core::mem::offset_of!(U, rbp),
        rdx = const core::mem::offset_of!(U, rdx),
        rcx = const core::mem::offset_of!(U, rcx),
        rbx = const core::mem::offset_of!(U, rbx),
        rax = const core::mem::offset_of!(U, rax),
        rsi = const core::mem::offset_of!(U, rsi),
        rdi = const core::mem::offset_of!(U, rdi),
    )
}
