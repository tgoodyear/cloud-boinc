#!/bin/sh
# Makes jobs on the running stock server.
#   stock-jobs.sh --work DIR --name NAME [--seconds N] [--delay-bound N]
# One workunit of the application, named NAME, whose command line makes the
# test application sleep N seconds first (default 5) and whose deadline is
# --delay-bound seconds (default 7200). The stock feeder picks it up within
# a few seconds.
usage() { sed -n '2,/^usage()/p' "$0" | sed '$d' >&2; exit 2; }
set -eu
work="" name="" seconds=5 delay=7200
while [ $# -gt 0 ]; do
    case "$1" in
        --work) work="$2" ;; --name) name="$2" ;; --seconds) seconds="$2" ;;
        --delay-bound) delay="$2" ;;
        *) usage ;;
    esac
    shift 2
done
[ -n "$work" ] && [ -n "$name" ] || { usage; }
WORK="$work"
. "$(dirname "$0")/lib.sh"
stock_exec bin/stage_file_native --copy /fixtures/inputs/in.txt >/dev/null 2>&1 || true
stock_exec bin/create_work --appname upper --wu_name "$name" --wu_template templates/upper_in \
    --result_template templates/upper_out --command_line "--seconds $seconds" \
    --delay_bound "$delay" in.txt >/dev/null
echo "created $name"
