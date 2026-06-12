//! KPCR — the per-Processor Control Region.
//!
//! Every NT processor owns a KPCR, found through the `GS` segment base in
//! kernel mode; `KeGetCurrentThread()` is ultimately a single
//! `mov rax, gs:[Prcb.CurrentThread]`. We keep the same access pattern:
//! the KPCR's address is programmed into `IA32_GS_BASE`, the structure
//! begins with a self-pointer (NT keeps one at `gs:[0x18]`; ours is at
//! offset 0 — noted as a deliberate layout divergence), and the embedded
//! [`Kprcb`] carries the scheduling state.
//!
//! Single-processor for now: one static KPCR for the boot CPU. The MP step
//! is allocating one per AP and pointing each AP's GS base at its own.

use crate::ke::irql::Kirql;
use core::arch::asm;

/// Per-processor scheduling state — `KPRCB`.
///
/// The fields the dispatcher actually needs; the real KPRCB has hundreds
/// more (statistics, per-CPU lookaside lists, …) that can be added behind
/// these without changing consumers.
#[repr(C)]
pub struct Kprcb {
    /// The thread currently running on this processor.
    pub current_thread: *mut crate::ke::thread::Kthread,
    /// The next thread selected to run (set by the dispatcher, consumed by
    /// the context switch).
    pub next_thread: *mut crate::ke::thread::Kthread,
    /// This processor's idle thread (runs when the ready queues are empty).
    pub idle_thread: *mut crate::ke::thread::Kthread,
    /// Zero-based processor number.
    pub number: u32,
    /// A DPC was queued and the dispatch interrupt hasn't drained it yet.
    pub dpc_interrupt_requested: bool,
    /// Set by the clock tick when the running thread exhausted its quantum;
    /// checked on the way out of the dispatch interrupt.
    pub quantum_end: bool,
}

/// Per-processor control region — `KPCR`.
#[repr(C)]
pub struct Kpcr {
    /// Self-pointer: lets `gs`-relative code recover a linear pointer to
    /// its own KPCR (`mov rax, gs:[0]`).
    pub self_ptr: *mut Kpcr,
    /// Kernel stack the `syscall` entry switches to (the current thread's
    /// kernel stack top). `syscall` does not switch stacks in hardware, so
    /// `KiSystemCall64` loads RSP from here via `gs:`. Kept in sync with
    /// TSS.RSP0 by the context switch and on user-mode entry.
    pub syscall_kernel_stack: u64,
    /// Scratch where `KiSystemCall64` stashes the inbound user RSP while it
    /// runs on the kernel stack.
    pub syscall_user_stack: u64,
    /// Current IRQL shadow. The authoritative IRQL is CR8 (see ke::irql);
    /// NT keeps this shadow for debugger visibility and we do the same.
    pub irql: Kirql,
    /// The processor control block.
    pub prcb: Kprcb,
}

/// Byte offsets into the KPCR for `gs:`-relative access from the syscall
/// entry assembly (which cannot use `offset_of!` inline as cleanly).
pub const KPCR_SYSCALL_KERNEL_STACK: usize = core::mem::offset_of!(Kpcr, syscall_kernel_stack);
pub const KPCR_SYSCALL_USER_STACK: usize = core::mem::offset_of!(Kpcr, syscall_user_stack);

/// Boot processor's KPCR. Initialized once in phase 0 before interrupts
/// are enabled; from then on only accessed via `GS` by its own CPU.
static mut BOOT_PCR: Kpcr = Kpcr {
    self_ptr: core::ptr::null_mut(),
    syscall_kernel_stack: 0,
    syscall_user_stack: 0,
    irql: 0,
    prcb: Kprcb {
        current_thread: core::ptr::null_mut(),
        next_thread: core::ptr::null_mut(),
        idle_thread: core::ptr::null_mut(),
        number: 0,
        dpc_interrupt_requested: false,
        quantum_end: false,
    },
};

const IA32_GS_BASE: u32 = 0xC000_0101;

/// Program the boot CPU's GS base to point at its KPCR. Phase 0 only.
pub fn init() {
    unsafe {
        let pcr = &raw mut BOOT_PCR;
        (*pcr).self_ptr = pcr;
        wrmsr(IA32_GS_BASE, pcr as u64);
    }
}

/// `KeGetPcr` — the current processor's KPCR via `gs:[0]`.
///
/// # Safety contract (internal)
/// Valid only after [`init`]; all callers are post-phase-0 kernel code.
#[inline]
pub fn ke_get_pcr() -> &'static mut Kpcr {
    unsafe {
        let pcr: *mut Kpcr;
        // The self-pointer lives at gs:[0]; one load recovers the linear
        // address (same trick as NT's KeGetPcr reading gs:[18h]).
        asm!("mov {}, gs:[0]", out(reg) pcr, options(nostack, preserves_flags));
        &mut *pcr
    }
}

/// `KeGetCurrentPrcb`.
#[inline]
pub fn ke_get_prcb() -> &'static mut Kprcb {
    &mut ke_get_pcr().prcb
}

/// Record the kernel stack the `syscall` entry should switch to (the
/// current thread's kernel stack top). Called on context switch and on
/// user-mode entry, mirroring how NT keeps the per-processor RSP0 current.
#[inline]
pub fn set_syscall_kernel_stack(rsp: u64) {
    ke_get_pcr().syscall_kernel_stack = rsp;
}

/// `KeGetCurrentThread`.
#[inline]
pub fn ke_get_current_thread() -> *mut crate::ke::thread::Kthread {
    ke_get_prcb().current_thread
}

/// Write a model-specific register.
///
/// # Safety
/// MSR writes reconfigure the CPU; callers must know the register.
pub unsafe fn wrmsr(msr: u32, value: u64) {
    let lo = value as u32;
    let hi = (value >> 32) as u32;
    unsafe {
        asm!("wrmsr", in("ecx") msr, in("eax") lo, in("edx") hi, options(nomem, nostack));
    }
}

/// Read a model-specific register.
///
/// # Safety
/// Reading an unimplemented MSR faults (#GP).
pub unsafe fn rdmsr(msr: u32) -> u64 {
    let lo: u32;
    let hi: u32;
    unsafe {
        asm!("rdmsr", in("ecx") msr, out("eax") lo, out("edx") hi, options(nomem, nostack));
    }
    ((hi as u64) << 32) | lo as u64
}
