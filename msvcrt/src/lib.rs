//! msvcrt.dll — a minimal classic C-runtime shim for ntoskrnl-rs.
//!
//! Goal: let a small *real* Windows console binary that links the legacy
//! `msvcrt.dll` (e.g. `sort.exe`) bind its CRT imports against our own
//! implementation, so the real `.exe` runs on our kernel without dragging in
//! the modern API-set / `KERNELBASE` / `ucrtbase` chain. We **substitute**
//! our msvcrt (matching the exported names) rather than loading Microsoft's.
//!
//! Like the `kernel32` shim, this DLL has no imports of its own — the few
//! functions that need kernel services issue `syscall` inline. The pure
//! string/memory/sort helpers are self-contained.
//!
//! Fidelity notes: the locale-aware compares assume the "C" locale (plain
//! byte/word ordering, ASCII case folding); `fprintf` writes its format
//! string without `%`-expansion (enough for the error-message paths); the
//! CRT-startup helpers provide a minimal argv/environment. These are honest
//! approximations, refined as real binaries exercise them.

#![no_std]

use core::ffi::c_void;

// System-service numbers (must match kernel `syscalls.rs`).
const NT_TERMINATE_THREAD: u32 = 0;
const NT_WRITE_FILE: u32 = 2;
const NT_CREATE_FILE: u32 = 3;
const NT_GET_COMMAND_LINE: u32 = 19;

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

/// Terminate the calling thread (our single-thread-per-process model).
unsafe fn terminate_self() -> ! {
    syscall3(NT_TERMINATE_THREAD, 0, 0, 0);
    loop {
        core::hint::spin_loop();
    }
}

/// Cached console handle for the stdio write path.
static mut CONSOLE_HANDLE: u64 = 0;

unsafe fn console_handle() -> u64 {
    if CONSOLE_HANDLE == 0 {
        let name = b"\\Device\\Console";
        CONSOLE_HANDLE = syscall3(NT_CREATE_FILE, name.as_ptr() as u64, name.len() as u64, 0);
    }
    CONSOLE_HANDLE
}

unsafe fn console_write(bytes: &[u8]) {
    let h = console_handle();
    syscall3(NT_WRITE_FILE, h, bytes.as_ptr() as u64, bytes.len() as u64);
}

// ---------------------------------------------------------------------------
// Pure memory / string helpers (self-contained, also serve as the compiler's
// `memcpy`/`memset` intrinsics in this freestanding DLL).
// ---------------------------------------------------------------------------

#[no_mangle]
pub unsafe extern "C" fn memcpy(dst: *mut u8, src: *const u8, n: usize) -> *mut u8 {
    let mut i = 0;
    while i < n {
        *dst.add(i) = *src.add(i);
        i += 1;
    }
    dst
}

#[no_mangle]
pub unsafe extern "C" fn memset(dst: *mut u8, c: i32, n: usize) -> *mut u8 {
    let b = c as u8;
    let mut i = 0;
    while i < n {
        *dst.add(i) = b;
        i += 1;
    }
    dst
}

// --- setjmp / longjmp ------------------------------------------------------
// cmd.exe uses setjmp/longjmp for its command loop (longjmp restarts the loop
// after each command / on error). It calls BOTH our `__intrinsic_setjmp` and
// `longjmp`, so they only need to agree on the buffer layout (the program never
// inspects the buffer). We save the GP non-volatiles + caller RSP + return
// address; that is sufficient for a C control-flow longjmp.
//
// Buffer layout (byte offsets): 0 rbx, 8 rbp, 16 rsi, 24 rdi, 32 r12, 40 r13,
// 48 r14, 56 r15, 64 rsp(after return), 72 rip(return address).

/// `__intrinsic_setjmp(jmpbuf, frame)` — save context, return 0. (`frame`, the
/// Win64 SEH frame pointer in rdx, is unused by our C-only longjmp.)
#[unsafe(naked)]
#[no_mangle]
pub unsafe extern "C" fn __intrinsic_setjmp() {
    core::arch::naked_asm!(
        "mov [rcx + 0], rbx",
        "mov [rcx + 8], rbp",
        "mov [rcx + 16], rsi",
        "mov [rcx + 24], rdi",
        "mov [rcx + 32], r12",
        "mov [rcx + 40], r13",
        "mov [rcx + 48], r14",
        "mov [rcx + 56], r15",
        "lea rax, [rsp + 8]", // caller's RSP (past our return address)
        "mov [rcx + 64], rax",
        "mov rax, [rsp]", // return address
        "mov [rcx + 72], rax",
        "xor eax, eax", // setjmp returns 0
        "ret",
    );
}

/// `_local_unwind(TargetFrame, TargetIp)` — the SEH local-unwind helper. The
/// real one (via RtlUnwindEx) runs intervening `__finally` blocks then transfers
/// control to `TargetIp` with RSP at the establisher frame; crucially it does
/// NOT return, so the compiler treats the code after the call as unreachable.
/// We do a longjmp-style transfer (skip the `__finally` cleanup): set RSP to the
/// target frame and jump. Correct for intra-function local unwinds where the
/// establisher frame is the target RSP; nonvolatiles are unchanged within one
/// function. (A full implementation would walk the scope table and run the
/// `__finally` handlers — deferred.)
#[unsafe(naked)]
#[no_mangle]
pub unsafe extern "C" fn _local_unwind() {
    core::arch::naked_asm!(
        "mov rsp, rcx", // TargetFrame (establisher frame == target RSP)
        "jmp rdx",      // TargetIp (resume point); never returns
    );
}

