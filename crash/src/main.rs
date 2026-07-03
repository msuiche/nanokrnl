//! crash.exe — deliberately bugcheck the kernel from ring 3.
//!
//! Prints a line, then issues the `SVC_BUGCHECK` syscall directly. The kernel
//! services it by calling `KeBugCheckEx(MANUALLY_INITIATED_CRASH)`, which prints
//! the classic `*** STOP` banner over the serial console and halts the machine.
//! The web page renders that banner as a blue screen. This is the user-mode
//! analog of Windows' manually-initiated crash (the keyboard/NotMyFault path):
//! ring 3 asks, ring 0 stops the world.

#![no_std]
#![no_main]

use core::ffi::c_void;

const STD_OUTPUT_HANDLE: u32 = 0xFFFF_FFF5;
/// Must match `kernel::syscalls::SVC_BUGCHECK`.
const SVC_BUGCHECK: u64 = 35;

extern "C" {
    fn GetStdHandle(n_std_handle: u32) -> u64;
    fn WriteFile(h: u64, buf: *const u8, n: u32, written: *mut u32, ov: *mut c_void) -> i32;
}

fn print(handle: u64, s: &[u8]) {
    let mut written: u32 = 0;
    unsafe {
        WriteFile(handle, s.as_ptr(), s.len() as u32, &mut written, core::ptr::null_mut());
    }
}

fn main() -> i32 {
    let out = unsafe { GetStdHandle(STD_OUTPUT_HANDLE) };
    print(out, b"crash: forcing a bugcheck (MANUALLY_INITIATED_CRASH)...\r\n");
    // Issue the bugcheck syscall (service number in RAX, arg1 in R10 per the
    // kernel's Windows-x64 convention). Never returns.
    unsafe {
        core::arch::asm!(
            "syscall",
            in("rax") SVC_BUGCHECK,
            in("r10") 0u64,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack),
        );
    }
    0
}

#[no_mangle]
pub extern "C" fn mainCRTStartup() -> ! {
    let _ = main();
    // The bugcheck halts the machine, so we never get here; loop just in case.
    loop {
        core::hint::spin_loop();
    }
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    loop {
        core::hint::spin_loop();
    }
}

#[no_mangle]
pub static _fltused: i32 = 0;
