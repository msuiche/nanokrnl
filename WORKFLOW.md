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

Each phase below is framed **What / Why / How**. (The git history begins at the
baseline commit, after Phases A–G were already built, so this narrative is the
record of those — see §0.1.)

### Phase A — the kernel core
- **What:** NT-shaped subsystems from the start — `ke` (scheduler, dispatcher,
  traps, IRQL via CR8), `mm` (paging), `ob` (object manager, typed headers +
  handle table), `ps` (threads), `ex` (pool), `io` (device/IRP), `ldr`, `rtl`,
  `hal` (serial, APIC timer at vector 0xD1), with NT-exact constants (NTSTATUS,
  KGDT64 selectors, clock vector).
- **Why:** matching NT's *shapes* (KTHREAD/ETHREAD, dispatcher objects,
  IO_STACK_LOCATION) up front means real Windows drivers and binaries later slot
  in instead of fighting a foreign model — and it's the honest way to learn NT.
- **How:** kernel builds on **stable** Rust for `x86_64-unknown-none`; the
  `bootloader` crate needs **nightly** (it `-Zbuild-std`s the boot stages).

### Phase B — loading real kernel drivers
- **What:** load and run an unmodified PE/COFF **kernel driver** compiled for
  `x86_64-pc-windows-msvc`, binding its imports to a ~47-export `ntoskrnl.exe`
  surface (Ke events/sema/mutex/timer/DPC, Ex pool, Rtl, the Io IRP
  stack-location path, device namespace, Mm map).
- **Why:** a controlled, kernel-side target to prove the PE-loading + win64-ABI
  machinery *before* user mode (which is harder to debug) had to depend on it.
- **How:** a PE loader (sections/relocs/import binding) + a shared `ntabi`
  repr(C) kernel↔driver ABI + an `ntoskrnl.lib` import lib from `llvm-dlltool`.
  Key hazard: make the image **executable** without an NXE reserved-bit fault —
  clear NX along the PTE path, do **not** disable EFER.NXE. A test driver
  exercised timer→DPC→event, spinlock IOCTLs, symlinks, and unload.

### Phase C — crossing into user mode
- **What:** ring-3 execution — syscalls, a system-wide handle table, a
  `\Device\Console` (writes → serial), the first SSDT services
  (NtWriteFile/Create/Close/Read/terminate), a first no-CRT ring-3 PE
  (`userapp`), and user virtual memory (NtAllocate/Free/ProtectVirtualMemory).
- **Why:** the foundation for running any user program at all.
- **How:** program STAR/LSTAR/FMASK/EFER.SCE; a naked `KiSystemCall64` (swapgs,
  per-CPU stack switch via KPCR, Windows→SysV arg marshalling, SSDT dispatch,
  sysretq); `ki_enter_user_mode` builds an `iretq` frame to ring 3. Two ABI bugs
  fixed at the boundary: preserve RDI/RSI (Windows-nonvolatile but SysV-volatile)
  and mark the full syscall clobber set incl. r9.

### Phase D — the dynamic-linking stack
- **What:** the pipeline that makes unmodified `.exe`s viable — an **ntdll
  trampoline** (`ldr/ntdll.rs`, a ring-3 page of `Nt*` syscall stubs), a
  **kernel32 shim** (`kernel32/`, a cdylib exporting Win32 names that call our
  syscalls), a **by-name loaded-module resolver** (`ldr/loaded.rs`), and a
  CRT-style startup with argc/argv + a pooled heap + a test-result channel.
- **Why:** real binaries import functions *by name* across DLLs; we need
  cross-module binding to our own shims, and a CRT entry to run normal `main`s.
- **How:** `pe::resolve_export` parses export tables; `loaded::resolve` looks a
  symbol up across ntdll/kernel32/msvcrt **ignoring the DLL name** — the trick
  that later makes apiset binaries bind for free (§1).

### Phase E — hardening + isolation
- **What:** SMEP then SMAP; relocate the kernel to the high half; per-process
  address spaces + true multiprocessing; a broad kernel32 surface expansion
  (versioning, system info, Interlocked, timing/QPC, per-thread last-error,
  Multi/WideChar, LoadLibrary/GetProcAddress, WriteConsole, lstr\*, …) +
  ProbeForRead/Write.
