//! kernel32.dll — a minimal Win32 console-API shim for ntoskrnl-rs.
//!
//! Real console programs import `kernel32` (directly or via the CRT), not
//! `ntdll`. This DLL provides the small surface a no-CRT console program
//! uses — `GetStdHandle`, `WriteFile`, `ReadFile`, `ExitProcess` — each
//! forwarding to this kernel's system calls. It is a genuine PE DLL with a
//! real export table; the loader resolves an app's `kernel32` imports
//! against it (cross-module dynamic linking).
//!
//! The functions issue `syscall` inline, so this DLL has no imports of its
//! own. `GetStdHandle` lazily opens `\Device\Console` and caches the handle
//! in the DLL's writable data — exactly the kind of per-module state a real
//! DLL keeps.

#![no_std]

use core::ffi::c_void;

// System-service numbers (must match kernel `syscalls.rs`).
const NT_TERMINATE_THREAD: u32 = 0;
const NT_DEBUG_WRITE: u32 = 1;
const NT_WRITE_FILE: u32 = 2;
const NT_CREATE_FILE: u32 = 3;
const NT_CLOSE: u32 = 4;
const NT_READ_FILE: u32 = 5;
const NT_ALLOCATE_VIRTUAL_MEMORY: u32 = 6;
const NT_FREE_VIRTUAL_MEMORY: u32 = 7;
const NT_REPORT_TEST_RESULT: u32 = 9;
const NT_DELAY_EXECUTION: u32 = 10;
const NT_QUERY_TICK_COUNT: u32 = 11;
const NT_INCREMENT_COUNTER: u32 = 12;
const NT_GET_MODULE_HANDLE: u32 = 13;
const NT_GET_PROC_ADDRESS: u32 = 14;
const NT_SET_LAST_ERROR: u32 = 15;
const NT_GET_LAST_ERROR: u32 = 16;
const NT_QUERY_FILE_SIZE: u32 = 17;
const NT_LOAD_MUI_STRING: u32 = 18;
const NT_GET_COMMAND_LINE: u32 = 19;
const NT_PEEK_CONSOLE_INPUT: u32 = 20;
const NT_QUERY_HANDLE: u32 = 21;
const NT_REG_OPEN_KEY: u32 = 22;
const NT_REG_CREATE_KEY: u32 = 23;
const NT_REG_QUERY_VALUE: u32 = 24;
const NT_REG_SET_VALUE: u32 = 25;
const NT_REG_ENUM_KEY: u32 = 26;
const NT_CREATE_PROCESS: u32 = 27;
const NT_WAIT_PROCESS: u32 = 28;
const NT_GET_EXIT_CODE_PROCESS: u32 = 29;
const NT_SET_CONSOLE_MODE: u32 = 30;
const NT_LOAD_MESSAGE: u32 = 31;

// A couple of Win32 error codes used by the shim.
const ERROR_SUCCESS: u32 = 0;
const ERROR_ENVVAR_NOT_FOUND: u32 = 203;

#[inline(always)]
unsafe fn syscall3(number: u32, a1: u64, a2: u64, a3: u64) -> u64 {
    let ret: u64;
    core::arch::asm!(
        "syscall",
        inlateout("rax") number as u64 => ret,
        inout("r10") a1 => _,
        inout("rdx") a2 => _,
        inout("r8") a3 => _,
        lateout("r9") _,
        lateout("rcx") _,
        lateout("r11") _,
        options(nostack),
    );
    ret
}

/// Four-argument system call (adds the `r9` slot).
#[inline(always)]
unsafe fn syscall4(number: u32, a1: u64, a2: u64, a3: u64, a4: u64) -> u64 {
    let ret: u64;
    core::arch::asm!(
        "syscall",
        inlateout("rax") number as u64 => ret,
        inout("r10") a1 => _,
        inout("rdx") a2 => _,
        inout("r8") a3 => _,
        inout("r9") a4 => _,
        lateout("rcx") _,
        lateout("r11") _,
        options(nostack),
    );
    ret
}

// Standard-stream handle ids (DWORD)-10/-11/-12.
const STD_INPUT_HANDLE: u32 = 0xFFFF_FFF6;
const STD_OUTPUT_HANDLE: u32 = 0xFFFF_FFF5;
const STD_ERROR_HANDLE: u32 = 0xFFFF_FFF4;

/// Per-stream console handles (stdin/stdout/stderr). Each is a *distinct*
/// handle to `\Device\Console`, opened lazily — so a program that closes one
/// standard stream (e.g. `sort` closes stdin after reading) does not
/// invalidate the others.
static mut STD_HANDLES: [u64; 3] = [0, 0, 0];

/// `GetStdHandle(nStdHandle)` — return the handle for the requested standard
/// stream, each backed by its own `\Device\Console` handle.
#[no_mangle]
pub unsafe extern "C" fn GetStdHandle(n_std_handle: u32) -> u64 {
    let idx = match n_std_handle {
        STD_INPUT_HANDLE => 0,
        STD_OUTPUT_HANDLE => 1,
        STD_ERROR_HANDLE => 2,
        _ => 1, // default to stdout
    };
    // Re-open if we have no handle yet, or if the cached one is stale — the
    // cache lives in shared kernel32 .data, so a prior process that closed its
    // console handle (e.g. sort closes stdin) leaves a dangling value here.
    let cached = STD_HANDLES[idx];
    let stale = cached != 0 && syscall3(NT_QUERY_HANDLE, cached, 0, 0) == 0;
    if cached == 0 || stale {
        let name = b"\\Device\\Console";
        STD_HANDLES[idx] = syscall3(NT_CREATE_FILE, name.as_ptr() as u64, name.len() as u64, 0);
    }
    STD_HANDLES[idx]
}

/// Whether `handle` is one of our opened standard-stream console handles.
unsafe fn is_std_console_handle(handle: u64) -> bool {
    handle != 0 && STD_HANDLES.iter().any(|&h| h == handle)
}

/// `MultiByteToWideChar(CodePage, dwFlags, lpMultiByteStr, cbMultiByte,
/// lpWideCharStr, cchWideChar)` — convert a byte string to UTF-16. We treat
/// the input as Latin-1/ASCII (each byte zero-extends to one wide char),
/// which covers the console/CRT path. `cbMultiByte == -1` means the input is
/// NUL-terminated and the terminator is included in the output. `cchWideChar
/// == 0` is the size query (return the required count without writing).
/// Returns the number of wide chars written (or required), 0 on bad args.
#[no_mangle]
pub unsafe extern "C" fn MultiByteToWideChar(
    _code_page: u32,
    _flags: u32,
    src: *const u8,
    cb: i32,
    dst: *mut u16,
    cch: i32,
) -> i32 {
    if src.is_null() {
        return 0;
    }
    let count = if cb < 0 { k_strlen(src) + 1 } else { cb as usize };
    if cch == 0 {
        return count as i32; // size query
    }
    if dst.is_null() || (cch as usize) < count {
        return 0; // insufficient buffer
    }
    for i in 0..count {
        *dst.add(i) = *src.add(i) as u16;
    }
    count as i32
}

/// `WideCharToMultiByte(CodePage, dwFlags, lpWideCharStr, cchWideChar,
/// lpMultiByteStr, cbMultiByte, lpDefaultChar, lpUsedDefaultChar)` — convert
/// UTF-16 to bytes. Each wide char narrows to its low byte (units above 0xFF
/// degrade to '?'). `cchWideChar == -1` means NUL-terminated (terminator
/// included). `cbMultiByte == 0` is the size query. Returns the number of
/// bytes written (or required), 0 on bad args.
#[no_mangle]
pub unsafe extern "C" fn WideCharToMultiByte(
    _code_page: u32,
    _flags: u32,
    src: *const u16,
    cch: i32,
    dst: *mut u8,
    cb: i32,
    _default_char: *const u8,
    _used_default: *mut i32,
) -> i32 {
    if src.is_null() {
        return 0;
    }
    let count = if cch < 0 {
        // length of the NUL-terminated wide string, including the terminator
        let mut n = 0;
        while *src.add(n) != 0 {
            n += 1;
        }
        n + 1
    } else {
        cch as usize
    };
    if cb == 0 {
        return count as i32; // size query
    }
    if dst.is_null() || (cb as usize) < count {
        return 0; // insufficient buffer
    }
    for i in 0..count {
        let u = *src.add(i);
        *dst.add(i) = if u <= 0xFF { u as u8 } else { b'?' };
    }
    count as i32
}

/// `GetFileType(hFile)` — classify a handle. The C runtime calls this on its
/// standard handles at startup to decide buffering. Our standard handles all
/// refer to the console device, so the console handle is `FILE_TYPE_CHAR`
/// (a character device); anything else is `FILE_TYPE_UNKNOWN`.
#[no_mangle]
pub unsafe extern "C" fn GetFileType(handle: u64) -> u32 {
    const FILE_TYPE_UNKNOWN: u32 = 0x0000;
    const FILE_TYPE_CHAR: u32 = 0x0002;
    if is_std_console_handle(handle) {
        FILE_TYPE_CHAR
    } else {
        FILE_TYPE_UNKNOWN
    }
}

/// `WriteFile(hFile, lpBuffer, nBytes, lpBytesWritten, lpOverlapped)`.
#[no_mangle]
pub unsafe extern "C" fn WriteFile(
    handle: u64,
    buffer: *const u8,
    bytes: u32,
    written: *mut u32,
    _overlapped: *mut c_void,
) -> i32 {
    syscall3(NT_WRITE_FILE, handle, buffer as u64, bytes as u64);
    if !written.is_null() {
        *written = bytes;
    }
    1 // TRUE
}

/// `ReadFile(hFile, lpBuffer, nBytes, lpBytesRead, lpOverlapped)`.
#[no_mangle]
pub unsafe extern "C" fn ReadFile(
    handle: u64,
    buffer: *mut u8,
    bytes: u32,
    read: *mut u32,
    _overlapped: *mut c_void,
) -> i32 {
    let n = syscall3(NT_READ_FILE, handle, buffer as u64, bytes as u64);
    if !read.is_null() {
        *read = n as u32;
    }
    1 // TRUE
}

/// `WriteConsoleA(hConsoleOutput, lpBuffer, nChars, lpCharsWritten, lpReserved)`
/// — the console-specific output API a real console program (and the CRT's
/// `_write`/`fputs` path) calls. ANSI bytes go straight to the console device
/// via `NtWriteFile`; the written count is reported in *characters* (1 byte
/// each here). Returns nonzero on success.
#[no_mangle]
pub unsafe extern "C" fn WriteConsoleA(
    handle: u64,
    buffer: *const u8,
    chars: u32,
    written: *mut u32,
    _reserved: *mut c_void,
) -> i32 {
    syscall3(NT_WRITE_FILE, handle, buffer as u64, chars as u64);
    if !written.is_null() {
        *written = chars;
    }
    1 // TRUE
}

/// `WriteConsoleW(hConsoleOutput, lpBuffer, nChars, lpCharsWritten, lpReserved)`
/// — the wide-char console output API. We have a byte-oriented console, so
/// each UTF-16 unit is narrowed to its low byte (ASCII fast path) in small
/// chunks and written via `NtWriteFile`. The written count is in wide
/// characters. Returns nonzero on success.
#[no_mangle]
pub unsafe extern "C" fn WriteConsoleW(
    handle: u64,
    buffer: *const u16,
    chars: u32,
    written: *mut u32,
    _reserved: *mut c_void,
) -> i32 {
    const CHUNK: usize = 256;
    let mut bytes = [0u8; CHUNK];
    let mut done = 0usize;
    let total = chars as usize;
    while done < total {
        let n = core::cmp::min(CHUNK, total - done);
        for i in 0..n {
            // Narrow to the low byte; non-ASCII units degrade to '?'.
            let u = *buffer.add(done + i);
            bytes[i] = if u < 0x80 { u as u8 } else { b'?' };
        }
        syscall3(NT_WRITE_FILE, handle, bytes.as_ptr() as u64, n as u64);
        done += n;
    }
    if !written.is_null() {
        *written = chars;
    }
    1 // TRUE
}

/// `ExitProcess(uExitCode)` — terminates the calling thread (single-thread
/// process model for now).
#[no_mangle]
pub unsafe extern "C" fn ExitProcess(_exit_code: u32) -> ! {
    syscall3(NT_TERMINATE_THREAD, 0, 0, 0);
    loop {
        core::hint::spin_loop();
    }
}

/// `CloseHandle(hObject)`.
#[no_mangle]
pub unsafe extern "C" fn CloseHandle(handle: u64) -> i32 {
    // NtClose returns an NTSTATUS; map success (0) to TRUE.
    if syscall3(NT_CLOSE, handle, 0, 0) == 0 {
        1
    } else {
        0
    }
}

// --- A small pooled heap ---------------------------------------------------
//
// A real bump + first-fit free-list arena (the same shape as the kernel's
// NonPagedPool), rather than one VM allocation per call. Each block carries
// a 16-byte header holding its total size; the returned pointer is past the
// header. Freed blocks go on a free list and are reused first-fit (no split,
// no coalescing — modest waste, ample for a console program). The arena
// grows by 64 KiB chunks from NtAllocateVirtualMemory.

const HEAP_CHUNK: u64 = 64 * 1024;

#[repr(C)]
struct FreeBlock {
    size: u64,
    next: *mut FreeBlock,
}

static mut FREE_HEAD: *mut FreeBlock = core::ptr::null_mut();
static mut BUMP: u64 = 0;
static mut BUMP_END: u64 = 0;

/// Heap spinlock. The arena (`BUMP`/`FREE_HEAD`) is shared by every thread in
/// the process, so `HeapAlloc`/`HeapFree` must be mutually exclusive — without
/// this, two threads allocating/freeing concurrently corrupt the free list.
/// A plain test-and-set spin: the preemptive scheduler runs the holder once a
/// spinning thread's quantum expires, so it always makes progress.
static HEAP_LOCK: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

#[inline]
fn heap_lock_acquire() {
    use core::sync::atomic::Ordering;
    while HEAP_LOCK
        .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
        .is_err()
    {
        core::hint::spin_loop();
    }
}

#[inline]
fn heap_lock_release() {
    HEAP_LOCK.store(false, core::sync::atomic::Ordering::Release);
}

