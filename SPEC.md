# nanokrnl — NT-compatible kernel specification

`nanokrnl` (the repo/project; `github.com/msuiche/nanokrnl`) is an
**NT-compatible kernel written in Rust** — the architecture, abstractions, and
(where it matters) the exact constants and layouts of the Windows NT kernel,
rebuilt as a memory-safe, freestanding Rust codebase that boots on x86-64 and
proves itself with self tests on every boot. This document specifies what it is,
how it boots, its subsystems, the system-call surface, how it runs real user
binaries, how it is tested, and how to build and run it.

> **Naming.** The project is **nanokrnl**. The browser emulator it runs under is
> **nanox** (crate in `emu/`, specified separately in `emu/SPEC.md` — not
> re-specified here). The kernel's on-disk boot banner literally prints
> `ntoskrnl-rs 0.1.0` (the codebase predates the rename; no code is renamed),
> so it **boots as `ntoskrnl-rs`**.

---

## 1. What nanokrnl is / is not

- **It is** a freestanding x86-64 kernel (`no_std`, `no_main`) targeting
  `x86_64-unknown-none`, faithful to NT's *kernel architecture* and bit-exact
  where the bits are ABI.
- **It is not** binary-compatible with Windows in general, single-image Windows.
  It is single-processor (boot CPU only; structures are per-PRCB-shaped),
  NonPagedPool-only (no paging-out), and exports a broad-but-partial subset of
  `ntoskrnl.exe`.

**Bit-exact where it's ABI.**
- `NTSTATUS` values and `NT_SUCCESS` severity rules (`rtl/status.rs`).
- `LIST_ENTRY` two-pointer layout + `CONTAINING_RECORD` (`rtl/list.rs`).
- `UNICODE_STRING` 16-byte x64 layout (`rtl/string.rs`).
- The x64 GDT **selector layout** chosen by NT so `syscall`/`sysret` work
  (`ke/selectors.rs`): `KGDT64_NULL=0x00`, `R0_CODE=0x10`, `R0_DATA=0x18`,
  `R3_CMCODE=0x20`, `R3_DATA=0x28`, `R3_CODE=0x30`, `SYS_TSS=0x40`.
- The IRQL model and hardware mapping (`ke/irql.rs`): IRQL **is** CR8/TPR; a
  vector is delivered iff `vector >> 4 > IRQL`; the clock runs on **vector
  0xD1** (`CLOCK_LEVEL = 13`), as on NT x64. Levels: `PASSIVE=0`, `APC=1`,
  `DISPATCH=2`, `CLOCK=13`, `IPI=14`, `HIGH=15`.

**Faithful in shape where bit-compat doesn't matter.** Dispatcher objects with
a common header and one wait API; DPCs queued from ISRs and retired at
`DISPATCH_LEVEL`; the driver/device/IRP triangle with dispatch tables; tagged
NonPagedPool with a 16-byte header; bugchecks that freeze the world.

**Modern & safe by construction.** `unsafe` is concentrated at the hardware
boundary and in intrusive data structures, each with an explicit safety
contract. `SpinLock<T>` owns its data and raises IRQL by construction.
`Box`/`Vec`/`String` work in-kernel, drawing from NonPagedPool with the `'Rust'`
tag.

---

## 2. Boot & handoff

The boot-image builder is the upstream **`bootloader` crate (~0.11.x)**; the
kernel links **`bootloader_api` 0.11** for the handoff ABI.

**Boot config** (`kernel/src/main.rs`, `BOOTLOADER_CONFIG`): all kernel mappings
in the **high canonical half** (NT-style). `dynamic_range_start =
0xFFFF_8000_0000_0000`, `dynamic_range_end = 0xFFFF_FEFF_FFFF_FFFF`; the
all-physical-memory window is a `FixedAddress(0xFFFF_FF00_0000_0000)`. Boot stack
256 KiB (phase-0/1 init runs deep call chains before any thread exists). The
entire **low half** (`< 0x8000_0000_0000`) is left free for per-process user
mappings. `entry_point!(kernel_main)` registers the entry; `kernel_main` forwards
to `kernel::init::ki_system_startup(boot_info)`.

**`KiSystemStartup`** (`kernel/src/init.rs`) never returns; the boot context
becomes the idle thread.

