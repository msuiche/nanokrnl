//! The kernel export table — ntoskrnl-rs's equivalent of `ntoskrnl.exe`'s
//! export directory.
//!
//! A loaded driver imports functions *by name* from `ntoskrnl.exe`; the PE
//! loader ([`super::pe`]) resolves each imported name against the table
//! built here. Every exported routine uses the Microsoft x64 calling
//! convention (`extern "win64"`) and operates on the shared [`ntabi`] types,
//! so a driver compiled by MSVC/clang for Windows calls them exactly as it
//! would the real kernel.
//!
//! These are thin shims over the in-tree Rust APIs: the shim adapts the ABI
//! (win64 ↔ the kernel's SysV) and the status types, then forwards. Adding
//! a new export is one `extern "win64"` shim plus one [`EXPORTS`] row.

use crate::ke::dispatcher::{
    DispatcherHeader, DispatcherObjectType, Kevent, Kmutant, Ksemaphore, Ktimer,
};
use crate::ke::dpc::Kdpc;
use crate::ke::{irql, scheduler, spinlock};
use crate::io;
use crate::mm::pool;
use ntabi::{
    DeviceObject, DriverObject, Irp, KEvent, KMutant, KSemaphore, KSpinLock, Kirql, Ntstatus,
    UnicodeString,
};

// Compile-time proof that a driver's opaque dispatcher-object storage is at
// least as large as the kernel's real type it gets reinterpreted as. If the
// kernel layout ever outgrows the ntabi blob, the build fails here (bump the
// word count in ntabi) rather than corrupting memory at runtime.
const _: () = {
    use core::mem::{align_of, size_of};
    assert!(size_of::<Kevent>() <= size_of::<KEvent>());
    assert!(size_of::<Ksemaphore>() <= size_of::<KSemaphore>());
    assert!(size_of::<Kmutant>() <= size_of::<KMutant>());
    assert!(align_of::<Kevent>() <= align_of::<KEvent>());
    assert!(align_of::<Ksemaphore>() <= align_of::<KSemaphore>());
    assert!(align_of::<Kmutant>() <= align_of::<KMutant>());
};

/// NT 100-ns timeout interval → our clock ticks (~1 ms). A null pointer means
/// "wait forever"; a negative value is a relative interval; a non-negative
/// (absolute) value is treated relatively as a best effort (we have a tick
/// counter, not a wall clock — documented).
unsafe fn timeout_to_ticks(timeout: *const i64) -> Option<u64> {
    if timeout.is_null() {
        return None; // infinite
    }
    let t = unsafe { *timeout };
    let hundred_ns = if t < 0 { (-t) as u64 } else { t as u64 };
    Some(hundred_ns / 10_000) // 10,000 * 100 ns = 1 ms ≈ 1 tick
}

// ---------------------------------------------------------------------------
// Exported routines (Microsoft x64 ABI)
// ---------------------------------------------------------------------------

/// `DbgPrint`-style debug output. Simplified to a counted UTF-8 string
/// (`buffer`, `length`) so drivers can log without a varargs formatter.
unsafe extern "win64" fn k_dbg_print(buffer: *const u8, length: usize) -> Ntstatus {
    if !buffer.is_null() && length > 0 {
        let bytes = unsafe { core::slice::from_raw_parts(buffer, length) };
        if let Ok(s) = core::str::from_utf8(bytes) {
            crate::kd_print!("{}", s);
        }
    }
    Ntstatus::SUCCESS
}

/// `ExAllocatePoolWithTag(POOL_TYPE, SIZE_T, ULONG)`. The pool type is
/// accepted and ignored (only NonPaged exists); returns null on failure
/// just like the kernel export.
unsafe extern "win64" fn k_ex_allocate_pool_with_tag(
    _pool_type: u32,
    size: usize,
    tag: u32,
) -> *mut u8 {
    pool::pool_alloc(size, tag)
}

/// `ExFreePoolWithTag(PVOID, ULONG)`.
unsafe extern "win64" fn k_ex_free_pool_with_tag(p: *mut u8, tag: u32) {
    pool::pool_free(p, tag)
}