#[inline]
fn align16(n: u64) -> u64 {
    (n + 15) & !15
}

/// `GetProcessHeap()` — the one default process heap (a constant token; the
/// arena state above is the actual heap).
#[no_mangle]
pub extern "C" fn GetProcessHeap() -> u64 {
    1
}

/// `HeapAlloc(hHeap, dwFlags, dwBytes)` — acquires the heap lock and delegates
/// to [`heap_alloc_locked`].
#[no_mangle]
pub unsafe extern "C" fn HeapAlloc(_heap: u64, _flags: u32, bytes: u64) -> *mut u8 {
    heap_lock_acquire();
    let p = heap_alloc_locked(bytes);
    heap_lock_release();
    p
}

/// The arena allocation itself; the caller must hold [`HEAP_LOCK`].
unsafe fn heap_alloc_locked(bytes: u64) -> *mut u8 {
    let want = align16(bytes + 16); // 16-byte header

    // First-fit reuse from the free list.
    let mut prev: *mut *mut FreeBlock = &raw mut FREE_HEAD;
    let mut cur = FREE_HEAD;
    while !cur.is_null() {
        if (*cur).size >= want {
            *prev = (*cur).next;
            return (cur as u64 + 16) as *mut u8;
        }
        prev = &raw mut (*cur).next;
        cur = (*cur).next;
    }

    // Bump from the arena, growing it if needed.
    if BUMP + want > BUMP_END {
        let chunk = if want > HEAP_CHUNK { align16(want) } else { HEAP_CHUNK };
        let base = syscall3(NT_ALLOCATE_VIRTUAL_MEMORY, chunk, 0, 0);
        if base == 0 {
            return core::ptr::null_mut();
        }
        BUMP = base;
        BUMP_END = base + chunk;
    }
    let hdr = BUMP;
    BUMP += want;
    *(hdr as *mut u64) = want; // size header
    (hdr + 16) as *mut u8
}

/// `HeapFree(hHeap, dwFlags, lpMem)` — return the block to the free list
/// under the heap lock.
#[no_mangle]
pub unsafe extern "C" fn HeapFree(_heap: u64, _flags: u32, mem: *mut u8) -> i32 {
    if mem.is_null() {
        return 0;
    }
    heap_lock_acquire();
    let hdr = mem as u64 - 16;
    let size = *(hdr as *const u64);
    let blk = hdr as *mut FreeBlock;
    (*blk).size = size;
    (*blk).next = FREE_HEAD;
    FREE_HEAD = blk;
    heap_lock_release();
    1
}

/// `lstrlenA(lpString)` — length of a NUL-terminated ANSI string.
#[no_mangle]
pub unsafe extern "C" fn lstrlenA(s: *const u8) -> i32 {
    if s.is_null() {
        return 0;
    }
    k_strlen(s) as i32
}

/// `lstrcmpA(lpString1, lpString2)` — case-sensitive byte comparison.
/// Returns <0, 0, or >0 like `strcmp`.
#[no_mangle]
pub unsafe extern "C" fn lstrcmpA(a: *const u8, b: *const u8) -> i32 {
    if a.is_null() || b.is_null() {
        return 0;
    }
    let mut i = 0;
    loop {
        let (ca, cb) = (*a.add(i), *b.add(i));
        if ca != cb {
            return ca as i32 - cb as i32;
        }
        if ca == 0 {
            return 0;
        }
        i += 1;
    }
}

/// `lstrcmpiA(lpString1, lpString2)` — case-insensitive comparison (ASCII).
#[no_mangle]
pub unsafe extern "C" fn lstrcmpiA(a: *const u8, b: *const u8) -> i32 {
    if a.is_null() || b.is_null() {
        return 0;
    }
    let lower = |c: u8| if c.is_ascii_uppercase() { c + 32 } else { c };
    let mut i = 0;
    loop {
        let (ca, cb) = (lower(*a.add(i)), lower(*b.add(i)));
        if ca != cb {
            return ca as i32 - cb as i32;
        }
        if ca == 0 {
            return 0;
        }
        i += 1;
    }
}

/// `lstrcpyA(lpString1, lpString2)` — copy `src` (with its NUL) into `dst`.
/// Returns `dst`.
#[no_mangle]
pub unsafe extern "C" fn lstrcpyA(dst: *mut u8, src: *const u8) -> *mut u8 {
    if dst.is_null() || src.is_null() {
        return dst;
    }
    let mut i = 0;
    loop {
        let c = *src.add(i);
        *dst.add(i) = c;
        if c == 0 {
            break;
        }
        i += 1;
    }
    dst
}

/// `lstrcatA(lpString1, lpString2)` — append `src` to the NUL-terminated
/// `dst`. Returns `dst`.
#[no_mangle]
pub unsafe extern "C" fn lstrcatA(dst: *mut u8, src: *const u8) -> *mut u8 {
    if dst.is_null() || src.is_null() {
        return dst;
    }
    let base = k_strlen(dst);
    let mut i = 0;
    loop {
        let c = *src.add(i);
        *dst.add(base + i) = c;
        if c == 0 {
            break;
        }
        i += 1;
    }
    dst
}

/// `InterlockedIncrement(Addend)` — atomically increment `*Addend` and return
/// the **new** value. The lock-free building block real Win32 code uses for
/// reference counts and shared counters.
#[no_mangle]
pub unsafe extern "C" fn InterlockedIncrement(addend: *mut i32) -> i32 {
    use core::sync::atomic::{AtomicI32, Ordering};
    if addend.is_null() {
        return 0;
    }
    (*(addend as *const AtomicI32)).fetch_add(1, Ordering::SeqCst) + 1
}

/// `InterlockedDecrement(Addend)` — atomically decrement and return the new
/// value (0 is the canonical "last reference released" signal).
#[no_mangle]
pub unsafe extern "C" fn InterlockedDecrement(addend: *mut i32) -> i32 {
    use core::sync::atomic::{AtomicI32, Ordering};
    if addend.is_null() {
        return 0;
    }
    (*(addend as *const AtomicI32)).fetch_sub(1, Ordering::SeqCst) - 1
}

/// `InterlockedExchange(Target, Value)` — atomically store `Value`, returning
/// the previous value.
#[no_mangle]
pub unsafe extern "C" fn InterlockedExchange(target: *mut i32, value: i32) -> i32 {
    use core::sync::atomic::{AtomicI32, Ordering};
    if target.is_null() {
        return 0;
    }
    (*(target as *const AtomicI32)).swap(value, Ordering::SeqCst)
}

/// `InterlockedCompareExchange(Destination, Exchange, Comparand)` — if
/// `*Destination == Comparand`, store `Exchange`. Returns the **initial**
/// value of `*Destination` either way (so `== Comparand` means it swapped).
#[no_mangle]
pub unsafe extern "C" fn InterlockedCompareExchange(
    destination: *mut i32,
    exchange: i32,
    comparand: i32,
) -> i32 {
    use core::sync::atomic::{AtomicI32, Ordering};
    if destination.is_null() {
        return 0;
    }
    let a = &*(destination as *const AtomicI32);
    match a.compare_exchange(comparand, exchange, Ordering::SeqCst, Ordering::SeqCst) {
        Ok(prev) => prev,  // swapped; prev == comparand
        Err(prev) => prev, // unchanged; prev is the current value
    }
}

/// `OutputDebugStringA(lpOutputString)` — emit a NUL-terminated string to the
/// debugger output (here, the kernel debug serial port via the debug-write
/// service). What apps and the CRT use for `OutputDebugString`-style tracing.
#[no_mangle]
pub unsafe extern "C" fn OutputDebugStringA(s: *const u8) {
    if s.is_null() {
        return;
    }
    let len = k_strlen(s);
    if len > 0 {
        syscall3(NT_DEBUG_WRITE, s as u64, len as u64, 0);
    }
}

/// `ReportTestResult(code)` — a debug-only export: hand `code` to the kernel
/// so a user-mode self-test can be asserted from the kernel side (robust,
/// unlike inferring success from console output length).
#[no_mangle]
pub unsafe extern "C" fn ReportTestResult(code: u64) {
    syscall3(NT_REPORT_TEST_RESULT, code, 0, 0);
}

/// `Sleep(dwMilliseconds)`.
#[no_mangle]
pub unsafe extern "C" fn Sleep(millis: u32) {
    syscall3(NT_DELAY_EXECUTION, millis as u64, 0, 0);
}

/// `GetTickCount64()` — milliseconds since boot (≈ clock ticks).
#[no_mangle]
pub unsafe extern "C" fn GetTickCount64() -> u64 {
    syscall3(NT_QUERY_TICK_COUNT, 0, 0, 0)
}

/// `GetTickCount()` — low 32 bits of the tick count.
#[no_mangle]
pub unsafe extern "C" fn GetTickCount() -> u32 {
    GetTickCount64() as u32
}

/// `QueryPerformanceFrequency(lpFrequency)` — ticks per second of the
/// high-resolution counter. Our counter is the millisecond tick count, so the
/// frequency is 1000 Hz. Writes the value and returns nonzero (TRUE).
#[no_mangle]
pub unsafe extern "C" fn QueryPerformanceFrequency(frequency: *mut i64) -> i32 {
    if frequency.is_null() {
        return 0;
    }
    *frequency = 1000; // 1 tick ≈ 1 ms
    1
}

/// `QueryPerformanceCounter(lpPerformanceCount)` — current value of the
/// high-resolution counter (here, the millisecond tick count, monotonic).
/// Writes the value and returns nonzero (TRUE).
#[no_mangle]
pub unsafe extern "C" fn QueryPerformanceCounter(count: *mut i64) -> i32 {
    if count.is_null() {
        return 0;
    }
    *count = GetTickCount64() as i64;
    1
}

/// FILETIME epoch base: 100-ns intervals from 1601-01-01 (the FILETIME epoch)
/// to a fixed recent wall-clock anchor. We have no real-time clock, so we
/// anchor "boot" at this constant and advance by the tick count — monotonic
/// and plausibly-dated, enough for `time()`/timestamp use.
const FILETIME_BOOT_ANCHOR: u64 = 133_000_000_000_000_000; // ~2022

/// `GetSystemTimeAsFileTime(lpSystemTimeAsFileTime)` — current time as a
/// FILETIME (100-ns intervals since 1601). Synthesized from the anchor plus
/// the tick count (1 ms = 10,000 intervals). Writes the two 32-bit halves.
#[no_mangle]
pub unsafe extern "C" fn GetSystemTimeAsFileTime(file_time: *mut u32) {
    if file_time.is_null() {
        return;
    }
    let value = FILETIME_BOOT_ANCHOR + GetTickCount64() * 10_000;
    // FILETIME is { dwLowDateTime, dwHighDateTime } — low half first.
    *file_time = value as u32;
    *file_time.add(1) = (value >> 32) as u32;
}

/// Win32 `SYSTEM_INFO` (x64 layout). Filled by [`GetSystemInfo`].
#[repr(C)]
pub struct SystemInfo {
    pub w_processor_architecture: u16,
    pub w_reserved: u16,
    pub dw_page_size: u32,
    pub lp_minimum_application_address: u64,
    pub lp_maximum_application_address: u64,
    pub dw_active_processor_mask: u64,
    pub dw_number_of_processors: u32,
    pub dw_processor_type: u32,
    pub dw_allocation_granularity: u32,
    pub w_processor_level: u16,
    pub w_processor_revision: u16,
}

/// `GetSystemInfo(lpSystemInfo)` — report basic machine parameters apps query
/// at startup (page size, allocation granularity, processor count). Reflects
/// this kernel: x64, 4 KiB pages, 64 KiB allocation granularity, single CPU,
/// user address space the low canonical half.
#[no_mangle]
pub unsafe extern "C" fn GetSystemInfo(info: *mut SystemInfo) {
    if info.is_null() {
        return;
    }
    const PROCESSOR_ARCHITECTURE_AMD64: u16 = 9;
    *info = SystemInfo {
        w_processor_architecture: PROCESSOR_ARCHITECTURE_AMD64,
        w_reserved: 0,
        dw_page_size: 4096,
        lp_minimum_application_address: 0x0000_0000_0001_0000, // 64 KiB
        lp_maximum_application_address: 0x0000_7FFF_FFFE_FFFF, // top of low half
        dw_active_processor_mask: 1,
        dw_number_of_processors: 1,
        dw_processor_type: 8664, // PROCESSOR_AMD_X8664
        dw_allocation_granularity: 64 * 1024,
        w_processor_level: 6,
        w_processor_revision: 0,
    };
}

// Version the shim reports (Windows 10.0 build 19041), encoded the way each
// API expects.
// A deliberately fake placeholder version (1.0.1) — nanokrnl reports its own
// version, not a real Windows build number.
const OS_MAJOR: u32 = 1;
const OS_MINOR: u32 = 0;
const OS_BUILD: u32 = 1;
const VER_PLATFORM_WIN32_NT: u32 = 2;

/// `GetVersion()` — the legacy packed version: low byte of LOWORD = major,
/// high byte = minor, HIWORD = build (high bit clear ⇒ NT platform).
#[no_mangle]
pub extern "C" fn GetVersion() -> u32 {
    ((OS_BUILD & 0x7FFF) << 16) | (OS_MINOR << 8) | OS_MAJOR
}

/// Win32 `OSVERSIONINFOA` (ANSI). Filled by [`GetVersionExA`].
#[repr(C)]
pub struct OsVersionInfoA {
    pub dw_os_version_info_size: u32,
    pub dw_major_version: u32,
    pub dw_minor_version: u32,
    pub dw_build_number: u32,
    pub dw_platform_id: u32,
    pub sz_csd_version: [u8; 128],
}

/// `GetVersionExA(lpVersionInformation)` — fill the OS version structure.
/// Reports NT platform 10.0 build 19041, empty service-pack string. Returns
/// nonzero (TRUE).
#[no_mangle]
pub unsafe extern "C" fn GetVersionExA(info: *mut OsVersionInfoA) -> i32 {
    if info.is_null() {
        return 0;
    }
    (*info).dw_major_version = OS_MAJOR;
    (*info).dw_minor_version = OS_MINOR;
    (*info).dw_build_number = OS_BUILD;
    (*info).dw_platform_id = VER_PLATFORM_WIN32_NT;
    (*info).sz_csd_version = [0u8; 128];
    1
}

