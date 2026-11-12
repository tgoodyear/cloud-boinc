#!/bin/sh
# Gives the local cloud-boinc new work, the way a project would after the move: a new
# version of the application (1.01) signed with the stock project's code-signing key, one input
# file, and jobs of that application.
#   post-work.sh --work DIR [--port N] [--jobs N] [--cboinc FILE] [--app-binary FILE]
# The jobs sleep 5 seconds and have the default quorum of two results.
usage() { sed -n '2,/^usage()/p' "$0" | sed '$d' >&2; exit 2; }
set -eu
work="" port=80 jobs=20 cboinc="" app=""
while [ $# -gt 0 ]; do
    case "$1" in
        --work) work="$2" ;; --port) port="$2" ;; --jobs) jobs="$2" ;;
        --cboinc) cboinc="$2" ;; --app-binary) app="$2" ;;
        *) usage ;;
    esac
    shift 2
done
[ -n "$work" ] || { usage; }
WORK="$work"
. "$(dirname "$0")/lib.sh"
cloud="$HERE/../.."
[ -n "$cboinc" ] || cboinc="$cloud/target/release/cboinc"
[ -n "$app" ] || app="$cloud/target/x86_64-unknown-linux-musl/release/cb-test-app"
export CBOINC_URL="http://127.0.0.1:$port" CBOINC_TOKEN_FILE="$WORK/cloud-state/token"
"$cboinc" version-add --app upper --platform x86_64-pc-linux-gnu --version-num 101 \
    --key "$WORK/fixtures/keys/code_sign_private" --main-program "$app" > /dev/null
"$cboinc" file-put --file "$WORK/fixtures/inputs/in.txt" --name post_input.txt --no-delete > /dev/null
"$cboinc" submit --app upper --input-template "$cloud/dev/fixtures/in_template.xml" \
    --result-template "$cloud/dev/fixtures/out_template.xml" --count "$jobs" --name-prefix post \
    --input-file post_input.txt --command-line="--seconds 5" > /dev/null
log "app version 1.01 and $jobs jobs added"
