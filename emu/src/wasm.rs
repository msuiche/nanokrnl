//! WebAssembly entry points + freestanding runtime for the browser build.
//!
//! Compiled with `cargo build --target wasm32-unknown-unknown --release`, this
//! exposes a tiny C ABI the JS shim (web/nanox/) drives: create a machine, load
//! an image, boot, step, drain the UART, feed keystrokes. A single global
//! machine instance keeps the ABI pointer-free.
//!
//! Since the wasm build is `no_std`, this module also provides the
//! `#[panic_handler]` and a `#[global_allocator]` (a simple bump allocator over
//! a static arena — the emulator allocates its RAM buffer once and the device
//! queues reach a steady size, so never freeing is acceptable for a v0 browser
//! demo; documented in SPEC.md).

extern crate alloc;

use crate::elf::Elf;
use crate::machine::{Machine, RunStop};
use core::alloc::{GlobalAlloc, Layout};
use core::cell::UnsafeCell;
use core::panic::PanicInfo;

// --- freestanding runtime -------------------------------------------------

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    // No unwinding in wasm; spin. (The JS side notices the machine stopped.)
    loop {}
}

/// Bump allocator over a fixed static arena. Single-threaded (wasm has no
/// threads here), so the `Sync` impl is sound.
const ARENA_SIZE: usize = 160 * 1024 * 1024; // RAM buffer (128 MiB) + image + overhead

#[repr(C, align(16))]
struct Arena(UnsafeCell<[u8; ARENA_SIZE]>);
unsafe impl Sync for Arena {}

static ARENA: Arena = Arena(UnsafeCell::new([0u8; ARENA_SIZE]));
static mut NEXT: usize = 0;

struct Bump;
unsafe impl GlobalAlloc for Bump {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let base = ARENA.0.get() as *mut u8;
        let align = layout.align();
        let start = (NEXT + align - 1) & !(align - 1);
        let end = start + layout.size();
        if end > ARENA_SIZE {
            return core::ptr::null_mut();
        }
        NEXT = end;
        base.add(start)
    }
    unsafe fn dealloc(&self, _ptr: *mut u8, _layout: Layout) {
        // bump allocator: never frees
    }
}

#[global_allocator]
static ALLOC: Bump = Bump;

// --- exported machine ABI -------------------------------------------------

/// The one global machine the JS shim drives.
static mut MACHINE: Option<Machine> = None;

/// A scratch buffer the JS side writes an image into before `nanox_load_elf`.
static mut IMAGE: Option<alloc::vec::Vec<u8>> = None;

#[no_mangle]
pub extern "C" fn nanox_new(ram_mb: u32) {
    unsafe {
        MACHINE = Some(Machine::new(ram_mb as usize * 1024 * 1024));
    }
}

/// Reserve an image buffer of `len` bytes and return a pointer for JS to fill.
#[no_mangle]
pub extern "C" fn nanox_image_alloc(len: u32) -> *mut u8 {
    unsafe {
        let mut v = alloc::vec![0u8; len as usize];
        let p = v.as_mut_ptr();
        IMAGE = Some(v);
        p
    }
}

/// Parse the staged image as ELF, load it, and boot long mode at its entry with
/// `rsp` as the stack pointer. Returns the entry, or 0 on failure.
#[no_mangle]
pub extern "C" fn nanox_boot_elf(rsp: u64) -> u64 {
    unsafe {
        let (Some(m), Some(img)) = (MACHINE.as_mut(), IMAGE.as_ref()) else {
            return 0;
        };
        let Ok(elf) = Elf::parse(img) else { return 0 };
        let entry = elf.entry;
        if m.load_elf(img).is_err() {
            return 0;
        }
        m.boot_long_mode(entry, rsp);
        entry
    }
}

/// Boot the staged image as the real ntoskrnl-rs kernel: load + relocate it
/// high-half, build the page tables + BootInfo handoff, and enter `_start`.
/// Returns 1 on success, 0 on failure.
#[no_mangle]
pub extern "C" fn nanox_boot_kernel() -> u32 {
    unsafe {
        let (Some(m), Some(img)) = (MACHINE.as_mut(), IMAGE.as_ref()) else {
            return 0;
        };
        match m.boot_kernel(img) {
            Ok(()) => 1,
            Err(_) => 0,
        }
    }
}

