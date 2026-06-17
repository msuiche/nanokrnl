//! ntoskrnl-rs — the WebAssembly build.
//!
//! The x86 kernel reaches hardware through `asm!`, ports, page tables and the
//! `syscall` instruction — none of which exist in WebAssembly. This module is
//! the start of running the kernel's NT subsystems in the browser with that
//! hardware layer **substituted** by host (JavaScript) imports: console output
//! is a host call, "physical memory" is a static arena in WASM linear memory,
//! and there are no interrupts or privilege rings.
//!
//! Phase 0 (this file) is proof of life: faithful-but-self-contained miniatures
//! of three kernel subsystems — a pool allocator (`mm`), an object-manager
//! namespace (`ob`), and NT status codes (`rtl`) — exercised by self-tests and
//! reported to the page. Later phases replace these with the kernel's real
//! modules once a HAL boundary lets that crate build for wasm32. See WORKLOG.md.
#![no_std]

use core::panic::PanicInfo;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

// Phase 1: the kernel's REAL run-time library, compiled into the WASM build
// unchanged. `rtl` touches no hardware and takes no locks (pure data-structure
// code), so the same source the x86 kernel uses also builds for wasm32 — this
// is the first actual kernel module (not a stand-in) running in the browser.
#[path = "../../kernel/src/rtl/mod.rs"]
#[allow(dead_code)] // the full module is included; not every item is exercised yet
mod rtl;
use rtl::string::UnicodeString;
use rtl::NtStatus;

// WASM-side HAL substitutes the kernel's real `ob` compiles against: a
// single-threaded `SpinLock` (ke) and a pool over a WASM-memory arena (mm). With
// those in place, the kernel's actual object manager + handle table build for
// wasm32 unchanged via the `#[path]` include below.
mod ke;
mod mm;

#[path = "../../kernel/src/ob/mod.rs"]
#[allow(dead_code)]
mod ob;

// --- Host interface (the substituted "hardware") ---------------------------
// The JS host supplies these; `memory` (WASM linear memory) is exported so the
// host can read the bytes a pointer/length refers to.
#[link(wasm_import_module = "env")]
extern "C" {
    /// Write `len` bytes of UTF-8 at `ptr` to the host console (the page).
    fn host_write(ptr: *const u8, len: usize);
    /// Clear the host console (the `cls` command).
    fn host_clear();
    /// Load and run a guest program named by `len` UTF-8 bytes at `ptr`.
    /// Returns the program's exit code, or -1 if no such program. The host
    /// instantiates the guest `.wasm` and bridges its `sys_print` syscall to the
    /// console — the WASM analogue of the kernel spawning a ring-3 process.
    fn host_run(name_ptr: *const u8, name_len: usize) -> i32;
}

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    // No unwinding in WASM; trap.
    core::arch::wasm32::unreachable()
}

/// Emit a string to the host console.
fn print(s: &str) {
    unsafe { host_write(s.as_ptr(), s.len()) };
}

/// Emit a line.
fn println(s: &str) {
    print(s);
    print("\n");
}

/// Emit a decimal `usize` (no alloc).
fn print_usize(mut v: usize) {
    let mut buf = [0u8; 20];
    let mut i = buf.len();
    if v == 0 {
        print("0");
        return;
    }
    while v > 0 {
        i -= 1;
        buf[i] = b'0' + (v % 10) as u8;
        v /= 10;
    }
    print(unsafe { core::str::from_utf8_unchecked(&buf[i..]) });
}

// --- self-test fixtures: a real `ob` object type + its delete procedure -----
// `ob_dereference_object` must run the type's delete callback on the last ref;
// this flag lets the self test observe that.
static DELETED: AtomicBool = AtomicBool::new(false);
fn null_delete(_body: *mut u8) {
    DELETED.store(true, Ordering::Relaxed);
}
static NULL_TYPE_NAME: [u16; 4] = [b'N' as u16, b'u' as u16, b'l' as u16, b'l' as u16];
static NULL_TYPE: ob::ObjectType = ob::ObjectType {
    name: UnicodeString::from_units(&NULL_TYPE_NAME),
    delete: Some(null_delete),
};

// --- self tests ------------------------------------------------------------
fn check(name: &str, ok: bool) -> bool {
    print(if ok { "  [ OK ] " } else { "  [FAIL] " });
    println(name);
    ok
}

