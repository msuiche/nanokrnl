//! # Ldr — the kernel image loader
//!
//! Loads PE/COFF kernel-mode drivers, the way NT's `MiLoadSystemImage` +
//! `IopLoadDriver` do: map the image, fix it up, bind its imports to the
//! kernel's exports, then call its `DriverEntry`.
//!
//! * [`exports`] — the kernel export table (`ntoskrnl.exe`'s export
//!   directory): named, Microsoft-x64 routines the loader binds driver
//!   imports against.
//! * [`pe`] — the PE32+ mapper/relocator/import-resolver.
//!
//! [`ldr_load_driver`] is the public entry: bytes in, a live
//! `DRIVER_OBJECT` (with its `DriverEntry` already run) out.

pub mod exports;
pub mod loaded;
pub mod mui;
pub mod ntdll;
pub mod pe;

use crate::io;
use crate::rtl::NtStatus;
use ntabi::{DriverObject, UnicodeString};

/// A successfully loaded driver: its object plus the image it runs from, so
/// the image can be freed on unload.
pub struct LoadedDriver {
    pub driver: *mut DriverObject,
    pub image_base: *mut u8,
    pub image_size: usize,
}

/// Load a driver from an in-memory PE image and run its `DriverEntry`.
///
/// Mirrors the NT load sequence: [`pe::load`] maps + relocates + binds
/// imports, then we create the `DRIVER_OBJECT` and invoke the image's entry
/// point through it (`io_create_driver` calls the initializer for us, with
/// the Microsoft x64 ABI). On `DriverEntry` failure the driver object is
/// torn down and the error propagated.
///
/// `driver_name` is the `\Driver\...` object name recorded in the object.
/// The mapped image is freed only by [`ldr_unload_driver`].
pub fn ldr_load_driver(
    image: &[u8],
    driver_name: UnicodeString,
) -> Result<LoadedDriver, NtStatus> {
    let loaded = pe::load(image)?;
    crate::kd_println!(
        "LDR: mapped driver image @ {:p} ({} bytes), entry @ {:p}",
        loaded.base,
        loaded.size,
        loaded.entry as *const ()
    );
    // io_create_driver allocates the DRIVER_OBJECT and calls the entry point
    // (DriverEntry) through the win64 ABI, exactly as the built-in null
    // driver is initialized — the only difference is where the code came from.
    let driver = io::io_create_driver(driver_name, loaded.entry)?;
    Ok(LoadedDriver {
        driver,
        image_base: loaded.base,
        image_size: loaded.size,
    })
}

/// Unload a driver: call its `DriverUnload` routine (if it registered one),
/// release the driver object, and free the mapped image. Mirrors NT's
/// `IopUnloadDriver` → free system image. After this the driver's code and
/// objects must not be touched again.
///
/// # Safety
/// `loaded` must come from [`ldr_load_driver`] and not have been unloaded.
pub unsafe fn ldr_unload_driver(loaded: &LoadedDriver) {
    unsafe {
        if let Some(unload) = (*loaded.driver).driver_unload {
            unload(loaded.driver);
        }
        // Drop the driver object's reference (created by io_create_driver).
        crate::ob::ob_dereference_object(loaded.driver as *mut u8);
        // Free the image the driver ran from.
        crate::mm::pool::pool_free_any(loaded.image_base);
    }
}
