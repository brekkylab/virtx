#!/bin/sh
# Build what every machine gets at `/alib`, the guest's half of a sandbox's GPU -- the Vulkan
# loader and Mesa's venus driver -- into PREFIX, which `cargo xtask build alib` names as
# `alib/` in virtx's cache, where a server about to boot, `install` and `archive` take it from.
#
#     xtask/scripts/build-alib.sh PREFIX
#
# The two libraries are loaded into the guest's own programs, so each has to be built against
# the C library of the program that loads it: once in Alpine for musl and once in
# manylinux_2_28 for glibc, glibc at 2.28 so an image as old as that loads it. Each is built in
# a container of that image for this host's architecture, docker's, as the guest kernel's is in
# build-libkrunfw.sh, under `build/alib/<libc>/` of this repository. What lands in PREFIX/musl
# or PREFIX/glibc is what `/alib/musl` or `/alib/glibc` holds, by the names the agent's
# `alib_env` (virtx-uvm-guest/src/main.rs) gives the guest:
#
#     libvulkan.so.1        the loader, which a Vulkan program links or opens
#     libvulkan_virtio.so   venus, which carries Vulkan to the host's GPU over virtio-gpu
#     virtio_icd.json       the loader's manifest for venus, naming it by file name
#
# Each needs nothing of the guest but its C library: libdrm is built here as a static archive
# and linked in, libgcc and libstdc++ are linked in too, and none of their symbols is exported.
# Venus is built with no window system and no shader cache, which would each want more.
# Mesa is MIT-licensed, libdrm MIT, and the loader Apache-2.0. Their tarballs are fetched into
# the build directory and checked against the sha256 pinned below.
set -eu

PREFIX="${1:?usage: build-alib.sh PREFIX}"
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
BUILD="$ROOT/build/alib"

case "$(uname -m)" in
    arm64 | aarch64) ARCH=aarch64 ;;
    x86_64) ARCH=x86_64 ;;
    *) echo "unsupported host arch: $(uname -m)" >&2; exit 1 ;;
esac

command -v docker >/dev/null 2>&1 || { echo "missing docker" >&2; exit 1; }
if [ "$(docker info --format '{{.OSType}}')" != "linux" ]; then
    echo "docker is in Windows container mode; switch it to Linux containers" >&2
    exit 1
fi

mkdir -p "$BUILD" "$PREFIX"

# Written out rather than passed to `docker run sh -c`, so that the quoting survives a Windows
# command line intact.
cat > "$BUILD/build.sh" <<'EOF'
set -eu

WORK=/build/$LIBC
# What the libraries are built into, and what of it goes to /alib.
STAGE=$WORK/stage
OUT=$WORK/out
rm -rf "$WORK/src" "$STAGE" "$OUT"
mkdir -p "$WORK/sources" "$WORK/src" "$STAGE" "$OUT"

if [ "$LIBC" = musl ]; then
    apk add --no-cache \
        build-base linux-headers cmake ninja pkgconf xz curl \
        meson py3-mako py3-packaging py3-yaml >/dev/null
else
    # manylinux: a gcc-toolset is already first on PATH, and cmake with it; a Python new enough
    # for Mesa's generators is under /opt/python, and meson comes from PyPI.
    python=/opt/python/cp312-cp312/bin/python3
    "$python" -m venv "$WORK/venv"
    "$WORK/venv/bin/pip" install --quiet --retries 10 meson ninja mako packaging pyyaml
    PATH=$WORK/venv/bin:$PATH
fi

# Each tarball, where it is fetched from, and its sha256. Mesa's venus is the guest's half of
# the host's virglrenderer: any release speaks to it, and the newest stable one knows the most
# of what the host's driver can do.
while read -r file url sum; do
    if [ ! -f "$WORK/sources/$file" ]; then
        echo "fetching $url"
        curl -fsSL --retry 3 -o "$WORK/sources/$file" "$url"
    fi
    if ! echo "$sum  $WORK/sources/$file" | sha256sum -c >/dev/null; then
        rm -f "$WORK/sources/$file"
        echo "$file is not the $sum pinned for it" >&2
        exit 1
    fi
    tar -xf "$WORK/sources/$file" -C "$WORK/src"
done <<'SOURCES'
mesa-26.2.6.tar.xz https://archive.mesa3d.org/mesa-26.2.6.tar.xz b9436b3e1a8be76fbd4823dc884f90596ab7c6ac5e1867a305e66ffccfb31a92
libdrm-2.4.134.tar.xz https://dri.freedesktop.org/libdrm/libdrm-2.4.134.tar.xz ac5e74d157830eb8bee44c6a6bf3ad49774ef0dd2a72bdad74a8f20308b52a95
Vulkan-Headers-vulkan-sdk-1.4.363.0.tar.gz https://github.com/KhronosGroup/Vulkan-Headers/archive/refs/tags/vulkan-sdk-1.4.363.0.tar.gz 4a078be12bef21cfebc09d878b77a63cff9d68f899254a0b00d0e37ef73e7f7e
Vulkan-Loader-vulkan-sdk-1.4.363.0.tar.gz https://github.com/KhronosGroup/Vulkan-Loader/archive/refs/tags/vulkan-sdk-1.4.363.0.tar.gz 941eff558fc74f248745fb7df367640cf89a8457f216dae498a74ae14b6ba6f7
SOURCES

