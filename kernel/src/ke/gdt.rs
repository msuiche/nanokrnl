//! GDT/TSS setup with the **NT x64 selector layout**.
//!
//! Segmentation is vestigial in long mode, but the *selector values* are
//! ABI: NT's syscall/sysret machinery, trap frames, and debugger all assume
//! this exact GDT arrangement (from ke/amd64/ntosdef — the `KGDT64_*`
//! constants):
//!
//! ```text
//! 0x00 KGDT64_NULL       null descriptor
//! 0x10 KGDT64_R0_CODE    kernel mode 64-bit code   (CS in kernel)
//! 0x18 KGDT64_R0_DATA    kernel mode data          (SS in kernel)
//! 0x20 KGDT64_R3_CMCODE  user mode 32-bit code     (WoW64 compatibility)
//! 0x28 KGDT64_R3_DATA    user mode data            (user SS, RPL 3)
//! 0x30 KGDT64_R3_CODE    user mode 64-bit code     (user CS, RPL 3)
//! 0x40 KGDT64_SYS_TSS    the TSS (16-byte system descriptor)
//! ```
//!
//! The ordering of 0x20/0x28/0x30 is not arbitrary: x86 `syscall`/`sysret`
//! require user32-code, user-data, user64-code to be consecutive selectors
//! starting at `IA32_STAR[63:48]`, and kernel-code/kernel-data consecutive
//! at `IA32_STAR[47:32]`. NT's layout is *designed* around that; by
//! adopting it we get syscall support for free later.
//!
//! The TSS in long mode no longer does hardware task switching; it holds
//! the stack pointers the CPU loads on privilege transitions (RSP0) and
//! the Interrupt Stack Table — known-good emergency stacks for NMI,
//! double-fault and machine-check, so even a kernel-stack overflow can be
//! diagnosed instead of triple-faulting.

use core::arch::asm;
use core::mem::size_of;

// Selector constants are defined once in `ke::selectors` (un-gated so the
// conformance tests can assert them on the host) and re-exported here.
pub use super::selectors::{
    KGDT64_NULL, KGDT64_R0_CODE, KGDT64_R0_DATA, KGDT64_R3_CMCODE, KGDT64_R3_CODE, KGDT64_R3_DATA,
    KGDT64_SYS_TSS, RPL_USER,
};

// --- IST slot assignments (1-based indices into Tss64::ist) -------------
/// Double fault: must run on a known-good stack — the faulting stack may
/// be the problem (kernel stack overflow).
pub const IST_DOUBLE_FAULT: u8 = 1;
/// NMI: can arrive between *any* two instructions, including mid-stack-switch.
pub const IST_NMI: u8 = 2;
/// Machine check: hardware says memory/CPU state is suspect.
pub const IST_MCE: u8 = 3;

const IST_STACK_SIZE: usize = 16 * 1024;

/// 64-bit TSS layout (Intel SDM Vol. 3, Figure 8-11).
#[repr(C, packed(4))]
pub struct Tss64 {
    _reserved0: u32,
    /// RSP loaded on a ring-3 -> ring-0 transition (interrupt from user mode).
    pub rsp0: u64,
    pub rsp1: u64,
    pub rsp2: u64,
    _reserved1: u64,
    /// Interrupt Stack Table: ist[0] is IST1 in descriptor terms.
    pub ist: [u64; 7],
    _reserved2: u64,
    _reserved3: u16,
    /// Offset to the I/O permission bitmap; sized past the limit == none.
    pub iomap_base: u16,
}

/// The boot processor's TSS. `static mut` is acceptable here under the
/// same reasoning NT applies to the boot-CPU KTSS: written exactly once
/// during single-threaded phase-0 init, then only read by hardware.
static mut BOOT_TSS: Tss64 = Tss64 {
    _reserved0: 0,
    rsp0: 0,
    rsp1: 0,
    rsp2: 0,
    _reserved1: 0,
    ist: [0; 7],
    _reserved2: 0,
    _reserved3: 0,
    iomap_base: size_of::<Tss64>() as u16, // no I/O bitmap
};

/// Emergency stacks for the IST entries (+ alignment via repr).
#[repr(C, align(16))]
struct IstStack([u8; IST_STACK_SIZE]);
static mut IST1_STACK: IstStack = IstStack([0; IST_STACK_SIZE]);
static mut IST2_STACK: IstStack = IstStack([0; IST_STACK_SIZE]);
static mut IST3_STACK: IstStack = IstStack([0; IST_STACK_SIZE]);

/// The GDT itself: 10 quadwords — selectors through 0x40, with the TSS
/// descriptor occupying two slots (indices 8 and 9). Index = selector >> 3.
static mut BOOT_GDT: [u64; 10] = [0; 10];

/// Base and limit of the loaded GDT — what `sgdt` would return. The crash dump
/// records this in `KPROCESSOR_STATE.SpecialRegisters.Gdtr` so a debugger can
/// resolve the CS/SS/DS descriptors; nanox does not implement `sgdt`, so we
/// report the table we handed to `lgdt` directly.
pub fn gdtr() -> (u64, u16) {
    (&raw const BOOT_GDT as u64, (size_of::<[u64; 10]>() - 1) as u16)
}

/// Pseudo-descriptor operand for `lgdt`.
#[repr(C, packed)]
struct DescriptorTablePointer {
    limit: u16,
    base: u64,
}

