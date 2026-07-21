#!/bin/sh
# Build sehtest.exe: a freestanding C console binary with real frame-based
# SEH (__try/__except/__finally), compiled with clang (MSVC ABI) and linked
# against the shim import libraries. Produces sehtest/sehtest.exe, which the
# kernel's build.rs embeds for the SEH boot self-test.
#
# -fasync-exceptions is required: without it clang treats hardware faults
# (the AVs this test raises) as outside the SEH model and emits no handler
# metadata at all (/EHsc vs /EHa on MSVC).
set -eu
cd "$(dirname "$0")/.."
WS="$PWD"

CLANG="${CLANG:-/opt/homebrew/opt/llvm/bin/clang}"
LLD_LINK="${LLD_LINK:-/opt/homebrew/bin/lld-link}"
DLLTOOL="${DLLTOOL:-/opt/homebrew/opt/llvm/bin/llvm-dlltool}"

# Same import-library generation as scripts/build-app.sh (kept in sync):
# shim export names are scraped from the shim sources.
gen_implib() {
    shim="$1" dll="$2" def="sehtest/$3.def" lib="sehtest/$3.lib"
    echo "LIBRARY $dll" > "$def"
    echo "EXPORTS" >> "$def"
    grep -oE 'pub (unsafe )?extern "C" fn [A-Za-z0-9_]+' "$shim" \
        | awk '{print $NF}' | grep -v '^DllMain$' >> "$def"
    "$DLLTOOL" -m i386:x86-64 -d "$def" -l "$lib" -D "$dll"
}
gen_implib kernel32/src/lib.rs kernel32.dll kernel32
gen_implib msvcrt/src/lib.rs   msvcrt.dll   msvcrt

"$CLANG" --target=x86_64-pc-windows-msvc -O1 -fasync-exceptions -fno-stack-protector \
    -c sehtest/sehtest.c -o target/sehtest.obj
"$LLD_LINK" /subsystem:console /entry:mainCRTStartup /nodefaultlib \
    /out:sehtest/sehtest.exe target/sehtest.obj "$WS/sehtest/kernel32.lib" "$WS/sehtest/msvcrt.lib"
ls -l sehtest/sehtest.exe
