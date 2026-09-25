#!/bin/sh
# Build the guest kernel, as `libkrunfw`, into `build/krunfw/`.
#
# **Built here rather than installed**, on every host, for one reason that is not
# reproducibility: the config libkrunfw ships for x86_64 has `CONFIG_MTRR` off, and
# `CONFIG_X86_PAT` is `def_bool y` with `depends on MTRR`, so PAT goes with it. A guest
# without PAT has no way to spell write-combining -- its PAT slots come up
# `WB WT UC- UC` with no WC among them -- and every `ioremap_wc` a driver asks for is
# handed back UC-. Nothing reports an error; it is only slower. On the GPU path that is
# the difference between writing a model's weights into a resizable BAR at 180 MB/s and
# at 5, measured here on one 900 MiB model: 5.0s against 178.
#
# It went out in libkrunfw's `d28d263` ("MTRRs are not relevant" in a VM, saving 75 KiB
# of kernel image), which is true of MTRR itself and takes PAT along by accident -- the
# commit says nothing about either PAT or write-combining, and libkrunfw's own SEV and
# TDX configs still set `CONFIG_MTRR=y`. So this is a regression to carry rather than a
# trade to respect, and carrying it means building the kernel rather than taking a
# release or a bottle.
#
# aarch64 has neither register set: memory types there come from the page tables' MAIR,
# and write-combining works without any of this. The patch below is x86_64's alone, and
# an arm64 build here is only about controlling what is in the guest.
#
#     xtask/scripts/build-libkrunfw.sh            # build if build/krunfw is empty
#     LIBKRUNFW_REBUILD=1 xtask/scripts/...       # build again over what is there
#
# # Where the kernel is built
#
# It is a Linux kernel, so it takes a Linux toolchain -- `make`, `flex`, `bison`,
# `libelf`, and for Windows a MinGW-w64 cross-compiler to wrap `vmlinux` in a DLL. What
# provides that depends on the host:
#
# * **Linux** builds it directly, in a throwaway `ubuntu:24.04` container, so that the
#   toolchain is the same one Windows gets rather than whatever the distribution has.
# * **Windows** has no such toolchain at all and builds in the same container. libkrunfw
#   ships no DLL, so there is nothing to install instead.
# * **macOS** cannot: the output is a `.dylib` that has to be linked on macOS, while the
#   kernel inside it has to be built on Linux. libkrunfw resolves that with `krunvm` --
#   it builds the kernel in a microVM and links the library on the host -- and this
#   defers to it. `brew install krunvm` if it is missing.
set -eu

# Pinned for the same reason the submodules are: the kernel config and the ABI version
# have to match what libkrun expects (KRUNFW_NAME, ABI 5).
LIBKRUNFW_TAG="${LIBKRUNFW_TAG:-v5.6.1}"
IMAGE="${LIBKRUNFW_IMAGE:-ubuntu:24.04}"

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
BUILD="$ROOT/build/libkrunfw-build"
PREFIX="$ROOT/build/krunfw"

case "$(uname -s)" in
    Linux) HOST=linux ;;
    Darwin) HOST=macos ;;
    MINGW* | MSYS* | CYGWIN*) HOST=windows ;;
    *) echo "unsupported host: $(uname -s)" >&2 && exit 1 ;;
esac

# What the library is called, which is also how this tells a build it already has from
# one it does not. The Linux name carries the full version, so it is matched rather
# than named.
case "$HOST" in
    linux) PATTERN="libkrunfw.so.*" ;;
    macos) PATTERN="libkrunfw.*.dylib" ;;
    windows) PATTERN="libkrunfw.dll" ;;
esac

# shellcheck disable=SC2086
if [ -z "${LIBKRUNFW_REBUILD:-}" ] && [ -n "$(find "$PREFIX" -maxdepth 1 -name "$PATTERN" 2>/dev/null)" ]; then
    echo "libkrunfw already built in $PREFIX (LIBKRUNFW_REBUILD=1 to build it again)"
    exit 0
fi

mkdir -p "$BUILD" "$PREFIX"

[ -d "$BUILD/libkrunfw" ] || git clone --depth 1 --branch "$LIBKRUNFW_TAG" \
    https://github.com/containers/libkrunfw.git "$BUILD/libkrunfw"

# The config the Makefile will copy in, named the way it names it:
# `config-libkrunfw$(VARIANT)_$(GUESTARCH)`.
case "$HOST" in
    windows) CONFIG="config-libkrunfw-windows_x86_64" ;;
    *) CONFIG="config-libkrunfw_$(uname -m | sed 's/^arm64$/aarch64/')" ;;
esac

(
    cd "$BUILD/libkrunfw"

    # See the top of this file. Idempotent, and a no-op on the aarch64 config, which has
    # no MTRR line to find because the option is x86's.
    sed -i.bak 's/^# CONFIG_MTRR is not set$/CONFIG_MTRR=y/' "$CONFIG" && rm -f "$CONFIG.bak"

    # The Makefile copies the config in only when it unpacks the kernel, so a tree that
    # is already there is still configured the way it was the first time.
    for tree in linux-*/; do
        [ -d "$tree" ] || continue
        cp "$CONFIG" "$tree/.config"
        (cd "$tree" && make olddefconfig)
    done
)

build_in_container() {
    command -v docker >/dev/null 2>&1 || { echo "missing docker" >&2; exit 1; }
    if [ "$(docker info --format '{{.OSType}}')" != "linux" ]; then
        echo "docker is in Windows container mode; switch it to Linux containers" >&2
        exit 1
    fi

    # Written out rather than passed to `docker run sh -c`, so that the quoting survives
    # a Windows command line intact.
    cat > "$BUILD/build.sh" <<'EOF'
set -eux
export DEBIAN_FRONTEND=noninteractive
apt-get update
apt-get install -y --no-install-recommends \
    build-essential flex bison bc libelf-dev libssl-dev \
    python3 python3-pyelftools git curl ca-certificates xz-utils \
    patch rsync cpio zstd kmod ${EXTRA_PACKAGES:-}

cd /build/libkrunfw
# bin2cbundle.py needs pyelftools; the kernel build needs the rest.
make ${MAKE_ARGS:-} -j"$(nproc)"
cp -v libkrunfw* /out/ 2>/dev/null || true
EOF

    # MSYS/Git Bash rewrites arguments that look like absolute paths; the leading `//`
    # is what stops it turning the container-side ones into Windows paths.
    MSYS_NO_PATHCONV=1 docker run --rm \
        -e EXTRA_PACKAGES="${EXTRA_PACKAGES:-}" \
        -e MAKE_ARGS="${MAKE_ARGS:-}" \
        -v "$BUILD:/build" \
        -v "$PREFIX:/out" \
        "$IMAGE" sh /build/build.sh
}

case "$HOST" in
    windows)
        EXTRA_PACKAGES="mingw-w64" MAKE_ARGS="OS=Windows" build_in_container
        ;;
    linux)
        build_in_container
        ;;
    macos)
        command -v krunvm >/dev/null 2>&1 || {
            echo "missing krunvm, which is what libkrunfw builds the kernel in on macOS:" >&2
            echo "  brew install krunvm" >&2
            exit 1
        }
        (cd "$BUILD/libkrunfw" && make)
        cp -v "$BUILD/libkrunfw"/libkrunfw.*.dylib "$PREFIX/"
        ;;
esac

echo "built libkrunfw ($LIBKRUNFW_TAG) into $PREFIX"
