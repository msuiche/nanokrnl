#!/bin/sh
# Build the msvcrt shim DLL.
#
# A freestanding PE DLL for x86_64-pc-windows-msvc, linked with lld-link: no
# CRT, /dll, entry DllMain, and an /EXPORT: directive per exported symbol so
# they land in the export table the loader resolves against. Export names are
# derived from src/lib.rs so they never drift from the code:
#   * `#[no_mangle] pub ... extern "C" fn NAME` -> /EXPORT:NAME
#   * `#[export_name = "DECORATED"] ... fn ...`  -> /EXPORT:DECORATED  (e.g. ?terminate@@YAXXZ)
#   * `#[no_mangle] pub static mut NAME`         -> /EXPORT:NAME,DATA
set -eu
set -f # keep glob chars in decorated names (e.g. '?') literal
cd "$(dirname "$0")/.."
WS="$PWD"

CARGO="${CARGO:-$HOME/.cargo/bin/cargo}"
SRC=msvcrt/src/lib.rs

# Functions, excluding the DllMain entry and any carrying an #[export_name]
# (those are exported under their decorated name below, not their Rust name).
FUNCS=$(grep -oE 'pub (unsafe )?extern "C" fn [A-Za-z0-9_]+' "$SRC" \
    | awk '{print $NF}' | grep -v '^DllMain$' | grep -v '^cpp_terminate$')
# Decorated export names from #[export_name = "..."] attributes.
EXPNAMES=$(grep -oE '#\[export_name = "[^"]+"\]' "$SRC" | sed -E 's/.*"([^"]+)".*/\1/')
# Data exports (CRT mode-flag globals).
DATA=$(grep -oE 'pub static mut [A-Za-z0-9_]+' "$SRC" | awk '{print $NF}')

EXPORT_ARGS=""
for name in $FUNCS $EXPNAMES; do
    EXPORT_ARGS="$EXPORT_ARGS -Clink-arg=/EXPORT:$name"
done
for name in $DATA; do
    EXPORT_ARGS="$EXPORT_ARGS -Clink-arg=/EXPORT:$name,DATA"
done
echo "msvcrt exports: $FUNCS $EXPNAMES (data: $DATA)"

cd msvcrt
RUSTFLAGS="-Clink-arg=/dll -Clink-arg=/entry:DllMain -Clink-arg=/nodefaultlib $EXPORT_ARGS" \
    "$CARGO" +nightly build --release

cp target/x86_64-pc-windows-msvc/release/msvcrt.dll ./msvcrt.dll
cd "$WS"
echo "msvcrt/msvcrt.dll:"
ls -l msvcrt/msvcrt.dll