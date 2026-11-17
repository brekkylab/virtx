#!/usr/bin/env bash
# Build the Python wheels as they are published: one per platform, with its `libvirtx_core`
# beside the Python files, and tagged for that platform.
#
#   scripts/packages/pack-wheels.sh <lib-dir> <out-dir> [--allow-missing]
#
# `<lib-dir>/<platform>/` holds each platform's library, as the `pipeline` workflow's platform
# jobs lay it out; `--allow-missing` builds only the platforms that have one. Needs `build` and
# `wheel`. hatchling builds a pure wheel, `py3-none-any`, so each is unpacked, retagged
# `py3-none-<platform>` and not purelib, and packed again.
set -euo pipefail

repo=$(cd "$(dirname "$0")/../.." && pwd)
libs=$(cd "$1" && pwd)
mkdir -p "$2"
out=$(cd "$2" && pwd)
allow_missing=${3:-}
package="$repo/virtx-py/src/virtx"
work=$(mktemp -d)
trap 'rm -rf "$work" "$package"/libvirtx_core.* "$package"/virtx_core.dll' EXIT

for entry in \
  darwin-arm64:macosx_11_0_arm64:libvirtx_core.dylib \
  linux-x64-gnu:manylinux_2_28_x86_64:libvirtx_core.so \
  linux-arm64-gnu:manylinux_2_28_aarch64:libvirtx_core.so \
  win32-x64-msvc:win_amd64:virtx_core.dll; do
  IFS=: read -r platform tag file <<< "$entry"
  lib="$libs/$platform/$file"
  if [ ! -f "$lib" ]; then
    [ "$allow_missing" = --allow-missing ] || { echo "no $lib" >&2; exit 1; }
    echo "skipping $platform: no $lib"
    continue
  fi
  rm -rf "$package"/libvirtx_core.* "$package"/virtx_core.dll "$work"/*
  cp "$lib" "$package/"
  python -m build --wheel --outdir "$work/pure" "$repo/virtx-py"
  python -m wheel unpack --dest "$work/unpacked" "$work"/pure/*.whl
  unpacked=$(echo "$work"/unpacked/*)
  sed -e 's/^Root-Is-Purelib: true$/Root-Is-Purelib: false/' \
    -e "s/^Tag: py3-none-any$/Tag: py3-none-$tag/" \
    "$unpacked"/*.dist-info/WHEEL > "$work/WHEEL"
  grep -qx "Tag: py3-none-$tag" "$work/WHEEL" || { echo "no py3-none-any tag to replace" >&2; exit 1; }
  mv "$work/WHEEL" "$unpacked"/*.dist-info/WHEEL
  python -m wheel pack --dest-dir "$out" "$unpacked"
done
ls -la "$out"