- **Phase 0** (single-threaded, interrupts off): serial up first → read
  `physical_memory_offset` from `BootInfo` → `enable_sse()` (clear CR0.EM, set
  CR0.MP, CR4.OSFXSR/OSXMMEXCPT — the Windows x64 ABI mandates SSE2, so loaded
  drivers use it) → `enable_smep()`/`enable_smap()` (CR4 bits 20/21, gated on
  CPUID leaf 7) → `ke::gdt::init` (GDT/TSS, NT selectors) → `ke::idt::init` →
  `ke::pcr::init` (KPCR via `IA32_GS_BASE`) → `ke::syscall::init` (program
  STAR/LSTAR/FMASK, set EFER.SCE) → `mm::phys::init` (PFN bitmap carved from the
  largest free region) → `mm::virt::mm_save_kernel_address_space` → `hal::pic`
  (mask legacy 8259s) → `hal::apic` (enable APIC, periodic clock on 0xD1).
- **Phase 1**: adopt the current context as the idle thread
  (`scheduler::ki_initialize`), `sti` (the clock begins preempting), then
  `ps::ps_create_system_thread(smoke_test_thread, …)`. The idle loop `hlt`s and
  watches `TESTS_DONE`, reporting the verdict via the QEMU `isa-debug-exit`
  device (port 0xF4: `0x10`→exit 33 all-passed, `0x01`→exit 3 failed).

---

## 3. Subsystems

| Dir | NT analog | Contents |
|---|---|---|
| `rtl/` | `Rtl*` | NTSTATUS + severity, intrusive `LIST_ENTRY`, `UNICODE_STRING`, run-finding bitmap |
| `ke/` | `Ke*`/`Ki*` | IRQL (=CR8), spinlocks, GDT/IDT/TSS (`gdt.rs`,`idt.rs`,`selectors.rs`), traps, KPCR/KPRCB (`pcr.rs`), dispatcher objects + waits (`dispatcher.rs`), DPCs (`dpc.rs`), threads (`thread.rs`), scheduler (`scheduler.rs`), bugcheck, `syscall.rs` (KiSystemCall64), `usermode.rs` (iretq to ring 3), `debug.rs` (single-step API tracer) |
| `mm/` | `Mm*` | PFN bitmap allocator (`phys.rs`), tagged NonPagedPool + global allocator (`pool.rs`), 4-level page-table walker + `probe_for_read`/`write`, SMAP `user_access_begin/end`, per-process address spaces (`virt.rs`) |
| `ex/` | `Ex*` | `ExAllocatePoolWithTag` API surface, `ex_allocate_object` |
| `ob/` | `Ob*` | Object headers/types, reference counting, handle table (`handle.rs`) |
| `ps/` | `Ps*` | ETHREAD/KTHREAD, `PsCreateSystemThread`; per-thread PEB/TEB, cmdline, last-error, mui pointers |
| `io/` | `Io*` | DRIVER/DEVICE/IRP + `IoCallDriver`, object namespace (`namespace.rs`), `\Device\Null` (`null.rs`), `\Device\Console` (`console.rs`), RAM filesystem (`ramfs.rs`) |
| `cm/` | `Cm*` | A small in-memory registry (Configuration Manager) — keys/values, HKLM seed |
| `hal/` | HAL | 16550 serial (KdPrint transport), 8259 PIC masking, local APIC + periodic clock |
| `ldr/` | `MiLoadSystemImage`/`Iop*` | PE loader (`pe.rs`), `ntoskrnl.exe` export table (`exports.rs`), shim DLLs (`loaded.rs`), ntdll syscall trampoline (`ntdll.rs`), MUI resolver (`mui.rs`) |
| `init.rs` | `KiSystemStartup` | Phase 0/1 init + boot self tests + the CreateProcess primitive |

**Ke.** Dispatcher objects (events, semaphores, mutants, timers, threads) share
a common header reached by one wait API — `KeWaitForSingleObject` (with
timeout), `KeWaitForMultipleObjects` (WaitAny/WaitAll), `KeDelayExecutionThread`.
The scheduler hands the dispatcher lock across context switches and switches CR3
per-thread. DPCs queue from ISRs and retire at `DISPATCH_LEVEL`.

