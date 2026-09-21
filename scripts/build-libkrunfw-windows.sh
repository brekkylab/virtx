#!/bin/sh
# Build libkrunfw.dll -- the guest kernel libkrun loads on a Windows host.
#
# libkrunfw gained a Windows target in containers/libkrunfw#122, but its
# releases ship no DLL and `make OS=Windows` cross-compiles from a *Linux* host
# with MinGW-w64: it configures and builds a Linux kernel, then wraps vmlinux
# in a DLL with x86_64-w64-mingw32-gcc. None of that runs on Windows.
#
# So the build happens in a throwaway Linux container. Docker is the only
# prerequisite a Windows host needs, and the result -- a self-contained DLL --
# is copied back into data/krunfw/, where scripts/windows-run.ps1 picks it up.
#
# Run this from Git Bash (or any POSIX shell that can reach `docker`). Expect
# 10-20 minutes the first time; it is a full kernel build.
set -eu

# Pinned for the same reason the submodules are: the kernel config and the
# ABI version have to match what libkrun expects (KRUNFW_NAME, ABI 5).
LIBKRUNFW_TAG="${LIBKRUNFW_TAG:-v5.6.1}"
IMAGE="${LIBKRUNFW_IMAGE:-ubuntu:24.04}"

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BUILD="$ROOT/data/libkrunfw-build"
PREFIX="$ROOT/data/krunfw"

if [ -f "$PREFIX/libkrunfw.dll" ]; then
    echo "libkrunfw.dll already present at $PREFIX"
    exit 0
fi

command -v docker >/dev/null 2>&1 || { echo "missing docker" >&2; exit 1; }
if [ "$(docker info --format '{{.OSType}}')" != "linux" ]; then
    echo "docker is in Windows container mode; switch it to Linux containers" >&2
    exit 1
fi

mkdir -p "$BUILD" "$PREFIX"

# Written out rather than passed with `docker run sh -c`, so the quoting
# survives the Windows command line intact.
cat > "$BUILD/build.sh" <<'EOF'
set -eux
export DEBIAN_FRONTEND=noninteractive
apt-get update
apt-get install -y --no-install-recommends \
    build-essential mingw-w64 flex bison bc libelf-dev libssl-dev \
    python3 python3-pyelftools git curl ca-certificates xz-utils \
    patch rsync cpio zstd kmod

cd /build
[ -d libkrunfw ] || git clone --depth 1 --branch "$LIBKRUNFW_TAG" \
    https://github.com/containers/libkrunfw.git
cd libkrunfw

# bin2cbundle.py needs pyelftools; the kernel build needs the rest.
make OS=Windows -j"$(nproc)"

cp -v libkrunfw.dll /out/
EOF

# MSYS/Git Bash rewrites arguments that look like absolute paths; the leading
# `//` stops it from turning the container-side paths into Windows ones.
MSYS_NO_PATHCONV=1 docker run --rm \
    -e LIBKRUNFW_TAG="$LIBKRUNFW_TAG" \
    -v "$BUILD:/build" \
    -v "$PREFIX:/out" \
    "$IMAGE" sh /build/build.sh

echo "built $PREFIX/libkrunfw.dll ($LIBKRUNFW_TAG)"
