//! Full-program **lockstep** differential. Boots the interactive kernel, types a
//! command, and for every ring-3 instruction runs that exact instruction through
//! Unicorn (QEMU's CPU core) from nanox's *real* runtime register + memory state,
//! then diffs the post-state. Unlike `diff_unicorn` (random states, isolated
//! instructions) this replays the genuine execution, so it catches data-dependent
//! bugs like a wrong branch deep inside a real binary.
//!
//!   cargo run --release --example diff_trace --features oracle -- "more readme.txt"
//!
//! Memory is mirrored lazily: Unicorn runs flat (no paging) and on any unmapped
//! access we translate that VA through nanox's page tables and copy the 4 KiB
//! frame in. AF is excluded (nanox doesn't model it).

use nanox::machine::Machine;
use nanox::mmu::{self, Access, Paging};
use nanox::StepResult;
use unicorn_engine::unicorn_const::{Arch, HookType, MemType, Mode, Prot};
use unicorn_engine::{RegisterX86, Unicorn};

const FLAG_MASK: u64 = (1 << 0) | (1 << 2) | (1 << 6) | (1 << 7) | (1 << 11) | (1 << 10);
const UREGS: [RegisterX86; 16] = [
    RegisterX86::RAX, RegisterX86::RCX, RegisterX86::RDX, RegisterX86::RBX,
    RegisterX86::RSP, RegisterX86::RBP, RegisterX86::RSI, RegisterX86::RDI,
    RegisterX86::R8, RegisterX86::R9, RegisterX86::R10, RegisterX86::R11,
    RegisterX86::R12, RegisterX86::R13, RegisterX86::R14, RegisterX86::R15,
];

#[derive(Clone, Copy)]
struct Ctx { ram: *const u8, len: usize, paging: Paging, user: bool, fs: u64, gs: u64 }

/// Read up to `n` bytes of guest code at VA `va` (for opcode inspection).
fn read_code(m: &Machine, va: u64, n: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(n);
    for i in 0..n as u64 {
        match mmu::translate(&m.ram, &m.cpu.paging, va + i, Access::Read, m.cpu.cpl == 3) {
            Ok(p) => out.push(*m.ram.get(p as usize).unwrap_or(&0)),
            Err(_) => break,
        }
    }
    out
}

/// Should we skip oracle comparison for this instruction? System instructions
/// (syscall/sysret/cpuid/rd-wrmsr/rdtsc/int/iret/hlt/swapgs) depend on CPU state
/// Unicorn doesn't share here, so their post-state would differ spuriously.
fn skip(bytes: &[u8]) -> bool {
    let mut i = 0;
    let mut rep = false;
    // skip legacy + REX prefixes
    while i < bytes.len() {
        match bytes[i] {
            0xF2 | 0xF3 => { rep = true; i += 1; }
            0x66 | 0x67 | 0xF0 | 0x2E | 0x36 | 0x3E | 0x26 | 0x64 | 0x65 => i += 1,
            0x40..=0x4F => i += 1,
            _ => break,
        }
    }
    if i >= bytes.len() { return true; }
    // REP/REPNE string ops: nanox retires the whole rep in one step while Unicorn
    // (count=1) runs a single iteration — same final state, different per-step.
    if rep && matches!(bytes[i], 0xA4..=0xA7 | 0xAA..=0xAF) { return true; }
    match bytes[i] {
        0xF4 => true,                 // hlt
        0xCD | 0xCC | 0xCE => true,   // int n / int3 / into
        0xCF => true,                 // iret
        0xE4..=0xE7 | 0xEC..=0xEF => true, // in/out
        0x0F => matches!(bytes.get(i + 1),
            Some(0x05) |              // syscall
            Some(0x07) |              // sysret
            Some(0x01) |              // group: swapgs/wrmsr-ish/invlpg/etc
            Some(0x06) |              // clts
            Some(0x30) | Some(0x31) | Some(0x32) | Some(0x33) | // wrmsr/rdtsc/rdmsr/rdpmc
            Some(0xA2) |              // cpuid
            Some(0x09) |              // wbinvd
            Some(0x20) | Some(0x22) | Some(0x21) | Some(0x23)), // mov cr/dr
        _ => false,
    }
}

