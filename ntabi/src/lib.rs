//! # ntabi — the kernel ⇄ driver ABI contract
//!
//! Every type here is `#[repr(C)]` and shared verbatim by both ntoskrnl-rs
//! (built for `x86_64-unknown-none`) and the drivers it loads (built for
//! `x86_64-pc-windows-msvc`). Sharing one definition is what makes a driver
//! compiled by a *different toolchain for a different target* interoperate
//! with the kernel: the struct offsets and the calling convention of the
//! exported functions are identical because they come from the same source.
//!
//! Calling convention: kernel-mode Windows uses the Microsoft x64 ABI, so
//! every function crossing the boundary — exported services and driver
//! callbacks alike — is `extern "win64"`. The kernel itself is compiled
//! with the SysV ABI; Rust bridges the two at each `extern "win64"`
//! boundary automatically.
//!
//! This crate is intentionally tiny and code-free: types and signatures
//! only, no logic that could drag in a CRT or diverge between the two
//! builds.

#![no_std]
#![allow(non_snake_case)]

/// `NTSTATUS` — 32-bit status code (see ntoskrnl-rs `rtl::status` for the
/// severity encoding). Repr-transparent so it is ABI-identical to a `LONG`.
#[repr(transparent)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Ntstatus(pub u32);

impl Ntstatus {
    pub const SUCCESS: Ntstatus = Ntstatus(0x0000_0000);
    pub const UNSUCCESSFUL: Ntstatus = Ntstatus(0xC000_0001);
    pub const INSUFFICIENT_RESOURCES: Ntstatus = Ntstatus(0xC000_009A);

    #[inline]
    pub const fn is_success(self) -> bool {
        (self.0 as i32) >= 0
    }
}

/// `LIST_ENTRY` — included so shared structures can embed it with the exact
/// NT layout.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ListEntry {
    pub flink: *mut ListEntry,
    pub blink: *mut ListEntry,
}

impl ListEntry {
    pub const fn zeroed() -> Self {
        ListEntry {
            flink: core::ptr::null_mut(),
            blink: core::ptr::null_mut(),
        }
    }
}

/// `UNICODE_STRING` — counted UTF-16 string. `Length`/`MaximumLength` are
/// in **bytes**.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct UnicodeString {
    pub length: u16,
    pub maximum_length: u16,
    pub buffer: *mut u16,
}

// The buffer is `'static` (or owned elsewhere) and only read; safe to place
// a UNICODE_STRING in a `static` and share it, as object names require.
unsafe impl Send for UnicodeString {}
unsafe impl Sync for UnicodeString {}

impl UnicodeString {
    pub const fn empty() -> Self {
        UnicodeString {
            length: 0,
            maximum_length: 0,
            buffer: core::ptr::null_mut(),
        }
    }

    /// Wrap a static UTF-16 buffer without copying (`RtlInitUnicodeString`
    /// over a literal). Length/MaximumLength are set to the buffer size in
    /// bytes. The buffer must outlive the string (`'static` here).
    pub const fn from_units(units: &'static [u16]) -> Self {
        UnicodeString {
            length: (units.len() * 2) as u16,
            maximum_length: (units.len() * 2) as u16,
            buffer: units.as_ptr() as *mut u16,
        }
    }
}

/// `IO_STATUS_BLOCK` — completion status + transferred/!returned count.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct IoStatusBlock {
    pub status: Ntstatus,
    pub information: u64,
}

/// `IRP_MJ_*` major function codes (values match wdm.h). The dispatch table
/// in [`DriverObject`] is indexed by these.
pub const IRP_MJ_CREATE: u8 = 0x00;
pub const IRP_MJ_CLOSE: u8 = 0x02;
pub const IRP_MJ_READ: u8 = 0x03;
pub const IRP_MJ_WRITE: u8 = 0x04;
pub const IRP_MJ_DEVICE_CONTROL: u8 = 0x0E;
/// Size of the dispatch table (covers all NT majors, `0x00..=0x1B`).
pub const IRP_MJ_MAXIMUM_FUNCTION: usize = 0x1C;

// Driver callbacks use the Microsoft x64 ABI (`win64`) on x86_64 — the real
// kernel-mode convention. The `win64` ABI is only a valid spelling on
// x86_64; on other targets (e.g. the aarch64 host that compiles this crate
// for `cargo test`) we fall back to `C`. The fallback never executes —
// driver loading is x86_64-only — and a function pointer has identical
// layout under either ABI, so shared structs are unaffected.

/// Driver dispatch routine — `PDRIVER_DISPATCH`. Microsoft x64 ABI.
#[cfg(target_arch = "x86_64")]
pub type DriverDispatch =
    unsafe extern "win64" fn(device: *mut DeviceObject, irp: *mut Irp) -> Ntstatus;
