//! System services — the `Nt*` routines reachable from user mode.
//!
//! These sit behind the system-service dispatch table ([`crate::ke::syscall`]).
//! Each has the uniform `(u64, u64, u64, u64) -> u64` service signature; the
//! return value is an NTSTATUS (or, for create-style calls, a handle in the
//! caller's RAX). They translate the raw register arguments into kernel
//! operations: handle-table lookups, IRPs to devices, thread termination.
//!
//! The service-number assignments are our own (a real system shares
//! `ntdll`'s numbers); the user side and this table just have to agree.
//!
//! User buffers are validated before use: SMAP is enabled (kernel accesses
//! to user pages are bracketed with `stac`/`clac`) and every service that
//! takes a user pointer runs it through `mm::virt::probe_for_read`/`write`
//! first, so a bogus or kernel pointer yields `STATUS_ACCESS_VIOLATION`
//! instead of faulting the kernel.

use crate::io::{self, DeviceObject, IRP_MJ_WRITE};
use crate::ke::scheduler;
use crate::ke::syscall::register_service;
use crate::ob::handle;
use crate::rtl::NtStatus;
use ntabi::UnicodeString;

/// Service numbers (must match the user-side stubs / future ntdll).
pub const SVC_EXIT_THREAD: usize = 0;
pub const SVC_DEBUG_WRITE: usize = 1;
pub const SVC_NT_WRITE_FILE: usize = 2;
pub const SVC_NT_CREATE_FILE: usize = 3;
pub const SVC_NT_CLOSE: usize = 4;
pub const SVC_NT_READ_FILE: usize = 5;
pub const SVC_NT_ALLOCATE_VIRTUAL_MEMORY: usize = 6;
pub const SVC_NT_FREE_VIRTUAL_MEMORY: usize = 7;
pub const SVC_NT_PROTECT_VIRTUAL_MEMORY: usize = 8;
pub const SVC_REPORT_TEST_RESULT: usize = 9;
pub const SVC_NT_DELAY_EXECUTION: usize = 10;
pub const SVC_NT_QUERY_TICK_COUNT: usize = 11;
pub const SVC_INCREMENT_COUNTER: usize = 12;
pub const SVC_GET_MODULE_HANDLE: usize = 13;
pub const SVC_GET_PROC_ADDRESS: usize = 14;
pub const SVC_SET_LAST_ERROR: usize = 15;
pub const SVC_GET_LAST_ERROR: usize = 16;
pub const SVC_QUERY_FILE_SIZE: usize = 17;
pub const SVC_LOAD_MUI_STRING: usize = 18;
pub const SVC_GET_COMMAND_LINE: usize = 19;
pub const SVC_PEEK_CONSOLE_INPUT: usize = 20;
pub const SVC_QUERY_HANDLE: usize = 21;
pub const SVC_REG_OPEN_KEY: usize = 22;
pub const SVC_REG_CREATE_KEY: usize = 23;
pub const SVC_REG_QUERY_VALUE: usize = 24;
pub const SVC_REG_SET_VALUE: usize = 25;
pub const SVC_REG_ENUM_KEY: usize = 26;
pub const SVC_CREATE_PROCESS: usize = 27;
pub const SVC_WAIT_PROCESS: usize = 28;
pub const SVC_GET_EXIT_CODE_PROCESS: usize = 29;
pub const SVC_SET_CONSOLE_MODE: usize = 30;
pub const SVC_LOAD_MESSAGE: usize = 31;
pub const SVC_QUERY_ATTRIBUTES: usize = 32;
pub const SVC_QUERY_DIRECTORY: usize = 33;
pub const SVC_NT_OPEN_FILE: usize = 34;
/// Deliberately bugcheck the machine (demo/`crash.exe`, akin to the test hook
/// behind Windows' manually-initiated crash). Never returns.
pub const SVC_BUGCHECK: usize = 35;
/// `CreatePipe` - allocate an anonymous pipe; returns two handles.
pub const SVC_CREATE_PIPE: usize = 36;
/// `GetStdHandle` - the current process's standard handle, or 0 (use console).
pub const SVC_GET_STD_HANDLE: usize = 37;
/// Stage the standard handles for the next child (from `STARTUPINFO`).
pub const SVC_SET_STARTUP_HANDLES: usize = 38;
/// `SetStdHandle` - redirect one of the calling process's standard handles
/// (cmd points its own stdout at a pipe/file while running a builtin).
pub const SVC_SET_STD_HANDLE: usize = 39;

/// A shared kernel counter incremented atomically by [`SVC_INCREMENT_COUNTER`].
/// Used to prove concurrent ring-3 threads both make progress through the
/// syscall path (each increments it N times; the total must be N×threads).
static SHARED_COUNTER: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// Read the shared counter.
pub fn counter_value() -> u64 {
    SHARED_COUNTER.load(core::sync::atomic::Ordering::Acquire)
}

/// Sanity ceiling on a single buffered I/O length — a buggy or hostile user
/// program passing an absurd length gets `STATUS_INVALID_PARAMETER` instead
/// of making the kernel walk gigabytes of (possibly unmapped) memory.
const MAX_IO_LEN: usize = 16 * 1024 * 1024;

/// Last value a user program reported via [`SVC_REPORT_TEST_RESULT`]. A
/// robust verification channel for user-mode self-tests: the program reports
/// a code and the kernel asserts it, instead of inferring success from
/// console byte counts.
static TEST_RESULT: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// Read the last reported test result.
pub fn test_result() -> u64 {
    TEST_RESULT.load(core::sync::atomic::Ordering::Acquire)
}