/// The WASM "boot": print a banner, run the hardware-independent self tests
/// against the substituted subsystems, and report. Returns 0 on success (the
/// x86 build's bugcheck-free idle), nonzero on a failed test.
#[no_mangle]
pub extern "C" fn kernel_main() -> i32 {
    println("ntoskrnl-rs 0.1.0 (wasm32) — NT-compatible kernel in Rust");
    println("KiSystemStartup: hardware layer = browser host (no MMU, no rings)");
    println("");

    let mut all = true;

    // mm (real pool API over the WASM-memory arena): distinct non-null blocks.
    let a = mm::pool::pool_alloc(64, 0);
    let b = mm::pool::pool_alloc(64, 0);
    let c = mm::pool::pool_alloc(1024, 0);
    all &= check(
        "mm: pool_alloc returns distinct non-null blocks",
        !a.is_null() && !b.is_null() && !c.is_null() && a != b && b != c,
    );

    // ob (REAL object manager + handle table): create a typed object, take a
    // handle (ref +1), resolve it, close it (ref -1), then drop the last ref and
    // confirm the type's delete procedure ran.
    let body = ob::ob_create_object(&NULL_TYPE, 0u32);
    let ok_create = body.is_ok();
    let bptr = body.unwrap_or(core::ptr::null_mut()) as *mut u8;
    let rc_initial = if bptr.is_null() { -1 } else { unsafe { ob::ob_ref_count(bptr) } };
    all &= check(
        "ob: ob_create_object returns a referenced object (refcount 1)",
        ok_create && rc_initial == 1,
    );

    let h = if bptr.is_null() { 0 } else { ob::handle::ob_create_handle(bptr, 0) };
    let resolved = ob::handle::ob_reference_object_by_handle(h);
    all &= check(
        "ob: ob_create_handle + resolve by handle",
        h != 0 && resolved == Ok(bptr),
    );
    let rc_with_handle = if bptr.is_null() { -1 } else { unsafe { ob::ob_ref_count(bptr) } };
    all &= check("ob: an open handle holds a reference (refcount 2)", rc_with_handle == 2);

    let closed = ob::handle::ob_close_handle(h);
    let rc_after_close = if bptr.is_null() { -1 } else { unsafe { ob::ob_ref_count(bptr) } };
    all &= check(
        "ob: closing the handle drops its reference (refcount 1)",
        closed == NtStatus::SUCCESS && rc_after_close == 1,
    );

    if !bptr.is_null() {
        unsafe { ob::ob_dereference_object(bptr) };
    }
    all &= check(
        "ob: last dereference runs the type's delete procedure",
        DELETED.load(Ordering::Relaxed),
    );

    // rtl (REAL kernel module): status codes are the documented Windows values
    // and the severity predicates work.
    all &= check(
        "rtl: NtStatus values + severity (ACCESS_VIOLATION=0xC0000005)",
        NtStatus::SUCCESS.is_success()
            && NtStatus::ACCESS_VIOLATION.is_error()
            && NtStatus::ACCESS_VIOLATION.0 == 0xC000_0005,
    );

    // rtl (REAL kernel module): the RTL_BITMAP allocator Mm/the handle table use.
    let mut words = [0u64; 2]; // 128 bits
    let mut bm = rtl::bitmap::RtlBitmap::new(&mut words, 128);
    let first = bm.find_clear_bits_and_set(8, 0);
    let second = bm.find_clear_bits_and_set(8, 0);
    all &= check(
        "rtl: RtlBitmap find_clear_bits_and_set hands out distinct runs",
        first == Some(0) && second == Some(8) && bm.count_set() == 16,
    );

    println("");
    if all {
        print("ALL SELF TESTS PASSED — ");
        print_usize(mm::pool::pool_used());
        println(" bytes of pool in use");
    } else {
        println("*** SELF TESTS FAILED ***");
    }
    // Drop to the interactive console: boot is done, the host now feeds typed
    // command lines to `kernel_input` (see below). This is what makes it a live
    // kernel in the page rather than a one-shot self test.
    println("");
    println("nanokrnl console (not cmd.exe — type 'help'; 'run <prog>' runs a real binary)");
    prompt();
    if all {
        0
    } else {
        1
    }
}