/// `longjmp(jmpbuf, val)` — restore context saved by `__intrinsic_setjmp` and
/// resume there, with the setjmp call appearing to return `val` (or 1 if 0).
#[unsafe(naked)]
#[no_mangle]
pub unsafe extern "C" fn longjmp() {
    core::arch::naked_asm!(
        "mov rbx, [rcx + 0]",
        "mov rbp, [rcx + 8]",
        "mov rsi, [rcx + 16]",
        "mov rdi, [rcx + 24]",
        "mov r12, [rcx + 32]",
        "mov r13, [rcx + 40]",
        "mov r14, [rcx + 48]",
        "mov r15, [rcx + 56]",
        "mov rax, rdx", // return value
        "test rax, rax",
        "jnz 2f",
        "mov eax, 1", // longjmp(buf, 0) makes setjmp return 1
        "2:",
        "mov rsp, [rcx + 64]", // restore stack
        "mov rdx, [rcx + 72]", // saved return address
        "jmp rdx",
    );
}

#[no_mangle]
pub unsafe extern "C" fn memcmp(a: *const u8, b: *const u8, n: usize) -> i32 {
    let mut i = 0;
    while i < n {
        let (x, y) = (*a.add(i), *b.add(i));
        if x != y {
            return x as i32 - y as i32;
        }
        i += 1;
    }
    0
}

#[no_mangle]
pub unsafe extern "C" fn memmove(dst: *mut u8, src: *const u8, n: usize) -> *mut u8 {
    if (dst as usize) < (src as usize) {
        let mut i = 0;
        while i < n {
            *dst.add(i) = *src.add(i);
            i += 1;
        }
    } else {
        let mut i = n;
        while i > 0 {
            i -= 1;
            *dst.add(i) = *src.add(i);
        }
    }
    dst
}

// --- C heap (malloc/free/calloc/realloc) -----------------------------------
// A simple arena over NtAllocateVirtualMemory, mirroring kernel32's heap: a
// 16-byte header stores the requested size (for realloc); a first-fit free list
// reuses freed blocks; the bump grows 64 KiB at a time. The CRT (ucrt) routes
// malloc/free here; Win32 code uses kernel32's separate HeapAlloc — programs
// don't mix the two, so two arenas is fine.
const NT_ALLOCATE_VIRTUAL_MEMORY: u32 = 6;
const HEAP_CHUNK: u64 = 0x10000;
static mut BUMP: u64 = 0;
static mut BUMP_END: u64 = 0;
#[repr(C)]
struct FreeBlock {
    size: u64,
    next: *mut FreeBlock,
}
static mut FREE_HEAD: *mut FreeBlock = core::ptr::null_mut();

#[inline]
fn align16(x: u64) -> u64 {
    (x + 15) & !15
}

#[no_mangle]
pub unsafe extern "C" fn malloc(size: usize) -> *mut u8 {
    let want = align16(size as u64 + 16); // 16-byte header holds the total size
    // First-fit reuse from the free list.
    let mut prev: *mut *mut FreeBlock = &raw mut FREE_HEAD;
    let mut cur = FREE_HEAD;
    while !cur.is_null() {
        if (*cur).size >= want {
            *prev = (*cur).next;
            // (*cur).size at [cur+0] already holds the block's total — keep it.
            return (cur as u64 + 16) as *mut u8;
        }
        prev = &raw mut (*cur).next;
        cur = (*cur).next;
    }
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
    *(hdr as *mut u64) = want; // total block size at [hdr+0]
    (hdr + 16) as *mut u8
}

#[no_mangle]
pub unsafe extern "C" fn free(p: *mut u8) {
    if p.is_null() {
        return;
    }
    // [hdr+0] holds the total block size and is never overwritten while live;
    // FreeBlock { size@0, next@8 } reuses it, so size stays valid.
    let hdr = (p as u64 - 16) as *mut FreeBlock;
    (*hdr).next = FREE_HEAD;
    FREE_HEAD = hdr;
}

#[no_mangle]
pub unsafe extern "C" fn calloc(count: usize, size: usize) -> *mut u8 {
    let n = count.wrapping_mul(size);
    let p = malloc(n);
    if !p.is_null() {
        core::ptr::write_bytes(p, 0, n);
    }
    p
}

#[no_mangle]
pub unsafe extern "C" fn realloc(p: *mut u8, size: usize) -> *mut u8 {
    if p.is_null() {
        return malloc(size);
    }
    // Old usable capacity = total block size minus the 16-byte header.
    let old_total = *((p as u64 - 16) as *const u64) as usize;
    let old_cap = old_total.saturating_sub(16);
    let np = malloc(size);
    if !np.is_null() {
        let n = core::cmp::min(old_cap, size);
        memcpy(np, p, n);
        free(p);
    }
    np
}

// --- wide-string helpers ---------------------------------------------------

#[no_mangle]
pub unsafe extern "C" fn wcslen(s: *const u16) -> usize {
    let mut n = 0;
    while *s.add(n) != 0 {
        n += 1;
    }
    n
}

#[no_mangle]
pub unsafe extern "C" fn wcscmp(a: *const u16, b: *const u16) -> i32 {
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
pub unsafe extern "C" fn wcsncmp(a: *const u16, b: *const u16, n: usize) -> i32 {
    let mut i = 0;
    while i < n {
        let (x, y) = (*a.add(i), *b.add(i));
        if x != y {
            return x as i32 - y as i32;
        }
        if x == 0 {
            return 0;
        }
        i += 1;
    }
    0
}

#[no_mangle]
pub unsafe extern "C" fn wcsspn(s: *const u16, set: *const u16) -> usize {
    let mut n = 0;
    'outer: loop {
        let c = *s.add(n);
        if c == 0 {
            break;
        }
        let mut j = 0;
        loop {
            let sc = *set.add(j);
            if sc == 0 {
                break 'outer;
            }
            if sc == c {
                break;
            }
            j += 1;
        }
        n += 1;
    }
    n
}

#[no_mangle]
pub unsafe extern "C" fn wcsstr(haystack: *const u16, needle: *const u16) -> *const u16 {
    if *needle == 0 {
        return haystack;
    }
    let mut i = 0;
    while *haystack.add(i) != 0 {
        let mut j = 0;
        while *needle.add(j) != 0 && *haystack.add(i + j) == *needle.add(j) {
            j += 1;
        }
        if *needle.add(j) == 0 {
            return haystack.add(i);
        }
        i += 1;
    }
    core::ptr::null()
}