/// `IsDebuggerPresent()` — no user-mode debugger is attached, so FALSE.
#[no_mangle]
pub extern "C" fn IsDebuggerPresent() -> i32 {
    0
}

// ---------------------------------------------------------------------------
// Additional surface used by classic (non-API-set) console binaries such as
// the real `sort.exe`. The memory/module/console/locale queries below resolve
// to our existing services or sensible constants; file-system and event/wait
// imports are deliberately *not* here — they need kernel subsystems we don't
// have yet.
// ---------------------------------------------------------------------------

/// `VirtualAlloc(lpAddress, dwSize, flAllocationType, flProtect)` — commit
/// `dwSize` bytes and return the base. We ignore the requested address and
/// flags (always commit, RWX) and forward to `NtAllocateVirtualMemory`.
#[no_mangle]
pub unsafe extern "C" fn VirtualAlloc(
    _address: *mut c_void,
    size: u64,
    _alloc_type: u32,
    _protect: u32,
) -> *mut c_void {
    syscall3(NT_ALLOCATE_VIRTUAL_MEMORY, size, 0, 0) as *mut c_void
}

/// `VirtualFree(lpAddress, dwSize, dwFreeType)` — release a `VirtualAlloc`
/// region. Returns nonzero on success.
#[no_mangle]
pub unsafe extern "C" fn VirtualFree(address: *mut c_void, size: u64, _free_type: u32) -> i32 {
    if address.is_null() {
        return 0;
    }
    // NtFreeVirtualMemory returns STATUS_SUCCESS (0) on success.
    (syscall3(NT_FREE_VIRTUAL_MEMORY, address as u64, size.max(4096), 0) == 0) as i32
}

/// `TerminateProcess(hProcess, uExitCode)` — for the current-process
/// pseudo-handle, end the (single-threaded) process; otherwise report success
/// without acting (we have no other processes to signal).
#[no_mangle]
pub unsafe extern "C" fn TerminateProcess(process: u64, _exit_code: u32) -> i32 {
    if process == u64::MAX {
        syscall3(NT_TERMINATE_THREAD, 0, 0, 0);
    }
    1
}

/// `GetModuleHandleW(lpModuleName)` — wide-name counterpart of
/// `GetModuleHandleA`. NULL name (the calling module) → 0 (untracked).
#[no_mangle]
pub unsafe extern "C" fn GetModuleHandleW(name: *const u16) -> u64 {
    if name.is_null() {
        return peb_image_base(); // the calling module
    }
    // Narrow to ASCII on the stack, then reuse the module-handle service.
    let mut buf = [0u8; 64];
    let mut n = 0;
    while n < 63 && *name.add(n) != 0 {
        buf[n] = *name.add(n) as u8;
        n += 1;
    }
    syscall3(NT_GET_MODULE_HANDLE, buf.as_ptr() as u64, n as u64, 0)
}

/// `GetModuleHandleExA(dwFlags, lpModuleName, phModule)` — resolve a module by
/// name and store the handle in `*phModule`. Returns nonzero on success.
#[no_mangle]
pub unsafe extern "C" fn GetModuleHandleExA(_flags: u32, name: *const u8, out: *mut u64) -> i32 {
    let h = if name.is_null() { 0 } else { GetModuleHandleA(name) };
    if !out.is_null() {
        *out = h;
    }
    (h != 0) as i32
}

/// `GetConsoleMode(hConsoleHandle, lpMode)` — report a plausible console mode.
#[no_mangle]
pub unsafe extern "C" fn GetConsoleMode(handle: u64, mode: *mut u32) -> i32 {
    if mode.is_null() {
        return 0;
    }
    *mode = if is_input_handle(handle) { INPUT_MODE } else { OUTPUT_MODE };
    1
}

/// `SetConsoleMode(hConsoleHandle, dwMode)` — accepted (no-op). Returns TRUE.
#[no_mangle]
pub unsafe extern "C" fn SetConsoleMode(handle: u64, mode: u32) -> i32 {
    if is_input_handle(handle) {
        INPUT_MODE = mode;
        // Tell the kernel so the console read honors ENABLE_LINE_INPUT.
        syscall3(NT_SET_CONSOLE_MODE, mode as u64, 0, 0);
    } else {
        OUTPUT_MODE = mode;
    }
    1
}

/// Console mode state mirrored shim-side for `GetConsoleMode`. Input defaults to
/// PROCESSED|LINE|ECHO, output to PROCESSED|WRAP_AT_EOL.
static mut INPUT_MODE: u32 = 0x0007;
static mut OUTPUT_MODE: u32 = 0x0003;

/// Is `handle` the standard input handle (so console-mode calls target input)?
unsafe fn is_input_handle(handle: u64) -> bool {
    handle != 0 && handle == STD_HANDLES[0]
}

/// Win32 `CPINFO`.
#[repr(C)]
pub struct CpInfo {
    pub max_char_size: u32,
    pub default_char: [u8; 2],
    pub lead_byte: [u8; 12],
}

/// `GetCPInfo(CodePage, lpCPInfo)` — report a single-byte code page (no DBCS
/// lead bytes). Returns TRUE.
#[no_mangle]
pub unsafe extern "C" fn GetCPInfo(_code_page: u32, info: *mut CpInfo) -> i32 {
    if info.is_null() {
        return 0;
    }
    (*info).max_char_size = 1;
    (*info).default_char = [b'?', 0];
    (*info).lead_byte = [0u8; 12];
    1
}

/// Win32 `MEMORYSTATUSEX`.
#[repr(C)]
pub struct MemoryStatusEx {
    pub dw_length: u32,
    pub dw_memory_load: u32,
    pub ull_total_phys: u64,
    pub ull_avail_phys: u64,
    pub ull_total_page_file: u64,
    pub ull_avail_page_file: u64,
    pub ull_total_virtual: u64,
    pub ull_avail_virtual: u64,
    pub ull_avail_extended_virtual: u64,
}

/// `GlobalMemoryStatusEx(lpBuffer)` — report this machine's memory (≈120 MiB
/// RAM, no page file). Returns TRUE.
#[no_mangle]
pub unsafe extern "C" fn GlobalMemoryStatusEx(buf: *mut MemoryStatusEx) -> i32 {
    if buf.is_null() {
        return 0;
    }
    const TOTAL: u64 = 120 * 1024 * 1024;
    (*buf).dw_memory_load = 15;
    (*buf).ull_total_phys = TOTAL;
    (*buf).ull_avail_phys = TOTAL - 16 * 1024 * 1024;
    (*buf).ull_total_page_file = TOTAL;
    (*buf).ull_avail_page_file = TOTAL - 16 * 1024 * 1024;
    (*buf).ull_total_virtual = 0x0000_7FFF_FFFF_0000;
    (*buf).ull_avail_virtual = 0x0000_7FFF_0000_0000;
    (*buf).ull_avail_extended_virtual = 0;
    1
}

/// `HeapSetInformation(...)` — accepted (we have no tunable heap policy).
#[no_mangle]
pub extern "C" fn HeapSetInformation(_heap: u64, _class: u32, _info: *mut c_void, _len: u64) -> i32 {
    1
}

/// `SetThreadUILanguage(LangId)` — set the thread UI language and return the
/// LANGID actually selected. A `LangId` of 0 asks the OS to pick the most
/// appropriate language; like the real API we resolve it to a concrete value
/// (US English, 0x0409) rather than echoing 0 back. Callers (e.g. where.exe's
/// startup) treat a 0 return as failure, so 0 must never be returned.
#[no_mangle]
pub extern "C" fn SetThreadUILanguage(lang: u16) -> u16 {
    if lang == 0 {
        0x0409
    } else {
        lang
    }
}

/// `SetUnhandledExceptionFilter(lpTopLevelFilter)` — accept and report no
/// previous filter (we have no top-level handler chain yet).
#[no_mangle]
pub extern "C" fn SetUnhandledExceptionFilter(_filter: *mut c_void) -> *mut c_void {
    core::ptr::null_mut()
}

/// `UnhandledExceptionFilter(ExceptionInfo)` — EXCEPTION_EXECUTE_HANDLER (1).
#[no_mangle]
pub extern "C" fn UnhandledExceptionFilter(_info: *mut c_void) -> i32 {
    1
}

/// `FormatMessageA(...)` — minimal: write a short fixed text for the message
/// id into the caller's buffer (we have no message-table resource). Returns
/// the character count (excluding the NUL), 0 on bad args.
#[no_mangle]
pub unsafe extern "C" fn FormatMessageA(
    _flags: u32,
    _source: *const c_void,
    _message_id: u32,
    _language_id: u32,
    buffer: *mut u8,
    size: u32,
    _args: *mut c_void,
) -> u32 {
    if buffer.is_null() || size == 0 {
        return 0;
    }
    let msg = b"Unknown error\0";
    let n = core::cmp::min(msg.len(), size as usize);
    for i in 0..n {
        *buffer.add(i) = msg[i];
    }
    *buffer.add(n - 1) = 0; // ensure NUL-terminated
    (n - 1) as u32
}

/// `GetCurrentProcess()` — the process pseudo-handle, `(HANDLE)-1`. A constant
/// token that APIs interpret as "the calling process".
#[no_mangle]
pub extern "C" fn GetCurrentProcess() -> u64 {
    u64::MAX // (HANDLE)-1
}

/// `GetCurrentThread()` — the thread pseudo-handle, `(HANDLE)-2`.
#[no_mangle]
pub extern "C" fn GetCurrentThread() -> u64 {
    u64::MAX - 1 // (HANDLE)-2
}

/// `GetCurrentProcessId()` / `GetCurrentThreadId()` — fixed tokens for now
/// (no per-process identity yet; single shared address space).
#[no_mangle]
pub extern "C" fn GetCurrentProcessId() -> u32 {
    4
}
#[no_mangle]
pub extern "C" fn GetCurrentThreadId() -> u32 {
    8
}

/// The process environment block: `KEY=VALUE` entries, each NUL-terminated,
/// the block ended by an empty entry (double NUL) — the Win32 environment
/// strings format. Static for now (no per-process parameters yet).
static ENVIRONMENT: &[u8] = b"OS=ntoskrnl-rs\0ARCH=x86_64\0\0";

/// Length of a NUL-terminated string.
unsafe fn k_strlen(s: *const u8) -> usize {
    let mut n = 0;
    while *s.add(n) != 0 {
        n += 1;
    }
    n
}

/// `GetEnvironmentVariableA(lpName, lpBuffer, nSize)` — look `lpName` up in
/// the environment block and copy its value (NUL-terminated) into
/// `lpBuffer`. Returns the value length in characters (excluding the NUL),
/// or 0 if not found — matching the Win32 contract closely enough for
/// `getenv`.
#[no_mangle]
pub unsafe extern "C" fn GetEnvironmentVariableA(
    name: *const u8,
    buffer: *mut u8,
    size: u32,
) -> u32 {
    let nlen = k_strlen(name);
    let mut p = ENVIRONMENT.as_ptr();
    loop {
        let elen = k_strlen(p);
        if elen == 0 {
            SetLastError(ERROR_ENVVAR_NOT_FOUND); // not found (Win32 contract)
            return 0; // empty entry = end of block
        }
        // Match "name=" at the start of this entry.
        let mut matches = elen > nlen && *p.add(nlen) == b'=';
        if matches {
            for i in 0..nlen {
                if *p.add(i) != *name.add(i) {
                    matches = false;
                    break;
                }
            }
        }
        if matches {
            let val = p.add(nlen + 1);
            let vlen = k_strlen(val);
            if size > 0 {
                let n = core::cmp::min(vlen, (size - 1) as usize);
                for i in 0..n {
                    *buffer.add(i) = *val.add(i);
                }
                *buffer.add(n) = 0;
            }
            SetLastError(ERROR_SUCCESS);
            return vlen as u32;
        }
        p = p.add(elen + 1);
    }
}

/// `GetEnvironmentStringsA()` — pointer to the raw environment block.
#[no_mangle]
pub extern "C" fn GetEnvironmentStringsA() -> *const u8 {
    ENVIRONMENT.as_ptr()
}

/// `GetModuleHandleA(lpModuleName)` — return the base address (`HMODULE`) of
/// a loaded module by name (e.g. "kernel32.dll"), or 0 if not loaded. A NULL
/// name (the calling module) is not tracked yet → 0.
#[no_mangle]
pub unsafe extern "C" fn GetModuleHandleA(name: *const u8) -> u64 {
    if name.is_null() {
        return peb_image_base(); // the calling module
    }
    let len = k_strlen(name);
    syscall3(NT_GET_MODULE_HANDLE, name as u64, len as u64, 0)
}

/// `GetProcAddress(hModule, lpProcName)` — resolve an exported routine by name
/// within `hModule` (an `HMODULE` from `GetModuleHandleA`) to a callable
/// function pointer, or NULL if unknown. Runtime dynamic linking.
#[no_mangle]
pub unsafe extern "C" fn GetProcAddress(module: u64, name: *const u8) -> *const c_void {
    if name.is_null() {
        return core::ptr::null();
    }
    let len = k_strlen(name);
    syscall3(NT_GET_PROC_ADDRESS, module, name as u64, len as u64) as *const c_void
}

/// `LoadLibraryA(lpLibFileName)` — get a usable `HMODULE` for a library by
/// name, to be paired with `GetProcAddress`/`FreeLibrary`. We have no
/// filesystem to load a *new* image from, so this resolves an already-loaded
/// module (kernel32, ntdll) — the same base `GetModuleHandleA` returns — and
/// fails (0) for anything else. Sufficient for the common
/// `LoadLibrary("kernel32")` + `GetProcAddress` runtime-linking idiom.
#[no_mangle]
pub unsafe extern "C" fn LoadLibraryA(name: *const u8) -> u64 {
    if name.is_null() {
        return 0;
    }
    let len = k_strlen(name);
    syscall3(NT_GET_MODULE_HANDLE, name as u64, len as u64, 0)
}

/// `FreeLibrary(hLibModule)` — release a module reference. Our loaded modules
/// are permanent (no reference counting / unmapping yet), so this is a no-op
/// that reports success. Returns nonzero (TRUE).
#[no_mangle]
pub extern "C" fn FreeLibrary(_module: u64) -> i32 {
    1 // TRUE
}

