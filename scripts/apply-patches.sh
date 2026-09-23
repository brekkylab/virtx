#!/bin/sh
# Apply the out-of-tree patches to the libkrun and rutabaga_gfx submodules, in
# place.
#
# These two are Rust crates that cargo resolves by path, so unlike the
# virglrenderer patch (see build-virglrenderer.sh) they cannot be applied to an
# exported copy under data/ -- cargo needs the real tree. The submodules
# therefore carry the patches in their working trees and are marked
# `ignore = dirty` in .gitmodules.
#
# All of them are applied on every host. The host-specific ones either touch
# code no other host compiles or are `cfg`-gated to it, and applying them
# everywhere keeps the submodule in a single canonical state -- so the
# `apply --check` below reports a stale patch as soon as the submodule moves,
# rather than on whichever day someone next builds for that host.
#
# One patch per file, even where a file needs work for two different hosts:
# stacking two patches on one file breaks the `apply --reverse --check` above,
# because the first one's context no longer matches once the second is in.
#
# Running this twice is a no-op.
set -eu

ROOT="$(cd "$(dirname "$0")/.." && pwd)"

apply_patch() {
    submodule="$1"
    patch_file="$2"
    dir="$ROOT/3rdparty/$submodule"

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
apply_patch libkrun "$ROOT/3rdparty/patches/libkrun-85bed71-overlay-root.patch"
apply_patch libkrun "$ROOT/3rdparty/patches/libkrun-85bed71-session-mounts.patch"
apply_patch libkrun "$ROOT/3rdparty/patches/libkrun-85bed71-gpu.patch"
apply_patch libkrun "$ROOT/3rdparty/patches/libkrun-85bed71-windows-virtiofs.patch"
apply_patch libkrun "$ROOT/3rdparty/patches/libkrun-85bed71-windows-vsock.patch"
apply_patch rutabaga_gfx "$ROOT/3rdparty/patches/rutabaga_gfx-8cb133e-virgl-renderer.patch"
