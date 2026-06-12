#!/bin/sh
# End-to-end driver-loading demo: build the PE driver, build the kernel with
# it embedded, boot under QEMU, and assert the boot self tests pass (which
# include loading the driver, running its DriverEntry, and exercising its
# IRP dispatch). Exit 0 on PASS.
set -eu
cd "$(dirname "$0")/.."

# 1. Build the real PE driver + its import library.
./scripts/build-driver.sh

# 2. Build the kernel (build.rs embeds driver/testdriver.sys) and boot.
exec ./scripts/qemu-test.sh
