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

/// `DRIVER_OBJECT` — one per loaded driver, laid out to match the documented
/// NT x64 `DRIVER_OBJECT` so an unmodified Windows driver finds every field
/// where it expects it (notably `MajorFunction[]` at 0x70, `DriverUnload` at
/// 0x68). The driver fills `major_function[]` in its `DriverEntry` exactly as
/// real WDM code does. `Option<fn>` is null-pointer-optimized, so each slot is
/// ABI-identical to a raw `PDRIVER_DISPATCH`. Fields we don't model
/// (`DriverStart`, `FastIoDispatch`, …) are present for layout only.
#[repr(C)]
pub struct DriverObject {
    pub type_: i16,                       // 0x00 Type (IO_TYPE_DRIVER = 4)
    pub size: i16,                        // 0x02 Size
    _reserved_04: u32,                    // 0x04 (alignment)
    pub device_object: *mut DeviceObject, // 0x08 DeviceObject
    pub flags: u32,                       // 0x10 Flags
    _reserved_14: u32,                    // 0x14
    pub driver_start: *mut core::ffi::c_void, // 0x18 DriverStart
    pub driver_size: u32,                 // 0x20 DriverSize
    _reserved_24: u32,                    // 0x24
    pub driver_section: *mut core::ffi::c_void, // 0x28 DriverSection
    pub driver_extension: *mut core::ffi::c_void, // 0x30 DriverExtension
    pub driver_name: UnicodeString,       // 0x38 DriverName (16 bytes)
    pub hardware_database: *mut core::ffi::c_void, // 0x48 HardwareDatabase
    pub fast_io_dispatch: *mut core::ffi::c_void,  // 0x50 FastIoDispatch
    pub driver_init: *mut core::ffi::c_void,       // 0x58 DriverInit
    pub driver_start_io: *mut core::ffi::c_void,   // 0x60 DriverStartIo
    pub driver_unload: Option<DriverUnload>,       // 0x68 DriverUnload
    pub major_function: [Option<DriverDispatch>; IRP_MJ_MAXIMUM_FUNCTION], // 0x70
}

// Lock the NT x64 offsets at compile time.
const _: () = {
    use core::mem::offset_of;
    assert!(offset_of!(DriverObject, device_object) == 0x08);
    assert!(offset_of!(DriverObject, flags) == 0x10);
    assert!(offset_of!(DriverObject, driver_name) == 0x38);
    assert!(offset_of!(DriverObject, driver_unload) == 0x68);
    assert!(offset_of!(DriverObject, major_function) == 0x70);
    assert!(core::mem::size_of::<DriverObject>() == 0x150);
};

/// `DEVICE_OBJECT` — the target of IRPs, laid out to match the documented NT
/// x64 `DEVICE_OBJECT`. A driver reads/writes `Flags` (0x30), `DeviceExtension`
/// (0x40), `StackSize` (0x4C) and `AlignmentRequirement` (0x78) directly; those
/// must be at the NT offsets. The unmodeled tail (device queue, DPC, security
/// descriptor, the kernel's private `DeviceObjectExtension`) is reserved space
/// so the object is the right total size. A device's *name* is not a field in
/// NT — it lives in the object namespace — so it is not stored here.
#[repr(C)]
pub struct DeviceObject {
    pub type_: i16,                          // 0x00 Type (IO_TYPE_DEVICE = 3)
    pub size: u16,                           // 0x02 Size
    pub reference_count: i32,                // 0x04 ReferenceCount
    pub driver: *mut DriverObject,           // 0x08 DriverObject
    pub next_device: *mut DeviceObject,      // 0x10 NextDevice
    pub attached_device: *mut DeviceObject,  // 0x18 AttachedDevice
    pub current_irp: *mut Irp,               // 0x20 CurrentIrp
    pub timer: *mut core::ffi::c_void,       // 0x28 Timer
    pub flags: u32,                          // 0x30 Flags
    pub characteristics: u32,                // 0x34 Characteristics
    pub vpb: *mut core::ffi::c_void,         // 0x38 Vpb
    /// `DeviceExtension` — driver-private per-device state.
    pub device_extension: *mut u8,           // 0x40 DeviceExtension
    pub device_type: u32,                    // 0x48 DeviceType
    pub stack_size: i8,                      // 0x4C StackSize
    _reserved_4d: [u8; 3],                   // 0x4D
    _queue: [u8; 0x28],                      // 0x50 Queue union (WAIT_CONTEXT_BLOCK)
    pub alignment_requirement: u32,          // 0x78 AlignmentRequirement
    _reserved_7c: u32,                       // 0x7C
    // 0x80.. KDEVICE_QUEUE, KDPC, ActiveThreadCount, SecurityDescriptor,
    // DeviceLock (KEVENT), SectorSize, Spare1, DeviceObjectExtension, Reserved.
    _tail: [u8; 0x150 - 0x80],               // reserved to the full NT size
}

