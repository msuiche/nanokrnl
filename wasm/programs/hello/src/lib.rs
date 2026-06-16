//! `hello` — a guest "executable" for the WASM kernel.
//!
//! A separate wasm32 module the kernel loads and runs via `run hello`. It has
//! no hardware access of its own; it reaches the outside world only through the
//! `sys_print` syscall the kernel/host provides — the WASM analogue of a ring-3
//! program calling `NtWriteFile`. Returns an exit code from `main`, like a real
//! console program.
#![no_std]

use core::panic::PanicInfo;

#[link(wasm_import_module = "env")]
extern "C" {
    /// Kernel syscall: write `len` UTF-8 bytes at `ptr` to the console.
    fn sys_print(ptr: *const u8, len: usize);
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    core::arch::wasm32::unreachable()
}

fn print(s: &str) {
    unsafe { sys_print(s.as_ptr(), s.len()) };
}

/// Program entry. The kernel calls this and reports the returned exit code.
#[no_mangle]
pub extern "C" fn main() -> i32 {
    print("hello from a guest program running under ntoskrnl-rs\n");
    print("I'm a separate .wasm module; the kernel loaded and ran me via 'run',\n");
    print("and this line came out through the kernel's sys_print syscall.\n");
    0
}