/// `IncrementCounter()` — a debug-only export over the kernel's shared
/// counter; lets a self-test prove concurrent threads both run.
#[no_mangle]
pub unsafe extern "C" fn IncrementCounter() -> u64 {
    syscall3(NT_INCREMENT_COUNTER, 0, 0, 0)
}

/// The process command line, returned by [`GetCommandLineA`]. We have no
/// process parameters yet, so it's a fixed line (NUL-terminated). The args
/// let the CRT shim demonstrate argv parsing.
/// Cached ANSI/UTF-16 command line, filled from the kernel on first use. The
/// kernel returns the *calling thread's* per-process command line.
static mut COMMAND_LINE_A: [u8; 260] = [0; 260];
static mut COMMAND_LINE_W: [u16; 260] = [0; 260];
static mut COMMAND_LINE_READY: bool = false;

unsafe fn ensure_command_line() {
    if COMMAND_LINE_READY {
        return;
    }
    let n = syscall3(NT_GET_COMMAND_LINE, (&raw mut COMMAND_LINE_A) as u64, 259, 0) as usize;
    let n = n.min(259);
    COMMAND_LINE_A[n] = 0;
    for i in 0..=n {
        COMMAND_LINE_W[i] = COMMAND_LINE_A[i] as u16;
    }
    COMMAND_LINE_READY = true;
}

/// `GetCommandLineA()` — the calling process's command line (NUL-terminated).
#[no_mangle]
pub unsafe extern "C" fn GetCommandLineA() -> *const u8 {
    ensure_command_line();
    (&raw const COMMAND_LINE_A) as *const u8
}

/// `GetCommandLineW()` — the command line as UTF-16 (the `wmain`/
/// `CommandLineToArgvW` input).
#[no_mangle]
pub unsafe extern "C" fn GetCommandLineW() -> *const u16 {
    ensure_command_line();
    (&raw const COMMAND_LINE_W) as *const u16
}

/// `GetLastError()` — the calling thread's last-error code. Backed by a
/// per-thread slot in the kernel (the moral equivalent of
/// `TEB.LastErrorValue`, which we don't have a TEB to hold yet).
#[no_mangle]
pub unsafe extern "C" fn GetLastError() -> u32 {
    syscall3(NT_GET_LAST_ERROR, 0, 0, 0) as u32
}

/// `SetLastError(dwErrCode)` — set the calling thread's last-error code.
#[no_mangle]
pub unsafe extern "C" fn SetLastError(code: u32) {
    syscall3(NT_SET_LAST_ERROR, code as u64, 0, 0);
}

// ---------------------------------------------------------------------------
// Stubs needed only so a real classic console binary (sort.exe) *resolves*
// and loads. These cover capabilities we don't fully have yet (a file system,
// real event objects) or paths a clean run never exercises (the SEH-unwind
// trio). Several are nominally ntdll/advapi32 exports, but our loader resolves
// imports by name regardless of the importing DLL, so they live here.
// ---------------------------------------------------------------------------

const INVALID_HANDLE_VALUE: u64 = u64::MAX; // (HANDLE)-1

/// `CreateFileA(lpFileName, ...)` — we have no file system, but we recognize
/// the console pseudo-files (`CONIN$`, `CONOUT$`, `CON`, `CONERR$`) and return
/// a handle to `\Device\Console`, so a console tool that opens its standard
/// streams that way (rather than via `GetStdHandle`) still works. Any other
/// path fails with INVALID_HANDLE_VALUE.
#[no_mangle]
pub unsafe extern "C" fn CreateFileA(
    name: *const u8,
    _access: u32,
    _share: u32,
    _sec: *mut c_void,
    _disp: u32,
    _flags: u32,
    _template: u64,
) -> u64 {
    if name.is_null() {
        return INVALID_HANDLE_VALUE;
    }
    // Case-insensitive match against the console device aliases.
    let is_console = ["CONIN$", "CONOUT$", "CONERR$", "CON"].iter().any(|alias| {
        let a = alias.as_bytes();
        let mut i = 0;
        loop {
            let c = *name.add(i);
            if i == a.len() {
                break c == 0; // full match iff the name ends here too
            }
            let lc = if c.is_ascii_lowercase() { c - 32 } else { c };
            if lc != a[i] {
                break false;
            }
            i += 1;
        }
    });
    if is_console {
        let dev = b"\\Device\\Console";
        return syscall3(NT_CREATE_FILE, dev.as_ptr() as u64, dev.len() as u64, 0);
    }
    // A regular path: open it from the RAM filesystem via NtCreateFile.
    let len = k_strlen(name);
    let h = syscall3(NT_CREATE_FILE, name as u64, len as u64, 0);
    if h == 0 {
        INVALID_HANDLE_VALUE
    } else {
        h
    }
}

/// `GetFileSize(hFile, lpFileSizeHigh)` — size of a RAM-filesystem file, or
/// INVALID_FILE_SIZE (0xFFFFFFFF) for a non-file handle.
#[no_mangle]
pub unsafe extern "C" fn GetFileSize(file: u64, high: *mut u32) -> u32 {
    if !high.is_null() {
        *high = 0;
    }
    syscall3(NT_QUERY_FILE_SIZE, file, 0, 0) as u32
}

/// `GetDiskFreeSpaceA(...)` — no file system; report failure.
#[no_mangle]
pub extern "C" fn GetDiskFreeSpaceA(
    _root: *const u8,
    _spc: *mut u32,
    _bps: *mut u32,
    _free: *mut u32,
    _total: *mut u32,
) -> i32 {
    0
}

/// `GetOverlappedResult(...)` — our I/O is synchronous; report success.
#[no_mangle]
pub extern "C" fn GetOverlappedResult(_file: u64, _ov: *mut c_void, _transferred: *mut u32, _wait: i32) -> i32 {
    1
}

/// `GetTempFileNameA(...)` — no temp dir; report failure (0).
#[no_mangle]
pub extern "C" fn GetTempFileNameA(_path: *const u8, _prefix: *const u8, _unique: u32, _buf: *mut u8) -> u32 {
    0
}

/// `GetTempPath2A(nBufferLength, lpBuffer)` — no temp dir; report 0.
#[no_mangle]
pub extern "C" fn GetTempPath2A(_len: u32, _buf: *mut u8) -> u32 {
    0
}

/// `CreateEventA(...)` — return a non-null dummy handle. Real event objects
/// are future work (used for overlapped I/O we don't perform).
#[no_mangle]
pub extern "C" fn CreateEventA(_sec: *mut c_void, _manual: i32, _initial: i32, _name: *const u8) -> u64 {
    0x1000 // dummy non-null handle
}

/// `ResetEvent(hEvent)` — accept (no-op). Returns TRUE.
#[no_mangle]
pub extern "C" fn ResetEvent(_event: u64) -> i32 {
    1
}

/// Process handles (from `CreateProcessW`) start here — must match the kernel
/// `init::PROC_HANDLE_BASE`.
const PROC_HANDLE_BASE: u64 = 0x3000_0000;

/// `WaitForSingleObject(hHandle, dwMilliseconds)` — for a process handle, wait
/// on the process via the kernel (WAIT_OBJECT_0 if it exited, WAIT_TIMEOUT
/// otherwise). Other handles have no waitable object exposed yet, so report
/// already-signaled.
#[no_mangle]
pub unsafe extern "C" fn WaitForSingleObject(handle: u64, millis: u32) -> u32 {
    if handle >= PROC_HANDLE_BASE {
        let st = syscall3(NT_WAIT_PROCESS, handle, millis as u64, 0);
        return if st == 0 { 0 } else { 0x102 }; // WAIT_OBJECT_0 / WAIT_TIMEOUT
    }
    0 // WAIT_OBJECT_0
}

/// `CreateProcessW(...)` — launch a new process. We take the image path from
/// `lpApplicationName` (or the first token of `lpCommandLine`), load it from
/// the filesystem, and start it. `lpProcessInformation` receives the process
/// handle (used as both hProcess and hThread here) and ids.
#[no_mangle]
pub unsafe extern "C" fn CreateProcessW(
    application_name: *const u16,
    command_line: *const u16,
    _proc_attrs: *const c_void,
    _thread_attrs: *const c_void,
    _inherit: i32,
    _flags: u32,
    _environment: *const c_void,
    _cur_dir: *const u16,
    _startup_info: *const c_void,
    process_information: *mut u8,
) -> i32 {
    // Choose the image path: prefer lpApplicationName; else the first
    // whitespace-delimited token of lpCommandLine.
    let mut tmp = [0u16; 128];
    let path: *const u16 = if !application_name.is_null() && *application_name != 0 {
        application_name
    } else if !command_line.is_null() {
        let mut i = 0;
        while i < tmp.len() - 1 {
            let c = *command_line.add(i);
            if c == 0 || c == b' ' as u16 {
                break;
            }
            tmp[i] = c;
            i += 1;
        }
        tmp[i] = 0;
        tmp.as_ptr()
    } else {
        return 0; // FALSE — no image specified
    };
    let len = wlen(path);
    let handle = syscall3(NT_CREATE_PROCESS, path as u64, len as u64, 0);
    if handle == 0 {
        return 0; // FALSE
    }
    // Fill PROCESS_INFORMATION { HANDLE hProcess; HANDLE hThread; DWORD pid; DWORD tid; }.
    if !process_information.is_null() {
        let pi = process_information;
        core::ptr::write_unaligned(pi as *mut u64, handle); // hProcess
        core::ptr::write_unaligned(pi.add(8) as *mut u64, handle); // hThread
        let id = (handle & 0xFFFF) as u32;
        core::ptr::write_unaligned(pi.add(16) as *mut u32, id); // dwProcessId
        core::ptr::write_unaligned(pi.add(20) as *mut u32, id); // dwThreadId
    }
    1 // TRUE
}

/// `GetExitCodeProcess(hProcess, lpExitCode)` — the process's exit code.
#[no_mangle]
pub unsafe extern "C" fn GetExitCodeProcess(handle: u64, exit_code: *mut u32) -> i32 {
    if !exit_code.is_null() {
        *exit_code = syscall3(NT_GET_EXIT_CODE_PROCESS, handle, 0, 0) as u32;
    }
    1 // TRUE
}

/// `RtlMultiByteToUnicodeN(dst, dstmax, written, src, srclen)` — Latin-1
/// widen (each byte → one UTF-16 unit). NTSTATUS 0 on success.
#[no_mangle]
pub unsafe extern "C" fn RtlMultiByteToUnicodeN(
    dst: *mut u16,
    dst_max: u32,
    written: *mut u32,
    src: *const u8,
    src_len: u32,
) -> i32 {
    let n = core::cmp::min(src_len as usize, (dst_max as usize) / 2);
    for i in 0..n {
        *dst.add(i) = *src.add(i) as u16;
    }
    if !written.is_null() {
        *written = (n * 2) as u32;
    }
    0
}

/// `RtlUnicodeToOemN(dst, dstmax, written, src, srclen_bytes)` — narrow each
/// UTF-16 unit to its low byte. NTSTATUS 0 on success.
#[no_mangle]
pub unsafe extern "C" fn RtlUnicodeToOemN(
    dst: *mut u8,
    dst_max: u32,
    written: *mut u32,
    src: *const u16,
    src_len_bytes: u32,
) -> i32 {
    let units = (src_len_bytes as usize) / 2;
    let n = core::cmp::min(units, dst_max as usize);
    for i in 0..n {
        let u = *src.add(i);
        *dst.add(i) = if u <= 0xFF { u as u8 } else { b'?' };
    }
    if !written.is_null() {
        *written = n as u32;
    }
    0
}

/// `RtlCaptureContext(ContextRecord)` — part of the SEH-unwind path, invoked
/// only when an exception propagates. On a clean run it is referenced but not
/// called; a no-op stub satisfies the import.
#[no_mangle]
pub extern "C" fn RtlCaptureContext(_context: *mut c_void) {}

/// `RtlLookupFunctionEntry(...)` — SEH unwind table lookup; not exercised on a
/// clean run. Returns null (no entry).
#[no_mangle]
pub extern "C" fn RtlLookupFunctionEntry(_pc: u64, _image_base: *mut u64, _history: *mut c_void) -> *mut c_void {
    core::ptr::null_mut()
}

/// `RtlVirtualUnwind(...)` — SEH frame unwinder; not exercised on a clean run.
/// Returns null.
#[no_mangle]
pub extern "C" fn RtlVirtualUnwind(
    _type: u32,
    _base: u64,
    _pc: u64,
    _func: *mut c_void,
    _context: *mut c_void,
    _handler_data: *mut *mut c_void,
    _establisher: *mut u64,
    _ctx_ptrs: *mut c_void,
) -> *mut c_void {
    core::ptr::null_mut()
}

/// `IsTextUnicode(lpv, iSize, lpiResult)` — report "not Unicode" so callers
/// treat the buffer as ANSI/OEM (the safe default for our byte console).
#[no_mangle]
pub unsafe extern "C" fn IsTextUnicode(_buf: *const c_void, _size: i32, result: *mut i32) -> i32 {
    if !result.is_null() {
        *result = 0;
    }
    0 // FALSE
}

// ---------------------------------------------------------------------------
// Wider surface for classic console tools (e.g. choice.exe): wide-string and
// heap helpers, locale/version stubs, and the resource-string loader.
// ---------------------------------------------------------------------------

/// Length of a NUL-terminated UTF-16 string.
unsafe fn wstrlen(s: *const u16) -> usize {
    let mut n = 0;
    while *s.add(n) != 0 {
        n += 1;
    }
    n
}

/// Read `PEB.ImageBaseAddress` via the TEB (`gs:[0x60]` → PEB, `+0x10`). Used
/// for `GetModuleHandle(NULL)` (the calling module's base).
unsafe fn peb_image_base() -> u64 {
    let peb: u64;
    core::arch::asm!("mov {}, gs:[0x60]", out(reg) peb, options(nostack, readonly));
    if peb == 0 {
        return 0;
    }
    *((peb + 0x10) as *const u64)
}

/// `lstrlenW(lpString)`.
#[no_mangle]
pub unsafe extern "C" fn lstrlenW(s: *const u16) -> i32 {
    if s.is_null() {
        return 0;
    }
    wstrlen(s) as i32
}

