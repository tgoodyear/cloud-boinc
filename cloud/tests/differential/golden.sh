#!/bin/bash
# Regenerates the golden replies of the corpus from the stock server.
#
#   golden.sh [--corpus DIR] [--state DIR] [--port N] [--image NAME] [--scenarios LIST]
#
# Starts the stock server (stock.sh), and for each scenario of the corpus
# resets the project to its starting state and replays the scenario's
# requests with replay.py, which writes <step>.reply and golden.json. Stops
# the server at the end. The requests themselves are not changed; see
# capture.sh and build-corpus.py for how they were made.
set -eu
here="$(cd "$(dirname "$0")" && pwd)"
corpus="$here/corpus" state="" port=18480 image=cloud-boinc-stock scenarios=""
while [ $# -gt 0 ]; do
    [ $# -ge 2 ] || { sed -n '2,12p' "$0" >&2; exit 2; }
    case "$1" in
        --corpus) corpus="$2" ;;
        --state) state="$2" ;;
        --port) port="$2" ;;
        --image) image="$2" ;;
        --scenarios) scenarios="$2" ;;
        *) sed -n '2,12p' "$0" >&2; exit 2 ;;
    esac
    shift 2
done
trap '"$here/stock.sh" --action stop >/dev/null 2>&1 || true' EXIT
args=(--action start --image "$image" --port "$port")
[ -z "$state" ] || args+=(--state "$state")
"$here/stock.sh" "${args[@]}" >/dev/null
if [ -z "$scenarios" ]; then
    scenarios="$(cd "$corpus" && for d in */steps.json; do dirname "$d"; done | tr '\n' ' ')"
fi
for s in $scenarios; do
    echo "== $s"
    "$here/stock.sh" --action reset >/dev/null
    python3 "$here/replay.py" --base "http://127.0.0.1:$port" --scenario "$corpus/$s"
done
