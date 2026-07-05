//! System initialization — `KiSystemStartup` and the two boot phases.
//!
//! NT brings the executive up in numbered phases; we keep the same shape:
//!
//! * **Phase 0** — single-threaded, interrupts off. Debug output first
//!   (everything after can report failure), then CPU tables (GDT/TSS,
//!   IDT, KPCR), then the memory manager (everything after can allocate),
//!   then interrupt controllers.
//! * **Phase 1** — the boot context is adopted as the idle thread, the
//!   scheduler comes online, interrupts open, subsystem self tests run in
//!   a real system thread, and the boot processor settles into the idle
//!   loop.
//!
//! The self tests are the kernel's boot-time proof of life: each exercises
//! one subsystem end-to-end (pool, dispatcher, timers, DPCs, I/O) and
//! reports over the debug port. Under QEMU the isa-debug-exit device turns
//! the result into the emulator's exit status, giving us a scriptable
//! "boot the kernel and assert it works".

use crate::ke::dispatcher::{ke_delay_execution_thread, ke_wait_for_single_object, DispatcherObjectType, Kevent};
use crate::ke::scheduler::{self, ke_query_tick_count};
use crate::ke::thread::Kthread;
use crate::mm::pool::pool_tag;
use crate::rtl::NtStatus;
use crate::{ex, hal, io, ke, kd_println, mm, ps};
use bootloader_api::BootInfo;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

/// Set by the self-test thread when every check has passed; the idle loop
/// watches it to know when to report success to the host.
static TESTS_DONE: AtomicBool = AtomicBool::new(false);
static TESTS_FAILED: AtomicBool = AtomicBool::new(false);

/// Enable SSE/SSE2 on the boot processor (`KiInitializeProcessor` does the
/// equivalent on real Windows). Clears `CR0.EM` and sets `CR0.MP`, then sets
/// `CR4.OSFXSR`/`CR4.OSXMMEXCPT` so SSE instructions and #XF exceptions are
/// permitted. Must run before any SSE instruction executes — loaded drivers
/// (Windows x64 ABI) rely on it.
fn enable_sse() {
    unsafe {
        let mut cr0: u64;
        core::arch::asm!("mov {}, cr0", out(reg) cr0, options(nomem, nostack));
        cr0 &= !(1 << 2); // EM = 0: no x87/SSE emulation
        cr0 |= 1 << 1; // MP = 1: monitor coprocessor
        core::arch::asm!("mov cr0, {}", in(reg) cr0, options(nomem, nostack));

        let mut cr4: u64;
        core::arch::asm!("mov {}, cr4", out(reg) cr4, options(nomem, nostack));
        cr4 |= 1 << 9; // OSFXSR: enable SSE + FXSAVE/FXRSTOR
        cr4 |= 1 << 10; // OSXMMEXCPT: unmasked SIMD FP exceptions vector to #XF
        core::arch::asm!("mov cr4, {}", in(reg) cr4, options(nomem, nostack));
    }
}

/// Enable SMEP (Supervisor Mode Execution Prevention, `CR4.SMEP`) if the CPU
/// reports support (`CPUID.(EAX=7,ECX=0):EBX[7]`). With SMEP, a fetch of an
/// instruction from a user-accessible (U/S) page while in ring 0 faults —
/// shutting down a whole class of privilege-escalation exploits. Returns
/// whether it was enabled. SMAP (the read/write counterpart) is enabled
/// separately by [`enable_smap`].
fn enable_smep() -> bool {
    unsafe {
        let leaf7 = core::arch::x86_64::__cpuid_count(7, 0);
        if leaf7.ebx & (1 << 7) == 0 {
            return false; // CPU lacks SMEP
        }
        let mut cr4: u64;
        core::arch::asm!("mov {}, cr4", out(reg) cr4, options(nomem, nostack));
        cr4 |= 1 << 20; // CR4.SMEP
        core::arch::asm!("mov cr4, {}", in(reg) cr4, options(nomem, nostack));
        true
    }
}

/// Enable SMAP (Supervisor Mode Access Prevention, `CR4.SMAP`) if supported
/// (`CPUID.(EAX=7,ECX=0):EBX[20]`). With SMAP, the kernel faults on any
/// access to a user (U/S) page unless RFLAGS.AC is set — so all kernel reads
/// of user buffers must be bracketed by `user_access_begin/end`. Returns
/// whether it was enabled.
fn enable_smap() -> bool {
    unsafe {
        let leaf7 = core::arch::x86_64::__cpuid_count(7, 0);
        if leaf7.ebx & (1 << 20) == 0 {
            return false; // CPU lacks SMAP
        }
        let mut cr4: u64;
        core::arch::asm!("mov {}, cr4", out(reg) cr4, options(nomem, nostack));
        cr4 |= 1 << 21; // CR4.SMAP
        core::arch::asm!("mov cr4, {}", in(reg) cr4, options(nomem, nostack));
        mm::virt::mm_set_smap(true);
        true
    }
}

/// `KiSystemStartup` — first Rust code after the bootloader. Never returns;
/// the boot context becomes the idle thread.
pub fn ki_system_startup(boot_info: &'static mut BootInfo) -> ! {
    // ---- Phase 0 -------------------------------------------------------
    hal::serial::init();
    kd_println!();
    kd_println!("nanokrnl 0.1.0 (x86_64) - a Windows NT-compatible kernel, written from scratch in Rust.");
    kd_println!("Boots in the browser under nanox, runs unmodified Windows console binaries on its own");
    kd_println!("NT syscalls, with a 9P host filesystem, an lldb/gdb debug stub, and crash-dump support.");
    kd_println!("By Matt Suiche (@msuiche), Fable 5, and Opus 4.8. Shout out to Fabrice Bellard.");
    kd_println!();
    kd_println!("KiSystemStartup: phase 0");

    let phys_offset = boot_info
        .physical_memory_offset
        .into_option()
        .expect("bootloader must map physical memory (see boot config)");

    // Enable SSE/SSE2. Our own kernel is a soft-float target and emits no
    // SSE, but the Windows x64 ABI mandates SSE2, so loaded drivers use it
    // (e.g. `movaps` to zero a stack buffer). Without OSFXSR/OSXMMEXCPT set
    // those instructions #UD. Real Windows enables this just as early.
    enable_sse();

    // Enable SMEP if the CPU supports it: the kernel must never execute a
    // user-accessible page, so trap any attempt. Safe here — driver images
    // are mapped supervisor-executable; only ring-3 code runs from U/S pages.
    let smep = enable_smep();
    // Enable SMAP: trap stray kernel reads/writes of user pages. The kernel's
    // deliberate user-buffer accesses are bracketed with stac/clac.
    let smap = enable_smap();

    // CPU tables. The current RSP approximates the boot stack top for
    // TSS.RSP0; it only matters for ring transitions, which cannot happen
    // before real threads (with real stacks) exist.
    let rsp: u64;
    unsafe { core::arch::asm!("mov {}, rsp", out(reg) rsp, options(nomem, nostack)) };
    ke::gdt::init(rsp);
    ke::idt::init();
    ke::pcr::init();
    ke::syscall::init();
    kd_println!(
        "KE: GDT/TSS/IDT loaded (NT selector layout), KPCR online, syscall enabled, SMEP={} SMAP={}",
        if smep { "on" } else { "off" },
        if smap { "on" } else { "off" }
    );

    // Memory: PFN bitmap, then pool rides on it implicitly.
    mm::phys::init(&boot_info.memory_regions, phys_offset);
    // Record the boot address space; per-process address spaces clone its
    // (high-half) kernel mappings.
    mm::virt::mm_save_kernel_address_space();
    // Map KUSER_SHARED_DATA into the kernel high half now, before any per-process
    // address space is cloned, so every process (and the debugger's current
    // context) sees it at 0xfffff78000000000.
    crate::dump::map_kuser_shared_data();

    // Interrupt controllers: legacy PICs masked away, APIC + clock on.
    hal::pic::init_and_mask();
    hal::apic::init(phys_offset);
    kd_println!("HAL: PIC masked, APIC enabled, clock on vector 0xD1 (CLOCK_LEVEL)");

    // ---- Phase 1 -------------------------------------------------------
    kd_println!("KiSystemStartup: phase 1");

    // Adopt this very context as the idle thread (NT does the same): no
    // forged stack — we are already running on it. stack_top stays 0;
    // it feeds TSS.RSP0 which is only consulted on user->kernel
    // transitions, and the idle thread never hosts one.
    let idle = ex::ex_allocate_object(Kthread::new(0, 0, 0, 0), pool_tag(b"Idle"))
        .expect("idle thread allocation");
    unsafe { scheduler::ki_initialize(idle) };

    // Scheduler is live: open interrupts. The clock starts preempting.
    unsafe { core::arch::asm!("sti", options(nomem, nostack)) };
    kd_println!("KE: scheduler online, interrupts enabled");

    // Self tests run in a real system thread — the idle thread must never
    // block, and the tests legitimately wait on things.
    ps::ps_create_system_thread(smoke_test_thread, core::ptr::null_mut())
        .expect("failed to create self-test thread");

    // ---- The idle loop -------------------------------------------------
    loop {
        if TESTS_DONE.load(Ordering::Acquire) {
            report_and_idle();
        }
        // hlt until the next interrupt; the dispatch interrupt will switch
        // us away whenever anything is ready to run.
        unsafe { core::arch::asm!("hlt", options(nomem, nostack)) };
    }
}

/// Print the verdict once, signal QEMU's debug-exit device, then idle
/// forever (on real hardware there is no exit — we just keep scheduling).
fn report_and_idle() -> ! {
    let failed = TESTS_FAILED.load(Ordering::Acquire);
    if failed {
        kd_println!("*** SELF TESTS FAILED ***");
        unsafe { hal::port::outl(0xF4, 0x01) }; // qemu exit code 3
    } else {
        kd_println!("ALL SELF TESTS PASSED - system idle");
        unsafe { hal::port::outl(0xF4, 0x10) }; // qemu exit code 33
    }
    loop {
        unsafe { core::arch::asm!("hlt", options(nomem, nostack)) };
    }
}

// ---------------------------------------------------------------------------
// Boot-time self tests
// ---------------------------------------------------------------------------

