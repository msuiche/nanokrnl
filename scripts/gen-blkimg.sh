#!/bin/sh
# Generate target/test-blk.img: a 1 MiB raw scratch disk for the virtio-blk
# self-test. Sector 0 carries a magic marker and the 0x55AA boot signature;
# sector 2 is left zero for the write-back check.
set -eu
cd "$(dirname "$0")/.."
IMG=target/test-blk.img
mkdir -p target
if [ ! -f "$IMG" ]; then
    dd if=/dev/zero of="$IMG" bs=1M count=1 2>/dev/null
    # Magic at the start of sector 0, boot signature at the end.
    printf 'NANOBLK1' | dd of="$IMG" bs=1 seek=0 conv=notrunc 2>/dev/null
    printf '\125\252' | dd of="$IMG" bs=1 seek=510 conv=notrunc 2>/dev/null
    echo "generated $IMG"
fi
