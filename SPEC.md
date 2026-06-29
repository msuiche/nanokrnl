# ntemu — bespoke x86-64 emulator specification

`ntemu` (crate in `emu/`) is a hand-written x86-64 **full-machine** emulator
whose purpose is to boot the unmodified ntoskrnl-rs kernel **in the browser** as
a lighter alternative to qemu-wasm. This document specifies what it is, how it is
built, what it implements, how it is tested, and what remains.

---

## 1. Why this exists

The browser route for booting our 64-bit kernel was qemu-wasm: it works, but it
emulates a whole generic PC (real mode → SeaBIOS → bootloader → long mode) and
ships a multi-megabyte wasm that needs threads, `SharedArrayBuffer`, and
COOP/COEP headers. The alternatives don't fit:

- **v86** — open, but its CPU and WebAssembly JIT are 32-bit to the bone:
  registers are stored as `8 × i32`, EIP/CR are `i32`, paging tops out at PAE,
  and the decode tables have no REX. Its own Readme states "64-bit kernels are
  not supported" (issue #648). Adding long mode means rewriting the JIT.
- **TinyEMU** — the released MIT source contains *no software x86 core* (RISC-V
  only; its x86 is KVM-backed). Bellard's browser x86 core *is* 64-bit and runs
  Windows NT, but it is closed source.

`ntemu` instead emulates **only what our kernel needs** and **boots directly in
long mode** (we own the bootloader, so real mode and BIOS are skipped entirely).
The result is a ~38 KB wasm with **no threads, no SharedArrayBuffer, no
COOP/COEP** — two orders of magnitude smaller than qemu-wasm.

---

## 2. Design

### 2.1 Scope decisions
- **Boot directly in long mode.** `Machine::boot_long_mode` installs identity
  page tables, an IDT, and the control registers (CR0.PG, CR4.PAE,
  EFER.LME/LMA) of an already-paged 64-bit machine, then jumps to an entry
  point. No reset vector, no real mode, no BIOS.
- **Minimal device set** — only what the kernel touches:
  - **16550 UART** (COM1, ports `0x3F8..0x3FF`) — the console.
  - **Local APIC + timer** (MMIO at `0xFEE00000`) — the scheduler tick on
    vector `0xD1`.
  - **PS/2 keyboard** (ports `0x60`/`0x64`) — interactive input.
- **Interpreter, no JIT.** A console workload doesn't need JIT speed;
  correctness first.
- **Self-contained Rust → wasm + a thin JS shim.** We reuse neither v86's CPU
  (32-bit) nor its JS device bus (assumes the 32-bit CPU ABI).

### 2.2 Components (`emu/src/`)
| Module | Responsibility |
|---|---|
| `lib.rs` | The CPU: register file, instruction decode/execute (REX/ModRM/SIB, RIP-relative), ALU/flags, the translated memory path, and the long-mode system instructions. |
| `mmu.rs` | IA-32e 4-level paging (PML4→PDPT→PD→PT) with 1 GiB / 2 MiB large pages, permission + canonical checks, and architectural #PF error codes. |
| `devices.rs` | UART, Local APIC (+timer), PS/2, with port-I/O and MMIO dispatch. |
| `machine.rs` | Physical RAM + a CPU + devices; long-mode bring-up, the run loop (timer ticks, interrupt + page-fault delivery), and the ELF loader entry. |
| `elf.rs` | Minimal ELF64 loader (`PT_LOAD` segments + entry point). |
| `pe.rs` | PE loader inherited from the seed (usermode path). |
| `wasm.rs` | `no_std` runtime (panic handler + bump allocator) and the exported C ABI for the browser. Compiled only for `wasm32`. |

### 2.3 Lineage
The instruction core is grown from the project's earlier usermode x86-64
interpreter (git `c32cc31`, ~1,434 lines: REX/ModRM/SIB decode, RIP-relative
addressing, ALU/stack/control-flow). A `machine_mode` flag separates the two
behaviours: off (default) = the original usermode interpreter (flat memory,
host-serviced syscalls, IAT import traps); on = full machine (paged memory,
architectural `syscall`/`sysret`, interrupts). All original usermode tests still
pass unchanged.

