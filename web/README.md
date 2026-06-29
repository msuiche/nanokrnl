# ntoskrnl-rs in the browser

This directory holds the in-browser story for the kernel. We tried three engines
to run a 64-bit NT-architecture kernel in a web page; only the last is kept.

## The journey

**1. v86** — a complete, open-source x86 PC emulator with an x86→WASM JIT.
Boots a BIOS disk image straight from the page. But its CPU and JIT are 32-bit
to the core (registers are `8 × i32`, paging tops out at PAE, no REX), and it
panics with "Unimplemented #GP" the moment our kernel enters long mode. v86's own
README says "64-bit kernels are not supported." Dead end for us.

**2. qemu-wasm** — real QEMU (its TCG dynamic translator) compiled to
WebAssembly. It genuinely boots 64-bit guests and is open source, so it *works*.
The cost: it emulates an entire generic PC (reset vector → SeaBIOS → bootloader →
long mode), ships a **~39 MB** `.wasm` plus a multi-MB disk/ROM pack, and requires
**pthreads + SharedArrayBuffer + COOP/COEP** cross-origin-isolation headers. A lot
of weight and deployment friction to boot one kernel. (Also of note: Bellard's
JSLinux has a 64-bit x86 core that runs Windows NT in a browser — but it's closed
source; only the RISC-V half of TinyEMU is released.)

**3. ntemu** — our own bespoke x86-64 emulator (`../emu`, crate `ntemu`).
Because we control the bootloader, it skips real mode and BIOS entirely and boots
**directly in long mode**, emulating only the devices the kernel touches (16550
UART, Local APIC + timer, PS/2). The result is a **~60 KB** `.wasm` with **no
threads, no SharedArrayBuffer, no COOP/COEP** — it serves from any static file
host. It boots the real kernel through both init phases, passes the full self-test
suite, and runs an interactive **cmd.exe**.

| Engine | 64-bit? | Payload | Threads / COOP-COEP | Status |
|---|---|---|---|---|
| v86 | no (32-bit core) | ~MB | no | doesn't boot our kernel |
| qemu-wasm | yes | ~46 MB | **yes** | works, heavy |
| **ntemu** | yes | **~60 KB** + kernel | **no** | **works — interactive cmd.exe** |

## What's here

- **`ntemu/`** — the live demo. `index.html` (terminal UI + Boot/Restart/Shutdown
  + keyboard → COM1), `ntemu.wasm`, the staged `kernel` ELF, and `background.js`
  (an animated ASCII-dither backdrop). Build/stage with `sh ../emu/build-wasm.sh`,
  then serve this directory and open `ntemu/`:

  ```sh
  sh ../emu/build-wasm.sh
  (cd ntemu && python3 -m http.server 8000)   # http://localhost:8000
  ```

The v86 page and the qemu-wasm build have been retired; see `../emu/README.md`
and `../SPEC.md` for the emulator design and how it's verified (differential
testing against iced-x86 and Unicorn).
