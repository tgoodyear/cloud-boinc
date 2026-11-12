#!/bin/sh
# Starts cloud-boinc locally: the five Workers behind the router, with a local
# database, bucket and queue under DIR/cloud-state (dev/local.sh).
#   cloud-up.sh --work DIR [--port N]
# The state directory gets the stock project's key pairs (the code-signing and the upload
# key) before the first start, so that dev/local.sh uses them instead of making new ones.
# The Workers must be built (dev/build.sh). Runs in the background; the log is
# DIR/cloud-state/wrangler.log and the process group's ID is in DIR/cloud-state/pid.
# Needs Node.js and wrangler on the PATH.
usage() { sed -n '2,/^usage()/p' "$0" | sed '$d' >&2; exit 2; }
set -eu
work="" port=80
while [ $# -gt 0 ]; do
    case "$1" in
        --work) work="$2" ;; --port) port="$2" ;;
        *) usage ;;
    esac
    shift 2
done
[ -n "$work" ] || { usage; }
WORK="$work"
. "$(dirname "$0")/lib.sh"
state="$WORK/cloud-state"
ensure_cloud_secrets
mkdir -p "$state/keys"
cp "$WORK/fixtures/keys/"* "$state/keys/"
cloud="$HERE/../.."
cd "$cloud"
nohup setsid nice -n 10 dev/local.sh --state "$state" --port "$port" --skip-build \
    --stock-short-name "$PROJECT" > "$state/wrangler.log" 2>&1 < /dev/null &
echo $! > "$state/pid"
i=0
until grep -q "Ready on" "$state/wrangler.log" 2>/dev/null; do
    i=$((i + 1))
    [ "$i" -le 180 ] || { tail -20 "$state/wrangler.log" >&2; echo "cloud-up.sh: not ready" >&2; exit 1; }
    /bin/sleep 1
done
log "cloud-boinc ready on port $port"