// === Interactive console ===================================================
// WASM can't block waiting for a keypress, so input is event-driven: the host
// reads a line and calls `kernel_input(ptr, len)`. We run the command, print
// output, and print the next prompt. (On x86 the shell is cmd.exe in ring 3;
// here it's a built-in shell over the same subsystems — and the place future
// "executables" plug in, as guest WASM modules over the syscall surface, since
// WASM can't execute the x86 PE binaries the native kernel runs.)

/// The kernel's own console prompt. Deliberately NOT a fake `C:\>` cmd prompt —
/// this is nanokrnl's built-in console, not a Windows command interpreter. The
/// real cmd.exe will run as an actual binary under the emulator (`run`), not be
/// imitated here.
fn prompt() {
    print("\nnanokrnl> ");
}

/// A handle the shell opened, for `handles`/`close`.
#[derive(Clone, Copy)]
struct ShellObj {
    handle: u64,
    body: *mut u8,
}
const SHELL_OBJ_MAX: usize = 16;
static mut SHELL_OBJS: [ShellObj; SHELL_OBJ_MAX] =
    [ShellObj { handle: 0, body: core::ptr::null_mut() }; SHELL_OBJ_MAX];
static SHELL_OBJ_COUNT: AtomicUsize = AtomicUsize::new(0);

/// Object type for shell-created objects; its delete procedure reports the
/// teardown so `close` visibly drives a real object lifetime.
static SHELL_TYPE_NAME: [u16; 7] = [
    b'S' as u16, b'h' as u16, b'e' as u16, b'l' as u16, b'l' as u16, b'O' as u16, b'b' as u16,
];
static SHELL_TYPE: ob::ObjectType = ob::ObjectType {
    name: UnicodeString::from_units(&SHELL_TYPE_NAME),
    delete: Some(shell_obj_delete),
};
fn shell_obj_delete(_body: *mut u8) {
    println("[ob] object's delete procedure ran; pool freed");
}

// The host writes a command line's UTF-8 bytes into this fixed buffer (whose
// address it gets from `kernel_input_ptr`), then calls `kernel_input(len)`.
// A fixed buffer avoids exposing an allocator across the boundary.
const INPUT_MAX: usize = 256;
static mut INPUT_BUF: [u8; INPUT_MAX] = [0; INPUT_MAX];

/// Address of the shared input buffer (a WASM linear-memory offset to the host).
#[no_mangle]
pub extern "C" fn kernel_input_ptr() -> *mut u8 {
    &raw mut INPUT_BUF as *mut u8
}

/// Run one command line of `len` bytes (already written into `INPUT_BUF`).
/// Called by the host for each entered line.
#[no_mangle]
pub extern "C" fn kernel_input(len: usize) {
    let n = len.min(INPUT_MAX);
    let bytes = unsafe { core::slice::from_raw_parts((&raw const INPUT_BUF) as *const u8, n) };
    let line = core::str::from_utf8(bytes).unwrap_or("").trim();
    // Echo the line so the page transcript reads like a real console session.
    print(line);
    print("\n");
    dispatch(line);
    prompt();
}

/// Split `s` into (first word, rest), both trimmed.
fn split_word(s: &str) -> (&str, &str) {
    let s = s.trim_start();
    match s.find(' ') {
        Some(i) => (&s[..i], s[i + 1..].trim_start()),
        None => (s, ""),
    }
}

fn dispatch(line: &str) {
    let (cmd, args) = split_word(line);
    match cmd {
        "" => {}
        "help" => {
            println("commands:");
            println("  help            this list");
            println("  ver             kernel version banner");
            println("  whoami          current user");
            println("  echo <text>     print text");
            println("  run <prog>      load and run a guest WASM program (e.g. 'run hello')");
            println("  run86           run a real x86-64 program through our emulator");
            println("  mem             pool bytes in use");
            println("  mkobj           create a kernel object + open a handle (real ob)");
            println("  handles         list open handles");
            println("  close <handle>  close a handle (drops its object reference)");
            println("  cls             clear the screen");
        }
        "ver" => {
            println("ntoskrnl-rs 0.1.0 (wasm32) — NT-compatible kernel in Rust");
        }
        "whoami" => {
            // Identity lives in the kernel, so this works as a built-in even
            // though the x86 whoami.exe can't run here.
            println("nanokrnl\\user");
        }
        "echo" => {
            println(args);
        }
        "run" => cmd_run(args),
        "run86" => cmd_run86(),
        "mem" => {
            print("pool in use: ");
            print_usize(mm::pool::pool_used());
            println(" bytes");
        }
        "mkobj" => cmd_mkobj(),
        "handles" => cmd_handles(),
        "close" => cmd_close(args),
        "cls" => unsafe { host_clear() },
        _ => {
            print("'");
            print(cmd);
            println("' is not recognized as a command. Try 'help'.");
        }
    }
}

