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
