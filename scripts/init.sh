#!/bin/sh
set -eu

ROOT="$(cd "$(dirname "$0")/.." && pwd)"

REBUILD=0
for arg in "$@"; do
    case "$arg" in
        --rebuild) REBUILD=1 ;;
        *) echo "usage: $0 [--rebuild]" >&2; exit 1 ;;
    esac
done

echo "Applying patches"

apply_patch() {
    submodule="$1"
    patch_file="$2"
    dir="$ROOT/3rdparty/$submodule"

    if [ ! -d "$dir/.git" ] && [ ! -f "$dir/.git" ]; then
        echo "$submodule is not checked out; run: git submodule update --init" >&2
        exit 1
    fi

    if git -C "$dir" apply --reverse --check "$patch_file" 2>/dev/null; then
        echo "$submodule: already patched"
        return
    fi

    if ! git -C "$dir" apply --check "$patch_file" 2>/dev/null; then
        echo "$submodule: patch does not apply -- the submodule has moved or" >&2
        echo "has other local changes. Reset it or refresh $patch_file." >&2
        exit 1
    fi

    git -C "$dir" apply "$patch_file"
    echo "$submodule: applied $(basename "$patch_file")"
}

for patch_file in "$ROOT"/3rdparty/_patches/*.patch; do
    patch_name="$(basename "$patch_file")"
    apply_patch "$(printf '%s' "$patch_name" | sed -E 's/-[0-9a-f]{7,40}-.*$//')" "$patch_file"
done

if [ "$(uname -s)" = Darwin ]; then
    echo "Setting up MoltenVK"

    MoltenVK_src="$ROOT/3rdparty/MoltenVK"
    MoltenVK_lib="$MoltenVK_src/Package/Release/MoltenVK/static/MoltenVK.xcframework/macos-arm64_x86_64/libMoltenVK.a"
    MoltenVK_stamp="$MoltenVK_src/Package/.cortex-stamp"

    if [ ! -f "$MoltenVK_src/Makefile" ]; then
        echo "MoltenVK is not checked out; run: git submodule update --init" >&2
        exit 1
    fi

    if ! xcodebuild -version >/dev/null 2>&1; then
        echo "MoltenVK needs Xcode, not only the Command Line Tools." >&2
        echo "Install Xcode, then: sudo xcode-select -s /Applications/Xcode.app" >&2
        exit 1
    fi

    if [ "$REBUILD" = 1 ]; then
        rm -f "$MoltenVK_stamp"
    fi

    SPIRV_Cross_src="$ROOT/3rdparty/SPIRV-Cross"
    SPIRV_Cross_pin="$(cat "$MoltenVK_src/ExternalRevisions/SPIRV-Cross_repo_revision")"
    if [ "$(git -C "$SPIRV_Cross_src" rev-parse HEAD)" != "$SPIRV_Cross_pin" ]; then
        echo "3rdparty/SPIRV-Cross is not at the revision MoltenVK pins ($SPIRV_Cross_pin)." >&2
        echo "Move the submodule there and refresh its patches." >&2
        exit 1
    fi

    MoltenVK_want="$(for dir in "$MoltenVK_src" "$SPIRV_Cross_src"; do (cd "$dir" && {
        git rev-parse HEAD
        { git diff HEAD; git ls-files -o --exclude-standard -z | xargs -0 git hash-object; } | shasum | cut -d' ' -f1
    }); done)"
    if [ -f "$MoltenVK_lib" ] && [ "$(cat "$MoltenVK_stamp" 2>/dev/null)" = "$MoltenVK_want" ]; then
        echo "MoltenVK up to date ($(git -C "$MoltenVK_src" rev-parse --short HEAD))"
    else
        echo "Fetching MoltenVK dependencies ($(git -C "$MoltenVK_src" rev-parse --short HEAD))"
        (cd "$MoltenVK_src" && ./fetchDependencies --macos --spirv-cross-root "$SPIRV_Cross_src" >/dev/null)

        echo "Compiling MoltenVK "
        make -C "$MoltenVK_src" macos

        if [ ! -f "$MoltenVK_lib" ]; then
            echo "MoltenVK build finished but $MoltenVK_lib is missing" >&2
            exit 1
        fi
        printf '%s\n' "$MoltenVK_want" > "$MoltenVK_stamp"
        echo "MoltenVK built $MoltenVK_lib"
    fi
fi

echo "Setting up virglrenderer"

virglrenderer_src="$ROOT/3rdparty/virglrenderer"
virglrenderer_build="$ROOT/build/virglrenderer-build"
virglrenderer_prefix="$ROOT/build/virglrenderer"
virglrenderer_pcdir="$ROOT/build/pkgconfig"
virglrenderer_venv="$ROOT/build/meson-venv"

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

echo "Configuring virglrenderer ($(git -C "$virglrenderer_src" rev-parse --short HEAD))"
PKG_CONFIG_PATH="$virglrenderer_pcdir${PKG_CONFIG_PATH:+:$PKG_CONFIG_PATH}" \
    meson setup $reconfigure "$virglrenderer_build" "$virglrenderer_src" \
        -Dvenus=true \
        -Dvrend=false \
        -Drender-server-mode=thread \
        -Drender-server-worker=thread \
        -Dvulkan-dload=false \
        -Ddefault_library=static \
        --prefix="$virglrenderer_prefix" \
        --buildtype=release >/dev/null

echo "Compiling virglrenderer"
PKG_CONFIG_PATH="$virglrenderer_pcdir${PKG_CONFIG_PATH:+:$PKG_CONFIG_PATH}" meson compile -C "$virglrenderer_build" >/dev/null
PKG_CONFIG_PATH="$virglrenderer_pcdir${PKG_CONFIG_PATH:+:$PKG_CONFIG_PATH}" meson install -C "$virglrenderer_build" >/dev/null

if [ -f "$virglrenderer_pcdir/vulkan.pc" ]; then
    cp "$virglrenderer_pcdir/vulkan.pc" "$virglrenderer_prefix/lib/pkgconfig/vulkan.pc"
fi

echo "virglrenderer $(PKG_CONFIG_PATH="$virglrenderer_prefix/lib/pkgconfig" pkg-config --modversion virglrenderer) installed to $virglrenderer_prefix"