/// Load and run a guest WASM program through the host (`run <prog>`). The host
/// instantiates `<prog>.wasm`, bridges its `sys_print` syscall to the console,
/// runs its `main`, and returns the exit code — the closest thing to "running
/// an executable" the WASM world allows (x86 PE binaries need an emulator).
fn cmd_run(name: &str) {
    let name = name.trim();
    if name.is_empty() {
        println("usage: run <program>   (try 'run hello')");
        return;
    }
    let code = unsafe { host_run(name.as_ptr(), name.len()) };
    if code < 0 {
        print("run: program not found: ");
        println(name);
    } else {
        print("[");
        print(name);
        print(" exited with code ");
        print_usize(code as usize);
        println("]");
    }
}

/// Run a real x86-64 program through our own interpreter (Track B). The program
/// is hand-assembled machine code; it prints via a `syscall` the kernel
/// services here (service 1 = write `[edi..edi+esi)`), proving x86 code runs
/// inside the WASM kernel with no emulator import and no x86 hardware.
fn cmd_run86() {
    // Emulator "physical memory": code at 0, data at 0x200, stack at the top.
    static mut EMU_MEM: [u8; 4096] = [0; 4096];
    let mem = unsafe { &mut *(&raw mut EMU_MEM) };
    for byte in mem.iter_mut() {
        *byte = 0;
    }

    let msg = b"hello from real x86-64 machine code, executed by our interpreter\n";
    let data_off: u32 = 0x200;
    mem[data_off as usize..data_off as usize + msg.len()].copy_from_slice(msg);

    // mov eax,1 ; mov edi,data_off ; mov esi,len ; syscall ; ret
    fn put(code: &mut [u8], n: &mut usize, bytes: &[u8]) {
        code[*n..*n + bytes.len()].copy_from_slice(bytes);
        *n += bytes.len();
    }
    let mut code = [0u8; 32];
    let mut n = 0usize;
    put(&mut code, &mut n, &[0xb8]);
    put(&mut code, &mut n, &1u32.to_le_bytes()); // mov eax, 1 (service: write)
    put(&mut code, &mut n, &[0xbf]);
    put(&mut code, &mut n, &data_off.to_le_bytes()); // mov edi, data ptr
    put(&mut code, &mut n, &[0xbe]);
    put(&mut code, &mut n, &(msg.len() as u32).to_le_bytes()); // mov esi, len
    put(&mut code, &mut n, &[0x0f, 0x05]); // syscall
    put(&mut code, &mut n, &[0xc3]); // ret
    mem[..n].copy_from_slice(&code[..n]);

    use x86emu::{Cpu, StepResult, RAX};
    let mut cpu = Cpu::new();
    let stack_top = mem.len() as u64;
    cpu.setup_frame(mem, 0, stack_top);
    let mut guard = 0;
    loop {
        guard += 1;
        if guard > 100_000 {
            println("run86: step limit reached");
            return;
        }
        match cpu.step(mem) {
            StepResult::Ok => {}
            StepResult::Syscall => {
                let svc = cpu.regs[RAX] & 0xFFFF_FFFF;
                if svc == 1 {
                    // write: edi = ptr, esi = len (regs 7 and 6).
                    let ptr = (cpu.regs[7] & 0xFFFF_FFFF) as usize;
                    let len = (cpu.regs[6] & 0xFFFF_FFFF) as usize;
                    if ptr + len <= mem.len() {
                        if let Ok(s) = core::str::from_utf8(&mem[ptr..ptr + len]) {
                            print(s);
                        }
                    }
                }
                // svc 0 (or any other) would be "exit"; fall through to continue.
            }
            StepResult::Import { index } => {
                // The hand-assembled run86 program imports nothing; a real PE
                // would resolve this against the kernel. Treat as a no-op return.
                let _ = index;
                println("run86: unexpected import call");
                break;
            }
            StepResult::Halt => break,
            StepResult::Unknown { rip, byte } => {
                print("run86: unimplemented opcode 0x");
                print_hex(byte as u64);
                print(" at rip 0x");
                print_hex(rip);
                println("");
                return;
            }
            StepResult::Fault { addr } => {
                print("run86: memory fault at 0x");
                print_hex(addr);
                println("");
                return;
            }
        }
    }
    println("[run86: x86-64 program exited]");
}