export PKG_CONFIG_PATH=$STAGE/lib/pkgconfig
JOBS=$(nproc)
# What every shared library here links with: the compiler's own runtime in, and nothing a
# static archive brought exported.
LINK="-static-libgcc -static-libstdc++ -Wl,--exclude-libs,ALL"

cd "$WORK/src/libdrm-2.4.134"
meson setup build --prefix="$STAGE" --libdir=lib --buildtype=release \
    -Ddefault_library=static -Db_staticpic=true \
    -Dauto_features=disabled -Dtests=false -Dudev=false >/dev/null
ninja -C build -j "$JOBS" install >/dev/null

cd "$WORK/src/Vulkan-Headers-vulkan-sdk-1.4.363.0"
cmake -S . -B build -G Ninja -DCMAKE_INSTALL_PREFIX="$STAGE" >/dev/null
cmake --install build >/dev/null

cd "$WORK/src/Vulkan-Loader-vulkan-sdk-1.4.363.0"
cmake -S . -B build -G Ninja -DCMAKE_BUILD_TYPE=Release \
    -DCMAKE_INSTALL_PREFIX="$STAGE" -DCMAKE_INSTALL_LIBDIR=lib \
    -DCMAKE_PREFIX_PATH="$STAGE" \
    -DCMAKE_SHARED_LINKER_FLAGS="$LINK" \
    -DBUILD_WSI_XCB_SUPPORT=OFF -DBUILD_WSI_XLIB_SUPPORT=OFF \
    -DBUILD_WSI_WAYLAND_SUPPORT=OFF -DBUILD_WSI_DIRECTFB_SUPPORT=OFF \
    -DBUILD_TESTS=OFF >/dev/null
cmake --build build -j "$JOBS" >/dev/null
cmake --install build >/dev/null

cd "$WORK/src/mesa-26.2.6"
meson setup build --prefix="$STAGE" --libdir=lib --buildtype=release -Db_ndebug=true \
    -Dauto_features=disabled \
    -Dvulkan-drivers=virtio -Dgallium-drivers= -Dplatforms= -Dvulkan-layers= \
    -Dopengl=false -Dgles1=disabled -Dgles2=disabled -Degl=disabled -Dglx=disabled \
    -Dgbm=disabled -Dllvm=disabled -Dzlib=disabled -Dshader-cache=disabled \
    -Dvideo-codecs= -Dtools= -Dbuild-tests=false \
    -Dvulkan-icd-dir="$STAGE/icd" \
    -Dc_link_args="$LINK" -Dcpp_link_args="$LINK" >/dev/null
ninja -C build -j "$JOBS" install >/dev/null

# The real files under the names the guest uses: a link would not survive every host the
# result is unpacked on.
cp -L "$STAGE/lib/libvulkan.so.1" "$OUT/libvulkan.so.1"
cp -L "$STAGE/lib/libvulkan_virtio.so" "$OUT/libvulkan_virtio.so"
strip --strip-unneeded "$OUT/libvulkan.so.1" "$OUT/libvulkan_virtio.so"
# A bare file name, which the loader opens as the system does: from `LD_LIBRARY_PATH`, where
# this directory is, like the loader itself.
sed 's|"library_path": *"[^"]*"|"library_path": "libvulkan_virtio.so"|' \
    "$STAGE"/icd/virtio_icd*.json >"$OUT/virtio_icd.json"

# Nothing past the C library: anything else is a library an image may not have.
for lib in "$OUT"/*.so*; do
    needed=$(readelf -d "$lib" | sed -n 's/.*(NEEDED).*\[\(.*\)\]/\1/p' |
        grep -vE '^(libc\.so|libc\.musl-|libm\.so|libdl\.so|libpthread\.so|librt\.so|ld-linux)' || true)
    if [ -n "$needed" ]; then
        echo "$lib needs $needed, which an image may not have" >&2
        exit 1
    fi
done

rm -rf "/alib/$LIBC"
mkdir -p "/alib/$LIBC"
cp "$OUT"/* "/alib/$LIBC/"
du -sh "/alib/$LIBC"/*
EOF

# Docker Hub's images through mirror.gcr.io, which has no rate limit for an anonymous pull.
for libc in musl glibc; do
    case "$libc" in
        musl) image=mirror.gcr.io/library/alpine:3.22 ;;
        glibc) image=quay.io/pypa/manylinux_2_28_$ARCH:2026.11.07-1 ;;
    esac
    echo "alib: $libc in $image"
    # MSYS/Git Bash rewrites arguments that look like absolute paths; this stops it turning
    # the container-side ones into Windows paths.
    MSYS_NO_PATHCONV=1 docker run --rm \
        -e LIBC="$libc" \
        -v "$BUILD:/build" \
        -v "$PREFIX:/alib" \
        "$image" sh /build/build.sh
done
echo "alib built into $PREFIX"
