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

# And the boot half beside it, when there is one. `cortex-krun` never creates a VM itself --
# it spawns `cortex-krun-host`, which does -- and an entitlement is read at `exec`, so signing
# the binary cargo is about to run is not enough: the process that calls hv_vm_create is the
# other one. Signed here rather than by a build script because cargo gives no hook that runs
# after a *sibling* member is linked, and this is the one place that already knows a binary is
# about to be run from that directory.
BOOT="$(dirname "$BINARY")/cortex-krun-host"
if [ "$BOOT" != "$BINARY" ] && [ -f "$BOOT" ]; then
    codesign --entitlements "$SCRIPT_DIR/hvf-entitlements.plist" --force -s - "$BOOT"
fi

if command -v brew >/dev/null 2>&1; then
    DYLD_LIBRARY_PATH="$(brew --prefix)/lib${DYLD_LIBRARY_PATH:+:$DYLD_LIBRARY_PATH}"
    export DYLD_LIBRARY_PATH
fi

exec "$BINARY" "$@"
