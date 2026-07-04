//! IDT — building and loading the Interrupt Descriptor Table.
//!
//! 256 16-byte gate descriptors mapping vectors to the entry stubs emitted
//! in [`super::traps`]. All gates are *interrupt gates* (the CPU clears IF
//! on entry), DPL 0, kernel code selector. Three exceptions are routed to
//! IST emergency stacks (see [`super::gdt`]):
//!
//! * vector 2 (NMI) — can fire mid-stack-switch,
//! * vector 8 (double fault) — current stack may be the failure,
//! * vector 18 (machine check) — hardware state is suspect.

use super::gdt::{IST_DOUBLE_FAULT, IST_MCE, IST_NMI, KGDT64_R0_CODE};
use super::traps;
use core::arch::asm;
use core::mem::size_of;

/// One 64-bit IDT gate (Intel SDM Vol. 3, Figure 6-8).
#[repr(C)]
#[derive(Clone, Copy)]
struct IdtGate {
    offset_low: u16,
    selector: u16,
    /// bits 0..2: IST index (0 = use current stack)
    ist: u8,
    /// P | DPL | 0 | type (0xE = 64-bit interrupt gate)
    type_attr: u8,
    offset_mid: u16,
    offset_high: u32,
    _reserved: u32,
}

impl IdtGate {
    const fn empty() -> Self {
        IdtGate {
            offset_low: 0,
            selector: 0,
            ist: 0,
            type_attr: 0,
            offset_mid: 0,
            offset_high: 0,
            _reserved: 0,
        }
    }

    /// Point this gate at `handler` as a present, DPL-0 interrupt gate.
    fn set(&mut self, handler: u64, ist: u8) {
        self.offset_low = handler as u16;
        self.selector = KGDT64_R0_CODE;
        self.ist = ist & 0b111;
        self.type_attr = 0x8E; // present | DPL0 | interrupt gate
        self.offset_mid = (handler >> 16) as u16;
        self.offset_high = (handler >> 32) as u32;
    }
}

/// The boot processor's IDT. Written once during phase 0 (single-threaded,
/// interrupts disabled), read by hardware afterwards.
static mut BOOT_IDT: [IdtGate; 256] = [IdtGate::empty(); 256];

/// Base and limit of the loaded IDT — what `sidt` would return. Recorded in the
/// crash dump's `KPROCESSOR_STATE.SpecialRegisters.Idtr` (nanox does not
/// implement `sidt`, so we report the table we handed to `lidt` directly).
pub fn idtr() -> (u64, u16) {
    (&raw const BOOT_IDT as u64, (size_of::<[IdtGate; 256]>() - 1) as u16)
}

#[repr(C, packed)]
struct Idtr {
    limit: u16,
    base: u64,
}

/// Populate all 256 gates from the fixed-stride stub array and load IDTR.
pub fn init() {
    // The stubs are emitted contiguously at 16-byte stride; see traps.rs.
    let stub_base = traps::ki_vector_stubs as *const () as u64;

    unsafe {
        // SAFETY: phase-0 single-threaded init; nothing else touches the IDT.
        let idt = &raw mut BOOT_IDT;
        for vector in 0..256usize {
            let ist = match vector as u8 {
                traps::VECTOR_NMI => IST_NMI,
                traps::VECTOR_DOUBLE_FAULT => IST_DOUBLE_FAULT,
                traps::VECTOR_MCE => IST_MCE,
                _ => 0,
            };
            (*idt)[vector].set(stub_base + (vector as u64) * 16, ist);
        }

        let idtr = Idtr {
            limit: (size_of::<[IdtGate; 256]>() - 1) as u16,
            base: idt as u64,
        };
        asm!("lidt [{}]", in(reg) &idtr, options(nostack));
    }
}
