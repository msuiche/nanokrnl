#!/usr/bin/env python3
r"""Generate target/test-blk.img: a 16 MiB disk holding a 4 MiB FAT32
superfloppy (no partition table) plus a 12 MiB raw pagefile region, for
the nanokrnl storage and paging tests.

Layout:
  sector 0:        BPB (OEM name "NANOBLK1", 0x55AA boot signature)
  sectors 1-31:    reserved (sector 2 is the vblk write-test scratch area)
  FAT1, FAT2:      64 sectors each (8192 clusters x 4 bytes)
  data region:     HELLO.TXT, README.TXT in the root; SUB\NESTED.TXT nested
  sectors 0..8192: the FAT32 volume (BPB total = 8192)
  sectors 8192+:   raw pagefile region (3072 page slots x 8 sectors) —
                   deliberately outside the volume: paging must never
                   recurse into the filesystem it might be paging out.

One sector per cluster keeps the geometry trivially verifiable.
"""

import struct

SECTOR = 512
SPC = 1                      # sectors per cluster
RESERVED = 32                # reserved sectors (incl. BPB at 0)
NUM_FATS = 2
TOTAL_SECTORS = 4 * 1024 * 1024 // SECTOR   # 8192: the FAT32 volume size
IMAGE_SECTORS = 16 * 1024 * 1024 // SECTOR  # 32768: the disk image size
CLUSTERS = TOTAL_SECTORS - RESERVED        # data-region clusters (cluster 2 = first)
FAT_SECTORS = (CLUSTERS * 4 + SECTOR - 1) // SECTOR
DATA_START = RESERVED + NUM_FATS * FAT_SECTORS

HELLO = b"Hello from the FAT32 drive\n"
README = b"FAT32 read path works. Boot-suite reads this from D:\\.\n"
NESTED = b"a nested file inside SUB\n"

def sector0():
    b = bytearray(SECTOR)
    b[0:3] = b"\xeb\x58\x90"               # jump + nop
    b[3:11] = b"NANOBLK1"                  # OEM name (also the vblk marker)
    struct.pack_into("<H", b, 11, SECTOR)  # bytes/sector
    b[13] = SPC                            # sectors/cluster
    struct.pack_into("<H", b, 14, RESERVED)
    b[16] = NUM_FATS
    struct.pack_into("<H", b, 17, 0)       # root entries (0 for FAT32)
    struct.pack_into("<H", b, 19, 0)       # total16 (0; use total32)
    b[21] = 0xF8                           # media: fixed disk
    struct.pack_into("<H", b, 22, 0)       # fat size 16 (0 for FAT32)
    struct.pack_into("<H", b, 24, 1)       # sectors/track
    struct.pack_into("<H", b, 26, 1)       # heads
    struct.pack_into("<I", b, 28, 0)       # hidden sectors
    struct.pack_into("<I", b, 32, TOTAL_SECTORS)
    struct.pack_into("<I", b, 36, FAT_SECTORS)
    struct.pack_into("<H", b, 40, 0)       # ext flags
    struct.pack_into("<H", b, 42, 0)       # fs version
    struct.pack_into("<I", b, 44, 2)       # root cluster
    struct.pack_into("<H", b, 48, 0)       # fsinfo sector (0 = none)
    struct.pack_into("<H", b, 50, 0)       # backup boot sector (0 = none)
    b[66] = 0x29                           # drive signature
    struct.pack_into("<I", b, 67, 0xC0FFEE)
    b[71:82] = b"FAT32 drive"
    b[82:90] = b"FAT32   "
    b[510:512] = b"\x55\xaa"
    return bytes(b)

def fat_entry(v):
    return struct.pack("<I", v & 0x0FFFFFFF)

def build_fats():
    """Two identical FAT copies: cluster chain map.

    cluster 2 = root dir, 3 = HELLO, 4 = README, 5 = SUB, 6 = NESTED.
    """
    EOC = 0x0FFFFFFF
    entries = [0x0FFFFFF8, 0xFFFFFFFF, EOC, EOC, EOC, EOC, EOC]
    fat = bytearray()
    for e in entries:
        fat += fat_entry(e)
    fat += b"\0" * (FAT_SECTORS * SECTOR - len(fat))
    return fat

def dirent(name83: bytes, attr: int, cluster: int, size: int) -> bytes:
    assert len(name83) == 11
    d = bytearray(32)
    d[0:11] = name83
    d[11] = attr
    struct.pack_into("<H", d, 20, (cluster >> 16) & 0xFFFF)
    struct.pack_into("<H", d, 26, cluster & 0xFFFF)
    struct.pack_into("<I", d, 28, size)
    return bytes(d)

def data_region():
    root = (
        dirent(b"HELLO   TXT", 0x20, 3, len(HELLO))
        + dirent(b"README  TXT", 0x20, 4, len(README))
        + dirent(b"SUB        ", 0x10, 5, 0)
    )
    root += b"\0" * (SECTOR - len(root))
    sub = dirent(b"NESTED  TXT", 0x20, 6, len(NESTED))
    sub += b"\0" * (SECTOR - len(sub))
    region = root + HELLO.ljust(SECTOR, b"\0") + README.ljust(SECTOR, b"\0") + sub + NESTED.ljust(SECTOR, b"\0")
    region += b"\0" * (TOTAL_SECTORS * SECTOR - (DATA_START * SECTOR) - len(region))
    return region

def main():
    img = sector0()
    img += b"\0" * ((RESERVED - 1) * SECTOR)
    img += build_fats()
    img += build_fats()
    img += data_region()
    assert len(img) == TOTAL_SECTORS * SECTOR, len(img)
    img += b"\0" * ((IMAGE_SECTORS - TOTAL_SECTORS) * SECTOR)  # pagefile region
    with open("target/test-blk.img", "wb") as f:
        f.write(img)
    print(f"wrote target/test-blk.img: {len(img)} bytes, FAT1/2 @ {RESERVED}, data @ {DATA_START}, pagefile @ {TOTAL_SECTORS}")

if __name__ == "__main__":
    main()
