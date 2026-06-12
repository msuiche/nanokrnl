//! Port-mapped I/O intrinsics — `READ_PORT_*` / `WRITE_PORT_*`.
//!
//! x86 has a 16-bit I/O address space separate from memory, accessed with
//! the `in`/`out` instructions. Legacy devices (UARTs, PICs, the PIT, QEMU's
//! debug-exit device) live there.
//!
//! All of these are `unsafe`: an errant port write can reprogram hardware
//! out from under the kernel. Callers are the device drivers in `hal`,
//! which own their respective ports.

use core::arch::asm;

/// Write one byte to an I/O port (`WRITE_PORT_UCHAR`).
///
/// # Safety
/// The caller must own the device decoding `port`.
#[inline]
pub unsafe fn outb(port: u16, value: u8) {
    unsafe {
        asm!("out dx, al", in("dx") port, in("al") value, options(nomem, nostack, preserves_flags));
    }
}

/// Read one byte from an I/O port (`READ_PORT_UCHAR`).
///
/// # Safety
/// The caller must own the device decoding `port`.
#[inline]
pub unsafe fn inb(port: u16) -> u8 {
    let value: u8;
    unsafe {
        asm!("in al, dx", out("al") value, in("dx") port, options(nomem, nostack, preserves_flags));
    }
    value
}

/// Write a 32-bit doubleword to an I/O port (`WRITE_PORT_ULONG`).
///
/// # Safety
/// The caller must own the device decoding `port`.
#[inline]
pub unsafe fn outl(port: u16, value: u32) {
    unsafe {
        asm!("out dx, eax", in("dx") port, in("eax") value, options(nomem, nostack, preserves_flags));
    }
}

/// A short, deterministic I/O delay: one write to the POST diagnostic port
/// 0x80. Old hardware (the 8259A in particular) needs a moment between
/// configuration writes; this is the traditional way to grant it.
#[inline]
pub unsafe fn io_wait() {
    unsafe { outb(0x80, 0) }
}
