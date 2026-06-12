//! # Io — the I/O Manager
//!
//! NT's I/O model in one paragraph: a **driver** (`DRIVER_OBJECT`) owns a
//! dispatch table of `IRP_MJ_*` handlers; it creates **devices**
//! (`DEVICE_OBJECT`) that requests are aimed at; every request travels as
//! an **IRP** — a packet carrying the major function, buffers, and
//! completion status. Synchronous or not, everything is "build an IRP, hand
//! it to the device's driver, wait for completion."
//!
//! The packet types ([`DriverObject`], [`DeviceObject`], [`Irp`], …) live
//! in the shared [`ntabi`] crate, so a driver compiled by a *different
//! toolchain for Windows* sees the exact same layouts and can fill
//! `DriverObject.major_function[]` directly, as real WDM code does. Dispatch
//! routines use the Microsoft x64 calling convention (`extern "win64"`).
//!
//! Synchronous completion: the IRP carries an opaque `completion_event`
//! pointer (a `*mut DispatcherHeader`); [`io_complete_request`] signals it,
//! and [`io_synchronous_request`] waits on a caller-stack event — the same
//! shape as `IoBuildSynchronousFsdRequest` + `KeWaitForSingleObject`.

pub mod console;
pub mod namespace;
pub mod null;
pub mod ramfs;

use crate::ke::dispatcher::{ke_wait_for_single_object, DispatcherObjectType, Kevent};
use crate::mm::pool::{pool_alloc_checked, pool_free, pool_tag};
use crate::ob;
use crate::rtl::NtStatus;
use crate::w;

// The boundary types come from the shared ABI crate; re-export under the
// familiar Io names so kernel code and drivers speak the same vocabulary.
pub use ntabi::{
    DeviceIoControlParams, DeviceObject, DriverDispatch, DriverInitialize, DriverObject,
    IoStackLocation, IoStatusBlock, Irp, Ntstatus, ReadWriteParams,
    UnicodeString as AbiUnicodeString, IRP_MAX_STACK_LOCATIONS, IRP_MJ_CLOSE, IRP_MJ_CREATE,
    IRP_MJ_DEVICE_CONTROL, IRP_MJ_MAXIMUM_FUNCTION, IRP_MJ_READ, IRP_MJ_WRITE,
};

const TAG_IRP: u32 = pool_tag(b"Irp ");

/// Bridge an `rtl::NtStatus` to the ABI's `Ntstatus` (both transparent u32).
#[inline]
pub fn to_abi(status: NtStatus) -> Ntstatus {
    Ntstatus(status.0)
}
/// Bridge an ABI `Ntstatus` back to the kernel's `rtl::NtStatus`.
#[inline]
pub fn from_abi(status: Ntstatus) -> NtStatus {
    NtStatus(status.0)
}

/// Object-manager type descriptors.
pub static DEVICE_TYPE: ob::ObjectType = ob::ObjectType {
    name: crate::rtl::string::UnicodeString::from_units(w!("Device")),
    delete: None,
};
pub static DRIVER_TYPE: ob::ObjectType = ob::ObjectType {
    name: crate::rtl::string::UnicodeString::from_units(w!("Driver")),
    delete: None,
};

/// `IoCreateDriver` — allocate the driver object and let `init` (a
/// `DriverEntry`) populate its dispatch table.
pub fn io_create_driver(
    name: AbiUnicodeString,
    init: DriverInitialize,
) -> Result<*mut DriverObject, NtStatus> {
    let driver = ob::ob_create_object(
        &DRIVER_TYPE,
        DriverObject {
            driver_name: name,
            major_function: [None; IRP_MJ_MAXIMUM_FUNCTION],
            device_object: core::ptr::null_mut(),
            driver_unload: None,
        },
    )?;
    // SAFETY: fresh, exclusively-owned object; DriverEntry uses win64 ABI.
    let status = unsafe { init(driver, core::ptr::null_mut()) };
    if !status.is_success() {
        unsafe { ob::ob_dereference_object(driver as *mut u8) };
        return Err(from_abi(status));
    }
    Ok(driver)
}