/// `strchr(s, c)` — first occurrence of `c` (incl. the terminating NUL when
/// `c == 0`), or null.
#[no_mangle]
pub unsafe extern "C" fn strchr(s: *const u8, c: i32) -> *const u8 {
    let target = c as u8;
    let mut p = s;
    loop {
        let ch = *p;
        if ch == target {
            return p;
        }
        if ch == 0 {
            return core::ptr::null();
        }
        p = p.add(1);
    }
}

/// `atoi(s)` — parse a leading optionally-signed decimal integer.
#[no_mangle]
pub unsafe extern "C" fn atoi(s: *const u8) -> i32 {
    let mut p = s;
    while *p == b' ' || *p == b'\t' {
        p = p.add(1);
    }
    let mut neg = false;
    if *p == b'+' || *p == b'-' {
        neg = *p == b'-';
        p = p.add(1);
    }
    let mut v: i64 = 0;
    while (*p).is_ascii_digit() {
        v = v * 10 + (*p - b'0') as i64;
        p = p.add(1);
    }
    if neg {
        -v as i32
    } else {
        v as i32
    }
}

/// `qsort(base, num, size, cmp)` — insertion sort (stable enough; simple and
/// correct). Swaps element-sized byte blocks via a small stack scratch.
#[no_mangle]
pub unsafe extern "C" fn qsort(
    base: *mut u8,
    num: usize,
    size: usize,
    cmp: extern "C" fn(*const u8, *const u8) -> i32,
) {
    if num < 2 || size == 0 {
        return;
    }
    let elem = |i: usize| base.add(i * size);
    let mut tmp = [0u8; 256]; // element-size scratch (sort uses small records)
    let sz = size.min(tmp.len());
    for i in 1..num {
        // Lift element i into scratch, shift larger predecessors right.
        memcpy(tmp.as_mut_ptr(), elem(i), sz);
        let mut j = i;
        while j > 0 && cmp(elem(j - 1), tmp.as_ptr()) > 0 {
            memcpy(elem(j), elem(j - 1), sz);
            j -= 1;
        }
        memcpy(elem(j), tmp.as_ptr(), sz);
    }
}

/// `strcpy_s(dst, dstsz, src)` — bounded copy; returns 0 on success, ERANGE
/// (34) if the destination is too small.
#[no_mangle]
pub unsafe extern "C" fn strcpy_s(dst: *mut u8, dstsz: usize, src: *const u8) -> i32 {
    if dst.is_null() || dstsz == 0 {
        return 22; // EINVAL
    }
    let mut i = 0;
    loop {
        if i >= dstsz {
            *dst = 0;
            return 34; // ERANGE
        }
        let c = *src.add(i);
        *dst.add(i) = c;
        if c == 0 {
            return 0;
        }
        i += 1;
    }
}

// ---------------------------------------------------------------------------
// Locale-aware compares — "C" locale: plain byte/word ordering with ASCII
// case folding for the case-insensitive variants.
// ---------------------------------------------------------------------------

