# ntoskrnl-rs

An **NT-compatible kernel written in Rust** — the architecture, abstractions,
and (where it matters) the exact constants and layouts of the Windows NT
kernel, rebuilt as a modern, memory-safe, freestanding Rust codebase that
boots on x86_64 and proves itself with self tests on every boot.

```
ntoskrnl-rs 0.1.0 (x86_64) — NT-compatible kernel in Rust
KiSystemStartup: phase 0
KE: GDT/TSS/IDT loaded (NT selector layout), KPCR online
MM: PFN bitmap @ 0x... — 118 MiB usable RAM
HAL: PIC masked, APIC enabled, clock on vector 0xD1 (CLOCK_LEVEL)
KiSystemStartup: phase 1
KE: scheduler online, interrupts enabled
KiSystemStartup: running self tests
  [ OK ] Mm: pool allocations succeed
  [ OK ] Ke: sync event wakes one waiter per set
  [ OK ] Io: IRP_MJ_WRITE to \Device\Null consumes all bytes
  ...
ALL SELF TESTS PASSED — system idle
```

## What "NT-compatible" means here

The point is fidelity to NT's *kernel architecture*, not binary
compatibility with Windows drivers (yet). Concretely:

**Bit-exact where it's ABI.**
- `NTSTATUS` values (`STATUS_ACCESS_VIOLATION == 0xC0000005`, …) and the
  `NT_SUCCESS` severity rules — `kernel/src/rtl/status.rs`
- `LIST_ENTRY` two-pointer layout and `CONTAINING_RECORD` recovery —
  `kernel/src/rtl/list.rs`
- `UNICODE_STRING` (byte counts, UTF-16 buffer, 16-byte x64 layout) —
  `kernel/src/rtl/string.rs`
- The x64 GDT **selector layout** (`KGDT64_R0_CODE = 0x10` … `KGDT64_SYS_TSS
  = 0x40`), chosen by NT so `syscall`/`sysret` work — `kernel/src/ke/gdt.rs`
- The IRQL model and its hardware mapping: IRQL **is** CR8/TPR, an interrupt
  is delivered iff `vector >> 4 > IRQL`, and the clock runs on **vector
  0xD1** (`CLOCK_LEVEL` 13), same as NT x64 — `kernel/src/ke/irql.rs`
- Pool tags, `IRP_MJ_*` codes, stop codes (`IRQL_NOT_LESS_OR_EQUAL`, …)

**Faithful in shape where bit-compat doesn't matter.**
Dispatcher objects with a common header and one wait API; DPCs queued from
ISRs and retired at `DISPATCH_LEVEL`; the dispatcher lock handed off across
context switches; driver/device/IRP triangle with dispatch tables; tagged
pool with a 16-byte header; bugchecks that freeze the world. Divergences are
documented at the definition site (e.g. our `KTRAP_FRAME` saves all GP
registers; NT splits volatile/non-volatile across two structures).

**Modern & safe by construction.**
Rust everywhere; `unsafe` is concentrated at the hardware boundary and in
the intrusive data structures, each block with an explicit safety contract.
`SpinLock<T>` *owns* its data and raises IRQL by construction — the classic
"touched shared state below DISPATCH_LEVEL" driver bug doesn't compile.
`Box`/`Vec`/`String` work in-kernel and draw from NonPagedPool with the
`'Rust'` tag.

## Subsystem map

