#!/usr/bin/env python3
"""Generate a PDB of public symbols for the nanokrnl kernel, so a Windows
debugger resolves nanokrnl's functions/globals by name (`lm`, `x nt!*`, symbolic
stacks) against the MEMORY.DMP.

The kernel is an ELF linked at 0, so each symbol's value is its RVA - exactly the
offset a debugger adds to the module load base (KERNEL_VIRT_BASE). We emit one
S_PUB32 per defined symbol into a PDB via `llvm-pdbutil yaml2pdb`. Load it in
WinDbg against the ntoskrnl.exe module (see the printed hint) with `.reload /i`.

    python3 tools/gen_pdb.py [path/to/kernel] [-o ntoskrnl.pdb]

Requires `llvm-pdbutil` (Homebrew llvm) and `nm` on PATH.
"""
import os
import re
import shutil
import subprocess
import sys

KERNEL_VIRT_BASE = 0xFFFF_8000_0000_0000

# CodeView type records for the NT structures nanokrnl lays out (see kernel
# src/kd.rs), so `dt`, `lm`, and `!process` decode them by name. Type indices are
# assigned sequentially from 0x1000 in record order; base types are CodeView
# simple types (0x0077 unsigned __int64, 0x0075 unsigned, 0x0021 unsigned short,
# 0x0020 unsigned char). Offsets/sizes are exactly kernel/src/kd.rs's #[repr(C)]
# layouts - these describe *our* compact structs, not a real Windows build's.
#   0x1001 _LIST_ENTRY   0x1003 _UNICODE_STRING   0x1004 char[16]
#   0x1006 _EPROCESS     0x1008 _KLDR_DATA_TABLE_ENTRY
TPI_RECORDS = """\
TpiStream:
  Version: VC80
  Records:
    - Kind: LF_FIELDLIST
      FieldList:
        - { Kind: LF_MEMBER, DataMember: { Attrs: 3, Type: 0x0077, FieldOffset: 0, Name: 'Flink' } }
        - { Kind: LF_MEMBER, DataMember: { Attrs: 3, Type: 0x0077, FieldOffset: 8, Name: 'Blink' } }
    - Kind: LF_STRUCTURE
      Class: { MemberCount: 2, Options: [ None ], FieldList: 0x1000, Name: '_LIST_ENTRY', UniqueName: '', DerivationList: 0, VTableShape: 0, Size: 16 }
    - Kind: LF_FIELDLIST
      FieldList:
        - { Kind: LF_MEMBER, DataMember: { Attrs: 3, Type: 0x0021, FieldOffset: 0, Name: 'Length' } }
        - { Kind: LF_MEMBER, DataMember: { Attrs: 3, Type: 0x0021, FieldOffset: 2, Name: 'MaximumLength' } }
        - { Kind: LF_MEMBER, DataMember: { Attrs: 3, Type: 0x0077, FieldOffset: 8, Name: 'Buffer' } }
    - Kind: LF_STRUCTURE
      Class: { MemberCount: 3, Options: [ None ], FieldList: 0x1002, Name: '_UNICODE_STRING', UniqueName: '', DerivationList: 0, VTableShape: 0, Size: 16 }
    - Kind: LF_ARRAY
      Array: { ElementType: 0x0020, IndexType: 0x0077, Size: 16, Name: '' }
    - Kind: LF_FIELDLIST
      FieldList:
        - { Kind: LF_MEMBER, DataMember: { Attrs: 3, Type: 0x0077, FieldOffset: 0, Name: 'UniqueProcessId' } }
        - { Kind: LF_MEMBER, DataMember: { Attrs: 3, Type: 0x1001, FieldOffset: 8, Name: 'ActiveProcessLinks' } }
        - { Kind: LF_MEMBER, DataMember: { Attrs: 3, Type: 0x0077, FieldOffset: 24, Name: 'DirectoryTableBase' } }
        - { Kind: LF_MEMBER, DataMember: { Attrs: 3, Type: 0x0077, FieldOffset: 32, Name: 'Peb' } }
        - { Kind: LF_MEMBER, DataMember: { Attrs: 3, Type: 0x1004, FieldOffset: 40, Name: 'ImageFileName' } }
    - Kind: LF_STRUCTURE
      Class: { MemberCount: 5, Options: [ None ], FieldList: 0x1005, Name: '_EPROCESS', UniqueName: '', DerivationList: 0, VTableShape: 0, Size: 56 }
    - Kind: LF_FIELDLIST
      FieldList:
        - { Kind: LF_MEMBER, DataMember: { Attrs: 3, Type: 0x1001, FieldOffset: 0, Name: 'InLoadOrderLinks' } }
        - { Kind: LF_MEMBER, DataMember: { Attrs: 3, Type: 0x0077, FieldOffset: 48, Name: 'DllBase' } }
        - { Kind: LF_MEMBER, DataMember: { Attrs: 3, Type: 0x0077, FieldOffset: 56, Name: 'EntryPoint' } }
        - { Kind: LF_MEMBER, DataMember: { Attrs: 3, Type: 0x0075, FieldOffset: 64, Name: 'SizeOfImage' } }
        - { Kind: LF_MEMBER, DataMember: { Attrs: 3, Type: 0x1003, FieldOffset: 72, Name: 'FullDllName' } }
        - { Kind: LF_MEMBER, DataMember: { Attrs: 3, Type: 0x1003, FieldOffset: 88, Name: 'BaseDllName' } }
    - Kind: LF_STRUCTURE
      Class: { MemberCount: 6, Options: [ None ], FieldList: 0x1007, Name: '_KLDR_DATA_TABLE_ENTRY', UniqueName: '', DerivationList: 0, VTableShape: 0, Size: 176 }\
"""


