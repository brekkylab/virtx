#!/bin/sh
# Build `cortex-abins/` for the guest and install it where a session picks it up:
# `$CORTEX_KRUN_HOME/abin`, or `~/.cache/cortex-krun/abin` when that is unset. The programs go
# at the top of that directory and what each one is -- its `SKILLS.md` -- under `skills/`, so
# that a session gets the programs and the reading of them from the one mount.
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
#
# The third name a program has -- the directory its crate is in -- is the program's own:
# `cortex-abins/index/` is where `index` is written. That is what makes a program's skill
# findable from this list alone, and it is checked below rather than assumed.
#
# `docread` is not on it, and that is a cross-compilation wall rather than a decision about
# what a session needs. It links pdfium into every build now, pdfium is C++, and the sysroot
# `scripts/fetch-musl-sysroot.sh` puts in `data/` is libc and nothing else -- no `c++` headers,
# no libc++ -- so there is no C++ toolchain here aimed at musl. `cortex-abins/docread/build.rs`
# says the same thing when it is asked for the guest's target. What lifts it is a
# `libpdfium.a` built for `<arch>-unknown-linux-musl` somewhere that has that toolchain,
# dropped at `~/.cache/cortex/pdfium/chromium-7881/<triple>/libpdfium.a`, where `build.rs`
# looks before it builds anything; then put `cortex-exec-docread:docread` back on the line
# below.
PROGRAMS='cortex-exec-index:index cortex-exec-mem:mem'

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
# The root the server reads, spelled in `sh`: `CORTEX_KRUN_HOME`, else `cortex-krun` under the
# user's cache directory -- `cortex_krun::home`'s rule, and the one place here that repeats it.
[ -n "$OUT" ] || OUT="${CORTEX_KRUN_HOME:-${XDG_CACHE_HOME:-$HOME/.cache}/cortex-krun}/abin"

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

# The skill beside the program: `<crate>/SKILLS.md` becomes `/abin/skills/<program>/SKILLS.md`,
# which is where a session reads what a program is and how it is called. Under `skills/` rather
# than beside the executables, because the top of `/abin` is what `PATH` names -- a file there
# that is not a program is something a shell would offer to run.
#
# A program whose crate carries no `SKILLS.md` installs none, rather than being a build that
# fails: what a session loses is the reading, not the program. One that used to be there is
# taken back out, because a skill left behind would go on describing a program from a file the
# tree no longer has -- by its own path only, so that nothing this script did not put there is
# ever removed.
for entry in $PROGRAMS; do
    program="${entry#*:}"
    [ -d "$ROOT/cortex-abins/$program" ] ||
        die "$program is not the name of its own crate directory -- see PROGRAMS"
    from="$ROOT/cortex-abins/$program/SKILLS.md"
    at="$OUT/skills/$program"
    if [ -f "$from" ]; then
        mkdir -p "$at"
        cp "$from" "$at/SKILLS.md.new"
        chmod 644 "$at/SKILLS.md.new"
        mv "$at/SKILLS.md.new" "$at/SKILLS.md"
    else
        rm -f "$at/SKILLS.md"
        rmdir "$at" 2>/dev/null || true
    fi
done

echo "build-abin: installed to $OUT" >&2
ls -l "$OUT" >&2
[ -d "$OUT/skills" ] && ls -l "$OUT/skills" >&2 || true