const _: () = {
    use core::mem::offset_of;
    assert!(offset_of!(DeviceObject, driver) == 0x08);
    assert!(offset_of!(DeviceObject, flags) == 0x30);
    assert!(offset_of!(DeviceObject, device_extension) == 0x40);
    assert!(offset_of!(DeviceObject, device_type) == 0x48);
    assert!(offset_of!(DeviceObject, stack_size) == 0x4C);
    assert!(offset_of!(DeviceObject, alignment_requirement) == 0x78);
    assert!(core::mem::size_of::<DeviceObject>() == 0x150);
};

impl DriverObject {
    /// A zeroed driver object with `Type`/`Size` set, ready for the loader to
    /// fill `DriverInit`/`DriverName` and the driver to fill `MajorFunction[]`.
    pub const fn new(driver_name: UnicodeString) -> Self {
        DriverObject {
            type_: 4, // IO_TYPE_DRIVER
            size: core::mem::size_of::<DriverObject>() as i16,
            _reserved_04: 0,
            device_object: core::ptr::null_mut(),
            flags: 0,
            _reserved_14: 0,
            driver_start: core::ptr::null_mut(),
            driver_size: 0,
            _reserved_24: 0,
            driver_section: core::ptr::null_mut(),
            driver_extension: core::ptr::null_mut(),
            driver_name,
            hardware_database: core::ptr::null_mut(),
            fast_io_dispatch: core::ptr::null_mut(),
            driver_init: core::ptr::null_mut(),
            driver_start_io: core::ptr::null_mut(),
            driver_unload: None,
            major_function: [None; IRP_MJ_MAXIMUM_FUNCTION],
        }
    }
}

impl DeviceObject {
    /// A zeroed device object with `Type`/`Size` set and the given driver +
    /// extension. `StackSize` defaults to 1 (a single-layer device).
    pub const fn new(driver: *mut DriverObject, device_extension: *mut u8) -> Self {
        DeviceObject {
            type_: 3, // IO_TYPE_DEVICE
            size: core::mem::size_of::<DeviceObject>() as u16,
            reference_count: 0,
            driver,
            next_device: core::ptr::null_mut(),
            attached_device: core::ptr::null_mut(),
            current_irp: core::ptr::null_mut(),
            timer: core::ptr::null_mut(),
            flags: 0,
            characteristics: 0,
            vpb: core::ptr::null_mut(),
            device_extension,
            device_type: 0,
            stack_size: 1,
            _reserved_4d: [0; 3],
            _queue: [0; 0x28],
            alignment_requirement: 0,
            _reserved_7c: 0,
            _tail: [0; 0x150 - 0x80],
        }
    }
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

/// Parameters for `IRP_MJ_DEVICE_CONTROL` (the `DeviceIoControl` arm). The NT
/// union packs each field on an 8-byte boundary (other arms hold pointers), so
/// `InputBufferLength` is at +0x08, `IoControlCode` at +0x10, `Type3InputBuffer`
/// at +0x18 — not tightly packed.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct DeviceIoControlParams {
    pub output_buffer_length: u32, // +0x00
    _pad0: u32,                    // +0x04
    pub input_buffer_length: u32,  // +0x08
    _pad1: u32,                    // +0x0C
    pub io_control_code: u32,      // +0x10
    _pad2: u32,                    // +0x14
    pub type3_input_buffer: *mut core::ffi::c_void, // +0x18
}

impl DeviceIoControlParams {
    pub const fn new(output_buffer_length: u32, input_buffer_length: u32, io_control_code: u32) -> Self {
        DeviceIoControlParams {
            output_buffer_length,
            _pad0: 0,
            input_buffer_length,
            _pad1: 0,
            io_control_code,
            _pad2: 0,
            type3_input_buffer: core::ptr::null_mut(),
        }
    }
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
    pub major_function: u8, // 0x00
    pub minor_function: u8, // 0x01
    pub flags: u8,          // 0x02
    pub control: u8,        // 0x03
    _pad04: u32,            // 0x04
    /// `Parameters` union (NT places it at +0x08), widened to its 32-byte
    /// span. Use the [`IoStackLocation::read_write`] /
    /// [`IoStackLocation::device_io_control`] accessors to view it.
    pub parameters: [u8; 0x20], // 0x08
    pub device_object: *mut DeviceObject, // 0x28
    pub file_object: *mut core::ffi::c_void, // 0x30
    pub completion_routine: *mut core::ffi::c_void, // 0x38
    pub context: *mut core::ffi::c_void, // 0x40
}

