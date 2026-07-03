#!/bin/sh
# Build the crash console program (thin wrapper over build-app.sh).
exec "$(dirname "$0")/build-app.sh" crash
