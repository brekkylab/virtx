#!/bin/sh
# Unpack a musl sysroot -- headers and the static libc -- for cross-compiling the C that
# `cortex-abins/` pulls in. Only one crate needs it: `rusqlite`'s `bundled` feature, which
# compiles SQLite's amalgamation, and which is `bundled` on purpose -- the schemas are FTS5
# indexes and FTS5 is a compile-time option no host's SQLite has to have been built with.
#
# clang is a cross-compiler already; what it does not have is a libc for the other side, and
# `rustup target add` ships only Rust's own `libc.a` -- no `stdio.h`. These are Alpine's
# `musl-dev` and `linux-headers` packages, which is the same libc the guest runs, unpacked
# from the same branch `fetch-alpine-rootfs.sh` pins.
set -eu

VERSION="${ALPINE_VERSION:-3.24.2}"
BRANCH="v$(echo "$VERSION" | cut -d. -f1,2)"
case "$(uname -m)" in
    arm64 | aarch64) ARCH=aarch64 ;;
    x86_64) ARCH=x86_64 ;;
    *) echo "unsupported host arch: $(uname -m)" >&2; exit 1 ;;
esac

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
DEST="$ROOT/data/musl-sysroot/$ARCH"
REPO="https://dl-cdn.alpinelinux.org/alpine/$BRANCH/main/$ARCH"

if [ -e "$DEST/usr/include/stdio.h" ]; then
    echo "sysroot already present at $DEST"
    exit 0
fi

mkdir -p "$DEST"
# The exact filenames are read from the index rather than pinned here: the CDN carries one
# build of each package per branch and deletes the last one when a rebuild lands, so a pinned
# `musl-dev-1.2.6-r2.apk` is a URL that 404s the day Alpine bumps the release number. The
# branch is what is pinned, and within a branch these are ABI-compatible by Alpine's own rule.
for pkg in musl-dev linux-headers; do
    file=$(curl -fsSL "$REPO/" | grep -oE "$pkg-[0-9][^\"]*\.apk" | sort -u | tail -1)
    [ -n "$file" ] || { echo "no $pkg in $REPO" >&2; exit 1; }
    echo "fetching $REPO/$file"
    # An apk is a gzipped tar with a signature segment glued to its front; tar reads past it
    # and warns, which is why the status is not checked here.
    curl -fsSL "$REPO/$file" | tar -xzf - -C "$DEST" 2>/dev/null || true
done

[ -e "$DEST/usr/include/stdio.h" ] || { echo "unpacked, but no headers in $DEST" >&2; exit 1; }
echo "unpacked to $DEST"
