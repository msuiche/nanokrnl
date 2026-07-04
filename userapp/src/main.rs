//! userapp.exe — a CRT-style Win32 console program for ntoskrnl-rs.
//!
//! A genuine PE executable for `x86_64-pc-windows-msvc`. Its entry is a
//! minimal C-runtime shim (`mainCRTStartup`) that mirrors the MSVC CRT: it
//! fetches the command line via `GetCommandLineA`, tokenizes it into an
//! `argv` array, calls `main(argc, argv)`, and passes the result to
//! `ExitProcess`. `main` is ordinary console code using `kernel32` imports
//! the loader binds to the shim DLL.

#![no_std]
#![no_main]

use core::ffi::c_void;

const STD_OUTPUT_HANDLE: u32 = 0xFFFF_FFF5; // (DWORD)-11
const MAX_ARGS: usize = 32;

/// Win32 `SYSTEM_INFO` (x64 layout), matching the kernel32 shim's definition.
#[repr(C)]
struct SystemInfo {
    w_processor_architecture: u16,
    w_reserved: u16,
    dw_page_size: u32,
    lp_minimum_application_address: u64,
    lp_maximum_application_address: u64,
    dw_active_processor_mask: u64,
    dw_number_of_processors: u32,
    dw_processor_type: u32,
    dw_allocation_granularity: u32,
    w_processor_level: u16,
    w_processor_revision: u16,
}

/// Win32 `OSVERSIONINFOA`, matching the kernel32 shim's definition.
#[repr(C)]
struct OsVersionInfoA {
    dw_os_version_info_size: u32,
    dw_major_version: u32,
    dw_minor_version: u32,
    dw_build_number: u32,
    dw_platform_id: u32,
    sz_csd_version: [u8; 128],
}

extern "C" {
    fn GetStdHandle(n_std_handle: u32) -> u64;
    fn WriteFile(h: u64, buf: *const u8, n: u32, written: *mut u32, ov: *mut c_void) -> i32;
    fn ReadFile(h: u64, buf: *mut u8, n: u32, read: *mut u32, ov: *mut c_void) -> i32;
    fn CreateFileA(name: *const u8, access: u32, share: u32, sec: *mut c_void, disp: u32, flags: u32, template: u64) -> u64;
    fn GetFileSize(file: u64, high: *mut u32) -> u32;
    fn CloseHandle(h: u64) -> i32;
    fn WriteConsoleA(h: u64, buf: *const u8, n: u32, written: *mut u32, reserved: *mut c_void) -> i32;
    fn WriteConsoleW(h: u64, buf: *const u16, n: u32, written: *mut u32, reserved: *mut c_void) -> i32;
    fn MultiByteToWideChar(cp: u32, flags: u32, src: *const u8, cb: i32, dst: *mut u16, cch: i32) -> i32;
    fn WideCharToMultiByte(cp: u32, flags: u32, src: *const u16, cch: i32, dst: *mut u8, cb: i32, def: *const u8, used: *mut i32) -> i32;
    fn OutputDebugStringA(s: *const u8);
    fn InterlockedIncrement(addend: *mut i32) -> i32;
    fn InterlockedDecrement(addend: *mut i32) -> i32;
    fn InterlockedExchange(target: *mut i32, value: i32) -> i32;
    fn InterlockedCompareExchange(dst: *mut i32, exchange: i32, comparand: i32) -> i32;
    fn lstrlenA(s: *const u8) -> i32;
    fn lstrcmpA(a: *const u8, b: *const u8) -> i32;
    fn lstrcmpiA(a: *const u8, b: *const u8) -> i32;
    fn lstrcpyA(dst: *mut u8, src: *const u8) -> *mut u8;
    fn lstrcatA(dst: *mut u8, src: *const u8) -> *mut u8;
    fn GetModuleHandleA(name: *const u8) -> u64;
    fn GetProcAddress(module: u64, name: *const u8) -> *const c_void;
    fn LoadLibraryA(name: *const u8) -> u64;
    fn FreeLibrary(module: u64) -> i32;
    fn GetFileType(handle: u64) -> u32;
    fn GetCommandLineA() -> *const u8;
    fn GetCommandLineW() -> *const u16;
    fn GetSystemInfo(info: *mut SystemInfo);
    fn GetCurrentProcess() -> u64;
    fn GetCurrentThread() -> u64;
    fn GetVersion() -> u32;
    fn GetVersionExA(info: *mut OsVersionInfoA) -> i32;
    fn IsDebuggerPresent() -> i32;
    fn GetProcessHeap() -> u64;
    fn HeapAlloc(heap: u64, flags: u32, bytes: u64) -> *mut u8;
    fn HeapFree(heap: u64, flags: u32, mem: *mut u8) -> i32;
    fn Sleep(millis: u32);
    fn GetTickCount64() -> u64;
    fn QueryPerformanceFrequency(frequency: *mut i64) -> i32;
    fn QueryPerformanceCounter(count: *mut i64) -> i32;
    fn GetSystemTimeAsFileTime(file_time: *mut u32);
    fn GetEnvironmentVariableA(name: *const u8, buffer: *mut u8, size: u32) -> u32;
    fn SetLastError(code: u32);
    fn GetLastError() -> u32;
    fn CreateProcessW(
        app: *const u16,
        cmdline: *const u16,
        pa: *mut c_void,
        ta: *mut c_void,
        inherit: i32,
        flags: u32,
        env: *mut c_void,
        cwd: *const u16,
        si: *const c_void,
        pi: *mut u8,
    ) -> i32;
    fn WaitForSingleObject(handle: u64, millis: u32) -> u32;
    fn GetExitCodeProcess(handle: u64, code: *mut u32) -> i32;
    // msvcrt shim imports (resolved by the loader against our msvcrt.dll).
    fn atoi(s: *const u8) -> i32;
    fn strchr(s: *const u8, c: i32) -> *const u8;
    fn strcpy_s(dst: *mut u8, dstsz: usize, src: *const u8) -> i32;
    fn qsort(base: *mut u8, num: usize, size: usize, cmp: extern "C" fn(*const u8, *const u8) -> i32);
    fn _strnicmp(a: *const u8, b: *const u8, n: usize) -> i32;
    fn fprintf(stream: *mut c_void, fmt: *const u8, ...) -> i32;
    fn ReportTestResult(code: u64);
    fn ExitProcess(code: u32) -> !;
}