/// `CharNextW(lpsz)` — advance one wide char (no surrogate handling).
#[no_mangle]
pub unsafe extern "C" fn CharNextW(s: *const u16) -> *const u16 {
    if s.is_null() || *s == 0 {
        s
    } else {
        s.add(1)
    }
}

/// `CharUpperW(lpsz)` — if the high bits are zero it's a single char to
/// uppercase; otherwise uppercase the string in place. ASCII folding.
#[no_mangle]
pub unsafe extern "C" fn CharUpperW(s: *mut u16) -> *mut u16 {
    let v = s as usize;
    if v <= 0xFFFF {
        // Single character passed by value.
        let c = v as u16;
        return (if (0x61..=0x7A).contains(&c) { c - 32 } else { c }) as usize as *mut u16;
    }
    let mut i = 0;
    while *s.add(i) != 0 {
        let c = *s.add(i);
        if (0x61..=0x7A).contains(&c) {
            *s.add(i) = c - 32;
        }
        i += 1;
    }
    s
}

/// `CharUpperBuffW(lpsz, cchLength)` — uppercase `cch` chars (ASCII).
#[no_mangle]
pub unsafe extern "C" fn CharUpperBuffW(s: *mut u16, cch: u32) -> u32 {
    for i in 0..cch as usize {
        let c = *s.add(i);
        if (0x61..=0x7A).contains(&c) {
            *s.add(i) = c - 32;
        }
    }
    cch
}

const CSTR_LESS: i32 = 1;
const CSTR_EQUAL: i32 = 2;
const CSTR_GREATER: i32 = 3;

/// `CompareStringW(Locale, dwCmpFlags, s1, c1, s2, c2)` — ordinal-ish wide
/// compare (NORM_IGNORECASE folds ASCII). `-1` length means NUL-terminated.
#[no_mangle]
pub unsafe extern "C" fn CompareStringW(
    _locale: u32,
    flags: u32,
    s1: *const u16,
    c1: i32,
    s2: *const u16,
    c2: i32,
) -> i32 {
    let n1 = if c1 < 0 { wstrlen(s1) } else { c1 as usize };
    let n2 = if c2 < 0 { wstrlen(s2) } else { c2 as usize };
    let fold = flags & 0x0001 != 0; // NORM_IGNORECASE
    let lc = |c: u16| if fold && (0x41..=0x5A).contains(&c) { c + 32 } else { c };
    let mut i = 0;
    loop {
        if i >= n1 && i >= n2 {
            return CSTR_EQUAL;
        }
        if i >= n1 {
            return CSTR_LESS;
        }
        if i >= n2 {
            return CSTR_GREATER;
        }
        let (a, b) = (lc(*s1.add(i)), lc(*s2.add(i)));
        if a < b {
            return CSTR_LESS;
        }
        if a > b {
            return CSTR_GREATER;
        }
        i += 1;
    }
}

/// `CompareStringA(...)` — ANSI counterpart.
#[no_mangle]
pub unsafe extern "C" fn CompareStringA(
    _locale: u32,
    flags: u32,
    s1: *const u8,
    c1: i32,
    s2: *const u8,
    c2: i32,
) -> i32 {
    let n1 = if c1 < 0 { k_strlen(s1) } else { c1 as usize };
    let n2 = if c2 < 0 { k_strlen(s2) } else { c2 as usize };
    let fold = flags & 0x0001 != 0;
    let lc = |c: u8| if fold && c.is_ascii_uppercase() { c + 32 } else { c };
    let mut i = 0;
    loop {
        if i >= n1 && i >= n2 {
            return CSTR_EQUAL;
        }
        if i >= n1 {
            return CSTR_LESS;
        }
        if i >= n2 {
            return CSTR_GREATER;
        }
        let (a, b) = (lc(*s1.add(i)), lc(*s2.add(i)));
        if a < b {
            return CSTR_LESS;
        }
        if a > b {
            return CSTR_GREATER;
        }
        i += 1;
    }
}

/// `FindStringOrdinal(...)` — substring/prefix/suffix search. Minimal: report
/// "not found" (-1). Callers use it for option matching; ours fall through.
#[no_mangle]
pub extern "C" fn FindStringOrdinal(
    _flags: u32,
    _src: *const u16,
    _src_len: i32,
    _val: *const u16,
    _val_len: i32,
    _ignore_case: i32,
) -> i32 {
    -1
}

/// `GetThreadLocale()` — US English (0x0409).
#[no_mangle]
pub extern "C" fn GetThreadLocale() -> u32 {
    0x0409
}

/// `GetConsoleOutputCP()` — OEM US (437).
#[no_mangle]
pub extern "C" fn GetConsoleOutputCP() -> u32 {
    437
}

/// `GetModuleFileNameW(hModule, lpFilename, nSize)` — report a fixed program
/// path (we have no real module list). Returns the length in wide chars.
#[no_mangle]
pub unsafe extern "C" fn GetModuleFileNameW(_module: u64, filename: *mut u16, size: u32) -> u32 {
    let name: &[u16] = &[
        b'C' as u16, b':' as u16, b'\\' as u16, b'a' as u16, b'p' as u16, b'p' as u16,
        b'.' as u16, b'e' as u16, b'x' as u16, b'e' as u16,
    ];
    if filename.is_null() || size == 0 {
        return 0;
    }
    let n = core::cmp::min(name.len(), (size - 1) as usize);
    for i in 0..n {
        *filename.add(i) = name[i];
    }
    *filename.add(n) = 0;
    n as u32
}

/// `SetConsoleCtrlHandler(...)` — accept (we have no Ctrl-C delivery). TRUE.
#[no_mangle]
pub extern "C" fn SetConsoleCtrlHandler(_handler: *mut c_void, _add: i32) -> i32 {
    1
}

/// `Beep(dwFreq, dwDuration)` — no speaker; succeed silently.
#[no_mangle]
pub extern "C" fn Beep(_freq: u32, _duration: u32) -> i32 {
    1
}

/// `LocalFree(hMem)` — free a `LocalAlloc`/`FormatMessage` block. We route
/// such allocations through the process heap, so free there. Returns NULL.
#[no_mangle]
pub unsafe extern "C" fn LocalFree(mem: *mut u8) -> u64 {
    if !mem.is_null() {
        HeapFree(1, 0, mem);
    }
    0
}

/// `FormatMessageW(...)` — minimal wide message ("Unknown error"). Returns the
/// character count.
#[no_mangle]
pub unsafe extern "C" fn FormatMessageW(
    flags: u32,
    source: *const c_void,
    message_id: u32,
    _language_id: u32,
    buffer: *mut u16,
    size: u32,
    args: *mut c_void,
) -> u32 {
    if buffer.is_null() || size == 0 {
        return 0;
    }
    const FROM_HMODULE: u32 = 0x0800;
    const FROM_SYSTEM: u32 = 0x1000;
    const IGNORE_INSERTS: u32 = 0x0200;

    // Load the raw message template from the module's RT_MESSAGETABLE (.mui).
    let mut tmpl = [0u16; 512];
    let mut tlen = 0usize;
    if flags & (FROM_HMODULE | FROM_SYSTEM) != 0 {
        let module = if !source.is_null() { source as u64 } else { peb_image_base() };
        tlen = syscall4(
            NT_LOAD_MESSAGE,
            module,
            message_id as u64,
            tmpl.as_mut_ptr() as u64,
            tmpl.len() as u64,
        ) as usize;
    }

    // Fallback string when the message id isn't in the table.
    if tlen == 0 {
        let msg: &[u16] = &[
            b'U' as u16, b'n' as u16, b'k' as u16, b'n' as u16, b'o' as u16, b'w' as u16,
            b'n' as u16, b' ' as u16, b'e' as u16, b'r' as u16, b'r' as u16, b'o' as u16,
            b'r' as u16,
        ];
        let n = core::cmp::min(msg.len(), (size - 1) as usize);
        for i in 0..n {
            *buffer.add(i) = msg[i];
        }
        *buffer.add(n) = 0;
        return n as u32;
    }

    // Emit into the caller's buffer, expanding inserts unless told to ignore
    // them. On x64 `lpArguments` (with or without ARGUMENT_ARRAY) is an array of
    // pointer-sized values; `%n` takes the n-th (default `!s!` = a wide string).
    let cap = (size - 1) as usize;
    let mut di = 0usize;
    let expand = flags & IGNORE_INSERTS == 0 && !args.is_null();
    let argv = args as *const u64;
    let mut i = 0usize;
    while i < tlen {
        let c = tmpl[i];
        if expand && c == b'%' as u16 && i + 1 < tlen {
            let d = tmpl[i + 1];
            if (b'1' as u16..=b'9' as u16).contains(&d) {
                let idx = (d - b'1' as u16) as usize;
                // Skip an optional `!printf-spec!` after the number.
                let mut j = i + 2;
                if j < tlen && tmpl[j] == b'!' as u16 {
                    j += 1;
                    while j < tlen && tmpl[j] != b'!' as u16 {
                        j += 1;
                    }
                    if j < tlen {
                        j += 1;
                    }
                }
                // Insert argv[idx] as a NUL-terminated wide string (bounded).
                let p = *argv.add(idx) as *const u16;
                if p as usize >= 0x1_0000 {
                    let mut k = 0usize;
                    while k < 256 {
                        let ch = *p.add(k);
                        if ch == 0 {
                            break;
                        }
                        if di < cap {
                            *buffer.add(di) = ch;
                        }
                        di += 1;
                        k += 1;
                    }
                }
                i = j;
                continue;
            } else if d == b'%' as u16 {
                if di < cap {
                    *buffer.add(di) = b'%' as u16;
                }
                di += 1;
                i += 2;
                continue;
            }
        }
        if di < cap {
            *buffer.add(di) = c;
        }
        di += 1;
        i += 1;
    }
    let term = di.min(cap);
    *buffer.add(term) = 0;
    term as u32
}

/// `HeapSize(hHeap, dwFlags, lpMem)` — usable bytes of a heap block (from our
/// 16-byte size header).
#[no_mangle]
pub unsafe extern "C" fn HeapSize(_heap: u64, _flags: u32, mem: *const u8) -> u64 {
    if mem.is_null() {
        return u64::MAX;
    }
    *((mem as u64 - 16) as *const u64) - 16
}

/// `HeapValidate(...)` — we have no corruption check; report valid.
#[no_mangle]
pub extern "C" fn HeapValidate(_heap: u64, _flags: u32, _mem: *const u8) -> i32 {
    1
}

/// `HeapReAlloc(hHeap, dwFlags, lpMem, dwBytes)` — grow/shrink a block by
/// allocating a new one, copying, and freeing the old.
#[no_mangle]
pub unsafe extern "C" fn HeapReAlloc(heap: u64, flags: u32, mem: *mut u8, bytes: u64) -> *mut u8 {
    if mem.is_null() {
        return HeapAlloc(heap, flags, bytes);
    }
    let old = HeapSize(heap, flags, mem);
    let new = HeapAlloc(heap, flags, bytes);
    if new.is_null() {
        return core::ptr::null_mut();
    }
    let n = core::cmp::min(old, bytes) as usize;
    for i in 0..n {
        *new.add(i) = *mem.add(i);
    }
    HeapFree(heap, flags, mem);
    new
}

/// `LoadStringW(hInstance, uID, lpBuffer, cchBufferMax)` — load a UTF-16
/// string resource from a module's `.rsrc`. Windows packs strings in bundles
/// of 16 (`RT_STRING`); string `uID` is item `uID & 15` of bundle
/// `(uID >> 4) + 1`, each item a `u16` length followed by that many `u16`
/// chars. Copies up to `cchBufferMax-1` chars (NUL-terminated) and returns the
/// count, or 0 if not found.
#[no_mangle]
pub unsafe extern "C" fn LoadStringW(
    h_instance: u64,
    u_id: u32,
    buffer: *mut u16,
    cch_max: i32,
) -> i32 {
    let base = if h_instance == 0 { peb_image_base() } else { h_instance };
    if base == 0 || buffer.is_null() || cch_max <= 0 {
        return 0;
    }
    // Try the module's own RT_STRING resources first; if it has none (modern
    // tools keep strings in a side .mui), fall back to the kernel's MUI
    // resolver, which parses the registered `<exe>.mui`.
    if let Some(n) = load_string_from_image(base, u_id, buffer, cch_max) {
        return n;
    }
    syscall4(NT_LOAD_MUI_STRING, base, u_id as u64, buffer as u64, cch_max as u64) as i32
}

/// Parse an `RT_STRING` resource directly from a *mapped* module image at
/// `base`. Returns the copied length, or `None` if the image has no such
/// string (so the caller can try the MUI fallback).
unsafe fn load_string_from_image(base: u64, u_id: u32, buffer: *mut u16, cch_max: i32) -> Option<i32> {
    let rd32 = |off: usize| core::ptr::read_unaligned((base + off as u64) as *const u32);
    let rd16 = |off: usize| core::ptr::read_unaligned((base + off as u64) as *const u16);
    if rd16(0) != 0x5A4D {
        return None; // 'MZ'
    }
    let e_lfanew = rd32(0x3C) as usize;
    let opt = e_lfanew + 24;
    let res_rva = rd32(opt + 112 + 2 * 8) as usize; // data dir 2 = resource
    if res_rva == 0 {
        return None;
    }
    // Offsets within an entry's OffsetToData are relative to the resource base.
    let dir_find = |dir_off: usize, want_id: u32| -> Option<usize> {
        let named = rd16(res_rva + dir_off + 12) as usize;
        let ids = rd16(res_rva + dir_off + 14) as usize;
        let entries = dir_off + 16 + named * 8;
        for i in 0..ids {
            let e = entries + i * 8;
            if rd32(res_rva + e) == want_id {
                return Some((rd32(res_rva + e + 4) & 0x7FFF_FFFF) as usize);
            }
        }
        None
    };
    const RT_STRING: u32 = 6;
    let bundle_id = (u_id >> 4) + 1;
    let type_dir = dir_find(0, RT_STRING)?;
    let name_dir = dir_find(type_dir, bundle_id)?;
    let first = name_dir + 16; // first language entry
    let data_off = (rd32(res_rva + first + 4) & 0x7FFF_FFFF) as usize;
    let blob_rva = rd32(res_rva + data_off) as usize;
    let mut p = base + blob_rva as u64;
    for _ in 0..(u_id & 15) as usize {
        let len = core::ptr::read_unaligned(p as *const u16) as u64;
        p += 2 + len * 2;
    }
    let len = core::ptr::read_unaligned(p as *const u16) as usize;
    let src = (p + 2) as *const u16;
    let n = core::cmp::min(len, (cch_max - 1) as usize);
    for i in 0..n {
        *buffer.add(i) = core::ptr::read_unaligned(src.add(i));
    }
    *buffer.add(n) = 0;
    Some(n as i32)
}

