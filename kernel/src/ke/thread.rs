//! `KTHREAD` — the kernel thread object and the context switch.
//!
//! A thread, to Ke, is: a kernel stack, a saved stack pointer when not
//! running, a scheduling state/priority, and a dispatcher header so other
//! threads can wait on its termination. Everything higher-level (handles,
//! a containing process, user mode) belongs to Ps' `ETHREAD`, which embeds
//! this structure — same split as NT.
//!
//! ## How the context switch works
//!
//! [`ki_swap_context`] is the only place a CPU changes threads. It is
//! called at `DISPATCH_LEVEL` with the dispatcher lock held, and performs
//! the minimal architectural switch:
//!
//! ```text
//!   push rbp,rbx,r12..r15        ; callee-saved per SysV ABI — the caller
//!                                ;   already saved everything else
//!   mov [old.kernel_rsp], rsp    ; park the outgoing thread
//!   mov rsp, [new.kernel_rsp]    ; adopt the incoming thread's stack
//!   pop r15..r12,rbx,rbp
//!   ret                          ; "return" on the NEW thread's stack
//! ```
//!
//! A *new* thread's stack is pre-forged ([`Kthread::initialize_stack`]) to
//! look exactly like a parked one whose `ret` lands in the
//! `ki_thread_startup` trampoline, which lowers IRQL to PASSIVE, enables
//! interrupts and calls the entry point. This "every thread resumes the
//! same way" property is what NT's `KiSwapContext`/`KiThreadStartup` pair
//! does, minus the legacy bits.

use crate::ke::dispatcher::{DispatcherHeader, DispatcherObjectType};
use crate::rtl::list::ListEntry;
use core::arch::naked_asm;
use core::mem::offset_of;

/// Scheduling states, same set as NT's `KTHREAD_STATE`.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThreadState {
    Initialized = 0,
    Ready = 1,
    Running = 2,
    /// On a wait list, off the ready queues.
    Waiting = 5,
    Terminated = 4,
}

/// `KWAIT_BLOCK` — one thread's claim on one dispatcher object.
///
/// A thread waiting on N objects links one block into each object's wait
/// list; the blocks all live inside the thread (`Kthread::wait_blocks`),
/// exactly as NT embeds `WaitBlock[]` in KTHREAD. When any object signals,
/// the satisfy logic reaches the thread through `thread` and consults all
/// of its blocks to decide whether the *whole* wait (WaitAll/WaitAny) is
/// now satisfiable.
#[repr(C)]
pub struct KwaitBlock {
    /// Linkage in the awaited object's wait list.
    pub wait_list_entry: ListEntry,
    /// Back-pointer to the waiting thread.
    pub thread: *mut Kthread,
    /// The object this block waits on (its `DispatcherHeader`).
    pub object: *mut DispatcherHeader,
    /// Reported as `WAIT_0 + wait_key` when this block satisfies a WaitAny.
    pub wait_key: u32,
    /// True while this block is linked into `object`'s wait list.
    pub active: bool,
}

impl KwaitBlock {
    pub const fn new() -> Self {
        KwaitBlock {
            wait_list_entry: ListEntry::new(),
            thread: core::ptr::null_mut(),
            object: core::ptr::null_mut(),
            wait_key: 0,
            active: false,
        }
    }
}

/// Whether a multi-object wait completes when *all* objects signal or when
/// *any* one does — `WAIT_TYPE` (`WaitAll`/`WaitAny`).
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitType {
    All = 0,
    Any = 1,
}

/// Usable wait objects per thread (`THREAD_WAIT_OBJECTS` is 3 on NT). One
/// extra block is reserved for the internal timeout timer.
pub const THREAD_WAIT_OBJECTS: usize = 3;
/// Wait-block array size: the usable objects plus the timeout slot.
pub const MAX_WAIT_BLOCKS: usize = THREAD_WAIT_OBJECTS + 1;
/// Index of the reserved timeout-timer wait block.
pub const TIMER_WAIT_BLOCK: usize = THREAD_WAIT_OBJECTS;

