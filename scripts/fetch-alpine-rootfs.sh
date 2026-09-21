#!/bin/sh
# Download and unpack an Alpine minirootfs to use as the guest root filesystem.
set -eu

VERSION="${ALPINE_VERSION:-3.24.2}"
BRANCH="v$(echo "$VERSION" | cut -d. -f1,2)"
case "$(uname -m)" in
    arm64 | aarch64) ARCH=aarch64 ;;
    x86_64) ARCH=x86_64 ;;
    *) echo "unsupported host arch: $(uname -m)" >&2; exit 1 ;;
esac

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
DEST="$ROOT/data/rootfs/alpine"
TARBALL="alpine-minirootfs-$VERSION-$ARCH.tar.gz"
URL="https://dl-cdn.alpinelinux.org/alpine/$BRANCH/releases/$ARCH/$TARBALL"

if [ -e "$DEST/bin/busybox" ]; then
    echo "rootfs already present at $DEST"
    exit 0
fi

mkdir -p "$DEST"
echo "fetching $URL"
curl -fsSL "$URL" | tar -xzf - -C "$DEST"

# The minirootfs ships no resolv.conf. With TSI there is no network interface
# in the guest and nothing to configure one, so point at a public resolver;
# TSI proxies the UDP queries out through the host.
printf 'nameserver 1.1.1.1\nnameserver 8.8.8.8\n' > "$DEST/etc/resolv.conf"

echo "unpacked to $DEST"