macro_rules! check {
    ($name:expr, $cond:expr) => {
        if $cond {
            kd_println!("  [ OK ] {}", $name);
        } else {
            kd_println!("  [FAIL] {}", $name);
            TESTS_FAILED.store(true, Ordering::Release);
        }
    };
}

/// Shared state for the event/thread test.
static PING_EVENT: crate::ke::spinlock::SpinLock<Kevent> =
    crate::ke::spinlock::SpinLock::new(Kevent::new(DispatcherObjectType::SynchronizationEvent, false));
static PONGS: AtomicU32 = AtomicU32::new(0);

/// Waiter half of the dispatcher test: wait for the ping, count a pong.
extern "C" fn pong_thread(_ctx: *mut core::ffi::c_void) -> ! {
    unsafe {
        let event = {
            // Take the event's address; it is a pinned static. The lock
            // is only used to satisfy the borrow rules around statics.
            let mut guard = PING_EVENT.lock();
            &raw mut (*guard).header
        };
        ke_wait_for_single_object(event);
    }
    PONGS.fetch_add(1, Ordering::AcqRel);
    ps::ps_terminate_system_thread();
}

/// DPC test callback.
fn dpc_routine(_dpc: *mut ke::dpc::Kdpc, context: *mut core::ffi::c_void) {
    let flag = context as *const AtomicBool;
    unsafe { (*flag).store(true, Ordering::Release) };
}

/// xorshift64 — a tiny deterministic PRNG. Deterministic on purpose: a
/// stress failure must be reproducible, not a heisenbug. (No `Math.random`
/// equivalent exists this low anyway.)
struct XorShift(u64);
impl XorShift {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
}

/// Hammer the NonPagedPool with randomized alloc/free and verify no
/// allocation ever corrupts another. Each block is filled with a byte
/// derived from its address; on free the fill is checked. At the end every
/// block is freed and the in-use byte count must return to its baseline
/// (leak check). Returns true on full success.
fn pool_stress() -> bool {
    use alloc::vec::Vec;
    const TAG: u32 = pool_tag(b"Strs");
    let baseline = mm::pool::pool_bytes_in_use();
    let mut rng = XorShift(0x9E37_79B9_7F4A_7C15);
    // (ptr, len, pattern) for each live allocation.
    let mut live: Vec<(*mut u8, usize, u8)> = Vec::new();

    for _ in 0..4000 {
        // Bias toward allocation while small, toward freeing while large,
        // so the population oscillates instead of running away.
        let alloc = live.is_empty() || (live.len() < 256 && rng.next() & 1 == 0);
        if alloc {
            // Mix of list-path and large-path sizes (1..8 KiB).
            let len = 1 + (rng.next() as usize % 8192);
            let p = mm::pool::pool_alloc(len, TAG);
            if p.is_null() {
                continue; // exhaustion is acceptable; just skip
            }
            let pat = (p as usize as u8) ^ 0x5A;
            unsafe { core::ptr::write_bytes(p, pat, len) };
            live.push((p, len, pat));
        } else {
            let idx = rng.next() as usize % live.len();
            let (p, len, pat) = live.swap_remove(idx);
            // Verify the pattern survived: any neighbor's overflow shows here.
            let ok = unsafe { core::slice::from_raw_parts(p, len) }
                .iter()
                .all(|&b| b == pat);
            if !ok {
                kd_println!("  pool_stress: corruption in block {:p} len {}", p, len);
                return false;
            }
            mm::pool::pool_free(p, TAG);
        }
    }

    // Drain everything still live and re-verify on the way out.
    for &(p, len, pat) in live.iter() {
        let ok = unsafe { core::slice::from_raw_parts(p, len) }
            .iter()
            .all(|&b| b == pat);
        if !ok {
            kd_println!("  pool_stress: corruption on drain {:p}", p);
            return false;
        }
        mm::pool::pool_free(p, TAG);
    }

    // Free the tracking Vec's own backing store *before* measuring, or its
    // (pool-allocated) capacity would read as a phantom leak. `baseline`
    // was captured when `live` was still capacity-0, so this is symmetric.
    drop(live);
    let leaked = mm::pool::pool_bytes_in_use() != baseline;
    if leaked {
        kd_println!(
            "  pool_stress: leak — {} bytes vs baseline {}",
            mm::pool::pool_bytes_in_use(),
            baseline
        );
    }
    !leaked
}

// ---------------------------------------------------------------------------
// User-mode round-trip self test (ring 3 + syscall)
// ---------------------------------------------------------------------------

/// Per-thread ring-3 start info, passed to [`user_thread_entry`] as its
/// context so multiple user threads can run concurrently (each with its own
/// entry point and stack) — no shared globals. `cr3 == 0` means run in the
/// kernel address space (image in the shared high-half window); a non-zero
/// `cr3` is a per-process address space to switch into first.
#[repr(C)]
struct UserStart {
    rip: u64,
    rsp: u64,
    cr3: u64,
    teb: u64,
}

/// Create a thread that enters ring 3 at `rip` with stack `rsp`, in the
/// kernel address space (no CR3 switch, no TEB).
fn spawn_user_thread(rip: u64, rsp: u64) -> Result<*mut ps::Ethread, crate::rtl::NtStatus> {
    let start = ex::ex_allocate_object(UserStart { rip, rsp, cr3: 0, teb: 0 }, pool_tag(b"UStr"))?;
    ps::ps_create_system_thread(user_thread_entry, start as *mut core::ffi::c_void)
}

/// Create a thread that switches into address space `cr3` (and sets the
/// user-mode GS base to `teb`, 0 for none) before entering ring 3 — a thread
/// of a per-process image.
fn spawn_process_thread(
    rip: u64,
    rsp: u64,
    cr3: u64,
    teb: u64,
) -> Result<*mut ps::Ethread, crate::rtl::NtStatus> {
    let start = ex::ex_allocate_object(UserStart { rip, rsp, cr3, teb }, pool_tag(b"UStr"))?;
    ps::ps_create_system_thread(user_thread_entry, start as *mut core::ffi::c_void)
}

/// Record the command line a freshly-spawned thread's program will see
/// (`GetCommandLine`/argv). Call before the thread first runs — on a single
/// CPU it only runs once the spawning thread blocks, so this is race-free.
fn set_cmdline(t: *mut ps::Ethread, cmdline: &'static str) {
    unsafe {
        (*t).tcb.cmdline_ptr = cmdline.as_ptr() as u64;
        (*t).tcb.cmdline_len = cmdline.len() as u32;
    }
}

/// The bare executable name inside a command line: the first whitespace-delimited
/// token, then the path component after the last `\` or `/`. `C:\cmd.exe /c dir`
/// -> `cmd.exe`.
fn exe_basename(cmdline: &[u8]) -> &[u8] {
    let end = cmdline.iter().position(|&b| b == b' ').unwrap_or(cmdline.len());
    let token = &cmdline[..end];
    let start = token
        .iter()
        .rposition(|&b| b == b'\\' || b == b'/')
        .map(|i| i + 1)
        .unwrap_or(0);
    &token[start..]
}

/// Publish the kernel-debugger view of the system: the loaded-module list and
/// the active-process list, plus `KdDebuggerDataBlock`. Called just before a
/// crash dump so the core carries a coherent snapshot a Windows debugger can
/// walk (`lm`, `!process 0 0`). See [`crate::kd`].
pub fn kd_snapshot() {
    use crate::kd;
    kd::begin();
    // The kernel itself first — its base becomes KernBase and a debugger loads
    // ntoskrnl's symbols (our DWARF, or the generated ntoskrnl.pdb) against it.
    // SizeOfImage must span the whole image: the highest kernel symbol sits near
    // RVA 0x313000 (past .text into .data/.bss), so a too-small size would leave
    // globals like PsLoadedModuleList outside the module range in `lm`.
    kd::push_module(kd::KERNEL_VIRT_BASE, 0x0040_0000, b"ntoskrnl.exe");
    // PsLoadedModuleList is the KERNEL module list (ntoskrnl + drivers). The
    // user-mode images the debug tracker knows about (cmd.exe/ntdll/kernel32/
    // msvcrt) are per-process user modules, not kernel modules - listing them
    // here (some at user VAs like 0x140000000) corrupts the debugger's
    // kernel/user address model. nanokrnl has no drivers yet, so the kernel
    // module list is just ntoskrnl.exe.
    // Active processes, from the process table.
    let tbl = PROC_TABLE.lock();
    let mut pid = 4u64;
    for e in tbl.iter() {
        if !e.in_use || e.ethread == 0 {
            continue;
        }
        let cr3 = unsafe { (*(e.ethread as *const ps::Ethread)).tcb.cr3 };
        // The PE loader maps a real PEB at a fixed user VA in every process's
        // address space (chained to the loader module list); report it so
        // `!process`/`!peb` and user-symbol loading see a valid PEB, not NULL.
        kd::push_process(pid, cr3, crate::ldr::pe::PEB_BASE, exe_basename(&e.cmdline[..e.cmdline_len]));
        pid += 4;
    }
    drop(tbl);
    kd::commit();
}

// ---------------------------------------------------------------------------
// Process creation (the CreateProcess primitive).
//
// A small process table backs created processes. Each entry owns the child's
// command-line bytes (stable static storage, so the child's `GetCommandLine`
// can point at them) and the child's initial-thread ETHREAD (stored as an
// address to keep the table `Sync`). A process "handle" is `PROC_HANDLE_BASE +
// index`; the parent waits on it and reads its exit code. This is the kernel
// side of `kernel32!CreateProcessW` (syscalls in `syscalls.rs`).
// ---------------------------------------------------------------------------

const MAX_PROCS: usize = 16;
const PROC_CMDLINE_MAX: usize = 128;
/// Process-handle base (distinct from registry `0x2000_0000` and Ob handles).
pub(crate) const PROC_HANDLE_BASE: u64 = 0x3000_0000;

#[derive(Clone, Copy)]
struct ProcEntry {
    in_use: bool,
    ethread: u64, // *mut ps::Ethread as address (kept as u64 for Sync)
    cmdline: [u8; PROC_CMDLINE_MAX],
    cmdline_len: usize,
    /// The console input mode at launch, restored when the child exits — a
    /// child (e.g. `choice`) may switch to raw single-key input and not put it
    /// back, which would leave the launching shell's line discipline broken.
    saved_console_mode: u32,
}

