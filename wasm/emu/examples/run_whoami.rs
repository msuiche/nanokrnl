//! Run the real whoami.exe in the interpreter with its imports *serviced* (not
//! faked), so it actually produces output. The import handlers here mirror the
//! native kernel32/advapi32 shims: a single fabricated user `nanokrnl\User`,
//! token queries, and console writes that print to stdout. This is the host
//! side of "an executable runs in the WASM kernel"; the same dispatch moves into
//! the kernel later. Run: `cargo run --example run_whoami` from wasm/emu.
use x86emu::pe::{import_name, load_pe};
use x86emu::{Cpu, StepResult};

const RAX: usize = 0;
const RCX: usize = 1;
const RDX: usize = 2;
const RSP: usize = 4;
const R8: usize = 8;
const R9: usize = 9;

// A scratch heap the import handlers hand out (HeapAlloc, GetCommandLineW, …).
struct Host {
    heap: u64,
    token_user_sid: u64, // address of the fabricated SID in emu memory
}

fn rd64(mem: &[u8], a: u64) -> u64 {
    u64::from_le_bytes(mem[a as usize..a as usize + 8].try_into().unwrap())
}
fn wr64(mem: &mut [u8], a: u64, v: u64) {
    mem[a as usize..a as usize + 8].copy_from_slice(&v.to_le_bytes());
}
fn wr32(mem: &mut [u8], a: u64, v: u32) {
    mem[a as usize..a as usize + 4].copy_from_slice(&v.to_le_bytes());
}
/// Win64 stack argument N (1-based; args 5+ live at [rsp+0x28], [rsp+0x30], …).
/// At an import trap, rsp points at the pushed return address.
fn stack_arg(cpu: &Cpu, mem: &[u8], n: usize) -> u64 {
    rd64(mem, cpu.regs[RSP] + 0x20 + (n as u64) * 8)
}
fn write_utf16(mem: &mut [u8], at: u64, s: &str) -> usize {
    let mut o = at as usize;
    let mut n = 0;
    for u in s.encode_utf16() {
        mem[o..o + 2].copy_from_slice(&u.to_le_bytes());
        o += 2;
        n += 1;
    }
    mem[o..o + 2].copy_from_slice(&0u16.to_le_bytes());
    n
}
fn read_utf16(mem: &[u8], ptr: u64, nchars: usize) -> String {
    let mut units = Vec::with_capacity(nchars);
    for i in 0..nchars {
        let a = (ptr as usize) + i * 2;
        units.push(u16::from_le_bytes(mem[a..a + 2].try_into().unwrap()));
    }
    String::from_utf16_lossy(&units)
}

fn service(host: &mut Host, cpu: &mut Cpu, mem: &mut [u8], dll: &str, name: &str) -> u64 {
    let (a1, a2, a3, a4) = (cpu.regs[RCX], cpu.regs[RDX], cpu.regs[R8], cpu.regs[R9]);
    let _ = dll;
    match name {
        "GetStdHandle" => 0x100 | (a1 & 0xff), // any nonzero, distinct per stream
        "GetConsoleMode" => {
            if a2 != 0 {
                wr32(mem, a2, 3);
            }
            1
        }
        "GetFileType" => 2,          // FILE_TYPE_CHAR -> use WriteConsoleW
        "GetConsoleOutputCP" | "GetConsoleCP" => 437,
        "GetACP" => 1252,
        "GetCurrentProcess" => 0xffff_ffff_ffff_ffff,
        "GetCommandLineW" => {
            let p = host.heap;
            write_utf16(mem, p, "whoami");
            host.heap += 64;
            p
        }
        "GetProcessHeap" => 1,
        "HeapAlloc" | "HeapReAlloc" => {
            // arg3 = bytes (HeapAlloc) ; for ReAlloc arg4 = bytes.
            let bytes = if name == "HeapAlloc" { a3 } else { a4 };
            let p = host.heap;
            host.heap += (bytes + 15) & !15;
            for i in 0..bytes {
                mem[(p + i) as usize] = 0;
            }
            p
        }
        "HeapFree" | "HeapSetInformation" | "CloseHandle" => 1,
        "WriteConsoleW" => {
            // (hConsole, lpBuffer, nChars, lpWritten, reserved)
            let s = read_utf16(mem, a2, a3 as usize);
            print!("{s}");
            if a4 != 0 {
                wr32(mem, a4, a3 as u32);
            }
            1
        }
        "WriteFile" => {
            // (h, buf, nbytes, *written, ovl)
            let buf = a2 as usize;
            let n = a3 as usize;
            let bytes = &mem[buf..buf + n];
            print!("{}", String::from_utf8_lossy(bytes));
            let written = stack_arg(cpu, mem, 4); // *written is the 4th arg (r9)
            let _ = written;
            if a4 != 0 {
                wr32(mem, a4, n as u32);
            }
            1
        }
        // --- token / identity: one fabricated user, NANOKRNL\User -------------
        "OpenProcessToken" => {
            if a3 != 0 {
                wr64(mem, a3, 0x4242); // *TokenHandle
            }
            1
        }
        "GetTokenInformation" => {
            // (token, class, info, len, *retlen)
            let class = a2 as u32;
            let info = a3;
            let len = a4;
            let retlen = stack_arg(cpu, mem, 5);
            let need: u32 = match class {
                1 => 16,            // TokenUser
                2 => 8 + 3 * 16,    // TokenGroups: 3 groups
                3 => 4 + 12,        // TokenPrivileges: 1 privilege
                33 | 34 => 16,      // claim attributes (empty)
                _ => 16,
            };
            if retlen != 0 {
                wr32(mem, retlen, need);
            }
            if info == 0 || (len as u32) < need {
                return 0; // ERROR_INSUFFICIENT_BUFFER path
            }
            match class {
                1 => {
                    wr64(mem, info, host.token_user_sid);
                    wr32(mem, info + 8, 0);
                }
                3 => {
                    wr32(mem, info, 1); // PrivilegeCount
                    wr32(mem, info + 4, 23); // SeChangeNotifyPrivilege
                    wr32(mem, info + 8, 0);
                    wr32(mem, info + 12, 3); // enabled
                }
                33 | 34 => {
                    wr32(mem, info, 1); // version
                    wr32(mem, info + 4, 0); // count 0
                    wr64(mem, info + 8, 0);
                }
                _ => {
                    wr32(mem, info, 0);
                }
            }
            1
        }
        "LookupAccountSidW" => {
            // (sys, sid, name, *cchName, domain, *cchDomain, *use)
            let name_buf = a3;
            let cch_name = a4;
            let domain = stack_arg(cpu, mem, 5);
            let cch_domain = stack_arg(cpu, mem, 6);
            let use_ = stack_arg(cpu, mem, 7);
            if name_buf != 0 {
                write_utf16(mem, name_buf, "User");
            }
            if cch_name != 0 {
                wr32(mem, cch_name, 4);
            }
            if domain != 0 {
                write_utf16(mem, domain, "NANOKRNL");
            }
            if cch_domain != 0 {
                wr32(mem, cch_domain, 8);
            }
            if use_ != 0 {
                wr32(mem, use_, 1); // SidTypeUser
            }
            1
        }
        "GetUserNameExW" => 1,
        "lstrlenW" => {
            // count UTF-16 units until the NUL at a1
            let mut n = 0u64;
            loop {
                let a = (a1 + n * 2) as usize;
                if a + 2 > mem.len() || mem[a] == 0 && mem[a + 1] == 0 {
                    break;
                }
                n += 1;
            }
            n
        }
        "CharLowerW" | "CharUpperW" => {
            // In-place ASCII case fold of the NUL-terminated string at a1, which
            // is also the return value.
            let lower = name == "CharLowerW";
            let mut o = a1 as usize;
            loop {
                let u = u16::from_le_bytes(mem[o..o + 2].try_into().unwrap());
                if u == 0 {
                    break;
                }
                let f = if lower && (b'A' as u16..=b'Z' as u16).contains(&u) {
                    u + 32
                } else if !lower && (b'a' as u16..=b'z' as u16).contains(&u) {
                    u - 32
                } else {
                    u
                };
                mem[o..o + 2].copy_from_slice(&f.to_le_bytes());
                o += 2;
            }
            a1
        }
        // CRT odds and ends that must not return failure mid-startup:
        "__C_specific_handler" | "_initterm" | "__set_app_type" | "_configthreadlocale"
        | "_set_fmode" | "_setmode" | "exit" | "_cexit" | "_amsg_exit" => 0,
        _ => 0, // default: succeed-ish with 0
    }
}