| Directory | NT analog | Contents |
|---|---|---|
| `kernel/src/rtl/` | `Rtl*` | NTSTATUS, intrusive lists, UNICODE_STRING, run-finding bitmap |
| `kernel/src/ke/` | `Ke*`/`Ki*` | IRQL, spinlocks, GDT/IDT/TSS, traps, KPCR/KPRCB, dispatcher objects, DPCs, threads, scheduler, bugcheck |
| `kernel/src/mm/` | `Mm*` | PFN allocator (bitmap edition), NonPagedPool + global allocator, page-table walker |
| `kernel/src/ex/` | `Ex*` | `ExAllocatePoolWithTag` API surface |
| `kernel/src/ob/` | `Ob*` | Object headers, types, reference counting |
| `kernel/src/ps/` | `Ps*` | ETHREAD, `PsCreateSystemThread` |
| `kernel/src/io/` | `Io*` | DRIVER_OBJECT/DEVICE_OBJECT/IRP, `IoCallDriver`, null.sys |
| `kernel/src/ldr/` | `MiLoadSystemImage`/`Iop*` | PE/COFF loader + kernel export table (`ntoskrnl.exe` exports) |
| `kernel/src/hal/` | HAL | 16550 serial (KdPrint transport), 8259 PIC masking, local APIC + clock |
| `kernel/src/init.rs` | `KiSystemStartup` | Phase 0/1 init + boot self tests |
| `ntabi/` | ntdef/wdm headers | Shared `#[repr(C)]` kernel⇄driver ABI (types + win64 signatures) |
| `driver/` | a WDK driver | Real PE test driver, built for `x86_64-pc-windows-msvc` |
| `boot/` | bootmgr/winload | Disk-image builder + QEMU runner (`bootloader` crate) |

## Building & running

Prereqs: Rust via rustup (`rust-toolchain.toml` pins stable + the
`x86_64-unknown-none` target for the kernel; the *boot-image builder* needs
a nightly with `rust-src`/`llvm-tools` because the upstream `bootloader`
crate compiles its real-mode stages with `-Zbuild-std`) and
`qemu-system-x86_64`:

```sh
rustup toolchain install nightly --profile minimal -c rust-src -c llvm-tools
```

```sh
# Host unit tests (rtl, IRQL rules, spinlocks — the arch-independent core)
cargo test -p kernel

# Build the kernel ELF (stable Rust)
cargo build -p kernel --target x86_64-unknown-none

# Wrap it in a bootable image and run under QEMU (serial -> stdio)
cargo +nightly run -p boot -- target/x86_64-unknown-none/debug/kernel --run
```

`--run` exits with QEMU's status: **33** means every boot self test passed
(the kernel reports through the `isa-debug-exit` device), **3** means a test
failed or the kernel bugchecked. `./scripts/qemu-test.sh` wraps this into a
single pass/fail command.

## Driver loading

ntoskrnl-rs loads genuine **PE/COFF kernel drivers** — `.sys` images built by
a different toolchain for the real Windows kernel target — resolves their
imports against the kernel's export table, and runs their `DriverEntry`.

The pieces:

- **`ntabi/`** — a tiny shared crate holding the `#[repr(C)]` boundary types
  (`DRIVER_OBJECT`, `DEVICE_OBJECT`, `IRP`, `UNICODE_STRING`, `NTSTATUS`) and
  the `extern "win64"` callback signatures. Both the kernel and the driver
  depend on it, so layouts and calling convention agree by construction.
- **`kernel/src/ldr/exports.rs`** — the kernel's export table, the
  `ntoskrnl.exe` export directory's stand-in: named, Microsoft-x64 shims
  (`DbgPrint`, `ExAllocatePoolWithTag`, `IoCreateDevice`, …) over the in-tree
  Rust APIs.
- **`kernel/src/ldr/pe.rs`** — the loader: maps sections by RVA, applies
  `IMAGE_REL_BASED_DIR64` base relocations, binds the import table to the
  export table, marks the image executable, and returns the `DriverEntry`
  pointer.
- **`driver/`** — a real freestanding driver compiled for
  `x86_64-pc-windows-msvc` with `lld-link` (no CRT, `/subsystem:native`,
  `/entry:DriverEntry`), linking an `ntoskrnl.lib` import library generated
  from the kernel's own export names.

### Exported `ntoskrnl.exe` API surface

The loader binds driver imports against the kernel export table
(`kernel/src/ldr/exports.rs`). Currently exported, by area:

- **Debug**: `DbgPrint`, `KeBugCheckEx`
- **Pool**: `ExAllocatePoolWithTag`, `ExFreePoolWithTag`, `ExAllocatePool2`,
  `ExFreePool`