def write_pe_stub(path):
    """Write a minimal ntoskrnl.exe PE stub whose headers + RSDS debug directory
    match the masquerade header nanokrnl overlays into MEMORY.DMP (same GUID, age,
    TimeDateStamp=0, SizeOfImage=0x400000). WinDbg reads the crash's memory header
    to learn the image identity, then reads the *CodeView record from this file*
    on the .exepath; the RSDS points it at ntoskrnl.pdb. Mirrors dump.rs's
    masquerade_pe byte for byte."""
    # File layout: headers in [0, 0x400); one section ".text" mapped RVA 0x1000
    # -> file 0x400, holding the debug directory + RSDS so a section-based reader
    # (dbghelp/llvm) can resolve their RVAs. RVA 0x1000 -> file 0x400.
    p = bytearray(0x600)

    def w16(o, v):
        p[o:o + 2] = int(v).to_bytes(2, "little")

    def w32(o, v):
        p[o:o + 4] = int(v).to_bytes(4, "little")

    def w64(o, v):
        p[o:o + 8] = int(v).to_bytes(8, "little")

    p[0:2] = b"MZ"
    w32(0x3c, 0x40)
    p[0x40:0x44] = b"PE\x00\x00"
    w16(0x44, 0x8664)  # Machine = AMD64
    w16(0x46, 1)  # NumberOfSections
    w16(0x54, 0xF0)  # SizeOfOptionalHeader
    w16(0x56, 0x0022)  # Characteristics
    opt = 0x58
    w16(opt, 0x20B)  # PE32+
    w32(opt + 0x14, 0x1000)  # BaseOfCode
    w64(opt + 0x18, KERNEL_VIRT_BASE)  # ImageBase
    w32(opt + 0x20, 0x1000)  # SectionAlignment
    w32(opt + 0x24, 0x200)  # FileAlignment
    w16(opt + 0x30, 10)  # MajorSubsystemVersion
    w32(opt + 0x38, 0x0040_0000)  # SizeOfImage
    w32(opt + 0x3c, 0x400)  # SizeOfHeaders
    w16(opt + 0x44, 1)  # Subsystem = NATIVE
    w32(opt + 0x6c, 16)  # NumberOfRvaAndSizes
    dd_debug = opt + 0x70 + 6 * 8
    w32(dd_debug, 0x1000)  # DEBUG dir VA (inside .text)
    w32(dd_debug + 4, 0x1c)  # DEBUG dir size
    # Section table @ 0x148: ".text" VA 0x1000, raw data at file 0x400.
    sec = 0x148
    p[sec:sec + 5] = b".text"
    w32(sec + 0x08, 0x0040_0000 - 0x1000)  # VirtualSize
    w32(sec + 0x0c, 0x1000)  # VirtualAddress
    w32(sec + 0x10, 0x200)  # SizeOfRawData
    w32(sec + 0x14, 0x400)  # PointerToRawData
    w32(sec + 0x24, 0x6000_0020)  # CODE|EXECUTE|READ
    # IMAGE_DEBUG_DIRECTORY at RVA 0x1000 -> file 0x400.
    dbg = 0x400
    w32(dbg + 0x0c, 2)  # Type = CODEVIEW
    w32(dbg + 0x10, 4 + 16 + 4 + 13)  # SizeOfData
    w32(dbg + 0x14, 0x1020)  # AddressOfRawData (RVA of RSDS)
    w32(dbg + 0x18, 0x420)  # PointerToRawData (file offset of RSDS)
    # RSDS at RVA 0x1020 -> file 0x420.
    rs = 0x420
    p[rs:rs + 4] = b"RSDS"
    p[rs + 4:rs + 20] = bytes(
        [0x67, 0x45, 0x23, 0x01, 0xAB, 0x89, 0xEF, 0xCD, 0x01, 0x23, 0x45, 0x67, 0x89, 0xAB, 0xCD, 0xEF]
    )
    w32(rs + 20, 1)  # Age
    p[rs + 24:rs + 24 + 13] = b"ntoskrnl.pdb\x00"
    with open(path, "wb") as f:
        f.write(p)