/// `ReadConsoleW(hConsole, lpBuffer, nChars, lpCharsRead, pInputControl)` —
/// read up to `nChars` wide characters from the console. We read bytes from
/// the console device (via `NtReadFile`) and zero-extend each to UTF-16. 0
/// chars read means EOF.
#[no_mangle]
pub unsafe extern "C" fn ReadConsoleW(
    handle: u64,
    buffer: *mut u16,
    chars: u32,
    chars_read: *mut u32,
    _input_control: *mut c_void,
) -> i32 {
    // Read into a temporary byte buffer, then widen.
    let mut bytes = [0u8; 256];
    let want = core::cmp::min(chars as usize, bytes.len());
    let n = syscall3(NT_READ_FILE, handle, bytes.as_mut_ptr() as u64, want as u64) as usize;
    for i in 0..n {
        *buffer.add(i) = bytes[i] as u16;
    }
    if !chars_read.is_null() {
        *chars_read = n as u32;
    }
    1 // TRUE
}

/// `FlushConsoleInputBuffer(hConsole)` — no buffered input model; succeed.
#[no_mangle]
pub extern "C" fn FlushConsoleInputBuffer(_handle: u64) -> i32 {
    1
}

/// `PeekConsoleInputW(hConsole, lpBuffer, nLength, lpNumberOfEventsRead)` —
/// report a pending key event without consuming it. We peek the console input
/// buffer (via syscall): if a byte is waiting we synthesize one `INPUT_RECORD`
/// `KEY_EVENT` (key-down, the buffered char) so interactive tools that poll for
/// an event before reading (e.g. choice.exe) proceed to `ReadConsoleW`;
/// otherwise we report zero events. The byte stays buffered for the read.
#[no_mangle]
pub unsafe extern "C" fn PeekConsoleInputW(
    _handle: u64,
    buffer: *mut c_void,
    length: u32,
    events_read: *mut u32,
) -> i32 {
    let peeked = syscall3(NT_PEEK_CONSOLE_INPUT, 0, 0, 0) as i64;
    let have = peeked >= 0 && length >= 1 && !buffer.is_null();
    if have {
        // INPUT_RECORD: EventType (u16) + pad; KEY_EVENT_RECORD at +4.
        let p = buffer as *mut u8;
        core::ptr::write_unaligned(p as *mut u16, 0x0001); // KEY_EVENT
        core::ptr::write_unaligned(p.add(4) as *mut i32, 1); // bKeyDown = TRUE
        core::ptr::write_unaligned(p.add(8) as *mut u16, 1); // wRepeatCount
        core::ptr::write_unaligned(p.add(10) as *mut u16, 0); // wVirtualKeyCode
        core::ptr::write_unaligned(p.add(12) as *mut u16, 0); // wVirtualScanCode
        core::ptr::write_unaligned(p.add(14) as *mut u16, peeked as u16); // UnicodeChar
        core::ptr::write_unaligned(p.add(16) as *mut u32, 0); // dwControlKeyState
    }
    if !events_read.is_null() {
        *events_read = if have { 1 } else { 0 };
    }
    1
}

// --- ntdll version-compare helpers + VERSION.dll (resolved by name) --------

/// `VerSetConditionMask(ConditionMask, TypeMask, Condition)` — build a version
/// comparison mask. We pair it with a permissive `RtlVerifyVersionInfo`, so
/// the exact bits don't matter; pass the mask through.
#[no_mangle]
pub extern "C" fn VerSetConditionMask(mask: u64, _type_mask: u32, _condition: u8) -> u64 {
    mask
}

/// `RtlVerifyVersionInfo(...)` — report the OS satisfies the requested version
/// (STATUS_SUCCESS). We present Windows 10.0, newer than anything a classic
/// tool checks for.
#[no_mangle]
pub extern "C" fn RtlVerifyVersionInfo(_info: *mut c_void, _type_mask: u32, _cond: u64) -> i32 {
    0 // STATUS_SUCCESS
}

const RT_VERSION_ID: u32 = 16;

/// Locate the `RT_VERSION` resource (the `VS_VERSIONINFO` block) in a mapped PE
/// image. Returns `(data_va, size_in_bytes)`. We take the first name/language
/// entry (tools ship a single version block). Mirrors the `.rsrc` walk in
/// [`load_string_from_image`].
unsafe fn find_version_resource(base: u64) -> Option<(u64, u32)> {
    let rd16 = |off: u64| core::ptr::read_unaligned((base + off) as *const u16);
    let rd32 = |off: u64| core::ptr::read_unaligned((base + off) as *const u32);
    if rd16(0) != 0x5A4D {
        return None; // 'MZ'
    }
    let e_lfanew = rd32(0x3C) as u64;
    let opt = e_lfanew + 24;
    let res_rva = rd32(opt + 112 + 2 * 8) as u64; // data dir 2 = resource
    if res_rva == 0 {
        return None;
    }
    // First id-entry under a directory at `dir_off` (relative to res base):
    // returns the entry's offset field (high bit = subdirectory).
    let first_entry = |dir_off: u64| -> u64 {
        let named = rd16(res_rva + dir_off + 12) as u64;
        rd32(res_rva + dir_off + 16 + named * 8 + 4) as u64 & 0x7FFF_FFFF
    };
    // Find the RT_VERSION subdirectory by id at the type level.
    let type_named = rd16(res_rva + 12) as u64;
    let type_ids = rd16(res_rva + 14) as u64;
    let mut type_dir = None;
    for i in 0..type_ids {
        let e = 16 + type_named * 8 + i * 8;
        if rd32(res_rva + e) == RT_VERSION_ID {
            type_dir = Some(rd32(res_rva + e + 4) as u64 & 0x7FFF_FFFF);
            break;
        }
    }
    let type_dir = type_dir?;
    let name_dir = first_entry(type_dir); // -> language directory
    let data_off = first_entry(name_dir); // -> data entry (leaf)
    let blob_rva = rd32(res_rva + data_off) as u64;
    let size = rd32(res_rva + data_off + 4);
    Some((base + blob_rva, size))
}

/// `GetFileVersionInfoSizeExW(flags, filename, lpdwHandle)` — return the size in
/// bytes of the queried file's version resource. We resolve "the file" to the
/// calling process image (callers query their own path via
/// `GetModuleFileNameW(NULL)`), so we read its `RT_VERSION` from memory. Must be
/// non-zero or callers (e.g. where.exe's startup) treat it as fatal.
#[no_mangle]
pub unsafe extern "C" fn GetFileVersionInfoSizeExW(_flags: u32, _filename: *const u16, handle: *mut u32) -> u32 {
    if !handle.is_null() {
        *handle = 0;
    }
    match find_version_resource(peb_image_base()) {
        Some((_, size)) => size,
        None => 0,
    }
}

/// `GetFileVersionInfoExW(flags, filename, handle, len, data)` — copy the
/// process image's `VS_VERSIONINFO` block into `data`. The block VerQueryValueW
/// later parses is this raw resource.
#[no_mangle]
pub unsafe extern "C" fn GetFileVersionInfoExW(
    _flags: u32,
    _filename: *const u16,
    _handle: u32,
    len: u32,
    data: *mut c_void,
) -> i32 {
    if let Some((va, size)) = find_version_resource(peb_image_base()) {
        if data.is_null() || len < size {
            return 0;
        }
        core::ptr::copy_nonoverlapping(va as *const u8, data as *mut u8, size as usize);
        return 1;
    }
    0
}

/// Round up to a 32-bit boundary (`VS_VERSIONINFO` node padding).
#[inline]
fn align4(x: u64) -> u64 {
    (x + 3) & !3
}

/// Compare a NUL-terminated wide key at `key_va` against `want` (ASCII
/// case-insensitive). `want` is not NUL-terminated.
unsafe fn wkey_eq(key_va: u64, want: &[u16]) -> bool {
    for (i, &w) in want.iter().enumerate() {
        let k = core::ptr::read_unaligned((key_va + (i as u64) * 2) as *const u16);
        let lc = |c: u16| if (b'A' as u16..=b'Z' as u16).contains(&c) { c + 32 } else { c };
        if lc(k) != lc(w) {
            return false;
        }
    }
    // The stored key must end right after `want`.
    core::ptr::read_unaligned((key_va + (want.len() as u64) * 2) as *const u16) == 0
}

/// `VerQueryValueW(pBlock, lpSubBlock, lplpBuffer, puLen)` — navigate the
/// `VS_VERSIONINFO` tree to the node named by the backslash-delimited
/// `lpSubBlock` and return a pointer to its value + length. `"\\"` (or empty)
/// returns the root `VS_FIXEDFILEINFO`. Text values report length in characters,
/// binary values in bytes (as the real API does).
#[no_mangle]
pub unsafe extern "C" fn VerQueryValueW(
    block: *const c_void,
    sub_block: *const u16,
    buffer: *mut *mut c_void,
    len: *mut u32,
) -> i32 {
    if block.is_null() || buffer.is_null() {
        return 0;
    }
    let block = block as u64;
    // The value pointer + length of a node at `node` (offset relative to block).
    let node_value = |node: u64| -> (u64, u16, u16) {
        let value_len = core::ptr::read_unaligned((node + 2) as *const u16);
        let wtype = core::ptr::read_unaligned((node + 4) as *const u16);
        // Key is NUL-terminated UTF-16 starting at +6.
        let mut k = node + 6;
        while core::ptr::read_unaligned(k as *const u16) != 0 {
            k += 2;
        }
        k += 2; // past terminator
        let value = block + align4(k - block);
        (value, value_len, wtype)
    };
    // Iterate the children of `node` (whose total length is `node_len`),
    // invoking `f(child_off)`; stop if `f` returns true.
    let find_child = |node: u64, comp: &[u16]| -> Option<u64> {
        let node_len = core::ptr::read_unaligned(node as *const u16) as u64;
        let (value, vlen, wtype) = node_value(node);
        let value_bytes = if wtype == 1 { vlen as u64 * 2 } else { vlen as u64 };
        let mut child = block + align4((value - block) + value_bytes);
        let end = node + node_len;
        while child + 6 <= end {
            let clen = core::ptr::read_unaligned(child as *const u16) as u64;
            if clen == 0 {
                break;
            }
            if wkey_eq(child + 6, comp) {
                return Some(child);
            }
            child = block + align4((child - block) + clen);
        }
        None
    };

    // Walk the backslash-delimited path. The leading "\" selects the root.
    let mut node = block;
    let mut p = sub_block;
    // Skip a leading backslash.
    if !p.is_null() && *p == b'\\' as u16 {
        p = p.add(1);
    }
    while !p.is_null() && *p != 0 {
        // Read one component up to the next '\\' or NUL.
        let start = p;
        let mut n = 0usize;
        while *p != 0 && *p != b'\\' as u16 {
            p = p.add(1);
            n += 1;
        }
        // Build a slice view of the component.
        let comp = core::slice::from_raw_parts(start, n);
        match find_child(node, comp) {
            Some(c) => node = c,
            None => return 0,
        }
        if *p == b'\\' as u16 {
            p = p.add(1);
        }
    }

    let (value, vlen, wtype) = node_value(node);
    *buffer = value as *mut c_void;
    if !len.is_null() {
        *len = vlen as u32; // chars for text, bytes for binary
    }
    1
}

// ---------------------------------------------------------------------------
// Surface for `where.exe`: wide file/path/find/time/locale helpers. Directory
// enumeration is stubbed to "no matches", which is enough for a search tool
// to run and report "not found" (its message comes from its .mui via MUI).
// ---------------------------------------------------------------------------

const INVALID_FILE_ATTRIBUTES: u32 = 0xFFFF_FFFF;
const ERROR_FILE_NOT_FOUND: u32 = 2;
const ERROR_NO_MORE_FILES: u32 = 18;

/// `GetUserDefaultLCID()` — US English.
#[no_mangle]
pub extern "C" fn GetUserDefaultLCID() -> u32 {
    0x0409
}

/// `SetErrorMode(uMode)` — accept; report previous mode 0.
#[no_mangle]
pub extern "C" fn SetErrorMode(_mode: u32) -> u32 {
    0
}

/// Copy a wide string (with NUL) into `dst` bounded by `cap`; return the
/// length written (excluding NUL), or required length if it doesn't fit.
unsafe fn wcopy(dst: *mut u16, cap: u32, src: &[u16]) -> u32 {
    let need = src.len(); // includes trailing 0 in callers' slices
    if dst.is_null() || (cap as usize) < need {
        return need as u32;
    }
    for (i, &c) in src.iter().enumerate() {
        *dst.add(i) = c;
    }
    (need - 1) as u32 // exclude NUL
}

// --- Environment block (wide) ----------------------------------------------
// cmd.exe copies the inherited environment via GetEnvironmentStringsW; a NULL
// block makes it report "out of environment space" and run a degraded startup.
// We keep a modifiable double-NUL-terminated UTF-16 block ("VAR=VAL\0...\0\0"),
// sorted case-insensitively by name (Windows guarantees that, and cmd relies on
// it). The placeholder values are deliberately fake (nanokrnl, not Windows).
static mut ENV_W: [u16; 4096] = [0; 4096];
static mut ENV_W_LEN: usize = 0; // units used, including the final block NUL
static mut ENV_W_INIT: bool = false;

const ENV_DEFAULTS: &[&str] = &[
    "COMSPEC=C:\\cmd.exe",
    "OS=nanokrnl",
    "PATH=C:\\",
    "PATHEXT=.EXE;.BAT;.CMD",
    "PROMPT=$P$G",
    "SystemRoot=C:\\fxcknmc",
];

unsafe fn ensure_env() {
    if ENV_W_INIT {
        return;
    }
    let mut p = 0usize;
    for e in ENV_DEFAULTS {
        for b in e.bytes() {
            ENV_W[p] = b as u16;
            p += 1;
        }
        ENV_W[p] = 0;
        p += 1;
    }
    ENV_W[p] = 0; // block terminator
    p += 1;
    ENV_W_LEN = p;
    ENV_W_INIT = true;
}

