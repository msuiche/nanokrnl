#!/bin/sh
# Build a ring-3 console app crate into a PE that imports kernel32.dll.
#
# Usage: build-app.sh <app-dir>   (the dir, crate, and [[bin]] name match,
# e.g. "userapp" or "userapp2").
#
# Generates a kernel32 import library from the shim's exported function names
# (so it never drifts from kernel32/src/lib.rs), then links the app with
# lld-link: no CRT, subsystem console, entry mainCRTStartup. At load time the
# kernel loads the kernel32 shim DLL and binds the app's imports to it.
set -eu
cd "$(dirname "$0")/.."
WS="$PWD"

APP="${1:?usage: build-app.sh <app-dir>}"
CARGO="${CARGO:-$HOME/.cargo/bin/cargo}"
DLLTOOL="${DLLTOOL:-/opt/homebrew/opt/llvm/bin/llvm-dlltool}"

# Generate an import library from a shim DLL's source-derived export names,
# so the app's imports never drift from the shim implementation.
gen_implib() {
    shim="$1" dll="$2" def="$APP/$3.def" lib="$APP/$3.lib"
    echo "LIBRARY $dll" > "$def"
    echo "EXPORTS" >> "$def"
    grep -oE 'pub (unsafe )?extern "C" fn [A-Za-z0-9_]+' "$shim" \
        | awk '{print $NF}' | grep -v '^DllMain$' >> "$def"
    "$DLLTOOL" -m i386:x86-64 -d "$def" -l "$lib" -D "$dll"
}
gen_implib kernel32/src/lib.rs kernel32.dll kernel32
gen_implib msvcrt/src/lib.rs   msvcrt.dll   msvcrt

cd "$APP"
RUSTFLAGS="-Clink-arg=/subsystem:console -Clink-arg=/entry:mainCRTStartup -Clink-arg=/nodefaultlib -Clink-arg=$WS/$APP/kernel32.lib -Clink-arg=$WS/$APP/msvcrt.lib" \
    "$CARGO" +nightly build --release
cp "target/x86_64-pc-windows-msvc/release/$APP.exe" "./$APP.exe"
cd "$WS"
echo "$APP/$APP.exe:"
ls -l "$APP/$APP.exe"