- **Why:** catch bad kernel↔user accesses early (SMEP/SMAP), give each process
  real isolation, and satisfy the API surface real binaries reach for.
- **How:** `user_access_begin/end` brackets at every kernel touch of user memory
  (enabling SMEP surfaced a large-page-coarsening bug, fixed with 1G→2M→4K
  **page splitting** along the protect path); moving the kernel high frees the
  low half for per-process mappings; per-thread CR3 + AS switch on context
  switch.

### Phase F — running real Microsoft binaries
- **What:** run unmodified **sort.exe**, then **choice.exe** and **where.exe**.
- **Why:** the project's whole point — prove real MS binaries run on our shims.
- **How:** an ISO feasibility study showed `msvcrt`/`cmd` link `api-ms-win-*` →
  KERNELBASE + ucrt (not classic kernel32), which set the **own-ABI-shim
  strategy**: substitute our shims by export name, skip apiset/kernelbase
  entirely (§1). Built the **msvcrt shim**; sort needed a TEB/PEB (binaries read
  `gs:[0x30]/[0x60]`), the Win64 entry frame (return addr + shadow space + RSP
  alignment), trap-entry `swapgs` gating, and per-stream std handles. choice/where
  needed the **MUI** loader (UI strings live in an external `<exe>.mui`), a
  **RAM filesystem**, and **per-process command-line args**.

### Phase G — the debugger, and finishing where.exe + choice.exe
- **What:** an in-kernel user-mode **API tracer/debugger** (§3); where.exe and
  choice.exe made to run correctly.
- **Why:** the binaries diverged into opaque error paths; we needed to *see* what
  they asked the OS and what they got back to find the divergence.
- **How:** single-step via the Trap Flag, made fast with a **DR0-hybrid** (trap
  only at API call/return, run library bodies native-speed). It found+fixed
  where.exe's chain (`SetThreadUILanguage(0)`→0, missing version-info APIs, a
  `_vsnwprintf %04x` zero-pad bug) and the **stale std-handle** bug (a closed
  cached console handle across processes) that blocked choice's interactive read.

### Phase H — the cmd.exe era (registry, CreateProcess, shell bring-up)
- **What:** a kernel **registry** (Configuration Manager); a **CreateProcess**
  primitive + `CreateProcessW`; **per-thread GS** save/restore; ring-3 faults
  that terminate only the faulting thread; **line-input** console mode; and
  **message-table** `FormatMessage` so cmd.exe prints its real banner. cmd.exe
  loads, runs its CRT + command loop, and exits cleanly.
- **Why:** cmd.exe is the marquee target, and `CreateProcess` + the registry are
  core Windows architecture a modern CLI depends on.
- **How:** `cm/` in-memory hive + `Reg*` syscalls; `create_user_process` +
  process wait/exit-code wired to `CreateProcessW`; the scheduler restores
  `IA32_KERNEL_GS_BASE` per thread (so a parent and child TEB coexist); `#PF`/`#GP`
  from CPL 3 → `ki_terminate_current_thread`; the console read honors
  `ENABLE_LINE_INPUT`; `cmd.exe.mui`'s `RT_MESSAGETABLE` is parsed for messages.
  **Limit:** cmd doesn't execute arbitrary commands yet — command dispatch faults
  on an unresolved API (folded into the generic stub; naming it needs per-name
  stub instrumentation). Chasing it also taught: kernel32 changes must be
  verified against *all four* binaries (an env block / a GetFileAttributesW
  last-error each regressed where.exe).

The result today: four real Windows binaries run on the kernel — **sort, choice,
where** fully, and **cmd** loads/runs/banners/exits — with the loader, shims,
debugger, registry, CreateProcess, and methodology to push further.

### 0.1 A note on history
The git repository was initialized partway through (during Phase H), so the
commit log starts at a baseline that already contains Phases A–G. The
commit-by-commit code history of those phases was not captured; this section and
the dated entries in the project memory are the authoritative record of them.

### 0.2 Which model did what (Fable vs Opus)

