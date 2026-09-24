#!/bin/sh
# Cargo target runner for aarch64-apple-darwin.
#
# Two things are needed before a libkrun binary can boot a VM on macOS:
#
#  1. Hypervisor.framework requires the `com.apple.security.hypervisor`
#     entitlement. Ad-hoc signing is enough; no developer certificate needed.
#  2. libkrun dlopen()s `libkrunfw.5.dylib` by leaf name, and Homebrew's lib
#     directory is not on dyld's default search path. It goes on the *fallback*
#     path, which dyld consults only for a leaf name it could not otherwise
#     find. DYLD_LIBRARY_PATH is consulted first for every library, absolute
#     install names included, so it would let a Homebrew libvirglrenderer --
#     krunkit's tap installs one -- stand in for the one data/ built.
set -eu

BINARY="$1"
shift

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
codesign --entitlements "$SCRIPT_DIR/hvf-entitlements.plist" --force -s - "$BINARY"

# Not the boot half, which is signed as it is linked -- see scripts/macos-link.sh.
# `cortex-krun` never creates a VM itself: it spawns `cortex-krun-host`, which does, and that
# one has to carry the entitlement however it was started. `cargo run` is not on the path a
# client takes to a session.

# Setting the fallback path replaces dyld's default one, so that is restated.
if command -v brew >/dev/null 2>&1; then
    DYLD_FALLBACK_LIBRARY_PATH="$(brew --prefix)/lib:${DYLD_FALLBACK_LIBRARY_PATH:-$HOME/lib:/usr/local/lib:/usr/lib}"
    export DYLD_FALLBACK_LIBRARY_PATH
fi

exec "$BINARY" "$@"
