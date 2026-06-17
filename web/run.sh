#!/bin/sh
# Build the interactive kernel + its BIOS disk image and stage it for the
# in-browser emulator (v86). Then serve web/ and open it.
#
#   sh web/run.sh
#   (cd web && python3 -m http.server 8000)   # then open http://localhost:8000
#
# v86 boots the exact same disk image that `qemu -serial stdio` boots; the
# kernel's console is COM1, wired to the page.
set -eu
cd "$(dirname "$0")/.."

# cmd needs the kernel32 + msvcrt shim DLLs embedded in the kernel image.
sh scripts/build-kernel32.sh
sh scripts/build-msvcrt.sh

cargo build -p kernel --features interactive --target x86_64-unknown-none
cargo +nightly run -q -p boot -- target/x86_64-unknown-none/debug/kernel

cp target/x86_64-unknown-none/debug/disk-bios.img web/disk-bios.img
ls -lh web/disk-bios.img
echo "staged web/disk-bios.img — now: (cd web && python3 -m http.server 8000) and open localhost:8000"