Derived from the resumed session that built the kernel (`e470ab1a…`,
2026-06-10 → 2026-06-18). The model split is lopsided — it maps to *task type*
rather than being interleaved, and each phase ran on one model end-to-end:

| Model | ~Turns | Window | Task type |
|-------|--------|--------|-----------|
| **Opus 4.8** | ~12 | Jun 10, 13:33–13:34 (~1 min) | Kickoff: acknowledge the goal, stand up the toolchain (Rust 1.95, QEMU, lld). Interrupted almost immediately. |
| **Fable 5** | ~200 | Jun 10, 13:35–17:59 (~4.5 h) | **Greenfield scaffolding** — the whole Phase A core from nothing in one burst: workspace, IDT/trap machinery (256 stubs + dispatcher), build/run + `qemu-test` harness, README; got it booting and the self-test to PASS, then fixed the `qemu-test` 60 s timeout bug. High-volume generation from a blank slate. |
| **Opus 4.8** | ~7,500 | Jun 10 18:00 → Jun 18 (~8 days) | **Everything else** — triggered by "continue … until we can load kernel drivers." Phases B–H and the WASM efforts: driver loading, user mode, the dynamic-linking shims, SMEP/SMAP hardening, sort/choice/where/cmd bring-up, the in-kernel debugger, registry + CreateProcess, more/whoami, the first own-emulator `nanokrnl.wasm` port, and finally the qemu-wasm browser build. Long, iterative, debug-heavy work. |

Takeaway: **Fable for the initial fast-from-scratch sprint; Opus for the long,
debug-heavy bring-up tail.** The single Fable→Opus handoff (Jun 10, ~18:00)
coincided with a deliberate task shift — from "write the kernel core" to "load
real drivers and run real binaries" — so the two models were never mixed within
a task.

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
objdump -d --x86-asm-syntax=intel winbin/where.exe > /tmp/where.asm
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

---

## 9. Model notes — Fable 5 vs Opus 4.8 (a field report)

The kernel was built across one resumed session that used both Claude Fable 5
(`claude-fable-5`) and Claude Opus 4.8 (`claude-opus-4-8`), in two cleanly
separated stints (see §0.2 for the turn-by-turn timeline). Working with both
back-to-back on the same codebase made their differences concrete in a way the
spec sheets don't. This section is the field report — written so it can be
reused as a blog post.

### 9.1 The split — and what Fable actually did

Fable 5 was used **once**, in a single contiguous stint (2026-06-10 13:35 →
17:59). It wrote the kernel **from scratch** (Phase A); Opus 4.8 then did
everything after — driver loading, user mode, the shims, real-binary bring-up,
the debugger, registry, CreateProcess, and the qemu-wasm port (~7,500 turns
over 8 days, ~97% of the project). The two were never mixed within a task.

**The Fable stint at a glance** (read from the transcript):

| Metric | Value |
|---|---|
| Runs / invocations | **1** (one contiguous stint) |
| Assistant turns | 197 (28 narration, 110 tool calls) |
| Tool calls | 45 Write · 25 Bash · 18 Edit · 13 TaskUpdate · 7 TaskCreate |
| Files created/edited | 43 unique (63 write/edit ops) |
| Tokens | ~407K output (the code) on ~11K fresh input; ~27.5M served from cache |
| Active work | two bursts — ~38 min to a bootable core, then ~13 min on test fixes |

Fable opened by **decomposing the kernel the way ntoskrnl itself is built**,
creating seven tasks for itself in dependency order:

1. Scaffold workspace (kernel crate, boot builder, target config, README)
2. `rtl` — NTSTATUS, LIST_ENTRY, UNICODE_STRING, bitmap, spinlock primitives
3. `hal`/`ki` — serial KdPrint, GDT/TSS/IDT with NT selector layout, exception
   handlers, KPCR, IRQL, APIC timer
4. `mm` — PFN database, page tables, NonPagedPool, global allocator
5. `ke` — dispatcher objects, DPCs, threads, scheduler
6. `ob`/`ps`/`ex`/`io` — object manager, processes, pool API, I/O manager skeleton
7. Boot in QEMU, run smoke tests, host unit tests, finalize docs

