//! User-mode exception delivery — the ring-3 half of `KiDispatchException`.
//!
//! On real Windows, an exception raised by user code (an access violation,
//! a divide-by-zero, …) does not kill the process outright: the kernel
//! builds an `EXCEPTION_RECORD` and a `CONTEXT` on the user stack and
//! resumes the thread at `ntdll!KiUserExceptionDispatcher`, which runs the
//! structured/vectored exception-handling machinery in user mode. Only when
//! nothing handles the exception does the thread die.
//!
//! This module is that delivery, at our scale: [`dispatch_exception`]
//! rewrites the trap frame so the normal trap epilogue "returns" to the
//! dispatcher thunk in the ntdll stub page; if nothing handles it there,
//! the shim terminates the thread — the exact fate unhandled exceptions
//! always had here. Handled exceptions come back through `NtContinue`
//! ([`nt_continue`]), which resumes the thread at a full register state.
//!
//! Stage-1 scope: the `CONTEXT` carries the integer registers, segment
//! selectors, RFLAGS and MxCsr fields at their real winnt.h offsets (so the
//! layout is honest and debuggers parse it), but the FP/SSE state
//! (`FltSave`) is left zeroed — the kernel is soft-float, so user XMM state
//! survives the round trip through the kernel untouched.

use super::traps::KtrapFrame;
use crate::ke::selectors::{KGDT64_R3_CODE, KGDT64_R3_DATA};

// ---------------------------------------------------------------------------
// NTSTATUS-shaped exception codes (ntstatus.h)
// ---------------------------------------------------------------------------

pub const STATUS_ACCESS_VIOLATION: u32 = 0xC000_0005;
pub const STATUS_ILLEGAL_INSTRUCTION: u32 = 0xC000_001D;
pub const STATUS_INTEGER_DIVIDE_BY_ZERO: u32 = 0xC000_0094;
pub const STATUS_INTEGER_OVERFLOW: u32 = 0xC000_0095;
pub const STATUS_STACK_OVERFLOW: u32 = 0xC000_00FD;

/// Map an architectural exception vector to its NT exception code, plus the
/// `ExceptionInformation[]` payload NT would attach. `cr2`/`error_code` are
/// the page-fault specifics (ignored for other vectors). Returns `None` for
/// vectors that must never reach user mode (NMI, double fault).
pub fn exception_code_for(vector: u8, error_code: u64, cr2: u64) -> Option<(u32, [u64; 2], usize)> {
    let (code, info, n): (u32, [u64; 2], usize) = match vector {
        0 => (STATUS_INTEGER_DIVIDE_BY_ZERO, [0; 2], 0),
        4 => (STATUS_INTEGER_OVERFLOW, [0; 2], 0),
        6 => (STATUS_ILLEGAL_INSTRUCTION, [0; 2], 0),
        12 => (STATUS_STACK_OVERFLOW, [0; 2], 0),
        14 => {
            // ExceptionInformation[0]: 0=read, 1=write, 8=execute; [1]: VA.
            let access: u64 = if error_code & 0x10 != 0 {
                8
            } else if error_code & 2 != 0 {
                1
            } else {
                0
            };
            (STATUS_ACCESS_VIOLATION, [access, cr2], 2)
        }
        13 => (STATUS_ACCESS_VIOLATION, [0; 2], 0),
        2 | 8 => return None, // NMI / double fault: kernel territory
        _ => (STATUS_ACCESS_VIOLATION, [0; 2], 0),
    };
    Some((code, info, n))
}

// ---------------------------------------------------------------------------
// winnt.h x64 layout: EXCEPTION_RECORD and CONTEXT
// ---------------------------------------------------------------------------

/// `sizeof(EXCEPTION_RECORD)` on x64: header through `ExceptionInformation[15]`.
const REC_SIZE: usize = 0x98;
/// `sizeof(CONTEXT)` on x64. Only the fields through `Rip` (0xF0) plus the
/// header are populated; `FltSave` et al. stay zero (stage-1 note above).
const CTX_SIZE: usize = 0x4D0;

