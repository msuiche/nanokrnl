//! worker.exe — a ring-3 program for demonstrating concurrent user threads.
//!
//! The kernel runs *two* threads on this one image at the same time. Each
//! loops `ITERATIONS` times, atomically bumping a shared kernel counter and
//! sleeping 1 ms. The `Sleep` blocks the thread, forcing the scheduler to run
//! the other one — so the two threads genuinely interleave in ring 3. After
//! both finish, the kernel checks the counter equals `ITERATIONS × 2`,
//! proving preemptive multitasking of user-mode threads.

#![no_std]
#![no_main]

const ITERATIONS: u32 = 25;

extern "C" {
    fn IncrementCounter() -> u64;
    fn Sleep(millis: u32);
    fn GetProcessHeap() -> u64;
    fn HeapAlloc(heap: u64, flags: u32, bytes: u64) -> *mut u8;
    fn HeapFree(heap: u64, flags: u32, mem: *mut u8) -> i32;
    fn ExitProcess(code: u32) -> !;
}

#[no_mangle]
pub extern "C" fn mainCRTStartup() -> ! {
    unsafe {
        let heap = GetProcessHeap();
        for _ in 0..ITERATIONS {
            // Both threads share one kernel32 heap; allocating and freeing
            // here puts the heap spinlock under genuine contention.
            let p = HeapAlloc(heap, 0, 48);
            if !p.is_null() {
                *p = 0xAA; // touch the block
                HeapFree(heap, 0, p);
            }
            IncrementCounter();
            Sleep(1); // yield so the sibling thread interleaves
        }
        ExitProcess(0);
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
