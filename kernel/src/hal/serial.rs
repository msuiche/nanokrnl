//! COM1 16550 UART driver — the `KdPrint` debug transport.
//!
//! NT routes `DbgPrint`/`KdPrint` through the kernel debugger transport
//! (historically a serial port). We do the same: COM1 at 115200 8N1, wired
//! by the boot crate to QEMU's stdio, so kernel debug output lands in the
//! terminal that launched the VM.
//!
//! The writer is protected by a raw spinlock acquired at HIGH_LEVEL-like
//! semantics (interrupts off) so that `kd_print!` is usable from any
//! context — including exception handlers and the bugcheck path, where
//! taking a blocking lock would be fatal. On the bugcheck path the lock is
//! deliberately *bypassed* (`force_unlock`) because the owner may be the
//! very CPU that crashed.

use crate::hal::port::{inb, outb};
use core::fmt::{self, Write};
use core::sync::atomic::{AtomicBool, Ordering};

/// Standard ISA I/O base for COM1.
const COM1: u16 = 0x3F8;

// 16550 register offsets from the base port.
const REG_DATA: u16 = 0; // THR (write) / RBR (read)
const REG_IER: u16 = 1; // interrupt enable
const REG_FCR: u16 = 2; // FIFO control (write)
const REG_LCR: u16 = 3; // line control
const REG_MCR: u16 = 4; // modem control
const REG_LSR: u16 = 5; // line status

/// LSR bit 5: transmit holding register empty (safe to write a byte).
const LSR_THRE: u8 = 1 << 5;
/// LSR bit 0: received data available (safe to read a byte).
const LSR_DATA_READY: u8 = 1 << 0;

/// Non-blocking receive: return one byte if the UART has one, else `None`.
/// The console-input path polls this to drain typed characters.
pub fn try_read_byte() -> Option<u8> {
    if !INITIALIZED.load(Ordering::Acquire) {
        return None;
    }
    // SAFETY: reading LSR/RBR is side-effect-free apart from consuming a byte.
    unsafe {
        if inb(COM1 + REG_LSR) & LSR_DATA_READY != 0 {
            Some(inb(COM1 + REG_DATA))
        } else {
            None
        }
    }
}

/// One-time init guard + writer lock. A full `KSPIN_LOCK` isn't available
/// this early in boot (ke isn't initialized), so the serial driver keeps a
/// private flag-based lock; see module docs for the bugcheck escape hatch.
static INITIALIZED: AtomicBool = AtomicBool::new(false);
static LOCK: AtomicBool = AtomicBool::new(false);

/// Program the UART: 115200 baud, 8 data bits, no parity, 1 stop bit,
/// FIFOs on. Mirrors what the kd serial transport negotiates.
pub fn init() {
    unsafe {
        outb(COM1 + REG_IER, 0x00); // no UART interrupts; we poll LSR
        outb(COM1 + REG_LCR, 0x80); // DLAB=1: map divisor registers
        outb(COM1 + REG_DATA, 0x01); // divisor low: 115200 / 115200 = 1
        outb(COM1 + REG_IER, 0x00); // divisor high
        outb(COM1 + REG_LCR, 0x03); // DLAB=0, 8N1
        outb(COM1 + REG_FCR, 0xC7); // enable + clear FIFOs, 14-byte trigger
        outb(COM1 + REG_MCR, 0x0B); // DTR | RTS | OUT2
    }
    INITIALIZED.store(true, Ordering::Release);
}

/// Busy-wait until the transmitter can accept a byte, then send it.
/// LF is expanded to CRLF so output renders correctly in raw terminals.
fn put_byte(b: u8) {
    unsafe {
        if b == b'\n' {
            put_raw(b'\r');
        }
        put_raw(b);
    }

    unsafe fn put_raw(b: u8) {
        unsafe {
            while inb(COM1 + REG_LSR) & LSR_THRE == 0 {
                core::hint::spin_loop();
            }
            outb(COM1 + REG_DATA, b);
        }
    }
}

/// Zero-sized `fmt::Write` adapter over the COM1 transmitter.
struct SerialWriter;

impl Write for SerialWriter {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for b in s.bytes() {
            put_byte(b);
        }
        Ok(())
    }
}

/// Write formatted text to the debug port. Interrupts are disabled for the
/// duration so a clock tick cannot deadlock against our own lock; this is
/// the backing routine for [`kd_print!`](crate::kd_print).
pub fn write_fmt(args: fmt::Arguments<'_>) {
    if !INITIALIZED.load(Ordering::Acquire) {
        return; // pre-init kd_print is a no-op, never a fault
    }
    // Mask interrupts while holding the lock (KeAcquireSpinLock raises to
    // DISPATCH; we go further to HIGH since the printer is used from ISRs).
    let rflags = crate::ke::irql::disable_interrupts();
    while LOCK
        .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
        .is_err()
    {
        core::hint::spin_loop();
    }
    let _ = SerialWriter.write_fmt(args);
    LOCK.store(false, Ordering::Release);
    crate::ke::irql::restore_interrupts(rflags);
}

/// Bugcheck path: smash the lock and print unconditionally. Only callable
/// from `KeBugCheck`, which has already frozen the system.
pub unsafe fn write_fmt_forced(args: fmt::Arguments<'_>) {
    LOCK.store(false, Ordering::Release);
    if !INITIALIZED.load(Ordering::Acquire) {
        init();
    }
    let _ = SerialWriter.write_fmt(args);
}

/// `KdPrint`/`DbgPrint` — formatted output to the kernel debug port.
///
/// Usable from any IRQL including exception handlers. Compiles to nothing
/// useful on non-x86_64 (host test) builds.
#[macro_export]
macro_rules! kd_print {
    ($($arg:tt)*) => {{
        #[cfg(target_arch = "x86_64")]
        $crate::hal::serial::write_fmt(format_args!($($arg)*));
        #[cfg(not(target_arch = "x86_64"))]
        { let _ = format_args!($($arg)*); }
    }};
}

/// `KdPrint` with a trailing newline, analogous to how virtually every
/// DbgPrint call site ends its format string with `\n`.
#[macro_export]
macro_rules! kd_println {
    () => { $crate::kd_print!("\n") };
    ($($arg:tt)*) => {{
        $crate::kd_print!($($arg)*);
        $crate::kd_print!("\n");
    }};
}
