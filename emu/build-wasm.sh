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
# Prefer the release build: it is smaller and reaches the prompt in far fewer
# guest instructions, so the interpreter boots much faster.
#   cargo build -p kernel --features interactive --release --target x86_64-unknown-none
KERNEL=../target/x86_64-unknown-none/release/kernel
[ -f "$KERNEL" ] || KERNEL=../target/x86_64-unknown-none/debug/kernel
if [ -f "$KERNEL" ]; then
  cp "$KERNEL" "$OUT/kernel.bin"
  echo "staged kernel: $KERNEL"
  ls -lh "$OUT/kernel.bin"

  # Stage matching WinDbg symbols so a visitor who downloads MEMORY.DMP can open
  # it *with symbols*. gen_pdb stamps ntoskrnl.pdb + its ntoskrnl.exe stub with
  # the same fixed RSDS GUID the kernel writes into every dump (kernel/src/dump.rs),
  # so they always pair - no per-dump patching needed. Needs llvm-pdbutil + nm.
  echo "generating WinDbg symbols (ntoskrnl.pdb + ntoskrnl.exe)..."
  if python3 ../tools/gen_pdb.py "$OUT/kernel.bin" --fixed-guid -o "$OUT/ntoskrnl.pdb"; then
    rm -f "$OUT/ntoskrnl.pdb.yaml"
    ls -lh "$OUT/ntoskrnl.pdb" "$OUT/ntoskrnl.exe"
  else
    echo "note: symbol generation skipped (need llvm-pdbutil + nm); dump opens without symbols"
  fi

  # Capture a boot-to-prompt snapshot so the page can resume at C:\> instantly
  # instead of interpreting the whole boot. Gzip it (the page gunzips via
  # DecompressionStream). Falls back to a normal boot if this step is skipped.
  echo "generating boot snapshot..."
  if cargo run --release --example snapshot -- "$KERNEL" "$OUT/snapshot.bin"; then
    gzip -9 -f "$OUT/snapshot.bin"
    ls -lh "$OUT/snapshot.bin.gz"
  else
    echo "note: snapshot generation failed; the page will boot normally"
    rm -f "$OUT/snapshot.bin" "$OUT/snapshot.bin.gz"
  fi
else
  echo "note: kernel ELF not found; build it first (see the command above)"
fi
# Stage the copy-paste debugger bridge so the live site can serve it at
# /bridge.py (the page's Debug panel offers a curl one-liner to run it).
cp ../tools/gdb-bridge.py "$OUT/bridge.py"
echo "staged bridge: $OUT/bridge.py"

echo "staged $OUT — serve web/nanox/ and open it"
