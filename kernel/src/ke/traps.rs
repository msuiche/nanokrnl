//! Trap entry: interrupt stubs, the trap frame, and `KiDispatchTrap`.
//!
//! Hardware gives an interrupt handler almost nothing — the CPU pushes
//! SS:RSP, RFLAGS, CS:RIP (and for some exceptions an error code) and
//! vectors through the IDT. Everything else (saving registers, building a
//! debuggable frame, routing to the right subsystem) is on us. NT calls
//! the saved-state record `KTRAP_FRAME`; ours ([`KtrapFrame`]) keeps the
//! same role and member names, though not the full NT layout (we save the
//! volatile *and* non-volatile GP registers unconditionally for
//! simplicity; NT splits them between KTRAP_FRAME and KEXCEPTION_FRAME).
//!
//! ## Stub scheme
//!
//! There are 256 entry stubs, one per vector, emitted by `global_asm!` at
//! a fixed 16-byte stride so the IDT builder can compute their addresses
//! (`ki_vector_stubs + vector * 16`). Each stub:
//!
//! 1. pushes a dummy 0 for the vectors where the CPU does **not** push an
//!    error code (so the frame layout is uniform),
//! 2. pushes its vector number,
//! 3. jumps to `ki_trap_common`, which spills the GP registers, aligns,
//!    and calls into Rust with `rdi = &mut KtrapFrame`.
//!
//! Returning simply unwinds the same pushes and `iretq`s.
//!
//! `ki_trap_common` `swapgs`es on entry/exit when the trap crossed from ring
//! 3 (a user thread may have the TEB as its GS base), gated on the saved CS.
//!
//! Historical note: there used to be no `swapgs` here because the kernel had no user
//! mode to return to; the GS base is always the kernel KPCR. The syscall
//! path will add it.

use crate::ke::bugcheck;
use crate::kd_println;
use core::arch::global_asm;

/// Saved CPU state for an interrupt/exception, in stack order.
/// Lower offsets were pushed later: the GP registers (by `ki_trap_common`),
/// then vector/error code (by the stub), then the hardware frame.
#[repr(C)]
#[derive(Debug)]
pub struct KtrapFrame {
    // -- pushed by ki_trap_common (reverse push order) --
    pub rax: u64,
    pub rcx: u64,
    pub rdx: u64,
    pub rbx: u64,
    pub rbp: u64,
    pub rsi: u64,
    pub rdi: u64,
    pub r8: u64,
    pub r9: u64,
    pub r10: u64,
    pub r11: u64,
    pub r12: u64,
    pub r13: u64,
    pub r14: u64,
    pub r15: u64,
    // -- pushed by the vector stub --
    pub vector: u64,
    /// Hardware error code, or 0 for vectors that have none. For page
    /// faults this is the PF error bits (P/W/U/RSVD/I).
    pub error_code: u64,
    // -- pushed by the CPU --
    pub rip: u64,
    pub cs: u64,
    pub rflags: u64,
    pub rsp: u64,
    pub ss: u64,
}

// Vector numbers for the architectural exceptions we treat specially.
/// Debug exception (single-step / hardware breakpoint) — the user-mode
/// debugger's single-step engine ([`crate::ke::debug`]) handles these.
pub const VECTOR_DEBUG: u8 = 1;
/// Breakpoint (`int3`) exception.
pub const VECTOR_BREAKPOINT: u8 = 3;
pub const VECTOR_NMI: u8 = 2;
pub const VECTOR_DOUBLE_FAULT: u8 = 8;
pub const VECTOR_GP_FAULT: u8 = 13;
pub const VECTOR_PAGE_FAULT: u8 = 14;
pub const VECTOR_MCE: u8 = 18;
/// APIC clock tick — vector 0xD1, exactly NT's clock vector (IRQL 13:
/// vector >> 4). See ke::irql for the priority mapping.
pub const VECTOR_CLOCK: u8 = 0xD1;
/// DPC/dispatch software interrupt — IRQL 2 ⇒ vector 0x2F's range; NT uses
/// 0x2F... we self-IPI this vector to drain the DPC queue.
pub const VECTOR_DPC: u8 = 0x2F;
/// APIC spurious vector (low 4 bits must be 0xF on old APICs): 0xFF.
pub const VECTOR_SPURIOUS: u8 = 0xFF;