---

## 3. CPU feature coverage

**Implemented and tested.**
- Long mode 64-bit GPRs (RAX..R15) via REX; 8/16/32/64-bit operands; XMM moves
  + `xorps`/`pxor`.
- Integer ALU (add/or/adc/sbb/and/sub/xor/cmp), inc/dec/neg/not, mul/imul/
  div/idiv, shifts/rotates, bit-test group, cmpxchg, movzx/movsx/movsxd,
  cbw/cwde/cdqe, setcc/cmovcc, lea, test, xchg.
- Control flow: call/ret, jmp/jcc (rel8/rel32), indirect call/jmp/push (group 5).
- Memory through the MMU: every operand and code fetch translates virtual →
  physical (identity below long mode), with Local APIC MMIO dispatch.
- System / long mode: `mov` to/from CR0/CR2/CR3/CR4, `rdmsr`/`wrmsr` (EFER,
  STAR/LSTAR/SFMASK, FS/GS/KernelGSBase), `swapgs`, `lgdt`/`lidt`, `cli`/`sti`,
  `hlt`, `in`/`out`, `int n`, `iretq`, architectural `syscall`/`sysret`,
  `rdtsc`, fences/`clts`/`wbinvd` as no-ops.
- Interrupt/exception delivery through the long-mode IDT: 16-byte gate decode,
  privilege-based stack switch (TSS.RSP0), frame push (SS/RSP/RFLAGS/CS/RIP
  [+error]), IF clear on interrupt gates.

**Not yet implemented** (grows trace-driven; the interpreter returns
`Unknown { rip, byte }` naming the next opcode):
- The long tail of opcodes a full kernel + CRT exercise (string ops `rep
  movs/stos`, full FPU, the rest of SSE/AVX, `bsf/bsr/popcnt`, etc.).
- A real GDT/segment-descriptor cache (selectors are synthesized by convention:
  ring0 `0x08`/`0x10`, ring3 `0x33`/`0x2B`).
- Precise #PF error-code reconstruction in the run loop (currently best-effort).

---

## 4. Browser ABI (`wasm.rs`)

A pointer-free C ABI over a single global machine; the JS shim
(`web/ntemu/index.html`) drives it.

| Export | Meaning |
|---|---|
| `ntemu_new(ram_mb)` | Create the machine with N MiB of RAM. |
| `ntemu_image_alloc(len) -> ptr` | Reserve an image buffer; JS writes bytes into wasm memory at `ptr`. |
| `ntemu_boot_elf(rsp) -> entry` | Parse the staged image as ELF, load it, boot long mode at its entry. |
| `ntemu_boot(entry, rsp)` | Boot long mode at a raw entry. |
| `ntemu_set_idt_gate(vector, handler)` | Install a 64-bit interrupt gate. |
| `ntemu_run(steps) -> code` | Run; `0` halted, `1` max-steps, `2` unknown-opcode, `3` unhandled-fault, `4` syscall-trap. |
| `ntemu_uart_read() -> i32` | Pop a UART output byte, or `-1`. |
| `ntemu_uart_write(byte)` / `ntemu_key(scancode)` | Feed console / keyboard input. |

The wasm has **no JS imports** — it instantiates with an empty import object.

---

## 5. Building & running

```sh
# Host tests (decoder, MMU, devices, machine, ELF) — std build:
cd emu && cargo test

# Browser build (wasm32, no_std) + stage into web/ntemu/:
sh emu/build-wasm.sh
(cd web/ntemu && python3 -m http.server 8000)   # open http://localhost:8000

# Inspect / attempt to boot the real kernel ELF under ntemu:
cd emu && cargo run --example inspect_kernel -- ../target/x86_64-unknown-none/debug/kernel
```

---

## 6. Testing & evidence

**The real ntoskrnl-rs kernel boots** under ntemu (see §7) — natively
(`cargo run --example inspect_kernel`) and through the 51 KB wasm in a real
WebAssembly runtime.