/// Build a normal (code/data) descriptor quadword.
///
/// In long mode the base/limit of code segments are ignored; the bits that
/// matter are: P (present), DPL, S=1 (non-system), type, and L (64-bit) /
/// D (default size) for code. Data segments only need P/S/W.
const fn segment_descriptor(typ: u8, dpl: u8, long: bool, default32: bool) -> u64 {
    let mut d: u64 = 0;
    d |= (typ as u64) << 40; // type + S bit live at bits 40..47
    d |= (dpl as u64) << 45;
    d |= 1 << 47; // present
    if long {
        d |= 1 << 53; // L: 64-bit code
    }
    if default32 {
        d |= 1 << 54; // D/B: 32-bit default (for the compat-mode segment)
    }
    // Limit/granularity for the 32-bit segments so compat code gets 4 GiB.
    d |= 0x000F_0000_0000_FFFF & if long { 0 } else { !0 };
    d |= (1 << 55) & if long { 0 } else { !0 }; // G: 4 KiB granularity
    d
}

/// Initialize and load the GDT + TSS on the boot processor, switching CS/SS
/// to the NT kernel selectors. Runs once, interrupts disabled, phase 0.
pub fn init(boot_kernel_rsp: u64) {
    unsafe {
        // SAFETY: phase-0, single-threaded, one-time initialization.
        let tss = &raw mut BOOT_TSS;

        // Wire up the emergency stacks (stacks grow down: store the top).
        (*tss).rsp0 = boot_kernel_rsp;
        (*tss).ist[(IST_DOUBLE_FAULT - 1) as usize] =
            (&raw mut IST1_STACK as u64) + IST_STACK_SIZE as u64;
        (*tss).ist[(IST_NMI - 1) as usize] = (&raw mut IST2_STACK as u64) + IST_STACK_SIZE as u64;
        (*tss).ist[(IST_MCE - 1) as usize] = (&raw mut IST3_STACK as u64) + IST_STACK_SIZE as u64;

        let gdt = &raw mut BOOT_GDT;
        // type 0b11010 = S|code|readable ; 0b10010 = S|data|writable
        (*gdt)[(KGDT64_R0_CODE >> 3) as usize] = segment_descriptor(0b1_1010, 0, true, false);
        (*gdt)[(KGDT64_R0_DATA >> 3) as usize] = segment_descriptor(0b1_0010, 0, false, false);
        (*gdt)[(KGDT64_R3_CMCODE >> 3) as usize] = segment_descriptor(0b1_1010, 3, false, true);
        (*gdt)[(KGDT64_R3_DATA >> 3) as usize] = segment_descriptor(0b1_0010, 3, false, false);
        (*gdt)[(KGDT64_R3_CODE >> 3) as usize] = segment_descriptor(0b1_1010, 3, true, false);

        // 16-byte TSS system descriptor (type 0b1001 = available 64-bit TSS).
        let base = tss as u64;
        let limit = (size_of::<Tss64>() - 1) as u64;
        let mut lo: u64 = limit & 0xFFFF;
        lo |= (base & 0xFF_FFFF) << 16;
        lo |= 0b1001 << 40; // type
        lo |= 1 << 47; // present
        lo |= ((limit >> 16) & 0xF) << 48;
        lo |= ((base >> 24) & 0xFF) << 56;
        (*gdt)[(KGDT64_SYS_TSS >> 3) as usize] = lo;
        (*gdt)[(KGDT64_SYS_TSS >> 3) as usize + 1] = base >> 32;

        let gdtr = DescriptorTablePointer {
            limit: (size_of::<[u64; 10]>() - 1) as u16,
            base: gdt as u64,
        };

        // Load GDTR, then reload CS with a far return (you cannot `mov cs`),
        // refresh the data selectors, and finally load the task register.
        asm!(
            "lgdt [{gdtr}]",
            // far-return trick: push new CS, push target RIP, retfq
            "lea {tmp}, [rip + 2f]",
            "push {cs}",
            "push {tmp}",
            "retfq",
            "2:",
            "mov ds, {ds:x}",
            "mov es, {ds:x}",
            "mov ss, {ds:x}",
            // fs/gs are zeroed; GS base is set via MSR by ke::pcr.
            "xor eax, eax",
            "mov fs, ax",
            "mov gs, ax",
            "ltr {tss_sel:x}",
            gdtr = in(reg) &gdtr,
            cs = in(reg) KGDT64_R0_CODE as u64,
            ds = in(reg) KGDT64_R0_DATA as u64,
            tss_sel = in(reg) KGDT64_SYS_TSS,
            tmp = lateout(reg) _,
            out("rax") _,
        );
    }
}

/// Update RSP0 in the TSS — called by the context switch path so that an
/// interrupt arriving in (future) user mode lands on the new thread's
/// kernel stack. Mirrors `KiSetTssRsp0`.
pub fn set_kernel_stack(rsp0: u64) {
    unsafe {
        // SAFETY: rsp0 sits at offset 4 of the packed TSS, so it is only
        // 4-byte aligned — an unaligned write is required. The CPU reads
        // it from memory only on ring transitions, which cannot race a
        // same-CPU store.
        (&raw mut BOOT_TSS.rsp0).write_unaligned(rsp0);
    }
}