**Mm.** The PFN allocator is a bitmap edition; the pool is first-fit with split
and a 16-byte header. The page-table walker backs `mm_get_physical_address`,
`probe_for_read/write` (each spanned page must be present + user-accessible
U/S), and per-process address spaces (`mm_create_address_space` clones the kernel
high half; `mm_map_user_range` maps the low half). SMEP and SMAP are enabled;
the kernel's deliberate user-buffer touches are bracketed `stac`/`clac`.

**Io.** `\Device\Null` (`null.sys`-shaped), `\Device\Console`
(`\DosDevices\CON`, writes routed to serial), and a **RAM filesystem**: a path
that isn't a device name resolves to a typed `FileObject` with a read cursor,
reachable through the normal `NtCreateFile`/`NtReadFile`/`NtOpenFile` path.

**Ldr.** Loads genuine PE/COFF images: maps sections by RVA, applies
`IMAGE_REL_BASED_DIR64` relocations, binds the import table against the kernel
export table (drivers) or against the shim DLLs (user programs), marks the image
executable. Driver entry: `ldr_load_driver` → `DriverEntry`; user entry:
`load_user` / `load_user_process` (`LoadedProcess` has its own CR3, entry VA,
user RSP, TEB). The shim DLLs (`kernel32`, `msvcrt`, `ulib`) are real parsed PEs
loaded once in the shared high half; `ntdll` (`ntdll.rs`) is a generated ring-3
syscall-stub trampoline — each export is the canonical Windows thunk
`mov r10,rcx; mov eax,<svc>; syscall; ret`, indexed at `base + svc*16`.

---

## 4. System services / SSDT

The user→kernel boundary is the real x64 Windows mechanism. `syscall`/`sysret`
MSRs are programmed (`ke/syscall.rs`): `STAR[47:32]=R0_CODE(0x10)`,
`STAR[63:48]=R3_CMCODE(0x20)` (so `sysret` loads `SS=0x28`, `CS=0x30`), `LSTAR`
= `KiSystemCall64`, `FMASK` clears IF|TF|DF|AC, `EFER.SCE` set. **Windows x64
convention:** service number in **EAX**, args in **R10, RDX, R8, R9** (the ntdll
stub copies RCX→R10 because `syscall` clobbers RCX). `KiSystemCall64` does
`swapgs` + stack switch (kernel RSP from `gs:`) + saves user RIP(RCX)/RFLAGS(R11)
and dispatches through the SSDT. Each service has the uniform
`(u64,u64,u64,u64)->u64` signature; the return is an NTSTATUS or a handle in RAX.

**Service-number table** (`syscalls.rs::register_all`; numbers are this kernel's
own — the ntdll stub table and the SSDT just agree):

| # | Name | # | Name |
|---|---|---|---|
| 0 | `NtTerminateThread` | 18 | `NtLoadMuiString` (LoadStringW MUI) |
| 1 | DbgWrite (bring-up `DbgPrint`) | 19 | `NtGetCommandLine` |
| 2 | `NtWriteFile` | 20 | PeekConsoleInput |
| 3 | `NtCreateFile` | 21 | QueryHandle |
| 4 | `NtClose` | 22 | `NtOpenKey` (reg open) |
| 5 | `NtReadFile` | 23 | `NtCreateKey` (reg create) |
| 6 | `NtAllocateVirtualMemory` | 24 | `NtQueryValueKey` |
| 7 | `NtFreeVirtualMemory` | 25 | `NtSetValueKey` |
| 8 | `NtProtectVirtualMemory` | 26 | `NtEnumerateKey` |
| 9 | ReportTestResult (test channel) | 27 | `NtCreateProcess` |
| 10 | `NtDelayExecution` | 28 | `NtWaitForSingleObject` (process) |
| 11 | QueryTickCount | 29 | GetExitCodeProcess |
| 12 | IncrementCounter (concurrency proof) | 30 | SetConsoleMode |
| 13 | GetModuleHandle | 31 | LoadMessage (FormatMessage) |
| 14 | GetProcAddress | 32 | QueryAttributes (GetFileAttributes) |
| 15 | SetLastError | 33 | QueryDirectory (FindFirst/Next) |
| 16 | GetLastError | 34 | `NtOpenFile` |
| 17 | QueryFileSize | | |

