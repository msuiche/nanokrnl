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

### 2026-07-17 - CI: the kernel now has a gate (host tests + QEMU boot self-tests)

CI previously guarded only nanox, on the premise (in the workflow comment) that
the kernel "cannot be built from a fresh clone" because its image embeds the
gitignored Microsoft binaries in winbin/. That premise was stale: build.rs has
always resolved every embedded PE through its empty-placeholder fallback, so a
clean checkout builds fine - only the winbin/ binaries and drivers/null.sys are
non-redistributable, and their tests report SKIP rather than FAIL.

Proved it before wiring it up: `git clone` into a scratch dir, built all seven
in-tree PEs from source with the repo's own scripts (driver, kernel32, msvcrt,
userapp, userapp2, worker, crash - nightly, x86_64-pc-windows-msvc, lld-link,
llvm-dlltool), then `scripts/qemu-test.sh`: full boot, ALL SELF TESTS PASSED,
exit 33, including the PE-driver load/IOCTL/unload cycle and the multi-process
ring-3 tests. Host side: `cargo test -p kernel` green (7 ABI-conformance +
2 allocator proptests).

`.github/workflows/ci.yml` gains a `kernel` job next to `nanox`:

- stable + x86_64-unknown-none for the kernel proper; nightly with rust-src /
  llvm-tools / x86_64-pc-windows-msvc for the boot-image builder and PE builds.
- `apt install qemu-system-x86 lld llvm`; Ubuntu hides llvm-dlltool under
  /usr/lib/llvm-*/bin, so the job symlinks it onto PATH (the scripts' default
  DLLTOOL path is Homebrew-only; the job also exports DLLTOOL=llvm-dlltool).
- Steps: `cargo test -p kernel` -> build the seven PEs -> `sh
  scripts/qemu-test.sh` as the pass/fail gate, with rust-cache for speed.

Verified locally end to end on macOS (fresh clone, exact script sequence);
the one thing only a real runner can exercise is the Ubuntu apt layer, which
the first CI run will confirm. The emulator job and the local-only Unicorn
differential gate are unchanged.

### 2026-07-17 - VADs + demand paging: the heap goes per-process, and two latent bugs surface

Phase A of the memory-manager roadmap: per-address-space VADs and a
fault-driven commit path, replacing the shared physical-window heap.

**What changed.** New `kernel/src/mm/vad.rs`: a sorted, non-overlapping list
of committed ranges per address space (keyed by PML4, since threads carry
`cr3` and there is no EPROCESS yet). `NtAllocateVirtualMemory` now just
inserts a descriptor in a low-half arena (`0x0000_7000_0000_0000`+); no page
is mapped until first touch. The #PF handler (`ke/traps.rs`) resolves a
*not-present* fault at PASSIVE_LEVEL against the current space's VADs — a
hit gets a zeroed frame mapped with the VAD's protection and the CPU retries;
anything else keeps its old fate (thread kill from ring 3, bugcheck from
ring 0). Protection faults (P bit set) are deliberately unresolvable, which
is how a write to a read-only VAD page stays an access violation.
`NtFreeVirtualMemory` unmaps/frees backed pages and splits/shrinks VADs;
`NtProtectVirtualMemory` is real now (VAD split + per-page RW/NX rewrite)
instead of a permissive no-op; the heap is NX (`PAGE_READWRITE`) instead of
the old documented RWX coarsening. `ProbeForRead/Write` keep their PTE walk
but take a VAD second chance, so a committed-but-unbacked buffer is valid
(the access faults it in) while anything outside every VAD is still
`STATUS_ACCESS_VIOLATION`. Process exit now reclaims the whole address
space: `on_user_thread_exit` switches to the kernel AS, drops the VADs, and
`mm_free_user_address_space` walks the low half freeing every leaf frame,
the tables, and the PML4 (previously all leaked).

**Two latent bugs this exposed** (both invisible while heap VAs were
window-backed and valid in every address space):

1. *Self-test processes shared the CRT heap arena.* `alloc_shim_data` (the
   per-process copy of kernel32/msvcrt `.data`) was only called on the
   `create_user_process` path; the boot harness's direct
   `load_user_process`+spawn sites (isolated userapp, worker pair, sort,
   choice, where, cmd) shared the kernel's arena. A stale `BUMP` then handed
   a process a chunk VA its own AS never backed: user #PF, thread killed,
   and the kernel32 `HEAP_LOCK` it died holding wedged every later heap user
   (watchdog timeout). Fixed by moving `alloc_shim_data` into
   `load_user_process` itself (idempotent per cr3), so *every* process gets
   private CRT data.
2. *`Kthread.cr3` was only set at first run* (in `user_thread_entry`), so
   the scheduler's first switch into a process thread saw `cr3 == 0` and
   skipped the shim-data swap — the process booted on the stale shared arena
   even with a slot allocated. Debug prints showed zero `swap` lines for the
   faulting process while the preempted worker pair swapped fine. Fixed by
   setting `tcb.cr3` in `spawn_process_thread` (race-free: the child can't
   run until the spawner blocks).

Diagnosis was print-driven: temporary `VAD:`/`SHIM:` kd prints showed the
faulting process's VAD space never existed and its arena swap never ran,
respectively; prints removed after the fix.

**Verified.** `cargo test -p kernel`: 18 unit + 2 proptest + 7 ABI
conformance, all green. Boot self-tests: 67 -> **76 checks** (9 new: VAD
alloc-starts-unmapped, demand fault on first touch incl. read-back, probe of
an unbacked committed page, protect-to-read-only PTE bit, free unmaps and
returns the frame, probe rejects the freed range, VAD list empty), and the
whole ring-3 suite (userapp/userapp2/worker pair/CreateProcess/sort/choice/
where/cmd) now runs on per-process demand-paged heaps. `ALL SELF TESTS
PASSED`, exit 33.

### 2026-07-17 - SEH groundwork: exceptions reach user mode (VEH + NtContinue)