/// Install all system services into the SSDT. Phase-1, single-threaded.
pub fn register_all() {
    register_service(SVC_EXIT_THREAD, nt_terminate_thread);
    register_service(SVC_DEBUG_WRITE, dbg_write_string);
    register_service(SVC_NT_WRITE_FILE, nt_write_file);
    register_service(SVC_NT_CREATE_FILE, nt_create_file);
    register_service(SVC_NT_OPEN_FILE, nt_open_file);
    register_service(SVC_NT_CLOSE, nt_close);
    register_service(SVC_NT_READ_FILE, nt_read_file);
    register_service(SVC_NT_ALLOCATE_VIRTUAL_MEMORY, nt_allocate_virtual_memory);
    register_service(SVC_NT_FREE_VIRTUAL_MEMORY, nt_free_virtual_memory);
    register_service(SVC_NT_PROTECT_VIRTUAL_MEMORY, nt_protect_virtual_memory);
    register_service(SVC_REPORT_TEST_RESULT, report_test_result);
    register_service(SVC_NT_DELAY_EXECUTION, nt_delay_execution);
    register_service(SVC_NT_QUERY_TICK_COUNT, nt_query_tick_count);
    register_service(SVC_INCREMENT_COUNTER, increment_counter);
    register_service(SVC_GET_MODULE_HANDLE, nt_get_module_handle);
    register_service(SVC_GET_PROC_ADDRESS, nt_get_proc_address);
    register_service(SVC_SET_LAST_ERROR, set_last_error);
    register_service(SVC_GET_LAST_ERROR, get_last_error);
    register_service(SVC_QUERY_FILE_SIZE, nt_query_file_size);
    register_service(SVC_LOAD_MUI_STRING, nt_load_mui_string);
    register_service(SVC_GET_COMMAND_LINE, nt_get_command_line);
    register_service(SVC_PEEK_CONSOLE_INPUT, nt_peek_console_input);
    register_service(SVC_QUERY_HANDLE, nt_query_handle);
    register_service(SVC_REG_OPEN_KEY, nt_reg_open_key);
    register_service(SVC_REG_CREATE_KEY, nt_reg_create_key);
    register_service(SVC_REG_QUERY_VALUE, nt_reg_query_value);
    register_service(SVC_REG_SET_VALUE, nt_reg_set_value);
    register_service(SVC_REG_ENUM_KEY, nt_reg_enum_key);
    register_service(SVC_CREATE_PROCESS, nt_create_process);
    register_service(SVC_WAIT_PROCESS, nt_wait_process);
    register_service(SVC_GET_EXIT_CODE_PROCESS, nt_get_exit_code_process);
    register_service(SVC_SET_CONSOLE_MODE, nt_set_console_mode);
    register_service(SVC_LOAD_MESSAGE, nt_load_message);
    register_service(SVC_QUERY_ATTRIBUTES, nt_query_attributes);
    register_service(SVC_QUERY_DIRECTORY, nt_query_directory);
    register_service(SVC_BUGCHECK, nt_bugcheck);
    register_service(SVC_CREATE_PIPE, nt_create_pipe);
    register_service(SVC_GET_STD_HANDLE, nt_get_std_handle);
    register_service(SVC_SET_STARTUP_HANDLES, nt_set_startup_handles);
    register_service(SVC_SET_STD_HANDLE, nt_set_std_handle);
}

/// `SetStdHandle(which, handle)` - redirect the calling thread's standard handle
/// (0 = stdin, 1 = stdout, 2 = stderr). cmd uses this to point its own stdout at
/// a pipe while running a builtin like `dir` on the left of `dir | sort`.
extern "C" fn nt_set_std_handle(which: u64, new_handle: u64, _a3: u64, _a4: u64) -> u64 {
    let i = which as usize;
    if i > 2 {
        return 0;
    }
    let t = crate::ke::pcr::ke_get_current_thread();
    if !t.is_null() {
        unsafe { (*t).std_handles[i] = new_handle };
    }
    1
}

/// `CreatePipe` - create an anonymous pipe and write its two handles (read then
/// write, each u64) to the user buffer at `out_ptr`. Returns 1 on success.
extern "C" fn nt_create_pipe(out_ptr: u64, _a2: u64, _a3: u64, _a4: u64) -> u64 {
    if out_ptr == 0 || crate::mm::virt::probe_for_write(out_ptr, 16, 8).is_err() {
        return 0;
    }
    let Some((r, w)) = io::pipe::create() else {
        return 0;
    };
    let rh = handle::ob_create_handle(r as *mut u8, 0);
    let wh = handle::ob_create_handle(w as *mut u8, 0);
    crate::mm::virt::user_access_begin();
    unsafe {
        *(out_ptr as *mut u64) = rh;
        *((out_ptr + 8) as *mut u64) = wh;
    }
    crate::mm::virt::user_access_end();
    1
}

/// `GetStdHandle(which)` - return the calling thread's standard handle
/// (0 = stdin, 1 = stdout, 2 = stderr), or 0 if none (caller uses the console).
extern "C" fn nt_get_std_handle(which: u64, _a2: u64, _a3: u64, _a4: u64) -> u64 {
    let i = which as usize;
    if i > 2 {
        return 0;
    }
    let t = crate::ke::pcr::ke_get_current_thread();
    if t.is_null() {
        return 0;
    }
    unsafe { (*t).std_handles[i] }
}

/// Stage the standard handles (stdin, stdout, stderr) for the next child this
/// thread creates. From `STARTUPINFO` when `STARTF_USESTDHANDLES` is set.
extern "C" fn nt_set_startup_handles(h_in: u64, h_out: u64, h_err: u64, _a4: u64) -> u64 {
    let t = crate::ke::pcr::ke_get_current_thread();
    if !t.is_null() {
        unsafe { (*t).child_std_handles = [h_in, h_out, h_err] };
    }
    0
}

/// Service for `crash.exe`: deliberately bugcheck the machine. The optional
/// stop code arrives in arg1 (r10); 0 defaults to `MANUALLY_INITIATED_CRASH`,
/// the code Windows uses for a user-forced crash. Never returns — the machine
/// prints the stop banner and halts.
extern "C" fn nt_bugcheck(code: u64, _a2: u64, _a3: u64, _a4: u64) -> u64 {
    let stop = if code == 0 {
        crate::ke::bugcheck::MANUALLY_INITIATED_CRASH
    } else {
        code as u32
    };
    // Author a crash dump to the host (H:\nanokrnl.core) BEFORE the STOP banner,
    // while the machine is still running so the host 9P server can service the
    // write. Best-effort: a no-op if no host server is attached.
    let params = [0u64; 4];
    #[cfg(target_arch = "x86_64")]
    {
        let _ = crate::dump::write_core(stop, &params);
        // Break into an attached kernel debugger (lldb/gdb), like KdBreak on a
        // real bugcheck. A no-op in nanox when nothing is attached.
        unsafe { core::arch::asm!("int3") };
    }
    crate::ke::bugcheck::ke_bug_check_ex(stop, params[0], params[1], params[2], params[3])
}