fn wlc(c: u16) -> u16 {
    if (b'A' as u16..=b'Z' as u16).contains(&c) {
        c + 32
    } else {
        c
    }
}

/// Find the entry matching `name`; returns (entry_start, value_start, end_past_nul).
unsafe fn env_find(name: *const u16) -> Option<(usize, usize, usize)> {
    ensure_env();
    let nlen = wlen(name);
    let mut i = 0;
    while i < ENV_W_LEN && ENV_W[i] != 0 {
        let start = i;
        let mut eq = i;
        while eq < ENV_W_LEN && ENV_W[eq] != 0 && ENV_W[eq] != b'=' as u16 {
            eq += 1;
        }
        let klen = eq - start;
        let mut e = eq;
        while e < ENV_W_LEN && ENV_W[e] != 0 {
            e += 1;
        }
        let end = e + 1;
        if klen == nlen && ENV_W[eq] == b'=' as u16 {
            let mut m = true;
            for k in 0..nlen {
                if wlc(ENV_W[start + k]) != wlc(*name.add(k)) {
                    m = false;
                    break;
                }
            }
            if m {
                return Some((start, eq + 1, end));
            }
        }
        i = end;
    }
    None
}

/// `GetEnvironmentStringsW()` — pointer to the environment block.
#[no_mangle]
pub unsafe extern "C" fn GetEnvironmentStringsW() -> *const u16 {
    ensure_env();
    (&raw const ENV_W) as *const u16
}

/// `FreeEnvironmentStringsW(block)` — the block is static; succeed.
#[no_mangle]
pub extern "C" fn FreeEnvironmentStringsW(_block: *const u16) -> i32 {
    1
}

/// `GetEnvironmentVariableW(lpName, lpBuffer, nSize)` — value from the block, or
/// 0 + ERROR_ENVVAR_NOT_FOUND. Returns the required size if the buffer is small.
#[no_mangle]
pub unsafe extern "C" fn GetEnvironmentVariableW(name: *const u16, buffer: *mut u16, size: u32) -> u32 {
    if name.is_null() {
        return 0;
    }
    match env_find(name) {
        Some((_, vstart, end)) => {
            let vlen = (end - 1) - vstart;
            if buffer.is_null() || (size as usize) < vlen + 1 {
                return (vlen + 1) as u32;
            }
            for k in 0..vlen {
                *buffer.add(k) = ENV_W[vstart + k];
            }
            *buffer.add(vlen) = 0;
            vlen as u32
        }
        None => {
            SetLastError(ERROR_ENVVAR_NOT_FOUND);
            0
        }
    }
}

/// `SetEnvironmentVariableW(lpName, lpValue)` — replace/append/delete. NULL
/// value deletes. Returns TRUE.
#[no_mangle]
pub unsafe extern "C" fn SetEnvironmentVariableW(name: *const u16, value: *const u16) -> i32 {
    if name.is_null() {
        return 0;
    }
    ensure_env();
    if let Some((start, _, end)) = env_find(name) {
        let shift = end - start;
        for k in end..ENV_W_LEN {
            ENV_W[k - shift] = ENV_W[k];
        }
        ENV_W_LEN -= shift;
    }
    if value.is_null() {
        return 1;
    }
    let nlen = wlen(name);
    let vlen = wlen(value);
    let need = nlen + 1 + vlen + 1;
    if ENV_W_LEN + need > ENV_W.len() {
        return 0;
    }
    let mut p = ENV_W_LEN - 1; // before the final block NUL
    for k in 0..nlen {
        ENV_W[p] = *name.add(k);
        p += 1;
    }
    ENV_W[p] = b'=' as u16;
    p += 1;
    for k in 0..vlen {
        ENV_W[p] = *value.add(k);
        p += 1;
    }
    ENV_W[p] = 0;
    p += 1;
    ENV_W[p] = 0;
    p += 1;
    ENV_W_LEN = p;
    1
}

/// `ExpandEnvironmentStringsW(src, dst, count)` — copy `src` to `dst`, replacing
/// `%VAR%` with its value. Returns chars written incl NUL. Unknown `%VAR%` is
/// left verbatim.
#[no_mangle]
pub unsafe extern "C" fn ExpandEnvironmentStringsW(src: *const u16, dst: *mut u16, count: u32) -> u32 {
    if src.is_null() {
        return 0;
    }
    ensure_env();
    let cap = count as usize;
    let mut di = 0usize;
    let mut si = 0usize;
    loop {
        let c = *src.add(si);
        if c == 0 {
            break;
        }
        if c == b'%' as u16 {
            let mut j = si + 1;
            while *src.add(j) != 0 && *src.add(j) != b'%' as u16 {
                j += 1;
            }
            if *src.add(j) == b'%' as u16 && j > si + 1 {
                let mut nm = [0u16; 64];
                let nlen = (j - si - 1).min(nm.len() - 1);
                for k in 0..nlen {
                    nm[k] = *src.add(si + 1 + k);
                }
                nm[nlen] = 0;
                if let Some((_, vstart, end)) = env_find(nm.as_ptr()) {
                    for k in vstart..(end - 1) {
                        if di < cap && !dst.is_null() {
                            *dst.add(di) = ENV_W[k];
                        }
                        di += 1;
                    }
                    si = j + 1;
                    continue;
                }
            }
        }
        if di < cap && !dst.is_null() {
            *dst.add(di) = c;
        }
        di += 1;
        si += 1;
    }
    if di < cap && !dst.is_null() {
        *dst.add(di) = 0;
    }
    (di + 1) as u32
}

/// `GetFullPathNameW(lpFileName, nBufferLength, lpBuffer, lpFilePart)` — our
/// paths are already canonical; copy through.
#[no_mangle]
pub unsafe extern "C" fn GetFullPathNameW(
    name: *const u16,
    len: u32,
    buffer: *mut u16,
    file_part: *mut *mut u16,
) -> u32 {
    if name.is_null() {
        return 0;
    }
    let n = wstrlen(name);
    if !file_part.is_null() {
        *file_part = core::ptr::null_mut();
    }
    if buffer.is_null() || (len as usize) < n + 1 {
        return (n + 1) as u32;
    }
    for i in 0..=n {
        *buffer.add(i) = *name.add(i);
    }
    n as u32
}

/// `GetLongPathNameW(lpszShortPath, lpszLongPath, cchBuffer)` — no 8.3
/// shortening; copy through.
#[no_mangle]
pub unsafe extern "C" fn GetLongPathNameW(short: *const u16, long: *mut u16, cch: u32) -> u32 {
    GetFullPathNameW(short, cch, long, core::ptr::null_mut())
}

/// `GetCurrentDirectoryW(nBufferLength, lpBuffer)` — `C:\`.
#[no_mangle]
pub unsafe extern "C" fn GetCurrentDirectoryW(len: u32, buffer: *mut u16) -> u32 {
    wcopy(buffer, len, &[b'C' as u16, b':' as u16, b'\\' as u16, 0])
}

/// `CreateFileW(...)` — wide path counterpart of `CreateFileA`; narrow to
/// ASCII and forward.
#[no_mangle]
pub unsafe extern "C" fn CreateFileW(
    name: *const u16,
    access: u32,
    share: u32,
    sec: *mut c_void,
    disp: u32,
    flags: u32,
    template: u64,
) -> u64 {
    if name.is_null() {
        return INVALID_HANDLE_VALUE;
    }
    let mut narrow = [0u8; 260];
    let n = wstrlen(name).min(259);
    for i in 0..n {
        narrow[i] = *name.add(i) as u8;
    }
    narrow[n] = 0;
    CreateFileA(narrow.as_ptr(), access, share, sec, disp, flags, template)
}

/// `GetFileAttributesW(lpFileName)` — report "not found" with the proper
/// last-error so a path-search caller (cmd probing PATH/PATHEXT) terminates.
#[no_mangle]
pub unsafe extern "C" fn GetFileAttributesW(_name: *const u16) -> u32 {
    SetLastError(ERROR_FILE_NOT_FOUND);
    INVALID_FILE_ATTRIBUTES
}

/// `GetFileInformationByHandle(hFile, lpFileInformation)` — unsupported; fail.
#[no_mangle]
pub extern "C" fn GetFileInformationByHandle(_file: u64, _info: *mut c_void) -> i32 {
    0
}

/// `FileTimeToSystemTime(lpFileTime, lpSystemTime)` — zero the SYSTEMTIME
/// (8×u16) and report success.
#[no_mangle]
pub unsafe extern "C" fn FileTimeToSystemTime(_ft: *const u64, st: *mut u16) -> i32 {
    if !st.is_null() {
        for i in 0..8 {
            *st.add(i) = 0;
        }
    }
    1
}

/// `FileTimeToLocalFileTime(lpFileTime, lpLocalFileTime)` — no time zone;
/// copy through.
#[no_mangle]
pub unsafe extern "C" fn FileTimeToLocalFileTime(ft: *const u64, lft: *mut u64) -> i32 {
    if !ft.is_null() && !lft.is_null() {
        *lft = *ft;
    }
    1
}

/// `GetTimeFormatW(...)` / `GetDateFormatW(...)` — write a fixed placeholder.
#[no_mangle]
pub unsafe extern "C" fn GetTimeFormatW(
    _locale: u32,
    _flags: u32,
    _time: *const u16,
    _fmt: *const u16,
    buffer: *mut u16,
    cch: i32,
) -> i32 {
    let s = [b'0' as u16, b'0' as u16, b':' as u16, b'0' as u16, b'0' as u16, 0];
    wcopy(buffer, cch.max(0) as u32, &s) as i32
}
#[no_mangle]
pub unsafe extern "C" fn GetDateFormatW(
    _locale: u32,
    _flags: u32,
    _date: *const u16,
    _fmt: *const u16,
    buffer: *mut u16,
    cch: i32,
) -> i32 {
    let s = [b'2' as u16, b'0' as u16, b'2' as u16, b'4' as u16, 0];
    wcopy(buffer, cch.max(0) as u32, &s) as i32
}

/// `FindFirstFileExW(...)` — directory enumeration. We have no directory model
/// yet, so report "no matching files"; a search tool then prints its
/// not-found message. Returns INVALID_HANDLE_VALUE.
#[no_mangle]
pub unsafe extern "C" fn FindFirstFileExW(
    _name: *const u16,
    _info_level: i32,
    _find_data: *mut c_void,
    _search_op: i32,
    _filter: *mut c_void,
    _flags: u32,
) -> u64 {
    SetLastError(ERROR_FILE_NOT_FOUND);
    INVALID_HANDLE_VALUE
}

/// `FindFirstFileW(lpFileName, lpFindFileData)` — like the Ex variant: no
/// directory model, so report no match with INVALID_HANDLE_VALUE (not 0 — a
/// caller checks against INVALID_HANDLE_VALUE and would use a 0 handle as if
/// valid, e.g. cmd.exe path globbing).
#[no_mangle]
pub unsafe extern "C" fn FindFirstFileW(_name: *const u16, _find_data: *mut c_void) -> u64 {
    SetLastError(ERROR_FILE_NOT_FOUND);
    INVALID_HANDLE_VALUE
}

/// `FindNextFileW(hFindFile, lpFindFileData)` — no more files.
#[no_mangle]
pub unsafe extern "C" fn FindNextFileW(_handle: u64, _find_data: *mut c_void) -> i32 {
    SetLastError(ERROR_NO_MORE_FILES);
    0
}

/// `FindClose(hFindFile)` — accept.
#[no_mangle]
pub extern "C" fn FindClose(_handle: u64) -> i32 {
    1
}

/// `StrTrimW(psz, pszTrimChars)` (SHLWAPI) — trim leading/trailing chars in
/// `trim` from `psz` in place. Returns nonzero if anything was trimmed.
#[no_mangle]
pub unsafe extern "C" fn StrTrimW(psz: *mut u16, trim: *const u16) -> i32 {
    if psz.is_null() || trim.is_null() {
        return 0;
    }
    let in_set = |c: u16| {
        let mut i = 0;
        while *trim.add(i) != 0 {
            if *trim.add(i) == c {
                return true;
            }
            i += 1;
        }
        false
    };
    let len = wstrlen(psz);
    let mut start = 0;
    while start < len && in_set(*psz.add(start)) {
        start += 1;
    }
    let mut end = len;
    while end > start && in_set(*psz.add(end - 1)) {
        end -= 1;
    }
    let trimmed = start != 0 || end != len;
    if start != 0 {
        let mut i = 0;
        while start + i < end {
            *psz.add(i) = *psz.add(start + i);
            i += 1;
        }
    }
    *psz.add(end - start) = 0;
    trimmed as i32
}

// ===========================================================================
// Startup / synchronization surface for ucrt-linked binaries (cmd.exe).
// Critical sections and SRW locks are no-ops: a single shim instance has no
// real preemption hazard they guard here, and there is no blocking primitive
// wired yet. They must only leave their opaque storage uncorrupted.
// ===========================================================================

#[no_mangle]
pub extern "C" fn InitializeCriticalSection(_cs: *mut c_void) {}
#[no_mangle]
pub extern "C" fn InitializeCriticalSectionEx(_cs: *mut c_void, _spin: u32, _flags: u32) -> i32 {
    1
}
#[no_mangle]
pub extern "C" fn InitializeCriticalSectionAndSpinCount(_cs: *mut c_void, _spin: u32) -> i32 {
    1
}
#[no_mangle]
pub extern "C" fn EnterCriticalSection(_cs: *mut c_void) {}
#[no_mangle]
pub extern "C" fn LeaveCriticalSection(_cs: *mut c_void) {}
#[no_mangle]
pub extern "C" fn DeleteCriticalSection(_cs: *mut c_void) {}

#[no_mangle]
pub extern "C" fn InitializeSRWLock(_l: *mut c_void) {}
#[no_mangle]
pub extern "C" fn AcquireSRWLockExclusive(_l: *mut c_void) {}
#[no_mangle]
pub extern "C" fn AcquireSRWLockShared(_l: *mut c_void) {}
#[no_mangle]
pub extern "C" fn ReleaseSRWLockExclusive(_l: *mut c_void) {}
#[no_mangle]
pub extern "C" fn ReleaseSRWLockShared(_l: *mut c_void) {}
#[no_mangle]
pub extern "C" fn TryAcquireSRWLockExclusive(_l: *mut c_void) -> u8 {
    1
}

