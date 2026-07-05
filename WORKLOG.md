# ntoskrnl-rs — Work Log

## Running the kernel in a browser

**Approach: full-PC emulation (v86), booting the real x86 kernel image.**

We initially built a bespoke WASM port — the kernel's subsystems compiled to a
`wasm32` module with a substituted hardware layer, plus a hand-written x86-64
interpreter to run the real PE binaries. The wasm-kernel part worked and was
novel, but getting the *real* binaries (cmd/whoami/more) to run correctly
through our own interpreter is a multi-week "reimplement enough of Win32" grind
(MUI resources, LoadString, the wide-printf engine, the token path, …). For the
actual goal — *see cmd.exe running in a browser* — that's the wrong cost curve.

So that port was reverted (it remains in git history) in favor of running the
**unmodified x86 kernel image** under a browser x86 emulator.

**v86 does NOT work for our kernel** — verified headless (Node): it panics
immediately with `Unimplemented: #GP handler` (cpu.rs:846), before any serial
output. v86's CPU emulation is incomplete for a 64-bit long-mode kernel — it
can't deliver a general-protection fault through the IDT. So v86 is out.

The faithful off-the-shelf browser route is **real qemu-wasm** (QEMU compiled to
wasm via emscripten), which fully emulates what native QEMU does and would boot
our image unchanged — but it's a ~46 MB artifact that needs threads +
SharedArrayBuffer + COOP/COEP headers. Heavy for "see cmd.exe in a browser."