It then executed that plan top-to-bottom and **38 minutes in had a bootable
kernel** — ~5,100 lines across 27 files, NT-exact where it is ABI (bit-exact
`NTSTATUS`, `LIST_ENTRY`/`UNICODE_STRING` layouts, the `KGDT64_*` selector
arrangement chosen so `syscall`/`sysret` bolt on later, IRQL as CR8/TPR with the
clock on vector 0xD1). The QEMU boot proved it live:

```
KiSystemStartup: running self tests
[ OK ] Mm: pool allocations succeed
[ OK ] Mm: page-table walk translates pool VA
[ OK ] Ke: KeDelayExecutionThread sleeps >= requested
[ OK ] Ke: sync event wakes one waiter per set
[ OK ] Ke: DPC queued from thread retires at DISPATCH
[ OK ] Io: null.sys DriverEntry + IoCreateDevice
[ OK ] Io: IRP_MJ_WRITE to \Device\Null consumes all bytes
[ OK ] Ob: ObCreateObject
...
ALL SELF TESTS PASSED — system idle
qemu-test: PASS (exit 33)
```

Fourteen `[ OK ]` boot self-tests covering pool, page tables, timers, events,
DPCs, the null-driver IRP path, and the object manager — ending in the
project's standing PASS contract, exit code 33.

Fable also **caught and fixed its own bugs along the way** — a useful signal for
how it behaves unsupervised:

- Trap-dispatch ordering: **EOI must precede a potential context switch** (a
  preemption mid-dispatch otherwise deadlocks the LAPIC).
- Host IRQL emulation used one global atomic shared across test threads → **must
  be per-thread** like a real per-CPU TPR; fixed with `thread_local`. This was
  the one failing host test (11/12 → 12/12).
- Two function-cast warnings; a stray attribute left in `main.rs`.
- Verified the **release** build also boots — "LTO can expose latent UB in
  low-level code."
- After the idle gap, fixed `qemu-test.sh`'s 60 s timeout (GNU `timeout` + TTY
  interaction swallowed the serial output) using `script(1)` to reproduce the
  interactive case.

The headline beat, in Fable's own words at 14:13:

> Done. `~/Documents/Projects/ntoskrnl-rs` is a working NT-compatible kernel in
> Rust — ~5,100 lines across 27 files, booting in QEMU with all self tests
> passing in both debug and release builds.

So the often-quoted "~4.5 h on Fable" is wall-clock; the kernel core went from
blank to booting-and-passing — including self-debugging — in **38 minutes**.
That is the stint that earned Fable its place in this project.

### 9.2 Fable's reasoning on a novel task

Rewriting ntoskrnl in Rust has no real precedent — there are active efforts to
write kernel *drivers* in Rust (and Linux is absorbing them), but a full,
booting, NT-shaped kernel in Rust is another level of problem. So *how* Fable
reasoned about it is the interesting question, and the most relevant to where
model capability is heading.

**The caveat first.** Fable produced 59 thinking blocks during its stint, and
**all 59 came back with empty text** — the raw chain of thought is never
returned (by design; see §9.5). We cannot inspect Fable's internal reasoning
directly. What we *can* read is its output: the design choices, the code, and
the diagnoses. On all three it reasons like a systems engineer, not a pattern
matcher.

**The strongest evidence is in the code comments** — Fable documented the
*why*, not just the *what*. From `ke/gdt.rs`, on the NT selector layout:

```rust
//! 0x10 KGDT64_R0_CODE    kernel mode 64-bit code   (CS in kernel)
//! 0x20 KGDT64_R3_CMCODE  user mode 32-bit code     (WoW64 compatibility)
//! 0x28 KGDT64_R3_DATA    user mode data            (user SS, RPL 3)
//! 0x30 KGDT64_R3_CODE    user mode 64-bit code     (user CS, RPL 3)
//! ...
//! The ordering of 0x20/0x28/0x30 is not arbitrary: x86 `syscall`/`sysret`
//! require user32-code, user-data, user64-code to be consecutive selectors
//! starting at `IA32_STAR[63:48]`... NT's layout is *designed* around that;
//! by adopting it we get syscall support for free later.
```

