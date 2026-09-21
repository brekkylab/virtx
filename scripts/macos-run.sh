#!/bin/sh
# Cargo target runner for aarch64-apple-darwin.
#
# Two things are needed before a libkrun binary can boot a VM on macOS:
#
#  1. Hypervisor.framework requires the `com.apple.security.hypervisor`
#     entitlement. Ad-hoc signing is enough; no developer certificate needed.
#  2. libkrun dlopen()s `libkrunfw.5.dylib` by leaf name, and Homebrew's lib
#     directory is not on dyld's default search path.
set -eu

BINARY="$1"
shift

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
codesign --entitlements "$SCRIPT_DIR/hvf-entitlements.plist" --force -s - "$BINARY"

if command -v brew >/dev/null 2>&1; then
    DYLD_LIBRARY_PATH="$(brew --prefix)/lib${DYLD_LIBRARY_PATH:+:$DYLD_LIBRARY_PATH}"
    export DYLD_LIBRARY_PATH
fi

exec "$BINARY" "$@"
