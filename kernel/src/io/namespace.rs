//! A minimal object namespace for devices and symbolic links.
//!
//! NT keeps a hierarchical object namespace (`\Device`, `\DosDevices`, …)
//! managed by the object manager. We implement just enough for drivers to
//! be reachable by name: a flat table of named devices and a table of
//! symbolic-link aliases (`\DosDevices\Foo` → `\Device\Bar`), with
//! case-insensitive lookup. `IoGetDeviceObjectPointer` resolves a name —
//! following one symbolic-link hop — to its `DEVICE_OBJECT`.
//!
//! This is the piece that, once user mode and handles exist, lets
//! `CreateFile("\\\\.\\Foo")` find a driver's device. Today it backs the
//! kernel-side `IoGetDeviceObjectPointer` export and the self tests.

use crate::ke::spinlock::SpinLock;
use crate::rtl::NtStatus;
use ntabi::{DeviceObject, UnicodeString};

/// A registered name → target. Names are owned copies (small, pool-backed)
/// so the registry doesn't depend on caller string lifetimes.
struct Entry {
    /// Owning copy of the UTF-16 name.
    name: alloc::vec::Vec<u16>,
    /// Device this name refers to (for device entries), or null for a pure
    /// symlink whose target is `link_target`.
    device: *mut DeviceObject,
    /// For symbolic links: the target name this alias resolves to.
    link_target: Option<alloc::vec::Vec<u16>>,
}

// SAFETY: the table is only touched under NAMESPACE's spinlock.
unsafe impl Send for Entry {}

struct Namespace {
    entries: alloc::vec::Vec<Entry>,
}

static NAMESPACE: SpinLock<Namespace> = SpinLock::new(Namespace {
    entries: alloc::vec::Vec::new(),
});

/// Read a `UNICODE_STRING` into an owned UTF-16 vector.
fn to_vec(s: &UnicodeString) -> alloc::vec::Vec<u16> {
    if s.buffer.is_null() || s.length == 0 {
        return alloc::vec::Vec::new();
    }
    // SAFETY: a valid UNICODE_STRING covers length/2 code units.
    let units = unsafe { core::slice::from_raw_parts(s.buffer, (s.length / 2) as usize) };
    units.to_vec()
}

/// ASCII-case-insensitive UTF-16 comparison (object names are ASCII here).
fn eq_ci(a: &[u16], b: &[u16]) -> bool {
    a.len() == b.len()
        && a.iter().zip(b).all(|(&x, &y)| {
            let lx = if (b'A' as u16..=b'Z' as u16).contains(&x) { x + 32 } else { x };
            let ly = if (b'A' as u16..=b'Z' as u16).contains(&y) { y + 32 } else { y };
            lx == ly
        })
}

/// Register a named device (called by the full `IoCreateDevice`). A device
/// with an empty name is anonymous and not registered.
pub fn register_device(name: &UnicodeString, device: *mut DeviceObject) {
    let n = to_vec(name);
    if n.is_empty() {
        return;
    }
    let mut ns = NAMESPACE.lock();
    ns.entries.push(Entry {
        name: n,
        device,
        link_target: None,
    });
}

/// `IoCreateSymbolicLink(LinkName, DeviceName)` — alias `link` to `target`.
pub fn create_symbolic_link(link: &UnicodeString, target: &UnicodeString) -> NtStatus {
    let l = to_vec(link);
    let t = to_vec(target);
    if l.is_empty() {
        return NtStatus::INVALID_PARAMETER;
    }
    let mut ns = NAMESPACE.lock();
    ns.entries.push(Entry {
        name: l,
        device: core::ptr::null_mut(),
        link_target: Some(t),
    });
    NtStatus::SUCCESS
}

/// `IoDeleteSymbolicLink(LinkName)`.
pub fn delete_symbolic_link(link: &UnicodeString) -> NtStatus {
    let l = to_vec(link);
    let mut ns = NAMESPACE.lock();
    let before = ns.entries.len();
    ns.entries
        .retain(|e| !(e.link_target.is_some() && eq_ci(&e.name, &l)));
    if ns.entries.len() < before {
        NtStatus::SUCCESS
    } else {
        NtStatus::OBJECT_NAME_NOT_FOUND
    }
}

/// `IoGetDeviceObjectPointer(Name, ...)` — resolve a device or symbolic-link
/// name to its `DEVICE_OBJECT`, following at most one link hop.
pub fn lookup_device(name: &UnicodeString) -> Result<*mut DeviceObject, NtStatus> {
    let mut target = to_vec(name);
    let ns = NAMESPACE.lock();
    for _hop in 0..2 {
        // Device match?
        if let Some(e) = ns
            .entries
            .iter()
            .find(|e| e.link_target.is_none() && eq_ci(&e.name, &target))
        {
            return Ok(e.device);
        }
        // Symbolic-link match? follow it once.
        if let Some(e) = ns
            .entries
            .iter()
            .find(|e| e.link_target.is_some() && eq_ci(&e.name, &target))
        {
            target = e.link_target.clone().unwrap();
            continue;
        }
        break;
    }
    Err(NtStatus::OBJECT_NAME_NOT_FOUND)
}