static PROC_TABLE: crate::ke::spinlock::SpinLock<[ProcEntry; MAX_PROCS]> =
    crate::ke::spinlock::SpinLock::new(
        [ProcEntry {
            in_use: false,
            ethread: 0,
            cmdline: [0; PROC_CMDLINE_MAX],
            cmdline_len: 0,
            saved_console_mode: 0,
        }; MAX_PROCS],
    );

/// Create a new process from a PE `image`: build its address space, spawn its
/// initial ring-3 thread, record it, and return a process handle (0 on error).
/// `cmdline` becomes the child's command line.
/// The `.mui` resource module that goes with an embedded program image, or an
/// empty slice if it has none. Matched by image length: the embedded programs
/// have distinct sizes, and `const` byte blobs are not guaranteed a single
/// address across use sites (so pointer identity is unreliable here).
fn mui_for_image(image: &[u8]) -> &'static [u8] {
    let n = image.len();
    if !CHOICE_IMAGE.is_empty() && n == CHOICE_IMAGE.len() {
        CHOICE_MUI
    } else if !WHERE_IMAGE.is_empty() && n == WHERE_IMAGE.len() {
        WHERE_MUI
    } else if !CMD_IMAGE.is_empty() && n == CMD_IMAGE.len() {
        CMD_MUI
    } else if !WHOAMI_IMAGE.is_empty() && n == WHOAMI_IMAGE.len() {
        WHOAMI_MUI
    } else {
        &[]
    }
}

pub(crate) fn create_user_process(image: &[u8], cmdline: &[u8], std_handles: [u64; 3]) -> u64 {
    // ulib.dll lives once in the shared high half, so its C-runtime data is
    // shared across processes. Reset it to its pristine post-load state so this
    // process's ulib init runs fresh — otherwise a second ulib-based program
    // (e.g. `more.com` run twice) sees stale "already initialized" CRT guards and
    // aborts at startup. Safe: user processes run serially, none is executing now.
    crate::ldr::loaded::reset_ulib_data();
    let proc = match crate::ldr::pe::load_user_process(image) {
        Ok(p) => p,
        Err(_) => return 0,
    };
    // Give this process its own copy of the shims' C-runtime data (msvcrt's fd
    // table, cached standard handles). The shim code is shared high-half code,
    // but this state must be per-process or a concurrent child's CRT init would
    // corrupt this process's - the root cause of `dir | sort` handing the wrong
    // handle to `sort`. The scheduler swaps this buffer in whenever the process
    // runs. Freed in on_user_thread_exit.
    crate::ldr::loaded::alloc_shim_data(proc.cr3.0);
    // Patch the PEB command line with the real invocation so a tool that reads
    // PEB.ProcessParameters.CommandLine directly (e.g. ulib's more.com) sees its
    // arguments. The per-thread cmdline (for the GetCommandLine syscall) is set
    // below; this covers the PEB-reading path.
    crate::ldr::pe::set_command_line(&proc, cmdline);
    let t = match spawn_process_thread(proc.entry_va, proc.user_rsp, proc.cr3.0, proc.teb) {
        Ok(t) => t,
        Err(_) => return 0,
    };
    // Give the child its program's `.mui` so its resource strings (prompts,
    // info/error messages) resolve — the same as the boot harness does for the
    // standalone runs. Matched by image identity (the const blobs are unique);
    // stored per-thread because every process shares the same image base.
    let mui = mui_for_image(image);
    if !mui.is_empty() {
        unsafe {
            (*t).tcb.mui_ptr = mui.as_ptr() as u64;
            (*t).tcb.mui_len = mui.len() as u32;
        }
    }
    // Standard handles: a stream the parent redirected (a pipe/file, nonzero)
    // is inherited; otherwise the child uses its own distinct console handle for
    // that stream. Distinct per-stream handles matter because a program that
    // closes one standard stream (cmd closing stdout after `> file`) must not
    // invalidate another (its stdin). Set both the thread state (GetStdHandle
    // syscall path) and the PEB (programs that read the handles directly).
    let mut std = std_handles;
    let child_cr3 = proc.cr3.0;
    for i in 0..3 {
        if std[i] == 0 {
            // No redirect for this stream: the child's own console handle,
            // already created in the child's table by setup_user_blocks.
            std[i] = proc.std_console[i];
        } else {
            // Inherited stream (a pipe end or file the parent redirected).
            // std[i] names it in the *parent's* (calling) handle table; the
            // child needs its own handle to the same object in the child's
            // table, so the parent closing its copy leaves the child's alive.
            let dup = match crate::ob::handle::ob_reference_object_by_handle(std[i]) {
                Ok(obj) => crate::ob::handle::ob_create_handle_in(child_cr3, obj as *mut u8, 0),
                Err(_) => 0,
            };
            std[i] = if dup != 0 { dup } else { proc.std_console[i] };
        }
    }
    unsafe { (*t).tcb.std_handles = std };
    crate::ldr::pe::set_std_handles(&proc, std);
    let mut tbl = PROC_TABLE.lock();
    let Some(i) = (0..MAX_PROCS).find(|&i| !tbl[i].in_use) else {
        return 0;
    };
    tbl[i].in_use = true;
    tbl[i].ethread = t as u64;
    tbl[i].saved_console_mode = crate::io::console::input_mode();
    let n = cmdline.len().min(PROC_CMDLINE_MAX);
    tbl[i].cmdline[..n].copy_from_slice(&cmdline[..n]);
    tbl[i].cmdline_len = n;
    // Point the child's command line at the table's stable storage (the static
    // array address is fixed; safe to use after the lock is dropped). The child
    // only runs once the creator blocks, so this is race-free.
    let cmd_ptr = (&raw const tbl[i].cmdline) as u64;
    unsafe {
        (*t).tcb.cmdline_ptr = cmd_ptr;
        (*t).tcb.cmdline_len = n as u32;
    }
    PROC_HANDLE_BASE + i as u64
}

/// Called when a user thread terminates. If it is a tracked created-process
/// thread, restore the console input mode the process was launched with — a
/// child (e.g. `choice`) may switch to raw single-key input and exit without
/// restoring it, which would leave the launching shell reading raw bytes.
pub(crate) fn on_user_thread_exit(thread: u64) {
    let mode = {
        let tbl = PROC_TABLE.lock();
        (0..MAX_PROCS)
            .find(|&i| tbl[i].in_use && tbl[i].ethread == thread)
            .map(|i| tbl[i].saved_console_mode)
    };
    if let Some(mode) = mode {
        crate::io::console::set_input_mode(mode);
    }
    // Tear down the exiting process's handle table, dropping every reference it
    // still holds and freeing the slot for reuse. `thread` is the ETHREAD; its
    // address space (cr3) keys the table.
    let cr3 = unsafe { (*(thread as *const ps::Ethread)).tcb.cr3 };
    if cr3 != 0 {
        crate::ob::handle::ob_free_table(cr3);
        crate::ldr::loaded::free_shim_data(cr3);
    }
}

fn proc_ethread(handle: u64) -> Option<*mut ps::Ethread> {
    if handle < PROC_HANDLE_BASE {
        return None;
    }
    let i = (handle - PROC_HANDLE_BASE) as usize;
    let tbl = PROC_TABLE.lock();
    if i < MAX_PROCS && tbl[i].in_use {
        Some(tbl[i].ethread as *mut ps::Ethread)
    } else {
        None
    }
}

/// Wait for a created process to terminate (up to `timeout_ms`). Returns the
/// wait NTSTATUS (0 = the process exited).
pub(crate) fn wait_user_process(handle: u64, timeout_ms: u64) -> u64 {
    let Some(t) = proc_ethread(handle) else {
        return NtStatus(0xC000_0008).0 as u64; // STATUS_INVALID_HANDLE
    };
    let st = unsafe { scheduler::ki_wait_for_object(&raw mut (*t).tcb.header, Some(timeout_ms)) };
    st.0 as u64
}

/// The exit code a created process reported (valid once it has terminated).
pub(crate) fn user_process_exit_code(handle: u64) -> u32 {
    match proc_ethread(handle) {
        Some(t) => unsafe { (*t).tcb.exit_code },
        None => 0,
    }
}

/// Message the ring-3 program writes through `NtWriteFile`.
const USER_MSG: &[u8] = b"UM: hello from a ring-3 program via NtWriteFile\n";

/// Hand-assemble a ring-3 program into a fresh page + user stack (both
/// marked user-accessible + executable). It calls `NtWriteFile(handle, buf,
/// len)` (service 2) on a pre-opened console handle, then `NtTerminateThread`
/// (service 0). The handle value is baked in as an immediate. Returns
/// `(user_rip, user_rsp)`.
fn build_user_stub(console_handle: u64) -> (u64, u64) {
    const CODE_LEN: usize = 42;

    let pa = mm::phys::mm_allocate_page().expect("user code page");
    let va = mm::phys_to_virt(pa);
    let msg_addr = va as u64 + CODE_LEN as u64;

    // NtWriteFile: r10 = handle (a1), rdx = buffer (a2), r8d = len (a3),
    // eax = 2; then NtTerminateThread: eax = 0.
    let mut code = [0u8; CODE_LEN];
    code[0] = 0x49; // movabs r10, console_handle
    code[1] = 0xBA;
    code[2..10].copy_from_slice(&console_handle.to_le_bytes());
    code[10] = 0x48; // movabs rdx, msg_addr
    code[11] = 0xBA;
    code[12..20].copy_from_slice(&msg_addr.to_le_bytes());
    code[20] = 0x41; // mov r8d, len
    code[21] = 0xB8;
    code[22..26].copy_from_slice(&(USER_MSG.len() as u32).to_le_bytes());
    code[26] = 0xB8; // mov eax, 2  (NtWriteFile)
    code[27..31].copy_from_slice(&2u32.to_le_bytes());
    code[31] = 0x0F; // syscall
    code[32] = 0x05;
    code[33] = 0xB8; // mov eax, 0  (NtTerminateThread)
    code[34..38].copy_from_slice(&0u32.to_le_bytes());
    code[38] = 0x0F; // syscall
    code[39] = 0x05;
    code[40] = 0xEB; // jmp $  (safety net)
    code[41] = 0xFE;

    unsafe {
        core::ptr::copy_nonoverlapping(code.as_ptr(), va, CODE_LEN);
        core::ptr::copy_nonoverlapping(USER_MSG.as_ptr(), va.add(CODE_LEN), USER_MSG.len());
        mm::virt::mm_set_user_executable(va as u64, CODE_LEN + USER_MSG.len());
    }

    let rsp = alloc_user_stack();
    (va as u64, rsp)
}

