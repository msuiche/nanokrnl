# ntoskrnl-rs — Workflow, Findings & Workarounds

How we build, test, and bring up real Windows console binaries on this kernel,
plus the findings and workarounds accumulated along the way. Read this before
trying to run a new `.exe` or extend the shims.

---

## 0. Project history (inception → now)

The project began as an **NT-architecture kernel written in Rust** and grew, in
bounded increments, into a system that runs **unmodified Microsoft console
binaries** in ring 3. The arc below is the order things were built and why each
step unlocked the next. (Dates are early; the project is young and moved fast.)

### Phase A — the kernel core
NT-shaped subsystems from the start: `ke` (scheduler, dispatcher, traps, IRQL via
CR8), `mm` (paging), `ob` (object manager with typed headers + handle table),
`ps` (threads), `ex` (pool), `io` (device/IRP), `ldr` (loader), `rtl`, `hal`
(serial, APIC timer at vector 0xD1). NT-exact constants throughout (NTSTATUS,
KGDT64 selectors, clock vector). The kernel builds on **stable** for
`x86_64-unknown-none`; the bootloader needs **nightly**. This established the
shapes (KTHREAD, ETHREAD, dispatcher objects, IO_STACK_LOCATION) that later let
real Windows code feel at home.

### Phase B — loading real kernel drivers
Before any user mode, the kernel could load real PE/COFF **kernel drivers** built
for `x86_64-pc-windows-msvc`: a PE loader (sections/relocs/import binding), an
export table of `extern "win64"` `ntoskrnl.exe` shims (~47: Ke events/sema/mutex/
timer/DPC, Ex pool, Rtl, the Io IRP stack-location path, device namespace, Mm
map), a shared `ntabi` repr(C) kernel<->driver ABI, and an `ntoskrnl.lib` import
library generated with `llvm-dlltool`. Key hazard solved here: making the loaded
image **executable** without tripping NXE reserved-bit faults (clear NX along the
PTE path, do not disable NXE). A test driver exercised timer->DPC->event,
spinlock IOCTLs, symlinks, and unload. This proved the PE-loading + win64-ABI
machinery that user-mode loading later reused.

### Phase C — crossing into user mode
- **Syscalls:** programmed STAR/LSTAR/FMASK/EFER.SCE; `KiSystemCall64` (naked:
  swapgs, per-CPU stack switch via KPCR, Windows->SysV arg marshalling, SSDT
  dispatch, sysretq). `ki_enter_user_mode` builds an `iretq` frame to ring 3.
  Two subtle ABI bugs fixed at the boundary: the dispatcher must preserve
  RDI/RSI (Windows-nonvolatile, SysV-volatile), and the user syscall stub must
  mark the full clobber set incl. r9.
- **Handles + console + Nt services:** a system-wide handle table, a
  `\Device\Console` routing writes to serial, and the first SSDT services
  (NtWriteFile/NtCreateFile/NtClose/NtReadFile/terminate).
- **First real ring-3 PE:** `userapp` (no CRT) opened the console, wrote, read,
  and exited — a Win32-ish console app running in ring 3.
- **User virtual memory:** NtAllocateVirtualMemory/Free/Protect.

### Phase D — the dynamic-linking stack
The path that makes unmodified `.exe`s viable:
1. **ntdll trampoline** (`ldr/ntdll.rs`): a ring-3 page of `Nt*` syscall stubs +
   a name->service table, so imports can bind to it.
2. **kernel32 shim** (`kernel32/`): a real cdylib exporting Win32 names, each
   calling our syscalls; `pe::resolve_export` parses its export table.
3. **loaded-module registry** (`ldr/loaded.rs`): resolve a symbol across
   ntdll/kernel32/msvcrt **by name** — cross-module dynamic linking.
4. **CRT-style startup, argc/argv, a pooled heap, a test-result channel**, and a
   second app — proving the loader runs arbitrary programs through one path.

### Phase E — hardening + isolation
- **SMEP**, then **SMAP** (with `user_access_begin/end` brackets at every kernel
  touch of user memory). Enabling SMEP surfaced a real large-page-coarsening bug,
  fixed properly with 1G->2M->4K **page splitting** along the protect path.
- **Kernel relocated to the high half**, freeing the entire low half for
  per-process mappings.
- **Per-process address spaces + isolation**, then **multiple concurrent
  processes** (per-thread CR3, AS switch on context switch). Real multiprocessing
  with isolation.
