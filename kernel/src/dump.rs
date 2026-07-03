//! Kernel-authored crash dump, in the Linux ELF-core style (`/proc/vmcore` /
//! kdump), written to the host over 9P.
//!
//! On a bugcheck nanokrnl builds an `ET_CORE` ELF image itself and streams it to
//! `H:\nanokrnl.core` through [`crate::io::p9`]. Because the kernel is an ELF
//! with DWARF, the *same* image (`kernel.bin`) is the symbol file: open it with
//! `gdb kernel.bin nanokrnl.core`, the `crash` utility, or a modern WinDbg (ELF
//! + DWARF) for a symbolic view of the crash. No PDB, nothing synthetic.
//!
//! The core carries:
//! * a `PT_NOTE` with `NT_PRSTATUS` (the crash register set) and `VMCOREINFO`
//!   (the kdump metadata note), and
//! * one `PT_LOAD` per higher-half virtual mapping (found by walking the page
//!   tables), so code and stacks are readable at their real virtual addresses.
//!
//! Only low physical memory is captured (a few MiB — where the kernel image,
//! page tables, pool, stacks and user processes live), dumped once; every
//! `PT_LOAD` points back into that single physical image. A real vmcore likewise
//! skips unpopulated RAM.

use crate::io::p9;
use crate::mm::{phys_to_virt, PhysAddr};
use alloc::vec::Vec;
use core::arch::asm;

/// Physical bytes captured, from PA 0. Bounds the 9P transfer; covers the low
/// RAM the kernel actually uses (image, page tables, pool, stacks). Kept small
/// because the byte-wise 9P transport does roughly one write per run-slice; a
/// bulk-copy channel is the future optimization for a full-size dump.
const CAP: u64 = 32 * 1024 * 1024;
const PAGE: u64 = 0x1000;

const ENTRY_PRESENT: u64 = 1 << 0;
const ENTRY_LARGE: u64 = 1 << 7;
const ADDR_MASK: u64 = 0x000F_FFFF_FFFF_F000;

/// A contiguous virtual mapping to physical memory (a coalesced page-table run).
struct Run {
    vaddr: u64,
    paddr: u64,
    size: u64,
}

/// The crash register set we can capture at the bugcheck point (callee-saved
/// registers are meaningful; RIP/RSP/RBP drive the stack unwind).
#[derive(Default, Clone, Copy)]
struct Gpr {
    rbx: u64,
    rbp: u64,
    rsp: u64,
    r12: u64,
    r13: u64,
    r14: u64,
    r15: u64,
    rip: u64,
    rflags: u64,
}

/// Read CR3.
fn cr3() -> u64 {
    let v: u64;
    unsafe { asm!("mov {}, cr3", out(reg) v, options(nomem, nostack, preserves_flags)) };
    v
}

/// Read `n` u64 page-table entries from a physical frame via the direct map.
fn table(frame: u64) -> &'static [u64] {
    let p = phys_to_virt(PhysAddr(frame & ADDR_MASK)) as *const u64;
    unsafe { core::slice::from_raw_parts(p, 512) }
}

/// Sign-extend a 48-bit virtual address to canonical form.
fn canonical(va: u64) -> u64 {
    if va & (1 << 47) != 0 {
        va | 0xFFFF_0000_0000_0000
    } else {
        va
    }
}

/// Walk the higher half (PML4 entries 256..512) and collect present leaf
/// mappings, coalescing virtually- and physically-contiguous runs.
fn walk_runs() -> Vec<Run> {
    let mut runs: Vec<Run> = Vec::new();
    let mut push = |va: u64, pa: u64, sz: u64| match runs.last_mut() {
        Some(r) if r.vaddr + r.size == va && r.paddr + r.size == pa => r.size += sz,
        _ => runs.push(Run { vaddr: va, paddr: pa, size: sz }),
    };
    let pml4 = table(cr3());
    for i4 in 256..512usize {
        let e4 = pml4[i4];
        if e4 & ENTRY_PRESENT == 0 {
            continue;
        }
        let pdpt = table(e4 & ADDR_MASK);
        for (i3, &e3) in pdpt.iter().enumerate() {
            if e3 & ENTRY_PRESENT == 0 {
                continue;
            }
            let va3 = ((i4 as u64) << 39) | ((i3 as u64) << 30);
            if e3 & ENTRY_LARGE != 0 {
                push(canonical(va3), e3 & ADDR_MASK, 1 << 30); // 1 GiB
                continue;
            }
            let pd = table(e3 & ADDR_MASK);
            for (i2, &e2) in pd.iter().enumerate() {
                if e2 & ENTRY_PRESENT == 0 {
                    continue;
                }
                let va2 = va3 | ((i2 as u64) << 21);
                if e2 & ENTRY_LARGE != 0 {
                    push(canonical(va2), e2 & ADDR_MASK, 1 << 21); // 2 MiB
                    continue;
                }
                let pt = table(e2 & ADDR_MASK);
                for (i1, &e1) in pt.iter().enumerate() {
                    if e1 & ENTRY_PRESENT == 0 {
                        continue;
                    }
                    push(canonical(va2 | ((i1 as u64) << 12)), e1 & ADDR_MASK, PAGE);
                }
            }
        }
    }
    runs
}