#[cfg(not(target_arch = "x86_64"))]
pub type DriverDispatch =
    unsafe extern "C" fn(device: *mut DeviceObject, irp: *mut Irp) -> Ntstatus;

/// Driver entry point — `DRIVER_INITIALIZE`. Microsoft x64 ABI.
#[cfg(target_arch = "x86_64")]
pub type DriverInitialize =
    unsafe extern "win64" fn(driver: *mut DriverObject, registry_path: *mut UnicodeString) -> Ntstatus;
#[cfg(not(target_arch = "x86_64"))]
pub type DriverInitialize =
    unsafe extern "C" fn(driver: *mut DriverObject, registry_path: *mut UnicodeString) -> Ntstatus;

/// Driver unload routine — `PDRIVER_UNLOAD`. Microsoft x64 ABI. A driver
/// stores this in `DriverObject.driver_unload` from its `DriverEntry`; the
/// loader calls it (if set) when unloading.
#[cfg(target_arch = "x86_64")]
pub type DriverUnload = unsafe extern "win64" fn(driver: *mut DriverObject);
#[cfg(not(target_arch = "x86_64"))]
pub type DriverUnload = unsafe extern "C" fn(driver: *mut DriverObject);

/// `DRIVER_OBJECT` — one per loaded driver. The driver fills
/// `major_function[]` in its `DriverEntry`, exactly as real WDM code does
/// (`DriverObject->MajorFunction[IRP_MJ_CREATE] = MyDispatch;`). `Option<fn>`
/// is null-pointer-optimized, so the slot is ABI-identical to a raw
/// `PDRIVER_DISPATCH`.
#[repr(C)]
pub struct DriverObject {
    pub driver_name: UnicodeString,
    pub major_function: [Option<DriverDispatch>; IRP_MJ_MAXIMUM_FUNCTION],
    pub device_object: *mut DeviceObject,
    /// Optional unload routine (`DriverObject->DriverUnload = MyUnload;`).
    pub driver_unload: Option<DriverUnload>,
}

/// `DEVICE_OBJECT` — the target of IRPs.
#[repr(C)]
pub struct DeviceObject {
    pub name: UnicodeString,
    pub driver: *mut DriverObject,
    /// `DeviceExtension` — driver-private per-device state.
    pub device_extension: *mut u8,
}

// ---------------------------------------------------------------------------
// Opaque dispatcher objects
// ---------------------------------------------------------------------------
//
// Drivers embed these (`KEVENT Event;` in a device extension) and pass
// pointers to the Ke* APIs; only the kernel ever interprets the bytes. We
// therefore expose them as opaque, correctly-sized, 8-aligned blobs. The
// kernel reinterprets each as its real internal type, and static-asserts at
// compile time that the real type fits (see kernel `ldr::exports`). Sizes
// carry headroom over the current kernel layouts so adding a field doesn't
// break the ABI. This is exactly how a driver treats these objects — by
// size, never by field — so it is both simpler and faithful.

/// Kernel IRQL type (`KIRQL`).
pub type Kirql = u8;

/// `KSPIN_LOCK` — a bare lock word, as in NT (a `ULONG_PTR`).
pub type KSpinLock = usize;

macro_rules! opaque_object {
    ($(#[$m:meta])* $name:ident, $words:literal) => {
        $(#[$m])*
        #[repr(C, align(8))]
        pub struct $name {
            _opaque: [u64; $words],
        }
        impl $name {
            /// Zeroed storage; the matching `Ke*Initialize*` export fills it in.
            pub const fn zeroed() -> Self {
                Self { _opaque: [0; $words] }
            }
        }
    };
}

opaque_object!(/// `KEVENT` — embed and pass to `KeInitializeEvent`/`KeSetEvent`/…
    KEvent, 4);
opaque_object!(/// `KSEMAPHORE`.
    KSemaphore, 5);
opaque_object!(/// `KMUTANT`/`KMUTEX`.
    KMutant, 5);
opaque_object!(/// `KTIMER`.
    KTimer, 8);
opaque_object!(/// `KDPC` — a deferred procedure call object.
    KDpc, 8);

/// `KDEFERRED_ROUTINE` — a DPC/timer callback. Microsoft x64 ABI.
#[cfg(target_arch = "x86_64")]
pub type KdeferredRoutine = unsafe extern "win64" fn(
    dpc: *mut KDpc,
    deferred_context: *mut core::ffi::c_void,
    system_argument1: *mut core::ffi::c_void,
    system_argument2: *mut core::ffi::c_void,
);
#[cfg(not(target_arch = "x86_64"))]
pub type KdeferredRoutine = unsafe extern "C" fn(
    dpc: *mut KDpc,
    deferred_context: *mut core::ffi::c_void,
    system_argument1: *mut core::ffi::c_void,
    system_argument2: *mut core::ffi::c_void,
);

/// Parameters for `IRP_MJ_READ`/`IRP_MJ_WRITE` (the `Read`/`Write` arm of the
/// `IO_STACK_LOCATION.Parameters` union).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ReadWriteParams {
    pub length: u32,
    pub key: u32,
    pub byte_offset: u64,
}

/// Parameters for `IRP_MJ_DEVICE_CONTROL` (the `DeviceIoControl` arm).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct DeviceIoControlParams {
    pub output_buffer_length: u32,
    pub input_buffer_length: u32,
    pub io_control_code: u32,
    pub _type3_input_buffer: *mut core::ffi::c_void,
}