/// `IoCreateDevice(DriverObject, DeviceExtensionSize, DeviceName, DeviceType,
/// DeviceCharacteristics, Exclusive, DeviceObject)` — the real NT prototype.
/// Create a device named `device_name` owned by `driver`, with `ext_size`
/// bytes of zeroed device extension, writing the new device to `*out_device`.
/// We don't model device types, characteristics, or exclusivity, so those
/// arguments are accepted and ignored — but the shim must take all seven so
/// an unmodified driver (e.g. null.sys) finds `out_device` in the 7th slot,
/// not where a 4-argument prototype would put it.
#[allow(clippy::too_many_arguments)]
unsafe extern "win64" fn k_io_create_device(
    driver: *mut DriverObject,
    ext_size: usize,
    device_name: *mut UnicodeString,
    _device_type: u32,
    _device_characteristics: u32,
    _exclusive: u8,
    out_device: *mut *mut DeviceObject,
) -> Ntstatus {
    unsafe {
        let extension = if ext_size > 0 {
            let p = pool::pool_alloc(ext_size, crate::mm::pool::pool_tag(b"DevE"));
            if p.is_null() {
                return Ntstatus::INSUFFICIENT_RESOURCES;
            }
            p
        } else {
            core::ptr::null_mut()
        };
        let name = if device_name.is_null() {
            UnicodeString::empty()
        } else {
            *device_name
        };
        match io::io_create_device(driver, name, extension) {
            Ok(dev) => {
                if !out_device.is_null() {
                    *out_device = dev;
                }
                Ntstatus::SUCCESS
            }
            Err(e) => io::to_abi(e),
        }
    }
}

/// `IoCompleteRequest(PIRP, CCHAR)` — the boost argument is accepted and
/// ignored (priority boosts are future work).
unsafe extern "win64" fn k_io_complete_request(irp: *mut Irp, _priority_boost: i8) {
    unsafe { io::io_complete_request(irp) }
}

/// `RtlInitUnicodeString(PUNICODE_STRING, PCWSTR)` — initialize `dst` from a
/// NUL-terminated wide string `src`.
unsafe extern "win64" fn k_rtl_init_unicode_string(dst: *mut UnicodeString, src: *const u16) {
    unsafe {
        if dst.is_null() {
            return;
        }
        let mut len = 0usize;
        if !src.is_null() {
            while *src.add(len) != 0 {
                len += 1;
            }
        }
        (*dst).length = (len * 2) as u16;
        (*dst).maximum_length = ((len + 1) * 2) as u16;
        (*dst).buffer = src as *mut u16;
    }
}

/// `KeBugCheckEx` — let a driver request a controlled crash.
unsafe extern "win64" fn k_ke_bug_check_ex(
    code: u32,
    p1: u64,
    p2: u64,
    p3: u64,
    p4: u64,
) -> ! {
    crate::ke::bugcheck::ke_bug_check_ex(code, p1, p2, p3, p4)
}

// ---------------------------------------------------------------------------
// Tier 1: synchronization, IRQL, time, memory
// ---------------------------------------------------------------------------

// EVENT_TYPE values (wdm.h): NotificationEvent = 0, SynchronizationEvent = 1.
unsafe extern "win64" fn k_ke_initialize_event(event: *mut KEvent, event_type: u32, state: u8) {
    let kind = if event_type == 1 {
        DispatcherObjectType::SynchronizationEvent
    } else {
        DispatcherObjectType::NotificationEvent
    };
    unsafe { (event as *mut Kevent).write(Kevent::new(kind, state != 0)) };
}

unsafe extern "win64" fn k_ke_set_event(event: *mut KEvent, _increment: i32, _wait: u8) -> i32 {
    unsafe { (*(event as *mut Kevent)).set() }
}

unsafe extern "win64" fn k_ke_clear_event(event: *mut KEvent) {
    unsafe {
        (*(event as *mut Kevent)).reset();
    }
}

unsafe extern "win64" fn k_ke_reset_event(event: *mut KEvent) -> i32 {
    unsafe { (*(event as *mut Kevent)).reset() }
}

/// `KeWaitForSingleObject(Object, WaitReason, WaitMode, Alertable, Timeout)`.
/// The object's `DispatcherHeader` is at offset 0 of every dispatcher type.
unsafe extern "win64" fn k_ke_wait_for_single_object(
    object: *mut core::ffi::c_void,
    _wait_reason: u32,
    _wait_mode: u8,
    _alertable: u8,
    timeout: *const i64,
) -> Ntstatus {
    unsafe {
        let ticks = timeout_to_ticks(timeout);
        io::to_abi(scheduler::ki_wait_for_object(object as *mut DispatcherHeader, ticks))
    }
}

