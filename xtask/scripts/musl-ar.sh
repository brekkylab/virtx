#!/bin/sh
# The archiver that goes with `musl-cc.sh`, and it cannot be Apple's: `/usr/bin/ar` is
# cctools', it writes the Mach-O flavour of the symbol table whatever the objects inside are,
# and `rust-lld` then links an archive it believes holds nothing -- which arrives as several
# hundred `undefined symbol: sqlite3_*` at the end of an otherwise clean build.
#
# `llvm-ar` ships with the LLVM that this repository already asks for. Homebrew's `llvm` is
# keg-only, so it is looked for where brew puts it as well as on `PATH`.
set -eu

for candidate in llvm-ar /opt/homebrew/opt/llvm/bin/llvm-ar /usr/local/opt/llvm/bin/llvm-ar; do
    if command -v "$candidate" >/dev/null 2>&1; then
        exec "$candidate" "$@"
    fi
done

echo "no llvm-ar found -- brew install llvm, or put one on PATH" >&2
exit 1
