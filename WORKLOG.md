# ntoskrnl-rs — Work Log

## Goal: run the nano kernel as a WebAssembly module in the browser

Compile the kernel's NT subsystems to `wasm32` and **substitute the hardware
layer** with browser-provided primitives, so the kernel runs as a module inside
a web page. Explicitly NOT an x86 emulator (no qemu-wasm): WASM cannot execute
the x86-64 Windows binaries natively, so the WASM build demonstrates the
kernel's own logic — object manager, registry, memory/pool allocators,
scheduler model, RAM filesystem, RTL — driven by a JS host that stands in for
"hardware" (console, timer, storage). Novel: an NT-compatible kernel core
running in the browser with no virtualization.

### Why this is non-trivial

WASM has no privilege rings, no MMU/page tables, no interrupts, no `syscall`
instruction, and no native code execution. The x86 build relies on all of them.
So the port is fundamentally a **HAL substitution**: the hardware-independent
subsystems compile as-is; the x86 machinery gets browser-backed equivalents or
software models.

### Architecture: what's hardware vs. portable

Portable (pure Rust, should compile to wasm32 ~unchanged):
- `rtl/` — runtime library (strings, status codes)
- `ob/` — object manager (handles, namespace)
- `cm/` — configuration manager (registry)
- `io/ramfs` — RAM filesystem
- `ex/` — executive (pool wrappers, lookaside)
- much of `ps/` — process/thread bookkeeping (the structs, not the context switch)

Hardware (needs substitution behind a HAL boundary; ~16 files use `asm!`/ports):
- `hal/` — `port.rs` (in/out), `apic.rs`, `pic.rs`, `serial.rs` → JS console + timer
- `mm/virt.rs` — x86 page tables (cr3) → software/identity memory model over WASM
  linear memory; `mm/phys.rs` pool stays (software allocator)
- `ke/` — `gdt`, `idt`, `traps`, `selectors` (CPU descriptor tables / interrupts →
  N/A in WASM), `syscall` (`syscall` instruction → direct dispatch), `scheduler`
  + `thread` + `pcr` + `usermode` (context switch via registers → cooperative
  model), `irql`/`spinlock` (→ no-op / single-threaded), `bugcheck`/`debug`

### Phased plan

- [x] **Phase 0 — Scaffolding & proof of life.** `wasm/` crate (cdylib,
  `wasm32-unknown-unknown`, excluded from the x86 workspace). Host (`web/index.html`
  + Node `web/run-node.mjs`) provides the `env.host_write` import and calls the
  exported `kernel_main()`; WASM linear memory stands in for RAM. Runs `mm`
  (pool), `ob` (namespace insert/lookup), and `rtl` (status) miniatures as self
  tests. **Verified**: boots headless under Node, all self tests pass, returns 0.
  Build: `sh wasm/build.sh`; test: `node wasm/web/run-node.mjs`.
- [~] **Phase 1 — reuse real kernel modules / HAL boundary.** Real kernel
  modules now run in WASM via `#[path]` includes, with WASM-side HAL shims for
  what they depend on:
  - `rtl` (status, bitmap, list, string) — hardware-free, included as-is.
  - `ob` (object manager + handle table) — included as-is; its deps are
    satisfied by `wasm/src/mm/pool.rs` (pool over a static arena) and
    `wasm/src/ke/spinlock.rs` (single-threaded no-op `SpinLock`). Self tests
    exercise the real ref-counting, handle create/resolve/close, and the type
    delete procedure firing on the last dereference. Verified (exit 0).
  Next: `cm` (registry) and `io/ramfs` — and eventually fold these shims into a
  real `hal` cfg seam in the kernel crate so it builds for wasm32 directly
  (`#[cfg]`-gate the x86 asm; serial→JS, timer→JS, ports→no-op).
- [ ] **Phase 2 — Memory.** Software phys allocator over a large static/linear
  arena; replace `mm/virt` page-table mapping with a flat software model
  (identity or a translation table) so `mm` APIs work without an MMU.
