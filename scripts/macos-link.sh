#!/bin/sh
# Cargo linker for aarch64-apple-darwin: `cc`, and then the one signature a build
# has to produce.
#
# `cortex-krun-host` is the process that calls `hv_vm_create`, and
# Hypervisor.framework refuses that without the `com.apple.security.hypervisor`
# entitlement. An entitlement is carried by a code signature and is read at
# `exec`, so it has to be applied to the file after it is linked -- an
# `__entitlements` section from the linker is not read, and `cargo build` leaves
# only the ad-hoc signature the linker itself writes, which carries none.
#
# Here rather than in a build script because cargo runs a build script *before*
# its own crate is linked and offers no hook after, and here rather than only in
# the cargo runner because a console server is started by whoever wants a
# session -- a test, an agent runtime, a CLI -- and `cargo run` is not on that
# path. A plain `cargo build` has to be enough.
#
# Only that one binary. Everything else linked for this target is passed through
# untouched, and `scripts/macos-run.sh` is what signs whatever `cargo run` was
# asked for.
set -eu

cc "$@"

# Where the linked file went. `-o <path>`, and rustc puts the whole command line
# in an `@argfile` when it grows too long -- so both are read, the argfile only
# when the arguments themselves did not say.
out=""
argfile=""
prev=""
for arg in "$@"; do
    [ "$prev" = "-o" ] && out="$arg"
    case "$arg" in
    @*) argfile="${arg#@}" ;;
    esac
    prev="$arg"
done
if [ -z "$out" ] && [ -n "$argfile" ] && [ -f "$argfile" ]; then
    # One argument per line, which is the format rustc writes.
    out=$(awk '/^-o$/ { getline; print; exit }' "$argfile")
fi

# The name rustc links under, which is not the name cargo puts beside the server:
# a binary is linked into `deps/` as `<crate name>-<hash>` and hardlinked to its
# final name afterwards, so signing the file rustc wrote is signing both. The
# unsuffixed form is there for a cargo that copied rather than linked.
case "${out##*/}" in
cortex-krun-host | cortex_krun_host-*)
    SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
    # Ad-hoc -- `-s -` -- because what matters is the entitlement and not who
    # signed it. Its output is held back unless it fails: codesign says
    # "replacing existing signature" on every relink, and cargo reports anything
    # a linker writes as a warning.
    if ! said=$(codesign --entitlements "$SCRIPT_DIR/hvf-entitlements.plist" \
        --force -s - "$out" 2>&1); then
        printf '%s\n' "$said" >&2
        echo "codesign of $out failed -- without the hypervisor entitlement it cannot create a VM" >&2
        exit 1
    fi
    ;;
esac