/// Create a real `ob` object and open a handle to it (the handle holds a
/// reference), recording it for `handles`/`close`.
fn cmd_mkobj() {
    let n = SHELL_OBJ_COUNT.load(Ordering::Relaxed);
    if n >= SHELL_OBJ_MAX {
        println("object table full");
        return;
    }
    let body = match ob::ob_create_object(&SHELL_TYPE, 0u32) {
        Ok(p) => p as *mut u8,
        Err(_) => {
            println("ob_create_object failed (out of pool)");
            return;
        }
    };
    let handle = ob::handle::ob_create_handle(body, 0);
    unsafe {
        let table = &raw mut SHELL_OBJS;
        (*table)[n] = ShellObj { handle, body };
    }
    SHELL_OBJ_COUNT.store(n + 1, Ordering::Relaxed);
    print("created ShellOb object, handle 0x");
    print_hex(handle);
    print(" (refcount ");
    print_usize(unsafe { ob::ob_ref_count(body) } as usize);
    println(")");
}

fn cmd_handles() {
    let n = SHELL_OBJ_COUNT.load(Ordering::Relaxed);
    let mut shown = 0;
    for i in 0..n {
        let o = unsafe { (*(&raw const SHELL_OBJS))[i] };
        if o.handle == 0 {
            continue;
        }
        print("  handle 0x");
        print_hex(o.handle);
        print("  refcount ");
        print_usize(unsafe { ob::ob_ref_count(o.body) } as usize);
        println("");
        shown += 1;
    }
    if shown == 0 {
        println("no open handles (use 'mkobj')");
    }
}

fn cmd_close(args: &str) {
    // `handles` prints handles in hex, so parse hex (with optional 0x).
    let s = args.trim().trim_start_matches("0x").trim_start_matches("0X");
    match parse_hex(s) {
        Some(h) => close_handle(h),
        None => println("usage: close <handle>   (e.g. close 0x4)"),
    }
}

fn close_handle(handle: u64) {
    let n = SHELL_OBJ_COUNT.load(Ordering::Relaxed);
    for i in 0..n {
        let o = unsafe { (*(&raw const SHELL_OBJS))[i] };
        if o.handle == handle && o.handle != 0 {
            let st = ob::handle::ob_close_handle(handle);
            if st == NtStatus::SUCCESS {
                // Drop the shell's own creator reference too, so the object
                // actually dies (and we see its delete procedure run).
                unsafe { ob::ob_dereference_object(o.body) };
                unsafe {
                    (*(&raw mut SHELL_OBJS))[i].handle = 0;
                }
                println("handle closed");
            } else {
                println("close failed: invalid handle");
            }
            return;
        }
    }
    println("no such handle");
}

/// Print a u64 as lowercase hex (no leading zeros).
fn print_hex(mut v: u64) {
    if v == 0 {
        print("0");
        return;
    }
    let mut buf = [0u8; 16];
    let mut i = buf.len();
    while v > 0 {
        i -= 1;
        let d = (v & 0xf) as u8;
        buf[i] = if d < 10 { b'0' + d } else { b'a' + (d - 10) };
        v >>= 4;
    }
    print(unsafe { core::str::from_utf8_unchecked(&buf[i..]) });
}

/// Parse lowercase/uppercase hex; None if empty/invalid.
fn parse_hex(s: &str) -> Option<u64> {
    if s.is_empty() {
        return None;
    }
    let mut v: u64 = 0;
    for b in s.bytes() {
        let d = match b {
            b'0'..=b'9' => b - b'0',
            b'a'..=b'f' => b - b'a' + 10,
            b'A'..=b'F' => b - b'A' + 10,
            _ => return None,
        };
        v = (v << 4) | d as u64;
    }
    Some(v)
}
