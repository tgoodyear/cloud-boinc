#!/bin/sh
# Copies the stock project's rows into DIR/rows.sql with cb-import, in place of dump-rows.sh.
#   import-rows.sh --work DIR --importer FILE [--finished leave|all|recent]
# Opens a SELECT-only account on the stock database and a short-lived TCP forward to its
# socket (the importer connects by TCP), then runs the importer with the stock project
# directory, config.xml, the code-signing public key and the local server's password
# pepper. Writes:
#   DIR/import-sql/*.sql      the D1 files
#   DIR/rows.sql              those files in order, for cloud-load.sh
#   DIR/import-manifest.tsv   the files to copy to the bucket
#   DIR/import-summary.txt    what the importer printed
#   DIR/import-config-set.txt the `cboinc config-set` lines for config.xml
#   DIR/import-verify.txt     the result of loading the files into SQLite and `verify`
# The account, the forward and the DSN file are removed at the end.
usage() { sed -n '2,/^usage()/p' "$0" | sed '$d' >&2; exit 2; }
set -eu
# Everything this script makes holds accounts' keys and hashes: private to this user.
umask 077
work="" importer="" finished=leave
while [ $# -gt 0 ]; do
    case "$1" in
        --work) work="$2" ;; --importer) importer="$2" ;; --finished) finished="$2" ;;
        *) usage ;;
    esac
    shift 2
done
[ -n "$work" ] && [ -n "$importer" ] || { usage; }
WORK="$work"
. "$(dirname "$0")/lib.sh"
ensure_cloud_secrets
importer="$(_abs "$importer")"
port=33406
mig="$WORK/stock-state/migrate"
mkdir -p "$mig"
cp "$HERE/fwd.py" "$mig/fwd.py"
chmod -R a+rwX "$mig"
rm -rf "$WORK/import-sql" "$WORK/import-work"
mkdir -p "$WORK/import-work"
pw="$(od -An -N12 -tx1 /dev/urandom | tr -d ' \n')"
(umask 077; echo "mysql://cbimp_ro:$pw@127.0.0.1:$port/$PROJECT" > "$WORK/import-work/dsn")
fwdpid=""
cleanup() {
    [ -z "$fwdpid" ] || podman exec "$STOCK_NAME" kill "$fwdpid" > /dev/null 2>&1 || true
    stock_sql "DROP USER IF EXISTS 'cbimp_ro'@'localhost'" > /dev/null 2>&1 || true
    rm -f "$WORK/import-work/dsn" "$mig/fwd.py"
}
trap cleanup EXIT
stock_sql "CREATE USER IF NOT EXISTS 'cbimp_ro'@'localhost' IDENTIFIED BY '$pw'; GRANT SELECT ON $PROJECT.* TO 'cbimp_ro'@'localhost'"
# Started inside the container, detached; its PID there is kept so cleanup can stop it.
fwdpid="$(podman exec "$STOCK_NAME" sh -c 'python3 -I /state/migrate/fwd.py --listen-port "$1" --socket /run/mysqld/mysqld.sock --seconds 600 > /dev/null 2>&1 & echo $!' sh "$port")"
case "$fwdpid" in ''|*[!0-9]*) fwdpid=""; die "the forwarder did not start" ;; esac
i=0
until python3 -I -c 'import socket,sys; socket.create_connection(("127.0.0.1", int(sys.argv[1])), 2).close()' "$port" 2> /dev/null; do
    i=$((i + 1))
    [ "$i" -le 20 ] || die "nothing listens on port $port after starting the forwarder"
    /bin/sleep 1
done
proj="$WORK/stock-state/project"
log "import"
"$importer" config --config "$proj/config.xml" > "$WORK/import-config-set.txt"
"$importer" import --source-dsn-file "$WORK/import-work/dsn" --project-dir "$proj" \
    --config "$proj/config.xml" --passwd-pepper-file "$WORK/cloud-state/pepper" \
    --code-sign-public-key "$WORK/fixtures/keys/code_sign_public" --finished "$finished" \
    --target-sql-dir "$WORK/import-sql" --files-manifest "$WORK/import-manifest.tsv" \
    > "$WORK/import-summary.txt" 2>&1 || { redact < "$WORK/import-summary.txt" >&2; die "the import failed"; }
log "verify the files by loading them into SQLite"
rm -f "$WORK/import-check.sqlite"
"$importer" load-sql --sql-dir "$WORK/import-sql" --target-sqlite "$WORK/import-check.sqlite" > "$WORK/import-verify.txt" 2>&1
"$importer" verify --source-dsn-file "$WORK/import-work/dsn" --project-dir "$proj" \
    --config "$proj/config.xml" --passwd-pepper-file "$WORK/cloud-state/pepper" \
    --finished "$finished" --target-sqlite "$WORK/import-check.sqlite" --sample 1000 \
    >> "$WORK/import-verify.txt" 2>&1 || { cat "$WORK/import-verify.txt" >&2; die "verify failed"; }
cat "$WORK"/import-sql/*.sql > "$WORK/rows.sql.tmp"
mv "$WORK/rows.sql.tmp" "$WORK/rows.sql"
log "rows.sql ready"