fn capture() -> Gpr {
    let mut g = Gpr::default();
    unsafe {
        asm!(
            "mov {rbx}, rbx", "mov {rbp}, rbp", "mov {rsp}, rsp",
            "mov {r12}, r12", "mov {r13}, r13", "mov {r14}, r14", "mov {r15}, r15",
            "lea {rip}, [rip]", "pushfq", "pop {rfl}",
            rbx = out(reg) g.rbx, rbp = out(reg) g.rbp, rsp = out(reg) g.rsp,
            r12 = out(reg) g.r12, r13 = out(reg) g.r13, r14 = out(reg) g.r14, r15 = out(reg) g.r15,
            rip = out(reg) g.rip, rfl = out(reg) g.rflags,
        );
    }
    g
}

// --- little-endian buffer helpers ----------------------------------------
fn p16(v: &mut Vec<u8>, x: u16) {
    v.extend_from_slice(&x.to_le_bytes());
}
fn p32(v: &mut Vec<u8>, x: u32) {
    v.extend_from_slice(&x.to_le_bytes());
}
fn p64(v: &mut Vec<u8>, x: u64) {
    v.extend_from_slice(&x.to_le_bytes());
}

/// Build the `NT_PRSTATUS` note descriptor (`elf_prstatus`, 336 bytes; the
/// x86-64 register array `pr_reg` sits at offset 0x70).
fn prstatus(g: &Gpr) -> Vec<u8> {
    let mut d = alloc::vec![0u8; 336];
    // pr_reg order: r15,r14,r13,r12,rbp,rbx,r11,r10,r9,r8,rax,rcx,rdx,rsi,rdi,
    //               orig_rax,rip,cs,eflags,rsp,ss,fs_base,gs_base,ds,es,fs,gs.
    let regs: [u64; 27] = [
        g.r15, g.r14, g.r13, g.r12, g.rbp, g.rbx, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        g.rip, 0x10, g.rflags, g.rsp, 0x18, 0, 0, 0, 0, 0, 0,
    ];
    for (i, r) in regs.iter().enumerate() {
        d[0x70 + i * 8..0x70 + i * 8 + 8].copy_from_slice(&r.to_le_bytes());
    }
    d
}

/// Append an ELF note (name padded to 4, desc padded to 4).
fn note(out: &mut Vec<u8>, name: &str, ntype: u32, desc: &[u8]) {
    let namesz = name.len() + 1; // include NUL
    p32(out, namesz as u32);
    p32(out, desc.len() as u32);
    p32(out, ntype);
    out.extend_from_slice(name.as_bytes());
    out.push(0);
    while out.len() % 4 != 0 {
        out.push(0);
    }
    out.extend_from_slice(desc);
    while out.len() % 4 != 0 {
        out.push(0);
    }
}

