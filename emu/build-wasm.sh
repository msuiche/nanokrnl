#!/bin/sh
# Build the nanox browser emulator (Rust → wasm32) and stage it for the web shim.
#
#   sh emu/build-wasm.sh
#   (cd web/nanox && python3 -m http.server 8000)   # open http://localhost:8000
#
# Output: a single self-contained ~40 KB nanox.wasm — no threads, no
# SharedArrayBuffer, no COOP/COEP (unlike qemu-wasm). The JS shim drives it
# through the exported C ABI (see src/wasm.rs).
set -eu
cd "$(dirname "$0")"

cargo build --release --target wasm32-unknown-unknown
OUT=../web/nanox
mkdir -p "$OUT"
cp target/wasm32-unknown-unknown/release/nanox.wasm "$OUT/nanox.wasm"
ls -lh "$OUT/nanox.wasm"

# Stage the kernel ELF so the page can boot it directly (no BIOS image needed).
KERNEL=../target/x86_64-unknown-none/debug/kernel
if [ -f "$KERNEL" ]; then
  cp "$KERNEL" "$OUT/kernel.bin"
  ls -lh "$OUT/kernel.bin"
else
  echo "note: kernel ELF not found ($KERNEL); build it to enable in-browser boot"
fi
echo "staged $OUT — serve web/nanox/ and open it"
