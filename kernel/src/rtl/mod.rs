//! # Rtl — Run-Time Library
//!
//! The NT run-time library: the freestanding utility layer shared by every
//! executive subsystem. Nothing in here touches hardware or takes locks —
//! it is pure data-structure code, which is why it also builds and
//! unit-tests on the host.
//!
//! Compatibility notes:
//! * [`status::NTSTATUS`] values are bit-for-bit the documented Windows
//!   values (`STATUS_SUCCESS == 0`, `STATUS_ACCESS_VIOLATION == 0xC0000005`, …).
//! * [`list::ListEntry`] has the exact two-pointer layout of `LIST_ENTRY`
//!   and the same insertion/removal semantics (`InsertTailList`,
//!   `RemoveHeadList`, …).
//! * [`string::UnicodeString`] matches `UNICODE_STRING` (`Length`,
//!   `MaximumLength` in *bytes*, UTF-16 buffer).
//! * [`bitmap::RtlBitmap`] mirrors `RTL_BITMAP` and the
//!   `RtlFindClearBitsAndSet` family used by Mm and the handle table.

pub mod bitmap;
pub mod list;
pub mod status;
pub mod string;

pub use status::NtStatus;