/// `FindFirstFile`/`FindNextFile` backend (a1 = UTF-16 wildcard pattern ptr,
/// a2 = char count, a3 = zero-based match index, a4 = user `WIN32_FIND_DATAW`
/// buffer). Fills the buffer with the `index`-th matching RAM-fs entry and
/// returns 1; returns 0 when there is no such match (end of enumeration).
extern "C" fn nt_query_directory(pat_ptr: u64, pat_len: u64, index: u64, out_ptr: u64) -> u64 {
    let mut wbuf = [0u16; 260];
    let Some(w) = read_user_u16(pat_ptr, pat_len as usize, &mut wbuf) else {
        return 0;
    };
    let mut abuf = [0u8; 260];
    let mut n = 0;
    for &c in w {
        if n < abuf.len() {
            abuf[n] = c as u8;
            n += 1;
        }
    }
    let Ok(pattern) = core::str::from_utf8(&abuf[..n]) else {
        return 0;
    };
    // The virtual "H:" drive is served by the host 9P server. A wildcard or a
    // bare "H:\" enumerates the host directory (dir); a concrete name is a
    // single-file stat, which ulib tools do via FindFirstFile before opening a
    // file to read it. Both must resolve here or the shell reports "not found".
    let host = pattern.strip_prefix("\\??\\").unwrap_or(pattern);
    if let Some(rest) = strip_host_drive(host) {
        if rest.is_empty() || rest.contains('*') || rest.contains('?') {
            // Directory enumeration: list the host root, filter by the wildcard,
            // and return the index-th match (with its real size, fetched once).
            let Some(names) = io::p9::list() else {
                return 0;
            };
            let Some(name) = names.iter().filter(|n| host_glob(rest, n)).nth(index as usize) else {
                return 0;
            };
            let Some(bytes) = io::p9::read(name.as_str()) else {
                return 0;
            };
            return write_find_data(out_ptr, name.as_str(), 0x80, bytes.len() as u64);
        }
        if index != 0 {
            return 0;
        }
        let Some(bytes) = io::p9::read(rest) else {
            return 0;
        };
        let fname = rest.rsplit(['\\', '/']).next().unwrap_or(rest);
        return write_find_data(out_ptr, fname, 0x80, bytes.len() as u64);
    }
    let Some(entry) = io::ramfs::find(pattern, index as usize) else {
        return 0;
    };
    write_find_data(out_ptr, entry.name, entry.attributes, entry.size)
}

/// Fill a `WIN32_FIND_DATAW` (x64 layout) at `out_ptr` with `name`, `attributes`
/// and `size`, returning 1, or 0 if the user buffer is unwritable. Layout:
/// dwFileAttributes@0, three FILETIMEs@4, sizes@28/32, two reserved DWORDs@36/40,
/// cFileName[260]@44 (UTF-16), cAlternateFileName[14]@564.
fn write_find_data(out_ptr: u64, name: &str, attributes: u32, size: u64) -> u64 {
    const FIND_DATA_SIZE: usize = 592;
    if out_ptr == 0 || crate::mm::virt::probe_for_write(out_ptr, FIND_DATA_SIZE, 2).is_err() {
        return 0;
    }
    crate::mm::virt::user_access_begin();
    unsafe {
        core::ptr::write_bytes(out_ptr as *mut u8, 0, FIND_DATA_SIZE);
        *(out_ptr as *mut u32) = attributes;
        *((out_ptr + 28) as *mut u32) = (size >> 32) as u32;
        *((out_ptr + 32) as *mut u32) = size as u32;
        let name_ptr = out_ptr + 44; // cFileName
        let nb = name.as_bytes();
        let cch = nb.len().min(259);
        for i in 0..cch {
            *((name_ptr + (i as u64) * 2) as *mut u16) = nb[i] as u16;
        }
        *((name_ptr + (cch as u64) * 2) as *mut u16) = 0;
    }
    crate::mm::virt::user_access_end();
    1
}

/// Minimal case-insensitive wildcard match for host directory listing: `*`
/// matches any run (including empty), `?` any one character, everything else
/// literal. An empty pattern (a bare "H:\") matches everything.
fn host_glob(pat: &str, name: &str) -> bool {
    fn glob(pat: &[u8], name: &[u8]) -> bool {
        match pat.split_first() {
            None => name.is_empty(),
            Some((&b'*', rest)) => {
                let mut tail = name;
                loop {
                    if glob(rest, tail) {
                        return true;
                    }
                    match tail.split_first() {
                        Some((_, t)) => tail = t,
                        None => return false,
                    }
                }
            }
            Some((&b'?', rest)) => !name.is_empty() && glob(rest, &name[1..]),
            Some((&p, rest)) => match name.split_first() {
                Some((&c, t)) if p.eq_ignore_ascii_case(&c) => glob(rest, t),
                _ => false,
            },
        }
    }
    pat.is_empty() || glob(pat.as_bytes(), name.as_bytes())
}

/// `GetFileAttributesW` backend (a1 = UTF-16 path ptr, a2 = char count).
/// Returns the Win32 attributes from the RAM filesystem, or
/// `INVALID_FILE_ATTRIBUTES` (0xFFFF_FFFF) if the path doesn't exist.
extern "C" fn nt_query_attributes(path_ptr: u64, path_len: u64, _a3: u64, _a4: u64) -> u64 {
    let mut wbuf = [0u16; 260];
    let Some(w) = read_user_u16(path_ptr, path_len as usize, &mut wbuf) else {
        return 0xFFFF_FFFF;
    };
    let mut abuf = [0u8; 260];
    let mut n = 0;
    for &c in w {
        if n < abuf.len() {
            abuf[n] = c as u8;
            n += 1;
        }
    }
    let Ok(path) = core::str::from_utf8(&abuf[..n]) else {
        return 0xFFFF_FFFF;
    };
    io::ramfs::attributes(path) as u64
}

/// `FormatMessage(FROM_HMODULE)` backend: load message `id` from `base`'s
/// registered `.mui` `RT_MESSAGETABLE` into the user buffer (`buf`/`cch`).
/// Returns the char count (excluding NUL), 0 if not found.
extern "C" fn nt_load_message(base: u64, id: u64, buf: u64, cch: u64) -> u64 {
    let cch = cch as usize;
    if buf == 0 || cch == 0 {
        return 0;
    }
    let mut tmp = [0u16; 512];
    let want = cch.saturating_sub(1).min(tmp.len());
    let cur = crate::ke::pcr::ke_get_current_thread();
    let n = unsafe {
        let (mp, ml) = ((*cur).mui_ptr, (*cur).mui_len as usize);
        if mp != 0 && ml != 0 {
            let bytes = core::slice::from_raw_parts(mp as *const u8, ml);
            crate::ldr::mui::load_message_from(bytes, id as u32, &mut tmp[..want])
        } else {
            crate::ldr::mui::load_message(base, id as u32, &mut tmp[..want])
        }
    };
    if n == 0 {
        return 0;
    }
    if crate::mm::virt::probe_for_write(buf, (n + 1) * 2, 2).is_err() {
        return 0;
    }
    crate::mm::virt::user_access_begin();
    let dst = buf as *mut u16;
    unsafe {
        for i in 0..n {
            *dst.add(i) = tmp[i];
        }
        *dst.add(n) = 0;
    }
    crate::mm::virt::user_access_end();
    n as u64
}

/// `SetConsoleMode` backend (a1 = mode bits). Sets the console input mode; the
/// read path honors `ENABLE_LINE_INPUT` (one line per read vs raw).
extern "C" fn nt_set_console_mode(mode: u64, _a2: u64, _a3: u64, _a4: u64) -> u64 {
    crate::io::console::set_input_mode(mode as u32);
    0
}