const _: () = {
    use core::mem::offset_of;
    assert!(offset_of!(IoStackLocation, parameters) == 0x08);
    assert!(offset_of!(IoStackLocation, device_object) == 0x28);
    assert!(offset_of!(IoStackLocation, completion_routine) == 0x38);
    assert!(offset_of!(IoStackLocation, context) == 0x40);
    assert!(core::mem::size_of::<IoStackLocation>() == 0x48);
};

impl IoStackLocation {
    pub const fn zeroed() -> Self {
        IoStackLocation {
            major_function: 0,
            minor_function: 0,
            flags: 0,
            control: 0,
            _pad04: 0,
            parameters: [0; 0x20],
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

/// `IRP` — the I/O Request Packet, laid out to match the documented NT x64
/// `_IRP` so an unmodified driver finds `IoStatus` (0x30), the system buffer
/// (`AssociatedIrp.SystemBuffer`, 0x18), `UserBuffer` (0x70) and the current
/// stack-location pointer (`Tail.Overlay.CurrentStackLocation`, 0xB8) where it
/// expects them. The per-layer [`IoStackLocation`]s are *not* inline: NT
/// allocates them in a tail right after the fixed `IRP`, and
/// `CurrentStackLocation` points into that array. Synchronous completion uses
/// the NT `UserEvent` field (a `KEVENT*` that `IoCompleteRequest` signals) —
/// no extra, non-standard field is needed.
#[repr(C)]
pub struct Irp {
    pub type_: i16,                 // 0x00 Type (IO_TYPE_IRP = 6)
    pub size: u16,                  // 0x02 Size (whole packet, incl. stack tail)
    _alloc_processor: u32,          // 0x04 AllocationProcessorNumber + Reserved
    pub mdl_address: *mut core::ffi::c_void, // 0x08 MdlAddress
    pub flags: u32,                 // 0x10 Flags
    _pad14: u32,                    // 0x14
    /// `AssociatedIrp.SystemBuffer` — the METHOD_BUFFERED / IOCTL system buffer.
    pub system_buffer: *mut u8,     // 0x18
    _thread_list_entry: [usize; 2], // 0x20 ThreadListEntry (LIST_ENTRY)
    pub io_status: IoStatusBlock,   // 0x30 IoStatus (Status@0x30, Information@0x38)
    pub requestor_mode: i8,         // 0x40
    pub pending_returned: u8,       // 0x41
    pub stack_count: i8,            // 0x42 StackCount
    pub current_location: i8,       // 0x43 CurrentLocation (counts down)
    pub cancel: u8,                 // 0x44
    pub cancel_irql: u8,            // 0x45
    pub apc_environment: i8,        // 0x46
    pub allocation_flags: u8,       // 0x47
    pub user_iosb: *mut IoStatusBlock, // 0x48
    /// `UserEvent` — signaled by `IoCompleteRequest`; our synchronous waiters
    /// point this at a stack `KEVENT`.
    pub user_event: *mut core::ffi::c_void, // 0x50
    _overlay: [usize; 2],           // 0x58 Overlay union
    pub cancel_routine: *mut core::ffi::c_void, // 0x68
    pub user_buffer: *mut u8,       // 0x70 UserBuffer
    _driver_context: [usize; 4],    // 0x78 Tail.Overlay.DriverContext[4]
    pub thread: *mut core::ffi::c_void, // 0x98
    pub auxiliary_buffer: *mut u8,  // 0xA0
    _list_entry: [usize; 2],        // 0xA8 Tail.Overlay.ListEntry (LIST_ENTRY)
    /// `Tail.Overlay.CurrentStackLocation` — points into the stack tail.
    pub current_stack_location: *mut IoStackLocation, // 0xB8
    pub original_file_object: *mut core::ffi::c_void,  // 0xC0
    _reserved_c8: usize,            // 0xC8 (to sizeof(_IRP) = 0xD0)
}

const _: () = {
    use core::mem::offset_of;
    assert!(offset_of!(Irp, mdl_address) == 0x08);
    assert!(offset_of!(Irp, system_buffer) == 0x18);
    assert!(offset_of!(Irp, io_status) == 0x30);
    assert!(offset_of!(Irp, user_event) == 0x50);
    assert!(offset_of!(Irp, user_buffer) == 0x70);
    assert!(offset_of!(Irp, current_stack_location) == 0xB8);
    assert!(core::mem::size_of::<Irp>() == 0xD0);
};