/// Allocate a one-page user-mode stack and return the initial RSP.
///
/// We enter ring 3 via `iretq`, which loads RSP directly, so the entry point
/// runs with exactly this value. The SysV/Win64 ABI requires RSP ≡ 8 (mod
/// 16) at a function's first instruction (the post-`call` state), so the
/// compiler's 16-byte stack-frame alignment lands correctly and aligned SSE
/// stores (`movaps`) on stack locals don't #GP. The page top is 16-aligned,
/// so we subtract 8.
fn alloc_user_stack() -> u64 {
    let spa = mm::phys::mm_allocate_page().expect("user stack page");
    let sva = mm::phys_to_virt(spa);
    unsafe { mm::virt::mm_set_user_executable(sva as u64, 4096) };
    (sva as u64 + 4096) - 8
}

/// Thread that drops itself into ring 3 at the entry/stack named by its
/// context ([`UserStart`]).
extern "C" fn user_thread_entry(ctx: *mut core::ffi::c_void) -> ! {
    // SAFETY: ctx is a UserStart from a spawn helper, live for this read.
    let (rip, rsp, cr3, teb) = unsafe {
        let s = ctx as *const UserStart;
        ((*s).rip, (*s).rsp, (*s).cr3, (*s).teb)
    };
    unsafe {
        // Switch into this process's address space (if any) before dropping
        // to ring 3. The kernel half is shared, so the code/stack executing
        // here stay mapped across the switch.
        let cur = ke::pcr::ke_get_current_thread();
        if cr3 != 0 {
            mm::virt::mm_switch_address_space(mm::PhysAddr(cr3));
            // Record it so the scheduler restores this address space whenever
            // the thread is switched back in (e.g. after a blocking syscall).
            (*cur).cr3 = cr3;
        }
        // This thread's kernel stack must be the one the syscall path switches
        // to (the context switch already set it; reassert for clarity).
        ke::pcr::set_syscall_kernel_stack((*cur).stack_top);
        ke::usermode::ki_enter_user_mode(rip, rsp, teb)
    }
}

