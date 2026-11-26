#!/bin/sh
# The GPU libraries the servers' `gpu` feature links, each built by its own script in 3rdparty/_scripts/,
# in the order one links another. What is given here is given to each: `--rebuild`.
set -eu

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"

if [ "$(uname -s)" = Darwin ]; then
    sh "$ROOT/3rdparty/_scripts/build-moltenvk.sh" "$@"
fi
sh "$ROOT/3rdparty/_scripts/build-virglrenderer.sh" "$@"
