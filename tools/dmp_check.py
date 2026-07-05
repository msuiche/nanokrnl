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

    # Masquerade PE header at the kernel base: what a Windows debugger reads to
    # find ntoskrnl.pdb (an RSDS whose GUID must match tools/gen_pdb.py).
    kbase = 0xFFFF_8000_0000_0000
    head = c.read(kbase, 0x1c0)
    if head[0:2] == b"MZ":
        e = struct.unpack_from("<I", head, 0x3c)[0]
        pe = head[e:e + 4] == b"PE\x00\x00"
        rsds = c.read(kbase + 0x1a0, 0x25)
        guid = rsds[4:20].hex() if rsds[0:4] == b"RSDS" else "(no RSDS)"
        pdbname = rsds[24:].split(b"\x00")[0].decode("latin1") if rsds[0:4] == b"RSDS" else ""
        print(f"masquerade PE: MZ+PE={pe}  RSDS guid={guid}  pdb='{pdbname}'")
    else:
        print(f"masquerade PE: MISSING (base starts with {head[0:4]!r})")

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

    # KPROCESSOR_STATE: what WinDbg reads for GetContextState / CS-descriptor.
    # KiProcessorBlock @ kdbg+0x218 -> [PKPRCB]; the ProcStateSpecialReg/Context
    # offsets tell it where SpecialRegisters (GDTR/CRs) and the CONTEXT sit.
    print("\n=== KPROCESSOR_STATE (KiProcessorBlock -> KPRCB) ===")
    ki = c.u64(c.kdbg + 0x218)
    size_prcb = c.u16(c.kdbg + 0x2b0)
    off_ctx = c.u16(c.kdbg + 0x2bc)
    off_sr = c.u16(c.kdbg + 0x2ec)
    print(f"  KiProcessorBlock={ki:#x} SizePrcb={size_prcb:#x} OffCtx={off_ctx:#x} OffSpecialReg={off_sr:#x}")
    if ki:
        prcb = c.u64(ki)
        sr = prcb + off_sr
        cr0, cr3sr, cr4 = c.u64(sr + 0x00), c.u64(sr + 0x10), c.u64(sr + 0x18)
        gdt_limit, gdt_base = c.u16(sr + 0x56), c.u64(sr + 0x58)
        idt_base = c.u64(sr + 0x68)
        print(f"  PRCB={prcb:#x}  Cr0={cr0:#x} Cr3={cr3sr:#x} Cr4={cr4:#x}  (header CR3={c.dtb:#x})")
        print(f"  Gdtr base={gdt_base:#x} limit={gdt_limit:#x}  Idtr base={idt_base:#x}")
        ctx = prcb + off_ctx
        segcs, rip = c.u16(ctx + 0x38), c.u64(ctx + 0xf8)
        print(f"  ContextFrame: Cs={segcs:#x} Rip={rip:#x}")
        try:
            idx = segcs >> 3
            d0 = c.u64(gdt_base + idx * 8)
            present, longm, dpl = (d0 >> 47) & 1, (d0 >> 53) & 1, (d0 >> 45) & 3
            print(f"  GDT[{idx}] (Cs={segcs:#x}) = {d0:#018x}  present={present} L={longm} dpl={dpl}")
            print(f"  CS descriptor: {'OK' if present and longm else 'CHECK'} "
                  f"(WinDbg resolves Cs via Gdtr; {'resolvable' if present else 'not present'})")
        except KeyError as e:
            print(f"  GDT NOT readable at Gdtr.Base: {e}")
        # Current thread -> process (what !process reads before walking the list).
        off_ct = c.u16(c.kdbg + 0x2b4)   # OffsetPrcbCurrentThread
        off_apc = c.u16(c.kdbg + 0x2a0)  # OffsetKThreadApcProcess
        cur = c.u64(prcb + off_ct)
        print(f"  OffsetPrcbCurrentThread={off_ct:#x} CurrentThread={cur:#x}")
        try:
            proc = c.u64(cur + off_apc)
            img = read_cstr(c.read(proc + 0x28, 16))
            print(f"  CurrentThread readable; ApcState.Process={proc:#x} Image='{img}'")
        except KeyError as e:
            print(f"  CurrentThread/Process NOT readable: {e}")
        # KPCR: what the engine reads GdtBase from for the CS-descriptor lookup
        # (KPCR = KiProcessorBlock[n] - OffsetPcrContainedPrcb; GdtBase @ KPCR+0).
        off_contained = c.u16(c.kdbg + 0x2e0)  # OffsetPcrContainedPrcb
        off_selfpcr = c.u16(c.kdbg + 0x2dc)     # OffsetPcrSelfPcr
        if off_contained:
            kpcr = prcb - off_contained
            kpcr_gdt = c.u64(kpcr + 0x00)
            kpcr_self = c.u64(kpcr + off_selfpcr)
            print(f"  KPCR={kpcr:#x} (Prcb-{off_contained:#x})  GdtBase={kpcr_gdt:#x} Self={kpcr_self:#x}")
            print(f"  KPCR.GdtBase==Gdtr.Base: {kpcr_gdt == gdt_base}; Self==KPCR: {kpcr_self == kpcr}")
            try:
                d0 = c.u64(kpcr_gdt + (segcs >> 3) * 8)
                print(f"  GDT[{segcs>>3}] via KPCR.GdtBase = {d0:#018x} present={(d0>>47)&1} L={(d0>>53)&1}")
            except KeyError as e:
                print(f"  KPCR.GdtBase GDT read FAILED: {e}")
        else:
            print("  OffsetPcrContainedPrcb=0 - engine can't find KPCR.GdtBase")
    else:
        print("  KiProcessorBlock is 0 - GetContextState will fail")

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
