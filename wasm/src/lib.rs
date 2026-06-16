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
use core::sync::atomic::{AtomicUsize, Ordering};

// --- Host interface (the substituted "hardware") ---------------------------
// The JS host supplies these; `memory` (WASM linear memory) is exported so the
// host can read the bytes a pointer/length refers to.
#[link(wasm_import_module = "env")]
extern "C" {
    /// Write `len` bytes of UTF-8 at `ptr` to the host console (the page).
    fn host_write(ptr: *const u8, len: usize);
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

// --- rtl: NT status codes --------------------------------------------------
#[derive(Clone, Copy, PartialEq, Eq)]
struct NtStatus(u32);
impl NtStatus {
    const SUCCESS: NtStatus = NtStatus(0x0000_0000);
    const INSUFFICIENT_RESOURCES: NtStatus = NtStatus(0xC000_009A);
    const OBJECT_NAME_NOT_FOUND: NtStatus = NtStatus(0xC000_0034);
    fn is_ok(self) -> bool {
        self.0 == 0
    }
}

// --- mm: a bump pool over a static "physical memory" arena -----------------
// Stands in for mm/phys + ex pool. In the x86 kernel this is real RAM behind
// page tables; here it's a fixed region of WASM linear memory.
const ARENA_SIZE: usize = 1 << 20; // 1 MiB
static mut ARENA: [u8; ARENA_SIZE] = [0; ARENA_SIZE];
static ARENA_BUMP: AtomicUsize = AtomicUsize::new(0);

/// Allocate `n` 16-byte-aligned bytes from the arena; null on exhaustion.
fn pool_alloc(n: usize) -> *mut u8 {
    let aligned = (n + 15) & !15;
    let off = ARENA_BUMP.fetch_add(aligned, Ordering::Relaxed);
    if off + aligned > ARENA_SIZE {
        return core::ptr::null_mut();
    }
    unsafe { (&raw mut ARENA as *mut u8).add(off) }
}

fn pool_used() -> usize {
    ARENA_BUMP.load(Ordering::Relaxed)
}

// --- ob: a tiny object-manager namespace -----------------------------------
// Name → opaque token (a real OB would hold an OBJECT_HEADER); enough to show
// directory insert/lookup, the heart of the object manager.
const OB_MAX: usize = 32;
const OB_NAME_MAX: usize = 64;
struct ObNamespace {
    names: [[u8; OB_NAME_MAX]; OB_MAX],
    name_len: [usize; OB_MAX],
    tokens: [u64; OB_MAX],
    count: usize,
}
static mut OB: ObNamespace = ObNamespace {
    names: [[0; OB_NAME_MAX]; OB_MAX],
    name_len: [0; OB_MAX],
    tokens: [0; OB_MAX],
    count: 0,
};

fn ob_insert(name: &str, token: u64) -> NtStatus {
    let ob = unsafe { &mut *(&raw mut OB) };
    if ob.count >= OB_MAX || name.len() > OB_NAME_MAX {
        return NtStatus::INSUFFICIENT_RESOURCES;
    }
    let i = ob.count;
    ob.names[i][..name.len()].copy_from_slice(name.as_bytes());
    ob.name_len[i] = name.len();
    ob.tokens[i] = token;
    ob.count += 1;
    NtStatus::SUCCESS
}

fn ob_lookup(name: &str) -> Result<u64, NtStatus> {
    let ob = unsafe { &*(&raw const OB) };
    for i in 0..ob.count {
        if ob.name_len[i] == name.len() && &ob.names[i][..name.len()] == name.as_bytes() {
            return Ok(ob.tokens[i]);
        }
    }
    Err(NtStatus::OBJECT_NAME_NOT_FOUND)
}

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

    // mm: pool allocate three blocks, confirm distinct + arena accounting.
    let a = pool_alloc(64);
    let b = pool_alloc(64);
    let c = pool_alloc(1024);
    all &= check(
        "mm: pool_alloc returns distinct non-null blocks",
        !a.is_null() && !b.is_null() && !c.is_null() && a != b && b != c,
    );
    all &= check("mm: arena bump accounts the allocations", pool_used() >= 64 + 64 + 1024);

    // ob: insert a couple of device names, look them up, miss on an absent one.
    let _ = ob_insert("\\Device\\Null", 0x1000);
    let _ = ob_insert("\\Device\\Console", 0x2000);
    all &= check("ob: lookup \\Device\\Null", ob_lookup("\\Device\\Null") == Ok(0x1000));
    all &= check("ob: lookup \\Device\\Console", ob_lookup("\\Device\\Console") == Ok(0x2000));
    all &= check(
        "ob: absent name -> OBJECT_NAME_NOT_FOUND",
        ob_lookup("\\Device\\Nope") == Err(NtStatus::OBJECT_NAME_NOT_FOUND),
    );

    // rtl: status predicate.
    all &= check("rtl: SUCCESS is_ok, error is not", NtStatus::SUCCESS.is_ok() && !NtStatus::OBJECT_NAME_NOT_FOUND.is_ok());

    println("");
    if all {
        print("ALL SELF TESTS PASSED — ");
        print_usize(pool_used());
        println(" bytes of pool in use; system idle");
        0
    } else {
        println("*** SELF TESTS FAILED ***");
        1
    }
}