- A broad **kernel32 surface** expansion across many increments (versioning,
  system info, Interlocked atomics, timing/QPC, per-thread last-error,
  Multi/WideChar, LoadLibrary/GetProcAddress, WriteConsole, lstr\*, ...), plus
  ProbeForRead/Write user-pointer validation.

### Phase F — running real Microsoft binaries
- **Feasibility study:** extracted binaries from the Win11 26H1 ISO and measured
  imports. Finding that set the whole strategy: real `msvcrt`/`cmd` link
  `api-ms-win-*` -> KERNELBASE + ucrt, not classic kernel32 — so we **substitute
  our own shims by export name** rather than load Microsoft's DLLs (see §1).
  `sort.exe` was uniquely minimal and chosen as the first target.
- **msvcrt shim** (`msvcrt/`): the C-runtime surface (mem/str/qsort/printf/CRT
  startup), calling our kernel32.
- **sort.exe runs** end-to-end (sorts real input). Required: TEB/PEB (real
  binaries read `gs:[0x30]/[0x60]`), the Win64 entry frame (return addr + shadow
  space + RSP alignment), trap-entry `swapgs` gating, and per-stream std handles.
- **choice.exe + the MUI finding:** modern tools externalize UI strings to an
  `<exe>.mui` resource-only PE -> built the MUI loader.
- **where.exe + three new subsystems:** a RAM filesystem, MUI, and per-process
  command-line args.

### Phase G — the debugger, and finishing where.exe + choice.exe
- Built the **in-kernel user-mode debugger / API tracer** (§3) to troubleshoot
  why where/choice diverged into error paths.
- Used it to find and fix where.exe's chain: `SetThreadUILanguage(0)` returning
  0, missing **version-info** APIs, and a `_vsnwprintf` `%04x` zero-pad bug.
  where.exe now runs correctly.
- Made the tracer **fast** (the DR0 hybrid, §3) so it is usable as a standing
  tool, not a 90-second crawl.
- Fixed the **stale std-handle** bug (a closed cached console handle across
  processes); **choice.exe now runs interactively**.
- Measured the **cmd.exe** gap (§5) — reachable but multi-session.

The result today: three real Windows binaries (sort, where, choice) run on the
kernel, with the loader, shims, debugger, and methodology to bring up more.

---

## 1. The strategy: own-ABI shims, not real system DLLs

We run **unmodified Microsoft binaries** (sort.exe, choice.exe, where.exe, …) in
ring 3 against shims **we** own:

