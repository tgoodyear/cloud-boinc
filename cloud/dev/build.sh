#!/bin/sh
# Builds the Workers to wasm with worker-build, one after the other.
#   dev/build.sh [--worker <name>]... [--dev]
# Without --worker, all five are built.
set -eu
cd "$(dirname "$0")/.."
workers=""
mode="--release"
while [ $# -gt 0 ]; do
    case "$1" in
        --worker) workers="$workers $2"; shift 2 ;;
        --dev) mode="--dev"; shift ;;
        *) echo "usage: dev/build.sh [--worker <name>]... [--dev]" >&2; exit 2 ;;
    esac
done
[ -n "$workers" ] || workers="scheduler files pipeline web admin"
for w in $workers; do
    echo "== $w"
    (cd "workers/$w" && worker-build "$mode")
done
