#!/bin/sh
# Build the guest kernel, as `libkrunfw`, into `build/krunfw/`.
#
# **Built here rather than installed**, on every host: libkrunfw ships no DLL for
# Windows, and a release or a bottle is whatever the packager built rather than the tag
# pinned below.
#
# **Not below v5.6.2.** Before it, the x86_64 config had `CONFIG_MTRR` off, and
# `CONFIG_X86_PAT` is `def_bool y` with `depends on MTRR`, so PAT went with it. A guest
# without PAT has no way to spell write-combining -- every `ioremap_wc` a driver asks for
# is handed back UC-. Nothing reports an error; it is only slower. On the GPU path that
# is the difference between writing a model's weights into a resizable BAR at 180 MB/s
# and at 5, measured here on one 900 MiB model: 5.0s against 178. It went out in
# libkrunfw's `d28d263` and came back in `529fbc0`, first released in v5.6.2.
#
# aarch64 has neither register set: memory types there come from the page tables' MAIR,
# and write-combining works without any of this.
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
# * **macOS** splits it: the output is a `.dylib` that has to be linked on macOS, while
#   the kernel inside it has to be built on Linux. The same container builds the kernel
#   as far as `kernel.c`, the C bundle libkrunfw wraps it in, and the host links that.
#   This is what libkrunfw's own `krunvm` path does, with the container in place of the
#   microVM; its native macOS build needs a case-sensitive volume and GNU tools besides.
#
# In the container the kernel tree lives in a named volume, not in the bind-mounted
# `build/`. Docker Desktop shares host directories over VirtioFS, and untarring a kernel
# there fails intermittently -- tar sees directories change inode under it ("Directory
# renamed before its status could be extracted") or fails to replace a symlink
# ("Operation not supported"). Only the results cross the bind mount: the library into
# `build/krunfw/`, and on macOS `kernel.c` back into the clone, for the host to link.
# The volume persists, so a rebuild is incremental; `docker volume rm` it to start over.
set -eu

# Pinned for the same reason the submodules are: the kernel config and the ABI version
# have to match what libkrun expects (KRUNFW_NAME, ABI 5).
LIBKRUNFW_TAG="${LIBKRUNFW_TAG:-v5.6.2}"
IMAGE="${LIBKRUNFW_IMAGE:-ubuntu:24.04}"
VOLUME="${LIBKRUNFW_VOLUME:-cortex-krun-libkrunfw}"

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
# one it does not. The Linux name carries the ABI version (`libkrunfw.so.5`, renamed so
# below), so it is matched rather than named.
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

# A tree from another tag is cloned again rather than moved: the Makefile unpacks the
# kernel and copies the config in once, so an old tree would keep the old kernel.
if [ "$(git -C "$BUILD/libkrunfw" describe --tags 2>/dev/null)" != "$LIBKRUNFW_TAG" ]; then
    rm -rf "$BUILD/libkrunfw"
    git clone --depth 1 --branch "$LIBKRUNFW_TAG" \
        https://github.com/containers/libkrunfw.git "$BUILD/libkrunfw"
fi

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

# The clone is copied into the volume once per tag, for the same reason the host one is
# cloned again: an old tree would keep the old kernel. Anything a build left in the host
# clone -- a kernel tree from before the volume, a `kernel.c` -- stays behind.
SRC=/work/libkrunfw
if [ "$(git -C "$SRC" describe --tags 2>/dev/null)" != "$LIBKRUNFW_TAG" ]; then
    rm -rf "$SRC"
    mkdir -p "$SRC"
    tar -C /build/libkrunfw --exclude='./linux-*' --exclude='./kernel.c' \
        --exclude='./libkrunfw*' -cf - . | tar -C "$SRC" -xf -
fi

cd "$SRC"
# bin2cbundle.py needs pyelftools; the kernel build needs the rest.
make ${MAKE_ARGS:-} -j"$(nproc)"
cp -v libkrunfw* /out/ 2>/dev/null || true
if [ -n "${COPY_BACK:-}" ]; then
    cp -v $COPY_BACK /build/libkrunfw/
fi
EOF

    # MSYS/Git Bash rewrites arguments that look like absolute paths; the leading `//`
    # is what stops it turning the container-side ones into Windows paths.
    MSYS_NO_PATHCONV=1 docker run --rm \
        -e EXTRA_PACKAGES="${EXTRA_PACKAGES:-}" \
        -e MAKE_ARGS="${MAKE_ARGS:-}" \
        -e COPY_BACK="${COPY_BACK:-}" \
        -e LIBKRUNFW_TAG="$LIBKRUNFW_TAG" \
        -v "$BUILD:/build" \
        -v "$PREFIX:/out" \
        -v "$VOLUME:/work" \
        "$IMAGE" sh /build/build.sh
}

case "$HOST" in
    windows)
        EXTRA_PACKAGES="mingw-w64" MAKE_ARGS="OS=Windows" build_in_container
        ;;
    linux)
        build_in_container
        # Kept under its soname, not the name make gives it. libkrun opens
        # `libkrunfw.so.5` by that name -- found beside the binary through its
        # `$ORIGIN` runpath -- and no `make install` runs here to add the link from
        # it to `libkrunfw.so.5.6.2`. Renamed rather than linked, so that
        # everything downstream copies one file under the one name that works.
        for lib in "$PREFIX"/libkrunfw.so.*.*.*; do
            [ -e "$lib" ] || continue
            mv -f "$lib" "$PREFIX/libkrunfw.so.$(basename "$lib" | cut -d. -f3)"
        done
        ;;
    macos)
        MAKE_ARGS="kernel.c" COPY_BACK="kernel.c" build_in_container
        (cd "$BUILD/libkrunfw" && make)
        cp -v "$BUILD/libkrunfw"/libkrunfw.*.dylib "$PREFIX/"
        ;;
esac

echo "built libkrunfw ($LIBKRUNFW_TAG) into $PREFIX"
