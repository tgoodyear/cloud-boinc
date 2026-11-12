#!/bin/sh
# What the clients do after the switch, and what the web RPCs answer. Run after switch.sh.
#   observe.sh --work DIR --clients FILE --tags "TAG ..." [--wait-seconds N] [--work-wait-seconds N]
#              [--cboinc FILE] [--app-binary FILE] [--port N]
# 1. waits (up to --wait-seconds, default 1500) until no client holds a task of the long
#    jobs (P_c, P_d), that is, until each has finished, uploaded and reported it or has
#    been told to abort it;
# 2. adds new work (post-work.sh), asks every client for it, and waits (up to
#    --work-wait-seconds, default 600) until the database says that every result of the new
#    jobs has been sent and received back: an empty task list on a client is not taken for
#    that, because a client may not have fetched yet (it waits out the request delay);
# 3. detaches every client and attaches it again with its account key;
# 4. writes DIR/results/: the log of each client since the switch (<tag>.log), the lines in it
#    that the client had not logged before (<tag>.novel), the web
#    RPCs' replies (probe-cloud.txt) and the project's tables (cloud-tables.txt).
# A wait that runs out is logged and makes the script exit with status 1 after it has written
# the results.
usage() { sed -n '2,/^usage()/p' "$0" | sed '$d' >&2; exit 2; }
set -eu
work="" clients="" tags="" wait_secs=1500 work_wait=600 port=80 cboinc="" app=""
while [ $# -gt 0 ]; do
    case "$1" in
        --work) work="$2" ;; --clients) clients="$2" ;; --tags) tags="$2" ;;
        --wait-seconds) wait_secs="$2" ;; --work-wait-seconds) work_wait="$2" ;; --port) port="$2" ;;
        --cboinc) cboinc="$2" ;; --app-binary) app="$2" ;;
        *) usage ;;
    esac
    shift 2
done
[ -n "$work" ] && [ -n "$clients" ] && [ -n "$tags" ] || { usage; }
WORK="$work" CLIENTS="$clients"
. "$(dirname "$0")/lib.sh"
export PATH="/opt/node/bin:$PATH"
res="$WORK/results"
mkdir -p "$res"
cloud="$HERE/../.."
failed=0
d1_raw() { # d1_raw SQL: wrangler's JSON for a query on the local database, or a failure
    (cd "$cloud/workers/scheduler" && npx wrangler d1 execute cloud-boinc-dev --local \
        --persist-to "$WORK/cloud-state/wrangler" --json --command "$1" 2>/dev/null)
}
d1() { # d1 SQL: rows of the local database as tab-separated text
    _out="$(d1_raw "$1")" || die "the database query failed: $1"
    printf '%s' "$_out" | python3 -I -c '
import json, sys
for part in json.load(sys.stdin):
    rows = part.get("results", [])
    if rows:
        cols = list(rows[0])
        print("\t".join(cols))
        for r in rows:
            print("\t".join(str(r[c]) for c in cols))
'
}
d1_n() { # d1_n SQL: sets N to the first value of the first row
    _out="$(d1_raw "$1")" || die "the database query failed: $1"
    N="$(printf '%s' "$_out" | python3 -I -c 'import json,sys; r=json.load(sys.stdin)[0]["results"][0]; print(list(r.values())[0])')" ||
        die "could not read the answer to: $1"
}
held() { # held TAG PATTERN: sets N to how many tasks matching PATTERN the client holds
    load_client "$1"
    _tasks="$(cc --get_tasks)" || die "boinccmd --get_tasks failed for $1"
    N="$(printf '%s\n' "$_tasks" | grep -cE "^   name: $2" || true)"
}

log "waiting for the long tasks"
i=0
while :; do
    left=0
    for t in $tags; do held "$t" "${t}_[cd]_0"; left=$((left + N)); done
    [ "$left" -eq 0 ] && break
    i=$((i + 15))
    [ "$i" -le "$wait_secs" ] || { log "TIMEOUT: $left long tasks still held after $wait_secs seconds"; failed=1; break; }
    /bin/sleep 15
done
log "long tasks left: $left"

# shellcheck disable=SC2086
sh "$HERE/post-work.sh" --work "$WORK" --port "$port" ${cboinc:+--cboinc "$cboinc"} ${app:+--app-binary "$app"}
nudge() { for t in $tags; do load_client "$t"; cc --project "$MASTER_URL" update > /dev/null; done; }
nudge
d1_n "SELECT count(*) FROM result WHERE name LIKE 'post%'"
total="$N"
[ "$total" -gt 0 ] || die "post-work.sh made no results"
# The project refuses work to a computer that asks again within `min_sendwork_interval` of its
# last request ("Last request too recent"), and the refused request counts as the last one, so
# the clients are asked to update only every 45 seconds, more than the interval plus the delay.
i=0
while :; do
    /bin/sleep 15
    d1_n "SELECT count(*) FROM result WHERE name LIKE 'post%' AND received_time > 0"
    got="$N"
    [ "$got" -eq "$total" ] && break
    i=$((i + 15))
    [ "$i" -le "$work_wait" ] || { log "TIMEOUT: $got of $total results of the new work received after $work_wait seconds"; failed=1; break; }
    [ $((i % 45)) -ne 0 ] || nudge
done
log "results of the new work received: $got of $total"

log "detach and attach again"
for t in $tags; do
    load_client "$t"
    cc --project "$MASTER_URL" detach > /dev/null
done
/bin/sleep 3
for t in $tags; do
    load_client "$t"
    cc --project_attach "$MASTER_URL" "$(secret "$C_USER" authenticator)" > /dev/null
done
/bin/sleep 30

log "web RPCs"
sh "$HERE/web-probe.sh" --work "$WORK" --base "http://127.0.0.1:$port/$PROJECT/" > "$res/probe-cloud.txt" 2>&1
{
    echo "== host"; d1 "SELECT id, userid, rpc_seqno, substr(host_cpid, 1, 6) AS cpid, domain_name FROM host ORDER BY id"
    echo "== result (not the new work)"
    d1 "SELECT r.name, r.server_state, r.outcome, r.validate_state, r.hostid, r.exit_status, w.error_mask FROM result r JOIN workunit w ON w.id = r.workunitid WHERE r.name NOT LIKE 'post%' ORDER BY r.id"
    echo "== new work by outcome"
    d1 "SELECT server_state, outcome, validate_state, count(*) AS n FROM result WHERE name LIKE 'post%' GROUP BY 1, 2, 3"
} > "$res/cloud-tables.txt"
for t in $tags; do
    sh "$HERE/show-client.sh" --work "$WORK" --tag "$t" > "$res/$t.log"
    python3 -I "$HERE/novel-lines.py" --log "$WORK/clients/$t/client.log" \
        --switch-line "$(cat "$WORK/clients/$t/log-lines-at-switch")" > "$res/$t.novel"
done
log "results are in $res"
[ "$failed" -eq 0 ] || exit 1
