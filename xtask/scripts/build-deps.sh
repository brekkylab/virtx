#!/bin/sh
set -eu

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"

REBUILD=0
for arg in "$@"; do
    case "$arg" in
        --rebuild) REBUILD=1 ;;
        *) echo "usage: $0 [--rebuild]" >&2; exit 1 ;;
    esac
done

# MoltenVK, macOS's Vulkan, which virglrenderer links there. A script of its own, so CI keeps
# what it builds by that script and the two submodules alone. Sourced: it sets MoltenVK_src,
# MoltenVK_lib and MoltenVK_want for what follows.
if [ "$(uname -s)" = Darwin ]; then
    . "$ROOT/xtask/scripts/build-moltenvk.sh"
fi

echo "Setting up virglrenderer"

virglrenderer_src="$ROOT/3rdparty/virglrenderer"
virglrenderer_build="$ROOT/build/virglrenderer-build"
virglrenderer_prefix="$ROOT/build/virglrenderer"
virglrenderer_pcdir="$ROOT/build/pkgconfig"
virglrenderer_venv="$ROOT/build/meson-venv"
virglrenderer_stamp="$virglrenderer_prefix/.virtx-stamp"

# What the installed virglrenderer was built from: this script, the submodule's commit and any
# change in its tree, where it is installed (its .pc names the prefix), and on macOS the
# MoltenVK it links. The same as the last build's, and it is not built again -- which is also
# what lets CI keep it in a cache.
tree_state() {
    (cd "$1" && {
        git rev-parse HEAD
        { git diff HEAD; git ls-files -o --exclude-standard -z | xargs -0 git hash-object; } | git hash-object --stdin
    })
}
virglrenderer_want="$(printf '%s\n' "$virglrenderer_prefix" "$(git hash-object "$0")" \
    "$(tree_state "$virglrenderer_src")" "${MoltenVK_want:-}")"
if [ "$REBUILD" = 0 ] && [ -f "$virglrenderer_prefix/lib/pkgconfig/virglrenderer.pc" ] &&
    [ "$(cat "$virglrenderer_stamp" 2>/dev/null)" = "$virglrenderer_want" ]; then
    echo "virglrenderer up to date ($(git -C "$virglrenderer_src" rev-parse --short HEAD))"
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
Cflags: -I$MoltenVK_src/Package/Release/MoltenVK/include
Libs: $MoltenVK_lib -framework Metal -framework Foundation -framework QuartzCore -framework IOSurface -framework IOKit -framework CoreGraphics -framework AppKit -lc++
PC
fi

if [ "$REBUILD" = 1 ]; then
    rm -rf "$virglrenderer_build" "$virglrenderer_prefix"
fi

if [ -f "$virglrenderer_build/build.ninja" ]; then
    reconfigure="--reconfigure"
else
    reconfigure=""
fi

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
echo "Configuring virglrenderer ($(git -C "$virglrenderer_src" rev-parse --short HEAD))"
PKG_CONFIG_PATH="$virglrenderer_pcdir${PKG_CONFIG_PATH:+:$PKG_CONFIG_PATH}" \
    meson setup $reconfigure "$virglrenderer_build" "$virglrenderer_src" \
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