/// Restore a `Machine::snapshot()` blob staged via `nanox_image_alloc`, resuming
/// the guest at the state it was captured in (e.g. right at the `C:\>` prompt),
/// with no boot. Returns 1 on success, 0 on a bad blob or RAM-size mismatch.
#[no_mangle]
pub extern "C" fn nanox_restore() -> u32 {
    unsafe {
        let (Some(m), Some(img)) = (MACHINE.as_mut(), IMAGE.as_ref()) else {
            return 0;
        };
        if m.restore(img) { 1 } else { 0 }
    }
}

/// Boot at an explicit entry (for raw, already-staged code).
#[no_mangle]
pub extern "C" fn nanox_boot(entry: u64, rsp: u64) {
    unsafe {
        if let Some(m) = MACHINE.as_mut() {
            m.boot_long_mode(entry, rsp);
        }
    }
}

/// Run up to `steps` instructions. Returns a status code:
/// 0 halted, 1 max-steps, 2 unknown opcode, 3 unhandled fault, 4 syscall trap,
/// 5 breakpoint (GDB stub).
#[no_mangle]
pub extern "C" fn nanox_run(steps: u32) -> u32 {
    unsafe {
        let Some(m) = MACHINE.as_mut() else { return 0 };
        match m.run(steps as usize) {
            RunStop::Halted => 0,
            RunStop::MaxSteps => 1,
            RunStop::Unknown { .. } => 2,
            RunStop::UnhandledFault { .. } => 3,
            RunStop::Syscall => 4,
            RunStop::Breakpoint { .. } => 5,
        }
    }
}

/// Details of the last stop (valid after `nanox_run` returns a non-running
/// code): the RIP at the stop, a relevant address (CR2 for a fault), and the
/// offending opcode byte for an unknown instruction.
#[no_mangle]
pub extern "C" fn nanox_fault_rip() -> u64 {
    unsafe { MACHINE.as_ref().map_or(0, |m| m.last_rip) }
}
#[no_mangle]
pub extern "C" fn nanox_fault_addr() -> u64 {
    unsafe { MACHINE.as_ref().map_or(0, |m| m.last_addr) }
}
#[no_mangle]
pub extern "C" fn nanox_fault_byte() -> u32 {
    unsafe { MACHINE.as_ref().map_or(0, |m| m.last_byte as u32) }
}

// --- crash-dump acquisition ----------------------------------------------
// The page reads guest physical RAM directly out of wasm memory and assembles a
// Windows-format MEMORY.DMP (host-side acquisition, like a hypervisor snapshot).
// These expose the RAM window and the metadata the dump header needs.

/// Byte offset of guest physical RAM within this module's linear memory. The
/// page builds a Uint8Array view at [ptr, ptr+len) to read all of physical
/// memory. Stable for the machine's lifetime (the RAM buffer is never resized).
#[no_mangle]
pub extern "C" fn nanox_ram_ptr() -> u32 {
    unsafe { MACHINE.as_ref().map_or(0, |m| m.ram.as_ptr() as u32) }
}
/// Size of guest physical RAM in bytes.
#[no_mangle]
pub extern "C" fn nanox_ram_len() -> u32 {
    unsafe { MACHINE.as_ref().map_or(0, |m| m.ram.len() as u32) }
}
/// CR3 (DirectoryTableBase) — lets WinDbg translate virtual addresses in the dump.
#[no_mangle]
pub extern "C" fn nanox_cr3() -> u64 {
    unsafe { MACHINE.as_ref().map_or(0, |m| m.cpu.paging.cr3) }
}

/// Pop one byte the guest wrote to the UART, or -1 if none.
#[no_mangle]
pub extern "C" fn nanox_uart_read() -> i32 {
    unsafe {
        match MACHINE.as_mut().and_then(|m| m.cpu.dev.uart.tx.pop_front()) {
            Some(b) => b as i32,
            None => -1,
        }
    }
}

/// Feed a byte to the guest's UART receive queue (keyboard → COM1).
#[no_mangle]
pub extern "C" fn nanox_uart_write(byte: u8) {
    unsafe {
        if let Some(m) = MACHINE.as_mut() {
            m.cpu.dev.uart.push_rx(byte);
        }
    }
}