/// `InitOnceBeginInitialize` — report "caller does the init" (pending = TRUE)
/// so the one-time init runs synchronously (we are effectively single-threaded
/// per process for this).
#[no_mangle]
pub unsafe extern "C" fn InitOnceBeginInitialize(
    _once: *mut c_void,
    _flags: u32,
    pending: *mut i32,
    context: *mut *mut c_void,
) -> i32 {
    if !pending.is_null() {
        *pending = 1;
    }
    if !context.is_null() {
        *context = core::ptr::null_mut();
    }
    1
}
#[no_mangle]
pub extern "C" fn InitOnceComplete(_once: *mut c_void, _flags: u32, _context: *mut c_void) -> i32 {
    1
}

#[no_mangle]
pub extern "C" fn InitializeSListHead(_head: *mut c_void) {}

/// `IsProcessorFeaturePresent` — report no optional CPU feature present (FALSE).
#[no_mangle]
pub extern "C" fn IsProcessorFeaturePresent(_feature: u32) -> i32 {
    0
}

/// `GetStartupInfoW(lpStartupInfo)` — zero the `STARTUPINFOW` and set its `cb`.
/// An all-zero struct (no inherited handles, no flags) makes the CRT use
/// console defaults.
#[no_mangle]
pub unsafe extern "C" fn GetStartupInfoW(info: *mut u8) {
    if !info.is_null() {
        core::ptr::write_bytes(info, 0, 104); // sizeof(STARTUPINFOW)
        *(info as *mut u32) = 104; // cb
    }
}

/// `GetACP()` — OEM US (437). (Output paths narrow Unicode to Latin-1 anyway.)
#[no_mangle]
pub extern "C" fn GetACP() -> u32 {
    437
}

/// `GlobalAlloc(uFlags, dwBytes)` — back the legacy global heap with our process
/// heap. `GMEM_ZEROINIT (0x40)` zeroes the block. Returns the block (we use the
/// pointer as the HGLOBAL — no movable-handle indirection).
#[no_mangle]
pub unsafe extern "C" fn GlobalAlloc(flags: u32, bytes: u64) -> *mut u8 {
    let p = HeapAlloc(1, 0, bytes);
    if flags & 0x40 != 0 && !p.is_null() {
        core::ptr::write_bytes(p, 0, bytes as usize);
    }
    p
}
#[no_mangle]
pub unsafe extern "C" fn GlobalFree(mem: *mut u8) -> *mut u8 {
    if !mem.is_null() {
        HeapFree(1, 0, mem);
    }
    core::ptr::null_mut()
}

// ETW tracing: no backend; report ERROR_SUCCESS so registration/writes are no-ops.
#[no_mangle]
pub extern "C" fn EventRegister(
    _provider: *const c_void,
    _cb: *const c_void,
    _ctx: *const c_void,
    handle: *mut u64,
) -> u32 {
    if !handle.is_null() {
        unsafe { *handle = 0 };
    }
    0
}
#[no_mangle]
pub extern "C" fn EventUnregister(_handle: u64) -> u32 {
    0
}
#[no_mangle]
pub extern "C" fn EventSetInformation(
    _handle: u64,
    _class: u32,
    _info: *const c_void,
    _len: u32,
) -> u32 {
    0
}
#[no_mangle]
pub extern "C" fn EventWriteTransfer(
    _handle: u64,
    _desc: *const c_void,
    _activity: *const c_void,
    _related: *const c_void,
    _count: u32,
    _data: *const c_void,
) -> u32 {
    0
}

#[no_mangle]
pub extern "C" fn DebugBreak() {}

#[no_mangle]
pub unsafe extern "C" fn OutputDebugStringW(s: *const u16) {
    if s.is_null() {
        return;
    }
    let mut buf = [0u8; 256];
    let mut i = 0;
    while i < buf.len() - 1 {
        let c = *s.add(i);
        if c == 0 {
            break;
        }
        buf[i] = c as u8;
        i += 1;
    }
    syscall3(NT_DEBUG_WRITE, buf.as_ptr() as u64, i as u64, 0);
}

/// `GetModuleHandleExW(flags, name, *module)` — hand back the calling image base
/// for the (common) NULL-name "this module" query. Returns TRUE.
#[no_mangle]
pub unsafe extern "C" fn GetModuleHandleExW(
    _flags: u32,
    _name: *const u16,
    module: *mut u64,
) -> i32 {
    if !module.is_null() {
        *module = peb_image_base();
    }
    1
}

#[no_mangle]
pub extern "C" fn GetConsoleWindow() -> u64 {
    0
}

/// `CompareStringOrdinal(s1, c1, s2, c2, ignoreCase)` — ordinal (code-unit) wide
/// comparison. Returns CSTR_LESS_THAN(1)/EQUAL(2)/GREATER_THAN(3). `c < 0` means
/// NUL-terminated. cmd.exe sorts/searches with this; a wrong result spins it.
#[no_mangle]
pub unsafe extern "C" fn CompareStringOrdinal(
    s1: *const u16,
    c1: i32,
    s2: *const u16,
    c2: i32,
    ignore_case: i32,
) -> i32 {
    let n1 = if c1 < 0 { wlen(s1) } else { c1 as usize };
    let n2 = if c2 < 0 { wlen(s2) } else { c2 as usize };
    let lc = |c: u16| {
        if ignore_case != 0 && (b'A' as u16..=b'Z' as u16).contains(&c) {
            c + 32
        } else {
            c
        }
    };
    let mut i = 0;
    loop {
        if i == n1 || i == n2 {
            return if n1 == n2 {
                2 // CSTR_EQUAL
            } else if n1 < n2 {
                1 // CSTR_LESS_THAN
            } else {
                3 // CSTR_GREATER_THAN
            };
        }
        let (a, b) = (lc(*s1.add(i)), lc(*s2.add(i)));
        if a != b {
            return if a < b { 1 } else { 3 };
        }
        i += 1;
    }
}

/// `lstrcmpW`/`lstrcmpiW` — wide string compare (case-sensitive / insensitive).
#[no_mangle]
pub unsafe extern "C" fn lstrcmpW(a: *const u16, b: *const u16) -> i32 {
    let mut i = 0;
    loop {
        let (x, y) = (*a.add(i), *b.add(i));
        if x != y {
            return x as i32 - y as i32;
        }
        if x == 0 {
            return 0;
        }
        i += 1;
    }
}
#[no_mangle]
pub unsafe extern "C" fn lstrcmpiW(a: *const u16, b: *const u16) -> i32 {
    let lc = |c: u16| if (b'A' as u16..=b'Z' as u16).contains(&c) { c + 32 } else { c };
    let mut i = 0;
    loop {
        let (x, y) = (lc(*a.add(i)), lc(*b.add(i)));
        if x != y {
            return x as i32 - y as i32;
        }
        if x == 0 {
            return 0;
        }
        i += 1;
    }
}

// ===========================================================================
// Registry (advapi32/kernelbase Reg* surface) over the kernel Configuration
// Manager (syscalls 22-26). Error codes are Win32 LSTATUS (ERROR_SUCCESS=0).
// ===========================================================================
const REG_OK: i32 = 0; // ERROR_SUCCESS (the u32 ERROR_SUCCESS const exists elsewhere)
const ERROR_FILE_NOT_FOUND_W: i32 = 2;
const ERROR_MORE_DATA: i32 = 234;
const ERROR_NO_MORE_ITEMS: i32 = 259;

unsafe fn wlen(s: *const u16) -> usize {
    if s.is_null() {
        return 0;
    }
    let mut n = 0;
    while *s.add(n) != 0 {
        n += 1;
    }
    n
}

#[no_mangle]
pub unsafe extern "C" fn RegOpenKeyExW(
    hkey: u64,
    subkey: *const u16,
    _options: u32,
    _sam: u32,
    result: *mut u64,
) -> i32 {
    let len = wlen(subkey);
    let h = syscall3(NT_REG_OPEN_KEY, hkey, subkey as u64, len as u64);
    if h == 0 {
        return ERROR_FILE_NOT_FOUND_W;
    }
    if !result.is_null() {
        *result = h;
    }
    REG_OK
}

#[no_mangle]
pub unsafe extern "C" fn RegCreateKeyExW(
    hkey: u64,
    subkey: *const u16,
    _reserved: u32,
    _class: *const u16,
    _options: u32,
    _sam: u32,
    _sa: *const c_void,
    result: *mut u64,
    disposition: *mut u32,
) -> i32 {
    let len = wlen(subkey);
    let h = syscall3(NT_REG_CREATE_KEY, hkey, subkey as u64, len as u64);
    if h == 0 {
        return ERROR_FILE_NOT_FOUND_W;
    }
    if !result.is_null() {
        *result = h;
    }
    if !disposition.is_null() {
        *disposition = 1; // REG_CREATED_NEW_KEY (we don't distinguish reuse)
    }
    REG_OK
}

#[no_mangle]
pub extern "C" fn RegCloseKey(_hkey: u64) -> i32 {
    REG_OK // keys live for the session; nothing to release
}

/// Shared query helper: fills type/data/size; `cap_bytes` in, true byte length
/// out. Returns the Win32 status.
unsafe fn reg_query(
    hkey: u64,
    name: *const u16,
    lp_type: *mut u32,
    lp_data: *mut u8,
    lpcb: *mut u32,
) -> i32 {
    let nlen = wlen(name);
    let cap = if lpcb.is_null() { 0u64 } else { *lpcb as u64 };
    let mut vtype: u32 = 0;
    let req: [u64; 5] = [
        name as u64,
        nlen as u64,
        (&raw mut vtype) as u64,
        lp_data as u64,
        cap,
    ];
    let n = syscall3(NT_REG_QUERY_VALUE, hkey, req.as_ptr() as u64, 0) as i64;
    if n < 0 {
        return ERROR_FILE_NOT_FOUND_W;
    }
    if !lp_type.is_null() {
        *lp_type = vtype;
    }
    if !lpcb.is_null() {
        *lpcb = n as u32;
    }
    if (n as u64) > cap && !lp_data.is_null() {
        return ERROR_MORE_DATA;
    }
    REG_OK
}

#[no_mangle]
pub unsafe extern "C" fn RegQueryValueExW(
    hkey: u64,
    value_name: *const u16,
    _reserved: *mut u32,
    lp_type: *mut u32,
    lp_data: *mut u8,
    lpcb: *mut u32,
) -> i32 {
    reg_query(hkey, value_name, lp_type, lp_data, lpcb)
}

#[no_mangle]
pub unsafe extern "C" fn RegGetValueW(
    hkey: u64,
    subkey: *const u16,
    value: *const u16,
    _flags: u32,
    pdw_type: *mut u32,
    pv_data: *mut u8,
    pcb_data: *mut u32,
) -> i32 {
    // Optionally open a subkey first, then read the value from it.
    let mut k = hkey;
    if !subkey.is_null() && *subkey != 0 {
        let len = wlen(subkey);
        k = syscall3(NT_REG_OPEN_KEY, hkey, subkey as u64, len as u64);
        if k == 0 {
            return ERROR_FILE_NOT_FOUND_W;
        }
    }
    reg_query(k, value, pdw_type, pv_data, pcb_data)
}

#[no_mangle]
pub unsafe extern "C" fn RegSetValueExW(
    hkey: u64,
    value_name: *const u16,
    _reserved: u32,
    vtype: u32,
    data: *const u8,
    cb_data: u32,
) -> i32 {
    let nlen = wlen(value_name);
    let req: [u64; 5] = [
        value_name as u64,
        nlen as u64,
        vtype as u64,
        data as u64,
        cb_data as u64,
    ];
    let r = syscall3(NT_REG_SET_VALUE, hkey, req.as_ptr() as u64, 0);
    if r == u64::MAX {
        ERROR_FILE_NOT_FOUND_W
    } else {
        REG_OK
    }
}

#[no_mangle]
pub unsafe extern "C" fn RegEnumKeyExW(
    hkey: u64,
    index: u32,
    name: *mut u16,
    lpcch_name: *mut u32,
    _reserved: *mut u32,
    _class: *mut u16,
    _lpcch_class: *mut u32,
    _filetime: *mut c_void,
) -> i32 {
    let cap = if lpcch_name.is_null() { 0u64 } else { *lpcch_name as u64 };
    let n = syscall4(NT_REG_ENUM_KEY, hkey, index as u64, name as u64, cap) as i64;
    if n < 0 {
        return ERROR_NO_MORE_ITEMS;
    }
    // NUL-terminate and report the char count (excluding NUL), per the API.
    if !name.is_null() && (n as u64) < cap {
        *name.add(n as usize) = 0;
    }
    if !lpcch_name.is_null() {
        *lpcch_name = n as u32;
    }
    REG_OK
}

/// `RegDeleteKeyExW`/`RegDeleteValueW` — not yet backed by the CM; report
/// success so callers proceed (deletion is rare on a CLI startup path).
#[no_mangle]
pub extern "C" fn RegDeleteKeyExW(_hkey: u64, _subkey: *const u16, _sam: u32, _reserved: u32) -> i32 {
    REG_OK
}
#[no_mangle]
pub extern "C" fn RegDeleteValueW(_hkey: u64, _value: *const u16) -> i32 {
    REG_OK
}

/// Generic fallback for imports we can't resolve precisely — notably
/// **by-ordinal** imports (e.g. a `WS2_32` ordinal a tool links but doesn't
/// truly depend on). Returns 0, which most `BOOL`/`int`/`HRESULT`-returning
/// APIs read as "no error / nothing". Lets a binary load and run; a tool that
/// genuinely needs the routine will misbehave (documented).
#[no_mangle]
pub extern "C" fn __ordinal_stub() -> u64 {
    0
}

/// DLL entry point — the loader doesn't call it, but lld-link wants one.
#[no_mangle]
pub extern "C" fn DllMain(_module: u64, _reason: u32, _reserved: u64) -> i32 {
    1 // TRUE
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    loop {
        core::hint::spin_loop();
    }
}

/// MSVC floating-point CRT marker (referenced by core under `/nodefaultlib`).
#[no_mangle]
pub static _fltused: i32 = 0;