unsafe extern "win64" fn k_ke_initialize_semaphore(sem: *mut KSemaphore, count: i32, limit: i32) {
    unsafe { (sem as *mut Ksemaphore).write(Ksemaphore::new(count, limit)) };
}

unsafe extern "win64" fn k_ke_release_semaphore(
    sem: *mut KSemaphore,
    _increment: i32,
    adjustment: i32,
    _wait: u8,
) -> i32 {
    unsafe { (*(sem as *mut Ksemaphore)).release(adjustment).unwrap_or(0) }
}

unsafe extern "win64" fn k_ke_initialize_mutex(mutant: *mut KMutant, _level: u32) {
    unsafe { (mutant as *mut Kmutant).write(Kmutant::new()) };
}

unsafe extern "win64" fn k_ke_release_mutex(mutant: *mut KMutant, _wait: u8) -> i32 {
    unsafe { (*(mutant as *mut Kmutant)).release().unwrap_or(0) }
}

unsafe extern "win64" fn k_ke_initialize_spin_lock(lock: *mut KSpinLock) {
    unsafe { lock.write(0) };
}

unsafe extern "win64" fn k_ke_acquire_spin_lock(lock: *mut KSpinLock, old_irql: *mut Kirql) {
    unsafe {
        let prev = spinlock::ke_acquire_spin_lock_raw(lock);
        if !old_irql.is_null() {
            *old_irql = prev;
        }
    }
}

unsafe extern "win64" fn k_ke_release_spin_lock(lock: *mut KSpinLock, new_irql: Kirql) {
    unsafe { spinlock::ke_release_spin_lock_raw(lock, new_irql) };
}

unsafe extern "win64" fn k_ke_get_current_irql() -> Kirql {
    irql::ke_get_current_irql()
}

unsafe extern "win64" fn k_ke_raise_irql(new_irql: Kirql, old_irql: *mut Kirql) {
    let prev = irql::ke_raise_irql(new_irql);
    unsafe {
        if !old_irql.is_null() {
            *old_irql = prev;
        }
    }
}

unsafe extern "win64" fn k_ke_lower_irql(new_irql: Kirql) {
    irql::ke_lower_irql(new_irql);
}

unsafe extern "win64" fn k_ke_query_tick_count(out: *mut u64) {
    unsafe {
        if !out.is_null() {
            *out = scheduler::ke_query_tick_count();
        }
    }
}

unsafe extern "win64" fn k_ke_delay_execution_thread(
    _wait_mode: u8,
    _alertable: u8,
    interval: *const i64,
) -> Ntstatus {
    let ticks = unsafe { timeout_to_ticks(interval) }.unwrap_or(1);
    io::to_abi(scheduler::ki_delay_thread(ticks))
}

/// `KeStallExecutionProcessor` — busy-wait roughly `microseconds`. Spin
/// count is an approximation (no calibrated TSC loop yet); documented.
unsafe extern "win64" fn k_ke_stall_execution_processor(microseconds: u32) {
    // ~ a few hundred pause-iterations per microsecond as a rough stand-in.
    let iters = (microseconds as u64).saturating_mul(300);
    for _ in 0..iters {
        core::hint::spin_loop();
    }
}

/// `ExAllocatePool2(Flags, NumberOfBytes, Tag)` — modern allocator. Flags
/// (pool type / zeroing) are accepted; pool is always non-paged and zeroing
/// is provided by the large path / left to the caller otherwise.
unsafe extern "win64" fn k_ex_allocate_pool2(_flags: u64, size: usize, tag: u32) -> *mut u8 {
    pool::pool_alloc(size, tag)
}

unsafe extern "win64" fn k_ex_free_pool(p: *mut u8) {
    pool::pool_free_any(p)
}

unsafe extern "win64" fn k_rtl_zero_memory(dst: *mut u8, len: usize) {
    unsafe { core::ptr::write_bytes(dst, 0, len) };
}

unsafe extern "win64" fn k_rtl_fill_memory(dst: *mut u8, len: usize, fill: u8) {
    unsafe { core::ptr::write_bytes(dst, fill, len) };
}

unsafe extern "win64" fn k_rtl_copy_memory(dst: *mut u8, src: *const u8, len: usize) {
    unsafe { core::ptr::copy_nonoverlapping(src, dst, len) };
}

