//! System-call entry — the user→kernel boundary (`KiSystemCall64`).
//!
//! Console applications are user-mode programs: they run in ring 3 and reach
//! the kernel through the `syscall` instruction. This module is that gate.
//!
//! ## How `syscall`/`sysret` are wired (identical to x64 Windows)
//!
//! `syscall` is configured by four MSRs:
//! * `EFER.SCE` (bit 0) — enable the instruction.
//! * `STAR[47:32]` — the kernel CS/SS base loaded on `syscall`. With our NT
//!   selector layout (`KGDT64_R0_CODE = 0x10`, `R0_DATA = 0x18 = 0x10+8`)
//!   this is `0x10`, exactly as NT programs it.
//! * `STAR[63:48]` — the user CS/SS base for `sysret`: the CPU loads
//!   `SS = base+8` and `CS = base+16` (RPL forced to 3). We set it to
//!   `KGDT64_R3_CMCODE = 0x20`, so `SS = 0x28` (`R3_DATA`) and
//!   `CS = 0x30` (`R3_CODE`) — again the NT value. This is *why* the GDT was
//!   laid out the way it was back in `ke::gdt`.
//! * `LSTAR` — the entry RIP ([`ki_system_call64`]).
//! * `FMASK` — RFLAGS bits cleared on entry (IF, so we enter with interrupts
//!   off; TF, DF, AC).
//!
//! ## Calling convention
//!
//! On `syscall` the CPU puts the return RIP in `RCX` and RFLAGS in `R11`
//! (so a handler must preserve both). Windows passes the service number in
//! `EAX` and arguments in `R10, RDX, R8, R9` (the user stub copies what
//! would be `RCX` into `R10`, since `syscall` clobbers `RCX`). We marshal
//! those into the SysV registers and call [`ki_syscall_dispatch`], whose
//! return value goes back to the caller in `RAX`.
//!
//! `syscall` does **not** switch stacks, so the entry stub does it by hand:
//! `swapgs` to reach the KPCR, stash the user RSP, load the kernel stack
//! from `gs:[syscall_kernel_stack]`.

use crate::ke::pcr::{self, KPCR_SYSCALL_KERNEL_STACK, KPCR_SYSCALL_USER_STACK};
use crate::ke::selectors::{KGDT64_R0_CODE, KGDT64_R3_CMCODE};
use core::arch::naked_asm;

const IA32_EFER: u32 = 0xC000_0080;
const IA32_STAR: u32 = 0xC000_0081;
const IA32_LSTAR: u32 = 0xC000_0082;
const IA32_FMASK: u32 = 0xC000_0084;
const EFER_SCE: u64 = 1 << 0;

/// Program the `syscall`/`sysret` MSRs and enable the instruction. Phase-0,
/// per processor. Must run after the GDT is loaded (it references the GDT's
/// selector layout) and after the KPCR/GS base is set (the entry stub uses
/// `gs:` and `swapgs`).
pub fn init() {
    unsafe {
        // STAR: [47:32] = kernel base (R0_CODE), [63:48] = user base (R3_CMCODE).
        let star = ((KGDT64_R0_CODE as u64) << 32) | ((KGDT64_R3_CMCODE as u64) << 48);
        pcr::wrmsr(IA32_STAR, star);
        pcr::wrmsr(IA32_LSTAR, ki_system_call64 as *const () as u64);
        // Clear IF | TF | DF | AC on entry (enter the kernel with interrupts
        // masked until the handler chooses otherwise).
        pcr::wrmsr(IA32_FMASK, (1 << 9) | (1 << 8) | (1 << 10) | (1 << 18));
        // Enable SCE.
        let efer = pcr::rdmsr(IA32_EFER);
        pcr::wrmsr(IA32_EFER, efer | EFER_SCE);
    }
}

