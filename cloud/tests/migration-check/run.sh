#!/bin/sh
# The whole migration check.
#   run.sh --work DIR --clients FILE --image NAME --cboinc FILE --app-binary FILE
#          [--steps "up drive release probe switch observe down"] [--long-seconds N]
#          [--cloud-config TAG=VALUE]... [--no-uploads]
# The project's URLs name no port, so both servers listen on port 80 and this script takes no
# --port (the scripts it calls have one for use on their own). --cboinc and --app-binary
# are used by `up`, `switch` and `observe`. Any failed step stops the run.
# See README.md. The steps, which can be run one at a time (--steps) in this order:
#   up       the stock server and its project (stock-up.sh)
#   drive    each client of FILE is attached and brought to the state it is left in at the
#            switch (drive-client.sh), one after the other
#   release  the long tasks of each client are let to run (release-tasks.sh)
#   probe    the web RPCs of the stock server are asked (web-probe.sh)
#   switch   the stock server stops and cloud-boinc takes its place (switch.sh): every
#            client's host row is copied except the clients of mode `nohost`; job rows only
#            for the clients of mode `copied` or `nohost`; the files of the stock upload
#            directory unless --no-uploads is given
#   observe  what the clients do next (observe.sh)
#   down     the clients, cloud-boinc and the stock server are stopped
usage() { sed -n '2,/^usage()/p' "$0" | sed '$d' >&2; exit 2; }
set -eu
work="" clients="" image="" cboinc="" app="" steps="up drive release probe switch observe down"
long=240 cfg="" uploads="--carry-uploads"
while [ $# -gt 0 ]; do
    case "$1" in
        --work) work="$2" ;; --clients) clients="$2" ;; --image) image="$2" ;;
        --cboinc) cboinc="$2" ;; --app-binary) app="$2" ;; --steps) steps="$2" ;;
        --long-seconds) long="$2" ;;
        --cloud-config) cfg="$cfg --config-set $2" ;;
        --no-uploads) uploads=""; shift; continue ;;
        *) usage ;;
    esac
    shift 2
done
[ -n "$work" ] && [ -n "$clients" ] || { usage; }
WORK="$work" CLIENTS="$clients"
. "$(dirname "$0")/lib.sh"
export PATH="/opt/node/bin:$PATH"
tags="$(awk '!/^#/ && NF {printf "%s ", $1}' "$CLIENTS")"
mkdir -p "$WORK/results"
has() { case " $steps " in *" $1 "*) return 0 ;; esac; return 1; }
if has up || has switch || has observe; then
    [ -n "$cboinc" ] && [ -n "$app" ] || die "--cboinc and --app-binary are needed by up, switch and observe"
    [ -x "$cboinc" ] || die "--cboinc $cboinc is not an executable file"
    [ -f "$app" ] || die "--app-binary $app is not a file"
fi
# A step that fails stops the run, with its output kept. `--steps down` cleans up.
if has up; then
    [ -n "$image" ] || die "up needs --image"
    sh "$HERE/stock-up.sh" --work "$WORK" --image "$image" --cboinc "$cboinc" --app-binary "$app" ||
        die "up failed"
fi
if has drive; then
    for t in $tags; do
        redacted sh "$HERE/drive-client.sh" --work "$WORK" --clients "$CLIENTS" --tag "$t" \
            --job-prefix "$t" --long-seconds "$long" || die "drive-client.sh failed for $t"
    done
fi
if has release; then
    for t in $tags; do
        redacted sh "$HERE/release-tasks.sh" --work "$WORK" --clients "$CLIENTS" --tag "$t" \
            --job-prefix "$t" || die "release-tasks.sh failed for $t"
    done
    stock_sql "SELECT id, userid, rpc_seqno, substr(host_cpid, 1, 6) AS cpid, domain_name FROM host; SELECT name, server_state, outcome, validate_state, hostid FROM result ORDER BY id" > "$WORK/results/stock-before.txt" ||
        die "reading the stock tables failed"
fi
if has probe; then
    sh "$HERE/web-probe.sh" --work "$WORK" --base "http://127.0.0.1/$PROJECT/" > "$WORK/results/probe-stock.txt" 2>&1 ||
        die "web-probe.sh failed on the stock server"
fi
if has switch; then
    args=""
    for t in $tags; do
        load_client "$t"
        case "$C_MODE" in
            copied) args="$args --copy-jobs ${t}_%" ;;
            nohost)
                args="$args --copy-jobs ${t}_%"
                id="$(stock_sql "SELECT id FROM host WHERE domain_name = 'cb-mig-$t'" | tail -1)" ||
                    die "reading the host ID of $t failed"
                case "$id" in ''|*[!0-9]*) die "no host ID for $t in the stock database ($id)" ;; esac
                args="$args --skip-host $id" ;;
            notcopied) ;;
            *) die "$t has no mode" ;;
        esac
    done
    # shellcheck disable=SC2086
    sh "$HERE/switch.sh" --work "$WORK" --clients "$CLIENTS" --tags "$tags" --cboinc "$cboinc" \
        $uploads $args $cfg || die "switch.sh failed"
fi
if has observe; then
    sh "$HERE/observe.sh" --work "$WORK" --clients "$CLIENTS" --tags "$tags" --cboinc "$cboinc" \
        --app-binary "$app" || die "observe.sh failed (its results, if any, are in $WORK/results)"
fi
if has down; then
    touch "$WORK/captures/stop" 2>/dev/null || true
    for t in $tags; do
        load_client "$t"
        sh "$HERE/client.sh" --action stop --dir "$C_DIR" --boinccmd "$C_BOINCCMD" --lib "$C_LIB" > /dev/null || true
    done
    sh "$HERE/cloud-down.sh" --work "$WORK"
    sh "$HERE/../differential/stock.sh" --action stop --name "$STOCK_NAME" || true
fi
true