/// The kernel thread object.
#[repr(C)]
pub struct Kthread {
    /// Dispatcher header: signaled when the thread terminates, so
    /// `KeWaitForSingleObject(thread)` is "join".
    pub header: DispatcherHeader,
    /// Saved RSP while the thread is not running. Offset is baked into the
    /// context-switch assembly via `offset_of!` — keep this field's
    /// position stable relative to repr(C) layout.
    pub kernel_rsp: u64,
    /// Base (lowest address) of the kernel stack allocation.
    pub stack_base: u64,
    /// Top (initial RSP) of the kernel stack; also loaded into TSS.RSP0.
    pub stack_top: u64,
    pub state: ThreadState,
    /// 0–31; ready-queue index. 0 is reserved for the idle thread.
    pub priority: u8,
    /// Remaining clock ticks before preemption.
    pub quantum: i32,
    /// Ready-queue linkage (when Ready).
    pub ready_list_entry: ListEntry,
    /// Wait blocks — one per object the thread is currently waiting on,
    /// plus the reserved timeout slot ([`TIMER_WAIT_BLOCK`]).
    pub wait_blocks: [KwaitBlock; MAX_WAIT_BLOCKS],
    /// WaitAll vs WaitAny over the `wait_count` real objects.
    pub wait_type: WaitType,
    /// Number of real (non-timer) objects in this wait, 0..=THREAD_WAIT_OBJECTS.
    pub wait_count: usize,
    /// Embedded one-shot timer backing a wait timeout; armed iff `wait_timed`.
    pub wait_timer: crate::ke::dispatcher::Ktimer,
    /// True while `wait_timer` is armed for the current wait.
    pub wait_timed: bool,
    /// Status delivered to a satisfied wait (`WAIT_0 + key`, or TIMEOUT).
    pub wait_status: crate::rtl::NtStatus,
    /// Small monotone ID for diagnostics (CID lives in ETHREAD on NT).
    pub thread_id: u64,
    /// The address space (CR3) to load when this thread runs. 0 means the
    /// kernel address space (kernel threads, and user threads whose image is
    /// in the shared high-half window). A per-process user thread sets this
    /// to its process PML4 so the scheduler restores it on every switch-in.
    pub cr3: u64,
    /// Per-thread Win32 last-error code (the value behind
    /// `GetLastError`/`SetLastError`). On real NT this lives in
    /// `TEB.LastErrorValue`; without a TEB yet we keep it here, which is the
    /// same per-thread semantics.
    pub last_error: u32,
    /// Command line (ASCII) this thread's program sees, behind
    /// `GetCommandLine`/`__getmainargs`. Points at static bytes; 0/0 means
    /// "use the default". On real NT this lives in
    /// `PEB.ProcessParameters.CommandLine`.
    pub cmdline_ptr: u64,
    pub cmdline_len: u32,
    /// The program's `.mui` resource bytes (UI strings behind `LoadStringW`),
    /// per-thread because every process loads at the same image base, so a
    /// global base→mui registry would have a parent and its child collide.
    /// Points at static bytes; 0/0 means "fall back to the base registry".
    pub mui_ptr: u64,
    pub mui_len: u32,
    /// Exit code recorded when the thread terminates (set by `ExitProcess` /
    /// `NtTerminateThread`), read back by `GetExitCodeProcess`.
    pub exit_code: u32,
    /// This thread's user-mode GS base (its TEB, or the KPCR for TEB-less
    /// apps). 0 for kernel threads. The scheduler restores `IA32_KERNEL_GS_BASE`
    /// from this on switch-in, so each user thread returns to ring 3 with its
    /// own GS — letting multiple TEB processes (e.g. a CreateProcess parent and
    /// its child) coexist.
    pub gs_base: u64,
    /// This thread's standard handles (stdin/stdout/stderr), behind
    /// `GetStdHandle`. 0 means "not redirected - use the console default". Set
    /// from the parent's `STARTUPINFO` at process creation so `dir | sort` and
    /// `> file` route the child's streams to a pipe or file.
    pub std_handles: [u64; 3],
    /// Standard handles staged for the *next* child this thread creates
    /// (`SetStartupHandles`, consumed by the create-process path). 0 = default.
    pub child_std_handles: [u64; 3],
}

/// Default quantum in clock ticks (~ms at our tick rate). NT's default on
/// workstations is comparable after unit conversion.
pub const DEFAULT_QUANTUM: i32 = 10;
/// Default priority for system threads, like NT's `THREAD_BASE_PRIORITY` 8.
pub const DEFAULT_PRIORITY: u8 = 8;

impl Kthread {
    /// Construct an embryonic thread over a caller-provided stack range.
    /// The thread is not runnable until [`initialize_stack`] forges the
    /// switch frame and the scheduler readies it.
    pub fn new(thread_id: u64, stack_base: u64, stack_size: usize, priority: u8) -> Self {
        Kthread {
            header: DispatcherHeader::new(DispatcherObjectType::Thread, 0),
            kernel_rsp: 0,
            stack_base,
            stack_top: stack_base + stack_size as u64,
            state: ThreadState::Initialized,
            priority,
            quantum: DEFAULT_QUANTUM,
            ready_list_entry: ListEntry::new(),
            wait_blocks: [const { KwaitBlock::new() }; MAX_WAIT_BLOCKS],
            wait_type: WaitType::Any,
            wait_count: 0,
            wait_timer: crate::ke::dispatcher::Ktimer::new(),
            wait_timed: false,
            wait_status: crate::rtl::NtStatus::SUCCESS,
            thread_id,
            cr3: 0,
            last_error: 0,
            cmdline_ptr: 0,
            cmdline_len: 0,
            mui_ptr: 0,
            mui_len: 0,
            exit_code: 0,
            gs_base: 0,
            std_handles: [0; 3],
            child_std_handles: [0; 3],
        }
    }

