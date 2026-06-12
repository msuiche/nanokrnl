//! # Hal — Hardware Abstraction Layer
//!
//! On x64 Windows the HAL was folded into the kernel image, but the
//! conceptual boundary remains: everything that talks to platform hardware
//! (interrupt controllers, timers, the debug port) lives behind `Hal*`
//! routines so the rest of the executive stays platform-agnostic.
//!
//! Components:
//! * [`serial`] — 16550 UART on COM1; transport for `KdPrint` output
//!   (stands in for the kd debug transport).
//! * [`pic`] — legacy 8259A PICs, initialized only to *mask* them so they
//!   cannot deliver spurious interrupts once the APIC is in control.
//! * [`apic`] — local APIC: spurious vector, TPR, and the periodic timer
//!   that drives the scheduler clock tick.
//! * [`port`] — x86 port-mapped I/O intrinsics (`inb`/`outb` etc.), the
//!   moral equivalent of `READ_PORT_UCHAR`/`WRITE_PORT_UCHAR`.

#[cfg(target_arch = "x86_64")]
pub mod apic;
#[cfg(target_arch = "x86_64")]
pub mod pic;
#[cfg(target_arch = "x86_64")]
pub mod port;
#[cfg(target_arch = "x86_64")]
pub mod serial;
