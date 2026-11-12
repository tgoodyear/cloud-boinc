#!/bin/sh
# What the clients do after the switch, and what the web RPCs answer. Run after switch.sh.
#   observe.sh --work DIR --clients FILE --tags "TAG ..." [--wait-seconds N] [--port N]
# 1. waits (up to --wait-seconds, default 1500) until no client holds a task of the long
#    jobs (P_c, P_d), that is, until each has finished, uploaded and reported it or has
#    been told to abort it;
# 2. adds new work (post-work.sh) and asks every client for it, then waits until the clients
#    have run it;
# 3. detaches every client and attaches it again with its account key;
# 4. writes DIR/results/: the log of each client since the switch (<tag>.log), the lines in it
#    that the client had not logged before (<tag>.novel), the web
#    RPCs' replies (probe-cloud.txt) and the project's tables (cloud-tables.txt).
usage() { sed -n '2,/^usage()/p' "$0" | sed '$d' >&2; exit 2; }
set -eu
work="" clients="" tags="" wait=1500 port=80
while [ $# -gt 0 ]; do
    case "$1" in
        --work) work="$2" ;; --clients) clients="$2" ;; --tags) tags="$2" ;;
        --wait-seconds) wait="$2" ;; --port) port="$2" ;;
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
cb() { CBOINC_URL="http://127.0.0.1:$port" CBOINC_TOKEN_FILE="$WORK/cloud-state/token" "$cloud/target/release/cboinc" "$@"; }
d1() { # d1 SQL: rows of the local database as tab-separated text
    (cd "$cloud/workers/scheduler" && npx wrangler d1 execute cloud-boinc-dev --local \
        --persist-to "$WORK/cloud-state/wrangler" --json --command "$1" 2>/dev/null) |
        python3 -I -c '
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
long_tasks() { # long_tasks TAG: how many tasks of the long jobs the client still holds
    load_client "$1"
    cc --get_tasks | grep -cE "^   name: ${1}_[cd]_0" || true
}

log "waiting for the long tasks"
i=0
while :; do
    left=0
    for t in $tags; do left=$((left + $(long_tasks "$t"))); done
    [ "$left" -eq 0 ] && break
    i=$((i + 15))
    [ "$i" -le "$wait" ] || { log "gave up with $left long tasks left"; break; }
    /bin/sleep 15
done
log "long tasks left: $left"

sh "$HERE/post-work.sh" --work "$WORK" --port "$port"
for t in $tags; do load_client "$t"; cc --project "$MASTER_URL" update > /dev/null; done
/bin/sleep 20
i=0
while [ "$i" -lt 300 ]; do
    busy=0
    for t in $tags; do
        load_client "$t"
        busy=$((busy + $(cc --get_tasks | grep -c "^   name: post_" || true)))
    done
    [ "$busy" -eq 0 ] && break
    i=$((i + 15)); /bin/sleep 15
    for t in $tags; do load_client "$t"; cc --project "$MASTER_URL" update > /dev/null; done
done
log "new-work tasks left on the clients: $busy"

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