- [ ] **Phase 3 — Scheduler.** Cooperative run-to-completion / green-thread model
  replacing register context switches; IRQL/spinlocks become no-ops
  (single-threaded host).
- [ ] **Phase 4 — Boot path & self-tests.** Run the hardware-independent self
  tests (pool stress, ob, cm, ramfs) from `kernel_main()` in the browser and
  report pass/fail to the page.
- [x] **Interactive console.** The WASM kernel is now interactive: event-driven
  input (`kernel_input_ptr` exposes a fixed buffer; the host writes a line and
  calls `kernel_input(len)`), a shell prompt, and a built-in command set
  (`help`, `ver`, `echo`, `mem`, `mkobj`, `handles`, `close`, `cls`) driving the
  real subsystems — e.g. `mkobj`/`close` create and tear down real `ob` objects,
  delete procedure and all. Both hosts updated: browser (input field) and Node
  (readline; also scriptable via a pipe). Verified.
- [~] **Phase 5 — running "executables".** WASM cannot execute the x86-64 PE
  binaries the native kernel runs (no emulation, per the goal), so an
  "executable" here is a **guest WASM module** over a syscall ABI. Working
  minimal version: `run <prog>` → the kernel calls `host_run`, the host
  instantiates `<prog>.wasm` as a second instance and bridges its `sys_print`
  syscall to the console, runs `main`, and returns the exit code. Sample guest
  in `wasm/programs/hello/`. Verified: `run hello` prints via the syscall and
  reports exit 0; `run nope` → not found. Next: richer syscall surface (read
  input, open/read kernel objects/files) and more guest programs; route guest
  syscalls *through* the kernel (not just the host) for real mediation.

## Track B — our own x86-64 emulator (run the real binaries & drivers in WASM)

Track A above runs the kernel's *Rust* subsystems in WASM with guest WASM
"executables". Track B is the harder, flashier goal: run the **actual x86-64 PE
binaries** (cmd/whoami/more) and **drivers** (null.sys) in the browser by
writing our own minimal x86-64 interpreter — NOT qemu-wasm, and NOT full-platform
emulation. The decisive leverage: our kernel already provides the syscall layer,
the ntoskrnl exports, the PE loader, and the WDM ABI as portable Rust. So the
emulator only runs the *instruction stream* and bridges calls into code we
already have — no MMU, no chipset, no device emulation (for software targets).

### Architecture