/// The smoke-test thread: one end-to-end exercise per subsystem.
extern "C" fn smoke_test_thread(_ctx: *mut core::ffi::c_void) -> ! {
    kd_println!("KiSystemStartup: running self tests");

    // --- Cpu: SMEP hardening --------------------------------------------
    {
        let cr4: u64;
        unsafe { core::arch::asm!("mov {}, cr4", out(reg) cr4, options(nomem, nostack)) };
        check!("Cpu: SMEP enabled (kernel cannot execute user pages)", cr4 & (1 << 20) != 0);
        check!("Cpu: SMAP enabled (kernel cannot access user pages unguarded)", cr4 & (1 << 21) != 0);
    }

    // --- Mm: ProbeForRead/Write user-pointer validation -----------------
    // The syscall boundary rejects bogus/kernel pointers before the kernel
    // ever dereferences them (the confused-deputy guard).
    {
        use crate::rtl::NtStatus as S;
        use mm::virt::probe_for_read;
        // A kernel stack local is present but U/S-clear (supervisor): rejected.
        let kernel_local: u64 = 0;
        let kernel_ptr = &kernel_local as *const u64 as u64;
        check!(
            "Mm: probe rejects a kernel/supervisor pointer",
            probe_for_read(kernel_ptr, 8, 1) == Err(S::ACCESS_VIOLATION)
        );
        // An unmapped low-half address is rejected (not faulted on).
        check!(
            "Mm: probe rejects an unmapped pointer",
            probe_for_read(0x40000, 8, 1) == Err(S::ACCESS_VIOLATION)
        );
        // Address-space wraparound (overflow) is rejected, not panicked.
        check!(
            "Mm: probe rejects wraparound",
            probe_for_read(u64::MAX - 3, 16, 1) == Err(S::ACCESS_VIOLATION)
        );
        // A misaligned pointer is rejected with STATUS_DATATYPE_MISALIGNMENT.
        check!(
            "Mm: probe rejects a misaligned pointer",
            probe_for_read(0x40001, 8, 8) == Err(S::DATATYPE_MISALIGNMENT)
        );
        // A zero-length probe is a no-op even on an otherwise bad pointer.
        check!(
            "Mm: probe treats zero length as a no-op",
            probe_for_read(kernel_ptr, 0, 1).is_ok()
        );
    }

    // --- Mm: per-process address space mechanism ------------------------
    // Build a fresh address space, map a page into its (low) user half, and
    // exercise the CR3 switch: the page is visible only in that address
    // space, the kernel high half stays shared, and the original kernel
    // address space does not see the user page (isolation). This is the
    // core machinery for per-process isolation.
    {
        const UVA: u64 = 0x4000_0000; // a low-half (user) virtual address
        const PATTERN: u64 = 0xCAFE_F00D_D15E_A5E5;
        unsafe {
            let kernel_as = mm::virt::mm_kernel_address_space();
            let proc_as = mm::virt::mm_create_address_space();

            // A physical page stamped with a pattern (written via the window).
            let pa = mm::phys::mm_allocate_page().expect("test page");
            *(mm::phys_to_virt(pa) as *mut u64) = PATTERN;
            mm::virt::mm_map_user_range(proc_as, UVA, pa, 1, true, false);

            // Isolation: the user VA is NOT mapped in the kernel address space.
            let isolated = mm::virt::mm_get_physical_address(UVA).is_none();

            // Switch into the process address space and read the user VA.
            // The page is U/S, so bracket the kernel-side read for SMAP.
            mm::virt::mm_switch_address_space(proc_as);
            mm::virt::user_access_begin();
            let read_back = core::ptr::read_volatile(UVA as *const u64);
            mm::virt::user_access_end();
            let mapped_here = mm::virt::mm_get_physical_address(UVA) == Some(pa);
            // ProbeForRead accepts this genuinely user-accessible page (it is
            // present + U/S in the active address space).
            let probe_ok = mm::virt::probe_for_read(UVA, 8, 1).is_ok();
            // Kernel high half still reachable (we're executing from it).
            let kernel_shared =
                mm::virt::mm_get_physical_address(mm::phys_to_virt(pa) as u64).is_some();
            mm::virt::mm_switch_address_space(kernel_as);

            check!("Mm: address space isolates the low (user) half", isolated);
            check!(
                "Mm: per-process page mapped + CR3 switch works",
                read_back == PATTERN && mapped_here
            );
            check!("Mm: per-process address space shares the kernel high half", kernel_shared);
            check!("Mm: probe accepts a mapped user page", probe_ok);
        }
    }

    // --- Mm: pool + global allocator -----------------------------------
    {
        let before = mm::pool::pool_bytes_in_use();
        let a = mm::pool::pool_alloc(100, pool_tag(b"Tst1"));
        let b = mm::pool::pool_alloc(5000, pool_tag(b"Tst2")); // large path
        check!("Mm: pool allocations succeed", !a.is_null() && !b.is_null());
        check!("Mm: pool pointers 16-aligned", (a as usize) % 16 == 0 && (b as usize) % 16 == 0);
        mm::pool::pool_free(a, pool_tag(b"Tst1"));
        mm::pool::pool_free(b, pool_tag(b"Tst2"));
        check!("Mm: pool frees return bytes", mm::pool::pool_bytes_in_use() == before);

        let mut v: alloc::vec::Vec<u64> = alloc::vec::Vec::new();
        for i in 0..1000 {
            v.push(i);
        }
        check!("Mm: Rust alloc (Vec) over pool", v.iter().sum::<u64>() == 499_500);
    }

    // --- Mm: pool stress (random alloc/free, corruption-checked) --------
    // The host property tests cover the bitmap; this covers the pool's own
    // first-fit/split logic under churn. Each live block is stamped with a
    // byte derived from its own address and re-verified on free — any
    // overlap or split miscalculation corrupts a neighbor and is caught.
    // Returns true iff every pattern survived and all bytes were reclaimed.
    {
        check!("Mm: pool stress (4000 ops, pattern-verified, leak-checked)", pool_stress());
    }

    // --- Mm: virtual translation ----------------------------------------
    {
        let probe = mm::pool::pool_alloc(64, pool_tag(b"Tst3"));
        let pa = mm::virt::mm_get_physical_address(probe as u64);
        // Pool lives in the physical window: va == offset + pa must hold.
        let expect = (probe as u64) - (mm::phys_to_virt(mm::PhysAddr(0)) as u64);
        check!("Mm: page-table walk translates pool VA", pa == Some(mm::PhysAddr(expect)));
        mm::pool::pool_free(probe, pool_tag(b"Tst3"));
    }

    // --- Ke: clock + timers ---------------------------------------------
    {
        let t0 = ke_query_tick_count();
        ke_delay_execution_thread(5);
        let dt = ke_query_tick_count() - t0;
        check!("Ke: KeDelayExecutionThread sleeps >= requested", dt >= 5);
    }

    // --- Ke: events + threads (ping/pong) -------------------------------
    {
        let waiters = 3;
        for _ in 0..waiters {
            ps::ps_create_system_thread(pong_thread, core::ptr::null_mut())
                .expect("pong thread");
        }
        // Let the waiters reach their wait, then ping once per waiter
        // (synchronization event: each set wakes exactly one).
        ke_delay_execution_thread(5);
        for _ in 0..waiters {
            unsafe {
                let mut guard = PING_EVENT.lock();
                let ev = &mut *guard;
                ev.set();
            }
            ke_delay_execution_thread(2);
        }
        check!("Ke: sync event wakes one waiter per set", PONGS.load(Ordering::Acquire) == waiters);
    }

    // --- Ke: wait timeout -----------------------------------------------
    {
        // Wait on a never-signaled notification event with a 5-tick
        // timeout; it must return STATUS_TIMEOUT after >= 5 ticks.
        static NEVER: crate::ke::spinlock::SpinLock<Kevent> = crate::ke::spinlock::SpinLock::new(
            Kevent::new(DispatcherObjectType::NotificationEvent, false),
        );
        let hdr = unsafe { &raw mut (*NEVER.as_mut_ptr()).header };
        let t0 = ke_query_tick_count();
        let st = unsafe {
            crate::ke::dispatcher::ke_wait_for_single_object_timeout(hdr, Some(5))
        };
        let waited = ke_query_tick_count() - t0;
        check!("Ke: wait timeout returns STATUS_TIMEOUT", st == NtStatus::TIMEOUT);
        check!("Ke: wait timeout elapses requested ticks", waited >= 5);
    }

    // --- Ke: KeWaitForMultipleObjects (WaitAny / WaitAll) ---------------
    {
        static E0: crate::ke::spinlock::SpinLock<Kevent> = crate::ke::spinlock::SpinLock::new(
            Kevent::new(DispatcherObjectType::NotificationEvent, false),
        );
        static E1: crate::ke::spinlock::SpinLock<Kevent> = crate::ke::spinlock::SpinLock::new(
            Kevent::new(DispatcherObjectType::NotificationEvent, false),
        );
        let h0 = unsafe { &raw mut (*E0.as_mut_ptr()).header };
        let h1 = unsafe { &raw mut (*E1.as_mut_ptr()).header };

        // WaitAny: signal only E1, expect WAIT_0 + 1 immediately.
        unsafe { (*E1.as_mut_ptr()).set() };
        let any = unsafe {
            crate::ke::dispatcher::ke_wait_for_multiple_objects(&[h0, h1], false, Some(20))
        };
        check!("Ke: WaitForMultiple(Any) returns satisfying index",
            any == NtStatus(NtStatus::WAIT_0.0 + 1));

        // WaitAll: both signaled now → WAIT_0. (E1 still set, set E0 too.)
        unsafe { (*E0.as_mut_ptr()).set() };
        let all = unsafe {
            crate::ke::dispatcher::ke_wait_for_multiple_objects(&[h0, h1], true, Some(20))
        };
        check!("Ke: WaitForMultiple(All) succeeds when all signaled",
            all == NtStatus::WAIT_0);
    }

    // --- Ke: KMUTANT recursive ownership --------------------------------
    {
        static MUT: crate::ke::spinlock::SpinLock<crate::ke::dispatcher::Kmutant> =
            crate::ke::spinlock::SpinLock::new(crate::ke::dispatcher::Kmutant::new());
        let mp = MUT.as_mut_ptr();
        // Acquire twice (recursive), release twice — all non-blocking for
        // the owner. A missing release level would leave it owned.
        let a1 = unsafe { crate::ke::dispatcher::ke_wait_for_mutant(mp) };
        let a2 = unsafe { crate::ke::dispatcher::ke_wait_for_mutant(mp) };
        check!("Ke: mutant recursive acquire succeeds",
            a1 == NtStatus::WAIT_0 && a2 == NtStatus::WAIT_0);
        let r1 = unsafe { (*mp).release() };
        let r2 = unsafe { (*mp).release() };
        check!("Ke: mutant release balances", r1.is_ok() && r2.is_ok());
        // Now free again: a fresh acquire must still succeed.
        let a3 = unsafe { crate::ke::dispatcher::ke_wait_for_mutant(mp) };
        check!("Ke: mutant reacquirable after full release", a3 == NtStatus::WAIT_0);
        unsafe { (*mp).release().ok() };
    }

    // --- Ke: DPCs ---------------------------------------------------------
    {
        static DPC_RAN: AtomicBool = AtomicBool::new(false);
        static mut DPC: ke::dpc::Kdpc =
            ke::dpc::Kdpc::new(dpc_routine, &raw const DPC_RAN as *mut core::ffi::c_void);
        unsafe { ke::dpc::ke_insert_queue_dpc(&raw mut DPC) };
        ke_delay_execution_thread(3);
        check!("Ke: DPC queued from thread retires at DISPATCH", DPC_RAN.load(Ordering::Acquire));
    }

    // --- Io: \Device\Null round trip -------------------------------------
    {
        let device = io::null::initialize();
        check!("Io: null.sys DriverEntry + IoCreateDevice", device.is_ok());
        if let Ok(device) = device {
            let mut buf = [0xAAu8; 16];
            let wrote = unsafe {
                io::io_synchronous_request(device, io::IRP_MJ_WRITE, buf.as_mut_ptr(), buf.len())
            };
            check!(
                "Io: IRP_MJ_WRITE to \\Device\\Null consumes all bytes",
                matches!(wrote, Ok(iosb) if iosb.information == 16)
            );
            let read = unsafe {
                io::io_synchronous_request(device, io::IRP_MJ_READ, buf.as_mut_ptr(), buf.len())
            };
            check!(
                "Io: IRP_MJ_READ from \\Device\\Null reports EOF",
                matches!(read, Ok(iosb) if iosb.information == 0)
            );
        }
    }

    // --- Ob: reference counting ------------------------------------------
    {
        static DUMMY_TYPE: crate::ob::ObjectType = crate::ob::ObjectType {
            name: crate::rtl::string::UnicodeString::from_units(crate::w!("Dummy")),
            delete: None,
        };
        let obj = crate::ob::ob_create_object(&DUMMY_TYPE, 0xC0FFEEu64);
        check!("Ob: ObCreateObject", obj.is_ok());
        if let Ok(obj) = obj {
            unsafe {
                crate::ob::ob_reference_object(obj as *mut u8);
                check!("Ob: reference count tracks", crate::ob::ob_ref_count(obj as *mut u8) == 2);
                check!(
                    "Ob: type check rejects mismatch",
                    crate::ob::ob_check_type(obj as *mut u8, &io::DEVICE_TYPE)
                        == Err(NtStatus::OBJECT_TYPE_MISMATCH)
                );
                crate::ob::ob_dereference_object(obj as *mut u8);
                crate::ob::ob_dereference_object(obj as *mut u8); // frees
            }
        }
    }

    // --- User mode: handle table, console device, Nt* services ----------
    // Stand up the console device and the system-service table, then drop a
    // thread into ring 3 running a program that opens nothing itself but
    // calls NtWriteFile on a pre-opened console handle and exits. This
    // exercises the full path: iretq → syscall → SSDT → handle lookup →
    // IRP_MJ_WRITE to \Device\Console → serial, then thread termination.
    {
        crate::syscalls::register_all();
        crate::cm::init(); // seed the registry (Configuration Manager)
        crate::ldr::ntdll::init(); // ring-3 syscall trampoline for user imports
        // Load the kernel32 shim DLL so console apps can import it.
        if !KERNEL32_IMAGE.is_empty() {
            crate::ldr::loaded::load_kernel32(KERNEL32_IMAGE).expect("load kernel32.dll");
        }
        // Load the msvcrt C-runtime shim (after kernel32) so a classic-CRT
        // console binary can bind its msvcrt imports to our implementation.
        if !MSVCRT_IMAGE.is_empty() {
            crate::ldr::loaded::load_msvcrt(MSVCRT_IMAGE).expect("load msvcrt.dll");
        }
        // Load ulib.dll (after the shims it depends on): a real dependent DLL
        // whose own imports the loader binds against kernel32/msvcrt/ntdll, so
        // that ulib-based tools (more.com, …) can resolve their ulib imports.
        crate::ldr::loaded::load_ulib(ULIB_IMAGE).expect("load ulib.dll");
        let console = io::console::initialize().expect("console device");

        // Handle table: open/lookup/close round trip on the console device.
        let h = crate::ob::handle::ob_create_handle(console as *mut u8, 0);
        check!("Ob: handle table allocates a handle", h != 0);
        check!(
            "Ob: ObReferenceObjectByHandle resolves the object",
            crate::ob::handle::ob_reference_object_by_handle(h) == Ok(console as *mut u8)
        );

        // A separate, persistent handle for the ring-3 program to use.
        let user_handle = crate::ob::handle::ob_create_handle(console as *mut u8, 0);
        let before = io::console::bytes_written();
        let (rip, rsp) = build_user_stub(user_handle);
        let ut = spawn_user_thread(rip, rsp).expect("user thread");
        unsafe {
            ke::dispatcher::ke_wait_for_single_object(&raw mut (*ut).tcb.header);
        }
        check!(
            "Um: ring-3 program wrote to \\Device\\Console via NtWriteFile",
            io::console::bytes_written() - before == USER_MSG.len() as u64
        );

        // NtClose drops the handle's object reference; the slot is freed.
        check!(
            "Ob: NtClose closes the handle",
            crate::ob::handle::ob_close_handle(h) == NtStatus::SUCCESS
        );
        check!(
            "Ob: closed handle no longer resolves",
            crate::ob::handle::ob_reference_object_by_handle(h).is_err()
        );
    }

    // --- Cm: registry (Configuration Manager) ----------------------------
    {
        use crate::cm;
        const HKLM: u64 = 0x8000_0002;
        // The seeded HKLM\Software\Microsoft\Command Processor key + value.
        let cp = cm::open_key(HKLM, crate::w!("Software\\Microsoft\\Command Processor"));
        check!("Cm: open seeded key", cp != 0);
        let mut t = 0u32;
        let mut buf = [0u8; 16];
        let n = cm::query_value(cp, crate::w!("EnableExtensions"), &mut t, &mut buf);
        check!(
            "Cm: query seeded DWORD",
            n == 4 && t == cm::REG_DWORD && buf[0] == 1
        );
        // Create a key, set a value, read it back.
        let k = cm::create_key(HKLM, crate::w!("Software\\ntoskrnl-rs\\Test"));
        check!("Cm: create key", k != 0);
        check!(
            "Cm: set value",
            cm::set_value(k, crate::w!("Answer"), cm::REG_DWORD, &[42, 0, 0, 0])
        );
        let mut t2 = 0u32;
        let mut b2 = [0u8; 16];
        let n2 = cm::query_value(k, crate::w!("Answer"), &mut t2, &mut b2);
        check!("Cm: value round-trips", n2 == 4 && b2[0] == 42);
        // Reopening the created key by path yields the same handle.
        let k2 = cm::open_key(HKLM, crate::w!("Software\\ntoskrnl-rs\\Test"));
        check!("Cm: reopen created key", k2 == k);
        // Missing values/keys report absence.
        let miss = cm::query_value(cp, crate::w!("NoSuchValue"), &mut t2, &mut b2);
        check!("Cm: missing value -> absent", miss < 0);
        check!(
            "Cm: missing key -> 0",
            cm::open_key(HKLM, crate::w!("Software\\Nope")) == 0
        );
        // Enumerate subkeys of HKLM\Software (Microsoft, ntoskrnl-rs).
        let sw = cm::open_key(HKLM, crate::w!("Software"));
        let mut nm = [0u16; 32];
        check!("Cm: enum subkey", cm::enum_key(sw, 0, &mut nm) > 0);
    }

    // --- Ps: CreateProcess primitive -------------------------------------
    // Build a brand-new process from a PE image (its own address space + ring-3
    // thread), wait for it, and read its exit code — the mechanism behind
    // kernel32!CreateProcessW. Uses the compute app (reports 5050 via the test
    // channel), so a successful wait + that result proves the child truly ran.
    if !USERAPP2_IMAGE.is_empty() {
        let h = create_user_process(USERAPP2_IMAGE, b"child.exe", [0, 0, 0]);
        check!("Ps: CreateProcess returns a handle", h != 0);
        let st = wait_user_process(h, 5000);
        unsafe {
            mm::virt::mm_switch_address_space(mm::virt::mm_kernel_address_space());
        }
        check!("Ps: created process ran to exit", st == 0);
        check!(
            "Ps: child executed (reported 5050)",
            crate::syscalls::test_result() == 5050
        );
    }

    // --- Ldr: load and run a real PE/COFF driver -------------------------
    // The headline test: take a driver compiled by a *different toolchain*
    // for x86_64-pc-windows-msvc, load its PE, bind its imports to our
    // export table, run DriverEntry, and exercise the device it creates.
    {
        if DRIVER_IMAGE.is_empty() {
            kd_println!("  [SKIP] Ldr: no driver embedded (run scripts/build-driver.sh first)");
        } else {
            let name = io::AbiUnicodeString::from_units(crate::w!("\\Driver\\RustDemo"));
            let loaded = crate::ldr::ldr_load_driver(DRIVER_IMAGE, name);
            // DriverEntry exercises a timer→DPC→event handshake before
            // returning; reaching here at all means that all worked.
            check!("Ldr: load PE driver + run DriverEntry (timer/DPC/event)", loaded.is_ok());
            if let Ok(loaded) = loaded {
                let driver = loaded.driver;
                let device = unsafe { (*driver).device_object };
                check!("Ldr: loaded driver created its device", !device.is_null());

                // Resolve the device by name through the object namespace,
                // following the driver's \DosDevices symbolic link.
                let by_name = unsafe {
                    io::namespace::lookup_device(&io::AbiUnicodeString::from_units(crate::w!(
                        "\\Device\\RustDemo"
                    )))
                };
                let by_link = unsafe {
                    io::namespace::lookup_device(&io::AbiUnicodeString::from_units(crate::w!(
                        "\\DosDevices\\RustDemo"
                    )))
                };
                check!(
                    "Ldr: IoGetDeviceObjectPointer resolves name + symlink",
                    by_name == Ok(device) && by_link == Ok(device)
                );

                if !device.is_null() {
                    // Read dispatch fills the buffer with the driver's
                    // signature byte — proof the loaded PE's code executed.
                    let mut buf = [0u8; 8];
                    let r = unsafe {
                        io::io_synchronous_request(device, io::IRP_MJ_READ, buf.as_mut_ptr(), buf.len())
                    };
                    let filled = buf.iter().all(|&b| b == 0x42); // DEMO_FILL_BYTE
                    check!(
                        "Ldr: loaded driver services IRP via stack location",
                        matches!(r, Ok(iosb) if iosb.information == 8) && filled
                    );

                    // IOCTL: the driver returns its spinlock-guarded request
                    // count (it increments per IRP). Non-zero proves the
                    // IOCTL path + spinlock worked.
                    let mut out = [0u8; 8];
                    let ioctl = unsafe {
                        io::io_synchronous_ioctl(device, 0x0022_2000, out.as_mut_ptr(), 0, 8)
                    };
                    let count = u64::from_le_bytes(out);
                    check!(
                        "Ldr: loaded driver handles IOCTL (spinlock-guarded count)",
                        matches!(ioctl, Ok(iosb) if iosb.information == 8) && count >= 1
                    );
                }

                // Unload: runs DriverUnload (deletes the symlink + device),
                // then frees the image. The name must no longer resolve.
                unsafe { crate::ldr::ldr_unload_driver(&loaded) };
                let after = unsafe {
                    io::namespace::lookup_device(&io::AbiUnicodeString::from_units(crate::w!(
                        "\\DosDevices\\RustDemo"
                    )))
                };
                check!("Ldr: DriverUnload removed the symbolic link", after.is_err());
            }
        }
    }

    // --- Ldr: load a real Microsoft kernel driver (null.sys) -------------
    // Unlike testdriver.sys (our own Rust driver), this is an unmodified
    // Windows binary. It binds its `ntoskrnl.exe` imports to our export table
    // and registers \Device\Null. Reported (not gated): it depends on a
    // hand-staged binary and may surface imports we haven't exported yet —
    // the loader logs any unresolved ones, which is exactly the signal we want.
    #[cfg(not(feature = "interactive"))]
    {
        if NULL_SYS_IMAGE.is_empty() {
            kd_println!("  [SKIP] Ldr: no null.sys staged (drop one in drivers/)");
        } else {
            kd_println!("NULL.SYS: loading real Microsoft null.sys ({} bytes)", NULL_SYS_IMAGE.len());
            let name = io::AbiUnicodeString::from_units(crate::w!("\\Driver\\Null"));
            match crate::ldr::ldr_load_driver(NULL_SYS_IMAGE, name) {
                Ok(loaded) => {
                    let device = unsafe { (*loaded.driver).device_object };
                    kd_println!(
                        "NULL.SYS: DriverEntry ran; device_object={:p}",
                        device
                    );
                    // \Device\Null should now resolve; a write consumes all
                    // bytes and a read returns end-of-file (0 bytes).
                    let by_name = unsafe {
                        io::namespace::lookup_device(&io::AbiUnicodeString::from_units(crate::w!(
                            "\\Device\\Null"
                        )))
                    };
                    if let Ok(dev) = by_name {
                        let src = [0u8; 4];
                        let w = unsafe {
                            io::io_synchronous_request(dev, io::IRP_MJ_WRITE, src.as_ptr() as *mut u8, src.len())
                        };
                        let mut dst = [0xFFu8; 4];
                        let r = unsafe {
                            io::io_synchronous_request(dev, io::IRP_MJ_READ, dst.as_mut_ptr(), dst.len())
                        };
                        kd_println!(
                            "NULL.SYS: \\Device\\Null write={:?} read={:?}",
                            w.map(|s| s.information),
                            r.map(|s| s.information)
                        );
                    } else {
                        kd_println!("NULL.SYS: \\Device\\Null did not resolve");
                    }
                    unsafe { crate::ldr::ldr_unload_driver(&loaded) };
                }
                Err(e) => kd_println!("NULL.SYS: load/DriverEntry failed: {:?}", e),
            }
        }
    }

    // --- User mode: load and run a real PE console application -----------
    // The capstone: take a PE executable compiled for Windows, map it into
    // ring-3 pages with the user-mode loader, run it as a user thread, and
    // let it open the console and print via syscalls — a console program
    // loaded from a PE and executed in user mode.
    {
        if USERAPP_IMAGE.is_empty() {
            kd_println!("  [SKIP] Um: no userapp embedded (run scripts/build-userapp.sh)");
        } else {
            match crate::ldr::pe::load_user(USERAPP_IMAGE) {
                Ok(image) => {
                    kd_println!(
                        "UM: mapped console app @ {:p} ({} bytes), entry @ {:#X}",
                        image.base,
                        image.size,
                        image.entry_va
                    );
                    let before = io::console::bytes_written();
                    let app = spawn_user_thread(image.entry_va, alloc_user_stack())
                        .expect("app thread");
                    set_cmdline(app, "userapp.exe alpha beta");
                    unsafe {
                        ke::dispatcher::ke_wait_for_single_object(&raw mut (*app).tcb.header);
                    }
                    // The app printed argc + each arg (console output grew),
                    // then ran a heap-reuse test and reported the verdict over
                    // the kernel test channel — a robust assertion that
                    // doesn't depend on exact console byte counts.
                    check!(
                        "Um: CRT console app ran (argv printed to console)",
                        io::console::bytes_written() - before > 40
                    );
                    check!(
                        "Um: kernel32 heap + Sleep timing + env var read",
                        crate::syscalls::test_result() == 0xABCD
                    );
                }
                Err(e) => {
                    kd_println!("  [FAIL] Um: load_user returned {:?}", e);
                    TESTS_FAILED.store(true, Ordering::Release);
                }
            }
        }

        // A second, independent console program — same loader, kernel32, and
        // CRT path, different code. Proves the system runs arbitrary apps.
        if !USERAPP2_IMAGE.is_empty() {
            match unsafe { run_user_pe(USERAPP2_IMAGE) } {
                Ok(()) => check!(
                    "Um: a different console app (userapp2) computed correctly",
                    crate::syscalls::test_result() == 5050 // sum(1..=100)
                ),
                Err(e) => {
                    kd_println!("  [FAIL] Um: userapp2 load returned {:?}", e);
                    TESTS_FAILED.store(true, Ordering::Release);
                }
            }
        }
    }

    // --- Um: concurrent ring-3 threads ----------------------------------
    // Run TWO threads on the same worker image at once. Each loops
    // incrementing a shared kernel counter and sleeping; the sleeps make the
    // scheduler interleave them. If both run to completion the counter equals
    // ITERATIONS(25) × 2 = 50 — proof of preemptive user-mode multitasking.
    {
        if !WORKER_IMAGE.is_empty() {
            match crate::ldr::pe::load_user(WORKER_IMAGE) {
                Ok(loaded) => {
                    let t1 = spawn_user_thread(loaded.entry_va, alloc_user_stack())
                        .expect("worker 1");
                    let t2 = spawn_user_thread(loaded.entry_va, alloc_user_stack())
                        .expect("worker 2");
                    unsafe {
                        ke::dispatcher::ke_wait_for_single_object(&raw mut (*t1).tcb.header);
                        ke::dispatcher::ke_wait_for_single_object(&raw mut (*t2).tcb.header);
                    }
                    check!(
                        "Um: two concurrent ring-3 threads (preemptive multitasking)",
                        crate::syscalls::counter_value() == 50
                    );
                }
                Err(e) => {
                    kd_println!("  [FAIL] Um: worker load returned {:?}", e);
                    TESTS_FAILED.store(true, Ordering::Release);
                }
            }
        }

        // Full per-process isolation: load a console app into its OWN
        // address space (image + stack mapped in the low half) and run it in
        // ring 3 under its own CR3. It still reaches the shared high-half
        // kernel32/ntdll stubs, so it runs normally — but its image is mapped
        // only in its address space.
        if !USERAPP_IMAGE.is_empty() {
            match crate::ldr::pe::load_user_process(USERAPP_IMAGE) {
                Ok(proc) => {
                    kd_println!("UM: process @ entry {:#X} (own address space)", proc.entry_va);
                    let before = io::console::bytes_written();
                    let t = spawn_process_thread(proc.entry_va, proc.user_rsp, proc.cr3.0, proc.teb)
                        .expect("process thread");
                    set_cmdline(t, "userapp.exe alpha beta");
                    unsafe {
                        ke::dispatcher::ke_wait_for_single_object(&raw mut (*t).tcb.header);
                        // Return to the kernel address space for the rest of
                        // the tests.
                        mm::virt::mm_switch_address_space(mm::virt::mm_kernel_address_space());
                    }
                    check!(
                        "Um: app ran in its own address space (ring 3, isolated)",
                        io::console::bytes_written() > before
                    );
                    // The app's low-half image is not mapped in the kernel AS.
                    check!(
                        "Um: per-process image is isolated from the kernel AS",
                        mm::virt::mm_get_physical_address(proc.entry_va).is_none()
                    );
                }
                Err(e) => {
                    kd_println!("  [FAIL] Um: load_user_process returned {:?}", e);
                    TESTS_FAILED.store(true, Ordering::Release);
                }
            }
        }

        // Two concurrent *processes*: load the worker into TWO separate
        // address spaces and run them at once. They interleave (each worker
        // sleeps), so the scheduler switches CR3 between two distinct user
        // address spaces on every context switch. Both bump the shared kernel
        // counter 25× → +50 proves both isolated processes ran.
        if !WORKER_IMAGE.is_empty() {
            let before = crate::syscalls::counter_value();
            let p1 = crate::ldr::pe::load_user_process(WORKER_IMAGE);
            let p2 = crate::ldr::pe::load_user_process(WORKER_IMAGE);
            match (p1, p2) {
                (Ok(a), Ok(b)) => {
                    let t1 = spawn_process_thread(a.entry_va, a.user_rsp, a.cr3.0, a.teb)
                        .expect("process 1");
                    let t2 = spawn_process_thread(b.entry_va, b.user_rsp, b.cr3.0, b.teb)
                        .expect("process 2");
                    unsafe {
                        ke::dispatcher::ke_wait_for_single_object(&raw mut (*t1).tcb.header);
                        ke::dispatcher::ke_wait_for_single_object(&raw mut (*t2).tcb.header);
                        mm::virt::mm_switch_address_space(mm::virt::mm_kernel_address_space());
                    }
                    check!(
                        "Um: two concurrent processes, each in its own address space",
                        crate::syscalls::counter_value() - before == 50
                    );
                    check!(
                        "Um: the two processes have distinct address spaces",
                        a.cr3.0 != b.cr3.0
                    );
                }
                _ => {
                    kd_println!("  [FAIL] Um: two-process load failed");
                    TESTS_FAILED.store(true, Ordering::Release);
                }
            }
        }
    }

    // --- Ldr: malformed-image robustness --------------------------------
    // The loader must reject bad images cleanly (return Err), never crash —
    // important when loading untrusted or corrupt programs.
    {
        let garbage = [0xCCu8; 128];
        check!(
            "Ldr: rejects a non-PE image",
            crate::ldr::pe::load_user(&garbage).is_err()
        );
        let mut mz_only = [0u8; 128];
        mz_only[0] = b'M';
        mz_only[1] = b'Z'; // valid DOS magic, but e_lfanew points at zeros (no PE)
        check!(
            "Ldr: rejects an MZ image with no PE header",
            crate::ldr::pe::load_user(&mz_only).is_err()
        );
        check!(
            "Ldr: rejects an empty image",
            crate::ldr::pe::load_user(&[]).is_err()
        );
    }

    // --- Experimental: run a REAL Windows console binary (sort.exe) -----
    // Loads an unmodified Microsoft sort.exe and binds its kernel32/msvcrt/
    // ntdll imports to our shims. stdin is /dev/null here (no EOF), so sort
    // blocks reading input after startup — reaching that point means the real
    // MSVC CRT startup plus our shims executed. We wait with a timeout and
    // only report (never gate the suite).
    #[cfg(not(feature = "interactive"))]
    if !SORT_IMAGE.is_empty() {
        kd_println!("SORT: loading real sort.exe ({} bytes)", SORT_IMAGE.len());
        match crate::ldr::pe::load_user_process(SORT_IMAGE) {
            Ok(proc) => {
                kd_println!("SORT: mapped, entry {:#X}, cr3 {:#X}", proc.entry_va, proc.cr3.0);
                // Feed unsorted lines on stdin, then EOF, so sort reads them,
                // sorts, and writes the result to the console before exiting.
                io::console::push_input_str(b"cherry\r\nbanana\r\napple\r\ndate\r\n");
                io::console::set_input_eof(true);
                let wr_before = io::console::bytes_written();
                kd_println!("SORT: stdin = [cherry, banana, apple, date]; expecting sorted output:");
                match spawn_process_thread(proc.entry_va, proc.user_rsp, proc.cr3.0, proc.teb) {
                    Ok(t) => {
                        set_cmdline(t, "sort.exe");
                        let st = unsafe {
                            scheduler::ki_wait_for_object(&raw mut (*t).tcb.header, Some(5000))
                        };
                        unsafe {
                            mm::virt::mm_switch_address_space(mm::virt::mm_kernel_address_space());
                        }
                        io::console::set_input_eof(false);
                        let (rc, rb) = io::console::read_stats();
                        kd_println!(
                            "SORT: wait -> {:#X} ({} bytes written; {} read calls, {} bytes read)",
                            st.0,
                            io::console::bytes_written() - wr_before,
                            rc,
                            rb
                        );
                    }
                    Err(e) => kd_println!("SORT: spawn failed {:?}", e),
                }
            }
            Err(e) => kd_println!("SORT: load_user_process failed {:?}", e),
        }
    }

    // --- Experimental: choice.exe (interactive console binary) -----------
    // choice loads + runs against our shims and resolves its UI strings from
    // its external `choice.exe.mui` via the MUI subsystem. Since the
    // version-info + wide-printf fixes it now gets PAST startup and reaches its
    // genuine interactive prompt ("[Y,N]?"), then waits for a keypress. choice
    // polls console INPUT EVENTS (PeekConsoleInputW) + console-mode state that
    // we don't fully model yet, so it never accepts our fed "Y" and never
    // exits cleanly. It reads its keypress by polling PeekConsoleInputW (which
    // reports a synthetic KEY_EVENT when console input is buffered) then
    // ReadConsoleW. We feed "Y\r\n", so it picks Y and exits 0. (This worked
    // once the stale-std-handle bug was fixed: GetStdHandle now re-opens the
    // console when a prior process — sort, which closes stdin — left a closed
    // handle cached in shared kernel32 .data.)
    #[cfg(not(feature = "interactive"))]
    if !CHOICE_IMAGE.is_empty() {
        kd_println!("CHOICE: loading real choice.exe ({} bytes)", CHOICE_IMAGE.len());
        match crate::ldr::pe::load_user_process(CHOICE_IMAGE) {
            Ok(proc) => {
                kd_println!("CHOICE: mapped, entry {:#X}; feeding 'Y':", proc.entry_va);
                // Register choice's .mui so LoadStringW finds its prompt strings.
                if !CHOICE_MUI.is_empty() {
                    crate::ldr::mui::register(proc.image_base, CHOICE_MUI);
                }
                io::console::push_input_str(b"Y\r\n");
                io::console::set_input_eof(true);
                let wr_before = io::console::bytes_written();
                match spawn_process_thread(proc.entry_va, proc.user_rsp, proc.cr3.0, proc.teb) {
                    Ok(t) => {
                        set_cmdline(t, "choice.exe");
                        let st = unsafe {
                            scheduler::ki_wait_for_object(&raw mut (*t).tcb.header, Some(5000))
                        };
                        unsafe {
                            mm::virt::mm_switch_address_space(mm::virt::mm_kernel_address_space());
                        }
                        io::console::set_input_eof(false);
                        kd_println!(
                            "\nCHOICE: wait -> {:#X} ({} bytes written)",
                            st.0,
                            io::console::bytes_written() - wr_before
                        );
                    }
                    Err(e) => kd_println!("CHOICE: spawn failed {:?}", e),
                }
            }
            Err(e) => kd_println!("CHOICE: load_user_process failed {:?}", e),
        }
    }

    // --- Experimental: where.exe (search tool, exercises FS + MUI + argv) -
    // Runs `where cmd`. With no directory-enumeration model, FindFirstFile
    // reports no matches, so `where` searches and prints its "Could not find
    // files for the given pattern(s)" message — loaded from where.exe.mui.
    #[cfg(not(feature = "interactive"))]
    if !WHERE_IMAGE.is_empty() {
        kd_println!("WHERE: loading real where.exe ({} bytes)", WHERE_IMAGE.len());
        match crate::ldr::pe::load_user_process(WHERE_IMAGE) {
            Ok(proc) => {
                kd_println!("WHERE: mapped, entry {:#X}; running `where cmd`:", proc.entry_va);
                if !WHERE_MUI.is_empty() {
                    crate::ldr::mui::register(proc.image_base, WHERE_MUI);
                }
                // Debugger: build the module map (image + shim libraries) so a
                // one-line `ke::debug::arm(N)` / `arm_with_trigger(N, rdx)` here
                // re-enables the single-step API trace when troubleshooting.
                // Left DISARMED on normal boots: single-stepping where.exe to
                // completion logs every API call over serial and is far too slow
                // for the boot suite (it pushes qemu-test past its timeout).
                ke::debug::clear_modules();
                ke::debug::add_module("where.exe", proc.image_base, proc.image_size, false);
                let (k32b, k32s) = crate::ldr::loaded::kernel32_range();
                ke::debug::add_module("kernel32", k32b, k32s as u64, true);
                let (mcb, mcs) = crate::ldr::loaded::msvcrt_range();
                ke::debug::add_module("msvcrt", mcb, mcs as u64, true);
                ke::debug::add_module("ntdll", crate::ldr::ntdll::trampoline_base(), 0x1000, true);
                                let wr_before = io::console::bytes_written();
                match spawn_process_thread(proc.entry_va, proc.user_rsp, proc.cr3.0, proc.teb) {
                    Ok(t) => {
                        set_cmdline(t, "where.exe cmd");
                        let st = unsafe {
                            scheduler::ki_wait_for_object(&raw mut (*t).tcb.header, Some(5000))
                        };
                        unsafe {
                            mm::virt::mm_switch_address_space(mm::virt::mm_kernel_address_space());
                        }
                        kd_println!(
                            "\nWHERE: wait -> {:#X} ({} bytes written)",
                            st.0,
                            io::console::bytes_written() - wr_before
                        );
                    }
                    Err(e) => kd_println!("WHERE: spawn failed {:?}", e),
                }
            }
            Err(e) => kd_println!("WHERE: load_user_process failed {:?}", e),
        }
    }

    // --- Experimental: cmd.exe (apiset/ucrt-linked) ----------------------
    // Goal target. cmd imports api-ms-win-* (-> our kernel32 by name) + ucrt
    // (-> our msvcrt). Unimplemented imports bind to a return-0 stub (logged at
    // load), and a ring-3 fault now terminates just this thread, so cmd can run
    // as far as our surface allows and we can see the first real wall in the
    // armed API trace. Building toward CreateProcess + the registry.
    if !CMD_IMAGE.is_empty() {
        kd_println!("CMD: loading real cmd.exe ({} bytes)", CMD_IMAGE.len());
        match crate::ldr::pe::load_user_process(CMD_IMAGE) {
            Ok(proc) => {
                kd_println!("CMD: mapped, entry {:#X}", proc.entry_va);
                // Register cmd.exe.mui so its messages (banner, errors, prompts)
                // resolve through FormatMessage/the message table.
                if !CMD_MUI.is_empty() {
                    crate::ldr::mui::register(proc.image_base, CMD_MUI);
                }
                ke::debug::clear_modules();
                ke::debug::add_module("cmd.exe", proc.image_base, proc.image_size, false);
                let (k32b, k32s) = crate::ldr::loaded::kernel32_range();
                ke::debug::add_module("kernel32", k32b, k32s as u64, true);
                let (mcb, mcs) = crate::ldr::loaded::msvcrt_range();
                ke::debug::add_module("msvcrt", mcb, mcs as u64, true);
                ke::debug::add_module("ntdll", crate::ldr::ntdll::trampoline_base(), 0x1000, true);
                // Tracer left disarmed so cmd's own console output is clean;
                // re-enable with `ke::debug::arm(N)` to trace API calls.
                #[cfg(not(feature = "interactive"))]
                {
                    // Deterministic smoke test: feed a builtin + exit, then EOF.
                    io::console::push_input_str(b"echo hi\r\nexit\r\n");
                    io::console::set_input_eof(true);
                }
                #[cfg(feature = "interactive")]
                {
                    // Interactive: no canned input and no EOF — cmd reads live
                    // keystrokes from the serial console until the user types
                    // `exit`. Echo + line editing happen in the console driver.
                    kd_println!("\n--- interactive cmd.exe: type commands, `exit` to quit ---\n");
                }
                let wr_before = io::console::bytes_written();
                match spawn_process_thread(proc.entry_va, proc.user_rsp, proc.cr3.0, proc.teb) {
                    Ok(t) => {
                        set_cmdline(t, "cmd.exe");
                        // Give cmd its distinct stdin/stdout/stderr console handles
                        // (the GetStdHandle syscall path), so closing one stream's
                        // handle during a redirect does not invalidate another.
                        unsafe { (*t).tcb.std_handles = proc.std_console };
                        #[cfg(not(feature = "interactive"))]
                        let st = unsafe {
                            scheduler::ki_wait_for_object(&raw mut (*t).tcb.header, Some(8000))
                        };
                        #[cfg(feature = "interactive")]
                        let st = unsafe {
                            // Wait until cmd exits — the session ends when the
                            // user types `exit`.
                            scheduler::ki_wait_for_object(&raw mut (*t).tcb.header, None)
                        };
                        unsafe {
                            mm::virt::mm_switch_address_space(mm::virt::mm_kernel_address_space());
                        }
                        io::console::set_input_eof(false);
                        kd_println!(
                            "\nCMD: wait -> {:#X} ({} bytes written)",
                            st.0,
                            io::console::bytes_written() - wr_before
                        );
                    }
                    Err(e) => kd_println!("CMD: spawn failed {:?}", e),
                }
            }
            Err(e) => kd_println!("CMD: load_user_process failed {:?}", e),
        }
    }

    TESTS_DONE.store(true, Ordering::Release);
    ps::ps_terminate_system_thread();
}