- **Events/sema/mutex**: `KeInitializeEvent`, `KeSetEvent`, `KeClearEvent`,
  `KeResetEvent`, `KeInitializeSemaphore`, `KeReleaseSemaphore`,
  `KeInitializeMutex`, `KeReleaseMutex`
- **Waits**: `KeWaitForSingleObject` (with timeout), `KeDelayExecutionThread`
- **Spinlocks/IRQL**: `KeInitializeSpinLock`, `KeAcquireSpinLock`,
  `KeReleaseSpinLock`, `KeGetCurrentIrql`, `KeRaiseIrql`, `KeLowerIrql`
- **DPCs/timers**: `KeInitializeDpc`, `KeInsertQueueDpc`, `KeInitializeTimer`,
  `KeSetTimer`, `KeCancelTimer`
- **Time**: `KeQueryTickCount`, `KeStallExecutionProcessor`
- **Strings/memory**: `RtlInitUnicodeString`, `RtlZeroMemory`,
  `RtlCopyMemory`, `RtlFillMemory`
- **I/O — devices**: `IoCreateDevice`, `IoDeleteDevice`,
  `IoCreateSymbolicLink`, `IoDeleteSymbolicLink`, `IoGetDeviceObjectPointer`
- **I/O — IRPs (stack-location model)**: `IoGetCurrentIrpStackLocation`,
  `IoGetNextIrpStackLocation`, `IoCallDriver`, `IofCompleteRequest`,
  `IoSetCompletionRoutine`, `IoSkipCurrentIrpStackLocation`
- **Mm**: `MmGetPhysicalAddress`, `MmMapIoSpace`, `MmUnmapIoSpace`

Plus `DriverObject->DriverUnload` support in the loader. The bundled
`testdriver` exercises the lot: a timer→DPC→event handshake in
`DriverEntry`, a spinlock-guarded request counter surfaced through a custom
IOCTL, IRP read/write via stack locations, a named device with a
`\DosDevices` symbolic link, and an unload routine.

Build the driver and run the end-to-end demo (driver build + kernel build +
boot + assert):

```sh
./scripts/driver-test.sh
```

At boot the loader maps the embedded `.sys`, runs its `DriverEntry` (which
prints over the debug port via the *imported* `DbgPrint` and creates a
device), then the self test sends it an IRP and checks the buffer the
driver's own code filled — end-to-end proof a foreign-compiled PE loaded,
linked, and executed inside the kernel:

```text
LDR: mapped driver image @ 0x... (28672 bytes), entry @ 0x...
RustDemo: DriverEntry running from loaded PE
RustDemo: waiting on timer event...
RustDemo: timer DPC fired, signaling event
RustDemo: timer event satisfied
RustDemo: \Device\RustDemo created, dispatch + unload registered
  [ OK ] Ldr: load PE driver + run DriverEntry (timer/DPC/event)
  [ OK ] Ldr: loaded driver created its device
  [ OK ] Ldr: IoGetDeviceObjectPointer resolves name + symlink
RustDemo: dispatch IRP major=3 (request #1)
  [ OK ] Ldr: loaded driver services IRP via stack location
RustDemo: dispatch IRP major=14 (request #2)
  [ OK ] Ldr: loaded driver handles IOCTL (spinlock-guarded count)
RustDemo: DriverUnload — cleaning up
  [ OK ] Ldr: DriverUnload removed the symbolic link
```

Building the driver needs the Windows target and LLVM's PE tools:

```sh
rustup target add x86_64-pc-windows-msvc   # + nightly for build-std
brew install llvm                          # provides lld-link, llvm-dlltool
```

## Boot flow

1. `bootloader` (BIOS or UEFI) loads the kernel ELF, builds initial page
   tables with **all physical memory mapped at an offset** (NT's equivalent
   window is what Mm calls the physical map), and calls `kernel_main` with a
   `BootInfo` — our `LOADER_PARAMETER_BLOCK`.
