//! `UNICODE_STRING` — NT's counted UTF-16 string.
//!
//! Kernel strings are *not* NUL-terminated C strings: they are counted
//! buffers of UTF-16 code units. `Length`/`MaximumLength` are in **bytes**
//! (a classic source of off-by-2 bugs in C drivers; the accessors here keep
//! the byte/char distinction explicit).
//!
//! ```text
//! typedef struct _UNICODE_STRING {
//!     USHORT Length;        // bytes in use (not chars, no terminator)
//!     USHORT MaximumLength; // bytes allocated
//!     PWSTR  Buffer;
//! } UNICODE_STRING;
//! ```

use core::fmt::{self, Write as _};
use core::slice;

/// Bit-compatible with `UNICODE_STRING` (8/16-byte layout on x64 with the
/// compiler inserting the same 4-byte pad before `buffer`).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct UnicodeString {
    /// Bytes currently in use (always even; UTF-16 code units * 2).
    pub length: u16,
    /// Bytes allocated in `buffer`.
    pub maximum_length: u16,
    /// UTF-16LE code units. May be null for an empty string.
    pub buffer: *mut u16,
}

unsafe impl Send for UnicodeString {}
unsafe impl Sync for UnicodeString {}

impl UnicodeString {
    /// An empty string (`RtlInitEmptyUnicodeString` with no buffer).
    pub const fn empty() -> Self {
        UnicodeString {
            length: 0,
            maximum_length: 0,
            buffer: core::ptr::null_mut(),
        }
    }

    /// `RtlInitUnicodeString` — wrap an existing UTF-16 buffer without
    /// copying. The kernel uses static UTF-16 literals for object names;
    /// see the [`w!`](crate::w) macro which builds those at compile time.
    ///
    /// # Safety
    /// `units` must outlive the returned string (in practice: `'static`
    /// literals or pool allocations owned by the same object).
    pub const fn from_units(units: &'static [u16]) -> Self {
        UnicodeString {
            length: (units.len() * 2) as u16,
            maximum_length: (units.len() * 2) as u16,
            buffer: units.as_ptr() as *mut u16,
        }
    }

    /// Number of UTF-16 code units (i.e. "characters" in Win32 parlance).
    pub fn char_len(&self) -> usize {
        (self.length / 2) as usize
    }

    /// View the in-use portion as a UTF-16 slice.
    pub fn as_units(&self) -> &[u16] {
        if self.buffer.is_null() || self.length == 0 {
            return &[];
        }
        // SAFETY: construction guarantees buffer covers `length` bytes.
        unsafe { slice::from_raw_parts(self.buffer, self.char_len()) }
    }

    /// `RtlEqualUnicodeString(.., CaseInSensitive=TRUE)` for the ASCII
    /// subset — sufficient for object-manager name lookups where all names
    /// the kernel itself creates are ASCII.
    pub fn eq_ignore_ascii_case(&self, other: &UnicodeString) -> bool {
        let (a, b) = (self.as_units(), other.as_units());
        a.len() == b.len()
            && a.iter().zip(b).all(|(&x, &y)| {
                let lx = if (b'A' as u16..=b'Z' as u16).contains(&x) { x + 32 } else { x };
                let ly = if (b'A' as u16..=b'Z' as u16).contains(&y) { y + 32 } else { y };
                lx == ly
            })
    }
}

/// Render lossily for KdPrint: surrogate pairs outside the BMP are printed
/// as U+FFFD, which is fine for diagnostics.
impl fmt::Display for UnicodeString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for c in char::decode_utf16(self.as_units().iter().copied()) {
            f.write_char(c.unwrap_or(char::REPLACEMENT_CHARACTER))?;
        }
        Ok(())
    }
}

impl fmt::Debug for UnicodeString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "u\"{}\"", self)
    }
}

/// Compile-time UTF-16 literal, mirroring the `L"..."` C idiom:
///
/// ```ignore
/// static NAME: UnicodeString = UnicodeString::from_units(w!("\\Device\\Null"));
/// ```
///
/// Only ASCII input is accepted at compile time (kernel-created names are
/// ASCII); non-ASCII panics the const evaluation, i.e. fails the build.
#[macro_export]
macro_rules! w {
    ($s:literal) => {{
        const S: &str = $s;
        const N: usize = S.len();
        const UNITS: [u16; N] = {
            let bytes = S.as_bytes();
            let mut out = [0u16; N];
            let mut i = 0;
            while i < N {
                assert!(bytes[i].is_ascii(), "w! only supports ASCII literals");
                out[i] = bytes[i] as u16;
                i += 1;
            }
            out
        };
        &UNITS
    }};
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn w_macro_builds_utf16_and_lengths_are_bytes() {
        static S: UnicodeString = UnicodeString::from_units(w!("\\Device\\Null"));
        assert_eq!(S.char_len(), 12);
        assert_eq!(S.length, 24); // bytes, not chars
        assert_eq!(format!("{}", S), "\\Device\\Null");
    }

    #[test]
    fn case_insensitive_compare() {
        static A: UnicodeString = UnicodeString::from_units(w!("\\Device\\NULL"));
        static B: UnicodeString = UnicodeString::from_units(w!("\\device\\null"));
        static C: UnicodeString = UnicodeString::from_units(w!("\\device\\nul"));
        assert!(A.eq_ignore_ascii_case(&B));
        assert!(!A.eq_ignore_ascii_case(&C));
    }

    #[test]
    fn layout_matches_unicode_string() {
        // USHORT, USHORT, pad, PWSTR == 16 bytes on x64-style targets.
        assert_eq!(core::mem::offset_of!(UnicodeString, length), 0);
        assert_eq!(core::mem::offset_of!(UnicodeString, maximum_length), 2);
        assert_eq!(core::mem::offset_of!(UnicodeString, buffer), 8);
        assert_eq!(core::mem::size_of::<UnicodeString>(), 16);
    }
}
