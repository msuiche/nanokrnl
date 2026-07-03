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
