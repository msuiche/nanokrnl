#!/usr/bin/env python3
"""Generate a small, valid Windows registry hive (regf) for nanokrnl's
cm::hive loader test. Emits hives/system.hive with:

  SYSTEM (root)
  └── ControlSet001
      └── Control
          └── HiveTest
              ├── Signature  REG_DWORD  0x00C0FFEE
              └── Greeting   REG_SZ     "hello hive"

The file is structurally a real hive: base block, one hbin, nk/vk/lh cells
with correct indices, negative cell sizes, 8-byte alignment, and the base
block checksum. Windows' own regedit would open it.
"""

import struct
import sys

OUT = sys.argv[1] if len(sys.argv) > 1 else "hives/system.hive"


def cell(payload: bytes) -> bytes:
    size = (4 + len(payload) + 7) & ~7
    return struct.pack("<i", -size) + payload + b"\0" * (size - 4 - len(payload))


def nk(flags: int, parent: int, sub_count: int, sub_list: int, val_count: int, val_list: int, name: bytes) -> bytes:
    rec = bytearray()
    rec += b"nk"
    rec += struct.pack("<H", flags)
    rec += b"\0" * 8                      # last-write timestamp
    rec += struct.pack("<I", 0)           # spare
    rec += struct.pack("<i", parent)
    rec += struct.pack("<I", sub_count)   # stable subkey count
    rec += struct.pack("<I", 0)           # volatile subkey count
    rec += struct.pack("<i", sub_list)    # stable subkey list
    rec += struct.pack("<i", -1)          # volatile subkey list
    rec += struct.pack("<I", val_count)
    rec += struct.pack("<i", val_list)
    rec += struct.pack("<i", -1)          # security
    rec += struct.pack("<i", -1)          # class
    rec += struct.pack("<IIII", 0, 0, 0, 0)  # max name/class/value-name/value-data
    rec += struct.pack("<I", 0)           # workvar
    rec += struct.pack("<H", len(name))
    rec += struct.pack("<H", 0)           # class length
    rec += name
    return bytes(rec)


def lh(entries: list[tuple[int, int]]) -> bytes:
    rec = bytearray(b"lh") + struct.pack("<H", len(entries))
    for idx, hint in entries:
        rec += struct.pack("<iI", idx, hint)
    return bytes(rec)


def val_list(indices: list[int]) -> bytes:
    return b"".join(struct.pack("<i", i) for i in indices)


def vk_inline(name: bytes, vtype: int, data: bytes) -> bytes:
    assert len(data) <= 4
    rec = bytearray(b"vk")
    rec += struct.pack("<H", len(name))
    rec += struct.pack("<I", 0x8000_0000 | len(data))
    rec += data.ljust(4, b"\0")
    rec += struct.pack("<I", vtype)
    rec += struct.pack("<H", 1)  # named
    rec += struct.pack("<H", 0)
    rec += name
    return bytes(rec)


def vk(name: bytes, vtype: int, data_idx: int, data_len: int) -> bytes:
    rec = bytearray(b"vk")
    rec += struct.pack("<H", len(name))
    rec += struct.pack("<I", data_len)
    rec += struct.pack("<i", data_idx)
    rec += struct.pack("<I", vtype)
    rec += struct.pack("<H", 1)  # named
    rec += struct.pack("<H", 0)
    rec += name
    return bytes(rec)


def main() -> None:
    greeting = "hello hive".encode("utf-16-le") + b"\0\0"

    # Final cell contents, with correct sizes but zero references (references
    # never change a record's size, so offsets computed from these hold after
    # patching the references in).
    templates = [
        cell(greeting),                                        # 0 Greeting data
        cell(vk_inline(b"Signature", 4, b"\0\0\0\0")),         # 1
        cell(vk(b"Greeting", 1, 0, len(greeting))),            # 2
        cell(val_list([0, 0])),                                # 3
        cell(nk(0x20, 0, 0, 0, 2, 0, b"HiveTest")),            # 4
        cell(lh([(0, 0)])),                                    # 5
        cell(nk(0x20, 0, 1, 0, 0, 0, b"Control")),             # 6
        cell(lh([(0, 0)])),                                    # 7
        cell(nk(0x20, 0, 1, 0, 0, 0, b"ControlSet001")),       # 8
        cell(lh([(0, 0)])),                                    # 9
        cell(nk(0x2C, -1, 1, 0, 0, 0, b"SYSTEM")),             # 10
    ]

    offsets = []
    off = 0x20  # hbin header size
    for c in templates:
        offsets.append(off)
        off += len(c)

    # Same records with real cross-references (identical sizes).
    cells = [
        templates[0],
        cell(vk_inline(b"Signature", 4, struct.pack("<I", 0x00C0FFEE))),
        cell(vk(b"Greeting", 1, offsets[0], len(greeting))),
        cell(val_list([offsets[1], offsets[2]])),
        cell(nk(0x20, offsets[6], 0, -1, 2, offsets[3], b"HiveTest")),
        cell(lh([(offsets[4], 0)])),
        cell(nk(0x20, offsets[8], 1, offsets[5], 0, -1, b"Control")),
        cell(lh([(offsets[6], 0)])),
        cell(nk(0x20, offsets[10], 1, offsets[7], 0, -1, b"ControlSet001")),
        cell(lh([(offsets[8], 0)])),
        cell(nk(0x2C, -1, 1, offsets[9], 0, -1, b"SYSTEM")),
    ]
    assert [len(a) for a in cells] == [len(a) for a in templates]

    body = b"".join(cells)
    hbin_size = (0x20 + len(body) + 4095) & ~4095  # hbin size is page-rounded

    hbin = bytearray()
    hbin += b"hbin"
    hbin += struct.pack("<I", 0)            # file offset of this hbin
    hbin += struct.pack("<I", hbin_size)
    hbin += b"\0" * 8                       # reserved
    hbin += b"\0" * 8                       # timestamp
    hbin += b"\0" * 4                       # spare
    hbin += body
    hbin += b"\0" * (hbin_size - len(hbin))

    base = bytearray(4096)
    base[0:4] = b"regf"
    struct.pack_into("<I", base, 4, 1)      # sequence1
    struct.pack_into("<I", base, 8, 1)      # sequence2
    # timestamp left zero.
    struct.pack_into("<I", base, 0x1C, 1)   # major version
    struct.pack_into("<I", base, 0x20, 1)   # minor version
    struct.pack_into("<I", base, 0x24, offsets[10])  # root cell index
    struct.pack_into("<I", base, 0x28, hbin_size)    # hbins size
    struct.pack_into("<I", base, 0x2C, 1)   # cluster factor
    # Checksum: XOR of dwords over the first 0x1F8 bytes.
    csum = 0
    for i in range(0, 0x1F8, 4):
        csum ^= struct.unpack_from("<I", base, i)[0]
    struct.pack_into("<I", base, 0x1FC, csum)

    with open(OUT, "wb") as f:
        f.write(base)
        f.write(hbin)
    print(f"wrote {OUT}: {4096 + len(hbin)} bytes, root cell at index {offsets[10]}")


if __name__ == "__main__":
    main()