def find_tool(*names):
    for n in names:
        p = shutil.which(n)
        if p:
            return p
    for d in ("/opt/homebrew/opt/llvm/bin", "/usr/local/opt/llvm/bin"):
        for n in names:
            p = os.path.join(d, n)
            if os.path.exists(p):
                return p
    return None


def read_symbols(nm, kernel):
    """Return [(rva, is_func, name)] for defined symbols with usable names."""
    out = subprocess.run([nm, kernel], capture_output=True, text=True).stdout
    syms = []
    seen = set()
    for line in out.splitlines():
        parts = line.split(maxsplit=2)
        if len(parts) != 3:
            continue  # undefined (U) symbols have no address
        addr, typ, name = parts
        if not re.fullmatch(r"[0-9a-fA-F]+", addr):
            continue
        rva = int(addr, 16)
        if rva == 0:
            continue
        # A public symbol offset is a 32-bit section offset; skip anything that
        # would not fit (nothing in our image is that large, but be safe).
        if rva >= 0xFFFF_FFFF:
            continue
        if "'" in name or any(ord(ch) < 0x20 for ch in name):
            continue
        if name in seen:
            continue
        seen.add(name)
        is_func = typ in ("T", "t")  # text (code) symbols
        syms.append((rva, is_func, name))
    return syms


def main():
    args = [a for a in sys.argv[1:]]
    out_pdb = "ntoskrnl.pdb"
    if "-o" in args:
        i = args.index("-o")
        out_pdb = args[i + 1]
        del args[i : i + 2]
    kernel = args[0] if args else "target/x86_64-unknown-none/debug/kernel"
    if not os.path.exists(kernel):
        sys.exit(f"kernel not found: {kernel}")

    pdbutil = find_tool("llvm-pdbutil")
    nm = find_tool("nm", "llvm-nm")
    if not pdbutil or not nm:
        sys.exit("need llvm-pdbutil and nm on PATH (brew install llvm)")

    syms = read_symbols(nm, kernel)
    if not syms:
        sys.exit("no symbols found in kernel ELF")

    yaml = [
        "MSF:",
        "  SuperBlock:",
        "    BlockSize: 4096",
        "    FreeBlockMap: 2",
        "    NumBlocks: 0",
        "    NumDirectoryBytes: 0",
        "    Unknown1: 0",
        "    BlockMapAddr: 3",
        "PdbStream:",
        "  Age: 1",
        "  Guid: '{01234567-89AB-CDEF-0123-456789ABCDEF}'",
        "  Signature: 0",
        "  Features: [ VC140 ]",
        "  Version: VC70",
        "DbiStream:",
        "  VerHeader: V70",
        "  Age: 1",
        "  MachineType: Amd64",
        "  Modules: []",
    ]
    yaml += TPI_RECORDS.splitlines()
    yaml += [
        "PublicsStream:",
        "  Records:",
    ]
    for rva, is_func, name in syms:
        flags = "[ Function ]" if is_func else "[ ]"
        yaml += [
            "    - Kind: S_PUB32",
            "      PublicSym32:",
            f"        Flags: {flags}",
            f"        Offset: {rva:#x}",
            "        Segment: 1",
            f"        Name: '{name}'",
        ]

    yaml_path = out_pdb + ".yaml"
    with open(yaml_path, "w") as f:
        f.write("\n".join(yaml) + "\n")

    subprocess.run([pdbutil, "yaml2pdb", f"--pdb={out_pdb}", yaml_path], check=True)
    # Sanity-count what actually landed.
    dump = subprocess.run([pdbutil, "dump", "-publics", out_pdb], capture_output=True, text=True).stdout
    n_pub = dump.count("S_PUB32")

    # Companion ntoskrnl.exe stub: WinDbg reads the RSDS from the image *file*,
    # so it needs this alongside the PDB (matching the masquerade header in the
    # dump). Written next to the PDB, named ntoskrnl.exe.
    exe_path = os.path.join(os.path.dirname(out_pdb) or ".", "ntoskrnl.exe")
    write_pe_stub(exe_path)

    print(f"wrote {out_pdb}: {n_pub} public symbols (of {len(syms)} defined)")
    print(f"wrote {exe_path}: ntoskrnl.exe PE stub (RSDS -> ntoskrnl.pdb)")
    print("WinDbg: put BOTH on a local path (e.g. C:\\sym), then in the dump:")
    print("  .exepath+ C:\\sym ; .sympath+ C:\\sym ; .reload /f")
    print("  (symbols resolve as KERNEL_VIRT_BASE + RVA; the kernel links at 0)")


if __name__ == "__main__":
    main()