/// `NtCreateProcess` backend (a1 = UTF-16 path ptr, a2 = char count). Looks the
/// image up in the RAM filesystem, builds a new process from it, and returns a
/// process handle (0 on failure). This is the kernel side of `CreateProcessW`.
extern "C" fn nt_create_process(path_ptr: u64, path_len: u64, cmd_ptr: u64, cmd_len: u64) -> u64 {
    let mut wbuf = [0u16; 128];
    let Some(w) = read_user_u16(path_ptr, path_len as usize, &mut wbuf) else {
        return 0;
    };
    // Narrow the path to ASCII for the RAM-fs lookup.
    let mut abuf = [0u8; 128];
    let mut n = 0;
    for &c in w {
        if n < abuf.len() {
            abuf[n] = c as u8;
            n += 1;
        }
    }
    let Ok(path) = core::str::from_utf8(&abuf[..n]) else {
        return 0;
    };
    let Some(image) = io::ramfs::lookup(path) else {
        return 0;
    };
    // The child's command line (its argv) comes separately from the image path
    // so launching `where cmd` gives the child "where cmd", not the image path.
    let mut cwbuf = [0u16; 128];
    let mut cbuf = [0u8; 128];
    let mut cn = 0;
    if cmd_ptr != 0 && cmd_len != 0 {
        if let Some(cw) = read_user_u16(cmd_ptr, cmd_len as usize, &mut cwbuf) {
            for &c in cw {
                if cn < cbuf.len() {
                    cbuf[cn] = c as u8;
                    cn += 1;
                }
            }
        }
    }
    let cmdline: &[u8] = if cn > 0 { &cbuf[..cn] } else { &abuf[..n] };
    // Consume any standard handles the parent staged for this child (pipe/file
    // redirection via STARTUPINFO), then clear the staging slot.
    let std = {
        let t = crate::ke::pcr::ke_get_current_thread();
        if t.is_null() {
            [0u64; 3]
        } else {
            unsafe {
                let s = (*t).child_std_handles;
                (*t).child_std_handles = [0; 3];
                s
            }
        }
    };
    crate::init::create_user_process(image, cmdline, std)
}

/// `NtWaitForSingleObject` on a process handle (a2 = timeout ms). Returns the
/// wait NTSTATUS (0 = the process exited).
extern "C" fn nt_wait_process(handle: u64, timeout_ms: u64, _a3: u64, _a4: u64) -> u64 {
    crate::init::wait_user_process(handle, timeout_ms)
}

/// `GetExitCodeProcess` — the exit code the process reported.
extern "C" fn nt_get_exit_code_process(handle: u64, _a2: u64, _a3: u64, _a4: u64) -> u64 {
    crate::init::user_process_exit_code(handle) as u64
}

/// Copy a user UTF-16 string (`count` units) into `dst`; returns the slice.
fn read_user_u16<'a>(ptr: u64, count: usize, dst: &'a mut [u16]) -> Option<&'a [u16]> {
    if ptr == 0 || count == 0 || count > dst.len() {
        return None;
    }
    if crate::mm::virt::probe_for_read(ptr, count * 2, 2).is_err() {
        return None;
    }
    crate::mm::virt::user_access_begin();
    unsafe { core::ptr::copy_nonoverlapping(ptr as *const u16, dst.as_mut_ptr(), count) };
    crate::mm::virt::user_access_end();
    Some(&dst[..count])
}

/// Read `n` u64 fields of a request struct from a user pointer.
fn read_user_req(ptr: u64, out: &mut [u64]) -> bool {
    let bytes = out.len() * 8;
    if ptr == 0 || crate::mm::virt::probe_for_read(ptr, bytes, 8).is_err() {
        return false;
    }
    crate::mm::virt::user_access_begin();
    unsafe { core::ptr::copy_nonoverlapping(ptr as *const u64, out.as_mut_ptr(), out.len()) };
    crate::mm::virt::user_access_end();
    true
}

/// `NtOpenKey` backend (a2 = path UTF-16 ptr, a3 = char count). Returns the key
/// handle, or 0 if the subkey does not exist.
extern "C" fn nt_reg_open_key(parent: u64, path_ptr: u64, path_len: u64, _a4: u64) -> u64 {
    let mut buf = [0u16; 256];
    match read_user_u16(path_ptr, path_len as usize, &mut buf) {
        Some(path) => crate::cm::open_key(parent, path),
        None => crate::cm::open_key(parent, &[]), // empty path == the parent itself
    }
}

/// `NtCreateKey` backend: open or create the subkey path.
extern "C" fn nt_reg_create_key(parent: u64, path_ptr: u64, path_len: u64, _a4: u64) -> u64 {
    let mut buf = [0u16; 256];
    match read_user_u16(path_ptr, path_len as usize, &mut buf) {
        Some(path) => crate::cm::create_key(parent, path),
        None => crate::cm::create_key(parent, &[]),
    }
}

/// `NtQueryValueKey` backend. `a2` points at a request struct
/// `{name_ptr, name_chars, out_type_ptr, out_buf_ptr, out_cap_bytes}` (5 u64).
/// Returns the value's byte length, or `u64::MAX` if it is absent.
extern "C" fn nt_reg_query_value(hkey: u64, req_ptr: u64, _a3: u64, _a4: u64) -> u64 {
    let mut req = [0u64; 5];
    if !read_user_req(req_ptr, &mut req) {
        return u64::MAX;
    }
    let (name_ptr, name_len, out_type_ptr, out_buf, out_cap) =
        (req[0], req[1], req[2], req[3], req[4] as usize);
    let mut nbuf = [0u16; 256];
    let Some(name) = read_user_u16(name_ptr, name_len as usize, &mut nbuf) else {
        return u64::MAX;
    };
    let mut data = [0u8; 256];
    let mut vtype = 0u32;
    let n = crate::cm::query_value(hkey, name, &mut vtype, &mut data);
    if n < 0 {
        return u64::MAX;
    }
    if out_type_ptr != 0 && crate::mm::virt::probe_for_write(out_type_ptr, 4, 4).is_ok() {
        crate::mm::virt::user_access_begin();
        unsafe { *(out_type_ptr as *mut u32) = vtype };
        crate::mm::virt::user_access_end();
    }
    let copy = (n as usize).min(out_cap).min(data.len());
    if out_buf != 0 && copy > 0 && crate::mm::virt::probe_for_write(out_buf, copy, 1).is_ok() {
        crate::mm::virt::user_access_begin();
        unsafe { core::ptr::copy_nonoverlapping(data.as_ptr(), out_buf as *mut u8, copy) };
        crate::mm::virt::user_access_end();
    }
    n as u64
}

