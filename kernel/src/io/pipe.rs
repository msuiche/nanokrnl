//! Anonymous pipes (`CreatePipe`) - an in-memory byte stream with a read end
//! and a write end, used by cmd for `dir | sort` and friends.
//!
//! A pipe is a growable buffer with a read cursor and a count of open write
//! ends. Both ends are object-manager objects sharing one [`PipeBuffer`]
//! (leaked to `'static`; pipes are short-lived per command, acceptable for the
//! demo). Closing the write end runs a delete procedure that drops the writer
//! count; a read of an empty pipe blocks until data arrives or the last writer
//! closes (end of file). The buffer is unbounded, so a producer never blocks -
//! it writes everything and exits, then the consumer drains it and sees EOF.

use crate::ke::spinlock::SpinLock;
use crate::ob;
use crate::rtl::string::UnicodeString;
use crate::w;
use alloc::boxed::Box;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicUsize, Ordering};

struct PipeData {
    buf: Vec<u8>,
    pos: usize,
}

/// Shared state behind both ends of a pipe.
pub struct PipeBuffer {
    data: SpinLock<PipeData>,
    writers: AtomicUsize,
}

/// One end of a pipe (read or write); the body a handle stores.
#[repr(C)]
pub struct PipeEnd {
    buf: *const PipeBuffer,
}
// SAFETY: the buffer is `'static` and internally synchronized.
unsafe impl Send for PipeEnd {}
unsafe impl Sync for PipeEnd {}

pub static PIPE_READ_TYPE: ob::ObjectType = ob::ObjectType {
    name: UnicodeString::from_units(w!("PipeRead")),
    delete: None,
};
pub static PIPE_WRITE_TYPE: ob::ObjectType = ob::ObjectType {
    name: UnicodeString::from_units(w!("PipeWrite")),
    delete: Some(write_end_deleted),
};

/// Closing (final deref of) a write end drops the writer count; when it reaches
/// zero, a subsequent read of an empty pipe returns EOF.
fn write_end_deleted(body: *mut u8) {
    let end = body as *mut PipeEnd;
    unsafe { (*(*end).buf).writers.fetch_sub(1, Ordering::AcqRel) };
}

/// `CreatePipe`: allocate a pipe and return `(read_end, write_end)` objects.
pub fn create() -> Option<(*mut PipeEnd, *mut PipeEnd)> {
    let buf: &'static PipeBuffer = Box::leak(Box::new(PipeBuffer {
        data: SpinLock::new(PipeData { buf: Vec::new(), pos: 0 }),
        writers: AtomicUsize::new(1),
    }));
    let r = ob::ob_create_object(&PIPE_READ_TYPE, PipeEnd { buf }).ok()?;
    let w = ob::ob_create_object(&PIPE_WRITE_TYPE, PipeEnd { buf }).ok()?;
    Some((r, w))
}

/// Whether `body` is a pipe read end.
///
/// # Safety
/// `body` must be a live object-manager object.
pub unsafe fn is_read_end(body: *mut u8) -> bool {
    unsafe { ob::ob_check_type(body, &PIPE_READ_TYPE).is_ok() }
}
/// Whether `body` is a pipe write end.
///
/// # Safety
/// `body` must be a live object-manager object.
pub unsafe fn is_write_end(body: *mut u8) -> bool {
    unsafe { ob::ob_check_type(body, &PIPE_WRITE_TYPE).is_ok() }
}

/// Append `len` bytes from `src` to the pipe. Returns the count written.
///
/// # Safety
/// `end` is a live write end; `src` valid for `len` bytes.
pub unsafe fn write(end: *mut PipeEnd, src: *const u8, len: usize) -> usize {
    let pb = unsafe { &*(*end).buf };
    let mut d = pb.data.lock();
    for i in 0..len {
        d.buf.push(unsafe { *src.add(i) });
    }
    len
}

/// Try to read up to `max` bytes into `dst`. Returns `(bytes, eof)`: `bytes` is
/// what was drained (0 if the pipe is momentarily empty), and `eof` is true only
/// when the pipe is empty and no writers remain.
///
/// # Safety
/// `end` is a live read end; `dst` valid for `max` bytes.
pub unsafe fn try_read(end: *mut PipeEnd, dst: *mut u8, max: usize) -> (usize, bool) {
    let pb = unsafe { &*(*end).buf };
    let mut d = pb.data.lock();
    let avail = d.buf.len() - d.pos;
    if avail == 0 {
        return (0, pb.writers.load(Ordering::Acquire) == 0);
    }
    let n = avail.min(max);
    for i in 0..n {
        unsafe { *dst.add(i) = d.buf[d.pos + i] };
    }
    d.pos += n;
    (n, false)
}