/// The embedded PE test driver, located by `build.rs`. Empty when no driver
/// has been built (see `scripts/build-driver.sh`); the load test skips then.
static DRIVER_IMAGE: &[u8] = include_bytes!(env!("NTOS_DRIVER_IMAGE"));

/// A real Microsoft kernel driver (`null.sys`), staged by hand in `drivers/`.
/// Empty when not present; the load test reports "skipped" then. Used to
/// exercise loading an unmodified `.sys` against our `ntoskrnl.exe` exports.
static NULL_SYS_IMAGE: &[u8] = include_bytes!(env!("NTOS_NULL_SYS_IMAGE"));

/// The embedded ring-3 console app, located by `build.rs`. Empty when not
/// built (see `scripts/build-userapp.sh`); the run test skips then.
static USERAPP_IMAGE: &[u8] = include_bytes!(env!("NTOS_USERAPP_IMAGE"));

/// The embedded kernel32 shim DLL, located by `build.rs`. Empty when not
/// built (see `scripts/build-kernel32.sh`).
static KERNEL32_IMAGE: &[u8] = include_bytes!(env!("NTOS_KERNEL32_IMAGE"));
static MSVCRT_IMAGE: &[u8] = include_bytes!(env!("NTOS_MSVCRT_IMAGE"));
/// `ulib.dll` — real Microsoft utility library (dependent DLL for more.com et
/// al.), loaded as a module so consumers' `ulib` imports resolve.
static ULIB_IMAGE: &[u8] = include_bytes!(env!("NTOS_ULIB_IMAGE"));

