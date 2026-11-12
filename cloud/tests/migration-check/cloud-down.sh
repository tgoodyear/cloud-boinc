#!/bin/sh
# Stops the local Workers started by cloud-up.sh.
#   cloud-down.sh --work DIR
usage() { sed -n '2,/^usage()/p' "$0" | sed '$d' >&2; exit 2; }
set -eu
[ "${1:-}" = --work ] && [ -n "${2:-}" ] || { usage; }
pid="$(cat "$2/cloud-state/pid" 2>/dev/null || true)"
[ -n "$pid" ] && kill -- "-$pid" 2>/dev/null || true
/bin/sleep 2
echo stopped