2. **Phase 0**: serial up → GDT/TSS + IDT with NT selectors → KPCR via
   `IA32_GS_BASE` → PFN bitmap carved from the largest free region → PICs
   masked, APIC enabled, periodic clock on vector 0xD1.
3. **Phase 1**: the boot context is adopted as the idle thread, the
   scheduler comes online, interrupts open, and the self-test system thread
   exercises every subsystem end-to-end.

## User mode & console applications

ntoskrnl-rs runs **ring-3 programs**. The user/kernel boundary is the real
x64 Windows mechanism:

- **`syscall`/`sysret`** (`kernel/src/ke/syscall.rs`): `STAR`/`LSTAR`/`FMASK`
  programmed against the NT selector layout, `KiSystemCall64` doing the
  `swapgs` + stack switch + SSDT dispatch.
- **Ring-3 entry** (`kernel/src/ke/usermode.rs`): `iretq` to user mode with
  GS parked in `KERNEL_GS_BASE`.
- **Handle table** (`kernel/src/ob/handle.rs`) and **`Nt*` services**
  (`kernel/src/syscalls.rs`): `NtCreateFile` (open a device by name),
  `NtWriteFile`, `NtClose`, `NtTerminateThread`.
- **Console device** (`kernel/src/io/console.rs`): `\Device\Console`
  (`\DosDevices\CON`) routing writes to the serial port.
- **User-mode PE loader** (`ldr::pe::load_user`): maps a PE into
  user-accessible pages and runs it in ring 3.

The bundled **`userapp/`** is a genuine PE console executable, compiled for
`x86_64-pc-windows-msvc` with no CRT and no imports — it issues this kernel's
syscalls directly. At boot the loader maps it and runs it:

```text
UM: mapped console app @ 0x... (12288 bytes), entry @ 0x...
APP: hello from a loaded PE console app in ring 3!
  [ OK ] Um: loaded PE console app ran and wrote via syscalls
```

The app issues **no inline syscalls** — it imports `Nt*` functions from
`ntdll.dll` like a normal Windows program. The kernel builds a ring-3
syscall-stub trampoline (`ldr::ntdll`) and the loader binds the app's imports
to it, so the app links against an `ntdll` import library and runs unmodified.
It also reads a line of console input and exercises a heap
(`NtAllocateVirtualMemory`):

```text
APP: hello from a loaded PE console app in ring 3!
APP: you typed: ntoskrnl-rs
APP: virtual memory alloc/free ok
```

The app imports the classic Win32 console functions — `GetStdHandle`,
`WriteFile`, `WriteConsoleA`/`WriteConsoleW`, `ReadFile`, `GetFileType`,
`MultiByteToWideChar`/`WideCharToMultiByte`, `SetLastError`/`GetLastError`
(a real per-thread last-error slot), the timing APIs
`QueryPerformanceCounter`/`QueryPerformanceFrequency`/`GetSystemTimeAsFileTime`,
`OutputDebugStringA`, the `lstr*` string helpers
(`lstrlenA`/`lstrcmpA`/`lstrcmpiA`/`lstrcpyA`/`lstrcatA`), `GetCommandLineW`,
the `Interlocked*` atomics
(`InterlockedIncrement`/`Decrement`/`Exchange`/`CompareExchange`),
`GetSystemInfo`, `GetCurrentProcess`/`GetCurrentThread` pseudo-handles,
`GetVersion`/`GetVersionExA`/`IsDebuggerPresent`, `VirtualAlloc`/`VirtualFree`,
`GetModuleHandleW`/`GetModuleHandleExA`, `GetConsoleMode`/`SetConsoleMode`,
`GetCPInfo`, `GlobalMemoryStatusEx`, `FormatMessageA`, `TerminateProcess`,
`ExitProcess`, plus the runtime-linking trio
`LoadLibraryA`/`GetModuleHandleA`/`GetProcAddress`/`FreeLibrary` — from
`kernel32.dll`, exactly as a no-CRT Win32 console program does. The kernel loads a `kernel32` shim DLL
(`kernel32/`) and resolves the app's imports against its **parsed PE export
table** — real cross-module dynamic linking. `kernel32` forwards to the
`Nt*` syscalls.

