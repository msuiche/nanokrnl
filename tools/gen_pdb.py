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
    print(f"wrote {out_pdb}: {n_pub} public symbols (of {len(syms)} defined)")
    print("WinDbg: put ntoskrnl.pdb on your symbol path, then in the dump:")
    print(f"  .reload /i /f ntoskrnl.exe=0x{KERNEL_VIRT_BASE:016x}")
    print("  (symbols resolve as KERNEL_VIRT_BASE + RVA; the kernel links at 0)")


if __name__ == "__main__":
    main()
