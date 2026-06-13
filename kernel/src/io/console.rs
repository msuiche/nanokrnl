//! `\Device\Console` — the console output device.
//!
//! A built-in driver (like [`super::null`]) whose write dispatch routes the
//! IRP's buffer to the debug serial port. This is the device a console
//! application's standard-output handle refers to: `WriteFile` →
//! `NtWriteFile` → an `IRP_MJ_WRITE` to this device → bytes on the wire.
//!
//! NT's real console stack is far more elaborate (conhost/condrv, input
//! records, screen buffers); this is the minimal output path that makes a
//! "print a line and exit" console program work end to end.

use super::{
    io_complete_request, io_create_device, io_create_driver, io_get_current_stack_location,
    namespace, AbiUnicodeString, DeviceObject, DriverObject, Irp, Ntstatus, IRP_MJ_CLOSE,
    IRP_MJ_CREATE, IRP_MJ_READ, IRP_MJ_WRITE,
};
use crate::ke::spinlock::SpinLock;
use crate::rtl::NtStatus;
use crate::w;
use core::sync::atomic::{AtomicU64, Ordering};

/// Total bytes written through the console device — lets the self tests
/// confirm an `IRP_MJ_WRITE` actually reached the device.
static BYTES_WRITTEN: AtomicU64 = AtomicU64::new(0);

/// `BYTES_WRITTEN` snapshot.
pub fn bytes_written() -> u64 {
    BYTES_WRITTEN.load(Ordering::Acquire)
}

/// Diagnostics: count of `IRP_MJ_READ` dispatches and total bytes drained.
static READ_CALLS: AtomicU64 = AtomicU64::new(0);
static READ_BYTES: AtomicU64 = AtomicU64::new(0);
/// `(read calls, read bytes)` snapshot.
pub fn read_stats() -> (u64, u64) {
    (READ_CALLS.load(Ordering::Acquire), READ_BYTES.load(Ordering::Acquire))
}

// ---------------------------------------------------------------------------
// Console input
// ---------------------------------------------------------------------------

/// A small ring buffer of pending input bytes, fed by the serial receiver
/// (and by [`push_input`] for deterministic tests). `IRP_MJ_READ` drains it.
const INPUT_CAP: usize = 256;
struct ConsoleInput {
    buf: [u8; INPUT_CAP],
    head: usize,
    tail: usize,
}
impl ConsoleInput {
    const fn new() -> Self {
        ConsoleInput { buf: [0; INPUT_CAP], head: 0, tail: 0 }
    }
    fn push(&mut self, b: u8) {
        let next = (self.tail + 1) % INPUT_CAP;
        if next != self.head {
            self.buf[self.tail] = b;
            self.tail = next;
        } // else: buffer full, drop (a real TTY would flow-control)
    }
    fn pop(&mut self) -> Option<u8> {
        if self.head == self.tail {
            return None;
        }
        let b = self.buf[self.head];
        self.head = (self.head + 1) % INPUT_CAP;
        Some(b)
    }
}

static INPUT: SpinLock<ConsoleInput> = SpinLock::new(ConsoleInput::new());

/// End-of-input flag. When set, a blocking `IRP_MJ_READ` returns 0 bytes
/// (EOF) once the input buffer drains, instead of waiting for more — the
/// signal a console program needs to stop reading stdin and finish (e.g. a
/// real `sort.exe` reads to EOF, then sorts and writes its output).
static INPUT_EOF: AtomicU64 = AtomicU64::new(0);

/// Mark the console input as ended (or not). With `eof` true, reads return
/// EOF after the buffer empties.
pub fn set_input_eof(eof: bool) {
    INPUT_EOF.store(eof as u64, Ordering::Release);
}

fn input_at_eof() -> bool {
    INPUT_EOF.load(Ordering::Acquire) != 0
}

/// Inject a byte into the console input buffer. Called by the (future)
/// serial-RX interrupt path and by tests to simulate typed input.
pub fn push_input(b: u8) {
    INPUT.lock().push(b);
}

/// Inject a string into the console input buffer (test/bring-up helper).
pub fn push_input_str(s: &[u8]) {
    let mut input = INPUT.lock();
    for &b in s {
        input.push(b);
    }
}

/// Peek the next pending input byte without consuming it. Drains the UART
/// first so freshly-typed bytes are visible. Returns -1 when no input is
/// buffered (so callers can distinguish "a key is ready" from "nothing yet").
/// Used by `PeekConsoleInputW` to report a pending key event — interactive
/// tools (e.g. choice.exe) poll for an input event before issuing the read.
pub fn peek_input_byte() -> i32 {
    feed_from_serial();
    let input = INPUT.lock();
    if input.head == input.tail {
        -1
    } else {
        input.buf[input.head] as i32
    }
}

/// Pending cooked-mode line being edited. Typed bytes accumulate here (with
/// echo + backspace handling) and are committed to [`INPUT`] only on Enter, so
/// a blocking read returns whole lines and the user can edit before pressing
/// Return — the classic canonical-TTY line discipline. Unused in raw mode.
struct LineEdit {
    buf: [u8; INPUT_CAP],
    len: usize,
}
static EDIT: SpinLock<LineEdit> = SpinLock::new(LineEdit { buf: [0; INPUT_CAP], len: 0 });

