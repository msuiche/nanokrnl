#!/bin/sh
# Generate target/test-blk.img: the FAT32 scratch disk for the storage
# self-test (BPB, FATs, HELLO.TXT / README.TXT / SUB\NESTED.TXT, and the
# NANOBLK1 OEM-name marker + 0x55AA signature the vblk test checks).
set -eu
cd "$(dirname "$0")/.."
mkdir -p target
if [ ! -f target/test-blk.img ]; then
    python3 tools/gen_fat32.py
fi
