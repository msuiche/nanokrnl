//! A minimal in-kernel user-mode debugger: single-step tracing.
//!
//! Single-stepping uses the x86 **Trap Flag** (`RFLAGS.TF`, bit 8): with TF
//! set, the CPU raises a `#DB` (vector 1) *after every instruction*. We set
//! TF when entering ring 3 for a traced thread; the `#DB` handler
//! ([`on_single_step`]) inspects the trap frame and decides whether to keep
//! stepping (leave TF set in the saved RFLAGS) or stop (clear it).
//!
//! Rather than dump millions of instructions, the tracer logs an **API
//! call/return trace**: it knows the address ranges of the program image and
//! of the shim libraries (`kernel32`/`msvcrt`/`ntdll`), and logs only the
//! transitions between them — a `CALL` (with argument registers) when control
//! leaves the image for a library, and a `RET` (with the return value) when it
//! comes back. That bounds the output to the number of API calls and shows
//! exactly what a third-party binary asks the OS to do, and what it gets back
//! — the view needed to see where `where.exe` diverges into its error path.
//!
//! **Hybrid stepping (the speed trick).** Single-stepping *every* instruction
//! means one `#DB` (a full ring3↔ring0 round trip) per instruction — far too
//! slow to leave armed. So we only single-step inside the *program image*. When
//! the image calls into a library we read the return address off the user stack
//! (the value the `CALL` just pushed), arm it in **`DR0`** as a hardware
//! execution breakpoint, and **clear `TF`** — the whole library body (and its
//! CRT internals, the bulk of the instructions) then runs at native speed, and
//! `#DB` fires again only when control returns to that image address. Cost is
//! ~one trap per API call/return, not per instruction (≈1000× fewer traps),
//! which is why the full trace of `where.exe` runs in the normal boot time.
//! Using `DR0` (not an `int3` patch) means we never write to user code pages.

use crate::kd_println;
use crate::ke::spinlock::SpinLock;
use crate::ke::traps::KtrapFrame;

const RFLAGS_TF: u64 = 1 << 8;
const MAX_MODULES: usize = 8;

#[derive(Clone, Copy)]
struct Module {
    name: &'static str,
    base: u64,
    size: u64,
    /// True for shim libraries (kernel32/msvcrt/ntdll); false for the program
    /// image. Drives the CALL/RET classification.
    is_lib: bool,
}

struct DebugState {
    /// Set TF on the next ring-3 entry (consumed by [`take_armed`]).
    armed: bool,
    /// Actively single-stepping.
    tracing: bool,
    /// Remaining single-step budget; tracing stops (TF cleared) at 0.
    budget: u64,
    /// Total steps taken (diagnostics).
    steps: u64,
    /// Were we in a library at the previous step?
    in_lib: bool,
    modules: [Module; MAX_MODULES],
    nmod: usize,
    /// Ring buffer of recent in-image RIPs (for an instruction backtrace when
    /// a trigger fires).
    recent: [u64; RECENT],
    recent_head: usize,
    recent_count: usize,
    /// Dump the backtrace once when a `call` with this rdx value occurs
    /// (0 = disabled). Used to catch the moment a binary enters error
    /// reporting, e.g. rdx = the "ERROR:" string id.
    trigger_rdx: u64,
    triggered: bool,
    /// Return address of the in-progress library call (armed in `DR0` as a
    /// hardware execution breakpoint). While non-zero we are *not*
    /// single-stepping: the library runs at full native speed with `TF` clear,
    /// and `#DB` next fires when control returns to this image address. 0 =
    /// single-stepping the image with `TF`. This is the "trace while executing"
    /// optimization: one trap per API call/return instead of per instruction.
    lib_ret: u64,
}

const RECENT: usize = 4096;

static DBG: SpinLock<DebugState> = SpinLock::new(DebugState {
    armed: false,
    tracing: false,
    budget: 0,
    steps: 0,
    in_lib: false,
    modules: [Module { name: "", base: 0, size: 0, is_lib: false }; MAX_MODULES],
    nmod: 0,
    recent: [0; RECENT],
    recent_head: 0,
    recent_count: 0,
    trigger_rdx: 0,
    triggered: false,
    lib_ret: 0,
});

/// Write `DR0` (a hardware breakpoint linear address).
#[inline]
unsafe fn write_dr0(v: u64) {
    core::arch::asm!("mov dr0, {}", in(reg) v, options(nostack, preserves_flags));
}

/// Write `DR7` (breakpoint control). `0x1` = enable DR0 (L0), execute-on-fetch
/// (R/W0=00), 1-byte length (LEN0=00); `0` disables all breakpoints.
#[inline]
unsafe fn write_dr7(v: u64) {
    core::arch::asm!("mov dr7, {}", in(reg) v, options(nostack, preserves_flags));
}

/// Read `DR6` (debug status). Bit 0 (`B0`) set means the `DR0` breakpoint hit.
#[inline]
unsafe fn read_dr6() -> u64 {
    let v: u64;
    core::arch::asm!("mov {}, dr6", out(reg) v, options(nostack, preserves_flags));
    v
}

