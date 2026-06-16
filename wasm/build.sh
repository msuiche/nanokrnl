#!/bin/sh
# Build the WebAssembly kernel and stage it for the web host.
#
#   sh wasm/build.sh
#   (cd wasm/web && python3 -m http.server 8000)   # then open localhost:8000
#
# Headless smoke test (no browser): node wasm/web/run-node.mjs
set -eu
cd "$(dirname "$0")"
cargo build --target wasm32-unknown-unknown --release
cp target/wasm32-unknown-unknown/release/ntoskrnl_wasm.wasm web/ntoskrnl_wasm.wasm
ls -l web/ntoskrnl_wasm.wasm
echo "staged web/ntoskrnl_wasm.wasm"