/// `NtSetValueKey` backend. `a2` points at a request struct
/// `{name_ptr, name_chars, vtype, data_ptr, data_len_bytes}` (5 u64). Returns 0
/// on success, `u64::MAX` on failure.
extern "C" fn nt_reg_set_value(hkey: u64, req_ptr: u64, _a3: u64, _a4: u64) -> u64 {
    let mut req = [0u64; 5];
    if !read_user_req(req_ptr, &mut req) {
        return u64::MAX;
    }
    let (name_ptr, name_len, vtype, data_ptr, data_len) =
        (req[0], req[1], req[2] as u32, req[3], req[4] as usize);
    let mut nbuf = [0u16; 256];
    let Some(name) = read_user_u16(name_ptr, name_len as usize, &mut nbuf) else {
        return u64::MAX;
    };
    let mut dbuf = [0u8; 256];
    let dl = data_len.min(dbuf.len());
    if dl > 0 {
        if crate::mm::virt::probe_for_read(data_ptr, dl, 1).is_err() {
            return u64::MAX;
        }
        crate::mm::virt::user_access_begin();
        unsafe { core::ptr::copy_nonoverlapping(data_ptr as *const u8, dbuf.as_mut_ptr(), dl) };
        crate::mm::virt::user_access_end();
    }
    if crate::cm::set_value(hkey, name, vtype, &dbuf[..dl]) {
        0
    } else {
        u64::MAX
    }
}

/// `NtEnumerateKey` backend (a2 = index, a3 = out UTF-16 buf, a4 = char cap).
/// Returns the subkey-name length in chars, or `u64::MAX` past the end.
extern "C" fn nt_reg_enum_key(hkey: u64, index: u64, out_buf: u64, out_cap: u64) -> u64 {
    let cap = (out_cap as usize).min(256);
    let mut buf = [0u16; 256];
    let n = crate::cm::enum_key(hkey, index as usize, &mut buf[..cap]);
    if n < 0 {
        return u64::MAX;
    }
    let copy = (n as usize).min(cap);
    if out_buf != 0 && copy > 0 && crate::mm::virt::probe_for_write(out_buf, copy * 2, 2).is_ok() {
        crate::mm::virt::user_access_begin();
        unsafe { core::ptr::copy_nonoverlapping(buf.as_ptr(), out_buf as *mut u16, copy) };
        crate::mm::virt::user_access_end();
    }
    n as u64
}

/// Report whether `handle` resolves to a live object (1) or not (0). Lets a
/// shim re-open a stale cached handle — e.g. `GetStdHandle` after another
/// process closed the shared console handle it had cached.
extern "C" fn nt_query_handle(handle: u64, _a2: u64, _a3: u64, _a4: u64) -> u64 {
    // Look up only — `ob_reference_object_by_handle` does not take a reference,
    // so we must not dereference (that would free a still-open object).
    match handle::ob_reference_object_by_handle(handle) {
        Ok(_) => 1,
        Err(_) => 0,
    }
}

/// Peek the next pending console input byte without consuming it. Returns the
/// byte (0..255) in `rax`, or `u64::MAX` (-1) when nothing is buffered. Lets
/// `PeekConsoleInputW` report whether a key event is ready.
extern "C" fn nt_peek_console_input(_a1: u64, _a2: u64, _a3: u64, _a4: u64) -> u64 {
    crate::io::console::peek_input_byte() as i64 as u64
}

/// Atomically increment the shared counter; return the new value.
extern "C" fn increment_counter(_a1: u64, _a2: u64, _a3: u64, _a4: u64) -> u64 {
    SHARED_COUNTER.fetch_add(1, core::sync::atomic::Ordering::AcqRel) + 1
}

/// `NtDelayExecution`-flavored sleep: `a1` = milliseconds (≈ clock ticks).
extern "C" fn nt_delay_execution(millis: u64, _a2: u64, _a3: u64, _a4: u64) -> u64 {
    crate::ke::scheduler::ki_delay_thread(millis.max(1));
    NtStatus::SUCCESS.0 as u64
}

/// Return `KeTickCount` (≈ milliseconds since boot) in RAX.
extern "C" fn nt_query_tick_count(_a1: u64, _a2: u64, _a3: u64, _a4: u64) -> u64 {
    crate::ke::scheduler::ke_query_tick_count()
}

/// Test-channel service: record `a1` as the program's reported result.
extern "C" fn report_test_result(code: u64, _a2: u64, _a3: u64, _a4: u64) -> u64 {
    TEST_RESULT.store(code, core::sync::atomic::Ordering::Release);
    NtStatus::SUCCESS.0 as u64
}

/// Window base: the virtual address physical page 0 maps to. The user heap
/// lives in the shared high-half physical-memory window (`window_base +
/// physical_address`), so this converts between the two for alloc/free.
///
/// NOTE: a *per-process* heap (mapping each allocation into the calling
/// process's own low-half address space) is blocked on per-process `kernel32`
/// instances — the shared `kernel32` keeps its heap arena (`BUMP`/free-list)
/// in shared `.data`, so a chunk it allocates while one address space is
/// active gets sub-allocated to a different process whose address space never
/// mapped it. Until `kernel32` is loaded per-process, the heap stays in the
/// shared window.
fn window_base() -> u64 {
    crate::mm::phys_to_virt(crate::mm::PhysAddr(0)) as u64
}

/// `NtAllocateVirtualMemory` (simplified): `a1` = byte size. Allocates the
/// rounded-up page count from the physical allocator, maps it
/// user-accessible, and returns the base VA in RAX (0 on failure). A user
/// heap allocator (in a future ntdll/CRT) layers on top of this.
extern "C" fn nt_allocate_virtual_memory(size: u64, _a2: u64, _a3: u64, _a4: u64) -> u64 {
    if size == 0 {
        return 0;
    }
    let pages = (size as usize).div_ceil(crate::mm::PAGE_SIZE);
    match crate::mm::phys::mm_allocate_contiguous_pages(pages) {
        Some(pa) => {
            let va = crate::mm::phys_to_virt(pa) as u64;
            // Make the region user-accessible (and executable — heap W^X is a
            // documented coarsening of the shared-address-space model).
            unsafe { crate::mm::virt::mm_set_user_executable(va, pages * crate::mm::PAGE_SIZE) };
            va
        }
        None => 0,
    }
}

/// `NtFreeVirtualMemory` (simplified): `a1` = base VA, `a2` = byte size.
/// Returns the pages to the physical allocator. Returns STATUS_SUCCESS.
extern "C" fn nt_free_virtual_memory(base: u64, size: u64, _a3: u64, _a4: u64) -> u64 {
    if base == 0 || size == 0 {
        return NtStatus::INVALID_PARAMETER.0 as u64;
    }
    let pages = (size as usize).div_ceil(crate::mm::PAGE_SIZE);
    let phys = base.wrapping_sub(window_base());
    crate::mm::phys::mm_free_contiguous_pages(crate::mm::PhysAddr(phys), pages);
    NtStatus::SUCCESS.0 as u64
}

