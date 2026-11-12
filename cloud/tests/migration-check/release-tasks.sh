#!/bin/sh
# Lets the long tasks run: P_c and P_d are resumed, so that one runs and the
# other waits for the one CPU.
#   release-tasks.sh --work DIR --clients FILE --tag TAG --job-prefix P
usage() { sed -n '2,/^usage()/p' "$0" | sed '$d' >&2; exit 2; }
set -eu
work="" clients="" tag="" prefix=""
while [ $# -gt 0 ]; do
    case "$1" in
        --work) work="$2" ;; --clients) clients="$2" ;; --tag) tag="$2" ;; --job-prefix) prefix="$2" ;;
        *) usage ;;
    esac
    shift 2
done
WORK="$work" CLIENTS="$clients"
. "$(dirname "$0")/lib.sh"
load_client "$tag"
cc --task "$MASTER_URL" "${prefix}_c_0" resume > /dev/null
cc --task "$MASTER_URL" "${prefix}_d_0" resume > /dev/null
/bin/sleep 3
tasks="$(cc --get_tasks)" || die "boinccmd --get_tasks failed for $C_TAG"
printf '%s\n' "$tasks" | grep -E "^   (name|state|active_task_state)" | tr '\n' ' ' | sed 's/   name/\n  name/g'
echo
