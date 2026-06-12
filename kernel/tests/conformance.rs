//! NT ABI conformance tests.
//!
//! These turn the README's "bit-exact where it's ABI" claim into something
//! CI can falsify. Every constant and layout we assert here is checked
//! against the *published* Windows x64 value (ntstatus.h, ntddk.h, the
//! amd64 `KGDT64_*` definitions, the documented IRQL table). If a refactor
//! ever perturbs one of these, this test fails before the incompatibility
//! reaches a driver.
//!
//! Runs on the host: the constants under test live in un-gated modules
//! precisely so this can execute without an x86_64 target.

use kernel::ke::irql;
use kernel::ke::selectors::*;
use kernel::rtl::list::ListEntry;
use kernel::rtl::status::NtStatus;
use kernel::rtl::string::UnicodeString;
use std::mem::{align_of, offset_of, size_of};

#[test]
fn gdt_selector_layout_matches_nt_amd64() {
    // From NT's amd64 ke.h: the selector values winload and the CPU's
    // syscall/sysret machinery depend on.
    assert_eq!(KGDT64_NULL, 0x00);
    assert_eq!(KGDT64_R0_CODE, 0x10);
    assert_eq!(KGDT64_R0_DATA, 0x18);
    assert_eq!(KGDT64_R3_CMCODE, 0x20);
    assert_eq!(KGDT64_R3_DATA, 0x28);
    assert_eq!(KGDT64_R3_CODE, 0x30);
    assert_eq!(KGDT64_SYS_TSS, 0x40);

    // syscall/sysret constraint: STAR[47:32] selects consecutive
    // kernel-code then kernel-data; STAR[63:48] selects the user triplet
    // CMCODE, DATA, CODE at +0,+8,+16. Verify the ordering holds.
    assert_eq!(KGDT64_R0_DATA, KGDT64_R0_CODE + 8);
    assert_eq!(KGDT64_R3_DATA, KGDT64_R3_CMCODE + 8);
    assert_eq!(KGDT64_R3_CODE, KGDT64_R3_CMCODE + 16);
    assert_eq!(RPL_USER, 3);
}

#[test]
fn list_entry_is_two_pointers() {
    // LIST_ENTRY { Flink; Blink; } — 16 bytes on x64, Flink first.
    assert_eq!(size_of::<ListEntry>(), 16);
    assert_eq!(offset_of!(ListEntry, flink), 0);
    assert_eq!(offset_of!(ListEntry, blink), 8);
}

#[test]
fn unicode_string_layout() {
    // UNICODE_STRING { USHORT Length; USHORT MaximumLength; PWSTR Buffer; }
    assert_eq!(size_of::<UnicodeString>(), 16);
    assert_eq!(align_of::<UnicodeString>(), 8);
    assert_eq!(offset_of!(UnicodeString, length), 0);
    assert_eq!(offset_of!(UnicodeString, maximum_length), 2);
    assert_eq!(offset_of!(UnicodeString, buffer), 8);
}

#[test]
fn ntstatus_values_are_bit_exact() {
    // Spot-check across all four severity classes against ntstatus.h.
    assert_eq!(NtStatus::SUCCESS.0, 0x0000_0000);
    assert_eq!(NtStatus::TIMEOUT.0, 0x0000_0102);
    assert_eq!(NtStatus::PENDING.0, 0x0000_0103);
    assert_eq!(NtStatus::UNSUCCESSFUL.0, 0xC000_0001);
    assert_eq!(NtStatus::ACCESS_VIOLATION.0, 0xC000_0005);
    assert_eq!(NtStatus::INVALID_PARAMETER.0, 0xC000_000D);
    assert_eq!(NtStatus::OBJECT_NAME_COLLISION.0, 0xC000_0035);
    assert_eq!(NtStatus::INSUFFICIENT_RESOURCES.0, 0xC000_009A);
}

#[test]
fn driver_object_dispatch_table_layout() {
    use ntabi::{DriverObject, IRP_MJ_MAXIMUM_FUNCTION, IRP_MJ_READ, IRP_MJ_WRITE};
    // DRIVER_OBJECT starts with the name, then the MajorFunction table that
    // WDM drivers index directly. The table covers the documented major
    // range and an Option<fn> slot is pointer-sized (null-optimized).
    assert_eq!(IRP_MJ_MAXIMUM_FUNCTION, 0x1C);
    assert_eq!(IRP_MJ_READ, 0x03);
    assert_eq!(IRP_MJ_WRITE, 0x04);
    assert_eq!(offset_of!(DriverObject, driver_name), 0);
    assert_eq!(
        size_of::<Option<ntabi::DriverDispatch>>(),
        size_of::<usize>()
    );
}

#[test]
fn io_stack_location_parameters_union_fits_all_arms() {
    use ntabi::{DeviceIoControlParams, IoStackLocation, ReadWriteParams};
    // The Parameters union storage must hold the largest arm. Read/Write is
    // 16 bytes; DeviceIoControl is 24 (with the type-3 buffer pointer).
    assert!(size_of::<ReadWriteParams>() <= 24);
    assert!(size_of::<DeviceIoControlParams>() <= 24);
    // Round-trip both arms through the union accessors.
    let mut sl = IoStackLocation::zeroed();
    sl.set_read_write(ReadWriteParams { length: 0x1234, key: 7, byte_offset: 0xAABB });
    let rw = sl.read_write();
    assert_eq!((rw.length, rw.key, rw.byte_offset), (0x1234, 7, 0xAABB));
    sl.set_device_io_control(DeviceIoControlParams {
        output_buffer_length: 10,
        input_buffer_length: 20,
        io_control_code: 0x222000,
        _type3_input_buffer: core::ptr::null_mut(),
    });
    assert_eq!(sl.device_io_control().io_control_code, 0x222000);
}

#[test]
fn irql_levels_and_clock_vector_mapping() {
    // Documented x64 IRQL table.
    assert_eq!(irql::PASSIVE_LEVEL, 0);
    assert_eq!(irql::APC_LEVEL, 1);
    assert_eq!(irql::DISPATCH_LEVEL, 2);
    assert_eq!(irql::CLOCK_LEVEL, 13);
    assert_eq!(irql::IPI_LEVEL, 14);
    assert_eq!(irql::HIGH_LEVEL, 15);

    // The architectural rule that makes IRQL == CR8 work: an interrupt on
    // vector V runs at IRQL (V >> 4). NT's clock is vector 0xD1, which must
    // therefore land exactly at CLOCK_LEVEL.
    const CLOCK_VECTOR: u8 = 0xD1;
    assert_eq!((CLOCK_VECTOR >> 4) as u8, irql::CLOCK_LEVEL);
}