A second shim, `msvcrt/`, provides the classic C-runtime surface (`atoi`,
`qsort`, `strchr`, `strcpy_s`, the locale-compare family, `_initterm`,
`__getmainargs`, the wide string/`_vsnwprintf` set, …) so a real classic-CRT
console binary can bind its `msvcrt` imports to our implementation rather than
dragging in the modern API-set/`KERNELBASE`/`ucrtbase` chain.

Two kernel subsystems back the real-binary work:

* **A RAM filesystem** (`kernel/src/io/ramfs.rs`): files held in kernel memory
  reached through the normal `NtCreateFile`/`NtReadFile` path. A path that
  isn't a device name resolves to a `FileObject` (a typed object-manager
  object with a read cursor); `GetFileSize`/`CreateFileA`/`ReadFile` work on
  it. (A console app reads `C:\hello.txt` end to end in the self tests.)
* **MUI string resolution** (`kernel/src/ldr/mui.rs`): modern Windows tools
  keep their UI text in a side-by-side `<exe>.mui` resource file, not the
  image. A `.mui` is registered against a module's base at load time;
  `LoadStringW` parses the image's `RT_STRING` first and falls back (via
  `NtLoadMuiString`) to the kernel's `.mui` resolver. (Verified: `choice.exe`
  loads its real strings from `choice.exe.mui`.)
* **Per-process command line** (`KTHREAD` + `NtGetCommandLine`): each process
  sees its own `argv`/`GetCommandLine`, not a shared static.

### A user-mode debugger

`kernel/src/ke/debug.rs` is a single-step tracer for ring-3 code. It sets the
x86 **Trap Flag** when entering a traced thread, so the CPU raises `#DB` after
every user instruction; the handler walks a module map (the program image plus
the `kernel32`/`msvcrt`/`ntdll` shims) and logs an **API call/return trace** —
each call into a library (with the Microsoft x64 argument registers) and each
return (with the result), bounded to the number of calls. Stepping is confined
to user mode because the syscall flag-mask already clears `TF`. This was built
to debug `where.exe`: the trace shows its CRT startup, then its descent into
the shared Microsoft command-line-parser's error path (`LoadStringW(5001)` =
`"ERROR:"` after a 7-byte `_memicmp`), which is why `where`/`choice` print only
an `ERROR:` prefix — exactly the kind of third-party-binary troubleshooting the
debugger is for.

**This works end to end:** an unmodified Microsoft Windows `sort.exe` loads
via `load_user_process`, binds all its `KERNEL32`/`msvcrt`/`ntdll`/`ADVAPI32`
imports to our shims, runs the real MSVC CRT startup (with a real per-thread
TEB reached via `gs:`), reads stdin, sorts, and writes the sorted output to
the console before exiting — `cherry/banana/apple/date` in, `apple/banana/
cherry/date` out. Output of our own console app:

```text
APP: hello from a Win32 console app (kernel32 imports)
APP: you typed: ntoskrnl-rs
```

