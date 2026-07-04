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

/// The crash register set we capture at the bugcheck point (callee-saved
/// registers are meaningful; RIP/RSP/RBP drive the stack unwind). Captured once
/// at the bugcheck entry and shared by both dump writers, so the recorded crash
/// context is the bugcheck site rather than deep inside the dump code.
#[derive(Default, Clone, Copy)]
pub struct Gpr {
    rbx: u64,
    rbp: u64,
    rsp: u64,
    r12: u64,
    r13: u64,
    r14: u64,
    r15: u64,
    rip: u64,
    rflags: u64,
    // Special registers for the KPROCESSOR_STATE a Windows debugger reads
    // (control regs + the GDTR/IDTR that resolve the CS descriptor).
    cr0: u64,
    cr2: u64,
    cr3: u64,
    cr4: u64,
    cr8: u64,
    gdt_base: u64,
    gdt_limit: u16,
    idt_base: u64,
    idt_limit: u16,
    tr: u16,
    ldtr: u16,
    cs: u16,
    ss: u16,
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

/// Snapshot the crash register set at the caller's point. Call once at the
/// bugcheck entry and pass the result to the dump writers.
pub fn capture() -> Gpr {
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
        // Control registers. nanox implements CR0/CR2/CR4/CR8; it does NOT
        // implement sgdt/sidt/str/sldt or reading CS/SS, so those are taken from
        // the kernel's own loaded tables/selectors below (which is exact anyway).
        asm!("mov {}, cr0", out(reg) g.cr0, options(nomem, nostack, preserves_flags));
        asm!("mov {}, cr2", out(reg) g.cr2, options(nomem, nostack, preserves_flags));
        asm!("mov {}, cr4", out(reg) g.cr4, options(nomem, nostack, preserves_flags));
        asm!("mov {}, cr8", out(reg) g.cr8, options(nomem, nostack, preserves_flags));
    }
    g.cr3 = cr3();
    // GDTR/IDTR: report the tables the kernel handed to lgdt/lidt (their bases are
    // higher-half VAs the debugger translates via CR3 and reads the descriptors).
    let (gb, gl) = crate::ke::gdt::gdtr();
    let (ib, il) = crate::ke::idt::idtr();
    g.gdt_base = gb;
    g.gdt_limit = gl;
    g.idt_base = ib;
    g.idt_limit = il;
    // Selectors the kernel runs with: the dumped GDT has valid long-mode
    // descriptors at these indices, so the debugger resolves CS/SS/DS.
    g.cs = crate::ke::selectors::KGDT64_R0_CODE;
    g.ss = crate::ke::selectors::KGDT64_R0_DATA;
    g.tr = crate::ke::selectors::KGDT64_SYS_TSS;
    g.ldtr = 0;
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
pub fn write_core(bugcheck: u32, params: &[u64; 4], g: &Gpr) -> Option<u64> {
    // Publish the kernel-debugger data (module + process lists +
    // KdDebuggerDataBlock) so the captured image carries a coherent snapshot a
    // Windows debugger can walk. Must run before we snapshot memory below.
    crate::init::kd_snapshot();

    // Note segment: NT_PRSTATUS + VMCOREINFO.
    let mut notes = Vec::new();
    note(&mut notes, "CORE", 1 /* NT_PRSTATUS */, &prstatus(g));
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
    if !write_phys_progress(&mut w, mem, "nanokrnl.core") {
        return None;
    }
    let total = w.offset();
    w.close();
    Some(total)
}

/// Size of a 64-bit Windows crash-dump header (`_DUMP_HEADER64`).
const DMP_HEADER: usize = 0x2000;

/// The RSDS PDB GUID nanokrnl advertises, matching `tools/gen_pdb.py`'s
/// `{01234567-89AB-CDEF-0123-456789ABCDEF}` (age 1). A Windows debugger reads it
/// from the masquerade PE below, then loads `ntoskrnl.pdb` with this identity.
/// GUID wire order: Data1 (LE u32), Data2/Data3 (LE u16), Data4 (8 bytes as-is).
const RSDS_GUID: [u8; 16] = [
    0x67, 0x45, 0x23, 0x01, 0xAB, 0x89, 0xEF, 0xCD, 0x01, 0x23, 0x45, 0x67, 0x89, 0xAB, 0xCD, 0xEF,
];
const RSDS_AGE: u32 = 1;

/// Build a minimal PE32+ image header that masquerades as `ntoskrnl.exe`, so a
/// Windows debugger (which reads a module's headers from memory at its base)
/// finds a real `IMAGE_NT_HEADERS` with a CodeView/RSDS debug directory instead
/// of nanokrnl's ELF header. It carries `SizeOfImage`, `ImageBase`, and the RSDS
/// record (GUID + `ntoskrnl.pdb`) that points the debugger at our PDB. Overlaid
/// onto the kernel image's first bytes *in the dump only* - the range is the ELF
/// header and program headers, not code - so nothing executing is disturbed.
fn masquerade_pe(size_of_image: u32, entry_rva: u32) -> Vec<u8> {
    let mut p = alloc::vec![0u8; 0x400];
    let w16 = |p: &mut [u8], o: usize, v: u16| p[o..o + 2].copy_from_slice(&v.to_le_bytes());
    let w32 = |p: &mut [u8], o: usize, v: u32| p[o..o + 4].copy_from_slice(&v.to_le_bytes());
    let w64 = |p: &mut [u8], o: usize, v: u64| p[o..o + 8].copy_from_slice(&v.to_le_bytes());

    // DOS header: 'MZ' and e_lfanew -> the PE header at 0x40.
    p[0] = b'M';
    p[1] = b'Z';
    w32(&mut p, 0x3c, 0x40);

    // PE signature + COFF header @ 0x40.
    p[0x40..0x44].copy_from_slice(b"PE\0\0");
    w16(&mut p, 0x44, 0x8664); // Machine = AMD64
    w16(&mut p, 0x46, 1); // NumberOfSections
    w32(&mut p, 0x48, 0); // TimeDateStamp (0 -> matches the dump's unknown stamp)
    w16(&mut p, 0x54, 0xF0); // SizeOfOptionalHeader
    w16(&mut p, 0x56, 0x0022); // Characteristics: EXECUTABLE | LARGE_ADDRESS_AWARE

    // Optional header (PE32+) @ 0x58.
    let opt = 0x58;
    w16(&mut p, opt, 0x20B); // Magic = PE32+
    w32(&mut p, opt + 0x10, entry_rva); // AddressOfEntryPoint
    w32(&mut p, opt + 0x14, 0x1000); // BaseOfCode
    w64(&mut p, opt + 0x18, super::kd::KERNEL_VIRT_BASE); // ImageBase
    w32(&mut p, opt + 0x20, 0x1000); // SectionAlignment
    w32(&mut p, opt + 0x24, 0x200); // FileAlignment
    w16(&mut p, opt + 0x30, 10); // MajorSubsystemVersion
    w32(&mut p, opt + 0x38, size_of_image); // SizeOfImage
    w32(&mut p, opt + 0x3c, 0x400); // SizeOfHeaders
    w16(&mut p, opt + 0x44, 1); // Subsystem = NATIVE
    w32(&mut p, opt + 0x6c, 16); // NumberOfRvaAndSizes
    // DataDirectory[6] = DEBUG, at opt+0x70 + 6*8.
    let dd_debug = opt + 0x70 + 6 * 8;
    w32(&mut p, dd_debug, 0x180); // VA of the debug directory
    w32(&mut p, dd_debug + 4, 0x1c); // size (one IMAGE_DEBUG_DIRECTORY)

    // Section header @ 0x148 (opt 0x58 + 0xF0): one section spanning the image.
    let sec = 0x148;
    p[sec..sec + 5].copy_from_slice(b".text");
    w32(&mut p, sec + 0x08, size_of_image.saturating_sub(0x1000)); // VirtualSize
    w32(&mut p, sec + 0x0c, 0x1000); // VirtualAddress
    w32(&mut p, sec + 0x24, 0x6000_0020); // CODE | EXECUTE | READ

    // IMAGE_DEBUG_DIRECTORY @ RVA 0x180.
    let dbg = 0x180;
    w32(&mut p, dbg + 0x0c, 2); // Type = IMAGE_DEBUG_TYPE_CODEVIEW
    w32(&mut p, dbg + 0x10, 4 + 16 + 4 + 13); // SizeOfData (RSDS record)
    w32(&mut p, dbg + 0x14, 0x1a0); // AddressOfRawData (RVA of RSDS)
    w32(&mut p, dbg + 0x18, 0x1a0); // PointerToRawData

    // CodeView RSDS record @ 0x1a0: 'RSDS', GUID, Age, "ntoskrnl.pdb\0".
    let rs = 0x1a0;
    p[rs..rs + 4].copy_from_slice(b"RSDS");
    p[rs + 4..rs + 20].copy_from_slice(&RSDS_GUID);
    w32(&mut p, rs + 20, RSDS_AGE);
    p[rs + 24..rs + 24 + 13].copy_from_slice(b"ntoskrnl.pdb\0");
    p
}

/// Stream `mem` to `w` in slices, printing a newline-terminated progress line
/// every ~12% - the multi-MiB physical-memory write is the slow part of a dump
/// (Windows shows the same "Dumping physical memory to disk" percentage). A
/// carriage-return bar would be prettier, but not every console honors `\r`;
/// discrete lines render everywhere. Returns false on any transport failure.
fn write_phys_progress(w: &mut p9::Writer, mem: &[u8], label: &str) -> bool {
    const STEP: usize = 1 << 21; // 2 MiB per slice
    let total = mem.len().max(1);
    let mut off = 0usize;
    let mut shown: u64 = 0;
    while off < mem.len() {
        let end = (off + STEP).min(mem.len());
        if !w.write(&mem[off..end]) {
            return false;
        }
        off = end;
        let pct = (off as u64 * 100 / total as u64).min(100);
        if pct >= shown + 12 || off == mem.len() {
            shown = pct;
            crate::kd_println!("***   {}: {}%", label, pct);
        }
    }
    true
}

/// Write a **Windows kernel crash dump** to `H:\MEMORY.DMP` over 9P, so a real
/// Windows debugger opens it as a kernel target (`lm`, `!process 0 0`).
///
/// Unlike the ELF core (a Linux target), this is the native `DUMP_HEADER64`
/// format: an 8 KiB header naming the address space (`DirectoryTableBase`), the
/// `KdDebuggerDataBlock`, the `PsLoadedModuleList`/`PsActiveProcessHead` heads,
/// the crash `CONTEXT`, and a `PHYSICAL_MEMORY_DESCRIPTOR`; followed by the raw
/// physical memory the descriptor's runs name. WinDbg reads `DirectoryTableBase`
/// (CR3), walks the captured page tables to translate every virtual address, and
/// finds the NT structures [`crate::kd`] laid out. Returns the byte count, or
/// `None` if the host 9P server is absent. Safe at bugcheck IRQL.
pub fn write_memory_dmp(bugcheck: u32, params: &[u64; 4], g: &Gpr) -> Option<u64> {
    // The KDBG structures must be current (write_core also refreshes them; doing
    // it again is idempotent).
    crate::init::kd_snapshot();

    let mut h = alloc::vec![0u8; DMP_HEADER];
    let put32 = |h: &mut [u8], off: usize, v: u32| h[off..off + 4].copy_from_slice(&v.to_le_bytes());
    let put64 = |h: &mut [u8], off: usize, v: u64| h[off..off + 8].copy_from_slice(&v.to_le_bytes());
    let put16 = |h: &mut [u8], off: usize, v: u16| h[off..off + 2].copy_from_slice(&v.to_le_bytes());

    // --- DUMP_HEADER64 fixed fields ------------------------------------
    h[0..4].copy_from_slice(b"PAGE"); // Signature
    h[4..8].copy_from_slice(b"DU64"); // ValidDump (64-bit)
    put32(&mut h, 0x08, 15); // MajorVersion (NT free-build indicator)
    put32(&mut h, 0x0c, 31337); // MinorVersion = build (nanokrnl 1.1.31337)
    put64(&mut h, 0x10, cr3() & ADDR_MASK); // DirectoryTableBase (kernel CR3)
    put64(&mut h, 0x18, 0); // PfnDataBase (not modeled)
    put64(&mut h, 0x20, &raw const crate::kd::PsLoadedModuleList as u64);
    put64(&mut h, 0x28, &raw const crate::kd::PsActiveProcessHead as u64);
    put32(&mut h, 0x30, 0x8664); // MachineImageType = IMAGE_FILE_MACHINE_AMD64
    put32(&mut h, 0x34, 1); // NumberProcessors
    put32(&mut h, 0x38, bugcheck); // BugCheckCode
    put64(&mut h, 0x40, params[0]);
    put64(&mut h, 0x48, params[1]);
    put64(&mut h, 0x50, params[2]);
    put64(&mut h, 0x58, params[3]);
    put64(&mut h, 0x80, &raw const crate::kd::KdDebuggerDataBlock as u64);

    // --- PHYSICAL_MEMORY_DESCRIPTOR @ 0x88 -----------------------------
    // One run covering the captured low window [0, CAP): BasePage/PageCount are
    // in pages. The debugger translates VAs into these frames via the page
    // tables (which live in this same window).
    let pages = CAP / PAGE;
    put32(&mut h, 0x88, 1); // NumberOfRuns
    put64(&mut h, 0x90, pages); // NumberOfPages
    put64(&mut h, 0x98, 0); // Run[0].BasePage
    put64(&mut h, 0xa0, pages); // Run[0].PageCount

    // --- CONTEXT record @ 0x348 (the KPROCESSOR_STATE.ContextFrame) --------
    // This is the crash context a Windows debugger reads on a full dump. It must
    // be a complete, self-consistent AMD64 CONTEXT: the ContextFlags advertise
    // exactly the groups we fill (control + integer + segments, NOT floating
    // point - we do not provide an XSAVE area, and claiming it we don't makes
    // the debugger reject the context). CR3 is not in the CONTEXT; it travels in
    // DirectoryTableBase above (the SpecialRegisters half of KPROCESSOR_STATE).
    let c = 0x348usize;
    const CONTEXT_AMD64: u32 = 0x0010_0000;
    const CONTEXT_CONTROL: u32 = 0x1;
    const CONTEXT_INTEGER: u32 = 0x2;
    const CONTEXT_SEGMENTS: u32 = 0x4;
    put32(&mut h, c + 0x30, CONTEXT_AMD64 | CONTEXT_CONTROL | CONTEXT_INTEGER | CONTEXT_SEGMENTS);
    put32(&mut h, c + 0x34, 0x1f80); // MxCsr (default; also mirrored in FltSave)
    // Segment selectors captured at the crash: SegCs must index a valid GDT
    // descriptor (the debugger walks GDTR to resolve it), so use the live CS/SS.
    put16(&mut h, c + 0x38, g.cs); // SegCs
    put16(&mut h, c + 0x3a, g.ss); // SegDs
    put16(&mut h, c + 0x3c, g.ss); // SegEs
    put16(&mut h, c + 0x3e, g.ss); // SegFs
    put16(&mut h, c + 0x40, g.ss); // SegGs
    put16(&mut h, c + 0x42, g.ss); // SegSs
    put32(&mut h, c + 0x44, g.rflags as u32); // EFlags
    put64(&mut h, c + 0x90, g.rbx); // Rbx
    put64(&mut h, c + 0x98, g.rsp); // Rsp
    put64(&mut h, c + 0xa0, g.rbp); // Rbp
    put64(&mut h, c + 0xd8, g.r12); // R12
    put64(&mut h, c + 0xe0, g.r13); // R13
    put64(&mut h, c + 0xe8, g.r14); // R14
    put64(&mut h, c + 0xf0, g.r15); // R15
    put64(&mut h, c + 0xf8, g.rip); // Rip
    put32(&mut h, c + 0x100 + 0x18, 0x1f80); // FltSave.MxCsr (kept consistent)

    // Publish processor 0's KPROCESSOR_STATE (special registers + this CONTEXT)
    // so the debugger can GetContextState and resolve the CS descriptor via the
    // captured GDTR. Wires KiProcessorBlock in KdDebuggerDataBlock; must run
    // before the memory snapshot below so the PRCB and wiring land in the dump.
    let mut ctx = [0u8; crate::kd::CONTEXT_SIZE];
    ctx.copy_from_slice(&h[c..c + crate::kd::CONTEXT_SIZE]);
    crate::kd::set_processor_state(
        g.cr0, g.cr2, g.cr3, g.cr4, g.cr8, g.gdt_base, g.gdt_limit, g.idt_base, g.idt_limit,
        g.tr, g.ldtr, &ctx,
    );

    // --- tail fields ---------------------------------------------------
    put32(&mut h, 0xf98, 1); // DumpType = DUMP_TYPE_FULL
    put64(&mut h, 0xfa0, DMP_HEADER as u64 + CAP); // RequiredDumpSpace

    // Stream: header, then the physical window [0, CAP). Overlay the masquerade
    // PE header at the kernel image's physical base so the debugger reads a valid
    // ntoskrnl.exe header (and its RSDS -> ntoskrnl.pdb) from the dump instead of
    // the ELF header. Splice it into the stream; the live image is untouched.
    let mut w = p9::create("MEMORY.DMP")?;
    if !w.write(&h) {
        return None;
    }
    let mem = unsafe { core::slice::from_raw_parts(phys_to_virt(PhysAddr(0)), CAP as usize) };
    let kbase_phys = crate::mm::virt::mm_get_physical_address(crate::kd::KERNEL_VIRT_BASE)
        .map(|p| p.0 as usize)
        .unwrap_or(usize::MAX);
    let pe = masquerade_pe(0x0040_0000, 0);
    if kbase_phys + pe.len() <= CAP as usize {
        // [0, kbase_phys)  then  PE header  then  [kbase_phys + pe.len(), CAP)
        if !w.write(&mem[..kbase_phys]) || !w.write(&pe) {
            return None;
        }
        if !write_phys_progress(&mut w, &mem[kbase_phys + pe.len()..], "MEMORY.DMP") {
            return None;
        }
    } else if !write_phys_progress(&mut w, mem, "MEMORY.DMP") {
        return None;
    }
    let total = w.offset();
    w.close();
    Some(total)
}