/// Echo a typed byte back to the console output when `ENABLE_ECHO_INPUT` is set
/// (so the user sees what they type). Only printable ASCII is echoed directly;
/// control bytes are handled by the caller.
fn echo_byte(b: u8) {
    if echo_mode() && (0x20..0x7f).contains(&b) {
        crate::kd_print!("{}", b as char);
    }
}

/// Apply the terminal line discipline to one freshly-received byte.
///
/// * Raw mode (`ENABLE_LINE_INPUT` clear): deliver and echo immediately — each
///   keystroke is its own read (what choice.exe wants).
/// * Cooked mode: accumulate into [`EDIT`], echoing printable chars; handle
///   Backspace/DEL (erase last char, emit `\b \b`) and Ctrl-C (discard line);
///   on Enter, echo CR/LF and commit the whole line plus `\r\n` to [`INPUT`].
fn ingest_serial_byte(b: u8) {
    if !line_mode() {
        INPUT.lock().push(b);
        echo_byte(b);
        return;
    }
    match b {
        b'\r' | b'\n' => {
            if echo_mode() {
                crate::kd_print!("\r\n");
            }
            let mut edit = EDIT.lock();
            let mut input = INPUT.lock();
            for i in 0..edit.len {
                input.push(edit.buf[i]);
            }
            input.push(b'\r');
            input.push(b'\n');
            edit.len = 0;
        }
        0x08 | 0x7f => {
            let mut edit = EDIT.lock();
            if edit.len > 0 {
                edit.len -= 1;
                if echo_mode() {
                    // Move back, overwrite with a space, move back again.
                    crate::kd_print!("\u{8} \u{8}");
                }
            }
        }
        0x03 => {
            EDIT.lock().len = 0;
            if echo_mode() {
                crate::kd_print!("^C\r\n");
            }
        }
        0x1a => {
            // Ctrl-Z: end-of-file for the console input stream (the Windows
            // console EOF key). Commit any text typed before it on this line,
            // then mark EOF so a program reading stdin (e.g. `sort`) stops
            // instead of blocking forever. The flag is consumed by the next
            // read, so the shell that launched the program is unaffected.
            if echo_mode() {
                crate::kd_print!("^Z\r\n");
            }
            let mut edit = EDIT.lock();
            let mut input = INPUT.lock();
            for i in 0..edit.len {
                input.push(edit.buf[i]);
            }
            edit.len = 0;
            drop(input);
            drop(edit);
            set_input_eof(true);
        }
        _ => {
            let mut edit = EDIT.lock();
            let n = edit.len;
            if n < edit.buf.len() {
                edit.buf[n] = b;
                edit.len = n + 1;
                drop(edit);
                echo_byte(b);
            }
        }
    }
}

/// Drain any bytes the UART has received, applying the line discipline. Live
/// typed input flows through here; canned test input uses [`push_input`]
/// directly and bypasses editing/echo (so the deterministic suite is
/// unaffected, and an automated run with no serial input is a no-op).
///
/// We read the UART's 16-byte receive FIFO into a local batch *first* (each
/// read is a cheap port `in`), then run the line discipline — which echoes,
/// and echoing is a slow busy-wait on the transmitter. Echoing inline between
/// reads would leave the FIFO full during each slow echo, so a fast burst
/// (paste, or a quick run of keystrokes) would overflow it and drop the
/// leading bytes. Emptying the FIFO promptly, then echoing, avoids that.
fn feed_from_serial() {
    loop {
        let mut batch = [0u8; 64];
        let mut n = 0;
        while n < batch.len() {
            match crate::hal::serial::try_read_byte() {
                Some(b) => {
                    batch[n] = b;
                    n += 1;
                }
                None => break,
            }
        }
        if n == 0 {
            break;
        }
        for &b in &batch[..n] {
            ingest_serial_byte(b);
        }
    }
}

/// Console input mode (the `GetConsoleMode`/`SetConsoleMode` bits that matter to
/// reads). `ENABLE_LINE_INPUT` (0x2) is the default: a read returns one line at
/// a time (cooked input — what cmd.exe and line-based tools expect). Clearing
/// it gives raw single-keystroke reads (what choice.exe uses).
const ENABLE_LINE_INPUT: u32 = 0x0002;
/// `ENABLE_ECHO_INPUT` (0x4): the console echoes typed characters back to the
/// output as they are received (terminal echo). Set by default with line input.
const ENABLE_ECHO_INPUT: u32 = 0x0004;
static INPUT_MODE: AtomicU64 = AtomicU64::new(0x0007); // PROCESSED|LINE|ECHO

/// Set the console input mode (from `SetConsoleMode`).
pub fn set_input_mode(mode: u32) {
    INPUT_MODE.store(mode as u64, Ordering::Release);
}

