//! Legacy 8259A PIC handling: remap, then mask.
//!
//! The two cascaded 8259As power on mapped over CPU exception vectors
//! (IRQ0 = vector 8 — colliding with #DF!). Even though this kernel uses
//! the local APIC exclusively, the PICs must still be (a) remapped away
//! from the exception range so any spurious interrupt that slips through
//! is identifiable, and (b) fully masked. This mirrors what the HAL does
//! when it switches the platform into APIC mode.

use crate::hal::port::{io_wait, outb};

const PIC1_CMD: u16 = 0x20;
const PIC1_DATA: u16 = 0x21;
const PIC2_CMD: u16 = 0xA0;
const PIC2_DATA: u16 = 0xA1;

/// Remap the PICs to vectors 0x20–0x2F, then mask every IRQ line.
pub fn init_and_mask() {
    unsafe {
        // ICW1: start initialization, expect ICW4.
        outb(PIC1_CMD, 0x11);
        io_wait();
        outb(PIC2_CMD, 0x11);
        io_wait();
        // ICW2: vector offsets (master 0x20, slave 0x28).
        outb(PIC1_DATA, 0x20);
        io_wait();
        outb(PIC2_DATA, 0x28);
        io_wait();
        // ICW3: master has the slave on IRQ2; slave its cascade identity.
        outb(PIC1_DATA, 1 << 2);
        io_wait();
        outb(PIC2_DATA, 2);
        io_wait();
        // ICW4: 8086 mode.
        outb(PIC1_DATA, 0x01);
        io_wait();
        outb(PIC2_DATA, 0x01);
        io_wait();
        // OCW1: mask all eight lines on both PICs — the APIC owns
        // interrupt delivery from here on.
        outb(PIC1_DATA, 0xFF);
        outb(PIC2_DATA, 0xFF);
    }
}
