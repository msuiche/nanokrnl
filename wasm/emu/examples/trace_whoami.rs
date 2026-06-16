//! Trace harness: load the real whoami.exe, run it through the interpreter, and
//! report where it stops (the next opcode to implement, a syscall, or a fault).
//! Drives B1 opcode coverage the same evidence-driven way we cracked the native
//! binaries. Run: `cargo run --example trace_whoami` from wasm/emu.
use x86emu::pe::{import_name, load_pe};
use x86emu::{Cpu, StepResult};

/// Pop the return address into rip and set the return value (rax) — used to
/// "service" an import or syscall by faking a function that returns `ret`.
fn fake_return(cpu: &mut Cpu, mem: &[u8], ret: u64) {
    let rsp = cpu.regs[4] as usize;
    let target = u64::from_le_bytes(mem[rsp..rsp + 8].try_into().unwrap());
    cpu.regs[4] += 8;
    cpu.rip = target;
    cpu.regs[0] = ret; // rax
}

fn main() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../winbin/whoami.exe");
    let data = match std::fs::read(path) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("cannot read {path}: {e}");
            return;
        }
    };
    let mut mem = vec![0u8; 64 * 1024 * 1024];
    let loaded = load_pe(&data, &mut mem).expect("load whoami.exe");
    println!("loaded whoami.exe: entry={:#x} image_size={:#x}", loaded.entry, loaded.image_size);

    let mut cpu = Cpu::new();
    let stack_top = mem.len() as u64 - 0x1000;

    // Minimal TEB/PEB so gs:[...] reads work (self, stack bounds, PEB, TLS).
    let teb = 0x0050_0000u64;
    let peb = 0x0052_0000u64;
    let tls = 0x0053_0000u64;
    let w64 = |mem: &mut [u8], at: u64, v: u64| {
        let a = at as usize;
        mem[a..a + 8].copy_from_slice(&v.to_le_bytes());
    };
    w64(&mut mem, teb + 0x08, stack_top); // NtTib.StackBase
    w64(&mut mem, teb + 0x10, stack_top - 0x100000); // NtTib.StackLimit
    w64(&mut mem, teb + 0x30, teb); // NtTib.Self
    w64(&mut mem, teb + 0x58, tls); // ThreadLocalStoragePointer
    w64(&mut mem, teb + 0x60, peb); // ProcessEnvironmentBlock
    cpu.gs_base = teb;

    cpu.setup_frame(&mut mem, loaded.entry, stack_top);

    let max = 200_000usize;
    let mut imports_seen = 0;
    for i in 0..max {
        match cpu.step(&mut mem) {
            StepResult::Ok => {}
            StepResult::Syscall => {
                println!("[{i}] syscall eax={}", cpu.regs[0] & 0xffffffff);
                // no service yet; treat as returning 0
            }
            StepResult::Import { index } => {
                let (dll, name) = import_name(&data, index).unwrap_or(("?", "?"));
                if imports_seen < 40 {
                    println!("[{i}] import #{index}: {dll}!{name}");
                    imports_seen += 1;
                }
                // Fake the import returning 0 so we can keep tracing past it.
                fake_return(&mut cpu, &mem, 0);
            }
            StepResult::Halt => {
                println!("[{i}] HALT (program returned to entry caller)");
                return;
            }
            StepResult::Unknown { rip, byte } => {
                println!("[{i}] UNKNOWN opcode {byte:#04x} at rip={rip:#x}");
                dump(&mem, rip);
                return;
            }
            StepResult::Fault { addr } => {
                println!("[{i}] FAULT accessing {addr:#x} at rip={:#x}", cpu.rip);
                dump(&mem, cpu.rip);
                return;
            }
        }
    }
    println!("ran {max} steps without stopping");
}

fn dump(mem: &[u8], rip: u64) {
    let r = rip as usize;
    let end = (r + 16).min(mem.len());
    print!("bytes @rip:");
    for b in &mem[r..end] {
        print!(" {b:02x}");
    }
    println!();
}