/// A real Windows console binary (`sort.exe`), embedded for the experiment of
/// running an unmodified `.exe` against our kernel32 + msvcrt shims. Empty
/// when not present (see `winbin/`).
// `pub(crate) const` so the RAM filesystem ([`crate::io::ramfs`]) can expose
// these same embedded images as files (e.g. `C:\cmd.exe`) without a second
// `include_bytes!` copy — a `const &[u8]` references the one emitted blob.
pub(crate) const SORT_IMAGE: &[u8] = include_bytes!(env!("NTOS_SORT_IMAGE"));
pub(crate) const CHOICE_IMAGE: &[u8] = include_bytes!(env!("NTOS_CHOICE_IMAGE"));
static CHOICE_MUI: &[u8] = include_bytes!(env!("NTOS_CHOICE_MUI_IMAGE"));
pub(crate) const WHERE_IMAGE: &[u8] = include_bytes!(env!("NTOS_WHERE_IMAGE"));
static WHERE_MUI: &[u8] = include_bytes!(env!("NTOS_WHERE_MUI_IMAGE"));
pub(crate) const CMD_IMAGE: &[u8] = include_bytes!(env!("NTOS_CMD_IMAGE"));
static CMD_MUI: &[u8] = include_bytes!(env!("NTOS_CMD_MUI_IMAGE"));
/// `more.com` — the console pager (a PE despite the `.com` name). Needs
/// `ulib.dll` to actually run; embedded so it can be enumerated and launched
/// once dependent-DLL loading exists.
pub(crate) const MORE_IMAGE: &[u8] = include_bytes!(env!("NTOS_MORE_IMAGE"));
/// `whoami.exe` — prints the current user/token. Needs advapi32/authz token
/// APIs to actually run; embedded so it enumerates and launches.
pub(crate) const WHOAMI_IMAGE: &[u8] = include_bytes!(env!("NTOS_WHOAMI_IMAGE"));
static WHOAMI_MUI: &[u8] = include_bytes!(env!("NTOS_WHOAMI_MUI_IMAGE"));

