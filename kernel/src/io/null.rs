//! `\Device\Null` — the canonical do-nothing driver, built in-kernel.
//!
//! This is the smallest complete driver and serves as both the I/O path's
//! unit test and the in-tree reference for the dispatch-table model. It now
//! uses the exact same shared ABI ([`ntabi`]) and Microsoft-x64 dispatch
//! signature that an externally-loaded `.sys` uses — the only difference is
//! that this one is linked into the kernel instead of loaded from a PE.
//!
//! Semantics, identical to NT's: reads complete with zero bytes (EOF),
//! writes succeed and swallow everything, create/close always succeed.

use super::{
    io_complete_request, io_create_device, io_create_driver, AbiUnicodeString, DeviceObject,
    DriverObject, Irp, Ntstatus, IRP_MJ_CLOSE, IRP_MJ_CREATE, IRP_MJ_READ, IRP_MJ_WRITE,
};
use crate::rtl::NtStatus;
use crate::w;

static DRIVER_NAME: AbiUnicodeString = AbiUnicodeString::from_units(w!("\\Driver\\Null"));
static DEVICE_NAME: AbiUnicodeString = AbiUnicodeString::from_units(w!("\\Device\\Null"));

/// The single dispatch routine: every supported major completes inline.
/// Microsoft x64 ABI, exactly like an external driver's dispatch routine.
/// Reads its major function from the current IRP stack location.
unsafe extern "win64" fn null_dispatch(_device: *mut DeviceObject, irp: *mut Irp) -> Ntstatus {
    // SAFETY: the I/O manager hands us a live IRP we own until completion.
    unsafe {
        let sl = super::io_get_current_stack_location(irp);
        (*irp).io_status.information = match (*sl).major_function {
            // Bit bucket accepts all bytes: report the write length from the
            // stack location (NT keeps the length there, not on the IRP).
            IRP_MJ_WRITE => (*sl).read_write().length as u64,
            _ => 0, // reads: instant EOF
        };
        (*irp).io_status.status = Ntstatus(NtStatus::SUCCESS.0);
        io_complete_request(irp);
    }
    Ntstatus(NtStatus::SUCCESS.0)
}

/// DriverEntry for null.sys. Microsoft x64 ABI.
unsafe extern "win64" fn driver_entry(
    driver: *mut DriverObject,
    _registry_path: *mut AbiUnicodeString,
) -> Ntstatus {
    // SAFETY: fresh driver object from io_create_driver.
    unsafe {
        for major in [IRP_MJ_CREATE, IRP_MJ_CLOSE, IRP_MJ_READ, IRP_MJ_WRITE] {
            (*driver).major_function[major as usize] = Some(null_dispatch);
        }
    }
    Ntstatus(NtStatus::SUCCESS.0)
}

/// Load the driver and create `\Device\Null`; returns the device so the
/// self tests can throw IRPs at it.
pub fn initialize() -> Result<*mut DeviceObject, NtStatus> {
    let driver = io_create_driver(DRIVER_NAME, driver_entry)?;
    io_create_device(driver, DEVICE_NAME, core::ptr::null_mut())
}