/// Which flag bits to compare for this instruction. nanox intentionally doesn't
/// model some architecturally-undefined or not-yet-implemented flags: OF after a
/// multi-bit shift/rotate is undefined, and nanox leaves CF/OF unmodeled for
/// mul/imul. Drop those bits so they don't mask a real functional divergence.
fn flag_mask_for(bytes: &[u8]) -> u64 {
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            0x66 | 0x67 | 0xF0 | 0xF2 | 0xF3 | 0x2E | 0x36 | 0x3E | 0x26 | 0x64 | 0x65 | 0x40..=0x4F => i += 1,
            _ => break,
        }
    }
    let op = bytes.get(i).copied().unwrap_or(0);
    let of = 1 << 11;
    let cf = 1 << 0;
    match op {
        // shift/rotate group — OF undefined for count != 1
        0xC0 | 0xC1 | 0xD0 | 0xD1 | 0xD2 | 0xD3 => FLAG_MASK & !of,
        // mul/imul (F6/F7 are also test/not/neg/div, but those set flags nanox
        // does model; only /4 mul /5 imul leave CF/OF — cheap to just relax CF/OF)
        0xF6 | 0xF7 => FLAG_MASK & !of & !cf,
        0x69 | 0x6B => FLAG_MASK & !of & !cf, // imul r, r/m, imm
        0x0F if bytes.get(i + 1) == Some(&0xAF) => FLAG_MASK & !of & !cf, // imul r, r/m
        _ => FLAG_MASK,
    }
}

fn run_one(ctx: Ctx, regs: &[u64; 16], rflags: u64, rip: u64) -> Option<([u64; 16], u64)> {
    let mut uc = Unicorn::new_with_data(Arch::X86, Mode::MODE_64, ctx).ok()?;
    let mirror = |uc: &mut Unicorn<Ctx>, _t: MemType, addr: u64, _sz: usize, _v: i64| -> bool {
        let c = *uc.get_data();
        let page = addr & !0xFFF;
        let ram = unsafe { core::slice::from_raw_parts(c.ram, c.len) };
        let phys = mmu::translate(ram, &c.paging, page, Access::Read, c.user).unwrap_or(page);
        let frame = (phys & !0xFFF) as usize;
        if frame + 0x1000 > c.len { return false; }
        if uc.mem_map(page, 0x1000, Prot::ALL).is_err() { return false; }
        uc.mem_write(page, &ram[frame..frame + 0x1000]).is_ok()
    };
    uc.add_mem_hook(HookType::MEM_READ_UNMAPPED, 0, u64::MAX, mirror).ok()?;
    uc.add_mem_hook(HookType::MEM_WRITE_UNMAPPED, 0, u64::MAX, mirror).ok()?;
    uc.add_mem_hook(HookType::MEM_FETCH_UNMAPPED, 0, u64::MAX, mirror).ok()?;
    for (i, &r) in regs.iter().enumerate() { uc.reg_write(UREGS[i], r).ok()?; }
    uc.reg_write(RegisterX86::RFLAGS, rflags | 0x2).ok()?;
    uc.reg_write(RegisterX86::FS_BASE, ctx.fs).ok()?;
    uc.reg_write(RegisterX86::GS_BASE, ctx.gs).ok()?;
    uc.reg_write(RegisterX86::RIP, rip).ok()?;
    uc.emu_start(rip, 0, 0, 1).ok()?;
    let mut out = [0u64; 16];
    for (i, s) in out.iter_mut().enumerate() { *s = uc.reg_read(UREGS[i]).ok()?; }
    let fl = uc.reg_read(RegisterX86::RFLAGS).ok()?;
    Some((out, fl))
}

