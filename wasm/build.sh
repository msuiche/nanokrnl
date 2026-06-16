#!/bin/sh
# Build the WebAssembly kernel and its guest programs, staged for the web host.
#
#   sh wasm/build.sh
#   (cd wasm/web && python3 -m http.server 8000)   # then open localhost:8000
#
# Headless interactive shell (no browser): node wasm/web/run-node.mjs
set -eu
cd "$(dirname "$0")"

# The kernel module.
cargo build --target wasm32-unknown-unknown --release
cp target/wasm32-unknown-unknown/release/nanokrnl.wasm web/nanokrnl.wasm

# Guest programs (each a separate wasm32 "executable" the kernel runs via `run`).
for prog in programs/*/; do
    name=$(basename "$prog")
    (cd "$prog" && cargo build --target wasm32-unknown-unknown --release)
    cp "$prog/target/wasm32-unknown-unknown/release/$name.wasm" "web/$name.wasm"
done

ls -l web/*.wasm
echo "staged kernel + guest programs"
