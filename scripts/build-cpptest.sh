#!/bin/sh
# Build cpptest.exe: a freestanding C++ console binary exercising real C++
# exceptions (throw/catch/destructor unwind), compiled with clang++ (MSVC
# ABI) and linked against the shim import libraries. Produces
# cpptest/cpptest.exe, which the kernel's build.rs embeds for the C++ EH
# boot test.
set -eu
cd "$(dirname "$0")/.."
WS="$PWD"

CLANG="${CLANG:-/opt/homebrew/opt/llvm/bin/clang++}"
LLD_LINK="${LLD_LINK:-/opt/homebrew/bin/lld-link}"
DLLTOOL="${DLLTOOL:-/opt/homebrew/opt/llvm/bin/llvm-dlltool}"

# Same import-library generation as scripts/build-app.sh (kept in sync).
gen_implib() {
    shim="$1" dll="$2" def="cpptest/$3.def" lib="cpptest/$3.lib"
    echo "LIBRARY $dll" > "$def"
    echo "EXPORTS" >> "$def"
    grep -oE 'pub (unsafe )?extern "C" fn [A-Za-z0-9_]+' "$shim" \
        | awk '{print $NF}' | grep -v '^DllMain$' >> "$def"
    "$DLLTOOL" -m i386:x86-64 -d "$def" -l "$lib" -D "$dll"
}
gen_implib kernel32/src/lib.rs kernel32.dll kernel32
# msvcrt's import library comes from its own link (/IMPLIB, with the data
# exports dlltool's def route can't express — the RTTI vtable is one).

"$CLANG" --target=x86_64-pc-windows-msvc -O1 -fexceptions -fno-stack-protector \
    -c cpptest/cpptest.cc -o target/cpptest.obj
"$LLD_LINK" /subsystem:console /entry:mainCRTStartup /nodefaultlib \
    /out:cpptest/cpptest.exe target/cpptest.obj "$WS/cpptest/kernel32.lib" "$WS/msvcrt/msvcrt.lib"
ls -l cpptest/cpptest.exe
