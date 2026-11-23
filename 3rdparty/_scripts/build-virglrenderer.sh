#!/bin/sh
# virglrenderer, which serves a guest's venus: built from 3rdparty/virglrenderer with its patches
# of 3rdparty/_patches applied, and installed to build/virglrenderer.
set -eu

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"

REBUILD=0
for arg in "$@"; do
    case "$arg" in
        --rebuild) REBUILD=1 ;;
        *) echo "usage: $0 [--rebuild]" >&2; exit 1 ;;
    esac
done

# On macOS virglrenderer links MoltenVK, which build-moltenvk.sh builds first and keeps here.
MoltenVK_prefix="$ROOT/build/MoltenVK"
MoltenVK_lib="$MoltenVK_prefix/static/MoltenVK.xcframework/macos-arm64_x86_64/libMoltenVK.a"
MoltenVK_want=""
if [ "$(uname -s)" = Darwin ]; then
    if [ ! -f "$MoltenVK_lib" ]; then
        echo "MoltenVK is not built; run: sh 3rdparty/_scripts/build-moltenvk.sh" >&2
        exit 1
    fi
    MoltenVK_want="$(cat "$MoltenVK_prefix/.virtx-stamp")"
fi

echo "Setting up virglrenderer"

virglrenderer_upstream="$ROOT/3rdparty/virglrenderer"
virglrenderer_patches="$ROOT/3rdparty/_patches/virglrenderer"
virglrenderer_src="$ROOT/build/virglrenderer-src"
virglrenderer_build="$ROOT/build/virglrenderer-build"
virglrenderer_prefix="$ROOT/build/virglrenderer"
virglrenderer_pcdir="$ROOT/build/pkgconfig"
virglrenderer_venv="$ROOT/build/meson-venv"
virglrenderer_stamp="$virglrenderer_prefix/.virtx-stamp"

# What the installed virglrenderer was built from: this script, the submodule's commit, the
# patches applied to it, where it is installed (its .pc names the prefix), and on macOS the
# MoltenVK it links. The same as the last build's, and it is not built again -- which is also
# what lets CI keep it in a cache.
virglrenderer_commit="$(git -C "$virglrenderer_upstream" rev-parse HEAD)"
virglrenderer_want="$(printf '%s\n' "$virglrenderer_prefix" "$(git hash-object "$0")" \
    "$virglrenderer_commit" "$(cat "$virglrenderer_patches"/*.patch | git hash-object --stdin)" \
    "$MoltenVK_want")"
if [ "$REBUILD" = 0 ] && [ -f "$virglrenderer_prefix/lib/pkgconfig/virglrenderer.pc" ] &&
    [ "$(cat "$virglrenderer_stamp" 2>/dev/null)" = "$virglrenderer_want" ]; then
    echo "virglrenderer up to date ($(git -C "$virglrenderer_upstream" rev-parse --short HEAD))"
    exit 0
fi

for tool in meson ninja; do
    command -v "$tool" >/dev/null 2>&1 || { echo "missing $tool (brew install $tool)" >&2; exit 1; }
done

if ! python3 -c 'import yaml, mako' 2>/dev/null; then
    [ -d "$virglrenderer_venv" ] || python3 -m venv "$virglrenderer_venv"
    "$virglrenderer_venv/bin/pip" install -q pyyaml mako
    PYTHONPATH="$("$virglrenderer_venv/bin/python" -c 'import site; print(site.getsitepackages()[0])')"
    export PYTHONPATH
fi

if [ "$(uname -s)" = Darwin ]; then
    mkdir -p "$virglrenderer_pcdir"
    cat > "$virglrenderer_pcdir/vulkan.pc" <<PC
Name: vulkan
Description: MoltenVK standing in for a Vulkan loader
Version: 1.4.357
Cflags: -I$MoltenVK_prefix/include
Libs: $MoltenVK_lib -framework Metal -framework Foundation -framework QuartzCore -framework IOSurface -framework IOKit -framework CoreGraphics -framework AppKit -lc++
PC
fi

# The source built is the submodule's commit with the patches applied, in a tree of its own:
# the submodule stays as upstream has it.
rm -rf "$virglrenderer_src" "$virglrenderer_build" "$virglrenderer_prefix"
git -c advice.detachedHead=false clone -q -c core.autocrlf=false "$virglrenderer_upstream" "$virglrenderer_src"
git -C "$virglrenderer_src" checkout -q --detach "$virglrenderer_commit"
for patch in "$virglrenderer_patches"/*.patch; do
    git -C "$virglrenderer_src" apply "$patch"
done

# How venus reaches Vulkan. Linked on macOS, where the Vulkan is MoltenVK built into the
# binary above and there is no loader to open. Opened at run time everywhere else: linked,
# a server needs `libvulkan.so.1` to *start*, so a host without a Vulkan loader
# could run no session at all rather than only no GPU one.
if [ "$(uname -s)" = Darwin ]; then
    vulkan_dload=false
else
    vulkan_dload=true
fi

# `--libdir=lib` below because meson's default is the host's: on Debian and Ubuntu that is
# `lib/x86_64-linux-gnu`, and `.cargo/config.toml` points rutabaga_gfx's pkg-config at
# `lib/pkgconfig` -- where macOS already puts it, and where it would find nothing there.
echo "Configuring virglrenderer ($(git -C "$virglrenderer_upstream" rev-parse --short HEAD), patched)"
PKG_CONFIG_PATH="$virglrenderer_pcdir${PKG_CONFIG_PATH:+:$PKG_CONFIG_PATH}" \
    meson setup "$virglrenderer_build" "$virglrenderer_src" \
        -Dvenus=true \
        -Dvrend=false \
        -Drender-server-mode=thread \
        -Drender-server-worker=thread \
        -Dvulkan-dload="$vulkan_dload" \
        -Ddefault_library=static \
        --prefix="$virglrenderer_prefix" \
        --libdir=lib \
        --buildtype=release >/dev/null

echo "Compiling virglrenderer"
PKG_CONFIG_PATH="$virglrenderer_pcdir${PKG_CONFIG_PATH:+:$PKG_CONFIG_PATH}" meson compile -C "$virglrenderer_build" >/dev/null
PKG_CONFIG_PATH="$virglrenderer_pcdir${PKG_CONFIG_PATH:+:$PKG_CONFIG_PATH}" meson install -C "$virglrenderer_build" >/dev/null

if [ -f "$virglrenderer_pcdir/vulkan.pc" ]; then
    cp "$virglrenderer_pcdir/vulkan.pc" "$virglrenderer_prefix/lib/pkgconfig/vulkan.pc"
fi
printf '%s\n' "$virglrenderer_want" > "$virglrenderer_stamp"

echo "virglrenderer $(PKG_CONFIG_PATH="$virglrenderer_prefix/lib/pkgconfig" pkg-config --modversion virglrenderer) installed to $virglrenderer_prefix"