/// `IoDeleteDevice` — drop the device object's reference (it was created by
/// the object manager via IoCreateDevice).
unsafe extern "win64" fn k_io_delete_device(device: *mut DeviceObject) {
    unsafe { crate::ob::ob_dereference_object(device as *mut u8) };
}

// ---------------------------------------------------------------------------
// Tier 1b: DPCs and timers (driver win64 callbacks)
// ---------------------------------------------------------------------------

unsafe extern "win64" fn k_ke_initialize_dpc(
    dpc: *mut ntabi::KDpc,
    routine: ntabi::KdeferredRoutine,
    context: *mut core::ffi::c_void,
) {
    unsafe { (dpc as *mut Kdpc).write(Kdpc::new_win64(routine, context)) };
}

unsafe extern "win64" fn k_ke_insert_queue_dpc(
    dpc: *mut ntabi::KDpc,
    system_arg1: *mut core::ffi::c_void,
    system_arg2: *mut core::ffi::c_void,
) -> u8 {
    unsafe {
        let kd = dpc as *mut Kdpc;
        (*kd).system_arg1 = system_arg1;
        (*kd).system_arg2 = system_arg2;
        crate::ke::dpc::ke_insert_queue_dpc(kd) as u8
    }
}

unsafe extern "win64" fn k_ke_initialize_timer(timer: *mut ntabi::KTimer) {
    unsafe { (timer as *mut Ktimer).write(Ktimer::new()) };
}

/// `KeSetTimer(Timer, DueTime, Dpc)`. `DueTime` is the NT 100-ns value
/// (negative = relative); we convert to an absolute tick. Returns whether
/// the timer was already armed.
unsafe extern "win64" fn k_ke_set_timer(
    timer: *mut ntabi::KTimer,
    due_time: i64,
    dpc: *mut ntabi::KDpc,
) -> u8 {
    let rel_ticks = (if due_time < 0 { (-due_time) as u64 } else { due_time as u64 }) / 10_000;
    let due_tick = scheduler::ke_query_tick_count() + rel_ticks.max(1);
    unsafe { scheduler::ke_set_timer(timer as *mut Ktimer, due_tick, dpc as *mut Kdpc) as u8 }
}

unsafe extern "win64" fn k_ke_cancel_timer(timer: *mut ntabi::KTimer) -> u8 {
    unsafe { scheduler::ke_cancel_timer(timer as *mut Ktimer) as u8 }
}

// ---------------------------------------------------------------------------
// Tier 2: IRP stack-location API (the WDM I/O path)
// ---------------------------------------------------------------------------

unsafe extern "win64" fn k_io_get_current_stack_location(irp: *mut Irp) -> *mut ntabi::IoStackLocation {
    unsafe { io::io_get_current_stack_location(irp) }
}

unsafe extern "win64" fn k_io_get_next_stack_location(irp: *mut Irp) -> *mut ntabi::IoStackLocation {
    unsafe { io::io_get_next_stack_location(irp) }
}

unsafe extern "win64" fn k_io_call_driver(device: *mut DeviceObject, irp: *mut Irp) -> Ntstatus {
    unsafe { io::to_abi(io::io_call_driver(device, irp)) }
}

unsafe extern "win64" fn k_io_complete_request2(irp: *mut Irp, _priority_boost: i8) {
    unsafe { io::io_complete_request(irp) }
}

/// `IoSetCompletionRoutine` — record a completion routine in the next stack
/// location (simplified: stores routine + context; the completion-routine
/// invocation path is future work, but the API and storage are in place so
/// driver source compiles and runs).
unsafe extern "win64" fn k_io_set_completion_routine(
    irp: *mut Irp,
    routine: *mut core::ffi::c_void,
    context: *mut core::ffi::c_void,
    _invoke_on_success: u8,
    _invoke_on_error: u8,
    _invoke_on_cancel: u8,
) {
    unsafe {
        let next = io::io_get_next_stack_location(irp);
        (*next).completion_routine = routine;
        (*next).context = context;
    }
}

/// `IoSkipCurrentIrpStackLocation` — pass the IRP down unchanged by copying
/// the current location into the next (so the descent in IoCallDriver lands
/// on an identical location).
unsafe extern "win64" fn k_io_skip_current_stack_location(irp: *mut Irp) {
    unsafe {
        let cur = *io::io_get_current_stack_location(irp);
        let next = io::io_get_next_stack_location(irp);
        *next = cur;
    }
}