/// `IO_STACK_LOCATION` — per-driver request parameters within an IRP. Real
/// NT IRPs carry an array of these (one per device-stack layer); a driver
/// reads its layer via `IoGetCurrentIrpStackLocation`. We keep a faithful
/// subset: the major/minor function, the parameters union (Read/Write/IOCTL,
/// stored as the largest arm — 24 bytes), the target device, and a
/// completion routine + context.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct IoStackLocation {
    pub major_function: u8,
    pub minor_function: u8,
    pub flags: u8,
    pub control: u8,
    /// `Parameters` union, widened to its largest arm. Use the
    /// [`IoStackLocation::read_write`] / [`IoStackLocation::device_io_control`]
    /// accessors to view it.
    pub parameters: [u8; 24],
    pub device_object: *mut DeviceObject,
    pub file_object: *mut core::ffi::c_void,
    pub completion_routine: *mut core::ffi::c_void,
    pub context: *mut core::ffi::c_void,
}

impl IoStackLocation {
    pub const fn zeroed() -> Self {
        IoStackLocation {
            major_function: 0,
            minor_function: 0,
            flags: 0,
            control: 0,
            parameters: [0; 24],
            device_object: core::ptr::null_mut(),
            file_object: core::ptr::null_mut(),
            completion_routine: core::ptr::null_mut(),
            context: core::ptr::null_mut(),
        }
    }

    /// View `Parameters` as the Read/Write arm.
    #[inline]
    pub fn read_write(&self) -> ReadWriteParams {
        // SAFETY: the union storage is at least as large as ReadWriteParams.
        unsafe { core::ptr::read_unaligned(self.parameters.as_ptr() as *const ReadWriteParams) }
    }

    /// Set `Parameters` to the Read/Write arm.
    #[inline]
    pub fn set_read_write(&mut self, p: ReadWriteParams) {
        unsafe { core::ptr::write_unaligned(self.parameters.as_mut_ptr() as *mut ReadWriteParams, p) };
    }

    /// View `Parameters` as the DeviceIoControl arm.
    #[inline]
    pub fn device_io_control(&self) -> DeviceIoControlParams {
        unsafe {
            core::ptr::read_unaligned(self.parameters.as_ptr() as *const DeviceIoControlParams)
        }
    }

    /// Set `Parameters` to the DeviceIoControl arm.
    #[inline]
    pub fn set_device_io_control(&mut self, p: DeviceIoControlParams) {
        unsafe {
            core::ptr::write_unaligned(
                self.parameters.as_mut_ptr() as *mut DeviceIoControlParams,
                p,
            )
        };
    }
}

/// Max device-stack depth we support (IRP stack-location count). Enough for
/// a filter + function + bus driver, the common layering depth.
pub const IRP_MAX_STACK_LOCATIONS: usize = 8;

/// `IRP` — the I/O Request Packet.
///
/// Carries an inline array of [`IoStackLocation`]s (real NT allocates them
/// in a tail; an inline array of fixed depth is the same idea, simpler to
/// manage). `current_location` indexes the active one; `IoCallDriver`
/// advances it down the stack, `IoGetCurrentIrpStackLocation` returns it.
///
/// `system_buffer`/`buffer_length` hold the METHOD_BUFFERED data (also used
/// as the IOCTL system buffer). Synchronous completion is signaled through
/// `completion_event`, an opaque dispatcher-object pointer the kernel sets
/// up and `IoCompleteRequest` signals; drivers never touch it (real NT IRPs
/// likewise carry no embedded event).
#[repr(C)]
pub struct Irp {
    pub io_status: IoStatusBlock,
    /// METHOD_BUFFERED system buffer (kernel VA) + its length / IOCTL buffer.
    pub system_buffer: *mut u8,
    pub buffer_length: usize,
    /// Index of the current stack location (counts down as the IRP descends).
    pub current_location: u8,
    /// Number of valid stack locations.
    pub stack_count: u8,
    /// Opaque `*mut DispatcherHeader` signaled on completion, or null.
    pub completion_event: *mut core::ffi::c_void,
    /// The per-layer stack locations.
    pub stack: [IoStackLocation; IRP_MAX_STACK_LOCATIONS],
}
