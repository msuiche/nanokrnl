//! Local APIC — interrupt delivery and the scheduler clock.
//!
//! The local APIC is the per-CPU interrupt controller: it prioritizes and
//! delivers external/IPI/timer interrupts, applying the priority rule that
//! makes the NT IRQL scheme work in hardware (`vector >> 4` must exceed
//! the TPR/CR8 for delivery — see `ke::irql`).
//!
//! We drive the classic xAPIC MMIO interface (4 KiB of registers at
//! `IA32_APIC_BASE`, physical 0xFEE00000), reached through the bootloader's
//! whole-physical-memory mapping. Used registers:
//!
//! ```text
//! +0x080 TPR     task priority (we leave it to CR8, its architectural alias)
//! +0x0B0 EOI     end-of-interrupt: write 0 after servicing
//! +0x0F0 SVR     spurious vector + APIC software enable
//! +0x320 LVT timer
//! +0x380 TICR    timer initial count
//! +0x3E0 TDCR    timer divide configuration
//! ```
//!
//! ## The clock tick
//!
//! The LVT timer is programmed periodic on **vector 0xD1** — the same
//! vector NT uses for its clock, putting the tick at `CLOCK_LEVEL`
//! (IRQL 13 = 0xD1 >> 4) so it preempts everything except IPIs/NMIs.
//! QEMU's APIC timer ticks at ~1 GHz / divider; with divide-by-16 and an
//! initial count of 62500 the tick lands near 1 kHz. We don't calibrate
//! against the PIT: the scheduler needs a steady beat, not wall-clock
//! precision (documented trade-off; calibration is a straightforward
//! follow-up).

use crate::ke::pcr::{rdmsr, wrmsr};
use crate::ke::traps::{VECTOR_CLOCK, VECTOR_DPC, VECTOR_SPURIOUS};

const IA32_APIC_BASE: u32 = 0x1B;
const APIC_BASE_ENABLE: u64 = 1 << 11;

// Register offsets (bytes) from the MMIO base.
const REG_EOI: usize = 0x0B0;
const REG_SVR: usize = 0x0F0;
const REG_ICR_LOW: usize = 0x300;
const REG_LVT_TIMER: usize = 0x320;
const REG_TICR: usize = 0x380;
const REG_TDCR: usize = 0x3E0;

const LVT_TIMER_PERIODIC: u32 = 1 << 17;
const SVR_APIC_ENABLE: u32 = 1 << 8;

/// Virtual address of the xAPIC register page, set during [`init`] from
/// the physical-memory window. Single write in phase 0, then read-only.
static mut APIC_MMIO_BASE: *mut u32 = core::ptr::null_mut();

#[inline]
unsafe fn reg(offset: usize) -> *mut u32 {
    // SAFETY: callers run after init; offset is a documented register.
    unsafe { APIC_MMIO_BASE.byte_add(offset) }
}

unsafe fn write(offset: usize, value: u32) {
    unsafe { reg(offset).write_volatile(value) }
}

unsafe fn read(offset: usize) -> u32 {
    unsafe { reg(offset).read_volatile() }
}

/// Enable the local APIC and start the periodic scheduler clock.
///
/// `phys_offset` is the virtual base where the bootloader mapped physical
/// memory (from `BootInfo::physical_memory_offset`).
pub fn init(phys_offset: u64) {
    unsafe {
        // Locate (and globally enable) the xAPIC. The base MSR holds the
        // physical address of the register page.
        let base = rdmsr(IA32_APIC_BASE);
        let phys = base & 0xF_FFFF_F000;
        wrmsr(IA32_APIC_BASE, base | APIC_BASE_ENABLE);
        APIC_MMIO_BASE = (phys_offset + phys) as *mut u32;

        // Software-enable via the spurious vector register.
        write(REG_SVR, SVR_APIC_ENABLE | VECTOR_SPURIOUS as u32);

        // Periodic timer on the NT clock vector. Divide-by-16 + 62500
        // initial count ≈ 1 kHz on QEMU's ~1 GHz APIC timer clock.
        write(REG_TDCR, 0b0011); // divide by 16
        write(REG_LVT_TIMER, LVT_TIMER_PERIODIC | VECTOR_CLOCK as u32);
        write(REG_TICR, 62_500);
    }
}

/// Signal end-of-interrupt for the in-service vector. Every serviced APIC
/// interrupt must EOI exactly once or delivery wedges at that priority.
pub fn eoi() {
    unsafe {
        if !APIC_MMIO_BASE.is_null() {
            write(REG_EOI, 0);
        }
    }
}

/// Request a DPC/dispatch interrupt on the current processor (self-IPI on
/// `VECTOR_DPC`). This is `HalRequestSoftwareInterrupt(DISPATCH_LEVEL)`:
/// the IPI stays pending until IRQL drops below DISPATCH_LEVEL, which is
/// exactly the semantics the DPC queue needs.
pub fn request_dispatch_interrupt() {
    unsafe {
        if !APIC_MMIO_BASE.is_null() {
            // Destination shorthand 01 (self), fixed delivery.
            write(REG_ICR_LOW, (0b01 << 18) | VECTOR_DPC as u32);
        }
    }
}