// ---------------------------------------------------------------------------
// Tier 3: object namespace — symbolic links + device-by-name lookup
// ---------------------------------------------------------------------------

unsafe extern "win64" fn k_io_create_symbolic_link(
    link_name: *mut UnicodeString,
    device_name: *mut UnicodeString,
) -> Ntstatus {
    unsafe {
        if link_name.is_null() || device_name.is_null() {
            return io::to_abi(crate::rtl::NtStatus::INVALID_PARAMETER);
        }
        io::to_abi(io::namespace::create_symbolic_link(&*link_name, &*device_name))
    }
}

unsafe extern "win64" fn k_io_delete_symbolic_link(link_name: *mut UnicodeString) -> Ntstatus {
    unsafe {
        if link_name.is_null() {
            return io::to_abi(crate::rtl::NtStatus::INVALID_PARAMETER);
        }
        io::to_abi(io::namespace::delete_symbolic_link(&*link_name))
    }
}

/// `IoGetDeviceObjectPointer(ObjectName, DesiredAccess, FileObject, DeviceObject)`.
unsafe extern "win64" fn k_io_get_device_object_pointer(
    object_name: *mut UnicodeString,
    _desired_access: u32,
    out_file_object: *mut *mut core::ffi::c_void,
    out_device_object: *mut *mut DeviceObject,
) -> Ntstatus {
    unsafe {
        if object_name.is_null() {
            return io::to_abi(crate::rtl::NtStatus::INVALID_PARAMETER);
        }
        match io::namespace::lookup_device(&*object_name) {
            Ok(dev) => {
                if !out_device_object.is_null() {
                    *out_device_object = dev;
                }
                if !out_file_object.is_null() {
                    *out_file_object = core::ptr::null_mut();
                }
                Ntstatus::SUCCESS
            }
            Err(e) => io::to_abi(e),
        }
    }
}

// ---------------------------------------------------------------------------
// Tier 4: Mm mapping
// ---------------------------------------------------------------------------

/// `MmGetPhysicalAddress(BaseAddress)` — translate a kernel VA to its
/// physical address by walking the page tables. Returns 0 if unmapped.
unsafe extern "win64" fn k_mm_get_physical_address(base: *mut core::ffi::c_void) -> u64 {
    crate::mm::virt::mm_get_physical_address(base as u64)
        .map(|pa| pa.0)
        .unwrap_or(0)
}

/// `MmMapIoSpace(PhysicalAddress, NumberOfBytes, CacheType)` — map a
/// physical range into kernel space. For RAM-backed addresses this returns
/// the physical-memory-window VA (with NX cleared so MMIO callbacks/code can
/// run); device MMIO outside the mapped window is future work (documented).
unsafe extern "win64" fn k_mm_map_io_space(
    physical_address: u64,
    bytes: usize,
    _cache_type: u32,
) -> *mut core::ffi::c_void {
    unsafe {
        let va = crate::mm::phys_to_virt(crate::mm::PhysAddr(physical_address));
        crate::mm::virt::mm_set_executable(va as u64, bytes);
        va as *mut core::ffi::c_void
    }
}

/// `MmUnmapIoSpace(BaseAddress, NumberOfBytes)` — no-op for window-backed
/// mappings (nothing was allocated to release).
unsafe extern "win64" fn k_mm_unmap_io_space(_base: *mut core::ffi::c_void, _bytes: usize) {}

/// `MmPageEntireDriver(AddressWithinSection)` — a driver calls this to mark its
/// whole image pageable (an optimization for rarely-used drivers; null.sys
/// does it from DriverEntry). We never page kernel memory, so this is a no-op
/// that returns the section base it was given.
unsafe extern "win64" fn k_mm_page_entire_driver(
    address_within_section: *mut core::ffi::c_void,
) -> *mut core::ffi::c_void {
    address_within_section
}

// ---------------------------------------------------------------------------
// The export table
// ---------------------------------------------------------------------------