Every service taking a user pointer first runs it through
`mm::virt::probe_for_read`/`write`, so a bogus/kernel/unmapped/misaligned pointer
returns `STATUS_ACCESS_VIOLATION`/`DATATYPE_MISALIGNMENT` instead of faulting the
kernel; the actual copy is bracketed `user_access_begin`/`end` for SMAP.
`NtOpenFile` uses the genuine NT prototype (path via
`ObjectAttributes->ObjectName`, NT form `\??\C:\file`, fills the IO_STATUS_BLOCK).

---

## 5. Running real user binaries

The loader runs **unmodified Microsoft binaries** in ring 3. A program imports
`Nt*`/Win32 functions like any Windows program; the loader binds those imports
to the in-tree shims:

- **`ntdll`** — the syscall trampoline (§3/§4).
- **`kernel32/`** — the Win32 surface (`GetStdHandle`, `WriteFile`/`ReadFile`,
  `WriteConsoleA/W`, the `lstr*`/`Interlocked*` families, `Heap*`,
  `Virtual*`, `LoadLibraryA`/`GetProcAddress`, `CreateProcessW`,
  `FindFirstFileW`, …) forwarding to the `Nt*` syscalls.
- **`msvcrt/`** — the classic C-runtime surface (`atoi`, `qsort`, `strcpy_s`,
  `_initterm`, `__getmainargs`, the wide-string set, …).
