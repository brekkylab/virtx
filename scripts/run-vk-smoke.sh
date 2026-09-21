#!/bin/sh
# Build and run guest/vk_smoke.c inside the VM.
#
# The rootfs is a plain host directory shared over virtio-fs, so the source is
# just copied in and compiled by the guest's own toolchain -- no cross
# compilation. The build deps are installed on first run and persist in the
# rootfs.
set -eu

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
ROOTFS="$ROOT/data/rootfs/alpine"

[ -d "$ROOTFS" ] || { echo "no rootfs; run ./scripts/fetch-alpine-rootfs.sh" >&2; exit 1; }

mkdir -p "$ROOTFS/root"
cp "$ROOT/guest/vk_smoke.c" "$ROOTFS/root/"

exec cargo run --features gpu --example macos_gpu -- '
    set -e
    command -v cc >/dev/null 2>&1 || apk add --no-cache -q build-base
    [ -e /usr/include/vulkan/vulkan.h ] || apk add --no-cache -q vulkan-headers vulkan-loader-dev
    [ -e /usr/share/vulkan/icd.d/virtio_icd.aarch64.json ] || apk add --no-cache -q mesa-vulkan-virtio vulkan-loader
    cd /root
    cc -O2 -Wall -o vk_smoke vk_smoke.c -lvulkan
    ./vk_smoke
'