/// `IoCreateDevice` — create a device owned by `driver`.
pub fn io_create_device(
    driver: *mut DriverObject,
    name: AbiUnicodeString,
    extension: *mut u8,
) -> Result<*mut DeviceObject, NtStatus> {
    let device = ob::ob_create_object(
        &DEVICE_TYPE,
        DeviceObject {
            name,
            driver,
            device_extension: extension,
        },
    )?;
    unsafe { (*driver).device_object = device };
    // Register a named device so IoGetDeviceObjectPointer can find it
    // (anonymous devices with an empty name are skipped by the registry).
    namespace::register_device(&name, device);
    Ok(device)
}

/// `IoAllocateIrp(StackSize)` — pool-allocate a request packet with
/// `stack_size` stack locations. `current_location` starts one past the top
/// (`stack_size`), so the initiator fills the top location via
/// [`io_get_next_stack_location`] and [`io_call_driver`] steps down into it.
pub fn io_allocate_irp(stack_size: u8) -> Result<*mut Irp, NtStatus> {
    let n = (stack_size as usize).clamp(1, IRP_MAX_STACK_LOCATIONS) as u8;
    let irp = pool_alloc_checked(core::mem::size_of::<Irp>(), TAG_IRP)? as *mut Irp;
    unsafe {
        irp.write(Irp {
            io_status: IoStatusBlock {
                status: Ntstatus(NtStatus::PENDING.0),
                information: 0,
            },
            system_buffer: core::ptr::null_mut(),
            buffer_length: 0,
            current_location: n,
            stack_count: n,
            completion_event: core::ptr::null_mut(),
            stack: [IoStackLocation::zeroed(); IRP_MAX_STACK_LOCATIONS],
        });
    }
    Ok(irp)
}

/// `IoFreeIrp`.
pub fn io_free_irp(irp: *mut Irp) {
    pool_free(irp as *mut u8, TAG_IRP);
}

/// `IoGetCurrentIrpStackLocation` — the location the current driver owns.
///
/// # Safety
/// `irp` live; valid only after at least one [`io_call_driver`] step.
pub unsafe fn io_get_current_stack_location(irp: *mut Irp) -> *mut IoStackLocation {
    unsafe {
        let i = (*irp).current_location as usize;
        &raw mut (*irp).stack[i.min(IRP_MAX_STACK_LOCATIONS - 1)]
    }
}

/// `IoGetNextIrpStackLocation` — the location set up for the next driver
/// down the stack (the initiator fills this before `IoCallDriver`).
///
/// # Safety
/// `irp` live with `current_location >= 1`.
pub unsafe fn io_get_next_stack_location(irp: *mut Irp) -> *mut IoStackLocation {
    unsafe {
        let i = ((*irp).current_location as usize).saturating_sub(1);
        &raw mut (*irp).stack[i]
    }
}

/// `IoCallDriver` — descend one stack location and invoke the target
/// device's driver dispatch routine (Microsoft x64 ABI) for the major
/// function recorded in that location. Returns the routine's status, or
/// `STATUS_INVALID_DEVICE_REQUEST` if there is no handler.
///
/// # Safety
/// `device` and `irp` must be live; the next stack location must be set up.
pub unsafe fn io_call_driver(device: *mut DeviceObject, irp: *mut Irp) -> NtStatus {
    unsafe {
        // Descend to the location the initiator prepared.
        if (*irp).current_location == 0 {
            (*irp).io_status.status = Ntstatus(NtStatus::INVALID_DEVICE_REQUEST.0);
            return NtStatus::INVALID_DEVICE_REQUEST;
        }
        (*irp).current_location -= 1;
        let sl = io_get_current_stack_location(irp);
        (*sl).device_object = device;
        let major = (*sl).major_function as usize;
        let driver = (*device).driver;
        match (*driver).major_function[major] {
            Some(dispatch) => from_abi(dispatch(device, irp)),
            None => {
                (*irp).io_status.status = Ntstatus(NtStatus::INVALID_DEVICE_REQUEST.0);
                io_complete_request(irp);
                NtStatus::INVALID_DEVICE_REQUEST
            }
        }
    }
}

