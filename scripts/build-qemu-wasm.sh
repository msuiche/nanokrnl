#!/bin/bash
# Build a browser-runnable QEMU (TCG -> WebAssembly, via ktock/qemu-wasm) that
# boots the UNMODIFIED ntoskrnl-rs BIOS disk image. Output lands in web/qemu/.
#
# Prerequisites:
#   - Docker (the emscripten build env runs in a container).
#   - The qemu-wasm fork cloned somewhere; point QEMU_WASM_REPO at it.
#   - The `buildqemu` image already built:
#         docker build -t buildqemu - < "$QEMU_WASM_REPO/Dockerfile"
#
# Usage:
#   QEMU_WASM_REPO=/path/to/qemu-wasm sh scripts/build-qemu-wasm.sh
#
# This is heavy: compiling qemu-system-x86_64 to wasm takes many minutes.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
QEMU_WASM_REPO="${QEMU_WASM_REPO:-/tmp/qemu-wasm-work/qemu-wasm}"
CONTAINER=build-ntos-qemu
OUT="${REPO_ROOT}/web/qemu"
IMG="${REPO_ROOT}/target/x86_64-unknown-none/debug/disk-bios.img"

if [ ! -d "$QEMU_WASM_REPO" ]; then
  echo "QEMU_WASM_REPO not found: $QEMU_WASM_REPO" >&2; exit 1
fi
if ! docker image inspect buildqemu >/dev/null 2>&1; then
  echo "buildqemu image missing. Build it first:" >&2
  echo "  docker build -t buildqemu - < \"$QEMU_WASM_REPO/Dockerfile\"" >&2
  exit 1
fi

# 1. Make sure we have the bootable disk image (same one native QEMU boots).
if [ ! -f "$IMG" ]; then
  echo "=== building kernel + disk image ==="
  sh "${REPO_ROOT}/scripts/build-kernel32.sh"
  sh "${REPO_ROOT}/scripts/build-msvcrt.sh"
  ( cd "$REPO_ROOT" && cargo build -p kernel --features interactive --target x86_64-unknown-none )
  ( cd "$REPO_ROOT" && cargo +nightly run -q -p boot -- target/x86_64-unknown-none/debug/kernel )
fi
echo "=== using disk image: $IMG ($(du -h "$IMG" | cut -f1)) ==="

# 2. (Re)start the build container with the qemu-wasm source mounted writable.
# QEMU's meson fetches the dtc (libfdt) subproject into the source tree, so the
# mount must be writable; a read-only mount breaks meson's `subproject('dtc')`.
docker rm -f "$CONTAINER" >/dev/null 2>&1 || true
docker run --rm -d --name "$CONTAINER" -v "${QEMU_WASM_REPO}":/qemu buildqemu

# 3. Configure + compile qemu-system-x86_64 to wasm (the long step).
EXTRA_CFLAGS="-O3 -g -Wno-error=unused-command-line-argument -matomics -mbulk-memory -DNDEBUG -DG_DISABLE_ASSERT -D_GNU_SOURCE -sLZ4=1 -sASYNCIFY=1 -pthread -sPROXY_TO_PTHREAD=1 -sFORCE_FILESYSTEM -sALLOW_TABLE_GROWTH -sTOTAL_MEMORY=2300MB -sWASM_BIGINT -sMALLOC=emmalloc --js-library=/build/node_modules/xterm-pty/emscripten-pty.js -sEXPORT_ES6=1 -sASYNCIFY_IMPORTS=ffi_call_js"
docker exec "$CONTAINER" emconfigure /qemu/configure --static --target-list=x86_64-softmmu \
  --cpu=wasm32 --cross-prefix= --without-default-features --enable-system \
  --with-coroutine=fiber --enable-virtfs \
  --extra-cflags="$EXTRA_CFLAGS" --extra-cxxflags="$EXTRA_CFLAGS" \
  --extra-ldflags="-sEXPORTED_RUNTIME_METHODS=getTempRet0,setTempRet0,addFunction,removeFunction,TTY,FS"
docker exec "$CONTAINER" emmake make -j "$(docker exec "$CONTAINER" nproc)" qemu-system-x86_64

# 4. Build /pack: the bootable disk image + the SeaBIOS / VGA roms QEMU needs.
TMPDIR="$(mktemp -d)"
mkdir -p "$TMPDIR/pack"
cp "$IMG" "$TMPDIR/pack/disk-bios.img"
cp "$QEMU_WASM_REPO"/pc-bios/{bios-256k.bin,vgabios-stdvga.bin,kvmvapic.bin,linuxboot_dma.bin,efi-virtio.rom} "$TMPDIR/pack/"
docker cp "$TMPDIR/pack" "$CONTAINER":/pack
docker exec "$CONTAINER" /bin/sh -c \
  "/emsdk/upstream/emscripten/tools/file_packager.py pack.data --preload /pack > load.js"

# 5. Pull artifacts into web/qemu/.
mkdir -p "$OUT"
docker cp "$CONTAINER":/build/qemu-system-x86_64 "$OUT/out.js"
for f in qemu-system-x86_64.wasm qemu-system-x86_64.worker.js pack.data load.js ; do
  docker cp "$CONTAINER":/build/"$f" "$OUT/"
done

docker rm -f "$CONTAINER" >/dev/null 2>&1 || true
rm -rf "$TMPDIR"

echo
echo "=== done. artifacts in web/qemu/ ==="
ls -lh "$OUT"
echo
echo "Serve and open (COOP/COEP are injected by coi-serviceworker.js):"
echo "  (cd web/qemu && python3 -m http.server 8000)   # then open http://localhost:8000"
