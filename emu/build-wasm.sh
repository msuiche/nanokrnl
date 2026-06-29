#!/bin/sh
# Build the ntemu browser emulator (Rust → wasm32) and stage it for the web shim.
#
#   sh emu/build-wasm.sh
#   (cd web/ntemu && python3 -m http.server 8000)   # open http://localhost:8000
#
# Output: a single self-contained ~40 KB ntemu.wasm — no threads, no
# SharedArrayBuffer, no COOP/COEP (unlike qemu-wasm). The JS shim drives it
# through the exported C ABI (see src/wasm.rs).
set -eu
cd "$(dirname "$0")"

cargo build --release --target wasm32-unknown-unknown
OUT=../web/ntemu
mkdir -p "$OUT"
cp target/wasm32-unknown-unknown/release/ntemu.wasm "$OUT/ntemu.wasm"
ls -lh "$OUT/ntemu.wasm"
echo "staged $OUT/ntemu.wasm — serve web/ntemu/ and open it"
