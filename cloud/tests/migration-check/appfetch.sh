#!/bin/sh
# A client that has never had the application downloads it through the URL the stock
# documents carry. Run after switch.sh, before observe.sh.
#   appfetch.sh --work DIR --clients FILE --tag TAG --cboinc FILE [--port N] [--wait-seconds N]
# A new client of the version and account of TAG's line (data directory DIR/clients/TAGf,
# GUI RPC port 100 above TAG's) is attached with the account's key. The signed documents
# are the stock ones: application version 1.00, whose application file the stock
# `update_versions` put at the top of the download directory, so its URL has no hash
# directory. One job is made for that version only (observe.sh adds 1.01 later), the client
# is asked for work, and the script waits (--wait-seconds, default 300) until the result is
# received. It then checks the client's log: the application file was downloaded from a URL
# with no hash directory, the input file from one with it, and no download failed. The
# client is stopped at the end. The clients of FILE are set to fetch no work while it runs.
# The new client's log is DIR/results/appfetch.log, without account keys.
usage() { sed -n '2,/^usage()/p' "$0" | sed '$d' >&2; exit 2; }
set -eu
work="" clients="" tag="" cboinc="" port=80 wait_secs=300
while [ $# -gt 0 ]; do
    case "$1" in
        --work) work="$2" ;; --clients) clients="$2" ;; --tag) tag="$2" ;;
        --cboinc) cboinc="$2" ;; --port) port="$2" ;; --wait-seconds) wait_secs="$2" ;;
        *) usage ;;
    esac
    shift 2
done
[ -n "$work" ] && [ -n "$clients" ] && [ -n "$tag" ] && [ -n "$cboinc" ] || { usage; }
WORK="$work" CLIENTS="$clients"
. "$(dirname "$0")/lib.sh"
export PATH="/opt/node/bin:$PATH"
cloud="$HERE/../.."
others="$(awk '!/^#/ && NF {printf "%s ", $1}' "$CLIENTS")"
load_client "$tag"
new="${C_TAG}f"
# The new client's identifiers are kept apart from C_*, which load_client overwrites.
T_DIR="$WORK/clients/$new" T_PORT=$((C_PORT + 100)) T_BOINC="$C_BOINC" T_CMD="$C_BOINCCMD" T_LIB="$C_LIB"
T_USER="$C_USER" T_STATE="$WORK/appfetch-client"
[ ! -e "$T_DIR" ] && [ ! -e "$T_STATE" ] || die "$T_DIR or $T_STATE exists: a client of an earlier run is left (run.sh --steps down)"
res="$WORK/results"
mkdir -p "$res"
status=0 logged=0
other_clients() { # other_clients VERB: boinccmd --project URL VERB on each client of FILE
    for _t in $others; do load_client "$_t"; cc --project "$MASTER_URL" "$1" > /dev/null || true; done
}
save_log() { [ "$logged" -eq 1 ] || [ ! -f "$T_DIR/client.log" ] || { redact < "$T_DIR/client.log" > "$res/appfetch.log"; logged=1; }; }
# Runs on every exit, signal included, and keeps the exit status: the other clients fetch work
# again, the new client is stopped and its directory and state file are removed.
cleanup() {
    _rc=$?
    trap - EXIT INT TERM
    set +e
    other_clients allowmorework
    save_log
    sh "$HERE/client.sh" --action stop --dir "$T_DIR" --boinccmd "$T_CMD" --lib "$T_LIB" > /dev/null 2>&1
    rm -rf "$T_DIR" "$T_STATE"
    exit "$_rc"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
# The state file tells `run.sh --steps down` about a client this script did not get to stop.
printf '%s\n%s\n%s\n' "$T_DIR" "$T_CMD" "$T_LIB" > "$T_STATE"
# The other clients of FILE are told not to fetch work meanwhile, so the new client gets the job.
other_clients nomorework
d1_n() { # d1_n SQL: sets N to the first value of the first row
    _out="$(cd "$cloud/workers/scheduler" && npx wrangler d1 execute cloud-boinc-dev --local \
        --persist-to "$WORK/cloud-state/wrangler" --json --command "$1" 2>/dev/null)" ||
        die "the database query failed: $1"
    N="$(printf '%s' "$_out" | python3 -I -c 'import json,sys; r=json.load(sys.stdin)[0]["results"][0]; print(list(r.values())[0])')" ||
        die "could not read the answer to: $1"
}

log "$new: one job for the stock application version, quorum 1"
export CBOINC_URL="http://127.0.0.1:$port" CBOINC_TOKEN_FILE="$WORK/cloud-state/token"
"$cboinc" file-put --file "$WORK/fixtures/inputs/in.txt" --name flat_input.txt --no-delete > /dev/null
"$cboinc" submit --app upper --input-template "$cloud/dev/fixtures/in_template.xml" \
    --result-template "$cloud/dev/fixtures/out_template.xml" --count 1 --quorum 1 \
    --name-prefix flat --input-file flat_input.txt --command-line="--seconds 5" > /dev/null

log "$new: start and attach with the account's key"
sh "$HERE/client.sh" --action start --dir "$T_DIR" --boinc "$T_BOINC" --port "$T_PORT" \
    --name "cb-mig-$new" --lib "$T_LIB" > /dev/null
/bin/sleep 5
C_DIR="$T_DIR" C_PORT="$T_PORT" C_BOINCCMD="$T_CMD" C_LIB="$T_LIB"
cc --project_attach "$MASTER_URL" "$(secret "$T_USER" authenticator)" > /dev/null
i=0
while :; do
    /bin/sleep 5
    d1_n "SELECT count(*) FROM result WHERE name LIKE 'flat%' AND received_time > 0"
    [ "$N" -eq 0 ] || break
    i=$((i + 5))
    [ "$i" -le "$wait_secs" ] || { log "TIMEOUT: no result of the job received after $wait_secs seconds"; status=1; break; }
done
log "results of the job received: $N"

lg="$T_DIR/client.log"
save_log
dl="$(grep -E "Started download of " "$lg" | sed -E 's/.*Started download of //' | sort -u | tr '\n' ' ')"
log "files downloaded: $dl"
app="$(echo "$dl" | tr ' ' '\n' | grep -vE '^(flat_input\.txt|)$' | head -1)"
[ -n "$app" ] || { log "FAIL: no application file was downloaded"; status=1; }
if [ -n "$app" ]; then
    get="$(grep -E "GET /$PROJECT/download/[^ ]*" "$lg" | grep -oE "GET /$PROJECT/download/[^ ]*" | sort -u)"
    echo "$get" | sed 's/^/    /'
    echo "$get" | grep -qxE "GET /$PROJECT/download/$app" ||
        { log "FAIL: no GET of the application file without a hash directory"; status=1; }
    echo "$get" | grep -qE "GET /$PROJECT/download/[0-9a-f]+/flat_input.txt" ||
        { log "FAIL: no GET of the input file with a hash directory"; status=1; }
fi
if grep -qE "Giving up on download|Backing off .* on download|download of .* failed|Download.*error" "$lg"; then
    log "FAIL: a download failed"; status=1
fi
grep -q "Finished download of $app" "$lg" || { log "FAIL: the application download did not finish"; status=1; }
d1_n "SELECT count(*) FROM result WHERE name LIKE 'flat%' AND outcome = 1"
[ "$N" -ge 1 ] || { log "FAIL: the result's outcome is not success"; status=1; }
[ "$status" -eq 0 ] && log "$new: the application was downloaded through its flat URL and the task completed"
exit "$status"
