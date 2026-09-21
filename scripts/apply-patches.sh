#!/bin/sh
# Apply the macOS GPU patches to the krun and rutabaga submodules, in place.
#
# These two are Rust crates that cargo resolves by path, so unlike the
# virglrenderer patch (see build-virglrenderer.sh) they cannot be applied to an
# exported copy under data/ -- cargo needs the real tree. The submodules
# therefore carry the patches in their working trees and are marked
# `ignore = dirty` in .gitmodules.
#
# Running this twice is a no-op.
set -eu

ROOT="$(cd "$(dirname "$0")/.." && pwd)"

apply_patch() {
    submodule="$1"
    patch_file="$2"
    dir="$ROOT/$submodule"

    if [ ! -d "$dir/.git" ] && [ ! -f "$dir/.git" ]; then
        echo "$submodule is not checked out; run: git submodule update --init" >&2
        exit 1
    fi

    if git -C "$dir" apply --reverse --check "$patch_file" 2>/dev/null; then
        echo "  $submodule: already patched"
        return
    fi

    if ! git -C "$dir" apply --check "$patch_file" 2>/dev/null; then
        echo "  $submodule: patch does not apply -- the submodule has moved or" >&2
        echo "  has other local changes. Reset it or refresh $patch_file." >&2
        exit 1
    fi

    git -C "$dir" apply "$patch_file"
    echo "  $submodule: applied $(basename "$patch_file")"
}

echo "applying patches"
apply_patch krun "$ROOT/patches/libkrun-e6cdb55-macos-gpu.patch"
apply_patch rutabaga "$ROOT/patches/rutabaga-8cb133e-macos-virgl-renderer.patch"
