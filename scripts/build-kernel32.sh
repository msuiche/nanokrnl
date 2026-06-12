#!/bin/sh
# Build the kernel32 shim DLL.
#
# A freestanding PE DLL for x86_64-pc-windows-msvc, linked with lld-link: no
# CRT, /dll, entry DllMain, and an /EXPORT: directive per exported function so
# they land in the export table the loader resolves against. The exported
# names are derived from the `#[no_mangle] pub ... extern "C" fn` definitions
# in src/lib.rs, so the .def/exports never drift from the code.
set -eu
cd "$(dirname "$0")/.."
WS="$PWD"

CARGO="${CARGO:-$HOME/.cargo/bin/cargo}"

# Collect exported function names (every no_mangle extern "C" fn except the
# DllMain entry).
EXPORTS=$(grep -oE 'pub (unsafe )?extern "C" fn [A-Za-z0-9_]+' kernel32/src/lib.rs \
    | awk '{print $NF}' | grep -v '^DllMain$')

EXPORT_ARGS=""
for name in $EXPORTS; do
    EXPORT_ARGS="$EXPORT_ARGS -Clink-arg=/EXPORT:$name"
done
echo "kernel32 exports:$EXPORTS"

cd kernel32
RUSTFLAGS="-Clink-arg=/dll -Clink-arg=/entry:DllMain -Clink-arg=/nodefaultlib $EXPORT_ARGS" \
    "$CARGO" +nightly build --release

cp target/x86_64-pc-windows-msvc/release/kernel32.dll ./kernel32.dll
cd "$WS"
echo "kernel32/kernel32.dll:"
ls -l kernel32/kernel32.dll