// CONTEXT field offsets (winnt.h, x64).
const CTX_FLAGS: usize = 0x30;
const CTX_MX_CSR: usize = 0x34;
const CTX_SEG_CS: usize = 0x38;
const CTX_SEG_SS: usize = 0x42;
const CTX_EFLAGS: usize = 0x44;
const CTX_RAX: usize = 0x78;
const CTX_RCX: usize = 0x80;
const CTX_RDX: usize = 0x88;
const CTX_RBX: usize = 0x90;
const CTX_RSP: usize = 0x98;
const CTX_RBP: usize = 0xA0;
const CTX_RSI: usize = 0xA8;
const CTX_RDI: usize = 0xB0;
const CTX_R8: usize = 0xB8;
const CTX_R9: usize = 0xC0;
const CTX_R10: usize = 0xC8;
const CTX_R11: usize = 0xD0;
const CTX_R12: usize = 0xD8;
const CTX_R13: usize = 0xE0;
const CTX_R14: usize = 0xE8;
const CTX_R15: usize = 0xF0;
const CTX_RIP: usize = 0xF8;

/// `CONTEXT_AMD64 | CONTEXT_CONTROL | CONTEXT_INTEGER | CONTEXT_SEGMENTS`.
const CONTEXT_FLAGS_VALUE: u32 = 0x0010_0007;

/// The register state [`nt_continue`] resumes with — the fields the kernel
/// reads back out of the user `CONTEXT`. Layout is ours (not winnt order);
/// the asm consumer indexes it via `offset_of!`.
#[repr(C)]
pub struct UserResume {
    pub rax: u64,
    pub rbx: u64,
    pub rcx: u64,
    pub rdx: u64,
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
    pub rip: u64,
    pub cs: u64,
    pub rflags: u64,
    pub rsp: u64,
    pub ss: u64,
}

/// RFLAGS bits a user context may carry: the arithmetic flags
/// (CF/PF/AF/ZF/SF/DF/OF) plus TF (the debugger's single-step works through
/// dispatched contexts). IF and the reserved bit 1 are forced on — an iretq
/// at CPL 0 sets IF regardless of IOPL, so user mode always resumes with
/// interrupts enabled, exactly like `ki_enter_user_mode`.
const RFLAGS_USER_MASK: u64 = 0x0DD5;
const RFLAGS_FORCED: u64 = 0x0202;

/// Attempt to deliver an exception to the faulting user thread. On success
/// the trap frame has been rewritten so the trap epilogue resumes the thread
/// at the `KiUserExceptionDispatcher` thunk with
/// `rcx = &EXCEPTION_RECORD, rdx = &CONTEXT` — the exact entry contract of
/// the real thing. Returns false (caller falls back to terminating the
/// thread) when delivery is impossible: no dispatcher installed yet, or the
/// user stack can't hold the frame (e.g. it *is* the faulting page).
pub fn dispatch_exception(frame: &mut KtrapFrame, code: u32, info: [u64; 2], nparams: usize) -> bool {
    let stub = crate::ldr::ntdll::user_exception_dispatcher_va();
    if stub == 0 {
        return false;
    }
    // Layout on the user stack: [CONTEXT at 16-alignment][EXCEPTION_RECORD]
    // above it, and one fake return address below — so RSP at entry to the
    // dispatcher is 8 mod 16, the exact post-`call` state a Rust/C function
    // prologue expects.
    let ctx_va = frame.rsp.wrapping_sub((CTX_SIZE + REC_SIZE + 8) as u64) & !0xF;
    let rec_va = ctx_va + CTX_SIZE as u64;
    let new_rsp = ctx_va - 8;
    if new_rsp >= frame.rsp || new_rsp < 0x1_0000 {
        return false; // stack pointer nonsense — don't compound the fault
    }
    // The real validity check: the frame must fit in *writable user* memory
    // below the old RSP. (No address-half assumptions — kernel-AS user
    // threads legitimately run on high-half window-backed stacks.)
    let span = (frame.rsp - new_rsp) as usize;
    if crate::mm::virt::probe_for_write(new_rsp, span, 1).is_err() {
        return false;
    }
    crate::mm::virt::user_access_begin();
    unsafe {
        // Zero the whole CONTEXT first (FltSave, debug registers, padding).
        core::ptr::write_bytes(ctx_va as *mut u8, 0, CTX_SIZE);
        let w16 = |off: usize, v: u16| ((ctx_va + off as u64) as *mut u16).write(v);
        let w32 = |off: usize, v: u32| ((ctx_va + off as u64) as *mut u32).write(v);
        let w64 = |off: usize, v: u64| ((ctx_va + off as u64) as *mut u64).write(v);
        w32(CTX_FLAGS, CONTEXT_FLAGS_VALUE);
        // The default x87/SSE control value user code expects; the rest of
        // the FP state stays zero (stage-1 note, module docs).
        w32(CTX_MX_CSR, 0x1F80);
        w16(CTX_SEG_CS, frame.cs as u16);
        w16(CTX_SEG_SS, frame.ss as u16);
        w32(CTX_EFLAGS, frame.rflags as u32);
        w64(CTX_RAX, frame.rax);
        w64(CTX_RCX, frame.rcx);
        w64(CTX_RDX, frame.rdx);
        w64(CTX_RBX, frame.rbx);
        w64(CTX_RSP, frame.rsp);
        w64(CTX_RBP, frame.rbp);
        w64(CTX_RSI, frame.rsi);
        w64(CTX_RDI, frame.rdi);
        w64(CTX_R8, frame.r8);
        w64(CTX_R9, frame.r9);
        w64(CTX_R10, frame.r10);
        w64(CTX_R11, frame.r11);
        w64(CTX_R12, frame.r12);
        w64(CTX_R13, frame.r13);
        w64(CTX_R14, frame.r14);
        w64(CTX_R15, frame.r15);
        w64(CTX_RIP, frame.rip);

        // EXCEPTION_RECORD: code, flags(0), chained record(NULL), address,
        // NumberParameters + information.
        core::ptr::write_bytes(rec_va as *mut u8, 0, REC_SIZE);
        let r32 = |off: usize, v: u32| ((rec_va + off as u64) as *mut u32).write(v);
        let r64 = |off: usize, v: u64| ((rec_va + off as u64) as *mut u64).write(v);
        r32(0x00, code);
        r64(0x10, frame.rip); // ExceptionAddress
        r32(0x18, nparams as u32);
        r64(0x20, info[0]);
        r64(0x28, info[1]);

        // Fake return address so the dispatcher entry is call-shaped.
        (new_rsp as *mut u64).write(0);
    }
    crate::mm::virt::user_access_end();

    // Redirect the trap return into the dispatcher with the ABI arguments.
    frame.rcx = rec_va;
    frame.rdx = ctx_va;
    frame.rip = stub;
    frame.rsp = new_rsp;
    true
}