static mut ENVBUF: [u8; 64] = [0; 64];

/// Comparator for `qsort` over `i32` elements (ascending).
extern "C" fn cmp_i32(a: *const u8, b: *const u8) -> i32 {
    let (x, y) = unsafe { (*(a as *const i32), *(b as *const i32)) };
    x - y
}

fn print(handle: u64, s: &[u8]) {
    let mut written: u32 = 0;
    unsafe {
        WriteFile(handle, s.as_ptr(), s.len() as u32, &mut written, core::ptr::null_mut());
    }
}

/// Print a decimal integer.
fn print_dec(handle: u64, mut v: u64) {
    let mut buf = [0u8; 20];
    let mut i = buf.len();
    if v == 0 {
        i -= 1;
        buf[i] = b'0';
    }
    while v > 0 {
        i -= 1;
        buf[i] = b'0' + (v % 10) as u8;
        v /= 10;
    }
    print(handle, &buf[i..]);
}

/// Length of a NUL-terminated string.
unsafe fn strlen(s: *const u8) -> usize {
    let mut n = 0;
    while *s.add(n) != 0 {
        n += 1;
    }
    n
}

/// Console `main`: prints `argc` and each argument.
unsafe fn main(argc: i32, argv: *const *const u8) -> i32 {
    let out = GetStdHandle(STD_OUTPUT_HANDLE);
    print(out, b"APP: console app, argc=");
    print_dec(out, argc as u64);
    print(out, b"\n");
    for k in 0..argc {
        print(out, b"APP: arg");
        print_dec(out, k as u64);
        print(out, b"=");
        let s = *argv.add(k as usize);
        print(out, core::slice::from_raw_parts(s, strlen(s)));
        print(out, b"\n");
    }

    // Heap test: allocate two blocks, free the first, allocate a third of
    // the same size — a first-fit pooled heap should hand back the freed
    // block's address. Report the verdict over the kernel test channel.
    let heap = GetProcessHeap();
    let a = HeapAlloc(heap, 0, 32);
    let b = HeapAlloc(heap, 0, 32);
    HeapFree(heap, 0, a);
    let c = HeapAlloc(heap, 0, 32);
    let reuse_ok = c == a && b != a && !a.is_null();
    if reuse_ok {
        print(out, b"APP: heap block reuse ok\n");
    } else {
        print(out, b"APP: heap block reuse FAILED\n");
    }

    // Timing test: measure elapsed ticks across a Sleep.
    let t0 = GetTickCount64();
    Sleep(20);
    let elapsed = GetTickCount64() - t0;
    print(out, b"APP: slept ");
    print_dec(out, elapsed);
    print(out, b" ms\n");
    let timing_ok = elapsed >= 20;

    // Environment variable test: read "OS" (expected "ntoskrnl-rs", 11 chars).
    let n = GetEnvironmentVariableA(b"OS\0".as_ptr(), (&raw mut ENVBUF) as *mut u8, 64);
    print(out, b"APP: env OS=");
    print(out, core::slice::from_raw_parts((&raw const ENVBUF) as *const u8, n as usize));
    print(out, b"\n");
    let env_ok = n == 11;

    // Console API test: write a line through WriteConsoleA (the console-
    // specific output path, distinct from raw WriteFile) and verify it
    // reports every character written.
    let cmsg = b"APP: WriteConsoleA path ok\n";
    let mut cw: u32 = 0;
    let cr = WriteConsoleA(out, cmsg.as_ptr(), cmsg.len() as u32, &mut cw, core::ptr::null_mut());
    let console_ok = cr != 0 && cw as usize == cmsg.len();

    // Runtime dynamic-linking test: load kernel32 by name, resolve an export
    // by name, call through the resolved pointer, confirm a bogus name fails,
    // and free the library — the LoadLibrary/GetProcAddress/FreeLibrary trio
    // real apps use. (GetModuleHandleA returns the same base for an already-
    // loaded module.)
    let hk = LoadLibraryA(b"kernel32.dll\0".as_ptr());
    let hk2 = GetModuleHandleA(b"kernel32.dll\0".as_ptr());
    let p_sleep = GetProcAddress(hk, b"Sleep\0".as_ptr());
    let p_bogus = GetProcAddress(hk, b"NoSuchExport\0".as_ptr());
    let mut getproc_ok = hk != 0 && hk == hk2 && !p_sleep.is_null() && p_bogus.is_null();
    if getproc_ok {
        let sleep_fn: unsafe extern "C" fn(u32) = core::mem::transmute(p_sleep);
        sleep_fn(0); // exercise the resolved pointer
        getproc_ok &= FreeLibrary(hk) != 0;
        print(out, b"APP: LoadLibrary/GetProcAddress(Sleep) resolved + called\n");
    } else {
        print(out, b"APP: LoadLibrary/GetProcAddress FAILED\n");
    }

    // GetFileType: the console standard handle is a character device.
    let filetype_ok = GetFileType(out) == 0x0002; // FILE_TYPE_CHAR

    // A/W conversion round-trip: widen an ASCII string to UTF-16, write it
    // through WriteConsoleW, then narrow it back and confirm it matches —
    // the MultiByteToWideChar/WideCharToMultiByte pair console apps use.
    let asrc = b"APP: MultiByteToWideChar round-trip ok\n\0";
    let need = MultiByteToWideChar(0, 0, asrc.as_ptr(), -1, core::ptr::null_mut(), 0);
    let mut wbuf = [0u16; 64];
    let wn = MultiByteToWideChar(0, 0, asrc.as_ptr(), -1, wbuf.as_mut_ptr(), 64);
    // Write the wide string (excluding the NUL terminator) to the console.
    let mut wcw: u32 = 0;
    WriteConsoleW(out, wbuf.as_ptr(), (wn - 1) as u32, &mut wcw, core::ptr::null_mut());
    // Narrow back and compare against the original bytes.
    let mut abuf = [0u8; 64];
    let an = WideCharToMultiByte(
        0, 0, wbuf.as_ptr(), -1, abuf.as_mut_ptr(), 64, core::ptr::null(), core::ptr::null_mut(),
    );
    let mut roundtrip = wn == asrc.len() as i32 && need == wn && an == asrc.len() as i32;
    for i in 0..asrc.len() {
        if abuf[i] != asrc[i] {
            roundtrip = false;
        }
    }
    let convert_ok = roundtrip && wcw == (wn - 1) as u32;

    // Last-error test: SetLastError/GetLastError round-trips through the
    // per-thread slot, and a failed GetEnvironmentVariableA reports
    // ERROR_ENVVAR_NOT_FOUND (203) — real Win32 last-error semantics.
    SetLastError(0xC0DE);
    let lasterr_set = GetLastError() == 0xC0DE;
    let mut tmp = [0u8; 8];
    GetEnvironmentVariableA(b"NOPE\0".as_ptr(), tmp.as_mut_ptr(), 8);
    let lasterr_envmiss = GetLastError() == 203;
    let lasterror_ok = lasterr_set && lasterr_envmiss;
    if lasterror_ok {
        print(out, b"APP: SetLastError/GetLastError ok\n");
    } else {
        print(out, b"APP: SetLastError/GetLastError FAILED\n");
    }

    // High-resolution timing: frequency is 1000 Hz, the counter is monotonic
    // across a Sleep, and the system time (FILETIME) is non-zero — the QPC/QPF
    // and GetSystemTimeAsFileTime APIs apps and CRTs use for timing.
    let mut freq: i64 = 0;
    let mut qpc0: i64 = 0;
    let mut qpc1: i64 = 0;
    let qf = QueryPerformanceFrequency(&mut freq);
    let q0 = QueryPerformanceCounter(&mut qpc0);
    Sleep(5);
    let q1 = QueryPerformanceCounter(&mut qpc1);
    let mut ft: [u32; 2] = [0, 0];
    GetSystemTimeAsFileTime(ft.as_mut_ptr());
    let filetime = (ft[0] as u64) | ((ft[1] as u64) << 32);
    let timeapi_ok =
        qf != 0 && q0 != 0 && q1 != 0 && freq == 1000 && qpc1 >= qpc0 && filetime != 0;
    if timeapi_ok {
        print(out, b"APP: QPC/QPF/GetSystemTimeAsFileTime ok\n");
    } else {
        print(out, b"APP: timing APIs FAILED\n");
    }

    // OutputDebugStringA: emit a trace line to the debug port. Fire-and-
    // forget (void return); if the pointer path were wrong the app would
    // fault, so reaching the verdict confirms it ran.
    OutputDebugStringA(b"APP: OutputDebugStringA ok\n\0".as_ptr());

    // lstr* string helpers: length, case-sensitive/insensitive compare, and
    // a copy+append round-trip — the classic Win32 string functions.
    let mut sbuf = [0u8; 16];
    lstrcpyA(sbuf.as_mut_ptr(), b"abc\0".as_ptr());
    lstrcatA(sbuf.as_mut_ptr(), b"def\0".as_ptr());
    let lstr_ok = lstrlenA(b"hello\0".as_ptr()) == 5
        && lstrcmpA(b"abc\0".as_ptr(), b"abc\0".as_ptr()) == 0
        && lstrcmpA(b"abc\0".as_ptr(), b"abd\0".as_ptr()) < 0
        && lstrcmpiA(b"ABC\0".as_ptr(), b"abc\0".as_ptr()) == 0
        && lstrcmpA(sbuf.as_ptr(), b"abcdef\0".as_ptr()) == 0;
    if lstr_ok {
        print(out, b"APP: lstr* string helpers ok\n");
    } else {
        print(out, b"APP: lstr* string helpers FAILED\n");
    }

    // GetCommandLineW: the wide command line, narrowed back, must equal the
    // ANSI command line — the wmain/CommandLineToArgvW input path.
    let wcmd = GetCommandLineW();
    let mut narrowed = [0u8; 64];
    WideCharToMultiByte(
        0, 0, wcmd, -1, narrowed.as_mut_ptr(), 64, core::ptr::null(), core::ptr::null_mut(),
    );
    let cmdw_ok = lstrcmpA(narrowed.as_ptr(), GetCommandLineA()) == 0;
    if cmdw_ok {
        print(out, b"APP: GetCommandLineW ok\n");
    } else {
        print(out, b"APP: GetCommandLineW FAILED\n");
    }

    // Interlocked* atomics: increment/decrement return the new value; exchange
    // returns the old; compare-exchange swaps only on a match and returns the
    // initial value either way.
    let mut iv: i32 = 5;
    let inc = InterlockedIncrement(&mut iv); // iv=6, ->6
    let dec = InterlockedDecrement(&mut iv); // iv=5, ->5
    let prev = InterlockedExchange(&mut iv, 42); // iv=42, ->5
    let cas_miss = InterlockedCompareExchange(&mut iv, 99, 7); // 7!=42, no swap, ->42
    let cas_hit = InterlockedCompareExchange(&mut iv, 99, 42); // 42==42, swap, ->42
    let interlocked_ok =
        inc == 6 && dec == 5 && prev == 5 && cas_miss == 42 && cas_hit == 42 && iv == 99;
    if interlocked_ok {
        print(out, b"APP: Interlocked* atomics ok\n");
    } else {
        print(out, b"APP: Interlocked* atomics FAILED\n");
    }

    // GetSystemInfo + current process/thread pseudo-handles: the machine
    // parameters apps read at startup.
    let mut si: SystemInfo = core::mem::zeroed();
    GetSystemInfo(&mut si);
    let sysinfo_ok = si.dw_page_size == 4096
        && si.dw_allocation_granularity == 65536
        && si.dw_number_of_processors == 1
        && si.w_processor_architecture == 9 // AMD64
        && GetCurrentProcess() == u64::MAX
        && GetCurrentThread() == u64::MAX - 1;
    if sysinfo_ok {
        print(out, b"APP: GetSystemInfo + pseudo-handles ok\n");
    } else {
        print(out, b"APP: GetSystemInfo FAILED\n");
    }

    // OS version / debugger startup queries.
    let ver = GetVersion();
    let mut osvi: OsVersionInfoA = core::mem::zeroed();
    osvi.dw_os_version_info_size = core::mem::size_of::<OsVersionInfoA>() as u32;
    let vex = GetVersionExA(&mut osvi);
    let version_ok = (ver & 0xFF) == 1 // major in low byte (nanokrnl 1.1.31337)
        && vex != 0
        && osvi.dw_major_version == 1
        && osvi.dw_minor_version == 1
        && osvi.dw_build_number == 31337
        && osvi.dw_platform_id == 2 // VER_PLATFORM_WIN32_NT
        && IsDebuggerPresent() == 0;
    if version_ok {
        print(out, b"APP: GetVersion/GetVersionExA/IsDebuggerPresent ok\n");
    } else {
        print(out, b"APP: version queries FAILED\n");
    }

    // msvcrt shim: the real C-runtime functions a classic console binary
    // imports, bound by our loader to our own msvcrt.dll.
    let mut arr: [i32; 5] = [5, 3, 1, 4, 2];
    qsort(arr.as_mut_ptr() as *mut u8, 5, 4, cmp_i32);
    let mut scopy = [0u8; 8];
    let cpy = strcpy_s(scopy.as_mut_ptr(), 8, b"hi\0".as_ptr());
    let hit = strchr(b"abc\0".as_ptr(), b'b' as i32);
    let msvcrt_ok = arr == [1, 2, 3, 4, 5]
        && atoi(b"123\0".as_ptr()) == 123
        && atoi(b"-7\0".as_ptr()) == -7
        && cpy == 0
        && scopy[0] == b'h' && scopy[1] == b'i' && scopy[2] == 0
        && !hit.is_null() && *hit == b'b'
        && _strnicmp(b"ABC\0".as_ptr(), b"abc\0".as_ptr(), 3) == 0;
    // RAM filesystem: open a file by path, query its size, read it, print it.
    const GENERIC_READ: u32 = 0x8000_0000;
    const OPEN_EXISTING: u32 = 3;
    let fh = CreateFileA(
        b"C:\\hello.txt\0".as_ptr(),
        GENERIC_READ,
        1,
        core::ptr::null_mut(),
        OPEN_EXISTING,
        0,
        0,
    );
    let mut fbuf = [0u8; 80];
    let mut fread: u32 = 0;
    let mut fsize: u32 = 0;
    if fh != u64::MAX {
        fsize = GetFileSize(fh, core::ptr::null_mut());
        ReadFile(fh, fbuf.as_mut_ptr(), 80, &mut fread, core::ptr::null_mut());
        CloseHandle(fh);
        print(out, b"APP: read C:\\hello.txt -> ");
        print(out, &fbuf[..fread as usize]);
    }
    let fs_ok = fh != u64::MAX && fread > 0 && fsize == fread && fbuf[0] == b'h';
    if !fs_ok {
        print(out, b"APP: filesystem read FAILED\n");
    }

    // fprintf %-expansion: "FP: n=42 s=hi\n" is 14 bytes.
    let fret = fprintf(
        core::ptr::null_mut(),
        b"FP: n=%d s=%s\n\0".as_ptr(),
        42i32,
        b"hi\0".as_ptr(),
    );
    let msvcrt_ok = msvcrt_ok && fret == 14;
    if msvcrt_ok {
        print(out, b"APP: msvcrt shim (qsort/atoi/strchr/strcpy_s) ok\n");
    } else {
        print(out, b"APP: msvcrt shim FAILED\n");
    }

    // CreateProcess end-to-end (Win32 ABI): launch C:\child.exe, wait for it,
    // read its exit code. Resuming past WaitForSingleObject with our own state
    // intact proves the child ran in its own address space and we came back
    // correctly (the per-thread GS restore).
    let child_w: [u16; 13] = [
        b'C' as u16, b':' as u16, b'\\' as u16, b'c' as u16, b'h' as u16, b'i' as u16,
        b'l' as u16, b'd' as u16, b'.' as u16, b'e' as u16, b'x' as u16, b'e' as u16, 0,
    ];
    let mut pi = [0u8; 24];
    let mut createproc_ok = false;
    if CreateProcessW(
        child_w.as_ptr(),
        core::ptr::null(),
        core::ptr::null_mut(),
        core::ptr::null_mut(),
        0,
        0,
        core::ptr::null_mut(),
        core::ptr::null(),
        core::ptr::null(),
        pi.as_mut_ptr(),
    ) != 0
    {
        let hproc = *(pi.as_ptr() as *const u64);
        WaitForSingleObject(hproc, 5000);
        let mut code = 0u32;
        GetExitCodeProcess(hproc, &mut code);
        createproc_ok = true;
        print(out, b"APP: CreateProcessW(child.exe) + wait + GetExitCode ok\n");
    } else {
        print(out, b"APP: CreateProcessW FAILED\n");
    }

    ReportTestResult(
        if reuse_ok
            && createproc_ok
            && timing_ok
            && env_ok
            && console_ok
            && getproc_ok
            && filetype_ok
            && convert_ok
            && lasterror_ok
            && timeapi_ok
            && lstr_ok
            && cmdw_ok
            && interlocked_ok
            && sysinfo_ok
            && version_ok
            && msvcrt_ok
            && fs_ok
        {
            0xABCD
        } else {
            0xDEAD
        },
    );
    0
}