The app is structured like a real Win32 console program: its entry is a CRT
shim (`mainCRTStartup`) that calls `main()` and passes the result to
`ExitProcess`, and `main` uses `GetStdHandle`/`WriteFile`/`ReadFile` plus a
`GetProcessHeap`/`HeapAlloc`/`HeapFree` heap (spinlock-guarded, so threads
sharing a process heap don't corrupt the free list) — all imported from the
`kernel32` shim:

```text
APP: console app with CRT-style entry
APP: you typed: ntoskrnl-rs
APP: built this line in a HeapAlloc buffer
```

Two independent console programs are bundled — `userapp/` (argv + heap) and
`userapp2/` (a compute program: `sum(1..=100)`, `fib(20)`) — both built by the
generic `scripts/build-app.sh` and run through the same loader/`kernel32`/CRT
path, demonstrating that the system runs *arbitrary* console programs, not one
bespoke app.

Build with `./scripts/build-kernel32.sh && ./scripts/build-userapp.sh &&
./scripts/build-userapp2.sh` (then rebuild the kernel, which embeds them). The
user-mode surface now covers syscalls, handles, bidirectional console I/O, a
pooled heap, DLL export/import resolution, a CRT-style entry, and `argv`.
Running *off-the-shelf* `cl.exe`/`libcmt` binaries would need the full MSVC CRT
plus TEB/PEB and many more `kernel32` APIs — out of scope; the model and stack
are demonstrated end to end.

## Memory layout

The kernel lives in the **high canonical half** (NT-style): the kernel image,
stack, and boot info map into `0xFFFF_8000_..`, and the all-physical-memory
window sits at `0xFFFF_FF00_0000_0000`. The entire **low half**
(`< 0x8000_0000_0000`) is therefore free for per-process user mappings — the
groundwork for real per-process address spaces (and SMAP). `mm` reads the
physical-memory offset from `BootInfo` at runtime, so the layout is set purely
by the bootloader config in `kernel/src/main.rs`.

**Per-process isolation works**: `ldr::pe::load_user_process` loads a console
app into its *own* address space — a fresh PML4 that clones the kernel high
half and maps the image + user stack in the low half (`mm_create_address_space`
/ `mm_map_user_range`). A thread switches CR3 into it before entering ring 3;
the app reaches the shared high-half `kernel32`/`ntdll` stubs so it runs
normally, but its image is mapped *only* in its address space (verified
unmapped in the kernel AS).

**Multiple concurrent processes** run too: each thread carries its address
space (CR3), and the scheduler switches CR3 on context switch. Two worker
processes in two distinct address spaces interleave and both make progress —
real multiprocessing with per-process isolation. Wrapping this in
`NtCreateProcess` and a per-process heap are the next steps.

## Honest limitations (current phase)

- **Single processor** (boot CPU only; structures are per-PRCB-shaped for
  the MP step).
- **No user mode yet** — the GDT/STAR-compatible selector layout and TSS
  are in place precisely so syscalls/user mode bolt on next.
- **No paging-out**: NonPagedPool only; pool frees don't fully coalesce.
- Terminated thread stacks await a reaper thread (documented leak).
- APIC timer uses a fixed divider (~1 kHz on QEMU), not PIT-calibrated.
- **Concurrency**: multiple ring-3 threads run with preemptive multitasking
  (two worker threads interleave via `Sleep` and both make progress through
  the syscall path). User RSP is saved per-thread across blocking syscalls.
- **Protection**: SMEP **and SMAP** are enabled (the kernel can neither
  execute nor read/write user pages by accident); page-table protection
  changes split large pages to 4 KiB so they're per-page-precise. The
  kernel's deliberate user-buffer accesses (console I/O, the `Nt*` string
  arguments, kernel32 export reads) are bracketed with `stac`/`clac`.
  Every `Nt*` service that takes a user buffer first runs it through a
  `ProbeForRead`/`ProbeForWrite` check (`mm::virt::probe_user_buffer`): a
  page-table walk requiring each spanned page to be present and user-accessible
  (U/S), so a bogus, unmapped, or kernel pointer returns
  `STATUS_ACCESS_VIOLATION` instead of faulting the kernel. (The walk inspects
  the U/S bit rather than NT's fixed `MM_USER_PROBE_ADDRESS` boundary because
  this kernel still maps some user memory in the high half.)
- **Driver loading**: the export surface covers synchronization, timers,
  DPCs, pool, the stack-location IRP path, a device namespace, and basic Mm
  mapping — a broad, useful subset, but not all of `ntoskrnl.exe` (no
  registry/`Zw*`, no PnP IRPs, no file systems — these need whole
  subsystems). Ordinal imports are unsupported (names only). Completion
  routines are stored but not yet invoked. `MmMapIoSpace` is RAM/window-
  backed (true device MMIO outside the mapped window is future work).
  Making an image executable clears NX at large-page granularity (a
  coarsening — finer per-page protection is future work). Drivers are
  trusted: the PE loader validates structure but is not hardened against
  hostile images.

Each limitation is also documented at its definition site.
