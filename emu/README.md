# nanox

A bespoke x86-64 emulator written in Rust whose one job is to boot the
unmodified `ntoskrnl-rs` kernel **in a web browser**, as a much lighter
alternative to running QEMU in the browser.

It is an interpreter (no JIT), it boots **directly in long mode** (no BIOS, no
real mode), and it emulates **only the few devices our kernel actually touches**
(16550 UART, Local APIC + timer, PS/2). The result is a ~52 KB WebAssembly
module with no threads, no `SharedArrayBuffer`, and no COOP/COEP headers — it
runs from a plain static file server.

See [`SPEC.md`](../SPEC.md) at the repo root for the full design and current
boot status.

## Why not just use an existing browser emulator?

We looked at all of them first. The short version: the ones that are *open and
hostable* can't run a 64-bit kernel without heavy machinery, and the one that
*can* run 64-bit guests in a tiny package is closed source. Hence nanox.

| Project | What it is | Language / tech | 64-bit guest? | Browser size | Needs threads / COOP-COEP | Source |
|---|---|---|---|---|---|---|
| **v86** | Full x86 **PC** emulator in the browser | Rust CPU + an x86→WASM **JIT**, plus JS devices | **No** — 32-bit to the core | ~MB | No | Open (BSD) |
| **JSLinux / "asm.js"** | Bellard's browser x86 emulator | Hand-written x86 core, originally **asm.js**, now WASM via emscripten | **Yes** (runs Windows NT) | small | No | **Closed** (only the RISC-V half is released) |
| **TinyEMU** | Small system emulator (the base JSLinux builds on) | C → WASM (emscripten) | RISC-V only in the open source; **x86 is KVM-based** (no software x86 released) | small | No | Open core is **RISC-V only** |
| **qemu-wasm** | Full **QEMU** compiled to the browser | C, **TCG→WASM** JIT (emscripten) | **Yes** | **many MB** | **Yes** (pthreads + `SharedArrayBuffer` + COOP/COEP) | Open (GPL) |
| **nanox** (this) | Minimal emulator for *our* kernel | **Rust → WASM**, interpreter | **Yes** | **~52 KB** | **No** | This repo |

### v86
An impressive, complete x86 *PC* emulator with an x86→WebAssembly JIT. But its
CPU and JIT are 32-bit to the bone: general registers are stored as eight
`i32`s, `EIP`/control registers are `i32`, paging tops out at PAE, and the
instruction decode tables have no REX prefix. Its own README states "64-bit
kernels are not supported" ([issue #648](https://github.com/copy/v86/issues/648)).
Adding long mode would mean rewriting the JIT — months of work.

### JSLinux ("the asm.js one")
Fabrice Bellard's original in-browser PC emulator (2011) was hand-written and
famously compiled to **asm.js**; today it's built on TinyEMU and compiled to
WASM with emscripten. Its x86 core *is* now 64-bit and runs Windows NT in the
browser — almost exactly the capability we want. The catch: **that x86 core is
closed source.** Bellard releases only the RISC-V half of TinyEMU. We can't
fork, build, host, or modify it.

### TinyEMU
The small, MIT-licensed system emulator that JSLinux is based on. Crucially, the
**released source contains no software x86 CPU** — its only software core is
RISC-V. TinyEMU's "x86" support runs guests through the Linux **KVM** API (real
hardware virtualization), which is unavailable in a browser. So there is nothing
in the open TinyEMU to "extend to x64."

### qemu-wasm
Full QEMU with its TCG dynamic translator compiled to WebAssembly. It genuinely
boots 64-bit guests in the browser and is open source — this is the route that
works for us today. The cost: it emulates an entire generic PC (reset vector →
SeaBIOS → bootloader → long mode), ships a multi-megabyte WASM, and requires
pthreads, `SharedArrayBuffer`, and COOP/COEP cross-origin-isolation headers.
That's a lot of weight and deployment friction to boot one kernel.

## What nanox does differently

We don't need to emulate a generic PC — we only need to boot *our* kernel, and
we control its bootloader. That lets us cut almost everything qemu-wasm carries:

- **Boots directly in long mode.** `Machine::boot_kernel` reproduces the
  `bootloader_api` handoff itself — loads + relocates the kernel high-half,
  builds the page tables and `BootInfo`, and enters `_start`. No real mode, no
  BIOS, no 16/32-bit bring-up to emulate.
- **Only the devices the kernel uses:** a 16550 UART (the console), the Local
  APIC + timer (the scheduler tick, vector 0xD1), and PS/2 (keyboard). Not the
  IDE/VGA/PCI/virtio chipset a general PC emulator must carry.
- **Interpreter, not a JIT.** A console-driven kernel doesn't need JIT speed, and
  an interpreter is far smaller and simpler to get correct.
- **Self-contained Rust → WASM + a tiny JS shim.** No external runtime, no
  threads, no cross-origin isolation. The whole thing is a ~52 KB `.wasm` plus a
  ~4 KB HTML page that streams the UART to the screen.

The lineage: nanox grew from the project's earlier usermode x86-64 interpreter
(REX/ModRM/SIB decode, RIP-relative addressing, ALU/stack/control flow), then
gained a long-mode MMU, the device set, interrupt/syscall handling, an ELF
loader, and the bootloader handoff. A `machine_mode` flag keeps the original
usermode path intact.

## Build & run

```sh
# Host tests (decoder, MMU, devices, machine, ELF):
cargo test

# Boot the real kernel natively and watch it run:
cargo run --release --example inspect_kernel

# Build the browser module + stage the kernel, then serve and open it:
sh build-wasm.sh
(cd ../web/nanox && python3 -m http.server 8000)   # http://localhost:8000 → "Boot kernel"
```

## Status (summary)

nanox boots the real `ntoskrnl-rs` kernel — native and in the browser — through
both init phases, runs it under a live preemptive scheduler, and **passes the
kernel's entire boot self-test suite (67 checks → "ALL SELF TESTS PASSED")**,
including loading and running the real Microsoft `null.sys` driver and ring-3
user processes (CreateProcess + CRT, scalar floating point). Verified identical
on native and the wasm/browser build.

### How we know it's correct
nanox is validated by **differential testing against authoritative oracles**, not
by hand:
- `cargo run --release --example conformance` — disassembles the kernel's
  `.text` with [iced-x86](https://github.com/icedland/iced) and checks nanox
  decodes every instruction to the **same length** (catches operand-size /
  decode desync bugs). 0 mismatches across ~53k instructions.
- `cargo run --release --example diff_unicorn --features oracle` — runs each
  kernel instruction through both **Unicorn (QEMU's CPU core)** and nanox from
  identical state and diffs registers + flags (catches wrong-result / EFLAGS
  bugs). This found the register-width, CF/OF, and shift bugs that were
  corrupting the boot.

The `--features interactive` build goes further: it loads the **real Microsoft
cmd.exe**, reaches a `C:\>` prompt, and runs typed commands (`ver`, `echo`,
`exit`) — verified native and in the browser (keystrokes → COM1). The web page
(`web/nanox/index.html`) is a terminal UI with Boot / Restart / Shutdown
controls; type into the console once the prompt appears. See
[`SPEC.md`](../SPEC.md) §6–7 and `ROADMAP.md`.