/// `KiSystemCall64` — the `syscall` entry point.
///
/// Naked because the prologue/epilogue must be exact: we own the stack
/// switch and the user-state save/restore. See the module docs for the
/// register and MSR contract.
#[unsafe(naked)]
unsafe extern "C" fn ki_system_call64() {
    naked_asm!(
        // Reach the kernel GS (KPCR) and switch off the user stack. The
        // per-CPU user-rsp slot is used only transiently here — interrupts
        // are masked (FMASK cleared IF), so nothing preempts between this
        // stash and the push below.
        "swapgs",
        "mov gs:[{user_rsp}], rsp",
        "mov rsp, gs:[{kern_rsp}]",
        // Save the user RSP on the *kernel stack* (per-thread), not the
        // per-CPU slot: a blocking syscall (e.g. Sleep) can switch to another
        // thread that enters its own syscall and overwrites the per-CPU slot
        // before we resume. The kernel stack survives the block.
        "push qword ptr gs:[{user_rsp}]", // [+32] saved user RSP
        // Preserve the user return state the C dispatcher would clobber.
        "push rcx", // [+24] user RIP
        "push r11", // [+16] user RFLAGS
        // RDI and RSI are *nonvolatile* in the Microsoft x64 ABI the user
        // program follows, but *volatile* in the SysV ABI the kernel's C
        // code uses — so the dispatcher would clobber them. (RBX/RBP/R12-R15
        // are SysV-nonvolatile too, so the C callee preserves them; XMM6-15
        // are untouched because the kernel is soft-float.)
        "push rdi", // [+8]
        "push rsi", // [+0]
        // Marshal Windows syscall args (eax, r10, rdx, r8, r9) into SysV
        // (rdi, rsi, rdx, rcx, r8). Read r8 into rcx before overwriting r8.
        "mov rdi, rax", // service number
        "mov rsi, r10", // arg1
        "mov rcx, r8",  // arg3
        "mov r8, r9",   // arg4
        // rdx already holds arg2. Five 8-byte pushes leave RSP 8-mod-16; one
        // more 8-byte adjustment makes it 16-aligned before the call.
        "sub rsp, 8",
        "call {dispatch}",
        "add rsp, 8",
        // rax = return value (stays in rax for the caller).
        "pop rsi", // restore user RSI
        "pop rdi", // restore user RDI
        "pop r11", // user RFLAGS
        "pop rcx", // user RIP
        "pop rsp", // restore user RSP (per-thread, survived any block)
        "swapgs",
        "sysretq",
        user_rsp = const KPCR_SYSCALL_USER_STACK,
        kern_rsp = const KPCR_SYSCALL_KERNEL_STACK,
        dispatch = sym ki_syscall_dispatch,
    )
}

/// Number of entries in the system service dispatch table.
pub const SSDT_SIZE: usize = 48;

/// A system service: receives up to four `u64` arguments and returns a
/// `u64` (an NTSTATUS or a value). Uniform signature keeps the dispatch
/// marshalling trivial; individual services interpret the arguments.
pub type SystemService = extern "C" fn(u64, u64, u64, u64) -> u64;

/// The System Service Dispatch Table (NT's `KiServiceTable`). Indexed by the
/// service number in `EAX`. Populated by [`register_service`] during init.
static mut SSDT: [Option<SystemService>; SSDT_SIZE] = [None; SSDT_SIZE];

/// Install a service at `index`. Phase-0, single-threaded.
pub fn register_service(index: usize, service: SystemService) {
    assert!(index < SSDT_SIZE);
    // Element-pointer arithmetic avoids forming a reference to the static.
    unsafe {
        let base = (&raw mut SSDT) as *mut Option<SystemService>;
        base.add(index).write(Some(service));
    }
}

/// Rust-side dispatch: validate the service number and forward. Runs on the
/// kernel stack with interrupts disabled (FMASK cleared IF). The C ABI here
/// matches the marshalling done by [`ki_system_call64`].
extern "C" fn ki_syscall_dispatch(index: u64, a1: u64, a2: u64, a3: u64, a4: u64) -> u64 {
    let i = index as usize;
    // SAFETY: SSDT is written once at init, read-only thereafter.
    let service = if i < SSDT_SIZE {
        unsafe { ((&raw const SSDT) as *const Option<SystemService>).add(i).read() }
    } else {
        None
    };
    match service {
        Some(f) => f(a1, a2, a3, a4),
        // STATUS_INVALID_SYSTEM_SERVICE
        None => 0xC000_001C,
    }
}