First slice of structured exception handling: user-mode exceptions are now
*delivered* to ring 3 instead of terminating the thread outright.

**Kernel side.** New `ke/exception.rs`: on a user exception (any vector the
demand-commit path can't resolve), `dispatch_exception` builds an
`EXCEPTION_RECORD` and a `CONTEXT` (real winnt.h offsets, 0x4D0 bytes,
integer+control+segments; FP state stays zeroed, documented) on the user
stack and rewrites the trap frame so the normal epilogue "returns" to a
`KiUserExceptionDispatcher` thunk in the ntdll stub page
(`rcx` = record, `rdx` = context — the real entry contract). The thunk
(`mov rax, imm; jmp rax`, patched when kernel32 loads) lands in the shim.
`NtContinue` (new svc 42) validates a context and resumes it with a full
15-GPR restore: a naked `ki_continue_asm` resets the kernel stack and
`iretq`s, which also bounds the stack across dispatch/continue round trips.
Segments and RFLAGS are forced, not trusted; RIP/RSP must probe
user-executable/user-writable. If delivery itself is impossible (e.g. the
stack is the faulting page), the old terminate path runs — no regression.

**Shim side (kernel32).** `AddVectoredExceptionHandler` /
`RemoveVectoredExceptionHandler` over a sparse, sequence-numbered list
(most-recent-first dispatch), and `KiUserExceptionDispatcher`: a handled
exception (`EXCEPTION_CONTINUE_EXECUTION`) resumes via `NtContinue`, an
unhandled one terminates the thread with the exception code — the exact
fate unhandled faults always had here.

**Test.** userapp registers a handler, reads `[0]` (deliberate AV), the
handler records the code (`0xC0000005`) and redirects `Rip` to a recovery
label; success folds into the existing `ReportTestResult(0xABCD)` gate, so
the boot suite fails if any link in the chain breaks.

**Bugs caught by the suite.** The CONTEXT offsets were first written 8
bytes off from winnt.h (the six Dr registers sit at 0x48..0x70, pushing
Rax to 0x78 and Rip to 0xF8), and `EXCEPTION_RECORD` was sized 0x50
instead of 0x98 — both fixed before they bit, worth a conformance test
later. The real failure was subtler: validation assumed user addresses are
low-half, but kernel-AS user threads run on high-half window-backed stacks
— the kernel-AS userapp run died at dispatch while the isolated one worked.
Validation now goes through the probes (U/S bit), which are half-agnostic.

**Verified.** Boot suite 76/76, exit 33, with the AV recovery printed in
BOTH address-space models (kernel-AS and isolated process runs); host
tests 18 unit + 2 proptest + 7 conformance; emu suite 36/36. Frame-based
SEH (.pdata unwind, `__C_specific_handler`) now has its delivery
foundation; per the roadmap, that or async IRP completion/APCs is next.

### 2026-07-17 - `dir | sort` works: the pipe blocker, closed end to end

The CFP's "still-open cmd.exe pipe blocker" is fixed. Five distinct bugs
stood between the prompt and a working pipe; each was found by tracing, not
guessing (nanox syscall tracer upgraded to log all four args + a sysret-time
CreatePipe handle dump, and temporary kd prints in the msvcrt fd layer, the
CreateProcess inheritance path, and the pipe writer count — all removed
after).

**1. nanox delivered a hardcoded page-fault error code** (`P`, always).
`xlate` computed the real P/WR/US/ID bits in `mmu::translate` and threw them
away. Cosmetic until VADs made the code load-bearing (`traps.rs` resolves
only not-present faults): every demand fault bugchecked. Now `Cpu::pf_code`
carries the real code to `deliver_interrupt`. Also added `invlpg` (0F 01 /7)
to the decoder — the kernel's VAD work uses it; nanox has no TLB, so it's a
documented no-op.

**2. Handle values weren't stable identities (the actual pipe blocker).**
Every `DuplicateHandle` allocated a *fresh* value, so a value cmd or the CRT
held went stale the moment a freed slot recycled — cmd moved its pipe read
end via Win32 DuplicateHandle+CloseHandle, the CRT's fd still named the old
value, and `_dup2(rfd, 0)` aliased the *console* into sort's stdin (proven
by instrumenting the CRT's fd table: `FD dup2 src 3 10` with 0x10 recycled).
Fix: `DuplicateHandle` is share semantics — same value, per-entry refcount;
distinct opens still get distinct values. Handle values are now stable
object identities, which is the invariant cmd's choreography assumes.

**3. Dup/close reference imbalance.** Each dup took an object reference but
closes only decremented the entry count, leaking one object reference per
dup copy. `ob_close_handle` now dereferences per close.

**4. EOF never fired.** The pipe writer count tracked nothing (create-time
only), and `pipe::create`'s initial object reference leaked — a phantom
writer forever, so sort's second read blocked forever. `nt_create_pipe`
drops the create-time refs; new `ObjectType::on_reference/on_dereference`
hooks make the writer count mirror live write-end references exactly
(dups, cross-process inheritance, process exit).

**5. User-buffer copies under DISPATCH_LEVEL spinlocks.** `pipe::try_read`/
`write`, `ramfs::write`/`read_writable`, and `console::drain_input` all
copied user memory while holding a DISPATCH_LEVEL lock — a demand fault on
a fresh VAD page can only resolve at PASSIVE, so any fresh buffer bugchecked
the machine (sort's 45 MB input buffer hit it instantly). All now stage
through a kernel bounce buffer and copy to/from user space unlocked. Also
removed the VirtualAlloc 16 MiB ceiling: with demand commit a 45 MB
reservation costs a VAD until touched (sort sizes its buffer from physical
RAM and treated the rejection as fatal -> "Unknown error").

**Verified** (nanox, interactive kernel): `dir | sort` prints the sorted
listing and returns to the prompt (writers reach 0, EOF fires, sort's 45 MB
buffer demand-faults in); `dir > out.txt` + `type out.txt` work standalone
*and* right after a pipe in the same session (the old corruption is gone);
`--plain` clean. QEMU boot suite 76/76 (exit 33); host tests 18+2+7; emu
suite 36/36. Remaining known issue, unrelated to pipes: `more.com` against
writable (redirect-created) files errors out.

### 2026-07-17 - the `more.com` investigation: two fixes, one documented mystery

Follow-up to the pipe work: `more out.txt` (a redirect-created file) printed
"Unknown error" instead of paging. The hunt went through a genuine red
herring first: the QEMU suite had overwritten `target/debug/kernel` with the
DEFAULT (canned-input) kernel while the nanox pipe harness boots that exact
path expecting the INTERACTIVE build — several confusing logs ("More? " for
everything) were the wrong kernel entirely. Worth remembering: any
`qemu-test.sh` run clobbers the interactive binary; rebuild with
`--features interactive` before harness runs.

Then the real chain, all trace-driven:

1. **`NtOpenFile` never found writable files.** The "real ntdll" open path
   (ulib tools) consulted only the const ramfs table; `NtCreateFile` already
   had the writable-first lookup. Fixed to match: redirect-created files now
   open through both services.
2. **The MUI loader only consulted the side-by-side `.mui` registry** — a
   module's OWN inline resources were unreadable. Added image-resource
   variants (`load_string_in_image` / `load_message_in_image`, direct-RVA
   mode) with a `module_image` fallback in both load services, SMAP-bracketed.
3. **The remaining "Unknown error" is understood, not fixed.** more.com
   opens, sizes, reads, and displays `out.txt` correctly (verified line by
   line in the trace — the file content was never the problem). At the tail
   it fails a lookup, calls ulib's error printer, and every message load
   (0x4e21-0x4e55) fails: the strings live in `ulib.dll.mui`, which we don't
   ship, and ulib.dll carries NO inline message table (checked locally: only
   a version resource). ulib falls back to its generic "Unknown error". Also
   ruled out along the way: console double-feed (instrumented `drain_input`:
   each command line is consumed exactly once), HKCU seeding (fine), and
   cmd-internal-more confusion (the "More? " prompt IS more.com's own).
   Full fix = authoring `ulib.dll.mui`/`more.com.mui` resource PEs (or
   finding what the trailing 10-char QUERY_DIRECTORY looks for) — logged as
   a known issue.

Verified: QEMU suite 76/76 (exit 33), host 18+2+7, emu 36/36, and
`dir | sort` still produces the sorted listing at the prompt.

### 2026-07-17 - `more out.txt` closes out: writable files are first-class

The trailing "Unknown error" from the previous entry is fixed — but NOT via
the error strings (that was the wrong branch of the diagnosis; the inline
-resource fallback stays, it's just not what more.com needed). The real
chain, found by dumping the QUERY_DIRECTORY patterns in the tracer:

1. **Glob expansion never saw writable files.** `ramfs::find()` and
   `ramfs::attributes()` consulted only the const `FILES` table, so a tool
   that stats or glob-expands its argument (ulib's file iterator, any
   `GetFileAttributesW`) couldn't see a redirect-created file — open it
   directly and it worked, enumerate for it and it didn't exist. Both now
   include the writable overlay (drive-root tolerant).
2. **`CreateFile("C:\out.txt", OPEN_EXISTING)` missed writable files** when
   the path was drive-qualified: creators key bare names ("out.txt") but
   `norm_key` kept the drive prefix on lookups ("c:\out.txt"), so the stat
   succeeded and the open failed one call later. `norm_key` now drops NT
   and drive prefixes at normalization, unifying bare and qualified forms
   everywhere (create, open, size, enumerate).
3. **`CloseHandle((HANDLE)-1)` was INVALID_HANDLE**, and ulib closes the
   `GetCurrentProcess()` pseudo-handle on its exit path — a documented
   no-op on real Windows that was fatal here. `NtClose` now succeeds
   silently for the -1/-2 pseudo-handles.

Result: `more out.txt` opens, reads, pages, and exits clean; `type`/`dir`
enumerate redirect-created files (charmingly, `dir > out.txt` lists
`out.txt` itself at its mid-write size, like a real fs). The remaining
gap: ulib/more message *strings* still can't render (no `ulib.dll.mui` in
the image set), so a real error path still falls back to "Unknown error"
text — but there is no error path left on this flow.

Verified: QEMU suite 76/76 (exit 33); host 18+2+7; emu 36/36; interactive:
`dir`, `dir | sort` (sorted), `dir > out.txt`, `type out.txt`,
`more out.txt` all clean end to end.

### 2026-07-17 - user APCs + alertable waits

The user-facing slice of NT's async model, done as a cleanly layered pair:

**Kernel.** `KTHREAD` grows a small FIFO of pending user APCs
(routine+argument pairs, 8 max). Three services: `QueueUserAPC` (target =
the GetCurrentThread pseudo-handle or a CreateProcess handle's main
thread), `NextUserAPC` (pop the caller's oldest pending pair into a user
buffer), and `NtDelayExecution` gains the alertable flag — a direct ntdll
caller with a pending APC gets `STATUS_USER_APC` (WAIT_IO_COMPLETION, 0xC0)
immediately instead of sleeping past it.

**Shim (kernel32).** `QueueUserAPC` forwards; `SleepEx(ms, alertable)`
drains the queue through `NextUserAPC` and invokes each routine in user
mode, returning `WAIT_IO_COMPLETION` if at least one ran. Documented
divergence: NT delivers via `KiUserApcDispatcher` on the kernel's return
path — our syscall/sysret path has no trap frame to rewrite, so delivery is
layered into the CRT shim; the observable semantics (callback runs in the
target thread's context during the alertable call) are the same.

**Test.** userapp queues an APC to itself: alertable `SleepEx` runs it and
reports 0xC0 (hits/arg verified), a second `SleepEx` reports the queue is
drained, plain `Sleep` doesn't fire, and the next alertable call does.
Folded into the `ReportTestResult(0xABCD)` gate, so it runs in BOTH the
kernel-AS and the isolated-process userapp boots.

Verified: QEMU suite 76/76 (exit 33, both APC prints present); host
18+2+7; emu 36/36. Next async-model items when wanted: kernel APCs
(`KeInsertQueueApc`), IRP completion-routine chaining, and
`WaitForSingleObjectEx` alertability.

### 2026-07-17 - async I/O, continued: IRP completion routines + alertable object waits

Two more rungs of the async-model ladder, both verified:

**IRP completion routines.** `IoCompleteRequest` now walks the IRP's stack
locations bottom-to-top and invokes any recorded completion routine
(`(device, irp, context) -> NTSTATUS`, Microsoft x64 ABI, at the real
0x38/0x40 stack-location offsets). The `IoSetCompletionRoutine` export —
previously storage-only ("invocation path is future work") — is now live.
Boot self-test (77 checks now): record a completion routine on a read IRP
to the RustDemo device, watch it fire when the driver completes the IRP.

**Alertable object waits.** `WaitForSingleObjectEx` honors `bAlertable` the
same way `SleepEx` does: the shim drains pending user APCs first and
returns `WAIT_IO_COMPLETION` if any ran (the shim previously ignored the
flag, with a stale "APCs we don't deliver" comment). The kernel-side
`NtWaitProcess` mirrors it for direct ntdll callers (immediate
`STATUS_USER_APC` when an APC is pending). userapp test: queue an APC,
alertable wait on the already-exited child returns 0xC0 and runs the APC;
the non-alertable call then reports the child normally — folded into the
0xABCD gate in both address-space boots.

Verified: QEMU suite 77/77 (exit 33, completion-routine check + both
WaitEx prints); host 18+2+7; emu 36/36. Remaining async items: kernel APCs
(`KeInsertQueueApc`) and alertability inside long kernel waits proper.

### 2026-07-17 - kernel APCs (KeInitializeApc / KeInsertQueueApc / delivery)

The last rung of the async-model ladder. New `ke/apc.rs`: a `KAPC` object
(queue entry, kernel routine, normal routine + context/args, target thread),
`KeInitializeApc` (NT's shape, collapsed to normal kernel APCs),
`KeInsertQueueApc` (dispatcher-lock-guarded insert, double-insert refused),
and `ki_deliver_apcs`, which drains the current thread's pending queue at
`APC_LEVEL` — kernel routine first, then `normal(context, arg1, arg2)`.

Delivery was the instructive part (two boot failures earned it):

1. **IRQL direction.** `ki_dispatch_interrupt` enters with CR8 wherever the
   interrupted code left it (PASSIVE in practice), so a hard-coded
   `lower_irql(APC_LEVEL)` tripped the strict direction asserts — delivery
   now raises or lowers to APC_LEVEL from either side and restores.
2. **Delivery point.** The first hook (dispatch-interrupt epilogue) never
   fired for a *woken* thread: a wait wake resumes the thread from its own
   wait frame (`ki_wait_for_objects`'s switch), not through the epilogue —
   the dispatch interrupt that readied it belonged to the idle thread. A
   second delivery hook after `release(old)` in `ki_wait_for_objects`
   covers the resume path, which is NT's "return toward PASSIVE in thread
   context" delivery semantics. (Also learned: `ListEntry` heads need
   `init()` — the `KTHREAD` queue field is lazily self-linked on first
   touch.)

Boot self-test (82 checks now): queue to self, double-insert refused, no
inline delivery, delivered at APC_LEVEL with context + arguments intact.

Verified: QEMU suite 82/82 (exit 33); host 18+2+7; emu 36/36. The APC
story is now complete at both layers: kernel APCs for in-kernel consumers
(suspension/context machinery can build on it), user APCs with alertable
waits for ring 3. A driver-facing export + `ntabi` Kapc type is the small
follow-up when a driver actually needs it.

### 2026-07-17 - registry hive persistence, part I: real hives load

CM now mounts a real Windows registry hive from file bytes at boot.

**Dynamic cm first.** The registry store was fixed-cap arrays (64 keys /
128 values / 48-unit names / 128-byte data) — a real hive would never fit.
Reworked `cm/mod.rs` to index-stable Vec-backed storage (`Option` slots;
handles are indices, so nothing moves) with the public API byte-identical:
`open_key`/`create_key`/`query_value`/`set_value`/`enum_key` and the
predefined-root/handle scheme unchanged.

**The parser** (`cm/hive.rs`): the `regf` format from base block to grafted
tree — base block signature/sizes, `4096 + index` cell addressing with
allocated-size and span checks, `nk` nodes (compressed-name flag, subkey
lists `lf`/`lh`/`li`/`ri`, value lists), `vk` values (inline ≤ 4-byte data,
data-cell data, named/default). Everything is bounds-checked and budgeted
(recursion cap, cell budget): a corrupt or hostile hive degrades to an
error, never a bad pointer. The tree grafts under `HKLM\SYSTEM`.

**The test hive** (`tools/gen_hive.py`): a small structurally-real hive
written by the repo — base block with checksum, one hbin, `SYSTEM →
ControlSet001 → Control → HiveTest` with `Signature` (DWORD, inline) and
`Greeting` (REG_SZ, data cell). Embedded via build.rs as `C:\system.hive`
(readable for inspection like any file), loaded by `cm::init` at boot, and
verified through the normal registry API in the boot suite (86 checks now):
key opens, DWORD round-trip, REG_SZ round-trip, subkey enumeration.

Verified: QEMU suite 86/86 (exit 33); host 18+2+7; emu 36/36. Part II is
the write half: serialize cm's model back to a valid `regf` file and
round-trip it (the kernel writes a hive Windows' own tools would open).

### 2026-07-17 - registry hive persistence, part II: the kernel writes hives

The write half, closing the loop: `cm::hive::save()` serializes any subtree
of the live registry back to a valid `regf` file, and the round-trip is
proven in the boot suite.

The serializer walks a subtree pre-order (parents before children, as cell
indices require) and emits the same format the parser accepts: base block
with XOR checksum, one page-rounded hbin, and cells for every key and
value — `nk` records (compressed ASCII names, root flagged 0x2C), `lh`
subkey lists with proper uppercase name hints, value lists, `vk` values
(≤ 4-byte data inline with the high bit, longer data in their own cells).
`cm::save_hive(path)` is the public surface (`RegSaveFile` in spirit).

Boot suite (89 checks now): serialize `HKLM\SYSTEM` → assert `regf` magic
→ re-load the bytes under a fresh graft (`SYSTEM2`) → query both values
through the normal registry API and require exact equality with the
originals (`Signature` = 0xC0FFEE, `Greeting` = "hello hive"). So both
directions are proven against each other, and the read half was already
proven against the Python-generated reference hive — the validation loop
is closed from both ends.

Verified: QEMU suite 89/89 (exit 33); host 18+2+7; emu 36/36. Registry
persistence is now real: load any hive, change it, write it back. A
`RegSaveFile`-style syscall (or a periodic flush of a system hive on the
host 9P drive) is a small follow-up whenever persistence-at-runtime is
wanted; the format machinery is done.

### 2026-07-17 - registry persistence is real: hives survive reboots over 9P

The loop is closed: `HKLM\SYSTEM` now lives on the host drive, and state
written by one boot is read by the next.

- **cm mounts host-first**: `cm::init` probes `H:\system.hive` (9P) before
  falling back to the embedded seed hive. A live 9P server means the
  registry has history; plain QEMU fails the probe fast (garbage port
  reads) and stays RAM-only.
- **BootCount + flush**: every boot bumps
  `HKLM\SYSTEM\PersistTest\BootCount` and streams the serialized hive back
  to the host (`cm::flush_to_host`, via the existing 9P create/write
  path). A `CM: boot #N from the persisted hive` banner prints whenever
  N > 1.
- **Suite checks (90 now)**: the counter exists and is ≥ 1, and — when a
  server is live — the flushed `H:\system.hive` parses back to the same
  content through 9P (skips cleanly otherwise).
- **The proof** (`emu/examples/p9_persist.rs`): three boots against one
  in-process 9P server (extended with `Tlcreate`/`Twrite` and the
  zero-name root-clone walk the kernel's `create()` needs). Boot 1 flushes
  a `regf`-valid hive; boots 2 and 3 print `boot #2` / `boot #3` from the
  persisted hive. PASS: BootCount 1 → 2 → 3 across reboots.

Verified: QEMU suite 90/90 (exit 33); host 18+2+7; emu 36/36;
`p9_persist` three-boot PASS. Registry work is done end to end: parse,
serialize, persist, survive reboots.

### 2026-07-17 - storage I: a real virtio-blk block device

The storage stack's foundation is in: PCI enumeration, the legacy
(transitional) virtio-blk interface, and synchronous sector read/write,
proven against a scratch disk on every boot.

- `hal/pci.rs`: `CF8`/`CFC` config-space access, bus scan, BAR r/w,
  bus-master + I/O enable.
- `io/virtblk.rs`: probe `1AF4:1001`, reset (with the asynchronous
  settle wait), legacy vring (desc/avail/used laid out at the
  device-reported queue depth, volatile ring access, DMA via physical
  addresses), 3-descriptor requests (header/data/status) with bounded
  completion poll.
- Wiring: `scripts/gen-blkimg.sh` stamps a 1 MiB scratch disk
  (NANOBLK1 marker + 0x55AA), the boot crate attaches it as
  `virtio-blk-pci,drive=scratch`, and the boot suite (93 checks now)
  reads sector 0 and round-trips a write to sector 2. nanox has no PCI
  emulation, so the test skips cleanly there (unknown ports read as
  all-ones, exactly like real hardware).

The bug that cost the afternoon deserves recording honestly: after every
layout detail was re-verified correct (queue PFN, ring offsets at the
device's own queue depth of 256, descriptor bytes dumped and decoded,
avail.idx advancing), the device still never completed a request. QEMU's
`-trace virtio_queue_notify,virtio_blk_req_complete` answered it in one
line: the notifies WERE arriving, followed by `virtio-blk missing
headers` — descriptor 0 was written with `flags=0, next=1`, so the chain
stopped at the header. `NEXT` is a flag, not an implication; one word
fixed it. Lesson kept: when the guest side is provably right, trace the
host side — QEMU's trace points are as good as a kernel debugger on the
other end of the DMA bus.

Verified: QEMU suite 93/93 (exit 33); host 18+2+7; emu 36/36; nanox
skips cleanly. Next storage rungs: a FAT32 reader on top of virtblk,
then write support and a pagefile — real paging-out, which is what makes
the whole memory manager honest.

### 2026-07-17 - storage II: a real FAT32 drive (D:\)

The kernel now mounts a FAT32 filesystem over the virtio-blk device and
serves files from it through the normal Win32 path.

- `tools/gen_fat32.py` builds a valid 16 MiB superfloppy (BPB with the
  NANOBLK1 OEM marker + 0x55AA, two FATs, root `HELLO.TXT` + `README.TXT`,
  and `SUB\NESTED.TXT` nested) — the same image the vblk test reads, so
  one disk exercises both layers.
- `io/fat.rs`: BPB parse + sanity, FAT chains with a one-sector cache,
  8.3 directory walk (LFN/volume/deleted entries skipped), nested-path
  lookup, whole-file read, directory + glob enumeration, attributes. The
  geometry is copied out of its lock once (static after mount) — the
  first version held it across `lookup()` and deadlocked the suite at the
  subdirectory test, the classic own-lock-reentry bug.
- `D:\` is wired into the same syscalls as `H:\`: `NtCreateFile` /
  `NtOpenFile` read whole files over the block layer into in-memory file
  objects; `NtQueryDirectory` and `GetFileAttributesW` enumerate and
  classify (`*.TXT` globs, `SUB\` subdirectories, proper
  NORMAL/DIRECTORY/not-found results).

Boot suite (**100 checks**): BPB recognized, read `HELLO.TXT`, read
`SUB\NESTED.TXT`, root and subdirectory enumeration, attributes, glob —
all through the same syscalls a Win32 app uses.

Verified: QEMU suite 100/100 (exit 33); host 18+2+7; emu 36/36. Next
storage rungs: FAT32 write support, then the pagefile — real paging-out,
the reason this stack exists.

### 2026-07-17 - storage III: FAT32 write support

`D:\` is now writable end to end: create, append, close, and the data is
real FAT — allocation, chains, and directory entries all update on disk.

- **Write primitives**: `alloc_cluster` (scan-forward from a hint, mark
  EOC, zero), `free_chain`, `set_fat_entry`, cluster writes, and
  `dir_add` (replace-or-claim a slot). `create_file(path, data)` does a
  whole-file write: free the old chain, allocate/fill a new one, update
  the dirent.
- **Write-back-on-close objects** (`FatWritable`): an open writable FAT
  file is an in-memory buffer (`NtCreateFile` with CREATE_NEW/
  CREATE_ALWAYS makes an empty one; OPEN_ALWAYS seeds it with the
  existing content for append semantics); the object's delete procedure
  flushes the buffer through `create_file` when the last handle closes.
  `NtWriteFile`/`NtReadFile`/`GetFileSize` operate on the buffer in
  between.
- **The create-time reference must be dropped**, exactly like the pipe
  ends: `NtCreateFile` (and the test) dereference the object right after
  making the handle, or the flush never fires (refs bottom out at 1
  forever). Second time that pattern has bitten; it now has a comment
  everywhere it applies.
- **One shared FAT sector window** for reads and writes: the flush
  allocated clusters through `set_fat_entry` while `fat_entry` kept a
  *separate* cache, so the read-back followed a stale "free" entry and
  the chain came up empty. One window, invalidated on write, fixed it.

Boot suite (103 checks): create + write + close flushes to the FAT
(content byte-exact), the written file enumerates with its size, and
recreating truncates to zero. Also fixed along the way: `read()` on an
empty (cluster-0) file no longer underflows the cluster math.

Verified: QEMU suite 103/103 (exit 33); host 18+2+7; emu 36/36. The
filesystem story is complete for the demo surface; the remaining storage
prize is the pagefile — real paging-out on top of this stack.

### 2026-07-18 - the pagefile: real paging-out, both directions

The memory manager now pages. User anonymous memory is demand-committed,
evicted to a pagefile under pressure (or on demand), and paged back in on
the next touch — content intact, through the real block device.

- **`mm::pageout`** (new): a working-set FIFO registry of every
  demand-committed page `(cr3, va)` (cap 8192; overflow evicts the oldest
  inline), a `(cr3, va) -> slot` paged-out map (capacity reserved at init,
  so eviction never allocates under memory pressure), and a slot bitmap
  over the pagefile region.
- **The pagefile is a raw disk region** (sectors 8192..32768, 3072 page
  slots = 12 MiB), *not* a `pagefile.sys` inside FAT — paging must never
  recurse into the filesystem it might be paging out. The scratch-disk
  image was re-laid-out for this: the FAT32 volume shrank to 4 MiB
  (`tools/gen_fat32.py`), leaving the last 12 MiB raw.
- **Eviction**: validate the PTE in the target address space
  (`mm_debug_pte_in`, dropping stale registry entries), write the 8
  sectors straight from the physical window, record the slot, unmap
  (`mm_unmap_user_page_in` — invlpg only when the target AS is live),
  free the frame. **Page-in** runs inside `vad_resolve` before the
  zero-fill path: find the record, read the slot into a fresh frame, map
  with the VAD's protection, re-register. `vad_free`/`vad_teardown`
  release orphaned slots (`drop_range`/`drop_process`).
- **Pressure path**: `mm_allocate_contiguous_pages` now tries once, then
  evicts `count` pages and retries — with the PFN lock *not* held
  (eviction does block I/O and ends in `mm_free`; PFN stays leaf-most).
- **Two self-deadlocks caught by the boot suite**: `register_page`
  re-locked `REGISTRY` while its guard was still alive (wedged the very
  first demand-commit), and `slot_free` locked `SLOT_HINT` twice in one
  assignment statement. Non-reentrant spinlocks make this class of bug
  loud — the boot simply stops.
- **A latent virtio-blk race, exposed statistically**: `request()`
  snapshotted the used-ring index *after* ringing the doorbell, so a
  device that completes a single-sector request in microseconds (QEMU
  does) advances the index before the baseline read — the poll then
  waits for a *second* advance that never comes and burns its full
  2-billion-spin escape bound (~10-20 s) per sector. The ~17 extra
  pagefile requests per boot turned a race the suite had always won into
  one it lost roughly every run. Fixed by snapshotting the baseline
  before publishing the descriptor chain.

Boot suite (108 checks): pagefile online, eviction writes the page out
and frees its frame (free-page count +1), the pagefile slot holds the
evicted page's actual bytes (read back raw from disk), and a second
touch pages the content back in byte-exact.

Verified: QEMU suite 108/108 twice in a row (exit 33) — the virtio race
was timing-dependent, so one green run was not accepted as proof; host
18+2+7; emu 36/36. NT's memory-management story is now complete through
the pagefile; the remaining frontier is SMP (AP startup, per-CPU KPCR,
IPIs, TLB shootdown) and frame-based SEH (`.pdata` unwind).

### 2026-07-18 - SMP I: the application processors come online

The kernel is now multiprocessor-aware at the bring-up level: the BSP
discovers the machine's CPUs from ACPI and starts every application
processor, each landing in Rust with its own GDT/TSS/IDT/KPCR and local
APIC before parking. The scheduler, clock, and user mode stay BSP-only;
per-CPU scheduling and TLB shootdown IPIs build on this.

- **`hal::acpi`** (new): RSDP scan (EBDA + F-segment), RSDT/XSDT walk,
  MADT parse — signature- and checksum-validated everywhere, so nanox
  (no ACPI) cleanly reports zero processors. Caught live: the RSDP
  revision lives at offset **15**, not 9 — SeaBIOS is revision 0
  (ACPI 1.0), so the wrong offset read the first OEMID byte, which is
  always >= 2, silently forcing the XSDT path onto a machine that only
  has an RSDT.
- **The trampoline** (`ke::smp`, `global_asm!`): 16-bit real mode ->
  32-bit protected -> 64-bit long mode, copied to physical 0x8000
  (SIPI vector 0x08, reserved in the PFN bitmap) with all references
  absolute literals and a `.org`-pinned layout. The BSP patches three
  fields into the page: transition CR3, the AP's stack top, and
  `ap_rust_entry`'s runtime VA (the kernel is PIE — no absolute
  relocation against a Rust symbol exists, so the address travels as
  data). A private transition PML4 (identity 2 MiB + a copy of the
  kernel high half) carries the AP across the `mov cr3` boundary.
- **Per-CPU everything**: `gdt::init_ap` / `pcr::init_ap` / `idt::load`
  / `apic::init_ap` give each AP private GDT+TSS (the busy bit is
  per-descriptor), a private KPCR behind its own GS base, the shared
  IDT, and a software-enabled LAPIC (no timer — the BSP's clock stays
  the only one). CR4 is adopted from the BSP so SMEP/SMAP match.
- **INIT-SIPI-SIPI** via the LAPIC ICR, one AP at a time, with an
  rdtsc-bounded online wait; a stubborn AP is skipped, not panicked.
- **Two trampoline bugs the stage-letter trick caught** (outb to COM1
  from each mode, since an AP fault before its IDT is a triple fault
  and a machine reset): the GDTR base literal pointed 2 bytes past the
  real GDT (a 6-byte GDTR is not 8-aligned — the `.align` lost in the
  `.org` rewrite had been hiding that), and the GDTR limit said
  `3*8-1` for a **4-entry** GDT, so the far jump to the 64-bit
  segment (selector 0x18) was out of bounds. Both times the AP reset
  the machine exactly one mode-transition earlier than it got.

Boot suite (112 checks, now with `-smp 4`): MADT enumerates the
processors, every processor comes online, APIC IDs are distinct, and
each processor reads its own number back *through its own GS* — the
per-CPU KPCR proof.

Verified: QEMU suite 112/112 twice (exit 33); host 18+2+7; emu 36/36;
nanox pipe session still boots (RSDP scan finds nothing, Smp checks
skip cleanly). Next on the SMP road: per-CPU scheduling with IPI-based
dispatch and TLB shootdowns, then SEH (`.pdata` unwind).

### 2026-07-18 - SMP II: every processor schedules

The parked APs from SMP I now run the full dispatcher: each has an idle
thread, its own LAPIC clock, and pulls work off the global ready queues.
Threads preempt and migrate across all four processors; user processes
run wherever they're allowed to.

- **APs join the scheduler**: `ki_initialize_ap` adopts the AP's startup
  context as its idle thread (the BSP trick, per CPU), `apic::init_ap`
  arms the per-CPU LVT timer, and `ap_idle_loop` parks in `sti; hlt`.
  `KeTickCount` advances only on the BSP's tick (four 1 kHz timers
  would quadruple time); every CPU does its own quantum accounting.
  `syscall::init()` now runs on each AP too — STAR/LSTAR/FMASK are
  per-CPU MSRs, and the first `syscall` on an AP was jumping to
  address 0. (That was the SMP II double-fault: a user thread stole
  onto an AP and vectored into nowhere.)
- **Load balancing by broadcast**: readying or signaling a thread sends
  the dispatch IPI to self *and* all-but-self; idle APs steal off the
  global queues. The dispatch ISR is idempotent, so extra wakeups cost
  nothing. Per-CPU `set_kernel_stack` (TSS.RSP0), per-CPU syscall
  stack, per-CPU `KERNEL_GS_BASE` — all already keyed off the PRCB.
- **TLB shootdowns**: every CPU publishes its current CR3; an unmap or
  protection change (`mm_unmap_user_page`, `_in`, `mm_protect_user_page`)
  now IPIs every other CPU running that address space (vector 0xE0,
  above the clock so a spinning shooter still answers inbound
  shootdowns — cross-shootdowns can't deadlock). Mailbox per CPU
  (VA + seq/ack), shooters serialized on a global lock, bounded wait.
  Plus the race SMP made real: two CPUs faulting the same VAD page at
  once — the loser of the VADS-lock race now sees the page already
  mapped and returns instead of double-backing it.
- **The spawn-ready race**: every "create thread, then write its state"
  path was built on the UP truth "it can't run until we block". With
  APs, a readied thread can start on another CPU *before the create
  call returns*: process threads booted on the kernel AS (the
  `tcb.cr3 = cr3` write arriving late), missing their cmdline/std
  handles/MUI. Now `cr3` is recorded before readying
  (`ps_create_system_thread_ex`), and every path with post-create
  writes creates suspended and resumes
  (`ps_create_system_thread_suspended` + `ps_resume_thread`) — NT's
  own `CREATE_SUSPENDED`/`ResumeThread` pattern.
- **A global `DELIVERING` flag suppressed APC deliveries** whenever two
  CPUs delivered concurrently (one always won, the other's APCs never
  ran — the flaky kernel-APC test failures). It's per-CPU now: nesting
  is a same-CPU concern.
- **Isolated processes pin to CPU 0** (processor affinity): their
  per-process DLL data lives in shared shim pages swapped on context
  switch — correct only when at most one isolated thread runs at a
  time. The scheduler's select/preemption checks skip threads the
  current CPU may not run (`cr3 != 0` ⇒ CPU 0 only). Kernel threads
  and shared-AS user threads run anywhere. The honest follow-up is
  real per-process shim pages (NT's COW), which lifts the pin.

Boot suite (114 checks, `-smp 4`): six kernel threads demonstrably run
on more than one processor, and a TLB shootdown to the kernel address
space is acked by every online CPU.

Verified: QEMU suite 114/114 three consecutive times (the pre-fix
failures were timing-dependent, so one green run wasn't accepted);
host 18+2+7; emu 36/36; nanox pipe session (uniprocessor) still clean.
Remaining SMP frontier: per-CPU ready queues (the global dispatcher
lock is the classic first design, not the scalable one), shootdown
batched by range, and unpinning isolated processes once shim data is
truly per-process.

### 2026-07-18 - per-process shim pages: real COW DLL data, and the end of the CPU-0 pin

SMP II pinned isolated processes to the BSP because their per-process
DLL data lived in shared shim pages swapped on context switch. That pin
is now gone for the right reason: every process gets *physically
private* shim .data pages — NT's copy-on-write, done eagerly.

- **`mm::virt::mm_privatize_pages`** (new): for each writable shim page
  of a new process, the page-table chain out to it is *cloned* (the
  window's PDPT copied per process, intermediate tables cloned,
  large leaves split to 4 KiB) and the leaf PTE gets a fresh frame —
  same VA, different physical page. `mm_free_privatized` reclaims the
  frames and cloned tables at process exit. Everything outside the
  shim regions keeps sharing tables and frames untouched.
- **The seed must be the pristine snapshot, never the shared page** —
  the bug this round's debugging chased. Seeding from the live shared
  page copied the *kernel-AS apps'* runtime state (VEH registrations
  pointing into their own images, heap arenas, fd tables) into every
  new process: an isolated process's first exception dispatched into
  *another image's* VEH handler and died returning through a foreign
  stack frame. The give-away in the log: a user fault whose RIP sat in
  a *different process's* image at a `pop rsi` epilogue, reading above
  its own stack top. `register_shim_data` now snapshots page-aligned
  spans at shim load (128 KiB budget so ulib fits); privatization
  seeds from it.
- **ulib joins the scheme**: `load_ulib` registers its writable
  sections too, so the `reset_ulib_data` pre-spawn ritual (and its
  "processes run serially" assumption) is deleted — every process's
  CRT guards start pristine by construction.
- **The context-switch swap and the affinity pin are deleted.**
  `swap_shim_data`, the slot buffers, and the scheduler's per-switch
  copy are gone; isolated (`cr3 != 0`) threads schedule on all four
  CPUs like everything else.

Boot suite (115 checks): the two-process test now proves *physical*
privacy — same shim .data VA in both processes, different frames,
neither the kernel AS's frame — while both CRTs run concurrently on
whatever CPUs pick them.

Verified: QEMU suite 115/115 three consecutive times; host 18+2+7;
emu 36/36; nanox pipe session clean. SMP story now: AP startup,
per-CPU scheduling, TLB shootdowns, and unrestricted migration. Next
candidates: SEH (`.pdata` unwinding), per-CPU ready queues, or driver
`.pdata`-based kernel SEH for crash paths.

### 2026-07-20 - frame-based SEH: .pdata unwinding works

The kernel's exception story is complete: vectored handlers first, then
frame-based SEH, then the unhandled fate — NT's exact order. sehtest.exe,
a clang-built C binary with real `__try/__except/__finally` metadata,
faults four times and recovers four times.

- **The shim's SEH engine** (kernel32, ~250 lines): `RtlLookupFunctionEntry`
  (the process module from the PEB's loader list; `.pdata` from the PE
  exception directory; binary search for the covering RUNTIME_FUNCTION),
  `RtlVirtualUnwind` (the version-1 UNWIND_CODE set: nonvol pushes/saves,
  small/large allocs, SET_FPREG frames, machine frames, mid-prolog
  partial unwinds, XMM slots skipped-by-size), the **leaf rule** for
  functions with no `.pdata` entry (caller = [RSP]), and the dispatch
  loop itself — each frame's registered handler is invoked with a real
  `DISPATCHER_CONTEXT`; `ExceptionContinueExecution` resumes through
  `NtContinue`, `ContinueSearch` unwinds one frame and keeps looking,
  walk exhausted = the old unhandled path.
- **Frame handlers are pluggable** — the .xdata handler field just names
  a function with the `__C_specific_handler` signature — and the test
  exploits that: sehtest carries its *own* handler that dispatches on a
  "which try is live" marker, so the OS machinery is proven without
  betting on a toolchain's scope-table layout. The four cases: fault in
  `__try` recovered; the `__finally`-carrying frame visited during the
  walk; a fault in a leaf callee recovered by the caller's frame (the
  leaf rule); nested try falling through to the outer frame.
- **`ExitProcess` now forwards the exit code** in the kernel32 shim (it
  used to drop it) — `GetExitCodeProcess` reads real values; sehtest
  reports its case bitmask through it.
- A nuance learned the hard way: clang constant-folds provable faults
  (a null store always faults, so the except "always runs" and the SEH
  metadata evaporates). Only non-provable fault sites keep real tables —
  which is why all four test faults go through `0x1234`.

Boot suite (118 checks): sehtest runs as a process and its exit bitmask
must be 0xF — all four cases recovered through the unwinder.

Verified: QEMU suite 118/118 three consecutive times; host 18+2+7;
emu 36/36; nanox pipe session clean. Deliberately NOT in this commit:
the CRT scope-table walk (`__C_specific_handler` proper in the msvcrt
shim) — no in-tree binary needs it today, and shipping it unvalidated
was worse than documenting it as the follow-up. Roadmap is now fully
clear through SMP and SEH; remaining candidates: per-CPU ready queues,
`RtlUnwindEx`/global unwind for longjmp-style exits, kernel-mode SEH
for driver crash paths.
