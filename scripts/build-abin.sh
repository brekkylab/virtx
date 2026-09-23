#!/bin/sh
# Build `cortex-abins/` for the guest and install it where a session picks it up:
# `$CORTEX_KRUN_HOME/abin`, or `~/.cache/cortex-krun/abin` when that is unset.
#
#   scripts/build-abin.sh              this host's architecture, release
#   scripts/build-abin.sh --out <dir>  somewhere else
#   scripts/build-abin.sh --debug      unstripped, and several times the size
#
# The server looks for that one directory and boots without an `/abin` if it is not there, so
# this is a step somebody runs rather than a dependency of `cargo build`. It is separate from
# the agent for the same reason the two are different mounts: the agent is this build's and
# is copied per machine, and these are the programs a session runs by name.
#
# Static musl for the guest's architecture, because the image is whoever's the session named
# -- an Alpine and a `python:3.13-slim` do not have the same libc, and `/abin` is one set of
# executables for every session. See `cortex-abins/.cargo/config.toml` for what compiles them.
set -eu

die() { echo "build-abin: $*" >&2; exit 1; }

# What `/abin` holds, as `<package>:<program>`. The two are not the same string -- a crate is
# `cortex-exec-index` so that it does not collide on crates.io, and the program is `index`
# because that is what a caller types. This is the only statement of what `/abin` holds.
PROGRAMS='cortex-exec-docread:docread cortex-exec-index:index cortex-exec-mem:mem'

PROFILE=release
OUT=
while [ $# -gt 0 ]; do
    case "$1" in
        --out) OUT="${2:-}"; [ -n "$OUT" ] || die "--out was given nothing to be"; shift 2 ;;
        --debug) PROFILE=debug; shift ;;
        *) die "$1 is not an argument this takes" ;;
    esac
done

case "$(uname -m)" in
    arm64 | aarch64) ARCH=aarch64 ;;
    x86_64) ARCH=x86_64 ;;
    *) die "unsupported host arch: $(uname -m)" ;;
esac
TRIPLE="$ARCH-unknown-linux-musl"

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
[ -n "$OUT" ] || OUT="${CORTEX_KRUN_HOME:-$HOME/.cache/cortex-krun}/abin"

[ -e "$ROOT/data/musl-sysroot/$ARCH/usr/include/stdio.h" ] ||
    die "no musl sysroot -- run scripts/fetch-musl-sysroot.sh"

set --
for entry in $PROGRAMS; do
    set -- "$@" -p "${entry%%:*}"
done

echo "build-abin: building $TRIPLE ($PROFILE)" >&2
if [ "$PROFILE" = release ]; then
    (cd "$ROOT/cortex-abins" && cargo build --release --target "$TRIPLE" "$@")
else
    (cd "$ROOT/cortex-abins" && cargo build --target "$TRIPLE" "$@")
fi

# Into place one file at a time rather than by replacing the directory: a session that is up
# has this mounted, and swapping the directory under a running guest is a mount pointing at
# something that is no longer there.
mkdir -p "$OUT"
for entry in $PROGRAMS; do
    program="${entry#*:}"
    from="$ROOT/cortex-abins/target/$TRIPLE/$PROFILE/$program"
    [ -x "$from" ] || die "$program was not built"
    cp "$from" "$OUT/$program.new"
    chmod 755 "$OUT/$program.new"
    mv "$OUT/$program.new" "$OUT/$program"
done

echo "build-abin: installed to $OUT" >&2
ls -l "$OUT" >&2
