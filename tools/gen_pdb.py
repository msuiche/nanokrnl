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
import struct
import subprocess
import sys
import zlib

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


def _hash_string_v1(s):
    """MSVC/LLVM TPI name hash (Hasher::lhashPbCb). dbghelp computes this over a
    type name and looks in NumHashBuckets to resolve `dt nt!<name>`, so the hash
    we store per UDT record must match or the type is unreachable by name."""
    r = 0
    nlongs = len(s) // 4
    for i in range(nlongs):
        r ^= struct.unpack_from("<I", s, i * 4)[0]
    rem = s[nlongs * 4:]
    if len(rem) >= 2:
        r ^= struct.unpack_from("<H", rem, 0)[0]
        rem = rem[2:]
    if len(rem) == 1:
        r ^= rem[0]
    r |= 0x20202020
    r ^= r >> 11
    r ^= r >> 16
    return r & 0xFFFFFFFF


def _jam_crc(data):
    """LLVM hashBufferV8 (JamCRC) - CRC-32 without the final complement; used to
    hash non-UDT records (field lists, arrays) that have no name."""
    return (zlib.crc32(data) ^ 0xFFFFFFFF) & 0xFFFFFFFF


def patch_tpi_hash(pdb_path):
    """llvm-pdbutil yaml2pdb writes an empty TPI HashValueBuffer, so dbghelp
    loads the PDB as "public symbols" and `dt nt!_EPROCESS` fails even though the
    type record is present. Compute the per-record hash values and inject them,
    patching the TPI stream header's hash offsets. In-place: the hash stream
    grows by 4*NumTypes bytes, which stays within its single MSF block."""
    UDT_KINDS = {0x1504, 0x1505, 0x1506, 0x1507}  # class, struct, union, enum
    d = bytearray(open(pdb_path, "rb").read())
    bs = struct.unpack_from("<I", d, 0x20)[0]
    ndir = struct.unpack_from("<I", d, 0x2C)[0]
    blockmap = struct.unpack_from("<I", d, 0x34)[0]
    ndir_blocks = (ndir + bs - 1) // bs
    dir_blk = list(struct.unpack_from(f"<{ndir_blocks}I", d, blockmap * bs))
    dirb = b"".join(bytes(d[b * bs:b * bs + bs]) for b in dir_blk)[:ndir]

    off = 0
    nstreams = struct.unpack_from("<I", dirb, off)[0]; off += 4
    sizes = list(struct.unpack_from(f"<{nstreams}I", dirb, off)); off += 4 * nstreams
    blocks = []
    for s in sizes:
        nb = 0 if s in (0, 0xFFFFFFFF) else (s + bs - 1) // bs
        blocks.append(list(struct.unpack_from(f"<{nb}I", dirb, off))); off += 4 * nb

    def read_stream(i):
        return b"".join(bytes(d[b * bs:b * bs + bs]) for b in blocks[i])[:sizes[i]]

    TPI = 2  # TPI is always stream 2
    tpi = read_stream(TPI)
    tib, tie, _recbytes = struct.unpack_from("<III", tpi, 8)
    hash_stream = struct.unpack_from("<H", tpi, 20)[0]
    buckets = struct.unpack_from("<I", tpi, 28)[0]
    idx_off, idx_len = struct.unpack_from("<iI", tpi, 40)
    adj_off, adj_len = struct.unpack_from("<iI", tpi, 48)

    # Compute one hash value per type record, in index order.
    hashes = []
    pos = 56  # records follow the 56-byte TPI header
    for _ in range(tie - tib):
        ln = struct.unpack_from("<H", tpi, pos)[0]
        rec = tpi[pos:pos + 2 + ln]
        kind = struct.unpack_from("<H", rec, 2)[0]
        h = None
        if kind in UDT_KINDS:
            prop = struct.unpack_from("<H", rec, 6)[0]  # ClassOptions after count
            fwd, scoped, uniq = prop & 0x80, prop & 0x100, prop & 0x200
            if not fwd and not scoped and not uniq:
                p = 4 + 4  # skip len+kind, count+property
                p += {0x1505: 12, 0x1504: 12, 0x1506: 4, 0x1507: 8}[kind]
                if kind != 0x1507:  # struct/class/union: numeric size leaf
                    if struct.unpack_from("<H", rec, p)[0] < 0x8000:
                        p += 2
                name = rec[p:rec.index(b"\x00", p)]
                h = _hash_string_v1(name) % buckets
        if h is None:
            h = _jam_crc(rec) % buckets
        hashes.append(h)
        pos += 2 + ln

    hv = b"".join(struct.pack("<I", h) for h in hashes)  # HashValueBuffer
    ib = read_stream(hash_stream)[idx_off:idx_off + idx_len]  # preserve IndexOffsetBuffer
    ab = read_stream(hash_stream)[adj_off:adj_off + adj_len]  # preserve HashAdjBuffer (empty)
    new_hash = hv + ib + ab
    if len(new_hash) > len(blocks[hash_stream]) * bs:
        sys.exit("TPI hash stream would exceed its MSF block; grow allocation")

    # Rewrite the TPI header hash offsets (relative to the hash stream start).
    hdr = blocks[TPI][0] * bs
    struct.pack_into("<iI", d, hdr + 32, 0, len(hv))                 # HashValueBuffer
    struct.pack_into("<iI", d, hdr + 40, len(hv), idx_len)           # IndexOffsetBuffer
    struct.pack_into("<iI", d, hdr + 48, len(hv) + idx_len, adj_len)  # HashAdjBuffer

    # Write the new hash stream content into its block, and fix its size in the
    # stream directory (StreamSizes[hash_stream]).
    hb = blocks[hash_stream][0] * bs
    d[hb:hb + len(new_hash)] = new_hash
    size_logoff = 4 + hash_stream * 4
    size_fileoff = dir_blk[size_logoff // bs] * bs + (size_logoff % bs)
    struct.pack_into("<I", d, size_fileoff, len(new_hash))

    open(pdb_path, "wb").write(d)
    return len(hashes)


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
    # yaml2pdb leaves the TPI HashValueBuffer empty; fill it so dbghelp can
    # resolve types by name (`dt nt!_EPROCESS`) instead of loading public-only.
    n_types = patch_tpi_hash(out_pdb)
    # Sanity-count what actually landed.
    dump = subprocess.run([pdbutil, "dump", "-publics", out_pdb], capture_output=True, text=True).stdout
    n_pub = dump.count("S_PUB32")

    # Companion ntoskrnl.exe stub: WinDbg reads the RSDS from the image *file*,
    # so it needs this alongside the PDB (matching the masquerade header in the
    # dump). Written next to the PDB, named ntoskrnl.exe.
    exe_path = os.path.join(os.path.dirname(out_pdb) or ".", "ntoskrnl.exe")
    write_pe_stub(exe_path)

    print(f"wrote {out_pdb}: {n_pub} public symbols (of {len(syms)} defined), {n_types} TPI type hashes")
    print(f"wrote {exe_path}: ntoskrnl.exe PE stub (RSDS -> ntoskrnl.pdb)")
    print("WinDbg: put BOTH on a local path (e.g. C:\\sym), then in the dump:")
    print("  .exepath+ C:\\sym ; .sympath+ C:\\sym ; .reload /f")
    print("  (symbols resolve as KERNEL_VIRT_BASE + RVA; the kernel links at 0)")


if __name__ == "__main__":
    main()