That is forward-looking ABI reasoning: match NT's exact GDT arrangement now, at
the segmentation layer, so the syscall path is a future bolt-on rather than a
redesign. From `ke/irql.rs`, on IRQL:

```rust
//! On x86_64 the IRQL *is* the APIC Task Priority Register, conveniently
//! architecturally aliased as CR8: an interrupt vector `v` is delivered
//! only if `v >> 4 > CR8`. ...
//! Raising IRQL is therefore a single `mov cr8, x` — no LAPIC MMIO access —
//! which is why `KeRaiseIrql`/`KeLowerIrql` are cheap enough to wrap every
//! spinlock acquisition.
```

Hardware-fidelity reasoning with the cost consequence derived from it. And from
`ke/traps.rs`, a *deliberate simplification* stated explicitly:

```rust
//! we save the volatile *and* non-volatile GP registers unconditionally for
//! simplicity; NT splits them between KTRAP_FRAME and KEXCEPTION_FRAME.
//! ...
//! Note: there is no `swapgs` handling yet because the kernel has no user
//! mode to return to; the syscall path will add it.
```

It knew what NT does, what it chose to skip, and why — and flagged the skip so
the next phase picks it up. That is design judgement, not transcription.

**Diagnostic reasoning under pressure.** When the boot script silently produced
no output, Fable root-caused it instead of retrying: `exit 124` + zero serial
output → GNU `timeout` had placed QEMU in a background process group → QEMU's
`-serial stdio` then called `tcsetattr` to put the TTY in raw mode → from a
background group that delivers `SIGTTOU` → QEMU froze before emitting a byte →
the 60 s watchdog killed it. The chain runs from an exit code to a TTY
signal-delivery rule — genuine systems debugging.

**Methodological reasoning.** Asked what to push next, it went straight for the
concentration of risk: "The dispatcher lock hand-off, spinlocks, and DPC queue
are where kernels die," proposing `loom` to exhaustively explore the
interleavings, Miri to catch UB that emulated hardware happily runs, and
`proptest` against a reference allocator model. It picked the highest-leverage
verification surface, not the most features.

The pattern across all four: Fable reasoned about *why* — ABI fidelity, hardware
constraints, forward compatibility, root cause, risk concentration. The
reasoning itself is opaque; its fruits are not, and they are the fruits of
engineering judgement applied to a problem with no prior solutions to copy.

### 9.3 The headline difference: Fable 5 refuses security-adjacent work that Opus serves

This is the difference you feel, and it explains the "I asked for a secure
implementation and it refused" experience.

Fable 5 runs **safety classifiers on incoming requests that Opus 4.8 does not
have**, targeting cybersecurity (`stop_details.category: "cyber"`) and research
biology (`"bio"`). Anthropic states plainly that Fable 5 "is not intended for
those domains," and — importantly — **acknowledges that benign adjacent work
trips false positives**: "security tooling, life-sciences tasks." A flagged
request returns HTTP 200 with `stop_reason: "refusal"` (empty content if it
fires before output, unbilled; partial output billed if it fires mid-stream).

Opus 4.8 has no such classifier. In fact **Opus 4.8 is the designated fallback
model for Fable refusals** — the documented recovery for a Fable refusal is to
re-run the same request on `claude-opus-4-8`. That is the clearest possible
signal that Opus serves the content Fable declines.

The classifier's reach is wider than "won't write an exploit." Anthropic notes
that Fable's bug-finding improvements **"exclude security-focused analysis,
where the cyber classifiers apply"** — i.e., Fable is *by design* less useful
than its raw capability suggests on anything security-flavored, including
defensive code review. For a project whose entire surface is security-adjacent
(ring-0/ring-3 boundaries, SMEP/SMAP, PE loading, the syscall ABI, a user-mode
debugger), that gating matters.

**A concrete artifact from this project.** The session's goal was set via a
session-scoped Stop hook. The Opus stint (2026-06-10 13:33:42) carried:

> "Write a compatible ntoskrnl in rust. Modern, **secure**, well
> documented/commented."

Two minutes later the Fable stint (13:35:00) restarted with:

> "Write a compatible ntoskrnl in rust. Modern, well documented/commented."

