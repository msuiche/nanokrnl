# nanokrnl in the browser

This directory holds the project's browser demo. The live site is served from
the **`nanox/`** subfolder (point GitHub Pages at `web/nanox/`): the unmodified
**nanokrnl** kernel booting in a web page via **nanox**, our ~60 KB x86-64
WebAssembly emulator (`../emu`), reaching a `C:\>` prompt and running real
Microsoft binaries (`cmd.exe`, `more.com`, …) typed at the keyboard.

Because we control the bootloader, nanox boots **directly in long mode** and
emulates only the devices the kernel touches (16550 UART, Local APIC + timer,
PS/2). The result is a single ~60 KB `.wasm` with **no threads, no
`SharedArrayBuffer`, and no COOP/COEP headers** — it serves from any plain
static file host. See [`../emu/README.md`](../emu/README.md) and
[`../emu/SPEC.md`](../emu/SPEC.md) for the emulator design and how it's verified
(differential testing against iced-x86 and Unicorn).

## What's here

- **`nanox/`** — the live demo:
  - `index.html` — terminal UI with Boot / Restart / Shutdown controls and a
    keyboard bridge (keystrokes → COM1).
  - `nanox.wasm` — the emulator module.
  - `kernel.bin` — the staged kernel ELF that nanox boots.
  - `ntoskrnl.pdb` + `ntoskrnl.exe` — WinDbg symbols for the crash dump. A crash
    writes `MEMORY.DMP` to `H:\`; download it plus these two from the H:\ Explorer
    to open the dump with symbols. `gen_pdb.py --fixed-guid` stamps them with the
    same RSDS GUID the kernel writes into every dump, so they always pair.
  - `background.js`, `chiptune/`, `tracks/` — an animated ASCII backdrop and a
    chiptune soundtrack played through an AudioWorklet.

## Build, stage & serve

The build script compiles nanox to WebAssembly and stages the current kernel
image into `nanox/`:

```sh
sh ../emu/build-wasm.sh
(cd nanox && python3 -m http.server 8000)   # http://localhost:8000
```

Click **Boot**, wait for the banner and self-tests to scroll past, then type at
the `C:\>` prompt.

**Serve over HTTP, not `file://`.** The AudioWorklet that drives the chiptune
backdrop only loads from a real origin, so opening the file directly won't play
audio (and some browsers block the worklet entirely).

## Why `tracks/manifest.json`

GitHub Pages has no directory autoindex, so a page can't discover the contents
of a folder at runtime. `tracks/manifest.json` is an explicit list of the
chiptune track filenames in `tracks/` that `index.html` fetches to know what to
play. Add or remove a track by editing both the directory and that manifest.

## History

This used to host two other attempts that have since been **removed**: a v86
harness (`index.html`) — v86's CPU is 32-bit only and panics on `#GP` the moment
the kernel enters long mode — and a `qemu-wasm` experiment (`qemu/`) — real QEMU,
but a multi-megabyte payload that needs threads + `SharedArrayBuffer` + COOP/COEP.
Only nanox is kept; the full reasoning is in [`../WORKLOG.md`](../WORKLOG.md).
