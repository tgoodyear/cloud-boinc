#!/bin/sh
# Gives the local cloud-boinc the stock project's settings, keys and files, and then its rows.
#   cloud-load.sh --work DIR [--port N] [--cboinc FILE] [--skip-rows] [--carry-uploads]
#                 [--config-set TAG=VALUE]...
# Reads the stock project's config.xml and keeps its <config> element as it is, except the
# database credentials and host name, and adds stock_short_name. The two public keys and
# every file under the stock download directory (application files and job inputs)
# are stored through the admin API with cboinc. The rows in DIR/rows.sql (dump-rows.sh)
# are applied to the local database with `wrangler d1 execute --local`.
# --carry-uploads stores every file under the stock upload directory (the output files
# tasks have uploaded, in the stock server's hash directories) in the bucket as
# upload/<name>. --config-set changes one tag of the config after it is stored.
usage() { sed -n '2,/^usage()/p' "$0" | sed '$d' >&2; exit 2; }
set -eu
work="" port=80 cboinc="" rows=1 uploads=0 sets=""
while [ $# -gt 0 ]; do
    case "$1" in
        --skip-rows) rows=0; shift; continue ;;
        --carry-uploads) uploads=1; shift; continue ;;
        --config-set) sets="$sets $2" ;;
        --work) work="$2" ;; --port) port="$2" ;; --cboinc) cboinc="$2" ;;
        *) usage ;;
    esac
    shift 2
done
[ -n "$work" ] || { usage; }
WORK="$work"
. "$(dirname "$0")/lib.sh"
cloud="$HERE/../.."
[ -n "$cboinc" ] || cboinc="$cloud/target/release/cboinc"
state="$WORK/cloud-state"
export CBOINC_URL="http://127.0.0.1:$port" CBOINC_TOKEN_FILE="$state/token"
python3 -I - "$WORK/stock-state/project/config.xml" "$WORK/cloud-config.xml" "$PROJECT" <<'PY'
import re, sys
src, dst, short = sys.argv[1:4]
m = re.search(r"<config>.*?</config>", open(src).read(), re.S).group(0)
for tag in ("db_user", "db_name", "db_passwd", "db_host", "host"):
    m = re.sub(r"\s*<%s>.*?</%s>" % (tag, tag), "", m, flags=re.S)
m = m.replace("</config>", "<stock_short_name>%s</stock_short_name>\n</config>" % short)
open(dst, "w").write(m)
PY
"$cboinc" config-put --name config --file "$WORK/cloud-config.xml"
"$cboinc" config-put --name code_sign_public_key --file "$WORK/fixtures/keys/code_sign_public"
"$cboinc" config-put --name upload_public_key --file "$WORK/fixtures/keys/upload_public"
# The stock server caches the MD5 of each file in a .md5 file beside it; they are not job files.
list="$(mktemp)"
trap 'rm -f "$list"' EXIT
find "$WORK/stock-state/project/download" -type f ! -name '*.md5' | sort > "$list"
while read -r f; do
    "$cboinc" file-put --file "$f" --name "$(basename "$f")" --no-delete > /dev/null ||
        die "file-put of $f failed"
    echo "file $(basename "$f")"
done < "$list"
for kv in $sets; do
    "$cboinc" config-set --tag "${kv%%=*}" --value "${kv#*=}"
done
if [ "$uploads" = 1 ]; then
    find "$WORK/stock-state/project/upload" -type f | sort > "$list"
    while read -r f; do
        out="$(cd "$cloud/workers/files" && npx wrangler r2 object put "cloud-boinc-dev/upload/$(basename "$f")" \
            --file "$f" --local --persist-to "$state/wrangler" 2>&1)" ||
            { echo "$out" | tail -5 >&2; die "storing $f in the bucket failed"; }
        echo "upload $(basename "$f")"
    done < "$list"
fi
if [ "$rows" = 1 ]; then
    cd "$cloud/workers/scheduler"
    # The exit status is wrangler's own: a pipe into tail would hide a refused file.
    out="$(npx wrangler d1 execute cloud-boinc-dev --local --persist-to "$state/wrangler" \
        --file "$WORK/rows.sql" 2>&1)" || { echo "$out" | tail -8 >&2; die "importing rows.sql failed"; }
    # And a count of rows that arrived, for the tables the check reads.
    for table in user host workunit result; do
        want="$(grep -c "^INSERT INTO $table " "$WORK/rows.sql" || true)"
        out="$(npx wrangler d1 execute cloud-boinc-dev --local --persist-to "$state/wrangler" --json \
            --command "SELECT count(*) AS n FROM $table" 2>&1)" || { echo "$out" | tail -5 >&2; die "counting $table failed"; }
        got="$(printf '%s' "$out" | python3 -I -c 'import json,sys; print(json.load(sys.stdin)[0]["results"][0]["n"])')" ||
            die "could not read the count of $table"
        [ "$got" = "$want" ] || die "$table has $got rows after the import, rows.sql has $want"
        echo "rows $table $got"
    done
fi
