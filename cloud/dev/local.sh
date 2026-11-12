#!/bin/sh
# Runs the five Workers behind a router on one local port, with a local D1
# database, bucket and queue kept under the state directory.
#   dev/local.sh --state <dir> [--port <n>] [--skip-build] [--stock-short-name <name>]
# The first run creates throwaway keys, an admin token and a password pepper
# in <dir> and writes them to each Worker's .dev.vars. Needs cargo,
# worker-build and Node.js. The scheduled sweep is not run by wrangler dev;
# trigger it with: curl http://127.0.0.1:<port>/__sweep
set -eu
cd "$(dirname "$0")/.."
state=""
port=8787
build=1
stock=""
while [ $# -gt 0 ]; do
    case "$1" in
        --state) state="$2"; shift 2 ;;
        --port) port="$2"; shift 2 ;;
        --skip-build) build=0; shift ;;
        --stock-short-name) stock="$2"; shift 2 ;;
        *) echo "usage: dev/local.sh --state <dir> [--port <n>] [--skip-build] [--stock-short-name <name>]" >&2; exit 2 ;;
    esac
done
[ -n "$state" ] || { echo "dev/local.sh: --state is required" >&2; exit 2; }
mkdir -p "$state/keys"
state="$(cd "$state" && pwd)"

umask 077
if [ ! -f "$state/keys/upload_private" ]; then
    cargo run --quiet --release -p cboinc -- keygen \
        --private "$state/keys/code_sign_private" --public "$state/keys/code_sign_public"
    cargo run --quiet --release -p cboinc -- keygen \
        --private "$state/keys/upload_private" --public "$state/keys/upload_public"
fi
[ -f "$state/token" ] || od -An -N24 -tx1 /dev/urandom | tr -d ' \n' > "$state/token"
[ -f "$state/pepper" ] || od -An -N24 -tx1 /dev/urandom | tr -d ' \n' > "$state/pepper"

# Each Worker gets only the secrets it reads.
printf 'PASSWD_PEPPER="%s"\n' "$(cat "$state/pepper")" > workers/web/.dev.vars
printf 'UPLOAD_PRIVATE_KEY="%s"\n' "$(cat "$state/keys/upload_private")" > workers/pipeline/.dev.vars
{
    printf 'ADMIN_TOKENS="%s"\n' "$(cat "$state/token")"
    printf 'PASSWD_PEPPER="%s"\n' "$(cat "$state/pepper")"
    printf 'UPLOAD_PRIVATE_KEY="%s"\n' "$(cat "$state/keys/upload_private")"
} > workers/admin/.dev.vars
: > workers/scheduler/.dev.vars
: > workers/files/.dev.vars
umask 022

[ "$build" = 0 ] || dev/build.sh
(cd workers/scheduler && npx wrangler d1 migrations apply cloud-boinc-dev --local \
    --persist-to "$state/wrangler")
# The router sends the stock layout's paths to the Workers when asked to; the
# config's stock_short_name setting is what makes the Workers answer them.
exec npx wrangler dev \
    ${stock:+--var "STOCK_SHORT_NAME:$stock"} \
    -c dev/router/wrangler.toml \
    -c workers/scheduler/wrangler.toml \
    -c workers/files/wrangler.toml \
    -c workers/web/wrangler.toml \
    -c workers/admin/wrangler.toml \
    -c workers/pipeline/wrangler.toml \
    --persist-to "$state/wrangler" --port "$port" --ip 127.0.0.1