/// `NtProtectVirtualMemory` (simplified): ensures the range is
/// user-accessible + executable + writable and returns STATUS_SUCCESS.
/// Permissive (it never *removes* access) — enough for a CRT's
/// `VirtualProtect` calls not to fail; real per-page W^X protection is
/// future work in the single-address-space model.
extern "C" fn nt_protect_virtual_memory(base: u64, size: u64, _protect: u64, _a4: u64) -> u64 {
    if base != 0 && size != 0 {
        unsafe { crate::mm::virt::mm_set_user_executable(base, size as usize) };
    }
    NtStatus::SUCCESS.0 as u64
}

/// `NtTerminateThread` (self): end the calling thread. Never returns to user.
extern "C" fn nt_terminate_thread(exit_code: u64, _a2: u64, _a3: u64, _a4: u64) -> u64 {
    // Record the exit code on the current thread before it dies, so a parent's
    // GetExitCodeProcess can read it. (ExitProcess passes the code in a1.)
    unsafe {
        let cur = crate::ke::pcr::ke_get_current_thread();
        (*cur).exit_code = exit_code as u32;
        // Restore the console mode if this is a created process that changed it.
        crate::init::on_user_thread_exit(cur as u64);
        scheduler::ki_terminate_current_thread()
    }
}

/// `DbgPrint`-from-user: `a1` = buffer VA, `a2` = length. Echoes to the debug
/// port. (A convenience service for bring-up; not an NT export.)
extern "C" fn dbg_write_string(a1: u64, a2: u64, _a3: u64, _a4: u64) -> u64 {
    let (ptr, len) = (a1 as *const u8, a2 as usize);
    // Reject a bogus/kernel pointer up front (confused-deputy guard).
    if crate::mm::virt::probe_for_read(a1, len.min(4096), 1).is_err() {
        return NtStatus::ACCESS_VIOLATION.0 as u64;
    }
    if !ptr.is_null() && len < 4096 {
        crate::mm::virt::user_access_begin(); // SMAP: reading a user buffer
        let bytes = unsafe { core::slice::from_raw_parts(ptr, len) };
        if let Ok(s) = core::str::from_utf8(bytes) {
            crate::kd_print!("{}", s);
        }
        crate::mm::virt::user_access_end();
    }
    NtStatus::SUCCESS.0 as u64
}

/// `NtWriteFile(Handle, Buffer, Length)` — resolve the handle to its device
/// and issue a synchronous `IRP_MJ_WRITE`. Simplified argument list (the
/// full NtWriteFile has ten); enough for console output.
extern "C" fn nt_write_file(handle: u64, buffer: u64, length: u64, _a4: u64) -> u64 {
    if buffer == 0 || length as usize > MAX_IO_LEN {
        return NtStatus::INVALID_PARAMETER.0 as u64;
    }
    // The device will read this buffer; ensure it is a valid user range.
    if crate::mm::virt::probe_for_read(buffer, length as usize, 1).is_err() {
        return NtStatus::ACCESS_VIOLATION.0 as u64;
    }
    match handle::ob_reference_object_by_handle(handle) {
        Ok(obj) => {
            // A pipe write end: append to the pipe buffer (unbounded, never blocks).
            if unsafe { io::pipe::is_write_end(obj) } {
                let end = obj as *mut io::pipe::PipeEnd;
                crate::mm::virt::user_access_begin();
                let _ = unsafe { io::pipe::write(end, buffer as *const u8, length as usize) };
                crate::mm::virt::user_access_end();
                return NtStatus::SUCCESS.0 as u64;
            }
            let device = obj as *mut DeviceObject;
            match unsafe {
                io::io_synchronous_request(device, IRP_MJ_WRITE, buffer as *mut u8, length as usize)
            } {
                Ok(_) => NtStatus::SUCCESS.0 as u64,
                Err(e) => e.0 as u64,
            }
        }
        Err(e) => e.0 as u64,
    }
}

/// `NtCreateFile` (simplified): `a1` = ASCII device-name VA, `a2` = length.
/// Resolves the name through the object namespace and returns an open
/// **handle** to the device in RAX (0 on failure). The real NtCreateFile
/// takes an `OBJECT_ATTRIBUTES`; this is the minimal "open a device by name"
/// path a console app's startup needs.
extern "C" fn nt_create_file(name_ptr: u64, name_len: u64, _a3: u64, _a4: u64) -> u64 {
    let (ptr, len) = (name_ptr as *const u8, name_len as usize);
    if ptr.is_null() || len == 0 || len > 128 {
        return 0;
    }
    // The name comes from user space — validate the range before reading it.
    if crate::mm::virt::probe_for_read(name_ptr, len, 1).is_err() {
        return 0;
    }
    // Copy the ASCII name into a kernel buffer (SMAP-bracketed).
    let mut ascii = [0u8; 128];
    crate::mm::virt::user_access_begin();
    unsafe { core::ptr::copy_nonoverlapping(ptr, ascii.as_mut_ptr(), len) };
    crate::mm::virt::user_access_end();
    let ascii = &ascii[..len];

    // First try the device namespace (e.g. \Device\Console), widening to
    // UTF-16 for the lookup.
    let mut units = [0u16; 128];
    for (i, &b) in ascii.iter().enumerate() {
        units[i] = b as u16;
    }
    let name = UnicodeString {
        length: (len * 2) as u16,
        maximum_length: (len * 2) as u16,
        buffer: units.as_mut_ptr(),
    };
    if let Ok(device) = io::namespace::lookup_device(&name) {
        return handle::ob_create_handle(device as *mut u8, 0);
    }
    // Otherwise treat the name as a filesystem path.
    if let Ok(path) = core::str::from_utf8(ascii) {
        // The virtual "H:" drive is served by a host 9P server: `H:\readme.txt`
        // reads readme.txt from the host over the p9 transport (see io::p9).
        let bare = path.strip_prefix("\\??\\").unwrap_or(path);
        if let Some(rest) = strip_host_drive(bare) {
            return match io::p9::read(rest) {
                Some(bytes) => io::ramfs::open_bytes(bytes)
                    .map_or(0, |f| handle::ob_create_handle(f as *mut u8, 0)),
                None => 0,
            };
        }
        // Otherwise a RAM-filesystem path.
        if let Some(file) = io::ramfs::open(path) {
            return handle::ob_create_handle(file as *mut u8, 0);
        }
    }
    0
}

/// Strip a leading `H:\` (case-insensitive) host-drive prefix, returning the
/// path relative to the host root, or `None` if it is not a host path.
fn strip_host_drive(p: &str) -> Option<&str> {
    let b = p.as_bytes();
    if b.len() >= 3 && b[0].eq_ignore_ascii_case(&b'h') && b[1] == b':' && (b[2] == b'\\' || b[2] == b'/') {
        Some(&p[3..])
    } else {
        None
    }
}