- One reusable **x86-64 decoder + interpreter** (same ISA for ring 3 and ring 0).
  Registers + flags + a flat linear-memory address space (the WASM heap is the
  binary's RAM; no paging).
- **User mode (ring 3):** execute the PE's code; when it reaches the ntdll
  `syscall` trampoline, stop interpreting and call our existing kernel syscall
  handlers (`register_service`/SSDT). Return into the interpreter.
- **Kernel mode (ring 0, drivers):** no `syscall` — driver `call`s resolve
  (via the existing driver loader's import binding) straight to our native
  `ntoskrnl` export functions. Privileged instructions (`cli`/`sti`, memory
  barriers, `cr8`/IRQL) are modeled or no-op'd; a software driver emits few/none.
- **Reuse, don't rebuild:** PE loader, kernel32/msvcrt/ntdll shims, ntoskrnl
  exports, IRQL/DPC/dispatcher, ob/mm/io — all already exist in Rust.

### Method: trace-driven, opcode-by-opcode

Run; on an unimplemented opcode, dump it and implement it; repeat until the
target runs. Same evidence-driven loop that cracked whoami/more natively. Flags
(CF/OF/ZF/SF/AF/PF) are where the bugs hide — test them hard.

### Phases

- [x] **B0 — decoder skeleton + harness.** `wasm/emu/` (`x86emu`, no_std but
  host-testable). Register file + RFLAGS, flat byte-slice memory, REX prefix, a
  `step`/`run_program` loop that returns `Unknown{rip,byte}` / `Fault{addr}` so
  growth is trace-driven. Verified by host unit tests.
- [~] **B1 — usermode core.** Instruction subset (growing trace-driven): ModRM
  addressing (reg-direct, `[base]`, `[base+disp8/32]`, SIB; RIP-relative
  approximate), reg/mem ALU both directions, the immediate-group ALU (0x81/0x83
  `/0../7` = add/or/adc/sbb/and/sub/xor/cmp — covers `sub rsp,N`, `cmp r,imm`,
  `and`, …), `mov r,imm`/`mov r/m,imm32`, `lea`, `test`, `movzx`/`movsx`, the
  stack (`push`/`pop`), control flow (`call`/`ret`+`HALT_ADDR`, `jmp` rel8/32,
  `jcc` rel8 **and rel32**), and `syscall` (0F 05). A shared `apply_alu` sets
  CF/OF/ZF/SF/PF. **Wired into the kernel** via `run86`: real x86-64 machine
  code runs inside the WASM kernel and calls back through a syscall the kernel
  services (write).
  - **PE loader** (`wasm/emu/src/pe.rs`): maps a real PE32+ image at VA 0 (VA==
    RVA), applies base relocations, and **resolves imports** — binds each IAT
    slot to an import-trap address (`IMPORT_BASE+idx*8`, a region inside the
    buffer so *data* imports read/write real memory while *function* imports
    trap by address). `import_name()` maps a trap index back to `dll!name`.
  - **Trace harness** (`cargo run --example trace_whoami`) runs the real
    `whoami.exe` and reports the next unimplemented opcode — driving coverage.
    Added, trace-driven: the full two-operand integer ALU (all 8 ops, both
    directions) via `apply_alu`; immediate group; `mov`/`movsxd`/`movzx`/`movsx`;
    `lea`, `test`, `xchg`, `cmpxchg`; the shift/rotate group (0xC1/D1/D3); the
    `0xF7` unary group (test/not/neg/mul/imul/div/idiv); group-5 (`0xFF`:
    inc/dec/call/jmp/push r/m); `jcc` rel8/32 + `setcc` (full 16 condition
    codes); FS/GS **segment-override prefixes** + `gs_base`/`fs_base` (so
    `gs:[...]` TEB access works); `syscall`.
  - Added since: SSE/XMM moves (`movups`/`movaps`/`movdqa`/`movdqu`) + `xorps`/
    `pxor` (XMM regs + mandatory-prefix tracking), multi-byte `nop` (0F 1F),
    8-bit `mov`/`test`, the bit-test group (0F BA), and the 8-bit unary group
    (0xF6).
  - **MILESTONE: the real `whoami.exe` now executes to completion** — **1874
    instructions** through `__security_init_cookie`, the full CRT startup,
    `_initterm` C++ ctors, `main`, and `exit`/`_cexit` → clean HALT, with **no
    unimplemented opcode**. Imports are still faked (return 0), so it doesn't
    print the user *yet*. 12 emu host tests pass.
  - **Next: service imports for real** — route the IAT traps (GetStdHandle,
    WriteFile/WriteConsoleW, OpenProcessToken, GetTokenInformation,
    LookupAccountSidW, …) to the kernel's implementations so `whoami` actually
    prints `nanokrnl\user`. Then wire the interpreter into the kernel's `run`
    path so it's invocable from the `C:\>` shell.
- [ ] **B1 — usermode core.** Implement the ALU/mov/stack/jump/string/SSE2 subset
  the MSVC CRT + our shims use; wire `syscall` → existing SSDT. Milestone: a
  tiny own-ABI program, then **`whoami` runs in the browser**.
- [ ] **B2 — ring-0 / software drivers.** Reuse the driver loader; resolve driver
  imports to native ntoskrnl exports; model/no-op privileged ops. Milestone:
  **null.sys DriverEntry → IRP → unload** in the browser (no hardware needed).
- [ ] **B3 (stretch) — browser-backed virtual devices.** Give an emulated driver
  a synthetic device the page provides — e.g. a framebuffer driver → `<canvas>`,
  a NIC → WebSocket. This is the genuinely novel "drivers in the browser" story;
  it needs a minimal interrupt/DPC delivery path on top of B2.

### Boundary (explicitly out of scope)

Real hardware drivers that drive physical ports/MMIO/IRQs need full PC-platform
+ device emulation (qemu/v86 scale). We do synthetic/browser-backed devices
only. No real-mode/BIOS, no SMM, no paging.

### Decisions / constraints

- No qemu-wasm / no x86 emulation **by import** — but our *own* minimal usermode+
  software-driver interpreter (Track B) is in scope if pursued.
- Target `wasm32-unknown-unknown` (no WASI dependency; host imports for I/O).
- Keep the x86 build fully working throughout (the WASM port is additive).

---

## Status — kernel (x86 build)

Working today: interactive `cmd.exe`, `echo`, `exit`, `dir`, `where`, `sort`,
`choice`, **`whoami`** (prints `nanokrnl\user`), real `null.sys` driver. Default
self-test suite passes (exit 33).

`more.com` (ulib/C++): **working** — `more readme.txt` prints the file. Required
running ulib's `DllMain` (per-process trampoline), a batch of CRT/console
functions, and finally file mapping (`CreateFileMappingW`/`MapViewOfFile`) +
`RtlIsTextUnicode`. Commits: 4657bab (per-process command line), 7cc5960 (ulib
DllMain + CRT/console surface), 47047aa (file mapping + RtlIsTextUnicode).

## Log

### 2026-06-16 (cont.) — Track B kicked off
- Scoped the x86-64 emulator track (run the real binaries + drivers in WASM via
  our own usermode/ring-0 interpreter, no platform emulation — leverage the
  existing kernel syscall layer, ntoskrnl exports, PE loader, WDM ABI).
- B0 started: `wasm/emu` (`x86emu`) — decoder spine + register file + flags +
  step/run loop that traps unknown opcodes; first opcodes (mov-imm, reg/reg
  add/sub/mov) with flags; 3 host unit tests pass.

### 2026-06-16
- Set the WASM-port goal; wrote this plan. Assessed the hardware surface: ~16
  files use `asm!`/ports/cr3/msr (all under `hal/`, `mm/virt`, `ke/`); the NT
  subsystems (`rtl`, `ob`, `cm`, `io/ramfs`, `ex`, much of `ps`) are portable.
- Phase 0 done: `wasm/` crate + browser/Node host + proof-of-life. The kernel's
  mm/ob/rtl miniatures boot and self-test in a WASM host (verified under Node,
  exit 0).
- Phase 1 started: replaced the rtl miniature with the kernel's **real** `rtl`
  module (`#[path]` include of `kernel/src/rtl/mod.rs`) — it's hardware-free, so
  it builds for wasm32 unchanged. WASM self tests now exercise the real
  `NtStatus` + `RtlBitmap`. Verified (exit 0).
- Phase 1 cont.: brought in the **real `ob`** (object manager + handle table)
  too, with the first HAL shims — `mm::pool` (arena) and `ke::spinlock`
  (single-threaded). The browser kernel now does real reference-counted object
  lifetimes (create → handle → close → delete procedure). Verified (exit 0).
- **Interactive console**: the WASM kernel now takes typed commands and runs them
  against the real subsystems (`mkobj`/`handles`/`close` drive real `ob` objects;
  `mem` reports pool use; `ver`/`echo`/`help`/`cls`). Event-driven input via
  `kernel_input`. Browser (input field) + Node (readline / scriptable) hosts.
- **Running executables (guest WASM)**: added `whoami` (built-in) and `run <prog>`
  — the latter loads a separate guest `.wasm` and runs it, bridging its
  `sys_print` syscall to the console. Sample: `wasm/programs/hello`. Verified
  `run hello` (prints via syscall, exit 0) and `run nope` (not found). This is
  the "give it an executable" path within WASM (x86 PE binaries need emulation,
  which is out of scope).
