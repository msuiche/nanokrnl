#!/bin/sh
# Build the PE test driver and its import library.
#
# Steps:
#   1. Derive the import names from the kernel export table (single source of
#      truth: kernel/src/ldr/exports.rs) and write ntoskrnl.def.
#   2. Generate ntoskrnl.lib from the .def with llvm-dlltool — the import
#      library the driver links against (resolves names to "ntoskrnl.exe").
#   3. Compile the driver for x86_64-pc-windows-msvc with lld-link, no CRT,
#      subsystem native, entry DriverEntry, linking the import lib.
#   4. Copy the resulting PE to driver/testdriver.sys for the kernel to embed.
set -eu
cd "$(dirname "$0")/.."
WS="$PWD"

CARGO="${CARGO:-$HOME/.cargo/bin/cargo}"
DLLTOOL="${DLLTOOL:-/opt/homebrew/opt/llvm/bin/llvm-dlltool}"

# 1. Export names -> .def. The export names are the only quoted tokens that
#    begin a line inside the kernel_exports! block ("..." => ...); the
#    `extern "win64"` token is always mid-line, so a leading-quote match
#    extracts exactly the names — keeping the import lib in lockstep with the
#    kernel's actual exports.
echo "LIBRARY ntoskrnl.exe" > driver/ntoskrnl.def
echo "EXPORTS" >> driver/ntoskrnl.def
grep -oE '^[[:space:]]+"[A-Za-z0-9]+" =>|^[[:space:]]+"[A-Za-z0-9]+"$' kernel/src/ldr/exports.rs \
    | grep -oE '"[A-Za-z0-9]+"' \
    | tr -d '"' \
    >> driver/ntoskrnl.def

echo "== ntoskrnl.def =="
cat driver/ntoskrnl.def

# 2. Import library.
"$DLLTOOL" -m i386:x86-64 -d driver/ntoskrnl.def -l driver/ntoskrnl.lib -D ntoskrnl.exe
echo "generated driver/ntoskrnl.lib"

# 3. Build the driver. Link args injected here so the import-lib path is
#    absolute; the rest of the config is in driver/.cargo/config.toml.
cd driver
RUSTFLAGS="-Clink-arg=/subsystem:native -Clink-arg=/entry:DriverEntry -Clink-arg=/nodefaultlib -Clink-arg=$WS/driver/ntoskrnl.lib" \
    "$CARGO" +nightly build --release

# 4. Publish the PE where the kernel build script expects it.
cp target/x86_64-pc-windows-msvc/release/testdriver.exe "$WS/driver/testdriver.sys"
cd "$WS"
echo "driver/testdriver.sys:"
ls -l driver/testdriver.sys