/// Write the crash core to `H:\nanokrnl.core` over 9P. Returns the byte count on
/// success, or `None` if the host 9P server is absent/failed. Safe to call at
/// bugcheck IRQL: it only does port I/O + reads of physical memory.
pub fn write_core(bugcheck: u32, params: &[u64; 4]) -> Option<u64> {
    // Publish the kernel-debugger data (module + process lists +
    // KdDebuggerDataBlock) so the captured image carries a coherent snapshot a
    // Windows debugger can walk. Must run before we snapshot memory below.
    crate::init::kd_snapshot();
    let g = capture();

    // Note segment: NT_PRSTATUS + VMCOREINFO.
    let mut notes = Vec::new();
    note(&mut notes, "CORE", 1 /* NT_PRSTATUS */, &prstatus(&g));
    let mut vci = Vec::new();
    let _ = bugcheck;
    vci.extend_from_slice(b"OSRELEASE=nanokrnl-0.1.0\n");
    vci.extend_from_slice(b"PAGESIZE=4096\n");
    // Record the debugger-data anchors so a tool can find them without symbols
    // (the way a Windows debugger uses KdDebuggerDataBlock, but symbol-free).
    let kdbg = &raw const crate::kd::KdDebuggerDataBlock as u64;
    let mut anchors = alloc::format!(
        "SYMBOL(KdDebuggerDataBlock)={:#x}\nSYMBOL(PsLoadedModuleList)={:#x}\nSYMBOL(PsActiveProcessHead)={:#x}\n",
        kdbg,
        &raw const crate::kd::PsLoadedModuleList as u64,
        &raw const crate::kd::PsActiveProcessHead as u64,
    );
    vci.append(unsafe { anchors.as_mut_vec() });
    // Record the bugcheck for context (nanokrnl-specific keys).
    let mut line = alloc::format!(
        "BUGCHECK={:#010x}\nBUGCHECK_P1={:#x}\nBUGCHECK_P2={:#x}\nBUGCHECK_P3={:#x}\nBUGCHECK_P4={:#x}\n",
        bugcheck, params[0], params[1], params[2], params[3]
    );
    vci.append(unsafe { line.as_mut_vec() });
    note(&mut notes, "VMCOREINFO", 0, &vci);

    // Higher-half mappings that fall within the captured physical window.
    let runs: Vec<Run> = walk_runs()
        .into_iter()
        .filter(|r| r.paddr < CAP)
        .collect();
    let phnum = 1 + runs.len(); // PT_NOTE + PT_LOADs

    // Prefix = ELF header + program headers + notes, padded to a page; the
    // physical image follows at `data_off`.
    let phoff = 64u64;
    let note_off = phoff + 56 * phnum as u64;
    let data_off = {
        let end = note_off + notes.len() as u64;
        (end + PAGE - 1) & !(PAGE - 1)
    };

    let mut hdr = Vec::new();
    // ELF64 header.
    hdr.extend_from_slice(&[0x7f, b'E', b'L', b'F', 2, 1, 1, 0]);
    hdr.extend_from_slice(&[0u8; 8]);
    p16(&mut hdr, 4); // e_type = ET_CORE
    p16(&mut hdr, 62); // e_machine = EM_X86_64
    p32(&mut hdr, 1); // e_version
    p64(&mut hdr, 0); // e_entry
    p64(&mut hdr, phoff); // e_phoff
    p64(&mut hdr, 0); // e_shoff
    p32(&mut hdr, 0); // e_flags
    p16(&mut hdr, 64); // e_ehsize
    p16(&mut hdr, 56); // e_phentsize
    p16(&mut hdr, phnum as u16);
    p16(&mut hdr, 0); // e_shentsize
    p16(&mut hdr, 0); // e_shnum
    p16(&mut hdr, 0); // e_shstrndx

    // PT_NOTE.
    p32(&mut hdr, 4); // p_type = PT_NOTE
    p32(&mut hdr, 0); // p_flags
    p64(&mut hdr, note_off); // p_offset
    p64(&mut hdr, 0); // p_vaddr
    p64(&mut hdr, 0); // p_paddr
    p64(&mut hdr, notes.len() as u64); // p_filesz
    p64(&mut hdr, notes.len() as u64); // p_memsz
    p64(&mut hdr, 4); // p_align
    // PT_LOAD per run (clamped to the captured window; file bytes shared).
    for r in &runs {
        let filesz = (CAP - r.paddr).min(r.size);
        p32(&mut hdr, 1); // p_type = PT_LOAD
        p32(&mut hdr, 7); // p_flags = RWX
        p64(&mut hdr, data_off + r.paddr); // p_offset into the physical image
        p64(&mut hdr, r.vaddr);
        p64(&mut hdr, r.paddr);
        p64(&mut hdr, filesz); // p_filesz
        p64(&mut hdr, r.size); // p_memsz
        p64(&mut hdr, PAGE); // p_align
    }
    // Pad the prefix out to data_off.
    hdr.resize(data_off as usize, 0);
    // Append the note bytes at note_off (they sit inside the prefix region).
    hdr[note_off as usize..note_off as usize + notes.len()].copy_from_slice(&notes);

    // Stream: prefix, then the low physical image [0, CAP) read directly through
    // the contiguous direct map (write() chunks + pipelines it over 9P).
    let mut w = p9::create("nanokrnl.core")?;
    if !w.write(&hdr) {
        return None;
    }
    let mem = unsafe { core::slice::from_raw_parts(phys_to_virt(PhysAddr(0)), CAP as usize) };
    if !w.write(mem) {
        return None;
    }
    let total = w.offset();
    w.close();
    Some(total)
}