- `kernel32/` — our `kernel32.dll` shim (cdylib). Exports the Win32 names a
  program imports; each calls **our** syscalls (our own SSDT numbers, not NT's).
- `msvcrt/` — our `msvcrt.dll` shim (the C runtime surface).
- `kernel/src/ldr/ntdll.rs` — a generated ring-3 trampoline page of `Nt*`
  syscall stubs.

We deliberately do **not** load real Microsoft `ntdll`/`kernelbase`/`msvcrt`.
Reason (measured): real `msvcrt.dll`/`cmd.exe` import almost entirely from
`api-ms-win-*` API sets that forward to **KERNELBASE.dll** (+ ucrt), which would
cascade into an API-set resolver + a full KERNELBASE + a real-syscall-ABI ntdll
(the ReactOS route — multi-year). Substituting our own shims with matching
**export names** sidesteps all of that: the binary binds to our names and our
syscalls, and the api-set/kernelbase machinery never matters.

### Import resolution is by NAME, not by DLL

`kernel/src/ldr/loaded.rs::resolve(name)` looks a symbol up across ntdll →
kernel32 → msvcrt **ignoring the DLL it was imported from**. Consequence: an
import from `api-ms-win-core-file-l1-2-0.dll!CreateFileW` resolves to our
`kernel32!CreateFileW` automatically. So "supporting an apiset binary" reduces to
"implement the function names it uses." (ucrt's `_o_<name>` indirection names are
the exception — they need an explicit `_o_<name>` → `<name>` alias.)

---

## 2. Build & run

Toolchain: kernel builds on **stable** for `x86_64-unknown-none`; the `boot`
crate needs **nightly** (bootloader 0.11 uses `-Zbuild-std`). Use `~/.cargo/bin`
(Homebrew rust has no rustup). MSVC-target shims link with `lld-link` +
`llvm-dlltool` from `/opt/homebrew/opt/llvm/bin`.

```sh
# Rebuild a shim after editing it (regenerates the .dll; build.rs re-embeds it):
sh scripts/build-kernel32.sh
sh scripts/build-msvcrt.sh
sh scripts/build-app.sh <dir>      # generic app builder (also gens import libs)

# Full boot self-test suite (boots in QEMU, exit code 33 == PASS):
sh scripts/qemu-test.sh

# Host unit/conformance tests:
cargo test -p kernel                # 12 unit + 7 conformance + 2 doctest
```

Rebuild order when changing shim surface + an app: `build-kernel32.sh` →
`build-<app>.sh` → the kernel (build.rs re-embeds both via rerun-if-changed).

### Reliable manual QEMU run (preferred while debugging)

`scripts/qemu-test.sh`'s `timeout 60` watchdog **fails to kill QEMU when the run
is backgrounded** (the detached process group escapes `timeout --foreground`),
which leaves a stale QEMU holding the disk-image **write lock** — the cause of
`qemu-system-x86_64: Failed to get "write" lock` and of "I can't run it either".
When iterating, drive QEMU yourself with serial to a file and a hard timeout:

```sh
pkill -9 qemu-system-x86_64                              # clear any stale instance
cargo build -p kernel --target x86_64-unknown-none      # rebuild kernel ELF
cargo +nightly run -q -p boot -- \
    target/x86_64-unknown-none/debug/kernel < /dev/null  # build disk image (NO --run)
timeout 90 qemu-system-x86_64 \
  -drive format=raw,file=target/x86_64-unknown-none/debug/disk-bios.img \
  -cpu qemu64,+smep,+smap -serial file:/tmp/boot.log \
  -device isa-debug-exit,iobase=0xf4,iosize=0x04 -display none -no-reboot < /dev/null
echo "exit $?"   # 33 = PASS (isa-debug-exit), 124 = timeout/hang
grep -E "PASS|FAIL|wait ->" /tmp/boot.log
```

`-serial file:` streams output to `/tmp/boot.log` incrementally, so you can read
progress even if the run hangs and the watchdog kills it.

---

## 3. The in-kernel user-mode debugger / API tracer

`kernel/src/ke/debug.rs`. The single most useful tool for bringing up a real
binary: it logs an **API call/return trace** (every call from the program image
into a shim, with `rcx/rdx/r8/r9` args, and the `rax` return) — exactly what the
binary asks the OS to do and what it gets back.

### Arming it

It is **disarmed on normal boots** (it floods serial / slows the suite). To
trace a binary, add one line where it is launched in `init.rs` (the module map
is already built there for where.exe; replicate for others):

```rust
ke::debug::clear_modules();
ke::debug::add_module("prog.exe", proc.image_base, proc.image_size, false); // image
ke::debug::add_module("kernel32", k32_base, k32_size, true);                // lib
ke::debug::add_module("msvcrt",   mc_base,  mc_size,  true);
ke::debug::add_module("ntdll", ntdll::trampoline_base(), 0x1000, true);
ke::debug::arm(200_000);                          // budget = image single-steps
// or: arm_with_trigger(200_000, 0x1389) to dump an instruction backtrace the
// first time a library call passes a chosen value in RDX (e.g. an "ERROR:"
// string id) — pinpoints the branch that entered an error path.
```

### Why it is fast (the hybrid design)

Full Trap-Flag single-stepping is one `#DB` (a ring3↔ring0 round trip) **per
instruction** — far too slow to leave armed. So we only single-step inside the
**program image**. On an image→library call we read the return address off the
user stack, arm it in **DR0** (a hardware execution breakpoint), and **clear
TF** — the whole library body (CRT internals, the bulk of instructions) runs at
native speed, and `#DB` fires again only on return. Cost ≈ one trap per API
call/return, ~1000× fewer traps. A full trace of where.exe to completion is ~615
calls and still boots in ~8s. Using DR0 (not an int3 patch) means we never write
to user code pages.

### Decoding offsets to names

The trace prints `kernel32+0xNNNN` / `prog.exe+0xNNNN`. Decode against the
binary on the host. macOS `/usr/bin/objdump` reads PE/COFF directly; image VMA =
preferred base (`0x140000000`) + RVA:

```sh
objdump -d --x86-asm-syntax=intel sortexe/where.exe > /tmp/where.asm
awk '/^140004f5/' /tmp/where.asm          # disassemble around RVA 0x4f50
```

For export offsets / imports / resources, parse the PE with `python3`. **Note:
the Homebrew python 3.14's `subprocess` is broken** (`ModuleNotFoundError:
_winapi`) — use `/usr/bin/python3` if you need to shell out, or avoid
`subprocess` in scripts.

---

## 4. Methodology: bringing up a new real binary

1. **Survey imports** (host python PE parser): list every imported function,
   collapse `api-ms-win-*`/`kernelbase`/`kernel32` into one namespace, diff
   against our shim exports to get the precise gap.
2. **Stub the gap so it loads.** Missing imports must bind to *something* or the
   load fails. By-ordinal misses already bind to `kernel32!__ordinal_stub`.
3. **Run under the tracer.** Watch the call/return trace; find the first call
   that returns a value the program treats as fatal (often `0`/`NULL`).
4. **Fix that one shim function** (or implement the subsystem behind it).
5. **Repeat.** Each fix advances the binary to the next wall.
6. **Verify, then disarm the tracer**, confirm `qemu-test` PASS + host tests.

This loop found and fixed every where.exe / choice.exe blocker below.

---

## 5. Real-binary scoreboard

| Binary | Status | Notes |
|--------|--------|-------|
| sort.exe | **Works** | Reads stdin, sorts, writes stdout. No arg parser. |
| where.exe | **Works** | `where cmd` → "Could not find files…" (correct; dir enum stubbed to no-matches). |
| choice.exe | **Works (interactive)** | Prompts `[Y,N]?`, reads a key, exits cleanly. |
| cmd.exe | Not yet | Apiset/ucrt-linked; gap measured below — multi-session. |

### cmd.exe scope (Win11 26H1), collapsed by name

286 import slots / 43 DLLs. We already have ~58 win32 + a few ucrt/ntdll.
Remaining gap:

- **win32: ~116 missing**, including real subsystems: `CreateProcessW`, the
  `Reg*` registry family, console **screen-buffer** APIs
  (`GetConsoleScreenBufferInfo`/`SetConsoleTextAttribute`/`FillConsoleOutput*`/
  `ScrollConsoleScreenBufferW`), critical sections + SRW locks, `FindFirstFileW`.
- **ucrt: ~84 missing**, but most are `_o_<name>` indirection aliases
  (`_o_malloc`=`malloc`, …) — alias `_o_<name>`→`<name>` and the true gap is small.
- **ntdll: ~18 missing**, incl. `RtlDosPathNameToNtPathName_U`,
  `NtQueryInformationProcess`, path/token bits.

A classic-linked shell (e.g. ReactOS cmd) would be far less work than the Win11
apiset binary, if "or equivalent" is acceptable.

---

## 6. Findings & workarounds (the gotcha catalog)

Things that were non-obvious and cost real debugging time. Each is a landmine to
remember.

- **`ob_reference_object_by_handle` does NOT take a reference** — it only looks
  up. Calling `ob_dereference_object` on its result is an *unbalanced* decrement
  that **frees a live object**. (Briefly freed the console device for all later
  writes.) For a validity check, look up only; never deref.

- **Std handles are cached in shared kernel32 `.data` across processes.** One
  process closing a std stream (sort closes stdin) leaves a *stale closed
  handle* cached for the next process → its reads fail silently (return 0) and it
  can hang. Fix: `GetStdHandle` validates the cached handle (syscall
  `NT_QUERY_HANDLE`) and re-opens `\Device\Console` when stale. This will bite
  any second interactive process — keep it robust.

- **`SetThreadUILanguage(0)` must return a concrete LANGID** (`0x0409`), not echo
  `0`. Callers treat a `0` return as fatal. General lesson: a shim that "returns
  the input" can be wrong when the real API resolves `0`/defaults to a value.

- **Version-info is real.** Modern tools call
  `GetFileVersionInfoSizeExW`/`GetFileVersionInfoExW`/`VerQueryValueW` on their
  own image during startup and treat absence as fatal. We parse the binary's own
  `RT_VERSION` resource and walk the `VS_VERSIONINFO` tree (4-byte node padding;
  text value length is in chars, binary in bytes). The "own image" is found via
  `peb_image_base()` (callers query their own path).

- **Printf width/zero-pad matters.** `_vsnwprintf` ignoring `%04x` produced
  version key `4094b0` instead of `040904b0`, so `VerQueryValueW` (correctly)
  failed the `\StringFileInfo\040904B0\…` lookup. Parse flags→width→precision→
  length in C order; honor `0`/width padding.

- **MUI: UI strings are external.** Modern tools keep only RT_VERSION/RT_MANIFEST
  in `.rsrc` and put UI strings in `<exe>.mui` (a resource-only PE). `LoadStringW`
  tries the image's RT_STRING, then falls back to the registered `.mui`
  (`ldr/mui.rs`, `NtLoadMuiString`). where/choice need their `.mui` staged.

- **Interactive input via byte-stream + synthetic key events.** choice polls
  `PeekConsoleInputW` then `ReadConsoleW`. We model console input as a byte ring;
  `PeekConsoleInputW` synthesizes a `KEY_EVENT` `INPUT_RECORD` when a byte is
  buffered (`NT_PEEK_CONSOLE_INPUT` → non-consuming `peek_input_byte()`), and the
  byte-stream `ReadConsoleW` returns the char. A fuller
  `INPUT_RECORD`/`ReadConsoleInput`/console-mode layer is still future work for
  cmd, but was not needed for choice.

- **Win64 entry frame.** A real binary's initial stack needs a return address
  (→ an ntdll terminate stub) + 32-byte shadow space above RSP; `rsp` must be
  `8 mod 16` at ring-3 entry (iretq loads RSP directly; the ABI wants post-call
  alignment, else `movaps` `#GP`s on a stack local).

- **TEB/PEB are required.** Real binaries read `gs:[0x30]`/`gs:[0x60]`. User GS
  base must be the **TEB**, not the KPCR. `ki_enter_user_mode` parks the TEB in
  `KERNEL_GS_BASE` before `swapgs`; `ki_trap_common` does a CS-RPL-gated `swapgs`
  on entry/exit (needed once any user thread's GS is the TEB and a timer preempts
  it). Only **one** TEB thread can be active at a time (GS base is per-CPU, not
  saved per thread).

- **SMAP brackets.** Any kernel read/write of a user buffer must be wrapped in
  `user_access_begin()`/`user_access_end()` (STAC/CLAC). The boot suite is the
  net that catches an unbracketed site (it faults `err=0x1`).

- **Per-process heap is blocked on shared kernel32.** A 64KiB chunk kernel32
  bump-allocates while process A is active gets sub-allocated to process B whose
  address space never mapped that VA. Per-process heaps require per-process
  kernel32 instances (private `.data`) — deferred. The shared-window allocator is
  the working compromise.

- **Single-stepping is for boundaries only.** See §3 — never leave full TF
  stepping armed; it pushes the boot suite past its timeout.

---

## 7. Syscall map (our own ABI, not NT's)

SSDT is currently size 24, services `0..21` used. See
`kernel/src/syscalls.rs` (`SVC_*` consts) and the matching `NT_*` consts in
`kernel32/src/lib.rs`. Recently added: `19 GET_COMMAND_LINE`,
`20 PEEK_CONSOLE_INPUT`, `21 QUERY_HANDLE`.

---

## 8. Deferred subsystems (each a focused, multi-step effort)

Per-process kernel32 instances (unblocks per-process heap), real directory
enumeration (`FindFirstFileW` over a ramfs directory model), console
screen-buffer model (for cmd's display), C++ exception handling, registry
persistence to disk.

**Done since (2026-06-12):**
- **Registry** — a kernel Configuration Manager (`kernel/src/cm/`): an in-memory
  hive (HKLM/HKCU/… roots, subkeys, values), `Reg*` shims over syscalls 22-26,
  9 self-tests; cmd.exe reads its config through it.
- **CreateProcess** — a real process primitive (`init::create_user_process`):
  new address space + ring-3 thread + wait + exit code, exposed as
  `CreateProcessW`/`WaitForSingleObject`/`GetExitCodeProcess` (syscalls 27-29).
  Proven end-to-end: a ring-3 process launches `C:\child.exe`, waits, reads the
  exit code, resumes.
- **Per-thread GS** — the scheduler now saves/restores `IA32_KERNEL_GS_BASE`
  per thread (KTHREAD `gs_base`), so multiple TEB processes coexist. The old
  "one TEB thread at a time" limit is gone.
- **Ring-3 faults terminate the thread, not the kernel** — a user `#PF`/`#GP`
  ends just that process; the boot suite survives a crashing binary.
- **cmd.exe** loads, runs its command loop, and exits cleanly.
