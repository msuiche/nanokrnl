#!/usr/bin/env python3
"""Walk a nanokrnl Windows crash dump (MEMORY.DMP / DUMP_HEADER64) the way WinDbg
does: read DirectoryTableBase (CR3), walk the captured page tables to translate
virtual addresses against the dumped physical memory, find KdDebuggerDataBlock,
verify the 'KDBG' tag, and follow PsLoadedModuleList / PsActiveProcessHead.

If this walks cleanly, the dump is NT-shaped and an off-the-shelf Windows debugger
that opens it as a kernel target sees the same `lm` / `!process 0 0`.

    python3 tools/dmp_check.py /tmp/MEMORY.DMP
"""
import struct
import sys

PRESENT = 1 << 0
LARGE = 1 << 7
ADDR_MASK = 0x000F_FFFF_FFFF_F000
DMP_HEADER = 0x2000


class Dmp:
    def __init__(self, path):
        self.d = open(path, "rb").read()
        assert self.d[0:4] == b"PAGE", "bad Signature"
        assert self.d[4:8] == b"DU64", "bad ValidDump (not a 64-bit dump)"
        self.dtb = self._h64(0x10)
        self.ps_mod = self._h64(0x20)
        self.ps_proc = self._h64(0x28)
        self.kdbg = self._h64(0x80)
        self.bugcheck = self._h32(0x38)
        # PHYSICAL_MEMORY_DESCRIPTOR @ 0x88: NumberOfRuns, NumberOfPages, runs.
        nruns = self._h32(0x88)
        self.runs = []  # (base_page, page_count, file_page_start)
        cum = 0
        for i in range(nruns):
            base = self._h64(0x98 + i * 16)
            cnt = self._h64(0x98 + i * 16 + 8)
            self.runs.append((base, cnt, cum))
            cum += cnt

    def _h32(self, o):
        return struct.unpack_from("<I", self.d, o)[0]

    def _h64(self, o):
        return struct.unpack_from("<Q", self.d, o)[0]

    def phys(self, pa, n):
        """Read n bytes of physical memory from the dump body."""
        page = pa >> 12
        for base, cnt, fstart in self.runs:
            if base <= page < base + cnt:
                fo = DMP_HEADER + (fstart + (page - base)) * 0x1000 + (pa & 0xFFF)
                return self.d[fo:fo + n]
        raise KeyError(f"PA {pa:#x} not in any physical run")

    def translate(self, va):
        """4-level x86-64 page-table walk from DirectoryTableBase -> PA."""
        def ent(table_pa, idx):
            return struct.unpack("<Q", self.phys(table_pa + idx * 8, 8))[0]
        pml4 = self.dtb & ADDR_MASK
        e4 = ent(pml4, (va >> 39) & 0x1FF)
        if not e4 & PRESENT:
            raise KeyError(f"VA {va:#x}: PML4E not present")
        e3 = ent(e4 & ADDR_MASK, (va >> 30) & 0x1FF)
        if not e3 & PRESENT:
            raise KeyError(f"VA {va:#x}: PDPTE not present")
        if e3 & LARGE:  # 1 GiB
            return (e3 & ADDR_MASK & ~((1 << 30) - 1)) | (va & ((1 << 30) - 1))
        e2 = ent(e3 & ADDR_MASK, (va >> 21) & 0x1FF)
        if not e2 & PRESENT:
            raise KeyError(f"VA {va:#x}: PDE not present")
        if e2 & LARGE:  # 2 MiB
            return (e2 & ADDR_MASK & ~((1 << 21) - 1)) | (va & ((1 << 21) - 1))
        e1 = ent(e2 & ADDR_MASK, (va >> 12) & 0x1FF)
        if not e1 & PRESENT:
            raise KeyError(f"VA {va:#x}: PTE not present")
        return (e1 & ADDR_MASK) | (va & 0xFFF)

    def read(self, va, n):
        out = b""
        while n > 0:
            pa = self.translate(va)
            chunk = min(n, 0x1000 - (va & 0xFFF))
            out += self.phys(pa, chunk)
            va += chunk
            n -= chunk
        return out

    def u16(self, va):
        return struct.unpack("<H", self.read(va, 2))[0]

    def u32(self, va):
        return struct.unpack("<I", self.read(va, 4))[0]

    def u64(self, va):
        return struct.unpack("<Q", self.read(va, 8))[0]


