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
use core::sync::atomic::{AtomicBool, Ordering};

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
        println(" bytes of pool in use; system idle");
        0
    } else {
        println("*** SELF TESTS FAILED ***");
        1
    }
}
