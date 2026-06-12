//! `KSPIN_LOCK` — the kernel spinlock.
//!
//! NT spinlocks are inseparable from IRQL: `KeAcquireSpinLock` first raises
//! to `DISPATCH_LEVEL` (so the holder cannot be preempted by the scheduler
//! or interrupted by a DPC on the same CPU), *then* spins on the lock word.
//! Releasing restores the caller's previous IRQL. Skipping the raise is how
//! classic deadlocks happen in C drivers; the type system here makes it
//! impossible — the only way to reach the protected data is through the
//! guard, and the guard's existence implies the raised IRQL.
//!
//! Rust-flavored design: instead of NT's bare `KSPIN_LOCK` word next to the
//! data it guards by convention, [`SpinLock<T>`] *owns* its data, and
//! [`SpinLockGuard`] hands out `&mut T` for exactly the duration between
//! acquire and release (RAII drop = `KeReleaseSpinLock`). Same algorithm,
//! same IRQL discipline, compiler-checked usage.

use crate::ke::irql::{self, Kirql, DISPATCH_LEVEL};
use core::cell::UnsafeCell;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

/// A data-owning `KSPIN_LOCK`.
pub struct SpinLock<T> {
    /// The lock word: true == held. (NT uses the low bit of a pointer-sized
    /// word; a bool-backed atomic is the same thing with clearer intent.)
    locked: AtomicBool,
    data: UnsafeCell<T>,
}

// SAFETY: the lock provides the mutual exclusion that makes sharing sound.
unsafe impl<T: Send> Send for SpinLock<T> {}
unsafe impl<T: Send> Sync for SpinLock<T> {}

impl<T> SpinLock<T> {
    /// `KeInitializeSpinLock` (const, so locks can be `static`).
    pub const fn new(data: T) -> Self {
        SpinLock {
            locked: AtomicBool::new(false),
            data: UnsafeCell::new(data),
        }
    }

    /// `KeAcquireSpinLock` — raise to DISPATCH_LEVEL and spin until owned.
    ///
    /// Test-and-test-and-set with `spin_loop` hints: contended waiters spin
    /// on a cached read instead of hammering the bus with locked RMW ops.
    pub fn lock(&self) -> SpinLockGuard<'_, T> {
        let old_irql = irql::ke_raise_irql(DISPATCH_LEVEL);
        loop {
            if self
                .locked
                .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
                .is_ok()
            {
                break;
            }
            while self.locked.load(Ordering::Relaxed) {
                core::hint::spin_loop();
            }
        }
        SpinLockGuard { lock: self, old_irql }
    }

    /// `KeAcquireSpinLockAtDpcLevel` — acquire when the caller is *already*
    /// at DISPATCH_LEVEL or above (e.g. inside a DPC); skips the raise.
    pub fn lock_at_dpc_level(&self) -> SpinLockGuard<'_, T> {
        let cur = irql::ke_get_current_irql();
        debug_assert!(cur >= DISPATCH_LEVEL, "lock_at_dpc_level below DISPATCH");
        loop {
            if self
                .locked
                .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
                .is_ok()
            {
                break;
            }
            while self.locked.load(Ordering::Relaxed) {
                core::hint::spin_loop();
            }
        }
        SpinLockGuard { lock: self, old_irql: cur }
    }

    /// Mutable access without locking — sound because `&mut self` proves
    /// exclusive ownership (used during single-threaded phase-0 init).
    pub fn get_mut(&mut self) -> &mut T {
        self.data.get_mut()
    }

    /// Raw pointer to the protected data, *without* acquiring the lock.
    ///
    /// This is the right tool for taking a stable address of pinned data in
    /// a `static SpinLock<T>` (e.g. a dispatcher object that is actually
    /// synchronized by the dispatcher lock, not this spinlock). It does not
    /// raise IRQL or provide exclusion — the caller is responsible for safe
    /// access through whatever mechanism really guards the data.
    pub fn as_mut_ptr(&self) -> *mut T {
        self.data.get()
    }
}

/// Lock ownership token; `Drop` is `KeReleaseSpinLock` (release the word,
/// then lower IRQL — strictly in that order, mirroring NT, so a waiter on
/// another CPU can proceed while we downgrade).
pub struct SpinLockGuard<'a, T> {
    lock: &'a SpinLock<T>,
    old_irql: Kirql,
}

impl<T> Deref for SpinLockGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        // SAFETY: guard existence == exclusive ownership of the lock.
        unsafe { &*self.lock.data.get() }
    }
}

impl<T> DerefMut for SpinLockGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: as above.
        unsafe { &mut *self.lock.data.get() }
    }
}

impl<T> Drop for SpinLockGuard<'_, T> {
    fn drop(&mut self) {
        self.lock.locked.store(false, Ordering::Release);
        irql::ke_lower_irql(self.old_irql);
    }
}

// ---------------------------------------------------------------------------
// Bare KSPIN_LOCK (the NT export model: a lock word separate from its data)
// ---------------------------------------------------------------------------

/// `KeAcquireSpinLock` over a bare `KSPIN_LOCK` word — raise to
/// DISPATCH_LEVEL, spin until the word is claimed, return the previous IRQL.
///
/// This is the model the exported `Ke*SpinLock` APIs use: drivers keep a
/// `KSPIN_LOCK` (a `usize`) next to the data it guards, rather than the
/// data-owning [`SpinLock<T>`] used inside the kernel.
///
/// # Safety
/// `lock` must point to a valid, initialized (zeroed) lock word.
pub unsafe fn ke_acquire_spin_lock_raw(lock: *mut usize) -> Kirql {
    let old = irql::ke_raise_irql(DISPATCH_LEVEL);
    let atom = unsafe { AtomicUsize::from_ptr(lock) };
    while atom
        .compare_exchange_weak(0, 1, Ordering::Acquire, Ordering::Relaxed)
        .is_err()
    {
        while atom.load(Ordering::Relaxed) != 0 {
            core::hint::spin_loop();
        }
    }
    old
}

/// `KeReleaseSpinLock` — release the word and lower to `old_irql`.
///
/// # Safety
/// `lock` must be the word acquired by [`ke_acquire_spin_lock_raw`], held.
pub unsafe fn ke_release_spin_lock_raw(lock: *mut usize, old_irql: Kirql) {
    let atom = unsafe { AtomicUsize::from_ptr(lock) };
    atom.store(0, Ordering::Release);
    irql::ke_lower_irql(old_irql);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn mutual_exclusion_across_threads() {
        // 8 threads x 10k increments; any lost update means broken exclusion.
        let lock = Arc::new(SpinLock::new(0u64));
        let mut handles = Vec::new();
        for _ in 0..8 {
            let lock = Arc::clone(&lock);
            handles.push(std::thread::spawn(move || {
                for _ in 0..10_000 {
                    *lock.lock() += 1;
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(*lock.lock(), 80_000);
    }
}