fn line_mode() -> bool {
    INPUT_MODE.load(Ordering::Acquire) as u32 & ENABLE_LINE_INPUT != 0
}

fn echo_mode() -> bool {
    INPUT_MODE.load(Ordering::Acquire) as u32 & ENABLE_ECHO_INPUT != 0
}

/// Copy buffered input bytes into `dst` (up to `max`). In line-input mode the
/// copy stops right after the first newline, so each read returns a single
/// line; in raw mode it drains everything available. Returns the count.
fn drain_input(dst: *mut u8, max: usize) -> usize {
    let line = line_mode();
    let mut input = INPUT.lock();
    let mut n = 0;
    while n < max {
        match input.pop() {
            Some(b) => {
                // SAFETY: caller guarantees dst is valid for `max` bytes.
                unsafe { *dst.add(n) = b };
                n += 1;
                if line && b == b'\n' {
                    break; // one line per read in cooked mode
                }
            }
            None => break,
        }
    }
    n
}

static DRIVER_NAME: AbiUnicodeString = AbiUnicodeString::from_units(w!("\\Driver\\Console"));
static DEVICE_NAME: AbiUnicodeString = AbiUnicodeString::from_units(w!("\\Device\\Console"));
static LINK_NAME: AbiUnicodeString = AbiUnicodeString::from_units(w!("\\DosDevices\\CON"));

/// Dispatch: writes go to the serial port; create/close/read succeed
/// trivially (reads return EOF — no console input yet).
unsafe extern "win64" fn console_dispatch(_device: *mut DeviceObject, irp: *mut Irp) -> Ntstatus {
    unsafe {
        let sl = io_get_current_stack_location(irp);
        let info = match (*sl).major_function {
            IRP_MJ_WRITE => {
                let len = (*sl).read_write().length as usize;
                let buf = (*irp).system_buffer;
                if !buf.is_null() && len > 0 {
                    // SMAP: bracket the read of the user buffer.
                    crate::mm::virt::user_access_begin();
                    let bytes = core::slice::from_raw_parts(buf, len);
                    // Console output is text; render UTF-8, falling back to
                    // raw bytes so non-UTF-8 still reaches the port.
                    match core::str::from_utf8(bytes) {
                        Ok(s) => crate::kd_print!("{}", s),
                        Err(_) => {
                            for &b in bytes {
                                crate::kd_print!("{}", b as char);
                            }
                        }
                    }
                    crate::mm::virt::user_access_end();
                    BYTES_WRITTEN.fetch_add(len as u64, Ordering::AcqRel);
                }
                len as u64
            }
            IRP_MJ_READ => {
                READ_CALLS.fetch_add(1, Ordering::AcqRel);
                let len = (*sl).read_write().length as usize;
                let buf = (*irp).system_buffer;
                if buf.is_null() || len == 0 {
                    0
                } else {
                    // Blocking read: poll the serial receiver into the input
                    // buffer, yielding the CPU between polls, until at least
                    // one byte is available, then drain what fits. Tests
                    // pre-inject input so this returns immediately.
                    loop {
                        feed_from_serial();
                        // SMAP: bracket the write into the user buffer only
                        // (not the blocking delay below).
                        crate::mm::virt::user_access_begin();
                        let n = drain_input(buf, len);
                        crate::mm::virt::user_access_end();
                        if n > 0 {
                            READ_BYTES.fetch_add(n as u64, Ordering::AcqRel);
                            break n as u64;
                        }
                        // Empty buffer: report EOF if input has ended, else
                        // block (yield) until more arrives. EOF is one-shot —
                        // consumed here — so a Ctrl-Z that ends one program's
                        // input doesn't also feed end-of-file to the next reader
                        // (e.g. the shell that launched it).
                        if input_at_eof() {
                            set_input_eof(false);
                            break 0;
                        }
                        crate::ke::dispatcher::ke_delay_execution_thread(1);
                    }
                }
            }
            _ => 0,
        };
        (*irp).io_status.status = Ntstatus(NtStatus::SUCCESS.0);
        (*irp).io_status.information = info;
        io_complete_request(irp);
    }
    Ntstatus(NtStatus::SUCCESS.0)
}

unsafe extern "win64" fn driver_entry(
    driver: *mut DriverObject,
    _registry_path: *mut AbiUnicodeString,
) -> Ntstatus {
    unsafe {
        for major in [IRP_MJ_CREATE, IRP_MJ_CLOSE, IRP_MJ_READ, IRP_MJ_WRITE] {
            (*driver).major_function[major as usize] = Some(console_dispatch);
        }
    }
    Ntstatus(NtStatus::SUCCESS.0)
}

/// Load the console driver, create `\Device\Console`, and alias it as
/// `\DosDevices\CON`. Returns the device.
pub fn initialize() -> Result<*mut DeviceObject, NtStatus> {
    let driver = io_create_driver(DRIVER_NAME, driver_entry)?;
    let device = io_create_device(driver, DEVICE_NAME, core::ptr::null_mut())?;
    namespace::create_symbolic_link(&LINK_NAME, &DEVICE_NAME);
    Ok(device)
}
