#!/bin/sh
# Boot the kernel with an interactive cmd.exe attached to the serial console.
#
# Unlike scripts/qemu-test.sh (deterministic canned input), this builds the
# kernel with the `interactive` feature: the console-app smoke tests are
# skipped and cmd.exe runs as a live shell reading your keystrokes from COM1.
# Type commands at the `C:\>` prompt; `exit` quits cmd and the VM shuts down.
# To force-quit QEMU instead, press Ctrl-C.
#
# Note: we use plain `-serial stdio` (not `mon:stdio`) so every keystroke goes
# straight to the guest's serial port. `mon:stdio` muxes the QEMU monitor onto
# the same terminal and steals input focus, so keys reach the monitor instead
# of cmd (and a stray key can quit QEMU).
set -eu
cd "$(dirname "$0")/.."

# cmd needs the kernel32 + msvcrt shim DLLs embedded in the kernel image.
sh scripts/build-kernel32.sh
sh scripts/build-msvcrt.sh

# Build the interactive kernel, then the bootable disk image from it.
cargo build -p kernel --features interactive --target x86_64-unknown-none
cargo +nightly run -q -p boot -- target/x86_64-unknown-none/debug/kernel

IMG=target/x86_64-unknown-none/debug/disk-bios.img

# A stale QEMU holds the disk-image write lock; clear it first.
pkill -9 qemu-system-x86_64 2>/dev/null || true

echo "=== interactive cmd.exe — wait for the C:\\> prompt, type commands, 'exit' to quit (Ctrl-C force-quits) ==="
exec qemu-system-x86_64 \
    -drive format=raw,file="$IMG" \
    -cpu qemu64,+smep,+smap \
    -serial stdio \
    -device isa-debug-exit,iobase=0xf4,iosize=0x04 \
    -display none \
    -no-reboot
