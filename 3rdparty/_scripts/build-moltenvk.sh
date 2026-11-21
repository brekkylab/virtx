#!/bin/sh
# MoltenVK, macOS's Vulkan, which virglrenderer links there: built from 3rdparty/MoltenVK against
# 3rdparty/SPIRV-Cross, each with its patches of 3rdparty/_patches applied, and its package kept
# in build/MoltenVK. A script of its own, as each library's is, so that CI keeps what one builds
# by that script, its submodules and their patches alone.
set -eu

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"

REBUILD=0
for arg in "$@"; do
    case "$arg" in
        --rebuild) REBUILD=1 ;;
        *) echo "usage: $0 [--rebuild]" >&2; exit 1 ;;
    esac
done

echo "Setting up MoltenVK"

MoltenVK_upstream="$ROOT/3rdparty/MoltenVK"
MoltenVK_patches="$ROOT/3rdparty/_patches/MoltenVK"
MoltenVK_src="$ROOT/build/MoltenVK-src"
MoltenVK_prefix="$ROOT/build/MoltenVK"
MoltenVK_lib="$MoltenVK_prefix/static/MoltenVK.xcframework/macos-arm64_x86_64/libMoltenVK.a"
MoltenVK_stamp="$MoltenVK_prefix/.virtx-stamp"
SPIRV_Cross_upstream="$ROOT/3rdparty/SPIRV-Cross"
SPIRV_Cross_patches="$ROOT/3rdparty/_patches/SPIRV-Cross"
SPIRV_Cross_src="$ROOT/build/SPIRV-Cross-src"

if [ ! -f "$MoltenVK_upstream/Makefile" ] || [ ! -f "$SPIRV_Cross_upstream/CMakeLists.txt" ]; then
    echo "MoltenVK or SPIRV-Cross is not checked out; run: git submodule update --init" >&2
    exit 1
fi

if ! xcodebuild -version >/dev/null 2>&1; then
    echo "MoltenVK needs Xcode, not only the Command Line Tools." >&2
    echo "Install Xcode, then: sudo xcode-select -s /Applications/Xcode.app" >&2
    exit 1
fi

MoltenVK_commit="$(git -C "$MoltenVK_upstream" rev-parse HEAD)"
SPIRV_Cross_commit="$(git -C "$SPIRV_Cross_upstream" rev-parse HEAD)"
# MoltenVK is written against one revision of SPIRV-Cross, which is the one the patches are on.
SPIRV_Cross_pin="$(cat "$MoltenVK_upstream/ExternalRevisions/SPIRV-Cross_repo_revision")"
if [ "$SPIRV_Cross_commit" != "$SPIRV_Cross_pin" ]; then
    echo "3rdparty/SPIRV-Cross is not at the revision MoltenVK pins ($SPIRV_Cross_pin)." >&2
    echo "Pin it there, and bring 3rdparty/_patches/SPIRV-Cross to that revision." >&2
    exit 1
fi

# What the built MoltenVK is built from: this script, both submodules' commits and the patches
# applied to each. The same as the last build's, and it is not built again.
MoltenVK_want="$(printf '%s\n' "$(git hash-object "$0")" \
    "$MoltenVK_commit" "$SPIRV_Cross_commit" \
    "$(cat "$MoltenVK_patches"/*.patch "$SPIRV_Cross_patches"/*.patch | git hash-object --stdin)")"
if [ "$REBUILD" = 0 ] && [ -f "$MoltenVK_lib" ] &&
    [ "$(cat "$MoltenVK_stamp" 2>/dev/null)" = "$MoltenVK_want" ]; then
    echo "MoltenVK up to date ($(git -C "$MoltenVK_upstream" rev-parse --short HEAD))"
else
    # Each source built is its submodule's commit with its patches applied, in a tree of its own:
    # the submodules stay as upstream has them.
    rm -rf "$MoltenVK_src" "$SPIRV_Cross_src" "$MoltenVK_prefix"
    git -c advice.detachedHead=false clone -q "$MoltenVK_upstream" "$MoltenVK_src"
    git -C "$MoltenVK_src" checkout -q --detach "$MoltenVK_commit"
    for patch in "$MoltenVK_patches"/*.patch; do
        git -C "$MoltenVK_src" apply "$patch"
    done
    git -c advice.detachedHead=false clone -q "$SPIRV_Cross_upstream" "$SPIRV_Cross_src"
    git -C "$SPIRV_Cross_src" checkout -q --detach "$SPIRV_Cross_commit"
    for patch in "$SPIRV_Cross_patches"/*.patch; do
        git -C "$SPIRV_Cross_src" apply "$patch"
    done

    echo "Fetching MoltenVK dependencies ($(git -C "$MoltenVK_upstream" rev-parse --short HEAD), patched)"
    (cd "$MoltenVK_src" && ./fetchDependencies --macos --spirv-cross-root "$SPIRV_Cross_src" >/dev/null)

    echo "Compiling MoltenVK "
    make -C "$MoltenVK_src" macos

    # Its package is all that is kept of the build: the headers and the libraries.
    cp -R "$MoltenVK_src/Package/Release/MoltenVK" "$MoltenVK_prefix"
    if [ ! -f "$MoltenVK_lib" ]; then
        echo "MoltenVK build finished but $MoltenVK_lib is missing" >&2
        exit 1
    fi
    printf '%s\n' "$MoltenVK_want" > "$MoltenVK_stamp"
    echo "MoltenVK built $MoltenVK_lib"
fi