— the word **"secure" was dropped at the exact moment the model switched to
Fable.** Whether pre-emption or coincidence, it lines up precisely with the
classifier risk: framing a kernel project as "secure" is exactly the kind of
language Fable's cyber classifier keys on.

Honest caveat: Fable did **not** actually refuse anything in *this* session — 0
`stop_reason: "refusal"` events, and no refusal-style text in its turns. The
scaffolding work was benign enough not to trip the classifier. The "it refused"
experience came from a separate request; the mechanism is the same. The
interesting finding here is the softer one: **the classifier's existence shaped
how the work was framed for Fable before any refusal could occur.**

### 9.4 Capability and cost positioning

| | Claude Fable 5 | Claude Opus 4.8 |
|---|---|---|
| Positioning | "Most capable widely released model" | "Most capable **Opus-tier** model" |
| Input / output (per 1M tok) | $10 / $50 | $5 / $25 (**half the price**) |
| Context / max output | 1M / 128K | 1M / 128K |
| Tokenizer | same as Opus 4.8 (from 4.7) | — |
| Safety classifiers (cyber / bio) | **yes — can refuse** | no |

Fable sits *above* Opus in the lineup — the ceiling model for the hardest,
longest-horizon agentic work (overnight runs, first-shot system builds). You pay
double for that ceiling, and on a security-heavy codebase you also pay in
classifier friction.

### 9.5 API and behavioral differences

Both share the modern Claude API surface — both reject last-turn assistant
prefills, both reject `temperature`/`top_p`/`top_k`, both support `effort`
(`low`–`max`), adaptive thinking, task budgets, compaction, and high-res
vision. The divergences that matter in practice:

| | Fable 5 | Opus 4.8 |
|---|---|---|
| Thinking | **Always on** — `{type:"disabled"}` is a 400; raw chain-of-thought is never returned (summaries only, opt-in via `display:"summarized"`). | Adaptive — on, off, or omitted. More flexible. |
| Data retention | Requires **30-day** retention; unavailable under zero data retention (ZDR orgs get 400 on every request). | No such requirement. |
| Single-request length | Designed for **minutes-long** turns on hard tasks (a 15-min single request is normal). | Strong long-horizon, but less extreme. |
| Effort at the low end | Even `low` often beats prior models' `xhigh` — sweep it, don't default to `max`. | `high` is the recommended default; sweep per route. |
| Writing voice / agentic style | Can over-elaborate / over-plan at high effort; wants a no-tidying instruction; excels at parallel **async** sub-agents and benefits from a memory surface. | Warmer, clearer, less hedged; more deliberate — asks more often (~12 pp ask-rate drop with explicit autonomy guidance); under-reaches for search / sub-agents / memory unless prompted. |

### 9.6 When we'd reach for which

Derived from this project, not from a spec sheet:

- **Opus 4.8 is the better default for security-adjacent systems work.** No
  classifier, half the cost, and more than capable enough for kernel bring-up,
  debugging, and tooling. This is why ~97% of the project ran on it.
- **Fable 5 earns its premium on the hardest, longest, *non-security* agentic
  runs** — a from-scratch greenfield scaffold where you want maximum first-shot
  quality and the domain won't trip the classifier. That is exactly the Phase A
  burst in this project.
- **Don't frame security work for Fable.** If Fable must be used on a
  security-adjacent task, lead with the *engineering* framing (build, scaffold,
  test) rather than the *security* framing, and opt into a server-side
  `fallbacks: [{"model":"claude-opus-4-8"}]` (beta header
  `server-side-fallback-2026-06-01`) so any refusal transparently retries on
  Opus instead of failing the request outright.

### 9.7 The broader takeaway

Model choice is a **safety-surface lever**, not just a capability/cost dial.
Fable 5's extra classifiers buy a tighter guardrail on the riskiest domains —
and they cost false positives on exactly the benign, defensive work that
security engineers spend most of their time on. Opus 4.8 trades that guardrail
for uninterrupted throughput on the same work. For a project where "secure" is
the whole point, Opus wasn't the compromise choice; it was the right tool, and
Fable's higher ceiling never got a chance to pay off because the classifier
gated the very work that defines the project.
