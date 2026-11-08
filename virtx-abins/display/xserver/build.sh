#!/bin/sh
# Build the X server `display` runs, as static musl executables, into OUT.
#
#     sh build.sh OUT [TARBALLS]
#
# Run inside Alpine (musl, with `-static` archives for most of what is needed), as root: it
# installs its build tools with `apk`. What it writes is what `/abin/libexec/display` holds:
#
#     OUT/Xvfb      the X server: a screen in memory, no input devices, no GPU
#     OUT/xkbcomp   the keymap compiler Xvfb runs at start, found at the path built into it
#     OUT/xkb/      the keymaps xkbcomp compiles, from Alpine's xkeyboard-config
#
# Every piece is MIT-licensed. The libraries Alpine ships no static archive of are built
# here from X.org's release tarballs, listed in `sources` beside this script and checked against
# the checksums there; the rest come from `apk`. A tarball in TARBALLS is used as it is, and
# one not there is downloaded.
set -eu

HERE=$(cd "$(dirname "$0")" && pwd)
mkdir -p "${1:?usage: build.sh OUT [TARBALLS]}"
OUT=$(cd "$1" && pwd)
TARBALLS=${2:-}
# Where the three live in the guest. Built into Xvfb, which runs `$LIBEXEC/xkbcomp` with
# `-R$LIBEXEC/xkb`, so it is the one path the result cannot be moved from.
LIBEXEC=/abin/libexec/display

WORK=/tmp/display-xserver
PREFIX=$WORK/prefix
mkdir -p "$WORK/src" "$PREFIX"

apk add --no-cache \
    build-base meson ninja pkgconf bison flex curl xz perl \
    xorgproto xtrans util-macros \
    pixman-dev pixman-static \
    nettle-dev nettle-static \
    zlib-dev zlib-static \
    libx11-dev libx11-static libxcb-dev libxcb-static \
    xkeyboard-config >/dev/null

grep -v '^#' "$HERE/sources" | while read -r name dir sum; do
    [ -n "$name" ] || continue
    tarball=$WORK/src/$name.tar.xz
    if [ -n "$TARBALLS" ] && [ -f "$TARBALLS/$name.tar.xz" ]; then
        cp "$TARBALLS/$name.tar.xz" "$tarball"
    elif [ ! -f "$tarball" ]; then
        curl -fsSL --retry 5 --retry-all-errors -o "$tarball" \
            "https://www.x.org/releases/individual/$dir/$name.tar.xz"
    fi
    echo "$sum  $tarball" | sha256sum -c - >/dev/null
    rm -rf "${WORK:?}/src/$name"
    tar -xJf "$tarball" -C "$WORK/src"
done

export PKG_CONFIG_PATH=$PREFIX/lib/pkgconfig
export CFLAGS="-O2 -fno-plt"
JOBS=$(nproc)

autotools() { # name, configure args...
    name=$1
    shift
    (cd "$WORK/src/$name" &&
        ./configure --prefix="$PREFIX" --enable-static --disable-shared "$@" >/dev/null &&
        make -j"$JOBS" >/dev/null && make install >/dev/null)
}

# Static libX11 and libxcb from Alpine name these two in their `Requires.private`, and Alpine
# has no archive of either.
autotools libXau-1.0.12
autotools libXdmcp-1.1.5
autotools libfontenc-1.1.9
# The server's core fonts: the two it has built in (`fixed`, `cursor`) and the bitmap formats.
# No FreeType -- an application today draws its own text with Xft and asks the server for none
# -- and no font server.
autotools libXfont2-2.0.9 --disable-freetype --disable-fc --disable-devel-docs

(cd "$WORK/src/libxkbfile-1.2.0" &&
    meson setup build --prefix="$PREFIX" --default-library=static --prefer-static >/dev/null &&
    ninja -C build install >/dev/null)

(cd "$WORK/src/xkbcomp-1.5.0" &&
    meson setup build --prefix="$PREFIX" --prefer-static \
        -Dc_link_args=-static -Dxkb-config-root="$LIBEXEC/xkb" >/dev/null &&
    ninja -C build >/dev/null)
install -m 0755 -s "$WORK/src/xkbcomp-1.5.0/build/xkbcomp" "$OUT/xkbcomp"

# Xvfb alone. What the server does with hardware -- GLX, DRI, udev, input drivers -- is off;
# so is listening on TCP, the X authorization protocols a network peer would use (XDMCP,
# XDM-AUTH-1, SUN-DES-1), and SELinux.
(cd "$WORK/src/xorg-server-21.1.25" &&
    meson setup build --prefix="$PREFIX" --prefer-static \
        -Dc_link_args=-static \
        -Dxvfb=true -Dxorg=false -Dxephyr=false -Dxnest=false -Dxwin=false -Dxquartz=false \
        -Dglx=false -Dglamor=false -Ddri1=false -Ddri2=false -Ddri3=false -Ddrm=false \
        -Dudev=false -Dudev_kms=false -Dhal=false -Dsystemd_logind=false -Dpciaccess=false \
        -Dxdmcp=false -Dxdm-auth-1=false -Dsecure-rpc=false -Dxselinux=false -Dlibunwind=false \
        -Dlisten_tcp=false -Dsha1=libnettle -Ddocs=false -Ddevel-docs=false \
        -Ddefault_font_path=built-ins \
        -Dxkb_dir="$LIBEXEC/xkb" -Dxkb_bin_dir="$LIBEXEC" -Dxkb_output_dir=/tmp \
        >/dev/null &&
    ninja -C build hw/vfb/Xvfb >/dev/null)
install -m 0755 -s "$WORK/src/xorg-server-21.1.25/build/hw/vfb/Xvfb" "$OUT/Xvfb"

rm -rf "$OUT/xkb"
cp -R /usr/share/X11/xkb "$OUT/xkb"

for program in "$OUT/Xvfb" "$OUT/xkbcomp"; do
    if readelf -l "$program" | grep -q INTERP; then
        echo "$program is linked dynamically" >&2
        exit 1
    fi
done
du -sh "$OUT"/*
