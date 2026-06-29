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
- [x] **M5b — bootloader handoff.** `Machine::boot_kernel`: high-half load +
  `R_X86_64_RELATIVE` relocations, page tables (kernel + stack + 4 GiB physical
  window), and a byte-exact `bootloader_api` 0.11.15 `BootInfo` (hand-encoded for
  the x86-64 layout, in `bootinfo.rs`), entered at `_start` with the pointer in
  RDI. **The real kernel boots.**
- [x] **M6 — wasm32 packaging.** `cdylib` + `no_std` runtime (panic handler +
  bump allocator) in `wasm.rs`; a pointer-free C ABI; JS shim in `web/ntemu/`
  that fetches + boots the staged kernel ELF. 51 KB wasm, verified booting the
  real kernel in a WASM runtime.
- [x] **M7 — opcode tail (boot-complete).** Trace-driven via
  `StepResult::Unknown`: added 8-bit ALU forms, `grp1`-8bit, REP string ops,
  `cpuid`, `imul`, `bsf/bsr`, `bt*`, `cmpxchg`, `xadd`, segment-register moves,
  `ltr`/`lgdt`/`lidt`, far-return/`iretq`, x2APIC+xAPIC. Enough for a full boot
  to the idle loop; a richer guest (shell, user processes) will surface more.

- [x] **M8 — preemptive scheduling.** APIC ICR self-IPI + a 256-bit IRR + the
  `v>>4 > CR8` delivery rule (CR8/IRQL now stored). The clock ISR's dispatch
  self-IPI fires once IRQL drops → the scheduler context-switches. The kernel's
  boot self-tests run and pass: Cpu (SMEP/SMAP), Mm (15), Ke (10), Io, Um
  (ring-3 program via NtWriteFile), Ob, Cm (registry).
- [ ] **M9 — process/driver loading.** The suite panics in `ldr/pe.rs` during
  the Ps (CreateProcess) / Ldr (PE driver) phase — a data divergence in the
  PE-mapping path. Gates "ALL SELF TESTS PASSED" and the interactive `cmd` shell.

## Status
The **real ntoskrnl-rs kernel boots and runs** under ntemu — native and in the
~52 KB wasm — through both init phases, then runs its self-test suite under a
live preemptive scheduler, passing dozens of Cpu/Mm/Ke/Io/Um/Ob/Cm checks
(including a ring-3 user program). It does **not** yet complete the suite: it
panics in the PE loader during CreateProcess (M9). `cargo test` → **33 passing**.

Correctness fixes found by booting/running: RIP-relative immediate length;
operand-size (0x66) immediates across mov/alu/test (`mov r16,imm16`, `0xC7`,
`0x81`, acc-imm, `0xA9`, `0xF7`); `pc`/addresses `u64` not `usize` (wasm32);
APIC self-IPI + CR8/IRQL-gated delivery (preemption); SMEP/SMAP CPUID +
STAC/CLAC.
