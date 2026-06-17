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

The faithful browser route is **real qemu-wasm** (QEMU compiled to wasm via
emscripten), which fully emulates what native QEMU does and would boot our image
unchanged. It's a heavy artifact to build/host and hasn't been wired up here.
Native QEMU works today (`sh scripts/run-interactive.sh`).

### How to run

- **Native QEMU (works today):** `sh scripts/run-interactive.sh` — the real
  `C:\>` shell on the serial console.
- **Browser:** not working yet. `web/run.sh` builds + stages `web/disk-bios.img`
  and `web/index.html` is a v86 harness, but v86 can't boot this kernel (see
  above). A real qemu-wasm harness is the remaining work.

## Status — kernel (x86)

Working: interactive `cmd.exe`, `echo`, `exit`, `dir`, `where`, `sort`,
`choice`, `whoami` (`nanokrnl\user`), `more readme.txt` (prints the file), and
the `null.sys` driver. Default self-test suite passes (exit 33). Key commits:
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