/// `IoCompleteRequest` — finish an IRP and signal its completion event (if
/// the issuer set one). Drivers call this from their dispatch routines.
///
/// # Safety
/// `irp` must be live; `io_status` must already be filled in.
pub unsafe fn io_complete_request(irp: *mut Irp) {
    unsafe {
        let ev = (*irp).completion_event as *mut crate::ke::dispatcher::DispatcherHeader;
        if !ev.is_null() {
            crate::ke::scheduler::ki_signal_object(ev);
        }
    }
}

/// Synchronous convenience used by kernel components and the self tests:
/// issue a buffered request and block until the driver completes it. Sets up
/// a single (top) stack location with the major function and a Read/Write
/// length, points the IRP's completion event at a stack event, and waits —
/// the shape of `IoBuildSynchronousFsdRequest` + `KeWaitForSingleObject`.
///
/// # Safety
/// `device` live; `buffer` valid for `len` bytes for the call's duration.
pub unsafe fn io_synchronous_request(
    device: *mut DeviceObject,
    major: u8,
    buffer: *mut u8,
    len: usize,
) -> Result<IoStatusBlock, NtStatus> {
    unsafe {
        let irp = io_allocate_irp(1)?;
        let mut event = Kevent::new(DispatcherObjectType::NotificationEvent, false);
        (*irp).system_buffer = buffer;
        (*irp).buffer_length = len;
        (*irp).completion_event = (&raw mut event.header) as *mut core::ffi::c_void;

        // Fill the next (top) stack location the target device will see.
        let next = io_get_next_stack_location(irp);
        (*next).major_function = major;
        (*next).device_object = device;
        (*next).set_read_write(ReadWriteParams {
            length: len as u32,
            key: 0,
            byte_offset: 0,
        });

        let status = io_call_driver(device, irp);
        if status == NtStatus::PENDING {
            ke_wait_for_single_object(&raw mut event.header);
        }
        let iosb = (*irp).io_status;
        io_free_irp(irp);
        if from_abi(iosb.status).is_success() {
            Ok(iosb)
        } else {
            Err(from_abi(iosb.status))
        }
    }
}

/// Synchronous IOCTL: like [`io_synchronous_request`] but sets up the
/// DeviceIoControl parameters (control code + buffer lengths). Used to drive
/// a driver's `IRP_MJ_DEVICE_CONTROL` handler from the self tests.
///
/// # Safety
/// As [`io_synchronous_request`].
pub unsafe fn io_synchronous_ioctl(
    device: *mut DeviceObject,
    control_code: u32,
    buffer: *mut u8,
    in_len: usize,
    out_len: usize,
) -> Result<IoStatusBlock, NtStatus> {
    unsafe {
        let irp = io_allocate_irp(1)?;
        let mut event = Kevent::new(DispatcherObjectType::NotificationEvent, false);
        (*irp).system_buffer = buffer;
        (*irp).buffer_length = in_len.max(out_len);
        (*irp).completion_event = (&raw mut event.header) as *mut core::ffi::c_void;

        let next = io_get_next_stack_location(irp);
        (*next).major_function = IRP_MJ_DEVICE_CONTROL;
        (*next).device_object = device;
        (*next).set_device_io_control(DeviceIoControlParams {
            output_buffer_length: out_len as u32,
            input_buffer_length: in_len as u32,
            io_control_code: control_code,
            _type3_input_buffer: core::ptr::null_mut(),
        });

        let status = io_call_driver(device, irp);
        if status == NtStatus::PENDING {
            ke_wait_for_single_object(&raw mut event.header);
        }
        let iosb = (*irp).io_status;
        io_free_irp(irp);
        if from_abi(iosb.status).is_success() {
            Ok(iosb)
        } else {
            Err(from_abi(iosb.status))
        }
    }
}
