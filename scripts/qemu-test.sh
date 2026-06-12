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

cargo build -p kernel --target x86_64-unknown-none

# Watchdog: prefer GNU timeout's --foreground (keeps the command in the
# terminal's process group); fall back to plain timeout, or none at all.
WATCHDOG=""
if command -v timeout >/dev/null 2>&1; then
    if timeout --foreground 1 true 2>/dev/null; then
        WATCHDOG="timeout --foreground 60"
    else
        WATCHDOG="timeout 60"
    fi
fi

# The boot crate (not the kernel) needs nightly: the `bootloader` build
# script compiles its 16/32-bit boot stages with -Zbuild-std.
#
# stdin is redirected from /dev/null: QEMU's `-serial stdio` puts a real
# terminal into raw mode, and when a watchdog runs it in a background
# process group that tcsetattr stops the whole pipeline with SIGTTOU
# (the kernel needs no serial *input* anyway).
set +e
$WATCHDOG cargo +nightly run -q -p boot -- \
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