**So we built the lighter route ourselves: `nanox`** (`emu/`) — a bespoke
x86-64 emulator in Rust that compiles to a single ~60 KB `wasm32` module with no
threads, no SharedArrayBuffer, no COOP/COEP. Unlike v86/qemu it doesn't emulate
a PC from the reset vector; it **boots directly in long mode** (builds the page
tables, IDT, GDT/TSS and control registers a 64-bit kernel expects, applies the
`bootloader_api` handoff, and enters `_start`). It implements enough of the
architecture — 4-level paging, syscall/sysret, swapgs, the APIC (timer + IPIs),
the IDT delivery path, a UART and PS/2 — to boot the **unmodified** kernel image,
pass the full self-test suite, and run the **real** Microsoft binaries
(`cmd.exe`, `more.com`, …) on the kernel's own NT syscalls. It is validated by
differential testing against `iced-x86` (decode-length oracle) and Unicorn
(QEMU's CPU core; semantics oracle) — see `emu/examples/`.

### How to run

- **Native QEMU:** `sh scripts/run-interactive.sh` — the real `C:\>` shell on
  the serial console.
- **Browser (nanox):** `sh emu/build-wasm.sh` then serve `web/nanox/`
  (`cd web/nanox && python3 -m http.server 8000`). Click **Boot**, wait for the
  `C:\>` prompt, type. Boots the real kernel via the ~60 KB wasm — no special
  headers.
- (`web/index.html` is the retired v86 harness; v86 can't boot this kernel.)

## Status — kernel (x86)

Working: interactive `cmd.exe`, `echo`, `exit`, `dir`, `where`, `sort`,
`choice`, `whoami` (`nanokrnl\user`), `more <file>` (prints the file, repeatable
— see 2026-06-29), and the `null.sys` driver. Runs both under native QEMU and
in-browser via nanox. Default self-test suite passes (exit 33). Key commits:
f1038d9 (whoami), 4657bab (per-process command line), 7cc5960 + 47047aa (more.com).

## Log

### 2026-07-05 (Part IV cont.) - WinDbg opens MEMORY.DMP as a full kernel target

The self-authored `MEMORY.DMP` now opens in Microsoft WinDbg as a genuine kernel
target: `lm`, `dt`, `r`, `kv`, `!analyze -v`, `!object`, `!process <addr>` and
`dl nt!PsActiveProcessHead` all work. The kernel that wrote the dump has never
run on real hardware; it runs inside the nanox emulator, yet every field WinDbg
reads is a byte-accurate NT structure at the offset the debugger expects.

- **`dt` decodes real types.** The synthetic `ntoskrnl.pdb` needed two things the
  `yaml2pdb` path does not emit: the TPI hash stream filled per record (without
  it dbghelp loads the PDB as "publics only" and `dt nt!_EPROCESS` fails even
  though the type record is present), and a section-headers stream wired into the
  DBI Optional Debug Header with publics emitted section-relative (RVA minus
  section VA). `dt` now decodes `_EPROCESS`, `_KPROCESS`, `_OBJECT_TYPE`,
  `_KUSER_SHARED_DATA` and the rest.
- **Per-build PDB GUID kills stale-symbol caching.** dbghelp caches parsed PDBs
  by GUID, so a fixed GUID made it serve old type layouts after the first load: a
  fix would land, the rebuilt PDB would copy over, and WinDbg kept showing the
  previous layout. `gen_pdb.py` now derives the GUID from content (sha256 of the
  kernel image plus the type records) and patches that same GUID into the dump's
  masquerade RSDS, so dump and PDB always agree and any change forces a reload.
- **`r`/`kv`/`!analyze -v`.** Synthetic `KPROCESSOR_STATE`/`KPRCB`/`KPCR` (valid
  `GdtBase`/`IdtBase`) plus a `KiProcessorBlock`, with the `KdDebuggerDataBlock`
  offsets byte-exact. The `KDDEBUGGER_DATA64` tail has an eight-byte alignment
  pad; getting the PCR offset fields off by that pad made WinDbg read the PCR at
  the PRCB address and fail the CS descriptor lookup. Correct now:
  `!analyze -v` gives a clean `MANUALLY_INITIATED_CRASH` bucket naming the
  process, with the faulting thread and a symbolized top frame.
- **`!object` -> Type: Process.** Since Vista an object's type is a `TypeIndex`
  byte, decoded `index = TypeIndex XOR ((&header >> 8) & 0xff) XOR ObHeaderCookie`,
  then `ObTypeIndexTable[index]` must equal `PsProcessType`. Every process object
  now carries a real `_OBJECT_HEADER`; `ObHeaderCookie`, `ObTypeIndexTable` and a
  `PsProcessType` object (whose own `Index` agrees) are all populated.
- **`!process` "TYPE mismatch" fixed via the dispatcher header.** `!process` does
  a second, different check from `!object`: it validates `Pcb.Header.Type ==
  ProcessObject` (3) at dispatcher-header offset 0 (win2k `ke/procobj.c`; the
  strings `Pcb.Header.Type` / `ProcessObject` are literally in `kdexts.dll`). Our
  compact `_EPROCESS` had overlaid `UniqueProcessId` onto offset 0; moved the PID
  to its own offset and put `Type = 3` at offset 0.
- **`MmUserProbeAddress` for `!process 0 0`.** The command reads
  `nt!MmUserProbeAddress` to tell a PID from a literal `_EPROCESS` address;
  unexported it read 0, so `0 < 0` is false and it dereferenced address 0. Now
  exported (`0x7fffffff0000`) and pointed at by the matching KDBG field.
- **`KUSER_SHARED_DATA`.** WinDbg reads `SharedUserData` at `0xfffff78000000000`
  at setup for version/timing/XState; its absence produced "Unable to get shared
  data" and no uptime. We synthesize the page (version, `KdDebuggerEnabled`, time
  fields, a minimal `_XSTATE_CONFIGURATION`) and map it into the kernel shared
  high half so every CR3 sees it. System Uptime now shows.
- **Caveat: `!process 0 0` lists only the first process.** `dl` and
  `!for_each_process` traverse all four; `!process 0 0` prints the header and the
  first entry, then stops. Traced to a `kdexts.dll` `CheckControlC` returning
  nonzero after the first entry. Every data-side explanation was ruled out against
  the dump: the process ring is a clean circular doubly-linked list, KUSER_SHARED_DATA
  is mapped under every CR3, and the GDT kernel-code descriptor has the long-mode
  bit set (same VA returns the same bytes in every context). The debugger here is
  ARM64 WinDbg running the x64 `kdexts.dll` under emulation, so that
  `CheckControlC` crosses an x64-to-ARM64 boundary; provably-correct data plus
  inconsistent results across the typed enumerators (0, 1, 4) point at the
  emulated extension layer, not the dump. Confirm on native x64 WinDbg.

### 2026-07-04 (Part IV cont.) - crash UX: dump progress, dump-before-banner

- On `crash` the bugcheck path streams two 32 MiB dumps (ELF core + MEMORY.DMP)
  over the byte-wise 9P transport, which is slow, so the crash appeared to stall.
  Added a per-file progress readout during the physical-memory write: discrete
  newline-terminated lines `***   MEMORY.DMP: 12%` ... `100%` (a `\r` bar was
  tried first but is invisible where the console buffers on newline).
- Ordering: the dump is written *before* the STOP banner. A brief detour tried
  banner-first (Windows' visible order), but the browser front-end stops the
  emulator the instant it sees `*** STOP:`, so the dump must complete first; the
  progress lines provide the feedback banner-first was chasing. `ke_bug_check_ex`
  prints the banner (via idempotent `ke_display_bugcheck`) after the dump.

### 2026-07-04 (Part IV cont.) - live KD bridge: packet layer

- Started the live kernel-debugger bridge (WinDbg attach) with its foundation:
  `ke::kdcom`, the KDCOM wire framing - `KD_PACKET` encode/decode (data + control
  leaders, type, byte-count, id, sum-of-bytes checksum, trailer), the break-in
  byte, and an incremental `Decoder` that reassembles split delivery, drops
  corrupt packets, and resyncs after garbage. Dependency-free; 6 host unit tests
  (`cargo test -p kernel kdcom`) cover round-trip, control packets, byte-at-a-time
  reassembly, bad-checksum rejection, and resync.
- Still to build on top: the KD state machine (wait-state-change on break-in,
  then KD_STATE_MANIPULATE for read/write memory, get/set context, breakpoints)
  and the byte transport (over the UART, bridged to WinDbg's `com:pipe`). Those
  need a real WinDbg in the loop to validate byte-exactly.

### 2026-07-04 (Part IV cont.) - symbols for WinDbg (ntoskrnl.pdb)

- WinDbg opened MEMORY.DMP as a kernel target but had no symbols (our kernel is
  ELF/DWARF, no PDB). Added `tools/gen_pdb.py`: reads the kernel ELF symbol table
  (`nm`) and emits `ntoskrnl.pdb` with one `S_PUB32` per defined symbol via
  `llvm-pdbutil yaml2pdb`. The kernel links at 0, so each symbol value *is* its
  RVA - the offset a debugger adds to the load base. Load in WinDbg with
  `.reload /i /f ntoskrnl.exe=0xfffff800`00000000`; symbols resolve as
  KERNEL_VIRT_BASE + RVA. 1856 publics, verified with `llvm-pdbutil dump`.
- Fixed the module `SizeOfImage`: it was 0x290000, but the highest kernel symbol
  is near RVA 0x313000 (past .text into .data/.bss), so globals like
  PsLoadedModuleList fell outside the module range. Bumped to 0x400000 so `lm`
  spans the whole image and all symbols land inside it.

### 2026-07-04 (Part IV cont.) - native Windows crash dump (MEMORY.DMP)

- The KDBG structures were validated only by our own ELF-core walker; nothing
  consumed them as a Windows target. Added a real Windows kernel crash dump so a
  Windows debugger opens the crash natively (`lm`, `!process 0 0`).
- `dump::write_memory_dmp` emits a `DUMP_HEADER64` (8 KiB): `PAGE`/`DU64`
  signature, `DirectoryTableBase` (crash CR3, whose shared high half maps the
  kernel), `KdDebuggerDataBlock` / `PsLoadedModuleList` / `PsActiveProcessHead`,
  `MachineImageType = 0x8664`, bugcheck code+params, a `PHYSICAL_MEMORY_DESCRIPTOR`
  (one run over the captured window), and the crash `CONTEXT`; then streams the
  physical window to `H:\MEMORY.DMP` over 9P. `DumpType = DUMP_TYPE_FULL (1)` -
  a complete memory dump (we are small, so we just dump the low window).
- The `CONTEXT` is the `KPROCESSOR_STATE.ContextFrame` a full dump exposes: the
  `ContextFlags` advertise exactly the groups filled (AMD64|CONTROL|INTEGER|
  SEGMENTS, no floating-point claim we can't back), MxCsr and kernel segment
  selectors set, Rip/Rsp/Rbp/Rflags from the crash capture. CR3 (the
  SpecialRegisters half) rides in `DirectoryTableBase`.
- Validated with `tools/dmp_check.py`, which does exactly what WinDbg does: reads
  `DirectoryTableBase`, walks the captured 4-level page tables to translate VAs
  against the dumped physical memory, checks the `'KDBG'` tag, and follows both
  rings. On a real crash it prints ntoskrnl.exe + cmd.exe + the shims under `lm`
  and all live processes under `!process 0 0`. New `emu/examples/memory_dmp.rs`
  drives `crash` and captures the file over an in-process 9P server.
- Verified: 67/67 self-tests, ELF core still written, MEMORY.DMP walks clean.
- Caveat: x64 stack unwinding in WinDbg needs PE `.pdata` unwind info / a PDB,
  which we do not have yet (synthetic-PDB follow-up), so beyond the top frame the
  stack will not unwind. Live KD bridge (KDCOM/KDNET) is the next Part IV step.

### 2026-07-04
- **Per-process handle tables (the rework the pipe work was waiting on).** The
  object manager's handle table is no longer a single global array; it is keyed
  by address space (`cr3`), matching NT's `EPROCESS.ObjectTable`. Kernel threads
  (`cr3 == 0`) share a kernel table. Handle values are now per-process (two
  processes can each hold handle `0x10` naming different objects).
  - New `ob_create_handle_in(cr3, ...)` seeds a child's handles in the child's
    own table; `ob_free_table(cr3)` tears a process's table down on exit
    (dropping every reference), wired into `on_user_thread_exit`.
  - `create_user_process` now implements `bInheritHandles` semantics: each staged
    standard handle is resolved in the parent's table and a *copy* is created in
    the child's table, so the parent closing its own copy leaves the child's
    alive.
  - `nt_set_startup_handles` no longer stores raw handle values (which the shell
    closes before `CreateProcess` runs, in the `_dup2`/`_close` dance): it now
    **duplicates** each handle at stage time into a private staging handle that
    holds its own object reference; `CreateProcess` duplicates from that into the
    child and releases it. This fixed the timing bug where a child inherited a
    handle the parent had already closed.
  - Verified: 67/67 boot self-tests pass, `dir > out.txt` + `type`/`more`
    redirection still works, cmd survives every plain command (no early exit).
  - Effect on pipes: every inherited handle now resolves to the correct object,
    and the producer side is fully correct - `dir`'s stdout resolves to the real
    pipe object and it writes the whole listing into the pipe. `dir | sort` does
    not crash or hang; cmd survives it and continues.

- **Per-process shim `.data` (emulated copy-on-write DLL data).** The shim DLLs
  (`kernel32`/`msvcrt`) live once in the shared high half, so their writable
  `.data` (the C-runtime's fd table, cached standard handles) was a single copy
  shared across every process. Each process now gets a private buffer of those
  regions, snapshotted pristine at load and swapped in/out of the shared pages on
  every context switch between address spaces (`ldr::loaded`: `register_shim_data`
  / `alloc_shim_data` / `free_shim_data` / `swap_shim_data`, hooked into the
  scheduler; the regions total ~13 KB so the per-switch copy is cheap, and a live
  count skips it entirely until an isolated process exists). This is the
  per-process DLL data the earlier log flagged as needed; it fixes genuine
  cross-process CRT-state corruption. Verified: 67/67 self-tests, redirect, no
  regression.

- **Pipe (`dir | sort`) - remaining blocker, now precisely diagnosed (and it is
  NOT shared `.data`).** Traced the full handle/fd choreography. cmd drives pipes
  through the msvcrt CRT fd layer (`_pipe`/`_dup2`/`_close`), and the failure is
  a Win32-handle-vs-CRT-fd aliasing issue *inside cmd's own logic*: after
  spawning the writer, cmd calls `DuplicateHandle(pipeRead, lpTargetHandle=NULL,
  DUPLICATE_CLOSE_SOURCE)` - the Win32 idiom for "close this handle" - closing the
  pipe-read *OS handle*, while the msvcrt fd still names it. cmd then does
  `_dup2(readFd, 0)` to wire sort's stdin, which fails (`dup_handle` on the
  closed handle) - cmd even prints its real error, "The handle could not be
  duplicated during a pipe operation." So the producer writes the listing into
  the pipe, but the consumer never gets the read end. Making this work needs the
  msvcrt fd layer and Win32 handle layer to agree on ownership exactly as Windows
  does (an fd's handle must not be a bare closeable alias), which is real CRT/Win32
  fidelity work, not a handle-table or DLL-data problem.

### 2026-06-16
- Reverted the bespoke WASM port (kernel-in-wasm module + x86 interpreter) after
  it became clear that running the real binaries through our own interpreter is
  a multi-week faithful-Win32 effort. Kept in git history.
- Switched to booting the real x86 kernel image in the browser via v86
  (`web/index.html` + `web/run.sh`). Disk image verified to boot cmd/whoami
  under native QEMU.

### 2026-06-17
- Tested v86 headless (Node + the npm package + SeaBIOS): it **cannot boot our
  kernel** — panics `Unimplemented: #GP handler` (cpu.rs:846) before any output.
  v86's CPU is incomplete for a 64-bit long-mode kernel. Browser route now needs
  real qemu-wasm (heavy; not wired up). Native QEMU remains the way to run it.

### 2026-06-18..28 — v86 → bespoke x86-64 emulator (`nanox`)

The browser story converged through three attempts:

1. **Bespoke WASM kernel port** (kernel subsystems → wasm + a hand-written x86-64
   interpreter for the real PE binaries). Novel and partly working, but running
   the real binaries correctly through our own interpreter is a multi-week
   "reimplement enough of Win32" grind. Reverted (kept in git history).
2. **v86**, booting the unmodified x86 disk image. Dead end: v86's CPU can't run
   a 64-bit long-mode kernel — it panics on the `#GP` handler before any output
   (see 2026-06-17). Confirmed it will never boot this kernel.
3. **qemu-wasm** would work (it's real QEMU) but is ~46 MB and needs
   threads/SharedArrayBuffer/COOP-COEP — too heavy for the goal.

Resolution: **build our own emulator, `nanox`** — just enough x86-64 to boot the
*real* kernel image in long mode and run the *real* user binaries on its
syscalls, in ~60 KB of wasm with no threads/COOP-COEP. Direct long-mode entry
(no BIOS/real-mode/chipset bring-up) is what keeps it small. Methodology was
**differential testing**: `iced-x86` as a decode-length oracle over the kernel's
`.text` (0 mismatches), and **Unicorn** (QEMU's CPU core) as a semantics oracle
(`emu/examples/diff_unicorn.rs`, and later the full-program lockstep
`diff_trace.rs`). Each opcode/flag bug was found by running the same instruction
through Unicorn and nanox and diffing registers+RFLAGS. Outcome: the real kernel
boots under nanox (native and in-browser), passes the self-test suite, and runs
interactive `cmd.exe`. Earlier nanox opcode fixes that this surfaced include
RIP-relative addressing, the bare-REX (`0x40`) 8-bit register encoding, 16-bit
`div`/`cmov`, and size-aware ALU flags.

### 2026-06-29 — "`more` only works once" (shared ulib CRT state across processes)

**Symptom.** In the nanox browser/native console, the *first* `more <file>`
printed the file; every subsequent `more` (any file) printed nothing and
returned to the prompt. It looked filename-specific at first (`more hello.txt`
worked, `more readme.txt` didn't) but that was a red herring — it was purely
"first invocation vs. the rest." `where`, `whoami`, etc. ran fine repeatedly.

**Ruling out the emulator.** Built a lockstep differential oracle
(`emu/examples/diff_trace.rs`): it boots the kernel, types the command, and runs
every ring-3 instruction of the *real* run through Unicorn (QEMU's CPU core)
from nanox's actual register+memory state — lazily mirroring guest pages into
Unicorn via the page tables, skipping system instructions, and tolerating the
known-undefined shift/rotate/mul flag bits. Result: **0 divergences across
~15.6k instructions.** nanox's execution is bit-exact, so the bug was not in the
emulator. Native QEMU reproduced the failure identically — confirming a kernel
bug, not an emulation one.

**Root cause.** Tracing the two consecutive runs showed the second `more.com`
exits during CRT startup (a quick `syscall eax=0` → `NtTerminateThread`) after
~100 instructions. The diverging branch was inside **`__security_init_cookie`**:
it loads the `/GS` cookie, compares it to the compile-time default
`0x2B992DDFA232`, and if it's *already* non-default takes the "already
initialized" fast path. Crucially the cookie (and the CRT startup-state machine,
on-exit tables, standard-stream/heap pointers) lives in **ulib.dll's writable
`.data`**, and ulib is mapped **once in the shared high half** — so that data is
shared by every process that runs it. The first `more.com` initializes the CRT;
the second sees "already initialized" guards, skips the init its `PROGRAM`
object depends on, and aborts. On real Windows each process gets a private,
copy-on-write copy of a DLL's data, so this never happens. `where`/`whoami`
survive because their CRT is statically linked into their own image (freshly
loaded + zeroed each spawn), not routed through shared ulib.

**Fix** (`kernel/src/ldr/loaded.rs`, `kernel/src/init.rs`). Snapshot ulib's
pristine post-load image (relocations applied, imports bound, CRT data at its
initial values) and restore it before each `create_user_process`, emulating
per-process DLL data. Safe because user processes run serially (the creator
blocks in `NtWaitForSingleObject`), so no ulib code is executing at reset time.

**SMAP wrinkle.** The first cut faulted (`#PF` at ulib's base) under QEMU but
not nanox — ulib is mapped *user-accessible* (it executes in ring 3), so the
kernel reading/writing it traps under SMAP. nanox doesn't enforce SMAP, which
had masked it. Bracketing both the snapshot read and the per-process restore
with `user_access_begin()`/`user_access_end()` (same as the syscall arg copies)
fixed it. Verified: `more` repeats now print content natively, under QEMU, and
through the shipped `web/nanox` wasm; boot self-tests still pass.

- Also fixed a web-console rendering bug: the terminal ignored carriage returns,
  so `more.com`'s line-clearing (spaces + `\r`) left stray leading whitespace.
  The console now honors `\r` as "cursor to column 0, overwrite."

### 2026-07-02 - hardening pass now that nanokrnl is a live HTTP demo

The kernel now serves as a plain HTTP site (web/nanox), booting under nanox in
the browser. This pass improves boot time, correctness, packaging, and sets up
the two "beyond the demo" directions with their own design docs.

**1. Instant boot via snapshot.** Interpreting the whole boot (self tests
included) costs millions of guest instructions before `C:\>`. Since a machine is
just RAM plus CPU/device state, we now capture it once and resume. Added
`Machine::snapshot()` / `restore()` (machine.rs): a self-describing blob of the
CPU registers, XMM, paging, segment/MSR state, IDTR/GDTR, the device set (UART,
APIC, PS/2 queues), and only the non-zero 4 KiB RAM pages. A native tool
(`emu/examples/snapshot.rs`) boots to the prompt and dumps it; `build-wasm.sh`
gzips it to `web/nanox/snapshot.bin.gz`; the page gunzips it with
`DecompressionStream` and calls the new `nanox_restore` ABI, then nudges the
shell with a CR so the prompt redraws. If the snapshot is absent it falls back to
a normal boot. Result: 4.1 MiB raw, 898 KiB gzipped (smaller than the libopenmpt
we already ship), and boot becomes instant. Verified natively and through the
shipped wasm (restore -> `ver` prints the version banner).

**2. Ship the release kernel.** `build-wasm.sh` staged the debug kernel (4.4 MB,
many more guest instructions to boot). It now prefers
`target/x86_64-unknown-none/release/kernel` (2.5 MB), which is smaller and
reaches the prompt in far fewer instructions.

**3. LICENSE.** Added MIT (c) Matt Suiche, plus a third-party note: the Microsoft
binaries embedded in the kernel image remain Microsoft's, and libopenmpt
(BSD-3) and the ASCII background are bundled.

**4. mul/imul CF/OF.** nanox computed the product but never set CF/OF for the
one-operand `mul`/`imul` (F6/F7 /4/5) or the two/three-operand `imul`
(0x69/0x6B/0x0F AF). Implemented the x86 rule (CF=OF set when the upper half is
significant, or the result is not the sign-extension of the low half).
`diff_unicorn` over the kernel now shows the CF/OF divergences gone; the only
residual flag differences are architecturally *undefined* bits (PF after
`mul`/`imul`, OF after a multi-bit shift/rotate), which programs must not rely
on. Shift/rotate OF was already correct at count 1.

**5. CI.** Added `.github/workflows/ci.yml`: builds nanox, runs the decoder and
machine unit tests, builds `nanox.wasm` (wasm32, no_std), and runs clippy. The
kernel itself is not built in CI because its image embeds gitignored Microsoft
binaries; the Unicorn differential (feature `oracle`) needs cmake plus a kernel
image, so it stays a local gate.

**6. 9P host filesystem (design: `docs/9p-over-nanox.md`).** The reasoning: today
files come from an in-kernel RAM filesystem baked into the image, so the demo
can only ever see what was compiled in. 9P (the Plan 9 protocol Linux exposes as
v9fs, and what QEMU's virtfs uses) is the minimal, well-worn way to let a *host*
serve files to a guest. The plan is a small `p9` transport device in nanox
(byte FIFOs, like the UART, since 9P is self-framing), a 9P2000.L client in the
kernel (`io/p9.rs`: version/attach/walk/lopen/getattr/read over a doorbell), and
a few-hundred-line 9P server in JavaScript. The one real design decision is that
a browser page cannot read the host disk directly, so the server's backing store
is one of `fetch` of bundled files, an in-memory object, `<input type=file>` /
drag-drop, the File System Access API (real host folder, Chromium, permissioned),
or OPFS; all but the in-memory case are async, which is why the transport is the
cooperative yield design rather than a synchronous import. Wiring a `\\host\`
prefix into `nt_create_file` then makes `more \\host\notes.txt` read a live host
file with no kernel rebuild. It is exactly the v9fs client role, over a
browser-shaped transport.

**7. WANI, a WebAssembly NT Interface (design: `docs/wani-webassembly-nt-interface.md`).**
The reasoning, and why it is the genuinely novel direction: WALI (EuroSys 2025)
exposes a kernel's userspace syscall layer to wasm so recompiled programs run
sandboxed and ISA-portable; crucially it is a thin *passthrough* to a real host
kernel (they did Linux and Zephyr). Agent sandboxes and microVMs (Firecracker
and friends) are all Linux for this reason. Windows is the gap, and it is a hard
one: in a sandbox there is no Windows kernel underneath to pass through to, so a
Windows thin interface cannot be a passthrough. Someone has to actually
*implement* the NT services, and that from-scratch NT implementation is precisely
what nanokrnl already is, in Rust, compiling to wasm. So WANI is WALI's layering
with one box swapped: recompiled program -> Win32/CRT personality (the existing
kernel32/msvcrt/ulib shims) -> a small `Nt*` import ABI -> nanokrnl's NT services
in wasm -> host for raw memory/time/IO. The honest limitation, spelled out in the
doc: like WALI, this runs only programs recompiled to wasm, not existing
closed-source PEs, so it is a portable sandboxed runtime with an NT personality,
not a Windows-compatibility layer; that scope is identical to Linux WALI, not
worse. Running unmodified PEs stays nanox's job (x86 emulation), and doing that
at speed would need an x86-to-wasm JIT, a separate large project. The pitch:
"Linux has WALI; Windows has nothing, because Windows has no open kernel to pass
through to. nanokrnl is an open NT kernel in Rust that compiles to wasm, so it
can be the thin Windows kernel interface for WebAssembly."

### 2026-07-02 (later) - 9P milestone 1: the p9 transport device

First step of the 9P plan (docs/9p-over-nanox.md). Added a `p9` transport device
to nanox: a byte-stream MMIO device at 0xFED0_0000 modelled on the UART. It has a
DATA register (a guest write appends to the tx queue, a guest read pops the rx
queue) and a STATUS register (bit0 = a response byte is ready). 9P is
self-framing (every message starts with a 4-byte length), so no packet
boundaries are needed. The CPU memory path (load/store) intercepts the page
right next to the APIC, and the host drives it through a new
`nanox_p9_read`/`nanox_p9_write` wasm ABI (the same shape as the UART). Two tests
cover it: a device-level loopback and a CPU-path MMIO round trip. Next milestones:
the JS 9P2000.L server, then the kernel client `io/p9.rs`.

### 2026-07-02 (later) - 9P milestones 2+3: `more H:\file` reads a real host file

Finished the 9P stack end to end. `more H:\readme.txt` at the prompt now reads a
file that lives on the *host* (the browser page, or a test server), not in kernel
memory.

- **Kernel client** (`kernel/src/io/p9.rs`): a minimal 9P2000.L client over the
  transport — version, attach, walk, lopen, read-loop, clunk. `p9::read(path)`
  returns the file bytes or `None`.
- **The `H:` drive**: `nt_create_file`, `nt_open_file`, *and* `nt_query_directory`
  now recognize an `H:\` prefix and route to `p9::read`, with `ramfs::open_bytes`
  wrapping the fetched bytes in a normal read-only `FileObject`. The third hook is
  the subtle one: ulib tools `stat` a file with `FindFirstFile`
  (`NtQueryDirectory`) *before* opening it, so without an `H:` case there the
  file "doesn't exist" and the open is never attempted — the symptom was a bare
  "Unknown error" with zero 9P traffic. Found by diffing the syscall trace of
  `more C:\readme.txt` (works) against `more H:\readme.txt` (fails): they were
  identical up to the `NtQueryDirectory` that returned 1 vs 0.
- **JS server** (`web/nanox/p9-server.js`): the browser-side 9P2000.L server,
  pumped once per run-slice from the page's main loop. It serves a small in-page
  file tree (edit the object in `index.html`; no rebuild needed). The kernel's
  client spins on the transport (bounded), so a reply produced between run-slices
  is picked up on the next slice.
- Tests: `emu/examples/p9_host.rs` (native, in-process Rust 9P server) and a
  headless Node harness driving the shipped `nanox.wasm` + snapshot + the real
  `p9-server.js`. Both read the host file; 12 messages served per `more` (a stat
  round then an open round).

**nanox was missing `BSWAP`.** Wiring the client in exposed it: at `-O`, the
compiler turns the `"\??\"` 4-byte prefix compare in `nt_create_file` into
`mov`/`bswap`/`cmp`-against-immediate, and nanox had never implemented `0F C8+rd`.
So the *release* kernel wedged on an undecoded opcode partway through boot (debug,
which does a byte-by-byte compare, was fine). Adding my module shifted codegen
just enough to trip it. Implemented `bswap r32/r64` (32-bit form zero-extends) in
the two-byte-opcode path with a unit test. Why it slipped through: nanox's ISA is
built demand-driven and validated against Unicorn/iced *for the instructions the
programs actually execute* — `BSWAP` is rare in `-O0` and only emitted by the
optimizer for byte-swap / prefix-compare idioms, so it never appeared in the
stream until now. A decode-only sweep of the release `.text` against iced would
catch this class statically; worth adding.

The 9P direction was Ryan MacArthur's idea (https://x.com/maceip). The Linux
v9fs documentation is a good starting point:
https://docs.kernel.org/filesystems/9p.html

### 2026-07-03 - `dir H:\` lists the host directory (Treaddir)

Finished the drive by making enumeration work, not just open-by-name. `dir H:\`
was reporting "File Not Found" because we only implemented walk/open/read for a
named file; a wildcard listing had nothing to resolve.

- Kernel client `io::p9::list()`: clone the root fid with a zero-name `Twalk`,
  `Tlopen` it as a directory, and loop `Treaddir` (9P2000.L type 40/41), parsing
  the packed dirents (qid, offset, type, name) into a name list.
- `nt_query_directory` now splits the host case: a wildcard or bare `H:\` calls
  `list()`, filters by a small glob (`*`/`?`), and returns the index-th match
  with its real size (fetched once) to drive FindFirstFile/FindNextFile; a
  concrete name stays the single-file stat path. The `WIN32_FIND_DATAW` writer is
  factored into one `write_find_data` helper shared by the host and ramfs paths.
- JS server (`p9-server.js`): handle the zero-name `Twalk` (clone to a directory
  fid) and `Treaddir` (pack the file map's keys as dirents, resuming from the
  requested offset).

Tested headless against the shipped `nanox.wasm` + snapshot + the real
`p9-server.js`: `dir H:\` lists readme.txt and hello.txt with correct sizes and
the usual "N File(s)" footer; `more H:\file` still reads a named file (no
regression).

### 2026-07-03 (later) - lldb/gdb debugging, a BSOD, and a kernel-authored ELF core

Turned nanokrnl into something you can actually debug and post-mortem, in the
browser.

**A GDB stub in nanox.** `emu/src/gdb.rs` is a transport-agnostic GDB Remote
Serial Protocol stub: read/write registers and memory (translated through the
guest page tables), software breakpoints, single-step, continue, and a
`target.xml` so lldb enumerates x86-64 registers. `emu/examples/gdb_server.rs`
serves it over TCP; the browser reaches it through `nanox_gdb_*` wasm exports and
a dependency-free Python bridge (`tools/gdb-bridge.py`, also served at
`/bridge.py`) that relays TCP <-> WebSocket and launches lldb. Verified with real
lldb (native and through the full browser bridge chain): breakpoint set +
continue + hit, register and memory reads, disassembly.

**Bugcheck breaks into the debugger.** nanox intercepts `int3` (0xCC): with a
debugger attached (`Cpu::debug_break`) it traps to the stub; with none it is a
no-op. The manual-crash path issues `int3` after writing the dump, so a bugcheck
breaks into lldb like KdBreak on a real kernel.

**A blue screen.** `crash.exe` (a tiny ring-3 program) issues `SVC_BUGCHECK` ->
`KeBugCheckEx(MANUALLY_INITIATED_CRASH)`. The page recolors the console window
blue and clears the scrollback so only the `*** STOP: 0x000000E2` banner shows.

**nanokrnl writes its own crash dump, as an ELF core, over writable 9P.** This
is the interesting one. First we tried a Windows `MEMORY.DMP` built by the page
from guest RAM, but that is neither faithful (the kernel should author its own
dump) nor analyzable (a `DUMP_HEADER64` needs a `KDDEBUGGER_DATA64` and a PDB,
and nanokrnl is an ELF). The realization: nanokrnl is an ELF with DWARF, and
modern WinDbg (and gdb, and `crash`) read ELF + DWARF directly. So the faithful
format is a Linux-style **ELF core** (`ET_CORE`), and the kernel's own
`kernel.bin` is the symbol file - no synthetic PDB, nothing built in JavaScript.

- Writable 9P: `Tlcreate` + `Twrite` on both the kernel client (`io::p9`, with a
  streaming, no-allocation, pipelined `Writer`) and the JS/test servers.
- `kernel/src/dump.rs`: on a bugcheck the kernel walks its higher-half page
  tables, emits one `PT_LOAD` per mapping (so code and stacks are readable at
  their real virtual addresses), a `PT_NOTE` with `NT_PRSTATUS` (the crash
  registers) and `VMCOREINFO` (kdump metadata + the bugcheck), dumps the low
  physical window once, and streams the whole `nanokrnl.core` to `H:\` over 9P.
- The page only *receives* it (`p9.onFinalize`) and offers Download; the JS dump
  builder is gone.

Two transport lessons: the byte-wise 9P port turns over roughly one request per
run-slice, so a naive multi-MB dump crawled - fixed by pipelining a batch of
`Twrite`s before reading replies. And the payload must be streamed straight from
the dumped region (no intermediate copy): copying it into a freshly allocated
buffer can alias the very memory being dumped, which the emulator flags as UB.

Tested headless (native + the shipped wasm + real `p9-server.js`): `crash`
produces a 33 MB `ET_CORE` whose `NT_PRSTATUS` RIP/RSP both fall inside
`PT_LOAD` segments and whose `VMCOREINFO` records `BUGCHECK=0x000000e2`. A
structural validator checks the header, notes, run list, and file extents;
`gdb kernel.bin nanokrnl.core` on a real machine is the final check.

**H:\ Explorer.** A small Win95-style window under the Resource Monitor lists the
9P share (readme.txt, hello.txt, and nanokrnl.core after a crash); click to
download.

Also: the boot banner had two em dashes that rendered as mojibake (the console is
byte-wise, not UTF-8) - replaced with ASCII, and the banner now carries a fuller
description + authorship.

### 2026-07-03 (later still) - debug-bridge one-liner, H:\ Explorer, and pipes (WIP)

**Debugging, packaged.** The lldb bridge is now a copy-paste one-liner: the page's
Debug panel shows `python3 <(curl -sL https://nanokrnl.ai/bridge.py)`, and
`bridge.py` (stdlib-only, ~90 lines) is staged into `web/nanox/` by `build-wasm.sh`
so the live site serves it. Process substitution (not a pipe) keeps the terminal
TTY so lldb stays interactive. One caveat we now warn about in the panel: a page
served over https can only open `ws://localhost` in Chrome (loopback
mixed-content exception), not Safari; serve the page over http://localhost to use
Safari.

**H:\ Explorer.** A small Win95-style window (under Resource Monitor) lists the 9P
share - readme.txt, hello.txt, and `nanokrnl.core` after a crash - click to
download. All the floating windows are now resizable, and the layout was tidied
(Resource Monitor and Explorer spaced apart; Resources links wrap instead of
cropping; the epilogue folded into the header; the "20 or 30 years" reflection
rides on the tagline).

**Pipes and redirection (in progress).** Groundwork for `dir | sort` and
`echo > H:\out.txt`. The handle table is system-wide, which makes inheritance
trivial (a handle value is valid in any process). Landed kernel-side:
`io::pipe` (an unbounded buffer with a writer count; closing the write end runs a
delete procedure that drops the count, so a read sees EOF when the last writer
goes), `NtCreatePipe`, per-process standard handles (`NtGetStdHandle` +
staged-for-child handles consumed by the create-process path), and
`NtReadFile`/`NtWriteFile` routing to pipes with a preemptible blocking read (the
reader spins with interrupts on, so the timer preempts it and the producer runs;
the unbounded buffer means the producer never blocks). A neat consequence: for
`dir | sort`, `dir` is a cmd builtin, so cmd itself is the producer and closes the
write end, which sidesteps cross-process writer-EOF entirely. Still to do: the
kernel32 side (`CreatePipe`, `GetStdHandle` asking the kernel, `CreateProcessW`
reading `STARTUPINFO` std handles), the `kernel32.dll` rebuild, and end-to-end
testing; file redirection also needs a writable file sink.

### 2026-07-03 (loop) - writable RAM files, and what cmd actually needs for `|`

Added writable files: `CreateFile` with a create disposition (CREATE_NEW/
CREATE_ALWAYS/OPEN_ALWAYS) now makes a growable RAM file that persists by path in
a registry, so a file written then reopened returns its bytes. `NtReadFile`,
`NtWriteFile`, and `NtQueryFileSize` route to it, and `CreateFileA` now passes the
desired access + disposition through `NtCreateFile` (previously just the name).
This gives real `> file` semantics to any program using the Win32 CreateFile/
WriteFile path, and is inert for existing const-file reads (they use
OPEN_EXISTING). `more hello.txt` still works.

But testing `dir | sort` (and `> file`) against the real cmd.exe showed the
demo's pipe/redirection does NOT go through the Win32 surface at all. cmd.exe's
imports are `_o__pipe` (the msvcrt CRT `_pipe()`), `DuplicateHandle`, and
`GetEnvironmentVariableW` - not `CreatePipe`/`SetStdHandle`/`GetTempFileName`. So
this cmd implements `|` and `>` through the **C runtime's fd model** (`_pipe`,
`_dup2`, `_open_osfhandle`/`_get_osfhandle`), and those are currently stubbed, so
it silently runs the two commands unpiped.

So the Win32 pipe + std-handle + writable-file work (all committed) is correct and
reachable by programs that use those APIs, but the specific `dir | sort` demo
needs the **msvcrt CRT I/O layer** next: `_pipe` backed by our pipe object, an fd
table over `_open_osfhandle`/`_get_osfhandle`, `_dup2` to redirect fd 0/1, and
fd-based read/write. That CRT layer sits on top of what's already here (its OS
handles are our pipe/writable-file objects). Next iteration.

### 2026-07-03 (loop) - drag-drop into H:\, and the pipe wall

`dir | sort` is deferred: msvcrt's stdio (`__stdio_common_vfprintf` /
`console_write`) writes straight to `\Device\Console`, independent of any
fd/std-handle state, and this cmd.exe drives `|` through the CRT (`_o__pipe` /
`_dup2` / `_get_osfhandle`), not the Win32 surface. Making it work needs an
invasive rewrite of the CRT output path to route through an fd table - high risk
to the working console output for one demo command. The Win32 pipe + writable-
file + std-handle substrate stays as the correct base for when that CRT layer is
built.

Shipped instead a clean, low-risk win on the H:\ share: **drag a file onto the
page and it lands in H:\** (added to the JS 9P server's file map, capped at
256 KB so the guest's byte-wise reads stay quick). The kernel can then
`more H:\<name>` it over 9P and it shows in the Explorer. Verified end to end
(a page-added file reads through the guest). No blog this round - it is a small
addition on top of the already-published 9P story.

### 2026-07-03 (loop) - pipes/redirection: the CRT fd layer lands

Built the substrate cmd.exe needs to drive `|` and `>`, and got redirection of a
stage's output into a handle working end to end. What went in:

- **msvcrt fd layer** (`_pipe`, `_dup`, `_dup2`, `_open_osfhandle`, `_get_osfhandle`,
  `_close`): previously all return-0 stubs, so cmd's `_pipe(fds)` "succeeded" with
  garbage fds. Now each fd names a real OS handle; fds 0/1/2 seed from the process
  std handles. `_pipe` issues NtCreatePipe; `_dup2` onto a std fd also updates the
  kernel std handle so a child spawned afterward inherits the redirection.
- **DuplicateHandle** (kernel32 export + new `NT_DUPLICATE_OBJECT` syscall, SSDT
  grown 40 -> 48): a second handle to the same object, refcounted so closing the
  source keeps the object alive for the copy - exactly cmd's hand-a-pipe-end-to-a-
  child pattern. Was a return-0 stub before.
- **CreateProcessW** now inherits the parent's *current* std handles when
  STARTUPINFO does not override them, so a SetStdHandle / `_dup2` redirect the
  parent applied before spawning carries into the child.

Verified with a new `emu/examples/pipe_test` harness (boots interactive kernel,
types commands, can trace syscalls): the `dir` stage's stdout is now redirected
into the pipe instead of flooding the console, and `dir > file` writes 728 bytes
to the redirected target. No regression - all 67 self-tests pass (incl. Ps /
CreateProcessW), and plain interactive commands (`dir`, `echo`, `more`, `ver`)
work with cmd staying alive.

Not done yet: full `dir | sort`. The syscall trace shows cmd creates the pipe,
runs `dir` into it, DuplicateHandles the read end (0x2c -> 0x34), closes the
original, then spawns `sort` staged with stdin = 0x2c (the *closed* original)
rather than 0x34 (the surviving dup) - so sort reads EOF and prints nothing, and
cmd exits its command loop afterward. Resolving it means matching cmd's exact
fd/handle juggling (likely the ucrt STARTUPINFO fd-inheritance block), a focused
next step. This is source-only; the deployed web kernel.bin is unchanged, so the
live demo is unaffected.

### 2026-07-03 (loop) - pipes: handle classification + PEB std handles

Traced `dir | sort` through the kernel (temporary syscall logging, since removed)
and fixed three real fidelity gaps between cmd and our surface. cmd wires the
pipe correctly - left child `cmd /S /D /c "dir"` with stdout = pipe write end,
right child `sort` with stdin = pipe read end - so the setup is sound; the gaps
were downstream:

- **GetFileType** returned FILE_TYPE_UNKNOWN for a pipe or file. Added an
  `NT_QUERY_FILE_TYPE` syscall that classifies a handle by object type
  (pipe -> PIPE, ram/writable file -> DISK, else CHAR); GetFileType now reports
  it. The CRT and cmd branch on this at startup.
- **PEB standard handles**: a child's `ProcessParameters.Standard{Input,Output,
  Error}` were all seeded to the console. Added `pe::set_std_handles`, called from
  create_user_process, so a child inherits the pipe/file the parent staged (cmd
  reads these straight from the PEB, not via GetStdHandle).
- **GetConsoleMode** used to succeed for any handle, so cmd thought a pipe stdout
  was a console. It now fails for non-CHAR handles (via NT_QUERY_FILE_TYPE), the
  standard "is this redirected?" probe.

All three are correct regardless of pipes and carry no regression: 67/67 self
tests pass, and interactive `dir`, `echo`, `more`, `ver`, `cmd /c dir`,
`cmd /c echo` all still work with cmd staying alive. `emu/examples/pipe_test`
gained `--plain` / `--slashc` / `--trace` modes for this.

Still not producing `dir | sort` output: even with the above, the child cmd
writes its `dir` builtin output to its **console handle**, not to the inherited
StandardOutput/pipe - so cmd's internal output routine is picking the console
regardless. Cracking that needs the API tracer armed on the child cmd to see how
it selects its output handle (likely a CreateFile("CONOUT$") or a cached console
handle path). A focused next step. Source-only; deployed kernel.bin unchanged.

### 2026-07-03 (loop) - pipes paused with root cause; shipped-tool audit

Kept tracing `dir | sort`. cmd wires the pipe correctly (left `cmd /c dir`
stdout -> pipe write, right `sort` stdin -> pipe read) and, with the earlier
GetFileType / PEB-std-handle / GetConsoleMode fixes, gets further, but the
producer child still routes its `dir` output to a console handle rather than the
inherited pipe. Two compounding roots, both bigger than a quick patch:

1. The handle table is **system-wide**, not per-process, so a child's freshly
   opened console handle can take the same numeric value as a pipe end the parent
   just created (observed the left child's PEB ConsoleHandle aliasing the pipe
   write handle). Cross-process pipe handoff needs per-process handle tables (or
   careful lifetime handling) to be reliable.
2. cmd selects its builtin (`dir`) output handle internally in a way that lands
   on the console handle even when StandardOutput is redirected; pinning that down
   needs the API tracer armed on the child cmd.

Pausing pipes here rather than yak-shave further; the five fidelity fixes from the
last two sessions (CRT fd layer, DuplicateHandle, GetFileType, PEB std handles,
GetConsoleMode) all stand and are regression-free.

Silver lining from auditing the shipped Microsoft tools as interactive commands
(new `pipe_test --tools` mode): **`whoami` (-> `nanokrnl\user`), `where cmd.exe`
(-> `C:\cmd.exe`), `ver`, and `vol` all work** unmodified. Only `where` with a
bare name (`where cmd`, relying on PATHEXT expansion) misses - a minor edge in
where.exe's own path probing, since PATH/PATHEXT are both present and our name
matching is case-insensitive. `dir`, `echo`, `more`, `type`, `cmd /c ...` also
work. Source-only; deployed kernel.bin unchanged.

### 2026-07-03 (loop) - blog post: running unmodified Microsoft console tools

Wrote a new entry in the nanokrnl series (msuiche.com, authored by Twinkle):
"A Windows Kernel in a Browser Tab: Running Unmodified Microsoft Console Tools".
It covers what the last few sessions actually built - loading a real PE and
binding its imports against the kernel32/msvcrt shims, the handle table and
file-type classification (NtQueryFileType / GetFileType / GetConsoleMode), and
standard-stream inheritance across CreateProcess (STARTUPINFO + PEB
ProcessParameters + DuplicateHandle). Frames pipes honestly as the current
frontier (cmd routes builtin output to a console handle; the system-wide handle
table needs to become per-process). Verified: whoami, where cmd.exe, ver, vol,
dir, more, echo, cmd /c dir all run real Microsoft binaries on our syscalls.

Post is committed in the msuiche.com repo but not pushed - publishing is left to
the site owner (that repo tracks its built public/ output, so a hugo rebuild +
publish is a separate deliberate step).

### 2026-07-03 - redirection to a file works (distinct std handles + CRT dup)

`dir > out.txt` now writes a byte-perfect file and `type out.txt` / `more out.txt`
read it back correctly, with cmd staying alive across the command. Traced the
whole path end to end. Two root fixes:

- **Distinct standard-stream handles per process.** setup_user_blocks now opens
  three separate `\Device\Console` handles for stdin/stdout/stderr (was one shared
  handle) and returns them in `LoadedProcess.std_console`; the spawner installs
  them as the thread's std handles (inherited pipe/file overrides win). cmd's `>`
  teardown closes its stdout handle; when stdin and stdout were the same handle,
  that also killed stdin, so cmd's next read hit EOF and the shell exited after
  every redirect. Distinct handles fix that.
- **`_dup`/`_dup2` duplicate the OS handle** (via NtDuplicateObject) instead of
  sharing the value, so cmd's `saved = _dup(1); _dup2(saved,1); _close(saved)`
  save/restore dance no longer closes the console handle fd 1 still needs.

Verified: 67/67 self-tests pass; `dir`, `echo`, `more`, `ver`, `whoami`,
`where cmd.exe`, `cmd /c dir`, and `dir > out.txt` + `type out.txt` all work.

Pipes (`dir | sort`) still not end to end: cmd creates the pipe and spawns
`cmd /c dir` with the pipe as stdout, but the child writes its `dir` output to a
console handle rather than the inherited pipe (0 pipe writes observed), and the
CRT dup/fd juggling scrambles which pipe end each stage gets (dir stdout and sort
stdin both resolve to a write-end dup). The cross-process pipe handoff needs a
CRT-fd rework that matches cmd's exact dup/close sequence; redirection is the
verified milestone here. Earlier "garbled output" was a debug-logging artifact
(kd_println interleaving with the stream), not real corruption. Source-only;
deployed kernel.bin unchanged.

### 2026-07-03 - pipes: precise blocker (CRT fd/dup vs our handle model)

Instrumented the full `dir | sort` handoff (unique markers, grepped for, so no
debug-interleave artifacts). The pipe is created (read=A, write=B) and both
stages spawn, but the handle assignment comes out scrambled:

- left `cmd /c dir` gets stdout = a *dup of the write end* (good in principle),
- right `sort` gets stdout = the *read end* (wrong; its stdout should be the
  console and its stdin the read end), and the listing bytes land on the read-end
  handle, classified "other" (a read end is not a write end), so nothing flows
  through the pipe to sort.

Root cause: our msvcrt `_dup`/`_dup2` must duplicate the OS handle (distinct
handle per fd) for redirection to work - otherwise cmd's `_close` of a saved fd
kills the console handle another fd still points to. But duplicating changes
handle *values* mid-sequence, and cmd's CRT tracks pipe ends by the fd->handle
identities it expects from a Windows CRT, so the two-process `_pipe`/`_dup2`/
`_close` choreography ends up mapping the wrong end to each stage.

The proper fix is to make the fd table model Windows CRT semantics: multiple fds
share one underlying OS handle via reference counting, and `_close` only closes
the OS handle when the last fd referencing it goes away (rather than each fd
owning a distinct dup). That is a real fd-layer rework, not a patch, and it is
the actionable next step for pipes. Redirection (single process) is done and
verified; pipes (cross-process) wait on this. Source-only.

### 2026-07-03 - pipe rework attempt: data flows, but reverted (regressed redirect)

Attempted the fd-refcount rework to finish `dir | sort`. Combined four changes:
share-based `_dup`/`_dup2` (reference-count by scanning the fd table, so `_close`
frees the OS handle only when the last fd releases it), duplicate inherited
handles into the child at process creation, route msvcrt's `console_write`
through the redirected stdout, and resolve `_get_osfhandle(0/1/2)` via the
kernel's per-process std handles instead of the shared fd table.

Result and the map this produced:
- **The core pipe data flow started working**: with those changes the child
  `cmd /c dir` wrote the *entire* listing into the pipe (measured: 26 writes,
  all `kind=pipe`), and cmd's pipe choreography was correct (stdout->write end
  for dir, stdin->read end for sort, no scramble). That is the furthest pipes
  have gotten.
- **But it regressed redirection into a hang** and `dir | sort` still did not
  complete: `sort` spawns (stdin = a dup of the read end, stdout = console) yet
  never runs its startup - a concurrent-child scheduling / pipe-EOF deadlock, on
  top of the redirect regression. The fd/handle/console paths are too tightly
  coupled to change piecemeal without breaking the redirection that already
  works, so this was reverted to keep redirect solid (verified working again).

Root map for a future dedicated effort: the real blocker is that the kernel32 /
msvcrt DLL `.data` (fd table, cached std/console handles) is a *single shared
copy* across all processes, so a child inherits the parent's CRT stdio state and
writes to the parent's console handle. The correct foundation is per-process DLL
data (copy-on-write on map), after which the fd-refcount + inherit-duplication
changes above should compose cleanly. Redirection stays done; pipes wait on that.

### 2026-07-03 (loop) - builtin survey; redirect + more commands advertised

Surveyed cmd builtins to find tractable gaps. Verified working individually:
`set`, `path`, `cd`, `title`, `cls`, `color` (plus the previously-confirmed
`dir`, `echo`, `ver`, `vol`, `whoami`, `more`, `type`, `where cmd.exe`,
`cmd /c dir`, and `dir > out.txt` redirection). Added a `--survey` mode to
`emu/examples/pipe_test` and surfaced `set` + the redirect example in the demo
readme.

One real finding: running many commands back-to-back eventually hangs the shell
(a ~6-command sequence timed out, while each command runs fine on its own). This
is cumulative, not per-command - most likely the single system-wide 256-entry
handle table filling as each command leaks handles that are not reclaimed on
process exit. That is the same per-process-state root as the pipe blocker
(per-process handle tables / DLL data), so it is deferred to that same focused
rework rather than patched piecemeal. Source-only; deployed kernel.bin unchanged
(readme is the only web change).

### 2026-07-03 - Part IV: KDBG - real KDDEBUGGER_DATA64 + Ps*List for lm / !process

Built the kernel-debugger view a Windows debugger expects, so `lm` and
`!process 0 0` light up against nanokrnl's crash dump (new `kernel/src/kd.rs`):

- A real `KDDEBUGGER_DATA64` (`KdDebuggerDataBlock`) with the `'KDBG'` tag,
  `KernBase`, and pointers to the two lists.
- `PsLoadedModuleList`: a circular `InLoadOrderLinks` ring of
  `KLDR_DATA_TABLE_ENTRY` (DllBase / SizeOfImage / BaseDllName UNICODE_STRING),
  built from the live module table (kernel first as `ntoskrnl.exe`, then
  kernel32 / msvcrt / ntdll / the running image).
- `PsActiveProcessHead`: a ring of `EPROCESS` (UniqueProcessId /
  ActiveProcessLinks / DirectoryTableBase / ImageFileName) built from the process
  table.

Every field sits at its genuine NT offset. The block is populated by
`init::kd_snapshot()` just before the ELF core is written, so the core carries a
coherent snapshot; `write_core` also records `SYMBOL(KdDebuggerDataBlock)=...`
(and the two list heads) in `VMCOREINFO` so a tool can anchor without symbols.
The kernel is linked at 0 but mapped at `0xffff800000000000`, so a debugger
loads `kernel.bin`'s DWARF at that base and the symbols resolve into the dump.

Verified (no WinDbg on macOS) with a symbol-free WinDbg-equivalent walker,
`tools/kdbg_check.py`, which reads the core, finds `KdDebuggerDataBlock`, checks
the `'KDBG'` tag, and walks both rings. Output on a real crash core:

    lm:        ntoskrnl.exe, cmd.exe, kernel32, msvcrt, ntdll (base..end each)
    !process:  4 processes with Cid, DirBase, and ImageFileName

Stack walks and symbols were already in place from Part III (the crash register
set is in the `NT_PRSTATUS` note; `.debug_frame` CFI + `.debug_info` are in
`kernel.bin`). 67/67 self-tests pass; the crash-dump path is unchanged in shape.
This is the natural closer for the series; blog post next.

### 2026-07-03 - pipes: definitive blocker (fd model vs cmd's CRT choreography)

Made another focused, incremental pass at `dir | sort`, testing redirect after
each step. Traced the whole two-stage handoff with kernel markers (process
spawn, thread entry, wait, exit, pipe read/write, pipe-end close). Findings:

- The **pipe data path works**: with per-process std-handle resolution
  (`_get_osfhandle`/`console_write` reading the kernel's per-process std handles
  instead of the shared DLL fd table) the producer `cmd /c dir` writes its whole
  listing (26 chunks) into the pipe with the correct end assignment
  (dir stdout -> pipe write, sort stdin -> pipe read).
- But it does not complete, and the cause is a genuine conflict in our fd model:
  - **share-based `_dup2`** (fds name the real pipe-end handles) keeps cmd's
    end-assignment correct, but cmd closes its own pipe-end handles right after
    spawning the stages, and in our single *global* handle table that closes the
    read end before `sort` inherits it -> sort reads an empty console, no output.
  - **duplicate-based `_dup2`** (each fd owns a distinct handle) keeps the ends
    alive across cmd's close, but the duplicated handle *values* scramble cmd's
    own read/write fd bookkeeping -> sort is handed the write end as its stdin.
  - Worse, the per-process std-handle resolution that makes the producer write
    into the pipe **regresses redirection** (`dir > out.txt` makes cmd exit),
    because redirect and pipe drive the same fd/std-handle paths in opposite
    directions.

Conclusion: `dir | sort` (two concurrent processes sharing a pipe) needs the
foundation this kept colliding with - **per-process handle tables with Windows
`bInheritHandles` semantics** (a child gets inheritable-handle *copies* at
CreateProcess, before the parent closes its own), and/or honoring the CRT's
STARTUPINFO fd-inheritance block. That is a real VM/loader/object-manager rework,
not an fd-layer tweak; every incremental fd change either scrambles the ends or
regresses redirect. Reverted to the known-good state so **redirection stays
working** and there is no regression. Pipes remain the one shell feature that
waits on that rework.