// One macro generates both halves of the table from a single list, so the
// published names and the resolver can never drift apart:
//   * `EXPORT_NAMES` — a const `&[&str]` (used to emit the driver-side
//     import library `ntoskrnl.lib`).
//   * `resolve(name)` — a runtime lookup returning the shim's address.
// The function-pointer→usize cast must be runtime (const eval forbids it),
// which is why this is a `fn` and not a `static` table.
macro_rules! kernel_exports {
    ($($name:literal => $func:expr),* $(,)?) => {
        /// Names of all kernel exports (what `ntoskrnl.exe` would publish).
        /// Consumed by the build that emits the driver import library.
        pub static EXPORT_NAMES: &[&str] = &[$($name),*];

        /// Resolve an imported name to the address of its shim, or `None`
        /// if unknown (the loader then fails with STATUS_PROCEDURE_NOT_FOUND).
        pub fn resolve(name: &str) -> Option<usize> {
            match name {
                // `as *const ()` first: a function item is not directly `as usize`.
                $($name => Some($func as *const () as usize),)*
                _ => None,
            }
        }
    };
}

kernel_exports! {
    "DbgPrint" => k_dbg_print as unsafe extern "win64" fn(*const u8, usize) -> Ntstatus,
    "ExAllocatePoolWithTag"
        => k_ex_allocate_pool_with_tag as unsafe extern "win64" fn(u32, usize, u32) -> *mut u8,
    "ExFreePoolWithTag"
        => k_ex_free_pool_with_tag as unsafe extern "win64" fn(*mut u8, u32),
    "IoCreateDevice" => k_io_create_device
        as unsafe extern "win64" fn(*mut DriverObject, usize, *mut UnicodeString, u32, u32, u8, *mut *mut DeviceObject) -> Ntstatus,
    "IoCompleteRequest"
        => k_io_complete_request as unsafe extern "win64" fn(*mut Irp, i8),
    // The __fastcall variant real drivers actually import (e.g. null.sys); same
    // ABI as our other exports on x64, same behavior.
    "IofCompleteRequest"
        => k_io_complete_request as unsafe extern "win64" fn(*mut Irp, i8),
    "RtlInitUnicodeString"
        => k_rtl_init_unicode_string as unsafe extern "win64" fn(*mut UnicodeString, *const u16),
    "KeBugCheckEx"
        => k_ke_bug_check_ex as unsafe extern "win64" fn(u32, u64, u64, u64, u64) -> !,

    // --- Tier 1: synchronization ---
    "KeInitializeEvent"
        => k_ke_initialize_event as unsafe extern "win64" fn(*mut KEvent, u32, u8),
    "KeSetEvent"
        => k_ke_set_event as unsafe extern "win64" fn(*mut KEvent, i32, u8) -> i32,
    "KeClearEvent" => k_ke_clear_event as unsafe extern "win64" fn(*mut KEvent),
    "KeResetEvent" => k_ke_reset_event as unsafe extern "win64" fn(*mut KEvent) -> i32,
    "KeWaitForSingleObject" => k_ke_wait_for_single_object
        as unsafe extern "win64" fn(*mut core::ffi::c_void, u32, u8, u8, *const i64) -> Ntstatus,
    "KeInitializeSemaphore"
        => k_ke_initialize_semaphore as unsafe extern "win64" fn(*mut KSemaphore, i32, i32),
    "KeReleaseSemaphore"
        => k_ke_release_semaphore as unsafe extern "win64" fn(*mut KSemaphore, i32, i32, u8) -> i32,
    "KeInitializeMutex"
        => k_ke_initialize_mutex as unsafe extern "win64" fn(*mut KMutant, u32),
    "KeReleaseMutex"
        => k_ke_release_mutex as unsafe extern "win64" fn(*mut KMutant, u8) -> i32,
    "KeInitializeSpinLock"
        => k_ke_initialize_spin_lock as unsafe extern "win64" fn(*mut KSpinLock),
    "KeAcquireSpinLock"
        => k_ke_acquire_spin_lock as unsafe extern "win64" fn(*mut KSpinLock, *mut Kirql),
    "KeReleaseSpinLock"
        => k_ke_release_spin_lock as unsafe extern "win64" fn(*mut KSpinLock, Kirql),

    // --- Tier 1: IRQL, time ---
    "KeGetCurrentIrql" => k_ke_get_current_irql as unsafe extern "win64" fn() -> Kirql,
    "KeRaiseIrql" => k_ke_raise_irql as unsafe extern "win64" fn(Kirql, *mut Kirql),
    "KeLowerIrql" => k_ke_lower_irql as unsafe extern "win64" fn(Kirql),
    "KeQueryTickCount" => k_ke_query_tick_count as unsafe extern "win64" fn(*mut u64),
    "KeDelayExecutionThread"
        => k_ke_delay_execution_thread as unsafe extern "win64" fn(u8, u8, *const i64) -> Ntstatus,
    "KeStallExecutionProcessor"
        => k_ke_stall_execution_processor as unsafe extern "win64" fn(u32),

    // --- Tier 1: memory ---
    "ExAllocatePool2"
        => k_ex_allocate_pool2 as unsafe extern "win64" fn(u64, usize, u32) -> *mut u8,
    "ExFreePool" => k_ex_free_pool as unsafe extern "win64" fn(*mut u8),
    "RtlZeroMemory" => k_rtl_zero_memory as unsafe extern "win64" fn(*mut u8, usize),
    "RtlFillMemory" => k_rtl_fill_memory as unsafe extern "win64" fn(*mut u8, usize, u8),
    "RtlCopyMemory" => k_rtl_copy_memory as unsafe extern "win64" fn(*mut u8, *const u8, usize),
    "IoDeleteDevice" => k_io_delete_device as unsafe extern "win64" fn(*mut DeviceObject),

    // --- Tier 1b: DPCs and timers ---
    "KeInitializeDpc" => k_ke_initialize_dpc
        as unsafe extern "win64" fn(*mut ntabi::KDpc, ntabi::KdeferredRoutine, *mut core::ffi::c_void),
    "KeInsertQueueDpc" => k_ke_insert_queue_dpc
        as unsafe extern "win64" fn(*mut ntabi::KDpc, *mut core::ffi::c_void, *mut core::ffi::c_void) -> u8,
    "KeInitializeTimer" => k_ke_initialize_timer as unsafe extern "win64" fn(*mut ntabi::KTimer),
    "KeSetTimer" => k_ke_set_timer
        as unsafe extern "win64" fn(*mut ntabi::KTimer, i64, *mut ntabi::KDpc) -> u8,
    "KeCancelTimer" => k_ke_cancel_timer as unsafe extern "win64" fn(*mut ntabi::KTimer) -> u8,

    // --- Tier 2: IRP stack-location API ---
    "IoGetCurrentIrpStackLocation" => k_io_get_current_stack_location
        as unsafe extern "win64" fn(*mut Irp) -> *mut ntabi::IoStackLocation,
    "IoGetNextIrpStackLocation" => k_io_get_next_stack_location
        as unsafe extern "win64" fn(*mut Irp) -> *mut ntabi::IoStackLocation,
    "IoCallDriver"
        => k_io_call_driver as unsafe extern "win64" fn(*mut DeviceObject, *mut Irp) -> Ntstatus,
    "IofCompleteRequest"
        => k_io_complete_request2 as unsafe extern "win64" fn(*mut Irp, i8),
    "IoSetCompletionRoutine" => k_io_set_completion_routine
        as unsafe extern "win64" fn(*mut Irp, *mut core::ffi::c_void, *mut core::ffi::c_void, u8, u8, u8),
    "IoSkipCurrentIrpStackLocation"
        => k_io_skip_current_stack_location as unsafe extern "win64" fn(*mut Irp),

    // --- Tier 3: object namespace ---
    "IoCreateSymbolicLink" => k_io_create_symbolic_link
        as unsafe extern "win64" fn(*mut UnicodeString, *mut UnicodeString) -> Ntstatus,
    "IoDeleteSymbolicLink"
        => k_io_delete_symbolic_link as unsafe extern "win64" fn(*mut UnicodeString) -> Ntstatus,
    "IoGetDeviceObjectPointer" => k_io_get_device_object_pointer
        as unsafe extern "win64" fn(*mut UnicodeString, u32, *mut *mut core::ffi::c_void, *mut *mut DeviceObject) -> Ntstatus,

    // --- Tier 4: Mm mapping ---
    "MmGetPhysicalAddress"
        => k_mm_get_physical_address as unsafe extern "win64" fn(*mut core::ffi::c_void) -> u64,
    "MmPageEntireDriver" => k_mm_page_entire_driver
        as unsafe extern "win64" fn(*mut core::ffi::c_void) -> *mut core::ffi::c_void,
    "MmMapIoSpace" => k_mm_map_io_space
        as unsafe extern "win64" fn(u64, usize, u32) -> *mut core::ffi::c_void,
    "MmUnmapIoSpace"
        => k_mm_unmap_io_space as unsafe extern "win64" fn(*mut core::ffi::c_void, usize),
}