global_asm!(
    r#"
// ---------------------------------------------------------------------
// 256 interrupt entry stubs at a fixed 16-byte stride.
//
// Vectors where the CPU pushes an error code (8,10..14,17,21,29,30) skip
// the dummy push so the frame stays uniform. Max stub size is 12 bytes
// (push imm8/imm32 + push imm + jmp rel32), comfortably under the stride.
// ---------------------------------------------------------------------
.section .text
.balign 16
.global ki_vector_stubs
ki_vector_stubs:
.set vec, 0
.rept 256
    .balign 16
    .if (vec == 8) | (vec == 10) | (vec == 11) | (vec == 12) | (vec == 13) | (vec == 14) | (vec == 17) | (vec == 21) | (vec == 29) | (vec == 30)
        // CPU already pushed an error code
    .else
        push 0
    .endif
    push vec
    jmp ki_trap_common
    .set vec, vec + 1
.endr

// ---------------------------------------------------------------------
// Common trap prologue/epilogue.
// On entry the stack holds: [vector][error][rip][cs][rflags][rsp][ss].
// We spill the 15 GP registers so the frame matches KtrapFrame, clear DF
// per the SysV ABI, and hand &frame to Rust in rdi.
// ---------------------------------------------------------------------
.balign 16
ki_trap_common:
    push r15
    push r14
    push r13
    push r12
    push r11
    push r10
    push r9
    push r8
    push rdi
    push rsi
    push rbp
    push rbx
    push rdx
    push rcx
    push rax
    // If the trap came from ring 3, the active GS base is the user TEB; swap
    // in the kernel KPCR before any gs-relative kernel access. CS sits at
    // [rsp+0x90] now (15 GP regs + vector + error code below the HW frame).
    // Ring-0 traps (e.g. a timer during a syscall) keep GS = KPCR, so skip.
    test byte ptr [rsp + 0x90], 3
    jz   1f
    swapgs
1:
    cld
    mov  rdi, rsp
    call ki_dispatch_trap
    // Symmetric: restore the user GS base if we are returning to ring 3.
    // (Re-read CS from the frame: a context switch may have changed it.)
    test byte ptr [rsp + 0x90], 3
    jz   2f
    swapgs
2:
    pop  rax
    pop  rcx
    pop  rdx
    pop  rbx
    pop  rbp
    pop  rsi
    pop  rdi
    pop  r8
    pop  r9
    pop  r10
    pop  r11
    pop  r12
    pop  r13
    pop  r14
    pop  r15
    add  rsp, 16        // drop vector + error code
    iretq
"#
);

unsafe extern "C" {
    /// Base of the stub array; stub for vector `v` is at `+ v*16`.
    pub fn ki_vector_stubs();
}

