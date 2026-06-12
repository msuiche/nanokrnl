//! `NTSTATUS` — the kernel-wide result type.
//!
//! NT does not use errno-style integers or exceptions internally; every
//! routine that can fail returns an `NTSTATUS`, a 32-bit value whose top
//! two bits encode severity:
//!
//! ```text
//!  31 30 29 28 27............16 15..............0
//! +-----+--+--+-----------------+----------------+
//! | Sev |C |R |    Facility     |      Code      |
//! +-----+--+--+-----------------+----------------+
//!   Sev: 00 success, 01 informational, 10 warning, 11 error
//!   C:   customer-defined bit (we set it for our private stop codes)
//! ```
//!
//! The constants below are bit-for-bit identical to the documented Windows
//! values so that status codes observed in logs/debuggers mean the same
//! thing here as on a real system.

/// 32-bit NT status code. `#[repr(transparent)]` so it can cross any future
/// FFI boundary exactly like the C `NTSTATUS` typedef (a `LONG`).
#[repr(transparent)]
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct NtStatus(pub u32);

impl NtStatus {
    // -- Success (severity 00) ------------------------------------------
    pub const SUCCESS: NtStatus = NtStatus(0x0000_0000);
    /// Wait satisfied by object 0 (== SUCCESS; named for readability when
    /// returned from KeWaitForMultipleObjects).
    pub const WAIT_0: NtStatus = NtStatus(0x0000_0000);
    /// A wait timed out before the object was signaled.
    pub const TIMEOUT: NtStatus = NtStatus(0x0000_0102);
    /// Operation queued and will complete later (IRP pipeline).
    pub const PENDING: NtStatus = NtStatus(0x0000_0103);

    // -- Errors (severity 11) -------------------------------------------
    pub const UNSUCCESSFUL: NtStatus = NtStatus(0xC000_0001);
    pub const NOT_IMPLEMENTED: NtStatus = NtStatus(0xC000_0002);
    pub const INVALID_HANDLE: NtStatus = NtStatus(0xC000_0008);
    pub const INVALID_PARAMETER: NtStatus = NtStatus(0xC000_000D);
    pub const NO_SUCH_DEVICE: NtStatus = NtStatus(0xC000_000E);
    pub const ACCESS_VIOLATION: NtStatus = NtStatus(0xC000_0005);
    /// A pointer/buffer was not aligned as the operation requires
    /// (returned by `ProbeForRead`/`ProbeForWrite`).
    pub const DATATYPE_MISALIGNMENT: NtStatus = NtStatus(0x8000_0002);
    pub const ACCESS_DENIED: NtStatus = NtStatus(0xC000_0022);
    pub const BUFFER_TOO_SMALL: NtStatus = NtStatus(0xC000_0023);
    pub const OBJECT_TYPE_MISMATCH: NtStatus = NtStatus(0xC000_0024);
    pub const OBJECT_NAME_NOT_FOUND: NtStatus = NtStatus(0xC000_0034);
    pub const OBJECT_NAME_COLLISION: NtStatus = NtStatus(0xC000_0035);
    pub const INSUFFICIENT_RESOURCES: NtStatus = NtStatus(0xC000_009A);
    pub const DEVICE_NOT_READY: NtStatus = NtStatus(0xC000_00A3);
    pub const INVALID_DEVICE_REQUEST: NtStatus = NtStatus(0xC000_0010);

    /// `NT_SUCCESS()` from ntdef.h: success or informational severity.
    #[inline]
    pub const fn is_success(self) -> bool {
        (self.0 as i32) >= 0
    }

    /// `NT_ERROR()`: severity == 11.
    #[inline]
    pub const fn is_error(self) -> bool {
        (self.0 >> 30) == 0b11
    }
}

impl core::fmt::Debug for NtStatus {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        // Print the canonical hex form used by WinDbg (`!error`-style).
        write!(f, "NTSTATUS({:#010X})", self.0)
    }
}

/// Idiomatic-Rust bridge: kernel-internal routines may return
/// `Result<T, NtStatus>` and convert at API boundaries with `into()`.
impl From<Result<(), NtStatus>> for NtStatus {
    fn from(r: Result<(), NtStatus>) -> Self {
        match r {
            Ok(()) => NtStatus::SUCCESS,
            Err(s) => s,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn severity_bits_match_windows_semantics() {
        assert!(NtStatus::SUCCESS.is_success());
        assert!(NtStatus::PENDING.is_success()); // informational
        assert!(NtStatus::TIMEOUT.is_success()); // informational
        assert!(!NtStatus::ACCESS_VIOLATION.is_success());
        assert!(NtStatus::ACCESS_VIOLATION.is_error());
        assert!(!NtStatus::SUCCESS.is_error());
    }

    #[test]
    fn well_known_values_are_bit_exact() {
        // Spot-check against ntstatus.h documented values.
        assert_eq!(NtStatus::ACCESS_VIOLATION.0, 0xC0000005);
        assert_eq!(NtStatus::INVALID_PARAMETER.0, 0xC000000D);
        assert_eq!(NtStatus::INSUFFICIENT_RESOURCES.0, 0xC000009A);
    }
}