/// Clear `DR6` (the status bits are sticky and must be cleared after handling).
#[inline]
unsafe fn write_dr6(v: u64) {
    core::arch::asm!("mov dr6, {}", in(reg) v, options(nostack, preserves_flags));
}

/// Read a `u64` from a user-mode address (the return address on the user
/// stack). Brackets the access for SMAP. The caller must have validated the
/// address is mapped (we probe first).
unsafe fn read_user_u64(addr: u64) -> u64 {
    crate::mm::virt::user_access_begin();
    let v = core::ptr::read_unaligned(addr as *const u64);
    crate::mm::virt::user_access_end();
    v
}

/// Register an address range so the tracer can label RIPs. `is_lib` marks a
/// shim library (vs the program image).
pub fn add_module(name: &'static str, base: u64, size: u64, is_lib: bool) {
    let mut d = DBG.lock();
    if d.nmod < MAX_MODULES {
        let n = d.nmod;
        d.modules[n] = Module { name, base, size, is_lib };
        d.nmod += 1;
    }
}

/// Clear the module table (between debug sessions).
pub fn clear_modules() {
    let mut d = DBG.lock();
    d.nmod = 0;
}

/// Arm tracing: the next thread entering ring 3 starts single-stepping, for up
/// to `budget` instructions. `trigger_rdx` (if non-zero) dumps an instruction
/// backtrace the first time a library call is made with that value in RDX —
/// e.g. the string id of an "ERROR:" resource, to catch entry into error
/// reporting.
pub fn arm(budget: u64) {
    arm_with_trigger(budget, 0);
}

pub fn arm_with_trigger(budget: u64, trigger_rdx: u64) {
    let mut d = DBG.lock();
    d.armed = true;
    d.tracing = false;
    d.budget = budget;
    d.steps = 0;
    d.in_lib = false;
    d.recent_head = 0;
    d.recent_count = 0;
    d.trigger_rdx = trigger_rdx;
    d.triggered = false;
    d.lib_ret = 0;
    // Make sure no stale hardware breakpoint is armed from a prior session.
    unsafe {
        write_dr7(0);
        write_dr6(0);
    }
}

/// Consume the armed flag (called by ring-3 entry): returns whether TF should
/// be set, and flips into the tracing state.
pub fn take_armed() -> bool {
    let mut d = DBG.lock();
    if d.armed {
        d.armed = false;
        d.tracing = true;
        true
    } else {
        false
    }
}

/// Resolve a RIP to `(module index+1, offset)`, or `(0, rip)` if unknown.
fn resolve(d: &DebugState, rip: u64) -> (usize, u64) {
    for i in 0..d.nmod {
        let m = d.modules[i];
        if rip >= m.base && rip < m.base + m.size {
            return (i + 1, rip - m.base);
        }
    }
    (0, rip)
}

/// `#DB` handler. Two sources: a `TF` single-step (while in the image) or the
/// `DR0` return-address breakpoint (a library call finished). Logs API
/// call/return transitions and manages the step budget. The library body runs
/// at full native speed between a call and its return — only the boundaries
/// trap — so the trace costs ~one trap per API call, not per instruction.
pub fn on_single_step(frame: &mut KtrapFrame) {
    let mut d = DBG.lock();
    if !d.tracing {
        // A stray #DB while not tracing: disarm everything and move on.
        frame.rflags &= !RFLAGS_TF;
        unsafe {
            write_dr7(0);
            write_dr6(0);
        }
        return;
    }

    // Case 1: the DR0 return-address breakpoint fired — the library call we let
    // run free has returned to the image. Log the return value and resume
    // single-stepping the image.
    if d.lib_ret != 0 && (unsafe { read_dr6() } & 1) != 0 {
        unsafe {
            write_dr7(0);
            write_dr0(0);
            write_dr6(0);
        }
        d.lib_ret = 0;
        d.in_lib = false;
        kd_println!("TRACE   ret -> rax={:#x}", frame.rax);
        if d.budget == 0 {
            d.tracing = false;
            frame.rflags &= !RFLAGS_TF;
            kd_println!("TRACE ended after {} steps", d.steps);
        } else {
            frame.rflags |= RFLAGS_TF; // RIP is at the return addr; step it next.
        }
        return;
    }

    // Case 2: a TF single-step in the image.
    d.steps += 1;
    let (idx, off) = resolve(&d, frame.rip);
    let cur_in_lib = idx != 0 && d.modules[idx - 1].is_lib;
    let name = if idx != 0 { d.modules[idx - 1].name } else { "?" };

    // Record in-image RIPs for the instruction backtrace.
    if idx != 0 && !d.modules[idx - 1].is_lib {
        let h = d.recent_head;
        d.recent[h] = frame.rip;
        d.recent_head = (h + 1) % RECENT;
        if d.recent_count < RECENT {
            d.recent_count += 1;
        }
    }

    if cur_in_lib && !d.in_lib {
        // Image -> library: an API call. Log the target and arg registers
        // (Microsoft x64: rcx, rdx, r8, r9).
        kd_println!(
            "TRACE call {}+{:#x}  rcx={:#x} rdx={:#x} r8={:#x} r9={:#x}",
            name,
            off,
            frame.rcx,
            frame.rdx,
            frame.r8,
            frame.r9
        );
        // Trigger: dump the instruction backtrace the first time the target
        // value appears in RDX (e.g. the "ERROR:" string id).
        if d.trigger_rdx != 0 && frame.rdx == d.trigger_rdx && !d.triggered {
            d.triggered = true;
            dump_backtrace(&d);
        }
        d.in_lib = true;

        // Fast path: read the return address (top of the user stack, just
        // pushed by the CALL) and, if it lands back in the image, arm it in DR0
        // and clear TF so the library runs at native speed until it returns.
        let rsp = frame.rsp;
        if crate::mm::virt::probe_for_read(rsp, 8, 8).is_ok() {
            let ret = unsafe { read_user_u64(rsp) };
            let (ri, _) = resolve(&d, ret);
            if ri != 0 && !d.modules[ri - 1].is_lib {
                d.lib_ret = ret;
                unsafe {
                    write_dr6(0);
                    write_dr0(ret);
                    write_dr7(0x1); // enable DR0, execute-on-fetch, 1 byte
                }
                frame.rflags &= !RFLAGS_TF;
                return;
            }
        }
        // Fallback (couldn't resolve a return address into the image): keep
        // single-stepping through the library.
    } else if !cur_in_lib && d.in_lib {
        // Library -> image via the single-step fallback. Log the return value.
        kd_println!("TRACE   ret -> rax={:#x}", frame.rax);
        d.in_lib = false;
    }

    if d.budget == 0 {
        // Budget exhausted: stop single-stepping, let the thread run free.
        d.tracing = false;
        frame.rflags &= !RFLAGS_TF;
        unsafe {
            write_dr7(0);
        }
        kd_println!("TRACE ended after {} steps", d.steps);
    } else {
        d.budget -= 1;
        // Keep TF set (it already is in the saved frame) to step again.
        frame.rflags |= RFLAGS_TF;
    }
}

