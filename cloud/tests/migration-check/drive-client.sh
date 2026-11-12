#!/bin/sh
# Brings one client to the state it is left in at the switch.
#   drive-client.sh --work DIR --clients FILE --tag TAG --job-prefix P [--long-seconds N]
# The client (a line of FILE, see lib.sh) is started in DIR/clients/TAG and attached
# to the stock server with its account's key. Then, one step at a time:
#  1. job P_a (5 s) is fetched, run, uploaded and reported, and validated by the stock daemons;
#  2. jobs P_b (5 s), P_c and P_d (--long-seconds each, default 240) are made and fetched while the client does
#     not compute, P_c and P_d are suspended, and computing is allowed: P_b runs, finishes
#     and uploads, and the client's network is switched off before its report goes out
#     (the stock server's request delay of 32 seconds is the time it has);
# release-tasks.sh then lets P_c and P_d run, which leaves one running and one not started.
usage() { sed -n '2,/^usage()/p' "$0" | sed '$d' >&2; exit 2; }
set -eu
work="" clients="" tag="" prefix="" long=240
while [ $# -gt 0 ]; do
    case "$1" in
        --work) work="$2" ;; --clients) clients="$2" ;; --tag) tag="$2" ;; --job-prefix) prefix="$2" ;;
        --long-seconds) long="$2" ;;
        *) usage ;;
    esac
    shift 2
done
[ -n "$work" ] && [ -n "$clients" ] && [ -n "$tag" ] && [ -n "$prefix" ] || { usage; }
WORK="$work" CLIENTS="$clients"
. "$(dirname "$0")/lib.sh"
load_client "$tag"
mkjob() { sh "$HERE/stock-jobs.sh" --work "$WORK" "$@" > /dev/null; }

log "$C_TAG ($C_VER): job a"
mkjob --name "${prefix}_a" --seconds 5
/bin/sleep 8
sh "$HERE/client.sh" --action start --dir "$C_DIR" --boinc "$C_BOINC" --port "$C_PORT" \
    --name "cb-mig-$C_TAG" --lib "$C_LIB" > /dev/null
/bin/sleep 5
cc --project_attach "$MASTER_URL" "$(secret "$C_USER" authenticator)" > /dev/null
wait_log "got ack for task ${prefix}_a_0" 1 180
/bin/sleep 34

log "$C_TAG: jobs b, c, d"
cc --set_run_mode never > /dev/null
mkjob --name "${prefix}_b" --seconds 5
mkjob --name "${prefix}_c" --seconds "$long" --delay-bound 14400
mkjob --name "${prefix}_d" --seconds "$long" --delay-bound 14400
/bin/sleep 10
n=$(grep -c "Scheduler request completed" "$(client_log)" || true)
cc --project "$MASTER_URL" update > /dev/null
wait_log "Scheduler request completed" $((n + 1)) 60
grep "Scheduler request completed" "$(client_log)" | tail -1 | cut -c1-200
cc --task "$MASTER_URL" "${prefix}_c_0" suspend > /dev/null
cc --task "$MASTER_URL" "${prefix}_d_0" suspend > /dev/null
cc --set_run_mode auto > /dev/null
wait_log "Finished upload of ${prefix}_b_0" 1 60
cc --set_network_mode never > /dev/null
log "$C_TAG: network off"
tasks="$(cc --get_tasks)" || die "boinccmd --get_tasks failed for $C_TAG"
printf '%s\n' "$tasks" | grep -E "^   (name|state|active_task_state|ready to report)" | tr '\n' ' ' | sed 's/   name/\n  name/g'
echo