unsafe fn cmp_bytes(a: *const u8, b: *const u8, fold: bool, limit: Option<usize>) -> i32 {
    let lower = |c: u8| if fold && c.is_ascii_uppercase() { c + 32 } else { c };
    let mut i = 0;
    loop {
        if let Some(n) = limit {
            if i >= n {
                return 0;
            }
        }
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

unsafe fn cmp_words(a: *const u16, b: *const u16, fold: bool, limit: Option<usize>) -> i32 {
    let lower = |c: u16| if fold && (0x41..=0x5A).contains(&c) { c + 32 } else { c };
    let mut i = 0;
    loop {
        if let Some(n) = limit {
            if i >= n {
                return 0;
            }
        }
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

#[no_mangle]
pub unsafe extern "C" fn strcoll(a: *const u8, b: *const u8) -> i32 {
    cmp_bytes(a, b, false, None)
}
#[no_mangle]
pub unsafe extern "C" fn _stricoll(a: *const u8, b: *const u8) -> i32 {
    cmp_bytes(a, b, true, None)
}
#[no_mangle]
pub unsafe extern "C" fn _strncoll(a: *const u8, b: *const u8, n: usize) -> i32 {
    cmp_bytes(a, b, false, Some(n))
}
#[no_mangle]
pub unsafe extern "C" fn _strnicmp(a: *const u8, b: *const u8, n: usize) -> i32 {
    cmp_bytes(a, b, true, Some(n))
}
#[no_mangle]
pub unsafe extern "C" fn _strnicoll(a: *const u8, b: *const u8, n: usize) -> i32 {
    cmp_bytes(a, b, true, Some(n))
}
#[no_mangle]
pub unsafe extern "C" fn wcscoll(a: *const u16, b: *const u16) -> i32 {
    cmp_words(a, b, false, None)
}
#[no_mangle]
pub unsafe extern "C" fn _wcsicoll(a: *const u16, b: *const u16) -> i32 {
    cmp_words(a, b, true, None)
}
#[no_mangle]
pub unsafe extern "C" fn _wcsncoll(a: *const u16, b: *const u16, n: usize) -> i32 {
    cmp_words(a, b, false, Some(n))
}
#[no_mangle]
pub unsafe extern "C" fn _wcsnicoll(a: *const u16, b: *const u16, n: usize) -> i32 {
    cmp_words(a, b, true, Some(n))
}

/// `setlocale(category, locale)` — we only have the "C" locale; return its
/// name regardless of the request.
#[no_mangle]
pub extern "C" fn setlocale(_category: i32, _locale: *const u8) -> *const u8 {
    b"C\0".as_ptr()
}

// ---------------------------------------------------------------------------
// MSVC CRT startup / teardown helpers.
// ---------------------------------------------------------------------------

/// `_initterm(first, last)` — call each non-null `void(*)(void)` initializer
/// in `[first, last)`. Used by the CRT to run global constructors.
#[no_mangle]
pub unsafe extern "C" fn _initterm(first: *const Option<extern "C" fn()>, last: *const Option<extern "C" fn()>) {
    let mut p = first;
    while p < last {
        if let Some(f) = *p {
            f();
        }
        p = p.add(1);
    }
}

const MAX_ARGS: usize = 32;
static mut ARGV_BUF: [u8; 256] = [0; 256]; // command line, tokenized in place
static mut ARGV: [*const u8; MAX_ARGS + 1] = [core::ptr::null(); MAX_ARGS + 1];
static mut ENVP: [*const u8; 1] = [core::ptr::null()];
static mut ARGC: i32 = 0;
static mut ARGV_BUILT: bool = false;
// ucrt narrow-arg model: `__p___argv()` returns `char***` (a pointer to the
// variable holding argv), so we keep a variable that holds the argv pointer.
static mut ARGV_VAR: *const *const u8 = core::ptr::null();
static mut COMMODE: i32 = 0;
static mut FMODE: i32 = 0;

/// Fetch the command line from the kernel and tokenize it (space-separated,
/// in-place NUL-split, no quote handling yet) into [`ARGV`]/[`ARGC`]. Idempotent
/// — runs once, then later callers reuse the built table.
unsafe fn build_argv() {
    if ARGV_BUILT {
        return;
    }
    let got =
        syscall3(NT_GET_COMMAND_LINE, (&raw mut ARGV_BUF) as u64, (ARGV_BUF.len() - 1) as u64, 0) as usize;
    let n = got.min(ARGV_BUF.len() - 1);
    ARGV_BUF[n] = 0;
    let mut count = 0usize;
    let mut i = 0usize;
    while i < n && ARGV_BUF[i] != 0 {
        while i < n && ARGV_BUF[i] == b' ' {
            ARGV_BUF[i] = 0;
            i += 1;
        }
        if i >= n || ARGV_BUF[i] == 0 {
            break;
        }
        if count < MAX_ARGS {
            ARGV[count] = (&raw const ARGV_BUF[i]) as *const u8;
            count += 1;
        }
        while i < n && ARGV_BUF[i] != 0 && ARGV_BUF[i] != b' ' {
            i += 1;
        }
    }
    ARGV[count] = core::ptr::null();
    ENVP[0] = core::ptr::null();
    ARGC = count as i32;
    ARGV_VAR = (&raw const ARGV) as *const *const u8;
    ARGV_BUILT = true;
}

/// `__getmainargs(&argc, &argv, &envp, doWildcard, startInfo)` — the legacy
/// (msvcrt) entry: build argv and hand back argc/argv/empty-env. Returns 0.
#[no_mangle]
pub unsafe extern "C" fn __getmainargs(
    argc: *mut i32,
    argv: *mut *const *const u8,
    envp: *mut *const *const u8,
    _do_wildcard: i32,
    _start_info: *mut c_void,
) -> i32 {
    build_argv();
    if !argc.is_null() {
        *argc = ARGC;
    }
    if !argv.is_null() {
        *argv = (&raw const ARGV) as *const *const u8;
    }
    if !envp.is_null() {
        *envp = (&raw const ENVP) as *const *const u8;
    }
    0
}

// --- ucrt narrow-arg / environment / mode startup accessors ----------------
// Modern binaries (cmd.exe) use the ucrt model: `_configure_narrow_argv` builds
// argv, then `__p___argc`/`__p___argv` return pointers to the CRT's argc/argv
// variables (the program dereferences them). These all share `build_argv`.

/// `_configure_narrow_argv(mode)` — build the narrow argv. Returns 0 (success).
#[no_mangle]
pub unsafe extern "C" fn _configure_narrow_argv(_mode: i32) -> i32 {
    build_argv();
    0
}

/// `_initialize_narrow_environment()` — no real environment block yet. Success.
#[no_mangle]
pub extern "C" fn _initialize_narrow_environment() -> i32 {
    0
}

/// `_get_initial_narrow_environment()` — pointer to the (empty, NUL-terminated)
/// environment array.
#[no_mangle]
pub unsafe extern "C" fn _get_initial_narrow_environment() -> *const *const u8 {
    (&raw const ENVP) as *const *const u8
}

/// `__p___argv()` — pointer to the variable holding `argv` (`char***`).
#[no_mangle]
pub unsafe extern "C" fn __p___argv() -> *mut *const *const u8 {
    build_argv();
    (&raw mut ARGV_VAR) as *mut *const *const u8
}

/// `__p___argc()` — pointer to `argc`.
#[no_mangle]
pub unsafe extern "C" fn __p___argc() -> *mut i32 {
    build_argv();
    &raw mut ARGC
}

/// `__p__commode()` / `__p__fmode()` — pointers to the CRT mode globals.
#[no_mangle]
pub unsafe extern "C" fn __p__commode() -> *mut i32 {
    &raw mut COMMODE
}

#[no_mangle]
pub unsafe extern "C" fn __p__fmode() -> *mut i32 {
    &raw mut FMODE
}

/// Minimal `FILE` placeholder. The real layout is opaque; our `fprintf`
/// ignores the stream and writes to the console, so a small stub suffices to
/// give `__iob_func` distinct, valid pointers for stdin/stdout/stderr.
#[repr(C)]
pub struct File {
    _opaque: [u8; 48],
}
static mut IOB: [File; 3] = [
    File { _opaque: [0; 48] },
    File { _opaque: [0; 48] },
    File { _opaque: [0; 48] },
];

/// `__iob_func()` — base of the stdio stream array (stdin/stdout/stderr).
#[no_mangle]
pub unsafe extern "C" fn __iob_func() -> *mut File {
    (&raw mut IOB) as *mut File
}

/// A bounded byte sink that formats into a fixed stack buffer.
struct FmtBuf {
    buf: [u8; 1024],
    len: usize,
}
impl FmtBuf {
    fn new() -> Self {
        FmtBuf { buf: [0; 1024], len: 0 }
    }
    fn push(&mut self, b: u8) {
        if self.len < self.buf.len() {
            self.buf[self.len] = b;
            self.len += 1;
        }
    }
    fn push_bytes(&mut self, s: &[u8]) {
        for &b in s {
            self.push(b);
        }
    }
    fn push_u64(&mut self, mut v: u64, base: u64, upper: bool) {
        let mut tmp = [0u8; 20];
        let mut i = tmp.len();
        let digits = if upper { b"0123456789ABCDEF" } else { b"0123456789abcdef" };
        if v == 0 {
            self.push(b'0');
            return;
        }
        while v > 0 {
            i -= 1;
            tmp[i] = digits[(v % base) as usize];
            v /= base;
        }
        self.push_bytes(&tmp[i..]);
    }
    fn push_i64(&mut self, v: i64) {
        if v < 0 {
            self.push(b'-');
            self.push_u64((v as i128).unsigned_abs() as u64, 10, false);
        } else {
            self.push_u64(v as u64, 10, false);
        }
    }
}

/// `fprintf(stream, fmt, ...)` — formats into the console (our single stdio
/// sink). Supports the common conversions `%d/%i/%u/%x/%X/%c/%s/%p/%%` with an
/// `l`/`ll` length modifier; width/precision/flags are accepted and ignored.
///
/// Rather than the unstable variadic API, we declare the first few format
/// arguments explicitly. On the Microsoft x64 ABI the integer/pointer
/// variadic arguments occupy the same register/stack slots as fixed
/// parameters, so `a0..a2` *are* the first three `%`-arguments — enough for
/// the integer/string error-message paths (floating-point `%f`, which uses
/// XMM registers, is unsupported; documented). Returns the bytes written.
#[no_mangle]
pub unsafe extern "C" fn fprintf(
    _stream: *mut File,
    fmt: *const u8,
    a0: u64,
    a1: u64,
    a2: u64,
) -> i32 {
    let argv = [a0, a1, a2];
    let mut argi = 0usize;
    let mut next = || {
        let v = if argi < argv.len() { argv[argi] } else { 0 };
        argi += 1;
        v
    };
    let mut out = FmtBuf::new();
    let mut p = fmt;
    while *p != 0 {
        let c = *p;
        p = p.add(1);
        if c != b'%' {
            out.push(c);
            continue;
        }
        // Skip flags/width/precision (not honored), track length modifier.
        let mut long = 0u8;
        loop {
            match *p {
                b'-' | b'+' | b' ' | b'#' | b'0'..=b'9' | b'.' | b'*' => p = p.add(1),
                b'l' => {
                    long += 1;
                    p = p.add(1);
                }
                b'h' | b'z' | b't' => p = p.add(1),
                _ => break,
            }
        }
        let conv = *p;
        p = p.add(1);
        match conv {
            b'd' | b'i' => {
                let raw = next();
                let v = if long >= 1 { raw as i64 } else { raw as u32 as i32 as i64 };
                out.push_i64(v);
            }
            b'u' => {
                let raw = next();
                let v = if long >= 1 { raw } else { raw as u32 as u64 };
                out.push_u64(v, 10, false);
            }
            b'x' | b'X' => {
                let raw = next();
                let v = if long >= 1 { raw } else { raw as u32 as u64 };
                out.push_u64(v, 16, conv == b'X');
            }
            b'p' => {
                out.push_bytes(b"0x");
                out.push_u64(next(), 16, false);
            }
            b'c' => out.push(next() as u8),
            b's' => {
                let s = next() as *const u8;
                if !s.is_null() {
                    let mut n = 0;
                    while *s.add(n) != 0 {
                        n += 1;
                    }
                    out.push_bytes(core::slice::from_raw_parts(s, n));
                }
            }
            b'%' => out.push(b'%'),
            0 => break,
            other => {
                out.push(b'%');
                out.push(other);
            }
        }
    }
    console_write(&out.buf[..out.len]);
    out.len as i32
}

/// `__set_app_type(t)` — records GUI vs console for the CRT; we don't care.
#[no_mangle]
pub extern "C" fn __set_app_type(_t: i32) {}

/// `__setusermatherr(f)` — install a math-error handler; unused.
#[no_mangle]
pub extern "C" fn __setusermatherr(_f: *mut c_void) {}

/// `_cexit()` — run C teardown without terminating. We have no atexit table.
#[no_mangle]
pub extern "C" fn _cexit() {}

#[no_mangle]
pub unsafe extern "C" fn exit(_code: i32) -> ! {
    terminate_self()
}
#[no_mangle]
pub unsafe extern "C" fn _exit(_code: i32) -> ! {
    terminate_self()
}
#[no_mangle]
pub unsafe extern "C" fn _amsg_exit(_code: i32) -> ! {
    terminate_self()
}

/// `?terminate@@YAXXZ` — the C++ `terminate()` (MSVC-decorated name); aborts.
/// Exported under its decorated name via `#[export_name]`; `build-msvcrt.sh`
/// emits a matching `/EXPORT:` from the attribute.
#[export_name = "?terminate@@YAXXZ"]
pub unsafe extern "C" fn cpp_terminate() -> ! {
    terminate_self()
}

/// `_commode` / `_fmode` — CRT global mode flags the startup reads (text mode,
/// default file-commit behaviour). **Data** exports — `build-msvcrt.sh` emits
/// `/EXPORT:name,DATA` for these.
#[no_mangle]
pub static mut _commode: i32 = 0;
#[no_mangle]
pub static mut _fmode: i32 = 0;

// ---------------------------------------------------------------------------
// SEH glue — referenced by the CRT's __try/__except wrapper around main. On a
// clean run no exception propagates, so these are present but not invoked;
// they default to "continue searching" / "no exception handled".
// ---------------------------------------------------------------------------

#[no_mangle]
pub extern "C" fn _XcptFilter(_xcpt_num: u32, _xcpt_info: *mut c_void) -> i32 {
    0 // EXCEPTION_CONTINUE_SEARCH
}

#[no_mangle]
pub extern "C" fn __C_specific_handler(
    _record: *mut c_void,
    _frame: *mut c_void,
    _context: *mut c_void,
    _dispatch: *mut c_void,
) -> i32 {
    1 // ExceptionContinueSearch
}

// ---------------------------------------------------------------------------
// Wider CRT surface for classic console tools (e.g. choice.exe): wide-string
// parsing, a couple of stdio shims, and a wide `vsnprintf`.
// ---------------------------------------------------------------------------


/// `wcschr(s, c)` — first occurrence of wide char `c` (incl. NUL), or null.
#[no_mangle]
pub unsafe extern "C" fn wcschr(s: *const u16, c: u16) -> *const u16 {
    let mut p = s;
    loop {
        if *p == c {
            return p;
        }
        if *p == 0 {
            return core::ptr::null();
        }
        p = p.add(1);
    }
}

/// Parse a wide integer in `base` (0 = auto 0x/0/decimal); used by
/// `wcstol`/`wcstoul`. Updates `*endptr` if non-null.
unsafe fn wcsto_int(s: *const u16, endptr: *mut *const u16, mut base: i32, signed: bool) -> i64 {
    let mut p = s;
    while *p == b' ' as u16 || *p == b'\t' as u16 {
        p = p.add(1);
    }
    let mut neg = false;
    if *p == b'+' as u16 || *p == b'-' as u16 {
        neg = *p == b'-' as u16;
        p = p.add(1);
    }
    if base == 0 {
        if *p == b'0' as u16 && (*p.add(1) == b'x' as u16 || *p.add(1) == b'X' as u16) {
            base = 16;
            p = p.add(2);
        } else if *p == b'0' as u16 {
            base = 8;
        } else {
            base = 10;
        }
    } else if base == 16 && *p == b'0' as u16 && (*p.add(1) == b'x' as u16 || *p.add(1) == b'X' as u16) {
        p = p.add(2);
    }
    let mut v: i64 = 0;
    loop {
        let c = *p;
        let digit = match c {
            d if (b'0' as u16..=b'9' as u16).contains(&d) => (d - b'0' as u16) as i64,
            d if (b'a' as u16..=b'f' as u16).contains(&d) => (d - b'a' as u16 + 10) as i64,
            d if (b'A' as u16..=b'F' as u16).contains(&d) => (d - b'A' as u16 + 10) as i64,
            _ => break,
        };
        if digit >= base as i64 {
            break;
        }
        v = v * base as i64 + digit;
        p = p.add(1);
    }
    if !endptr.is_null() {
        *endptr = p;
    }
    let _ = signed;
    if neg {
        -v
    } else {
        v
    }
}

#[no_mangle]
pub unsafe extern "C" fn wcstol(s: *const u16, endptr: *mut *const u16, base: i32) -> i64 {
    wcsto_int(s, endptr, base, true)
}
#[no_mangle]
pub unsafe extern "C" fn wcstoul(s: *const u16, endptr: *mut *const u16, base: i32) -> u64 {
    wcsto_int(s, endptr, base, false) as u64
}

/// `wcstod(s, endptr)` — minimal: parse the integer part only (no fraction/
/// exponent). Enough for the integer-valued arguments our tools pass.
#[no_mangle]
pub unsafe extern "C" fn wcstod(s: *const u16, endptr: *mut *const u16) -> f64 {
    wcsto_int(s, endptr, 10, true) as f64
}

/// `_memicmp(a, b, n)` — case-insensitive (ASCII) memory compare.
#[no_mangle]
pub unsafe extern "C" fn _memicmp(a: *const u8, b: *const u8, n: usize) -> i32 {
    let lower = |c: u8| if c.is_ascii_uppercase() { c + 32 } else { c };
    for i in 0..n {
        let (x, y) = (lower(*a.add(i)), lower(*b.add(i)));
        if x != y {
            return x as i32 - y as i32;
        }
    }
    0
}

/// Per-"thread" errno cell (single-threaded process model). `_errno` returns
/// its address, the CRT's `errno` accessor.
static mut ERRNO: i32 = 0;
#[no_mangle]
pub unsafe extern "C" fn _errno() -> *mut i32 {
    &raw mut ERRNO
}

/// `_fileno(stream)` — map a stdio stream to its fd (0/1/2) by its position in
/// the IOB array.
#[no_mangle]
pub unsafe extern "C" fn _fileno(stream: *mut File) -> i32 {
    let base = (&raw const IOB) as *const File as usize;
    let s = stream as usize;
    if s >= base {
        ((s - base) / core::mem::size_of::<File>()) as i32
    } else {
        -1
    }
}

/// `_get_osfhandle(fd)` — map a CRT fd (0/1/2 = std streams) to the OS handle
/// by opening the console (all our std streams are the console device).
#[no_mangle]
pub unsafe extern "C" fn _get_osfhandle(_fd: i32) -> isize {
    console_handle() as isize
}

/// `fflush(stream)` — our writes are unbuffered (straight to the device), so
/// nothing to flush. Returns 0 (success).
#[no_mangle]
pub extern "C" fn fflush(_stream: *mut File) -> i32 {
    0
}

/// `_vsnwprintf(buffer, count, format, valist)` — wide `vsnprintf`. The
/// Microsoft x64 `va_list` is a pointer to the stacked arguments (8 bytes
/// each); we pull each conversion's argument from there in order. Supports
/// `%s` (wide), `%hs`/`%S` (narrow), `%c`, `%d/%i/%u/%x`, `%%`. Returns the
/// count written (excluding NUL), or -1 if it didn't fit.
#[no_mangle]
pub unsafe extern "C" fn _vsnwprintf(
    buffer: *mut u16,
    count: usize,
    format: *const u16,
    valist: *const u64,
) -> i32 {
    let mut out = 0usize;
    let mut put = |c: u16| {
        if out + 1 < count {
            unsafe { *buffer.add(out) = c };
        }
        out += 1;
    };
    let mut ai = 0usize;
    let mut next = || {
        let v = unsafe { *valist.add(ai) };
        ai += 1;
        v
    };
    let mut f = format;
    while *f != 0 {
        let c = *f;
        f = f.add(1);
        if c != b'%' as u16 {
            put(c);
            continue;
        }
        // Parse flags, then field width, then precision, then length modifier,
        // in C order. We honor the `0` flag (zero-pad) and the numeric width so
        // conversions like `%04x` produce the expected fixed-width output
        // (e.g. version-resource lookup formats `\StringFileInfo\%04x%04x\...`).
        let mut narrow = false;
        let mut zero_pad = false;
        let mut width = 0usize;
        loop {
            match *f as u8 {
                b'0' => {
                    zero_pad = true;
                    f = f.add(1);
                }
                b'-' | b'+' | b' ' | b'#' => f = f.add(1),
                _ => break,
            }
        }
        while (b'0'..=b'9').contains(&(*f as u8)) {
            width = width * 10 + (*f as u8 - b'0') as usize;
            f = f.add(1);
        }
        if *f as u8 == b'.' {
            f = f.add(1);
            while (b'0'..=b'9').contains(&(*f as u8)) {
                f = f.add(1);
            }
        }
        loop {
            match *f as u8 {
                b'l' => f = f.add(1),
                b'h' => {
                    narrow = true;
                    f = f.add(1);
                }
                _ => break,
            }
        }
        let conv = *f as u8;
        f = f.add(1);
        match conv {
            b'd' | b'i' => {
                let mut v = next() as i32 as i64;
                let neg = v < 0;
                if neg {
                    v = -v;
                }
                let mut tmp = [0u16; 20];
                let mut i = tmp.len();
                if v == 0 {
                    i -= 1;
                    tmp[i] = b'0' as u16;
                }
                while v > 0 {
                    i -= 1;
                    tmp[i] = b'0' as u16 + (v % 10) as u16;
                    v /= 10;
                }
                let total = (tmp.len() - i) + if neg { 1 } else { 0 };
                let padn = width.saturating_sub(total);
                if zero_pad {
                    if neg {
                        put(b'-' as u16);
                    }
                    for _ in 0..padn {
                        put(b'0' as u16);
                    }
                } else {
                    for _ in 0..padn {
                        put(b' ' as u16);
                    }
                    if neg {
                        put(b'-' as u16);
                    }
                }
                for &d in &tmp[i..] {
                    put(d);
                }
            }
            b'u' | b'x' | b'X' => {
                let base: u64 = if conv == b'u' { 10 } else { 16 };
                let upper = conv == b'X';
                let mut v = next() as u32 as u64;
                let digits = if upper { b"0123456789ABCDEF" } else { b"0123456789abcdef" };
                let mut tmp = [0u16; 20];
                let mut i = tmp.len();
                if v == 0 {
                    i -= 1;
                    tmp[i] = b'0' as u16;
                }
                while v > 0 {
                    i -= 1;
                    tmp[i] = digits[(v % base) as usize] as u16;
                    v /= base;
                }
                let padn = width.saturating_sub(tmp.len() - i);
                let padc = if zero_pad { b'0' as u16 } else { b' ' as u16 };
                for _ in 0..padn {
                    put(padc);
                }
                for &d in &tmp[i..] {
                    put(d);
                }
            }
            b'c' => put(next() as u16),
            b's' | b'S' => {
                let p = next();
                if p != 0 {
                    // %S flips width; %hs forces narrow.
                    if narrow || conv == b'S' {
                        let s = p as *const u8;
                        let mut i = 0;
                        while *s.add(i) != 0 {
                            put(*s.add(i) as u16);
                            i += 1;
                        }
                    } else {
                        let s = p as *const u16;
                        let mut i = 0;
                        while *s.add(i) != 0 {
                            put(*s.add(i));
                            i += 1;
                        }
                    }
                }
            }
            b'%' => put(b'%' as u16),
            0 => break,
            other => {
                put(b'%' as u16);
                put(other as u16);
            }
        }
    }
    if count > 0 {
        let term = core::cmp::min(out, count - 1);
        *buffer.add(term) = 0;
    }
    if out < count {
        out as i32
    } else {
        -1
    }
}

/// `__wgetmainargs(&argc, &wargv, &wenvp, doWildcard, startInfo)` — the wide
/// counterpart of `__getmainargs`: fetch the command line, widen it, and
/// tokenize into a UTF-16 argv.
static mut WARGV_BUF: [u16; 256] = [0; 256]; // widened command line, NUL-split
static mut WARGV: [*const u16; MAX_ARGS + 1] = [core::ptr::null(); MAX_ARGS + 1];
static mut WENVP: [*const u16; 1] = [core::ptr::null()];
#[no_mangle]
pub unsafe extern "C" fn __wgetmainargs(
    argc: *mut i32,
    argv: *mut *const *const u16,
    envp: *mut *const *const u16,
    _do_wildcard: i32,
    _start_info: *mut c_void,
) -> i32 {
    // Fetch the command line (ASCII) and widen it into WARGV_BUF.
    let mut bytes = [0u8; 256];
    let got = syscall3(NT_GET_COMMAND_LINE, bytes.as_mut_ptr() as u64, 255, 0) as usize;
    let n = got.min(255);
    for i in 0..n {
        WARGV_BUF[i] = bytes[i] as u16;
    }
    WARGV_BUF[n] = 0;
    // Tokenize on spaces, in place.
    let mut count = 0usize;
    let mut i = 0usize;
    while i < n && WARGV_BUF[i] != 0 {
        while i < n && WARGV_BUF[i] == b' ' as u16 {
            WARGV_BUF[i] = 0;
            i += 1;
        }
        if i >= n || WARGV_BUF[i] == 0 {
            break;
        }
        if count < MAX_ARGS {
            WARGV[count] = (&raw const WARGV_BUF[i]) as *const u16;
            count += 1;
        }
        while i < n && WARGV_BUF[i] != 0 && WARGV_BUF[i] != b' ' as u16 {
            i += 1;
        }
    }
    WARGV[count] = core::ptr::null();
    WENVP[0] = core::ptr::null();
    if !argc.is_null() {
        *argc = count as i32;
    }
    if !argv.is_null() {
        *argv = (&raw const WARGV) as *const *const u16;
    }
    if !envp.is_null() {
        *envp = (&raw const WENVP) as *const *const u16;
    }
    0
}

// --- More wide CRT helpers (where.exe) -------------------------------------

/// `towupper(c)` — ASCII uppercase of a wide char.
#[no_mangle]
pub extern "C" fn towupper(c: u16) -> u16 {
    if (0x61..=0x7A).contains(&c) {
        c - 32
    } else {
        c
    }
}

// --- Wide character classification / case (ASCII-range; what a CLI needs) ---
#[no_mangle]
pub extern "C" fn towlower(c: u16) -> u16 {
    if (b'A' as u16..=b'Z' as u16).contains(&c) {
        c + 32
    } else {
        c
    }
}
#[no_mangle]
pub extern "C" fn iswalpha(c: u16) -> i32 {
    (((b'A' as u16..=b'Z' as u16).contains(&c)) || ((b'a' as u16..=b'z' as u16).contains(&c))) as i32
}
#[no_mangle]
pub extern "C" fn iswdigit(c: u16) -> i32 {
    (b'0' as u16..=b'9' as u16).contains(&c) as i32
}
#[no_mangle]
pub extern "C" fn iswspace(c: u16) -> i32 {
    matches!(c, 0x20 | 0x09 | 0x0A | 0x0B | 0x0C | 0x0D) as i32
}
#[no_mangle]
pub extern "C" fn iswxdigit(c: u16) -> i32 {
    ((b'0' as u16..=b'9' as u16).contains(&c)
        || (b'a' as u16..=b'f' as u16).contains(&c)
        || (b'A' as u16..=b'F' as u16).contains(&c)) as i32
}

/// `_wcsicmp(a, b)` — case-insensitive wide compare.
#[no_mangle]
pub unsafe extern "C" fn _wcsicmp(a: *const u16, b: *const u16) -> i32 {
    let mut i = 0;
    loop {
        let (x, y) = (towlower(*a.add(i)), towlower(*b.add(i)));
        if x != y {
            return x as i32 - y as i32;
        }
        if x == 0 {
            return 0;
        }
        i += 1;
    }
}

/// `_wcsnicmp(a, b, n)` — case-insensitive wide compare, first `n` units.
#[no_mangle]
pub unsafe extern "C" fn _wcsnicmp(a: *const u16, b: *const u16, n: usize) -> i32 {
    let mut i = 0;
    while i < n {
        let (x, y) = (towlower(*a.add(i)), towlower(*b.add(i)));
        if x != y {
            return x as i32 - y as i32;
        }
        if x == 0 {
            return 0;
        }
        i += 1;
    }
    0
}

/// `_wcslwr(s)` / `_wcsupr(s)` — in-place ASCII case conversion; returns `s`.
#[no_mangle]
pub unsafe extern "C" fn _wcslwr(s: *mut u16) -> *mut u16 {
    let mut i = 0;
    while *s.add(i) != 0 {
        *s.add(i) = towlower(*s.add(i));
        i += 1;
    }
    s
}
#[no_mangle]
pub unsafe extern "C" fn _wcsupr(s: *mut u16) -> *mut u16 {
    let mut i = 0;
    while *s.add(i) != 0 {
        *s.add(i) = towupper(*s.add(i));
        i += 1;
    }
    s
}

/// `wcsrchr(s, c)` — last occurrence of `c` (incl. NUL), or null.
#[no_mangle]
pub unsafe extern "C" fn wcsrchr(s: *const u16, c: u16) -> *const u16 {
    let mut last = core::ptr::null();
    let mut p = s;
    loop {
        if *p == c {
            last = p;
        }
        if *p == 0 {
            return last;
        }
        p = p.add(1);
    }
}

/// `wcspbrk(s, set)` — first char of `s` that appears in `set`, or null.
#[no_mangle]
pub unsafe extern "C" fn wcspbrk(s: *const u16, set: *const u16) -> *const u16 {
    let mut p = s;
    while *p != 0 {
        let mut q = set;
        while *q != 0 {
            if *p == *q {
                return p;
            }
            q = q.add(1);
        }
        p = p.add(1);
    }
    core::ptr::null()
}

/// Saved position for the classic (2-arg) `wcstok`. The msvcrt.dll `wcstok`
/// keeps its state internally — callers pass `NULL` as `str` to continue — so
/// we must NOT read a third "context" argument (it would be garbage from a
/// 2-arg call and crash on the continuation). Not thread-safe, matching msvcrt.
static mut WCSTOK_STATE: *mut u16 = core::ptr::null_mut();

#[no_mangle]
pub unsafe extern "C" fn wcstok(str: *mut u16, delim: *const u16) -> *mut u16 {
    let in_delim = |c: u16| {
        let mut q = delim;
        while *q != 0 {
            if *q == c {
                return true;
            }
            q = q.add(1);
        }
        false
    };
    let mut p = if str.is_null() { WCSTOK_STATE } else { str };
    if p.is_null() {
        return core::ptr::null_mut();
    }
    while *p != 0 && in_delim(*p) {
        p = p.add(1);
    }
    if *p == 0 {
        WCSTOK_STATE = p;
        return core::ptr::null_mut();
    }
    let token = p;
    while *p != 0 && !in_delim(*p) {
        p = p.add(1);
    }
    if *p != 0 {
        *p = 0;
        p = p.add(1);
    }
    WCSTOK_STATE = p;
    token
}

/// `localtime(time)` — no real-time clock; report failure (null).
#[no_mangle]
pub extern "C" fn localtime(_time: *const i64) -> *mut c_void {
    core::ptr::null_mut()
}

/// `_wstat(path, buffer)` — no stat-able filesystem here; report failure.
#[no_mangle]
pub extern "C" fn _wstat(_path: *const u16, _buffer: *mut c_void) -> i32 {
    -1
}

/// `_wgetenv(name)` — empty environment; not found.
#[no_mangle]
pub extern "C" fn _wgetenv(_name: *const u16) -> *const u16 {
    core::ptr::null()
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
