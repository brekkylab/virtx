#!/bin/sh
# Build the guest's half of a sandbox's GPU, the Vulkan loader and Mesa's venus driver, into OUT.
#
#     sh build.sh OUT SOURCES
#
# Run as root, once in Alpine for musl and once in manylinux_2_28 for glibc: the two libraries
# are loaded into the guest's own programs, so each has to be built against the C library of
# the program that loads it. What it writes is what `/alib/musl` or `/alib/glibc` holds:
#
#     OUT/libvulkan.so.1        the loader, which a Vulkan program links or opens
#     OUT/libvulkan_virtio.so   venus, which carries Vulkan to the host's GPU over virtio-gpu
#     OUT/virtio_icd.json       the loader's manifest for venus, naming it by file name
#
# Each needs nothing of the guest but its C library: libdrm is built here as a static archive
# and linked in, libgcc and libstdc++ are linked in too, and none of their symbols is exported.
# Venus is built with no window system and no shader cache, which would each want more.
# Mesa is MIT-licensed, libdrm MIT, and the loader Apache-2.0. The tarballs are in SOURCES, as
# `sources` beside this script lists them, checked already.
set -eu

HERE=$(cd "$(dirname "$0")" && pwd)
mkdir -p "${1:?usage: build.sh OUT SOURCES}"
OUT=$(cd "$1" && pwd)
SOURCES=${2:?usage: build.sh OUT SOURCES}

WORK=/tmp/vulkan
PREFIX=$WORK/prefix
mkdir -p "$WORK/src" "$PREFIX"

if [ -f /etc/alpine-release ]; then
    apk add --no-cache \
        build-base linux-headers cmake ninja pkgconf xz \
        meson py3-mako py3-packaging py3-yaml >/dev/null
else
    # manylinux: a gcc-toolset is already first on PATH, and cmake with it; a Python new enough
    # for Mesa's generators is under /opt/python, and meson comes from PyPI.
    python=/opt/python/cp312-cp312/bin/python3
    "$python" -m venv "$WORK/venv"
    "$WORK/venv/bin/pip" install --quiet --retries 10 meson ninja mako packaging pyyaml
    PATH=$WORK/venv/bin:$PATH
fi

grep -v '^#' "$HERE/sources" | while read -r file _; do
    [ -n "$file" ] && tar -xf "$SOURCES/$file" -C "$WORK/src"
done

export PKG_CONFIG_PATH=$PREFIX/lib/pkgconfig
JOBS=$(nproc)
# What every shared library here links with: the compiler's own runtime in, and nothing a
# static archive brought exported.
LINK="-static-libgcc -static-libstdc++ -Wl,--exclude-libs,ALL"

cd "$WORK/src/libdrm-2.4.134"
meson setup build --prefix="$PREFIX" --libdir=lib --buildtype=release \
    -Ddefault_library=static -Db_staticpic=true \
    -Dauto_features=disabled -Dtests=false -Dudev=false >/dev/null
ninja -C build -j "$JOBS" install >/dev/null

cd "$WORK/src/Vulkan-Headers-vulkan-sdk-1.4.363.0"
cmake -S . -B build -G Ninja -DCMAKE_INSTALL_PREFIX="$PREFIX" >/dev/null
cmake --install build >/dev/null

cd "$WORK/src/Vulkan-Loader-vulkan-sdk-1.4.363.0"
cmake -S . -B build -G Ninja -DCMAKE_BUILD_TYPE=Release \
    -DCMAKE_INSTALL_PREFIX="$PREFIX" -DCMAKE_INSTALL_LIBDIR=lib \
    -DCMAKE_PREFIX_PATH="$PREFIX" \
    -DCMAKE_SHARED_LINKER_FLAGS="$LINK" \
    -DBUILD_WSI_XCB_SUPPORT=OFF -DBUILD_WSI_XLIB_SUPPORT=OFF \
    -DBUILD_WSI_WAYLAND_SUPPORT=OFF -DBUILD_WSI_DIRECTFB_SUPPORT=OFF \
    -DBUILD_TESTS=OFF >/dev/null
cmake --build build -j "$JOBS" >/dev/null
cmake --install build >/dev/null

cd "$WORK/src/mesa-26.2.6"
meson setup build --prefix="$PREFIX" --libdir=lib --buildtype=release -Db_ndebug=true \
    -Dauto_features=disabled \
    -Dvulkan-drivers=virtio -Dgallium-drivers= -Dplatforms= -Dvulkan-layers= \
    -Dopengl=false -Dgles1=disabled -Dgles2=disabled -Degl=disabled -Dglx=disabled \
    -Dgbm=disabled -Dllvm=disabled -Dzlib=disabled -Dshader-cache=disabled \
    -Dvideo-codecs= -Dtools= -Dbuild-tests=false \
    -Dvulkan-icd-dir="$PREFIX/icd" \
    -Dc_link_args="$LINK" -Dcpp_link_args="$LINK" >/dev/null
ninja -C build -j "$JOBS" install >/dev/null

# The real files under the names the guest uses: a link would not survive every host the
# result is unpacked on.
cp -L "$PREFIX/lib/libvulkan.so.1" "$OUT/libvulkan.so.1"
cp -L "$PREFIX/lib/libvulkan_virtio.so" "$OUT/libvulkan_virtio.so"
strip --strip-unneeded "$OUT/libvulkan.so.1" "$OUT/libvulkan_virtio.so"
# A bare file name, which the loader opens as the system does: from `LD_LIBRARY_PATH`, where
# this directory is, like the loader itself.
sed 's|"library_path": *"[^"]*"|"library_path": "libvulkan_virtio.so"|' \
    "$PREFIX"/icd/virtio_icd*.json >"$OUT/virtio_icd.json"

# Nothing past the C library: anything else is a library an image may not have.
for lib in "$OUT"/*.so*; do
    needed=$(readelf -d "$lib" | sed -n 's/.*(NEEDED).*\[\(.*\)\]/\1/p' |
        grep -vE '^(libc\.so|libc\.musl-|libm\.so|libdl\.so|libpthread\.so|librt\.so|ld-linux)' || true)
    if [ -n "$needed" ]; then
        echo "$lib needs $needed, which an image may not have" >&2
        exit 1
    fi
done
du -sh "$OUT"/*
