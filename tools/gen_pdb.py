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
