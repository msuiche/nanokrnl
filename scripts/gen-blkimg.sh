#!/bin/sh
# Generate target/test-blk.img: the scratch disk for the storage self-test
# (a 4 MiB FAT32 volume: BPB, FATs, HELLO.TXT / README.TXT / SUB\NESTED.TXT,
# the NANOBLK1 OEM-name marker + 0x55AA signature the vblk test checks)
# followed by a 12 MiB raw pagefile region for the paging self-test.
set -eu
cd "$(dirname "$0")/.."
mkdir -p target
if [ ! -f target/test-blk.img ]; then
    python3 tools/gen_fat32.py
fi
