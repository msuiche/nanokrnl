#!/bin/sh
# Build the worker console program (thin wrapper over build-app.sh).
exec "$(dirname "$0")/build-app.sh" worker
