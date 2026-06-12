//! userapp2.exe — a second, independent ring-3 console program.
//!
//! Distinct from `userapp`: it does actual computation (a running sum and a
//! Fibonacci loop) and prints the results, then reports the sum to the
//! kernel test channel. Its only purpose is to demonstrate that the loader,
//! `kernel32` shim, and CRT entry run *arbitrary* console programs — there is
//! nothing bespoke about `userapp`.

#![no_std]
#![no_main]

use core::ffi::c_void;

const STD_OUTPUT_HANDLE: u32 = 0xFFFF_FFF5;

extern "C" {
    fn GetStdHandle(n_std_handle: u32) -> u64;
    fn WriteFile(h: u64, buf: *const u8, n: u32, written: *mut u32, ov: *mut c_void) -> i32;
    fn ReportTestResult(code: u64);
    fn ExitProcess(code: u32) -> !;
}

fn print(handle: u64, s: &[u8]) {
    let mut written: u32 = 0;
    unsafe {
        WriteFile(handle, s.as_ptr(), s.len() as u32, &mut written, core::ptr::null_mut());
    }
}

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

fn main() -> i32 {
    let out = unsafe { GetStdHandle(STD_OUTPUT_HANDLE) };

    // Sum 1..=100 (= 5050).
    let mut sum: u64 = 0;
    for i in 1..=100u64 {
        sum += i;
    }
    print(out, b"APP2: sum(1..=100) = ");
    print_dec(out, sum);
    print(out, b"\n");

    // Fibonacci(20) (= 6765).
    let (mut a, mut b) = (0u64, 1u64);
    for _ in 0..20 {
        let n = a + b;
        a = b;
        b = n;
    }
    print(out, b"APP2: fib(20) = ");
    print_dec(out, a);
    print(out, b"\n");

    // Report the computed sum so the kernel can verify this distinct program
    // ran correctly in ring 3.
    unsafe { ReportTestResult(sum) };
    0
}

#[no_mangle]
pub extern "C" fn mainCRTStartup() -> ! {
    let code = main();
    unsafe { ExitProcess(code as u32) }
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    loop {
        core::hint::spin_loop();
    }
}

#[no_mangle]
pub static _fltused: i32 = 0;
