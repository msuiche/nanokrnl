# ntemu — bespoke x86-64 full-machine emulator

Goal: boot the unmodified ntoskrnl-rs kernel in the browser as a **lighter
alternative to qemu-wasm**. qemu-wasm emulates a generic PC (real mode → SeaBIOS
→ bootloader → long mode) and ships a large wasm with COOP/COEP + threads. ntemu
emulates **only what our kernel needs**, starting the CPU already in long mode.

## Why not v86 / TinyEMU
- **v86**: open, but its CPU + WASM JIT are 32-bit to the bone (registers are
  `8 × i32`, EIP/CR are `i32`, paging tops out at PAE, decode tables have no
  REX). "64-bit kernels are not supported" (its Readme, issue #648). Retrofitting
  long mode means rewriting the JIT — months.
- **TinyEMU**: released source has *no software x86 core* (RISC-V only; x86 is
  KVM). Bellard's browser x86 core is 64-bit and runs Windows NT but is closed.
- So: reuse **our own** proven interpreter, add long mode + a few devices.

## Architecture decisions
- **Boot directly in long mode.** We control the bootloader, so skip real mode +
  SeaBIOS entirely: load the kernel image into guest physical memory, set up an
  initial PML4 / GDT / control regs, and start at the kernel entry in 64-bit
  mode. Removes the single biggest emulation surface.
- **Minimal device set** (what the kernel actually touches, per WORKFLOW.md):
  16550 UART (COM1, the console), Local APIC timer (vector 0xD1), PS/2 keyboard
  (interactive input). No IDE/VGA/PCI/virtio.
- **Interpreter, no JIT** (initially). Console workload is light; correctness
  first. JIT is a far-future optimization.
- **Self-contained Rust → wasm32 + thin JS terminal shim.** No v86 JS layer (its
  device bus assumes the 32-bit CPU ABI); we own a tiny shim instead.

## Milestones
- [x] **M0 — execution-core seed.** Restore the proven usermode interpreter
  (REX/ModRM/SIB, RIP-relative, ALU/stack/control-flow) as the `ntemu` crate.
  *12 tests green.*
- [x] **M1 — long-mode MMU.** 4-level PML4 walk over flat physical memory, gated
  by CR0.PG/CR4.PAE/EFER.LMA; 1G/2M large pages; #PF error codes; canonical
  check. Standalone + unit-tested (`src/mmu.rs`). *7 tests green.*
- [x] **M2 — machine state + memory bus.** `Machine` owns physical RAM; the CPU
  gained control/MSR/IDTR/GDTR state; every operand and code fetch routes
  through `mmu::translate` with Local APIC MMIO dispatch. The seed's flat
  accessors are now translating instance methods. Gated by `machine_mode` so the
  usermode path is unchanged.
- [x] **M3 — long-mode bring-up + faults.** IDT-based interrupt/exception
  delivery (gate decode, stack switch, frame push, IF clear), `iretq`,
  `swapgs`, architectural `syscall`/`sysret` against real MSRs, `mov` CR,
  `rd/wrmsr`, `cli`/`sti`, `hlt`, `int n`. (#PF error-code reconstruction is
  best-effort — refine later.)
- [x] **M4 — devices.** 16550 UART (port I/O), Local APIC + timer (MMIO, vector
  0xD1), PS/2 keyboard — all unit-tested.
- [x] **M5 — ELF loader.** `elf.rs` parses ELF64 and `Machine::load_elf` places
  `PT_LOAD` segments. The real kernel ELF loads (entry `0x1ba260`, ~2.3 MiB).
- [ ] **M5b — bootloader handoff.** Apply `R_X86_64_RELATIVE` relocations and
  synthesize the `bootloader_api` `BootInfo` (+ memory map / physical-memory
  offset) so `kernel_main` runs. *This is the gate to a real kernel boot.*
- [x] **M6 — wasm32 packaging.** `cdylib` + `no_std` runtime (panic handler +
  bump allocator) in `wasm.rs`; a pointer-free C ABI; JS shim in `web/ntemu/`.
  38 KB wasm, verified to boot+print+take-an-interrupt in a real WASM runtime.
- [ ] **M7 — close opcode gaps.** Trace-driven via `StepResult::Unknown`: the
  full kernel + CRT will surface string ops, FPU, more SSE, `bsf/bsr`, etc.

## Status
M0–M6 complete and tested (`cargo test` → **32 passing**; wasm verified in
Node). The path to a real in-browser kernel boot is **M5b** (relocations +
`BootInfo` handoff) then **M7** (opcode tail). See `SPEC.md` at the repo root for
the full specification and evidence.