/// Pop one byte the guest wrote to the P9 transport (a 9P request), or -1 if
/// none. The JS 9P server drains these, assembles a message, and replies via
/// `nanox_p9_write`. See docs/9p-over-nanox.md.
#[no_mangle]
pub extern "C" fn nanox_p9_read() -> i32 {
    unsafe {
        match MACHINE.as_mut().and_then(|m| m.cpu.dev.p9.tx.pop_front()) {
            Some(b) => b as i32,
            None => -1,
        }
    }
}

/// Push one response byte for the guest to read from the P9 transport.
#[no_mangle]
pub extern "C" fn nanox_p9_write(byte: u8) {
    unsafe {
        if let Some(m) = MACHINE.as_mut() {
            m.cpu.dev.p9.rx.push_back(byte);
        }
    }
}

// --- GDB remote debugging bridge -----------------------------------------
// The page relays bytes between a debugger (lldb/gdb, via a TCP<->WebSocket
// bridge) and this stub. While `nanox_gdb_running()` is 0 the page must NOT
// call `nanox_run` — the target is halted under debugger control.

static mut GDB: Option<crate::gdb::GdbStub> = None;

/// Attach the GDB stub (idempotent). After this the page should route debugger
/// bytes through `nanox_gdb_write` and stop calling `nanox_run` unless
/// `nanox_gdb_running()` returns 1.
#[no_mangle]
pub extern "C" fn nanox_gdb_attach() {
    unsafe {
        if GDB.is_none() {
            GDB = Some(crate::gdb::GdbStub::new());
        }
        if let Some(m) = MACHINE.as_mut() {
            m.cpu.debug_break = true; // int3 (e.g. a bugcheck) now traps to the debugger
        }
    }
}

/// Detach the stub and let the machine run freely again.
#[no_mangle]
pub extern "C" fn nanox_gdb_detach() {
    unsafe {
        GDB = None;
        if let Some(m) = MACHINE.as_mut() {
            m.breakpoints.clear();
            m.cpu.debug_break = false;
        }
    }
}

/// Feed one byte received from the debugger. Complete packets are dispatched
/// immediately against the machine (may set/clear breakpoints, single-step,
/// resume, etc.).
#[no_mangle]
pub extern "C" fn nanox_gdb_write(byte: u8) {
    unsafe {
        if let (Some(g), Some(m)) = (GDB.as_mut(), MACHINE.as_mut()) {
            g.on_input(m, &[byte]);
        }
    }
}

/// Pop one byte to send back to the debugger, or -1 if none.
#[no_mangle]
pub extern "C" fn nanox_gdb_read() -> i32 {
    unsafe {
        match GDB.as_mut().and_then(|g| g.out.pop_front()) {
            Some(b) => b as i32,
            None => -1,
        }
    }
}

/// Whether the debugger has the target running (1) or halted (0). The page runs
/// the machine only while this is 1.
#[no_mangle]
pub extern "C" fn nanox_gdb_running() -> u32 {
    unsafe { GDB.as_ref().map_or(1, |g| g.running as u32) }
}

/// Tell the stub the machine just stopped (breakpoint/fault); it sends the
/// debugger a stop reply and halts. Call after `nanox_run` returns a non-running
/// code while attached.
#[no_mangle]
pub extern "C" fn nanox_gdb_notify_stop(signal: u32) {
    unsafe {
        if let Some(g) = GDB.as_mut() {
            g.report_stop(signal as u8);
        }
    }
}

/// Install a 64-bit interrupt gate (vector → handler) in the guest IDT.
#[no_mangle]
pub extern "C" fn nanox_set_idt_gate(vector: u8, handler: u64) {
    unsafe {
        if let Some(m) = MACHINE.as_mut() {
            m.set_idt_gate(vector, handler);
        }
    }
}

/// Push a PS/2 scancode from the host keyboard.
#[no_mangle]
pub extern "C" fn nanox_key(scancode: u8) {
    unsafe {
        if let Some(m) = MACHINE.as_mut() {
            m.cpu.dev.ps2.push_scancode(scancode);
        }
    }
}