/// Offsets (into the program image) of `mov ecx, 5001` error sites in
/// where.exe — places that decide "report error string 5001". When one of
/// these appears in the recorded RIP stream, it is the decision that led to the
/// error, so the backtrace flags it explicitly.
const ERROR_SITES: [u64; 6] = [0x2f01, 0x2f9c, 0x3112, 0x31a4, 0x3295, 0x5c3e];

/// Print the instruction backtrace — the recent in-image RIPs leading up to a
/// trigger. Dumping every step would flood the serial line, so this scans the
/// ring for known decision sites and otherwise prints only a short tail.
fn dump_backtrace(d: &DebugState) {
    let n = d.recent_count;
    kd_println!("TRACE === backtrace: scanned {} in-image RIPs before trigger ===", n);
    let start = (d.recent_head + RECENT - n) % RECENT;

    // Flag any recorded RIP that lands within an instruction or two of a known
    // error-decision site — that is the branch that chose the error path.
    for i in 0..n {
        let rip = d.recent[(start + i) % RECENT];
        let (idx, off) = resolve(d, rip);
        if idx != 0 && !d.modules[idx - 1].is_lib {
            for &site in ERROR_SITES.iter() {
                if off >= site && off < site + 8 {
                    kd_println!("TRACE   *** error-decision at {}+{:#x} (step -{})",
                        d.modules[idx - 1].name, off, n - 1 - i);
                    // Print the ~16 in-image RIPs leading into this decision so
                    // we can see which branch/path chose the error.
                    let lo = if i >= 120 { i - 120 } else { 0 };
                    for j in lo..=i {
                        let r = d.recent[(start + j) % RECENT];
                        let (ix, of) = resolve(d, r);
                        let nm = if ix != 0 { d.modules[ix - 1].name } else { "?" };
                        kd_println!("TRACE       in {}+{:#x}", nm, of);
                    }
                }
            }
        }
    }

    // Tail: the last few in-image RIPs immediately before the trigger.
    let tail = if n > 24 { 24 } else { n };
    kd_println!("TRACE   --- last {} in-image RIPs ---", tail);
    for i in (n - tail)..n {
        let rip = d.recent[(start + i) % RECENT];
        let (idx, off) = resolve(d, rip);
        let name = if idx != 0 { d.modules[idx - 1].name } else { "?" };
        kd_println!("TRACE   bt {}+{:#x}", name, off);
    }
}

/// `#BP` (int3) handler — report a breakpoint hit with the faulting RIP.
pub fn on_breakpoint(frame: &KtrapFrame) {
    let d = DBG.lock();
    let (idx, off) = resolve(&d, frame.rip.wrapping_sub(1)); // int3 RIP points past
    let name = if idx != 0 { d.modules[idx - 1].name } else { "?" };
    kd_println!("TRACE breakpoint at {}+{:#x} (rip={:#x})", name, off, frame.rip);
}
