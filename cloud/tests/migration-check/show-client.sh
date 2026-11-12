#!/bin/sh
# Prints what a client logged since the switch, without the HTTP debug lines, with
# every run of 32 or more hex digits replaced.
#   show-client.sh --work DIR --tag TAG [--http] [--all]
# --http keeps the lines of the HTTP debug flags, --all prints the whole log.
usage() { sed -n '2,/^usage()/p' "$0" | sed '$d' >&2; exit 2; }
set -eu
work="" tag="" http=0 all=0
while [ $# -gt 0 ]; do
    case "$1" in
        --http) http=1; shift; continue ;;
        --all) all=1; shift; continue ;;
        --work) work="$2" ;; --tag) tag="$2" ;;
        *) usage ;;
    esac
    shift 2
done
[ -n "$work" ] && [ -n "$tag" ] || { usage; }
d="$work/clients/$tag"
from=1
[ "$all" = 1 ] || from=$(( $(cat "$d/log-lines-at-switch") + 1 ))
if [ "$http" = 1 ]; then
    tail -n +"$from" "$d/client.log"
else
    tail -n +"$from" "$d/client.log" | grep -v '\[http\]\|http_op\|Throughput\|\[file_xfer\]'
fi | sed -E 's/[0-9a-f]{32,}/<redacted>/g'
