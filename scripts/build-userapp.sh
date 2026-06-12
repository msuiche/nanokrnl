#!/bin/sh
# Build the userapp console program (thin wrapper over build-app.sh).
exec "$(dirname "$0")/build-app.sh" userapp