- **`ulib/`** — a real dependent DLL (Microsoft command-line tools' library);
  its own imports are bound against kernel32/msvcrt/ntdll.

At boot, phase 1 loads `kernel32` → `msvcrt` → `ulib` (`init.rs`), then the
console device. A child process is built by `create_user_process`
(`init.rs`): look the image up in the RAM-fs, `load_user_process` (fresh PML4,
image + stack in the low half, TEB), spawn its ring-3 thread, record it in a
16-entry process table, and return a process handle (`PROC_HANDLE_BASE =
0x3000_0000 + index`). The parent blocks in `NtWaitForSingleObject` and reads
the exit code via GetExitCodeProcess. The child's `.mui` is attached per-thread
(`mui_for_image`) so its resource strings resolve.

**Per-process state.** Each thread carries its CR3, command line (so
`GetCommandLine`/argv differ per process), last-error slot, and `.mui` pointer.
On exit, `on_user_thread_exit` restores the console input mode (a child like
`choice` may switch to raw single-key input).

**The shared-ulib reset** (WORKLOG 2026-06-29). `ulib.dll` is mapped **once** in
the shared high half, so its writable `.data` — including the `/GS` cookie set by
`__security_init_cookie` and the CRT startup state machine — is shared across all
processes that run it. The first `more.com` initialized the CRT; the second saw
"already initialized" guards, skipped init, and aborted (the symptom: `more`
worked only once). Fix: `reset_ulib_data` snapshots ulib's pristine post-load
image and restores it before each `create_user_process`, emulating per-process
copy-on-write DLL data — safe because user processes run serially (the creator
blocks). Because ulib is mapped user-accessible, the snapshot/restore is
bracketed with `user_access_begin`/`end` (SMAP), or the kernel `#PF`s reading it
under QEMU (nanox doesn't enforce SMAP, which had masked the fault).

Working tools (WORKLOG): interactive `cmd.exe`, `echo`, `exit`, `dir`, `where`,
`sort`, `choice`, `whoami` (`nanokrnl\user`), `more <file>` (repeatable), and the
`null.sys` driver — under native QEMU and in-browser via nanox.

---

## 6. Self-tests

`smoke_test_thread` (`init.rs`) runs one end-to-end exercise per subsystem in a
real system thread, each reporting `[ OK ]`/`[FAIL]` over the debug port; any
fail flips `TESTS_FAILED` and the idle loop signals QEMU exit 3 (vs 33 on
all-pass). Coverage, in order:

- **Cpu**: SMEP + SMAP enabled (CR4 bits 20/21).
- **Mm**: probe rejects kernel/unmapped/wraparound/misaligned pointers, accepts a
  mapped user page; per-process address space isolation + CR3 switch; pool
  alloc/free + 16-alignment + leak balance; `Vec` over the pool; pool stress
  (4000 randomized ops, pattern-verified, leak-checked); page-table-walk
  translation.
- **Ke**: `KeDelayExecutionThread` >= requested; sync event wakes one waiter per
  set (3 pong threads); wait timeout → `STATUS_TIMEOUT`; WaitForMultiple
  Any/All; recursive mutant acquire/release; DPC retires at DISPATCH.
- **Io**: `\Device\Null` `IRP_MJ_WRITE` consumes all bytes, `IRP_MJ_READ` EOF.
- **Ob**: ObCreateObject, refcount tracking, type-mismatch rejection, handle
  table allocate/resolve/close.
- **Um**: a hand-assembled ring-3 stub calls `NtWriteFile` on a pre-opened
  console handle then `NtTerminateThread` — full path iretq→syscall→SSDT→handle
  →IRP→serial.
- **Cm**: seeded HKLM key/DWORD, create/set/reopen/enumerate, absence reporting.
- **Ps**: `create_user_process(USERAPP2)`, wait, assert it reported 5050 via the
  test channel.
- **Ldr**: load the Rust testdriver PE + run DriverEntry (timer→DPC→event),
  resolve its device by name + symlink, IRP read, IOCTL (spinlock-guarded
  count), DriverUnload; and (non-interactive) load the real Microsoft `null.sys`.

Default build → `ALL SELF TESTS PASSED — system idle` (exit 33).
**`--features interactive`** drops to the real `cmd.exe` shell on the serial
console instead of running the gated portion of the suite.

---

## 7. Build & run

```sh
rustup toolchain install nightly --profile minimal -c rust-src -c llvm-tools

# Host unit tests (rtl, IRQL, spinlocks — arch-independent core)
cargo test -p kernel

# Build the kernel ELF (stable Rust)
cargo build -p kernel --target x86_64-unknown-none

# Wrap in a bootable image + run under QEMU (serial -> stdio)
cargo +nightly run -p boot -- target/x86_64-unknown-none/debug/kernel --run
```

`--run` exits with QEMU's status: **33** = every self test passed, **3** =
failed/bugchecked. `./scripts/qemu-test.sh` wraps this pass/fail.

**Native QEMU, interactive cmd.exe.** `sh scripts/run-interactive.sh` builds the
shims, builds the kernel `--features interactive`, makes the disk image, and
boots QEMU (`-cpu qemu64,+smep,+smap -serial stdio`) to the real `C:\>` prompt.

**Browser (nanox).** `sh emu/build-wasm.sh` builds the ~38–60 KB wasm and stages
`web/nanox/`; then `cd web/nanox && python3 -m http.server 8000` and open it —
**Boot**, wait for `C:\>`, type. nanox boots the **unmodified** kernel ELF in
long mode (no threads / SharedArrayBuffer / COOP-COEP). See `emu/SPEC.md` for
nanox itself. (`web/index.html` is a retired v86 harness — v86 cannot boot this
64-bit kernel; it panics on the `#GP` handler before any output.)

---

## 8. Repo layout

```
kernel/          the kernel library + binary (src/{rtl,ke,mm,ex,ob,ps,io,cm,hal,ldr}, init.rs, syscalls.rs, main.rs)
ntabi/           shared #[repr(C)] kernel<->driver ABI (DRIVER/DEVICE/IRP, UNICODE_STRING, NTSTATUS, win64 sigs)
driver/          a real freestanding test driver (x86_64-pc-windows-msvc, lld-link)
drivers/         staged Microsoft binaries (e.g. null.sys)
kernel32/ msvcrt/ ulib/   shim DLLs (real PEs) the user loader binds imports against
userapp/ userapp2/        bundled ring-3 console programs (argv+heap; compute)
winbin/ worker/  Windows binary staging / worker helpers
boot/            disk-image builder + QEMU runner (upstream `bootloader` crate)
emu/             nanox — the bespoke x86-64 browser emulator (own spec: emu/SPEC.md)
web/             nanox browser harness (web/nanox/); retired v86 harness (web/index.html)
scripts/         build/run helpers (run-interactive.sh, qemu-test.sh, driver-test.sh, build-*.sh)
README.md WORKLOG.md WORKFLOW.md SPEC.md
```

Each current limitation (single-CPU, NonPagedPool-only, partial `ntoskrnl.exe`
export surface, names-only imports, stored-but-not-invoked completion routines,
large-page-granular NX clearing) is documented at its definition site.