def read_unicode(c, va_str):
    length = c.u16(va_str)
    buf = c.u64(va_str + 8)
    return c.read(buf, length).decode("utf-16-le", "replace")


def read_cstr(b):
    z = b.find(b"\x00")
    return b[:z if z >= 0 else len(b)].decode("latin1")


def main():
    path = sys.argv[1] if len(sys.argv) > 1 else "/tmp/MEMORY.DMP"
    c = Dmp(path)
    print(f"DUMP_HEADER64: DirectoryTableBase={c.dtb:#x}  BugCheck={c.bugcheck:#x}")
    print(f"  KdDebuggerDataBlock = {c.kdbg:#x}  (header)")

    # Crash CONTEXT (KPROCESSOR_STATE.ContextFrame) @ header offset 0x348.
    ctx = 0x348
    cf = struct.unpack_from("<I", c.d, ctx + 0x30)[0]
    rip = struct.unpack_from("<Q", c.d, ctx + 0xf8)[0]
    rsp = struct.unpack_from("<Q", c.d, ctx + 0x98)[0]
    rbp = struct.unpack_from("<Q", c.d, ctx + 0xa0)[0]
    segcs = struct.unpack_from("<H", c.d, ctx + 0x38)[0]
    print(f"  CONTEXT: Flags={cf:#x} Cs={segcs:#x} Rip={rip:#x} Rsp={rsp:#x} Rbp={rbp:#x}")
    if not cf & 0x00100000:
        print("  WARNING: ContextFlags missing CONTEXT_AMD64 bit")

    # KDDEBUGGER_DATA64: Header{List(16), OwnerTag(4), Size(4)}, KernBase@0x18,
    # PsLoadedModuleList@0x48, PsActiveProcessHead@0x50.
    owner = c.read(c.kdbg + 16, 4)
    size = c.u32(c.kdbg + 20)
    kern_base = c.u64(c.kdbg + 0x18)
    ps_mod = c.u64(c.kdbg + 0x48)
    ps_proc = c.u64(c.kdbg + 0x50)
    print(f"  OwnerTag = {owner!r}  Size = {size:#x}  KernBase = {kern_base:#x}")
    ok = owner == b"KDBG"
    print(f"  'KDBG' tag: {'OK' if ok else 'BAD'}")
    print(f"  PsLoadedModuleList  = {ps_mod:#x}  (header says {c.ps_mod:#x})")
    print(f"  PsActiveProcessHead = {ps_proc:#x}  (header says {c.ps_proc:#x})")

    print("\n=== lm  (loaded modules) ===")
    print(f"{'start':<18} {'end':<18} {'module'}")
    head = ps_mod
    node = c.u64(head)
    n = 0
    while node != head and n < 64:
        base = c.u64(node + 0x30)
        img = c.u32(node + 0x40)
        name = read_unicode(c, node + 0x58)
        print(f"{base:#018x} {base + img:#018x} {name}")
        node = c.u64(node)
        n += 1
    print(f"({n} modules)")

    print("\n=== !process 0 0  (active processes) ===")
    head = ps_proc
    link = c.u64(head)
    n = 0
    while link != head and n < 64:
        eproc = link - 0x08
        pid = c.u64(eproc + 0x00)
        cr3 = c.u64(eproc + 0x18)
        name = read_cstr(c.read(eproc + 0x28, 16))
        print(f"PROCESS {eproc:#018x}  Cid: {pid:#06x}  DirBase: {cr3:#x}  Image: {name}")
        link = c.u64(link)
        n += 1
    print(f"({n} processes)")

    if not ok:
        sys.exit(1)


if __name__ == "__main__":
    main()
