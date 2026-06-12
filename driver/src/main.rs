//! testdriver.sys — a real PE/COFF kernel driver for ntoskrnl-rs.
//!
//! An ordinary freestanding Windows kernel driver written in Rust: it imports
//! services from `ntoskrnl.exe`, exposes `DriverEntry`, creates a named
//! device with a symbolic link, registers IRP dispatch routines and a
//! `DriverUnload`, and exercises the kernel's synchronization, timer, and
//! DPC services — exactly as WDM driver source does. It is compiled by a
//! *different toolchain for a different target* (`x86_64-pc-windows-msvc`)
//! than the kernel; the shared [`ntabi`] ABI and the kernel export table are
//! all that make them interoperate.
//!
//! What it demonstrates when loaded:
//! * import resolution + relocation + execution (DriverEntry runs);
//! * the IRP stack-location I/O path (reads/writes/IOCTL);
//! * Ke synchronization: a spinlock-guarded counter, and a timer→DPC→event
//!   handshake the driver waits on during DriverEntry;
//! * a named device + symbolic link, resolvable via IoGetDeviceObjectPointer;
//! * orderly teardown via DriverUnload.

#![no_std]
#![no_main]

use core::ffi::c_void;
use core::panic::PanicInfo;
use ntabi::{
    DeviceObject, DriverObject, Irp, KDpc, KEvent, KSpinLock, KTimer, Kirql, Ntstatus,
    UnicodeString, IRP_MJ_CLOSE, IRP_MJ_CREATE, IRP_MJ_DEVICE_CONTROL, IRP_MJ_READ, IRP_MJ_WRITE,
};

/// Byte this driver writes into read buffers — the kernel self-test looks
/// for it as proof the loaded driver's code ran.
pub const DEMO_FILL_BYTE: u8 = 0x42;
/// Our private IOCTL: "return the live request count". The self-test issues
/// it and checks the value the driver maintains under its spinlock.
pub const IOCTL_GET_COUNT: u32 = 0x0022_2000;

// Services imported from the kernel (bound by the loader to the export table).
extern "win64" {
    fn DbgPrint(buffer: *const u8, length: usize) -> Ntstatus;
    fn IoCreateDevice(
        driver: *mut DriverObject,
        ext_size: usize,
        device_name: *mut UnicodeString,
        out_device: *mut *mut DeviceObject,
    ) -> Ntstatus;
    fn IoCreateSymbolicLink(link: *mut UnicodeString, target: *mut UnicodeString) -> Ntstatus;
    fn IoDeleteSymbolicLink(link: *mut UnicodeString) -> Ntstatus;
    fn IoDeleteDevice(device: *mut DeviceObject);
    fn IofCompleteRequest(irp: *mut Irp, priority_boost: i8);
    fn IoGetCurrentIrpStackLocation(irp: *mut Irp) -> *mut ntabi::IoStackLocation;

    fn KeInitializeSpinLock(lock: *mut KSpinLock);
    fn KeAcquireSpinLock(lock: *mut KSpinLock, old_irql: *mut Kirql);
    fn KeReleaseSpinLock(lock: *mut KSpinLock, new_irql: Kirql);

    fn KeInitializeEvent(event: *mut KEvent, event_type: u32, state: u8);
    fn KeSetEvent(event: *mut KEvent, increment: i32, wait: u8) -> i32;
    fn KeWaitForSingleObject(
        object: *mut c_void,
        wait_reason: u32,
        wait_mode: u8,
        alertable: u8,
        timeout: *const i64,
    ) -> Ntstatus;

    fn KeInitializeDpc(dpc: *mut KDpc, routine: ntabi::KdeferredRoutine, context: *mut c_void);
    fn KeInitializeTimer(timer: *mut KTimer);
    fn KeSetTimer(timer: *mut KTimer, due_time: i64, dpc: *mut KDpc) -> u8;
}

// --- DbgPrint helpers ------------------------------------------------------

fn print(s: &str) {
    unsafe {
        DbgPrint(s.as_ptr(), s.len());
    }
}

fn print_dec(mut v: u64) {
    let mut buf = [0u8; 20];
    let mut i = buf.len();
    if v == 0 {
        i -= 1;
        buf[i] = b'0';
    }
    while v > 0 {
        i -= 1;
        buf[i] = b'0' + (v % 10) as u8;
        v /= 10;
    }
    unsafe {
        DbgPrint(buf[i..].as_ptr(), buf.len() - i);
    }
}

// --- Driver-global state (a real driver would hang this off the device
//     extension; a static keeps the demo compact) -----------------------

static mut REQUEST_COUNT: u64 = 0;
static mut COUNT_LOCK: KSpinLock = 0;
/// Event the timer's DPC sets; DriverEntry waits on it to prove the
/// timer→DPC→event chain works end to end.
static mut TIMER_EVENT: KEvent = KEvent::zeroed();
static mut TIMER: KTimer = KTimer::zeroed();
static mut TIMER_DPC: KDpc = KDpc::zeroed();

/// Bump the spinlock-guarded request counter and return the new value.
fn bump_count() -> u64 {
    unsafe {
        let mut old_irql: Kirql = 0;
        KeAcquireSpinLock(&raw mut COUNT_LOCK, &mut old_irql);
        REQUEST_COUNT += 1;
        let v = REQUEST_COUNT;
        KeReleaseSpinLock(&raw mut COUNT_LOCK, old_irql);
        v
    }
}

/// The timer's expiry DPC: signal the event DriverEntry is waiting on.
unsafe extern "win64" fn timer_dpc(
    _dpc: *mut KDpc,
    _ctx: *mut c_void,
    _a1: *mut c_void,
    _a2: *mut c_void,
) {
    print("RustDemo: timer DPC fired, signaling event\n");
    KeSetEvent(&raw mut TIMER_EVENT, 0, 0);
}