    /// Forge the initial switch frame so the first `ki_swap_context` into
    /// this thread "returns" into `ki_thread_startup` with the entry point
    /// in r15 and its context argument in r14 (see module docs).
    ///
    /// # Safety
    /// The stack range must be valid, writable, and exclusively owned.
    pub unsafe fn initialize_stack(
        &mut self,
        entry: extern "C" fn(*mut core::ffi::c_void) -> !,
        context: *mut core::ffi::c_void,
    ) {
        unsafe {
            // Stack-alignment contract: the SysV/win64 ABI requires that on
            // entry to a function RSP ≡ 8 (mod 16) — the state right after a
            // `call` pushes its 8-byte return address onto a 16-aligned
            // stack. `ki_swap_context` resumes this thread by popping 6
            // callee-saved registers (48 bytes) and `ret`ting into
            // `ki_thread_startup` (8 bytes) — 56 bytes total — after which
            // RSP equals the value just below where we start laying out the
            // frame. We therefore base that value at `(top & !0xF) - 8` so
            // `ki_thread_startup` (and the `jmp`-tail-called `ki_thread_begin`)
            // see the ABI-correct 8-mod-16 alignment. Getting this wrong is
            // invisible to our soft-float kernel but faults the first
            // `movaps` in Windows-ABI driver code.
            //
            // Frame, laid out descending from that base:
            //   [ki_thread_startup][rbp][rbx][r12][r13][r14=ctx][r15=entry]
            let mut sp = ((self.stack_top & !0xF) - 8) as *mut u64;
            sp = sp.sub(1);
            sp.write(ki_thread_startup as *const () as u64);
            sp = sp.sub(1);
            sp.write(0); // rbp
            sp = sp.sub(1);
            sp.write(0); // rbx
            sp = sp.sub(1);
            sp.write(0); // r12
            sp = sp.sub(1);
            sp.write(0); // r13
            sp = sp.sub(1);
            sp.write(context as u64); // r14
            sp = sp.sub(1);
            sp.write(entry as usize as u64); // r15
            self.kernel_rsp = sp as u64;
        }
    }
}

/// Byte offset of `kernel_rsp`, consumed by the switch assembly.
const KERNEL_RSP_OFFSET: usize = offset_of!(Kthread, kernel_rsp);

/// `KiSwapContext` — switch the CPU from `old` to `new`.
///
/// # Safety
/// Must be called at DISPATCH_LEVEL with the dispatcher lock held (the
/// lock "travels" with the switch and is released by the resumed side).
/// `old` must be the running thread, `new` a parked-or-forged one.
#[unsafe(naked)]
pub unsafe extern "C" fn ki_swap_context(old: *mut Kthread, new: *mut Kthread) {
    naked_asm!(
        // SysV: rdi = old, rsi = new. Save callee-saved state; everything
        // volatile is dead across the call boundary by ABI contract.
        "push rbp",
        "push rbx",
        "push r12",
        "push r13",
        "push r14",
        "push r15",
        "mov [rdi + {rsp_off}], rsp", // park outgoing
        "mov rsp, [rsi + {rsp_off}]", // adopt incoming
        "pop r15",
        "pop r14",
        "pop r13",
        "pop r12",
        "pop rbx",
        "pop rbp",
        "ret",
        rsp_off = const KERNEL_RSP_OFFSET,
    )
}

/// First instructions of every new thread (`KiThreadStartup`): forward the
/// forged r15/r14 (entry/context) to the Rust-side begin routine, which
/// finishes scheduler bookkeeping and drops to PASSIVE_LEVEL.
#[unsafe(naked)]
unsafe extern "C" fn ki_thread_startup() -> ! {
    naked_asm!(
        "mov rdi, r15", // entry point
        "mov rsi, r14", // context
        "jmp {begin}",
        begin = sym ki_thread_begin,
    )
}

/// Rust half of thread startup. Runs on the new thread's stack, still at
/// DISPATCH_LEVEL with the dispatcher lock word held by the switched-away
/// thread's acquisition — release it, drop IRQL, enable interrupts, go.
extern "C" fn ki_thread_begin(
    entry: extern "C" fn(*mut core::ffi::c_void) -> !,
    context: *mut core::ffi::c_void,
) -> ! {
    crate::ke::scheduler::ki_finish_switch_to_new_thread();
    entry(context)
    // entry never returns; threads end via ps::ps_terminate_system_thread,
    // which signals the dispatcher header and switches away for good.
}