fn main() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../winbin/whoami.exe");
    let data = std::fs::read(path).expect("read whoami.exe");
    let mut mem = vec![0u8; 64 * 1024 * 1024];
    let loaded = load_pe(&data, &mut mem).expect("load whoami.exe");

    // TEB/PEB so gs:[...] works.
    let (teb, peb, tls) = (0x0050_0000u64, 0x0052_0000u64, 0x0053_0000u64);
    let stack_top = mem.len() as u64 - 0x1000;
    wr64(&mut mem, teb + 0x08, stack_top);
    wr64(&mut mem, teb + 0x10, stack_top - 0x100000);
    wr64(&mut mem, teb + 0x30, teb);
    wr64(&mut mem, teb + 0x58, tls);
    wr64(&mut mem, teb + 0x60, peb);

    // Fabricated user SID S-1-5-21-1-2-3-1000 in emu memory.
    let sid = 0x0040_0000u64;
    let sid_bytes: [u8; 28] = [
        1, 5, 0, 0, 0, 0, 0, 5, 21, 0, 0, 0, 1, 0, 0, 0, 2, 0, 0, 0, 3, 0, 0, 0, 0xe8, 3, 0, 0,
    ];
    mem[sid as usize..sid as usize + 28].copy_from_slice(&sid_bytes);

    let mut host = Host { heap: 0x0080_0000, token_user_sid: sid };
    let mut cpu = Cpu::new();
    cpu.gs_base = teb;
    cpu.setup_frame(&mut mem, loaded.entry, stack_top);

    print!("whoami output: ");
    for _ in 0..2_000_000usize {
        match cpu.step(&mut mem) {
            StepResult::Ok | StepResult::Syscall => {}
            StepResult::Import { index } => {
                let (dll, name) = import_name(&data, index).unwrap_or(("?", "?"));
                eprintln!("[import] {dll}!{name}");
                let rax = service(&mut host, &mut cpu, &mut mem, dll, name);
                // ret: pop return address, set rax.
                let rsp = cpu.regs[RSP];
                cpu.rip = rd64(&mem, rsp);
                cpu.regs[RSP] = rsp + 8;
                cpu.regs[RAX] = rax;
            }
            StepResult::Halt => {
                println!("\n[whoami exited]");
                return;
            }
            StepResult::Unknown { rip, byte } => {
                print!("\n[unknown opcode {byte:#04x} at {rip:#x}] bytes:");
                let r = rip as usize;
                for b in &mem[r..r + 16] {
                    print!(" {b:02x}");
                }
                println!();
                return;
            }
            StepResult::Fault { addr } => {
                println!("\n[fault at {addr:#x} rip={:#x}]", cpu.rip);
                return;
            }
        }
    }
    println!("\n[step limit]");
}