/// IRP dispatch — reads fill the buffer, writes are accepted, the IOCTL
/// returns the live request count. Reads its parameters from the current
/// IRP stack location (the WDM way).
unsafe extern "win64" fn demo_dispatch(_device: *mut DeviceObject, irp: *mut Irp) -> Ntstatus {
    let sl = IoGetCurrentIrpStackLocation(irp);
    let major = (*sl).major_function;
    let count = bump_count();

    print("RustDemo: dispatch IRP major=");
    print_dec(major as u64);
    print(" (request #");
    print_dec(count);
    print(")\n");

    let info = if major == IRP_MJ_READ {
        let rw = (*sl).read_write();
        let buf = (*irp).system_buffer;
        if !buf.is_null() {
            for i in 0..rw.length as usize {
                *buf.add(i) = DEMO_FILL_BYTE;
            }
        }
        rw.length as u64
    } else if major == IRP_MJ_WRITE {
        (*sl).read_write().length as u64
    } else if major == IRP_MJ_DEVICE_CONTROL {
        let ioctl = (*sl).device_io_control();
        if ioctl.io_control_code == IOCTL_GET_COUNT {
            let buf = (*irp).system_buffer as *mut u64;
            if !buf.is_null() {
                *buf = count;
            }
            8
        } else {
            0
        }
    } else {
        0
    };

    (*irp).io_status.status = Ntstatus::SUCCESS;
    (*irp).io_status.information = info;
    IofCompleteRequest(irp, 0);
    Ntstatus::SUCCESS
}

/// `DriverUnload` — tear down the symbolic link and device.
unsafe extern "win64" fn demo_unload(driver: *mut DriverObject) {
    print("RustDemo: DriverUnload — cleaning up\n");
    let mut link = make_unicode(&LINK_NAME_UNITS);
    IoDeleteSymbolicLink(&mut link);
    if !(*driver).device_object.is_null() {
        IoDeleteDevice((*driver).device_object);
    }
}

// --- Static UTF-16 names (no allocator; build them inline) ----------------

// "\\Device\\RustDemo" and "\\DosDevices\\RustDemo" as UTF-16 code units.
static DEVICE_NAME_UNITS: [u16; 16] = utf16(b"\\Device\\RustDemo");
static LINK_NAME_UNITS: [u16; 20] = utf16(b"\\DosDevices\\RustDemo");

/// Compile-time ASCII→UTF-16. (`N` must equal the byte length.)
const fn utf16<const N: usize>(s: &[u8; N]) -> [u16; N] {
    let mut out = [0u16; N];
    let mut i = 0;
    while i < N {
        out[i] = s[i] as u16;
        i += 1;
    }
    out
}

fn make_unicode(units: &'static [u16]) -> UnicodeString {
    UnicodeString {
        length: (units.len() * 2) as u16,
        maximum_length: (units.len() * 2) as u16,
        buffer: units.as_ptr() as *mut u16,
    }
}

/// `DriverEntry` — the image entry point (linker `/entry:DriverEntry`).
#[no_mangle]
pub unsafe extern "win64" fn DriverEntry(
    driver: *mut DriverObject,
    _registry_path: *mut UnicodeString,
) -> Ntstatus {
    print("RustDemo: DriverEntry running from loaded PE\n");

    // Synchronization primitives.
    KeInitializeSpinLock(&raw mut COUNT_LOCK);
    KeInitializeEvent(&raw mut TIMER_EVENT, 0 /* NotificationEvent */, 0);
    KeInitializeDpc(&raw mut TIMER_DPC, timer_dpc, core::ptr::null_mut());
    KeInitializeTimer(&raw mut TIMER);

    // Arm a 2 ms timer whose DPC signals our event, then wait for it —
    // proves the timer + DPC + event chain works from driver code.
    let due: i64 = -20_000; // 2 ms, relative (100-ns units)
    KeSetTimer(&raw mut TIMER, due, &raw mut TIMER_DPC);
    print("RustDemo: waiting on timer event...\n");
    KeWaitForSingleObject(
        &raw mut TIMER_EVENT as *mut c_void,
        0,
        0,
        0,
        core::ptr::null(),
    );
    print("RustDemo: timer event satisfied\n");

    // Dispatch routines + unload.
    (*driver).major_function[IRP_MJ_CREATE as usize] = Some(demo_dispatch);
    (*driver).major_function[IRP_MJ_CLOSE as usize] = Some(demo_dispatch);
    (*driver).major_function[IRP_MJ_READ as usize] = Some(demo_dispatch);
    (*driver).major_function[IRP_MJ_WRITE as usize] = Some(demo_dispatch);
    (*driver).major_function[IRP_MJ_DEVICE_CONTROL as usize] = Some(demo_dispatch);
    (*driver).driver_unload = Some(demo_unload);

    // Named device + symbolic link.
    let mut device: *mut DeviceObject = core::ptr::null_mut();
    let mut dev_name = make_unicode(&DEVICE_NAME_UNITS);
    let status = IoCreateDevice(driver, 0, &mut dev_name, &mut device);
    if !status.is_success() {
        print("RustDemo: IoCreateDevice FAILED\n");
        return status;
    }
    let mut link_name = make_unicode(&LINK_NAME_UNITS);
    IoCreateSymbolicLink(&mut link_name, &mut dev_name);

    print("RustDemo: \\Device\\RustDemo created, dispatch + unload registered\n");
    Ntstatus::SUCCESS
}

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    loop {
        core::hint::spin_loop();
    }
}

/// `_fltused` — satisfies the MSVC FP-CRT marker referenced by core under
/// `/nodefaultlib`. We use no floating point.
#[no_mangle]
pub static _fltused: i32 = 0;
