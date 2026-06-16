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
- [ ] **Phase 1 — HAL boundary.** Introduce a `hal` trait/cfg seam so the kernel
  crate builds for both `x86_64-unknown-none` and `wasm32`. `#[cfg]`-gate the
  x86 asm; provide wasm stubs (serial→JS, timer→JS, ports→panic/no-op).
- [ ] **Phase 2 — Memory.** Software phys allocator over a large static/linear
  arena; replace `mm/virt` page-table mapping with a flat software model
  (identity or a translation table) so `mm` APIs work without an MMU.
- [ ] **Phase 3 — Scheduler.** Cooperative run-to-completion / green-thread model
  replacing register context switches; IRQL/spinlocks become no-ops
  (single-threaded host).
- [ ] **Phase 4 — Boot path & self-tests.** Run the hardware-independent self
  tests (pool stress, ob, cm, ramfs) from `kernel_main()` in the browser and
  report pass/fail to the page.
- [ ] **Phase 5 (stretch) — user code.** Without x86 execution, "user programs"
  can't be the real PE binaries. Option: run WASM-compiled task modules against
  the NT syscall surface, or a tiny bytecode interpreter. Scoped later.

### Decisions / constraints

- No qemu-wasm / no x86 emulation (per goal).
- Target `wasm32-unknown-unknown` (no WASI dependency; host imports for I/O).
- Keep the x86 build fully working throughout (the WASM port is additive).

---

## Status — kernel (x86 build)

Working today: interactive `cmd.exe`, `echo`, `exit`, `dir`, `where`, `sort`,
`choice`, **`whoami`** (prints `nanokrnl\user`), real `null.sys` driver. Default
self-test suite passes (exit 33).

`more.com` (ulib/C++): now runs through ulib init (its `DllMain` is invoked via a
per-process trampoline) and reaches the file-display stage; remaining gaps are
`CreateFileMappingW`/`MapViewOfFile` (it memory-maps the file to read) and
`RtlIsTextUnicode`. Commits: 4657bab (per-process command line), 7cc5960 (ulib
DllMain + CRT/console surface).

## Log

### 2026-06-16
- Set the WASM-port goal; wrote this plan. Assessed the hardware surface: ~16
  files use `asm!`/ports/cr3/msr (all under `hal/`, `mm/virt`, `ke/`); the NT
  subsystems (`rtl`, `ob`, `cm`, `io/ramfs`, `ex`, much of `ps`) are portable.
- Phase 0 done: `wasm/` crate + browser/Node host + proof-of-life. The kernel's
  mm/ob/rtl miniatures boot and self-test in a WASM host (verified under Node,
  exit 0). Next: Phase 1 — a `hal` cfg/trait seam so the real kernel crate builds
  for wasm32, starting by reusing the actual `rtl` and `ob` modules.
