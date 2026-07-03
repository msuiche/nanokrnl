#!/usr/bin/env python3
"""Walk nanokrnl's KDDEBUGGER_DATA64 + PsLoadedModuleList + PsActiveProcessHead in
an ELF core, the way a Windows debugger's `lm` and `!process 0 0` do.

This is the symbol-free proxy for those debugger commands: it finds
KdDebuggerDataBlock (from the core's VMCOREINFO anchors, matching how a debugger
resolves the symbol), verifies the 'KDBG' tag, then follows the module and
process list rings and prints them. If this walks cleanly, the structures are
NT-shaped and a debugger that loads the DWARF at the kernel's load base sees the
same thing.

    python3 tools/kdbg_check.py /tmp/nanokrnl.core
"""
import struct
import sys


class Core:
    def __init__(self, path):
        self.d = open(path, "rb").read()
        # Parse PT_LOAD segments into (vaddr, memsz, file_off).
        e_phoff = struct.unpack_from("<Q", self.d, 0x20)[0]
        e_phnum = struct.unpack_from("<H", self.d, 0x38)[0]
        self.loads = []
        self.notes = []
        for i in range(e_phnum):
            o = e_phoff + i * 56
            p_type = struct.unpack_from("<I", self.d, o)[0]
            p_off = struct.unpack_from("<Q", self.d, o + 8)[0]
            p_vaddr = struct.unpack_from("<Q", self.d, o + 16)[0]
            p_filesz = struct.unpack_from("<Q", self.d, o + 32)[0]
            p_memsz = struct.unpack_from("<Q", self.d, o + 40)[0]
            if p_type == 1:  # PT_LOAD
                self.loads.append((p_vaddr, p_memsz, p_off, p_filesz))
            elif p_type == 4:  # PT_NOTE
                self.notes.append((p_off, p_filesz))

    def read(self, va, n):
        for vaddr, memsz, off, filesz in self.loads:
            if vaddr <= va < vaddr + memsz:
                fo = off + (va - vaddr)
                if va - vaddr + n <= filesz:
                    return self.d[fo:fo + n]
                break
        raise KeyError(f"VA {va:#x} not in any PT_LOAD file range")

    def u16(self, va):
        return struct.unpack("<H", self.read(va, 2))[0]

    def u32(self, va):
        return struct.unpack("<I", self.read(va, 4))[0]

    def u64(self, va):
        return struct.unpack("<Q", self.read(va, 8))[0]

    def vmcoreinfo(self):
        """Return the concatenated VMCOREINFO note text."""
        text = b""
        for off, size in self.notes:
            blob = self.d[off:off + size]
            p = 0
            while p + 12 <= len(blob):
                namesz, descsz, ntype = struct.unpack_from("<III", blob, p)
                p += 12
                name = blob[p:p + namesz]
                p += (namesz + 3) & ~3
                desc = blob[p:p + descsz]
                p += (descsz + 3) & ~3
                if name.startswith(b"VMCOREINFO"):
                    text += desc
        return text.decode("latin1")


def anchor(text, key):
    for line in text.splitlines():
        if line.startswith(f"SYMBOL({key})="):
            return int(line.split("=", 1)[1], 16)
    raise KeyError(f"no VMCOREINFO anchor for {key}")


def read_unicode(c, va_str):
    """Read a UNICODE_STRING at va_str -> python str."""
    length = c.u16(va_str)          # bytes
    buf = c.u64(va_str + 8)
    raw = c.read(buf, length)
    return raw.decode("utf-16-le", "replace")


def read_cstr(b):
    z = b.find(b"\x00")
    return b[:z if z >= 0 else len(b)].decode("latin1")


def main():
    path = sys.argv[1] if len(sys.argv) > 1 else "/tmp/nanokrnl.core"
    c = Core(path)
    vci = c.vmcoreinfo()

    kdbg = anchor(vci, "KdDebuggerDataBlock")
    # KDDEBUGGER_DATA64: Header{List(16), OwnerTag(4), Size(4)}, KernBase@0x18,
    # PsLoadedModuleList@0x48, PsActiveProcessHead@0x50.
    owner = c.read(kdbg + 16, 4)
    size = c.u32(kdbg + 20)
    kern_base = c.u64(kdbg + 0x18)
    ps_mod = c.u64(kdbg + 0x48)
    ps_proc = c.u64(kdbg + 0x50)
    print(f"KdDebuggerDataBlock @ {kdbg:#x}")
    print(f"  OwnerTag = {owner!r}  Size = {size:#x}  KernBase = {kern_base:#x}")
    ok = owner == b"KDBG"
    print(f"  'KDBG' tag: {'OK' if ok else 'BAD'}")
    print(f"  PsLoadedModuleList  = {ps_mod:#x}")
    print(f"  PsActiveProcessHead = {ps_proc:#x}")

    # --- lm: walk PsLoadedModuleList via InLoadOrderLinks (offset 0) ---
    print("\n=== lm  (loaded modules) ===")
    print(f"{'start':<18} {'end':<18} {'module'}")
    head = ps_mod
    node = c.u64(head)  # Flink -> first entry's InLoadOrderLinks (at offset 0)
    n = 0
    while node != head and n < 64:
        base = c.u64(node + 0x30)          # DllBase
        img = c.u32(node + 0x40)           # SizeOfImage
        name = read_unicode(c, node + 0x58)  # BaseDllName
        print(f"{base:#018x} {base + img:#018x} {name}")
        node = c.u64(node)                 # InLoadOrderLinks.Flink
        n += 1
    print(f"({n} modules)")

    # --- !process 0 0: walk PsActiveProcessHead via ActiveProcessLinks (+0x08) ---
    print("\n=== !process 0 0  (active processes) ===")
    head = ps_proc
    link = c.u64(head)  # Flink -> first entry's ActiveProcessLinks (offset 0x08)
    n = 0
    while link != head and n < 64:
        eproc = link - 0x08
        pid = c.u64(eproc + 0x00)
        cr3 = c.u64(eproc + 0x18)
        name = read_cstr(c.read(eproc + 0x28, 16))
        print(f"PROCESS {eproc:#018x}  Cid: {pid:#06x}  DirBase: {cr3:#x}  Image: {name}")
        link = c.u64(link)  # ActiveProcessLinks.Flink
        n += 1
    print(f"({n} processes)")

    if not ok:
        sys.exit(1)


if __name__ == "__main__":
    main()
