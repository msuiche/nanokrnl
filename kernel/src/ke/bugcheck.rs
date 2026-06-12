//! `KeBugCheckEx` — the controlled crash ("blue screen").
//!
//! When the kernel detects an inconsistency it cannot recover from, the
//! only safe action is to stop *everything* before corruption spreads to
//! disk. NT's sequence is: raise to HIGH_LEVEL, freeze other processors,
//! print/record the stop code and four parameters, halt. Ours is the same,
//! with the "screen" being the serial debug port.
//!
//! Rust panics funnel in here too (stop code [`RUST_PANIC`]): a panic in
//! kernel code is by definition a bug of bugcheck severity.

use crate::ke::irql::{self, HIGH_LEVEL};
use core::panic::PanicInfo;
use core::sync::atomic::{AtomicBool, Ordering};

/// Classic NT stop codes we raise, bit-exact with bugcodes.h.
pub const IRQL_NOT_LESS_OR_EQUAL: u32 = 0x0000000A;
pub const KMODE_EXCEPTION_NOT_HANDLED: u32 = 0x0000001E;
pub const PFN_LIST_CORRUPT: u32 = 0x0000004E;
pub const PAGE_FAULT_IN_NONPAGED_AREA: u32 = 0x00000050;
pub const MANUALLY_INITIATED_CRASH: u32 = 0x000000E2;
pub const CRITICAL_PROCESS_DIED: u32 = 0x000000EF;
/// Private stop code for Rust panics (customer bit set, see rtl::status).
pub const RUST_PANIC: u32 = 0xDEAD_0001;

/// Re-entrancy latch: a bugcheck *during* a bugcheck (e.g. the printer
/// faults) must not recurse forever; second entry goes straight to halt.
static IN_BUGCHECK: AtomicBool = AtomicBool::new(false);

/// `KeBugCheckEx` — fatal stop with four diagnostic parameters.
///
/// Never returns; the processor is left halted with interrupts off.
pub fn ke_bug_check_ex(code: u32, p1: u64, p2: u64, p3: u64, p4: u64) -> ! {
    // From here on nothing may preempt or interrupt us.
    irql::disable_interrupts();
    let _ = irql::ke_raise_irql(HIGH_LEVEL);

    if IN_BUGCHECK.swap(true, Ordering::SeqCst) {
        halt(); // recursive bugcheck: give up silently
    }

    #[cfg(target_arch = "x86_64")]
    unsafe {
        crate::hal::serial::write_fmt_forced(format_args!(
            "\n\
             *** STOP: {:#010X} ({:#018X},{:#018X},{:#018X},{:#018X})\n\
             *** {}\n\n",
            code,
            p1,
            p2,
            p3,
            p4,
            stop_code_name(code),
        ));
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        let _ = (p1, p2, p3, p4);
        panic!("KeBugCheckEx({code:#010X})");
    }

    #[allow(unreachable_code)]
    {
        exit_qemu_failure();
        halt()
    }
}

/// `KeBugCheck` — the no-parameters convenience form.
pub fn ke_bug_check(code: u32) -> ! {
    ke_bug_check_ex(code, 0, 0, 0, 0)
}

/// Panic handler back end: format the panic message + location into the
/// stop banner so a Rust `assert!` failure reads like a stop code.
pub fn ke_bug_check_panic(info: &PanicInfo) -> ! {
    irql::disable_interrupts();
    let _ = irql::ke_raise_irql(HIGH_LEVEL);

    if IN_BUGCHECK.swap(true, Ordering::SeqCst) {
        halt();
    }

    #[cfg(target_arch = "x86_64")]
    unsafe {
        crate::hal::serial::write_fmt_forced(format_args!(
            "\n*** STOP: {RUST_PANIC:#010X} (RUST_PANIC)\n*** {info}\n\n"
        ));
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        let _ = info;
    }

    exit_qemu_failure();
    halt()
}

/// Human-readable stop-code names for the banner, as the real blue screen
/// shows for well-known codes.
fn stop_code_name(code: u32) -> &'static str {
    match code {
        IRQL_NOT_LESS_OR_EQUAL => "IRQL_NOT_LESS_OR_EQUAL",
        KMODE_EXCEPTION_NOT_HANDLED => "KMODE_EXCEPTION_NOT_HANDLED",
        PFN_LIST_CORRUPT => "PFN_LIST_CORRUPT",
        PAGE_FAULT_IN_NONPAGED_AREA => "PAGE_FAULT_IN_NONPAGED_AREA",
        MANUALLY_INITIATED_CRASH => "MANUALLY_INITIATED_CRASH",
        CRITICAL_PROCESS_DIED => "CRITICAL_PROCESS_DIED",
        RUST_PANIC => "RUST_PANIC",
        _ => "UNKNOWN_STOP_CODE",
    }
}

/// Under QEMU, report failure through the isa-debug-exit device so the
/// boot runner's exit status reflects the crash; harmless on real hardware
/// (just a write to an undecoded port).
fn exit_qemu_failure() {
    #[cfg(target_arch = "x86_64")]
    unsafe {
        crate::hal::port::outl(0xF4, 0x01); // exit status (1<<1)|1 = 3
    }
}

/// Halt this processor forever (`cli; hlt` loop — NMIs can still wake the
/// hlt, hence the loop).
fn halt() -> ! {
    loop {
        #[cfg(target_arch = "x86_64")]
        unsafe {
            core::arch::asm!("cli; hlt", options(nomem, nostack));
        }
        #[cfg(not(target_arch = "x86_64"))]
        core::hint::spin_loop();
    }
}
