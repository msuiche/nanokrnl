#!/bin/sh
# Boot the kernel under QEMU and assert that every boot self test passed.
#
# Exit status of the boot runner == QEMU's exit status, which the kernel
# controls via the isa-debug-exit device:
#   33  ((0x10 << 1) | 1)  -> ALL SELF TESTS PASSED
#    3  ((0x01 << 1) | 1)  -> a self test failed or the kernel bugchecked
#  124                     -> watchdog timeout (kernel hung)
#   anything else          -> QEMU/boot infrastructure problem
set -eu
cd "$(dirname "$0")/.."

sh scripts/gen-blkimg.sh

cargo build -p kernel --target x86_64-unknown-none

# The boot crate (not the kernel) needs nightly: the `bootloader` build
# script compiles its 16/32-bit boot stages with -Zbuild-std. CI pins the
# toolchain via NANOKRNL_NIGHTLY (newer nightlies break bootloader
# 0.11.15's UEFI build); locally the plain `nightly` default is fine.
# Build outside the watchdog: on CI this compile is cold (the bootloader
# stage builds land in /tmp, outside the cargo cache) and can exceed the
# watchdog window on its own.
cargo +"${NANOKRNL_NIGHTLY:-nightly}" build -q -p boot

# Watchdog: prefer GNU timeout's --foreground (keeps the command in the
# terminal's process group); fall back to plain timeout, or none at all.
# NANOKRNL_WATCHDOG overrides the 60s default (CI runs without KVM, so
# the TCG boot is slower than on a dev machine).
WATCHDOG=""
if command -v timeout >/dev/null 2>&1; then
    if timeout --foreground 1 true 2>/dev/null; then
        WATCHDOG="timeout --foreground ${NANOKRNL_WATCHDOG:-60}"
    else
        WATCHDOG="timeout ${NANOKRNL_WATCHDOG:-60}"
    fi
fi

# stdin is redirected from /dev/null: QEMU's `-serial stdio` puts a real
# terminal into raw mode, and when a watchdog runs it in a background
# process group that tcsetattr stops the whole pipeline with SIGTTOU
# (the kernel needs no serial *input* anyway).
set +e
$WATCHDOG cargo +"${NANOKRNL_NIGHTLY:-nightly}" run -q -p boot -- \
    target/x86_64-unknown-none/debug/kernel --run < /dev/null
code=$?
set -e

if [ "$code" -eq 33 ]; then
    echo "qemu-test: PASS (exit $code)"
    exit 0
else
    echo "qemu-test: FAIL (exit $code)"
    exit 1
fi