/// `NtOpenFile(FileHandle, DesiredAccess, ObjectAttributes, IoStatusBlock, …)`
/// — the real ntdll file-open path that ulib-based tools (more.com, …) use.
/// Unlike our simplified `NtCreateFile`, this takes the genuine NT prototype:
/// the path is a UNICODE_STRING reached through `ObjectAttributes->ObjectName`,
/// in NT form (`\??\C:\file`). We strip the `\??\` device prefix, open the
/// RAM-fs file, write the handle to `*FileHandle`, fill the IO_STATUS_BLOCK,
/// and return an NTSTATUS (0 on success). ShareAccess/OpenOptions (on the
/// stack) are ignored — we have no sharing model. The first four arguments map
/// to our four syscall registers.
extern "C" fn nt_open_file(
    file_handle_out: u64,
    _desired_access: u64,
    object_attributes: u64,
    io_status_block: u64,
) -> u64 {
    const STATUS_INVALID_PARAMETER: u64 = 0xC000_000D;
    const STATUS_OBJECT_NAME_NOT_FOUND: u64 = 0xC000_0034;
    const FILE_OPENED: u64 = 1;
    if file_handle_out == 0 || object_attributes == 0 {
        return STATUS_INVALID_PARAMETER;
    }
    // OBJECT_ATTRIBUTES.ObjectName (PUNICODE_STRING) sits at +0x10.
    if crate::mm::virt::probe_for_read(object_attributes, 0x18, 8).is_err() {
        return STATUS_INVALID_PARAMETER;
    }
    let name_ptr = unsafe {
        crate::mm::virt::user_access_begin();
        let p = *((object_attributes + 0x10) as *const u64);
        crate::mm::virt::user_access_end();
        p
    };
    if name_ptr == 0 || crate::mm::virt::probe_for_read(name_ptr, 16, 8).is_err() {
        return STATUS_INVALID_PARAMETER;
    }
    // UNICODE_STRING { u16 Length; u16 MaximumLength; u32 _pad; u64 Buffer; }
    let (len_bytes, buf_va) = unsafe {
        crate::mm::virt::user_access_begin();
        let l = *(name_ptr as *const u16) as usize;
        let b = *((name_ptr + 8) as *const u64);
        crate::mm::virt::user_access_end();
        (l, b)
    };
    let mut wbuf = [0u16; 260];
    let Some(w) = read_user_u16(buf_va, (len_bytes / 2).min(260), &mut wbuf) else {
        return STATUS_INVALID_PARAMETER;
    };
    let mut abuf = [0u8; 260];
    let mut n = 0;
    for &c in w {
        if n < abuf.len() {
            abuf[n] = c as u8;
            n += 1;
        }
    }
    let Ok(mut path) = core::str::from_utf8(&abuf[..n]) else {
        return STATUS_INVALID_PARAMETER;
    };
    // DOS paths arrive in NT form: "\??\C:\file" — strip the device prefix.
    if let Some(rest) = path.strip_prefix("\\??\\") {
        path = rest;
    }
    // The virtual "H:" drive is served by a host 9P server (see io::p9).
    let opened = if let Some(rest) = strip_host_drive(path) {
        io::p9::read(rest).and_then(io::ramfs::open_bytes)
    } else {
        io::ramfs::open(path)
    };
    let Some(file) = opened else {
        return STATUS_OBJECT_NAME_NOT_FOUND;
    };
    let h = handle::ob_create_handle(file as *mut u8, 0);
    if crate::mm::virt::probe_for_write(file_handle_out, 8, 8).is_ok() {
        crate::mm::virt::user_access_begin();
        unsafe { *(file_handle_out as *mut u64) = h };
        crate::mm::virt::user_access_end();
    }
    if io_status_block != 0 && crate::mm::virt::probe_for_write(io_status_block, 16, 8).is_ok() {
        crate::mm::virt::user_access_begin();
        unsafe {
            *(io_status_block as *mut u32) = 0; // Status = STATUS_SUCCESS
            *((io_status_block + 8) as *mut u64) = FILE_OPENED; // Information
        }
        crate::mm::virt::user_access_end();
    }
    0 // STATUS_SUCCESS
}

/// `NtReadFile(Handle, Buffer, Length)` — resolve the handle to its device
/// and issue a synchronous `IRP_MJ_READ`. Returns the **byte count** read in
/// RAX (0 on error/EOF) — a simplified convention so the caller can use the
/// result directly as a length.
extern "C" fn nt_read_file(handle: u64, buffer: u64, length: u64, _a4: u64) -> u64 {
    if buffer == 0 || length as usize > MAX_IO_LEN {
        return 0; // bytes read
    }
    // The device will write into this buffer; ensure it is a valid user range.
    if crate::mm::virt::probe_for_write(buffer, length as usize, 1).is_err() {
        return 0; // bytes read
    }
    match handle::ob_reference_object_by_handle(handle) {
        Ok(obj) => {
            // A RAM-filesystem file reads from memory, not a device IRP.
            if unsafe { io::ramfs::is_file_object(obj) } {
                let file = obj as *mut io::ramfs::FileObject;
                crate::mm::virt::user_access_begin(); // SMAP: writing the user buffer
                let n = unsafe { io::ramfs::read(file, buffer as *mut u8, length as usize) };
                crate::mm::virt::user_access_end();
                return n as u64;
            }
            // A pipe read end: drain the buffer, blocking (preemptibly) until data
            // arrives or the last writer closes (EOF). Between checks interrupts
            // are on, so the timer preempts us and the producer thread runs.
            if unsafe { io::pipe::is_read_end(obj) } {
                let end = obj as *mut io::pipe::PipeEnd;
                let mut spins: u64 = 0;
                loop {
                    crate::mm::virt::user_access_begin();
                    let (n, eof) = unsafe { io::pipe::try_read(end, buffer as *mut u8, length as usize) };
                    crate::mm::virt::user_access_end();
                    if n > 0 {
                        return n as u64;
                    }
                    if eof {
                        return 0; // no data, no writers: end of file
                    }
                    spins += 1;
                    if spins > 2_000_000_000 {
                        return 0; // producer wedged; give up rather than hang
                    }
                    core::hint::spin_loop();
                }
            }
            let device = obj as *mut DeviceObject;
            match unsafe {
                io::io_synchronous_request(
                    device,
                    crate::io::IRP_MJ_READ,
                    buffer as *mut u8,
                    length as usize,
                )
            } {
                Ok(iosb) => iosb.information, // bytes read
                Err(_) => 0,
            }
        }
        Err(_) => 0,
    }
}

