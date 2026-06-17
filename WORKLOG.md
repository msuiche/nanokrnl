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
**unmodified x86 kernel image** under a browser x86 emulator. v86 is a complete
PC emulator that runs client-side and boots the exact same BIOS disk image QEMU
does; our kernel's console is COM1, wired to the page.

### How to run

```sh
sh web/run.sh                          # builds the interactive kernel + disk image, stages web/disk-bios.img
(cd web && python3 -m http.server 8000)
# open http://localhost:8000 , click the console, type
```

`web/index.html` loads v86 (from a CDN), boots `disk-bios.img`, and bridges COM1
to the page — so you get the real `C:\>` prompt with `cmd`, `whoami`, `more`,
`dir`, `where`, `sort`, `choice`, and the `null.sys` driver, all running as
actual x86-64 code in the browser. (Browser boot needs to be opened manually;
the staged image is verified to boot cmd under native QEMU.)

Note: this is v86, a browser-native full-PC emulator (the practical "qemu in the
browser"). Literal qemu-compiled-to-wasm is an alternative but heavier to host;
the disk image is the same either way.

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
  under native QEMU — the same image v86 runs.
