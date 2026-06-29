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
