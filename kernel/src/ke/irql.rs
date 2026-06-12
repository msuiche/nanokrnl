//! IRQL — Interrupt Request Levels.
//!
//! IRQL is NT's central synchronization concept: a per-processor priority
//! that masks all interrupt sources of equal or lower priority. Code at
//! `DISPATCH_LEVEL` cannot be preempted by the scheduler; code at a device
//! IRQL cannot be interrupted by that device; `HIGH_LEVEL` masks
//! everything.
//!
//! ## Hardware mapping (identical to x64 Windows)
//!
//! On x86_64 the IRQL *is* the APIC Task Priority Register, conveniently
//! architecturally aliased as **CR8**: an interrupt vector `v` is delivered
//! only if `v >> 4 > CR8`. Hence the standard NT x64 table:
//!
//! ```text
//! IRQL  0 PASSIVE_LEVEL   — normal thread execution
//! IRQL  1 APC_LEVEL       — asynchronous procedure calls
//! IRQL  2 DISPATCH_LEVEL  — scheduler/DPCs (vectors 0x30..0x3F)
//! IRQL  3..12             — device interrupts (DIRQL)
//! IRQL 13 CLOCK_LEVEL     — clock tick      (vector 0xD1, like NT!)
//! IRQL 14 IPI_LEVEL       — inter-processor interrupts
//! IRQL 15 HIGH_LEVEL      — everything masked (bugcheck path)
//! ```
//!
//! Raising IRQL is therefore a single `mov cr8, x` — no LAPIC MMIO access —
//! which is why `KeRaiseIrql`/`KeLowerIrql` are cheap enough to wrap every
//! spinlock acquisition.
//!
//! On non-x86_64 (host unit tests) the IRQL is emulated with a per-process
//! atomic so the dispatcher logic can still be exercised.

/// The IRQL type — `KIRQL` is a `UCHAR` in NT.
pub type Kirql = u8;

pub const PASSIVE_LEVEL: Kirql = 0;
pub const APC_LEVEL: Kirql = 1;
pub const DISPATCH_LEVEL: Kirql = 2;
pub const CLOCK_LEVEL: Kirql = 13;
pub const IPI_LEVEL: Kirql = 14;
pub const HIGH_LEVEL: Kirql = 15;

// ---------------------------------------------------------------------------
// x86_64: IRQL == CR8 == APIC TPR
// ---------------------------------------------------------------------------

#[cfg(target_arch = "x86_64")]
mod arch {
    use super::Kirql;
    use core::arch::asm;

    /// `KeGetCurrentIrql` — read CR8.
    #[inline]
    pub fn current() -> Kirql {
        let cr8: u64;
        unsafe { asm!("mov {}, cr8", out(reg) cr8, options(nomem, nostack, preserves_flags)) };
        cr8 as Kirql
    }

    /// Set CR8 directly. Internal helper for raise/lower, which enforce the
    /// monotonicity rules.
    #[inline]
    pub fn set(irql: Kirql) {
        unsafe {
            asm!("mov cr8, {}", in(reg) irql as u64, options(nomem, nostack, preserves_flags))
        };
    }

    /// Disable maskable interrupts, returning the previous RFLAGS so they
    /// can be restored exactly (interrupts may already have been off).
    #[inline]
    pub fn disable_interrupts() -> u64 {
        let rflags: u64;
        unsafe {
            asm!("pushfq; pop {}; cli", out(reg) rflags, options(nomem, preserves_flags));
        }
        rflags
    }

    /// Re-enable interrupts iff they were enabled in `rflags` (IF bit 9).
    #[inline]
    pub fn restore_interrupts(rflags: u64) {
        if rflags & (1 << 9) != 0 {
            unsafe { asm!("sti", options(nomem, nostack)) };
        }
    }
}

// ---------------------------------------------------------------------------
// Host (test) emulation: one global IRQL, no real masking
// ---------------------------------------------------------------------------

#[cfg(not(target_arch = "x86_64"))]
mod arch {
    use super::Kirql;

    // IRQL is a *per-processor* notion; in host tests each OS thread plays
    // the role of a CPU, so the emulated IRQL must be thread-local or
    // concurrent tests would observe each other's levels.
    #[cfg(test)]
    std::thread_local! {
        static EMULATED_IRQL: core::cell::Cell<Kirql> = const { core::cell::Cell::new(0) };
    }

    #[cfg(test)]
    pub fn current() -> Kirql {
        EMULATED_IRQL.with(|i| i.get())
    }

    #[cfg(test)]
    pub fn set(irql: Kirql) {
        EMULATED_IRQL.with(|i| i.set(irql));
    }

    // Non-test host build (the stub binary's dependency build): nothing
    // executes, a single global suffices to satisfy the type checker.
    #[cfg(not(test))]
    static EMULATED_IRQL: core::sync::atomic::AtomicU8 = core::sync::atomic::AtomicU8::new(0);

    #[cfg(not(test))]
    pub fn current() -> Kirql {
        EMULATED_IRQL.load(core::sync::atomic::Ordering::Relaxed)
    }

    #[cfg(not(test))]
    pub fn set(irql: Kirql) {
        EMULATED_IRQL.store(irql, core::sync::atomic::Ordering::Relaxed);
    }

    pub fn disable_interrupts() -> u64 {
        0
    }

    pub fn restore_interrupts(_rflags: u64) {}
}

pub use arch::{disable_interrupts, restore_interrupts};

/// `KeGetCurrentIrql` — the IRQL of the current processor.
#[inline]
pub fn ke_get_current_irql() -> Kirql {
    arch::current()
}

/// `KeRaiseIrql` — raise to `new_irql`, returning the previous level for
/// the matching [`ke_lower_irql`]. Raising to a *lower* level is a fatal
/// caller bug on NT (`IRQL_NOT_GREATER_OR_EQUAL`); we assert the same.
#[inline]
pub fn ke_raise_irql(new_irql: Kirql) -> Kirql {
    let old = arch::current();
    debug_assert!(
        new_irql >= old,
        "IRQL_NOT_GREATER_OR_EQUAL: raise {} -> {}",
        old,
        new_irql
    );
    arch::set(new_irql);
    old
}

/// `KeLowerIrql` — return to a previously saved level. Lowering to a
/// *higher* level is likewise fatal (`IRQL_NOT_LESS_OR_EQUAL`).
#[inline]
pub fn ke_lower_irql(old_irql: Kirql) {
    debug_assert!(
        old_irql <= arch::current(),
        "IRQL_NOT_LESS_OR_EQUAL: lower {} -> {}",
        arch::current(),
        old_irql
    );
    arch::set(old_irql);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raise_lower_roundtrip() {
        let old = ke_raise_irql(DISPATCH_LEVEL);
        assert_eq!(ke_get_current_irql(), DISPATCH_LEVEL);
        let old2 = ke_raise_irql(HIGH_LEVEL);
        assert_eq!(old2, DISPATCH_LEVEL);
        ke_lower_irql(old2);
        ke_lower_irql(old);
        assert_eq!(ke_get_current_irql(), old);
    }
}