fn main() {
    let cmd = std::env::args().nth(1).unwrap_or_else(|| "more readme.txt".to_string());
    let image = std::fs::read("../target/x86_64-unknown-none/debug/kernel").expect("kernel");
    let mut m = Machine::new(128 * 1024 * 1024);
    m.boot_kernel(&image).expect("boot");
    let mut out = String::new();
    for _ in 0..120 {
        m.run(20_000_000);
        for b in m.take_uart_output() { out.push(b as char); }
        if out.contains("C:\\>") { break; }
    }
    eprintln!("at prompt; typing {:?}", cmd);
    for &b in cmd.as_bytes() { m.cpu.dev.uart.push_rx(b); }
    m.cpu.dev.uart.push_rx(b'\r');

    let mut checked = 0u64;
    let mut steps = 0u64;
    let budget = 8u64 * 20_000_000;
    let mut diverged = 0;
    // Track syscall depth so we only oracle-check kernel code that is actually
    // servicing a syscall — not the idle/scheduler/timer loop (millions of insns).
    let mut in_syscall: i32 = 0;
    let check_kernel = std::env::var("ORACLE_KERNEL").is_ok();
    while steps < budget {
        steps += 1;
        m.cpu.dev.apic.tick(1);
        m.service_pending_irq();

        let user = m.cpu.cpl == 3;
        let rip = m.cpu.rip;
        let pre_regs = m.cpu.regs;
        let pre_rflags = m.cpu.rflags;
        let bytes = read_code(&m, rip, 15);
        // Maintain syscall depth (0F 05 = syscall enters kernel, 0F 07 = sysret).
        let op0 = bytes.first().copied().unwrap_or(0);
        let op1 = bytes.get(1).copied().unwrap_or(0);
        let is_syscall = op0 == 0x0F && op1 == 0x05;
        let is_sysret = op0 == 0x0F && op1 == 0x07;
        // Skip instructions that likely touch device MMIO (APIC at 0xFEE0_0000):
        // Unicorn treats those as plain memory, nanox as a device — a false diff.
        let near_mmio = pre_regs.iter().any(|&r| (0xFEE0_0000..0xFEE0_1000).contains(&r));
        // Compare ring-3 always (fast, the common case). Ring-0 kernel code is
        // only checked while inside a syscall handler, and only when explicitly
        // requested (ORACLE_KERNEL=1) since spawning a Unicorn per kernel
        // instruction is slow. Idle/scheduler/timer code is never compared.
        let in_scope = user || (check_kernel && in_syscall > 0);
        let want_oracle = in_scope && !bytes.is_empty() && !skip(&bytes) && !near_mmio;
        let ctx = Ctx { ram: m.ram.as_ptr(), len: m.ram.len(), paging: m.cpu.paging, user,
                        fs: m.cpu.fs_base, gs: m.cpu.gs_base };
        let oracle = if want_oracle { run_one(ctx, &pre_regs, pre_rflags, rip) } else { None };
        let fmask = flag_mask_for(&bytes);

        match m.cpu.step(&mut m.ram) {
            StepResult::Ok => {
                if is_syscall { in_syscall += 1; }
                if is_sysret && in_syscall > 0 { in_syscall -= 1; }
                if let Some((ur, uf)) = oracle {
                    checked += 1;
                    let regs_ok = ur == m.cpu.regs;
                    let flags_ok = (uf & fmask) == (m.cpu.rflags & fmask);
                    if !regs_ok || !flags_ok {
                        let hex: String = bytes.iter().take(12).map(|b| format!("{:02x}", b)).collect::<Vec<_>>().join(" ");
                        eprintln!("\nDIVERGENCE @ rip={:#x} cpl{}  [{}]", rip, if user {3} else {0}, hex);
                        if !regs_ok {
                            for k in 0..16 {
                                if ur[k] != m.cpu.regs[k] {
                                    eprintln!("  {:?}: uc={:#x}  nt={:#x}  (pre={:#x})", UREGS[k], ur[k], m.cpu.regs[k], pre_regs[k]);
                                }
                            }
                        }
                        if !flags_ok {
                            eprintln!("  FLAGS uc={:#06x} nt={:#06x} (masked {:#06x})", uf & fmask, m.cpu.rflags & fmask, fmask);
                        }
                        diverged += 1;
                        if diverged >= 12 { eprintln!("...stopping after {} divergences", diverged); break; }
                    }
                }
            }
            StepResult::Hlt => {
                if m.cpu.rflags & (1 << 9) != 0 {
                    if m.service_pending_irq() { continue; }
                    if m.cpu.dev.apic.expire().is_some() && m.service_pending_irq() { continue; }
                }
                break;
            }
            StepResult::Fault { addr } => {
                m.cpu.cr2 = addr;
                if !m.cpu.deliver_interrupt(&mut m.ram, 14, Some(mmu::PageFault::P as u64)) {
                    eprintln!("unhandled fault @ {:#x}", addr);
                    break;
                }
            }
            other => { eprintln!("stop: {:?}", other); break; }
        }
        for b in m.take_uart_output() { out.push(b as char); }
    }
    eprintln!("\nchecked {} ring-3 instructions, {} divergences", checked, diverged);
    eprintln!("tail output: {:?}", &out[out.len().saturating_sub(120)..]);
}