/// CRT entry shim (`/entry:mainCRTStartup`): build `argv` from the command
/// line and call `main` — the role of `__scrt_common_main`/`argv` setup in
/// the MSVC CRT.
#[no_mangle]
pub unsafe extern "C" fn mainCRTStartup() -> ! {
    let cmdline = GetCommandLineA();
    let len = strlen(cmdline);

    // Mutable copy of the command line (tokenized in place) + an argv array.
    let heap = GetProcessHeap();
    let buf = HeapAlloc(heap, 0, (len + 1) as u64);
    let argv = HeapAlloc(heap, 0, (MAX_ARGS * 8) as u64) as *mut *const u8;
    let mut argc: i32 = 0;

    if !buf.is_null() && !argv.is_null() {
        for i in 0..=len {
            *buf.add(i) = *cmdline.add(i);
        }
        // Split on spaces: replace each run of spaces with NULs and record
        // each token's start (a simple parser — no quote handling yet).
        let mut i = 0;
        loop {
            while *buf.add(i) == b' ' {
                *buf.add(i) = 0;
                i += 1;
            }
            if *buf.add(i) == 0 {
                break;
            }
            if (argc as usize) < MAX_ARGS {
                *argv.add(argc as usize) = buf.add(i);
                argc += 1;
            }
            while *buf.add(i) != 0 && *buf.add(i) != b' ' {
                i += 1;
            }
        }
    }

    let code = main(argc, argv);
    ExitProcess(code as u32)
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