/// Central trap router — every interrupt and exception lands here with a
/// fully populated frame. The fast paths (clock, DPC) are at the top.
#[unsafe(no_mangle)]
extern "C" fn ki_dispatch_trap(frame: &mut KtrapFrame) {
    let vector = frame.vector as u8;
    match vector {
        VECTOR_CLOCK => {
            crate::ke::scheduler::ki_clock_tick(frame);
            crate::hal::apic::eoi();
        }
        VECTOR_DPC => {
            // EOI *before* the dispatch work: ki_dispatch_interrupt may
            // context-switch away, and the next thread must not run with
            // this vector still in-service (it would mask further dispatch
            // interrupts until we eventually iretq).
            crate::hal::apic::eoi();
            crate::ke::scheduler::ki_dispatch_interrupt();
        }
        VECTOR_SPURIOUS => {
            // Spurious APIC interrupt: no EOI by definition.
        }
        VECTOR_PAGE_FAULT => {
            // CR2 holds the faulting linear address.
            let cr2: u64;
            unsafe {
                core::arch::asm!("mov {}, cr2", out(reg) cr2, options(nomem, nostack));
            }
            // A page fault from ring 3 (saved CS RPL 3) is a user-program bug,
            // not a kernel one: terminate the faulting thread instead of
            // bugchecking, so one bad process doesn't take down the system.
            if frame.cs & 3 == 3 {
                kd_println!(
                    "!! user page fault: va={:#018X} err={:#X} rip={:#018X} -> terminating thread",
                    cr2,
                    frame.error_code,
                    frame.rip
                );
                unsafe {
                    // A crashing child can leave console state (e.g. raw input
                    // mode) changed; restore it for the launching shell.
                    crate::init::on_user_thread_exit(
                        crate::ke::pcr::ke_get_current_thread() as u64,
                    );
                    crate::ke::scheduler::ki_terminate_current_thread()
                };
            }
            kd_println!(
                "!! page fault: va={:#018X} err={:#X} rip={:#018X}",
                cr2,
                frame.error_code,
                frame.rip
            );
            bugcheck::ke_bug_check_ex(
                bugcheck::PAGE_FAULT_IN_NONPAGED_AREA,
                cr2,
                frame.error_code,
                frame.rip,
                0,
            );
        }
        VECTOR_DOUBLE_FAULT => {
            // Running on the IST1 emergency stack; the original RSP is in
            // the frame. Unrecoverable by definition.
            bugcheck::ke_bug_check_ex(
                bugcheck::KMODE_EXCEPTION_NOT_HANDLED,
                vector as u64,
                frame.rip,
                frame.rsp,
                0,
            );
        }
        VECTOR_DEBUG => {
            // Single-step trap from the debugger's tracer.
            crate::ke::debug::on_single_step(frame);
        }
        VECTOR_BREAKPOINT => {
            crate::ke::debug::on_breakpoint(frame);
        }
        v if v < 32 => {
            // An architectural exception. If it came from ring 3 (saved CS has
            // RPL 3), it is a fault in a user program: terminate that thread
            // rather than crashing the kernel (no SEH dispatch yet). A real OS
            // would raise an exception the program could handle; for us, an
            // unhandled user fault ends the process thread and its join object
            // signals, so the launcher wakes. Kernel-mode faults stay fatal.
            if frame.cs & 3 == 3 {
                kd_println!(
                    "!! user fault vec={} err={:#X} rip={:#018X} rsp={:#018X} rcx={:#x} rdx={:#x} r8={:#x} r9={:#x} -> terminating thread",
                    v,
                    frame.error_code,
                    frame.rip,
                    frame.rsp,
                    frame.rcx,
                    frame.rdx,
                    frame.r8,
                    frame.r9
                );
                unsafe {
                    // A crashing child can leave console state (e.g. raw input
                    // mode) changed; restore it for the launching shell.
                    crate::init::on_user_thread_exit(
                        crate::ke::pcr::ke_get_current_thread() as u64,
                    );
                    crate::ke::scheduler::ki_terminate_current_thread()
                };
            }
            kd_println!(
                "!! exception vec={} err={:#X} rip={:#018X} rsp={:#018X}",
                v,
                frame.error_code,
                frame.rip,
                frame.rsp
            );
            bugcheck::ke_bug_check_ex(
                bugcheck::KMODE_EXCEPTION_NOT_HANDLED,
                v as u64,
                frame.rip,
                frame.error_code,
                0,
            );
        }
        _ => {
            // Unexpected device vector: acknowledge and continue; a real
            // system would track these as spurious-interrupt statistics.
            crate::hal::apic::eoi();
        }
    }
}
