#!/bin/sh
# The switch: the stock server is stopped and cloud-boinc answers at the same URLs.
#   switch.sh --work DIR --clients FILE --tags "TAG ..." [--copy-jobs PATTERN]...
#             [--skip-host ID]... [--carry-uploads] [--config-set TAG=VALUE]... [--cboinc FILE]
#             [--port N]
# 1. every client in --tags is let to run its long tasks (release-tasks.sh is not called
#    here; the clients are expected to be in the state drive-client.sh leaves them in);
# 2. the stock rows are copied (dump-rows.sh), with the jobs whose workunit names match
#    each --copy-jobs pattern (SQL LIKE) and without the hosts given by --skip-host, and a dump
#    of the whole stock database is kept in DIR/stock-final.sql;
# 3. the stock server is stopped, cloud-boinc is started on --port (default 80), given the
#    stock project's settings, keys and files and then the rows (cloud-load.sh);
# 4. watch-requests.sh starts copying every scheduler exchange of the clients, and
#    the clients' network is turned on again.
usage() { sed -n '2,/^usage()/p' "$0" | sed '$d' >&2; exit 2; }
set -eu
work="" clients="" tags="" port=80 likes="" load="" cboinc=""
while [ $# -gt 0 ]; do
    case "$1" in
        --work) work="$2" ;; --clients) clients="$2" ;; --tags) tags="$2" ;; --port) port="$2" ;;
        --copy-jobs) likes="$likes --wu-like $2" ;;
        --skip-host) likes="$likes --skip-host $2" ;;
        --config-set) load="$load --config-set $2" ;;
        --cboinc) cboinc="$2" ;;
        --carry-uploads) load="$load --carry-uploads"; shift; continue ;;
        *) usage ;;
    esac
    shift 2
done
[ -n "$work" ] && [ -n "$clients" ] && [ -n "$tags" ] || { usage; }
WORK="$work" CLIENTS="$clients"
. "$(dirname "$0")/lib.sh"

log "copy the stock rows"
# shellcheck disable=SC2086
sh "$HERE/dump-rows.sh" --work "$WORK" $likes
podman exec "$STOCK_NAME" mariadb-dump --socket=/run/mysqld/mysqld.sock --databases "$PROJECT" > "$WORK/stock-final.sql"
for t in $tags; do
    load_client "$t"
    wc -l < "$(client_log)" > "$C_DIR/log-lines-at-switch"
done

log "stop the stock server, start cloud-boinc"
sh "$HERE/../differential/stock.sh" --action stop --name "$STOCK_NAME"
# From here the stock server is down. If anything fails, the clients' network stays off.
offline() { log "$1; the clients' network stays off and the stock server is stopped"; exit 1; }
sh "$HERE/cloud-up.sh" --work "$WORK" --port "$port" || offline "cloud-boinc did not start"
# shellcheck disable=SC2086
sh "$HERE/cloud-load.sh" --work "$WORK" --port "$port" $load ${cboinc:+--cboinc "$cboinc"} ||
    offline "loading the stock project's settings, files and rows failed"

log "clients on again"
nohup sh "$HERE/watch-requests.sh" --work "$WORK" --tags "$tags" > /dev/null 2>&1 < /dev/null &
echo $! > "$WORK/watch.pid"
/bin/sleep 1
for t in $tags; do
    load_client "$t"
    cc --set_network_mode auto > /dev/null
done
log "switched"
