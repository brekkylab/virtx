#!/bin/sh
# The C compiler cargo hands to `cc-rs` when building `cortex-abins/` for the guest. Named by
# `cortex-abins/.cargo/config.toml`, the same way `macos-link.sh` is named as a linker.
#
# It is Apple's own clang: a clang is a cross-compiler for every target LLVM was built with,
# and the two things it is missing for this one are a target triple and somewhere to find a
# libc -- which is all this adds. No musl-gcc, no second toolchain; `lld` is already a
# dependency of this repository and `llvm-ar` comes with it (see `musl-ar.sh`).
set -eu

case "$(uname -m)" in
    arm64 | aarch64) ARCH=aarch64 ;;
    x86_64) ARCH=x86_64 ;;
    *) echo "unsupported host arch: $(uname -m)" >&2; exit 1 ;;
esac

SYSROOT="$(cd "$(dirname "$0")/.." && pwd)/data/musl-sysroot/$ARCH"
if [ ! -e "$SYSROOT/usr/include/stdio.h" ]; then
    echo "no musl sysroot at $SYSROOT -- run scripts/fetch-musl-sysroot.sh" >&2
    exit 1
fi

exec /usr/bin/clang --target="$ARCH-unknown-linux-musl" --sysroot="$SYSROOT" "$@"
