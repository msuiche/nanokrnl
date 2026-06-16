//! `ke::spinlock` — the kernel's `SpinLock<T>` surface for the WASM build.
//!
//! The x86 SpinLock raises IRQL and spins on an atomic word so multiple CPUs
//! synchronize. WASM (today) is single-threaded with no IRQL, so the lock is a
//! pass-through that still *owns* its data and hands out `&mut T` through a
//! guard — the exact API `ob`'s handle table compiles against (`const new`,
//! `lock()`, `get_mut`, `as_mut_ptr`). Exclusion is automatic: nothing else runs
//! while a guard is held.
use core::cell::UnsafeCell;
use core::ops::{Deref, DerefMut};

pub struct SpinLock<T> {
    data: UnsafeCell<T>,
}

// Single-threaded host: sound to share. (Send/Sync mirror the x86 lock so the
// same `static SpinLock<...>` declarations compile.)
unsafe impl<T: Send> Send for SpinLock<T> {}
unsafe impl<T: Send> Sync for SpinLock<T> {}

impl<T> SpinLock<T> {
    pub const fn new(data: T) -> Self {
        SpinLock { data: UnsafeCell::new(data) }
    }

    pub fn lock(&self) -> SpinLockGuard<'_, T> {
        SpinLockGuard { lock: self }
    }

    // Part of the SpinLock surface (used by other kernel modules); kept so the
    // API matches the x86 lock even when `ob` alone doesn't call them.
    #[allow(dead_code)]
    pub fn get_mut(&mut self) -> &mut T {
        self.data.get_mut()
    }

    #[allow(dead_code)]
    pub fn as_mut_ptr(&self) -> *mut T {
        self.data.get()
    }
}

pub struct SpinLockGuard<'a, T> {
    lock: &'a SpinLock<T>,
}

impl<T> Deref for SpinLockGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        unsafe { &*self.lock.data.get() }
    }
}

impl<T> DerefMut for SpinLockGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        unsafe { &mut *self.lock.data.get() }
    }
}