/// `NtContinue(Context)`: resume the calling thread at a previously
/// dispatched (and possibly handler-modified) `CONTEXT`. Returns an
/// NTSTATUS only when the context is rejected; on success it never returns.
pub fn nt_continue(ctx_va: u64) -> u64 {
    if crate::mm::virt::probe_for_read(ctx_va, CTX_SIZE, 16).is_err() {
        return crate::rtl::NtStatus::ACCESS_VIOLATION.0 as u64;
    }
    crate::mm::virt::user_access_begin();
    let r = |off: usize| unsafe { ((ctx_va + off as u64) as *const u64).read() };
    let mut resume = UserResume {
        rax: r(CTX_RAX),
        rbx: r(CTX_RBX),
        rcx: r(CTX_RCX),
        rdx: r(CTX_RDX),
        rbp: r(CTX_RBP),
        rsi: r(CTX_RSI),
        rdi: r(CTX_RDI),
        r8: r(CTX_R8),
        r9: r(CTX_R9),
        r10: r(CTX_R10),
        r11: r(CTX_R11),
        r12: r(CTX_R12),
        r13: r(CTX_R13),
        r14: r(CTX_R14),
        r15: r(CTX_R15),
        rip: r(CTX_RIP),
        cs: 0, // forced below
        rflags: unsafe { ((ctx_va + CTX_EFLAGS as u64) as *const u32).read() as u64 },
        rsp: r(CTX_RSP),
        ss: 0, // forced below
    };
    crate::mm::virt::user_access_end();

    // The context may only describe a return to ring 3: RIP must be
    // user-executable, RSP user-writable (the probe checks the U/S bit, so
    // supervisor pages in either address half are rejected).
    if resume.rip == 0
        || crate::mm::virt::probe_for_read(resume.rip, 1, 1).is_err()
        || resume.rsp < 0x1_0000
        || crate::mm::virt::probe_for_write(resume.rsp - 8, 8, 8).is_err()
    {
        return crate::rtl::NtStatus::ACCESS_VIOLATION.0 as u64;
    }
    // Segments and flags are forced, not trusted (a context can't be a
    // privilege-escalation vector).
    resume.cs = (KGDT64_R3_CODE | 3) as u64;
    resume.ss = (KGDT64_R3_DATA | 3) as u64;
    resume.rflags = (resume.rflags & RFLAGS_USER_MASK) | RFLAGS_FORCED;

    // Never returns: resets the kernel stack and iretqs to the context.
    unsafe { super::usermode::ki_continue_user_mode(&resume) }
}