/// A second, independent console app — proves the loader runs arbitrary
/// programs. Empty when not built (see `scripts/build-userapp2.sh`).
pub(crate) const USERAPP2_IMAGE: &[u8] = include_bytes!(env!("NTOS_USERAPP2_IMAGE"));

/// `crash.exe` — a ring-3 program that deliberately bugchecks the machine, so a
/// visitor can trigger a blue screen from the shell. Empty when not built (see
/// `scripts/build-crash.sh`).
pub(crate) const CRASH_IMAGE: &[u8] = include_bytes!(env!("NTOS_CRASH_IMAGE"));

/// The worker app — run by two concurrent threads to demonstrate preemptive
/// user-mode multitasking. Empty when not built (`scripts/build-worker.sh`).
static WORKER_IMAGE: &[u8] = include_bytes!(env!("NTOS_WORKER_IMAGE"));

/// Load a user PE, run it to completion as a ring-3 thread, and return.
/// Shared by the console-app self tests.
///
/// # Safety
/// Call at PASSIVE_LEVEL from a thread (it blocks waiting for the app).
unsafe fn run_user_pe(image: &[u8]) -> Result<(), crate::rtl::NtStatus> {
    let loaded = crate::ldr::pe::load_user(image)?;
    let app = spawn_user_thread(loaded.entry_va, alloc_user_stack())?;
    unsafe { ke::dispatcher::ke_wait_for_single_object(&raw mut (*app).tcb.header) };
    Ok(())
}