/// `NtClose(Handle)` — close a handle, dropping its object reference.
extern "C" fn nt_close(handle: u64, _a2: u64, _a3: u64, _a4: u64) -> u64 {
    handle::ob_close_handle(handle).0 as u64
}

/// `LoadStringW` MUI fallback: `a1` = module base (HINSTANCE), `a2` = string
/// id, `a3` = user UTF-16 buffer VA, `a4` = buffer capacity in chars. Loads
/// the string from the module's registered `.mui` and copies it (NUL-
/// terminated) to the user buffer. Returns the character count (excluding the
/// NUL), 0 if not found.
extern "C" fn nt_load_mui_string(base: u64, id: u64, buf: u64, cch: u64) -> u64 {
    let cch = cch as usize;
    if buf == 0 || cch == 0 {
        return 0;
    }
    let mut tmp = [0u16; 512];
    let want = cch.saturating_sub(1).min(tmp.len());
    // Prefer the calling thread's own `.mui` (every process loads at the same
    // image base, so a base→mui registry would collide between a parent and its
    // child); fall back to the base registry for threads that set none.
    let cur = crate::ke::pcr::ke_get_current_thread();
    let n = unsafe {
        let (mp, ml) = ((*cur).mui_ptr, (*cur).mui_len as usize);
        if mp != 0 && ml != 0 {
            let bytes = core::slice::from_raw_parts(mp as *const u8, ml);
            crate::ldr::mui::load_string_from(bytes, id as u32, &mut tmp[..want])
        } else {
            crate::ldr::mui::load_string(base, id as u32, &mut tmp[..want])
        }
    };
    if n == 0 {
        return 0;
    }
    // Copy the string + NUL into the user buffer (validated, SMAP-bracketed).
    if crate::mm::virt::probe_for_write(buf, (n + 1) * 2, 2).is_err() {
        return 0;
    }
    crate::mm::virt::user_access_begin();
    let dst = buf as *mut u16;
    unsafe {
        for i in 0..n {
            *dst.add(i) = tmp[i];
        }
        *dst.add(n) = 0;
    }
    crate::mm::virt::user_access_end();
    n as u64
}

/// `GetCommandLine`/`__getmainargs` backend: copy the calling thread's
/// command line (ASCII) into the user buffer, returning the byte count. A
/// thread with no command line set gets a default program name.
extern "C" fn nt_get_command_line(buf: u64, max: u64, _a3: u64, _a4: u64) -> u64 {
    let max = max as usize;
    if buf == 0 || max == 0 {
        return 0;
    }
    let cur = crate::ke::pcr::ke_get_current_thread();
    let (ptr, len) = unsafe { ((*cur).cmdline_ptr, (*cur).cmdline_len as usize) };
    const DEFAULT: &[u8] = b"app.exe";
    let (src, src_len) = if ptr == 0 {
        (DEFAULT.as_ptr(), DEFAULT.len())
    } else {
        (ptr as *const u8, len)
    };
    let n = src_len.min(max);
    if crate::mm::virt::probe_for_write(buf, n, 1).is_err() {
        return 0;
    }
    crate::mm::virt::user_access_begin();
    unsafe { core::ptr::copy_nonoverlapping(src, buf as *mut u8, n) };
    crate::mm::virt::user_access_end();
    n as u64
}

/// `GetFileSize` backend: total byte size of a RAM-filesystem file handle, or
/// `0xFFFFFFFF` (INVALID_FILE_SIZE) if the handle isn't a file.
extern "C" fn nt_query_file_size(handle: u64, _a2: u64, _a3: u64, _a4: u64) -> u64 {
    match handle::ob_reference_object_by_handle(handle) {
        Ok(obj) if unsafe { io::ramfs::is_file_object(obj) } => unsafe {
            io::ramfs::size(obj as *mut io::ramfs::FileObject) as u64
        },
        _ => 0xFFFF_FFFF,
    }
}

/// Copy a counted ASCII string from a user pointer into `dst`, returning the
/// borrowed `&str` (or `None` on a bad pointer / over-long / non-UTF-8 name).
/// Probes the range and brackets the read for SMAP — the safe way to pull a
/// short name argument across the user/kernel boundary.
fn read_user_name<'a>(ptr: u64, len: usize, dst: &'a mut [u8]) -> Option<&'a str> {
    if ptr == 0 || len == 0 || len > dst.len() {
        return None;
    }
    if crate::mm::virt::probe_for_read(ptr, len, 1).is_err() {
        return None;
    }
    crate::mm::virt::user_access_begin();
    unsafe { core::ptr::copy_nonoverlapping(ptr as *const u8, dst.as_mut_ptr(), len) };
    crate::mm::virt::user_access_end();
    core::str::from_utf8(&dst[..len]).ok()
}

/// `GetModuleHandleA` backend: `a1` = ASCII module-name VA, `a2` = length.
/// Returns the module's loaded base VA (its `HMODULE`) in RAX, or 0 if the
/// module is not loaded. The runtime half of the loader's load-time linking.
extern "C" fn nt_get_module_handle(name_ptr: u64, name_len: u64, _a3: u64, _a4: u64) -> u64 {
    let mut buf = [0u8; 64];
    match read_user_name(name_ptr, name_len as usize, &mut buf) {
        Some(name) => crate::ldr::loaded::module_base(name),
        None => 0,
    }
}

/// `GetProcAddress` backend: `a1` = module base (an `HMODULE` from
/// [`nt_get_module_handle`]), `a2` = ASCII proc-name VA, `a3` = length.
/// Returns the exported routine's VA in RAX, or 0 if unknown — runtime symbol
/// resolution against the module's PE export table (or ntdll's stub table).
extern "C" fn nt_get_proc_address(module_base: u64, name_ptr: u64, name_len: u64, _a4: u64) -> u64 {
    let mut buf = [0u8; 128];
    match read_user_name(name_ptr, name_len as usize, &mut buf) {
        Some(name) => crate::ldr::loaded::proc_address(module_base, name) as u64,
        None => 0,
    }
}

/// `SetLastError(dwErrCode)` backend — store the code in the calling thread's
/// per-thread last-error slot. Always succeeds (returns SUCCESS).
extern "C" fn set_last_error(code: u64, _a2: u64, _a3: u64, _a4: u64) -> u64 {
    let thread = crate::ke::pcr::ke_get_current_thread();
    if !thread.is_null() {
        unsafe { (*thread).last_error = code as u32 };
    }
    NtStatus::SUCCESS.0 as u64
}

/// `GetLastError()` backend — return the calling thread's last-error code.
extern "C" fn get_last_error(_a1: u64, _a2: u64, _a3: u64, _a4: u64) -> u64 {
    let thread = crate::ke::pcr::ke_get_current_thread();
    if thread.is_null() {
        0
    } else {
        unsafe { (*thread).last_error as u64 }
    }
}