`cargo test` in `emu/` runs **33 tests, all passing**:
- decoder/ALU/control-flow (the seed's 12),
- MMU: identity, 4 KiB, 2 MiB pages, not-present / RO-write / user-vs-supervisor
  / non-canonical faults (7),
- devices: UART tx/rx + LSR, APIC one-shot + periodic timer, MMIO window, PS/2,
  unknown-port float (7),
- machine: **end-to-end long-mode boot** (paging on → UART "OK" → APIC one-shot
  timer → interrupt through the IDT → handler prints → `iretq` → "D!", final
  output `OKTD!`), **syscall/sysret ring round-trip**, unmapped-high-address
  fault, **ELF load-and-run** (4),
- ELF parser (2).

**Browser end-to-end (verified in a real WebAssembly runtime):** the 38 KB
`ntemu.wasm`, driven through its JS ABI, loads a synthetic ELF, boots long mode,
prints over the UART, takes the APIC timer interrupt through an installed IDT
gate, and `iretq`s back — producing UART output `nte*ok` (the `*` is the timer
handler; `ok` is post-return). No threads / SharedArrayBuffer / COOP-COEP.

---

## 7. Booting the real kernel — WORKING

`Machine::boot_kernel` boots the **unmodified** ntoskrnl-rs ELF, on both the
native host and the wasm/browser build. It reproduces the `bootloader` crate's
handoff:

1. **High-half load + relocations.** Segments are loaded at
   `0xFFFF_8000_0000_0000 + vaddr`; the PIE's `R_X86_64_RELATIVE` relocations
   (from `PT_DYNAMIC` → `DT_RELA`) are applied for that base.
2. **Page tables.** A fresh PML4 maps the kernel image, a 256 KiB stack, the
   `BootInfo` page, and the **whole low-4 GiB physical window** at
   `0xFFFF_FF00_0000_0000` (so the kernel reaches the Local APIC via the
   physical-memory offset). The Local APIC page is intercepted as MMIO.
3. **`BootInfo`.** A byte-exact `bootloader_api` 0.11.15 `BootInfo` (hand-encoded
   for the x86-64 layout — see `src/bootinfo.rs`) with the memory map,
   `physical_memory_offset`, and kernel/stack fields. The pointer is placed in
   RDI and control transfers to `_start`.

With this, the kernel runs its full init and reaches its idle loop:

```
ntoskrnl-rs 0.1.0 (x86_64) — NT-compatible kernel in Rust
KiSystemStartup: phase 0
KE: GDT/TSS/IDT loaded (NT selector layout), KPCR online, syscall enabled, SMEP=off SMAP=off
MM: PFN bitmap @ 0xC00000 (4 KiB) — 115 MiB usable RAM
HAL: PIC masked, APIC enabled, clock on vector 0xD1 (CLOCK_LEVEL)
KiSystemStartup: phase 1
KE: scheduler online, interrupts enabled
```

After this it enters a healthy APIC-timer-driven idle loop: in a 50M-instruction
run, **134,936 `hlt` idles each woken by exactly one vector-0xD1 timer
interrupt** — i.e. the scheduler tick is live. No crashes, unknown opcodes, or
unhandled faults.

Opcodes are added **trace-driven** via the `Unknown { rip, byte }` signal; the
boot above exercises the 8-bit ALU forms, `grp1`-8bit, REP string ops, `cpuid`,
`imul`, `bsf/bsr`, `bt*`, `cmpxchg`, segment-register/`ltr`/far-return/`iret`,
x2APIC and xAPIC paths, and the full long-mode system instruction set. Booting a
guest that does more (a shell, user processes) will surface further opcodes the
same way.

### Cross-architecture note
The interpreter's program counter and all guest addresses are `u64`, never
`usize` — `usize` is 32-bit on `wasm32` and would truncate high-half kernel
addresses. The `BootInfo` is hand-encoded with explicit 64-bit fields for the
same reason (the wasm host is 32-bit; the guest is 64-bit).
