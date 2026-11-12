#!/bin/sh
# Starts the stock server of the check with a project made for it.
#   stock-up.sh --work DIR --image NAME --cboinc FILE --app-binary FILE [--port N]
# Writes the fixtures (new key pairs and accounts) to DIR/fixtures, starts the
# stock server container (tests/differential/stock.sh) on 127.0.0.1:--port
# (default 80) as project `proj` whose master URL is http://project.test/proj/,
# starts the daemons the stock tree has (transitioner, file_deleter, the sample
# trivial validator and the sample assimilator of the application), and
# stores the second account's password hash in the old form (the bare MD5).
usage() { sed -n '2,/^usage()/p' "$0" | sed '$d' >&2; exit 2; }
set -eu
work="" image="" cboinc="" app="" port=80
while [ $# -gt 0 ]; do
    case "$1" in
        --work) work="$2" ;; --image) image="$2" ;; --cboinc) cboinc="$2" ;;
        --app-binary) app="$2" ;; --port) port="$2" ;;
        *) usage ;;
    esac
    shift 2
done
[ -n "$work" ] && [ -n "$image" ] && [ -n "$cboinc" ] && [ -n "$app" ] || { usage; }
WORK="$work"
. "$(dirname "$0")/lib.sh"
mkdir -p "$WORK"
rm -rf "$WORK/fixtures" "$WORK/stock-state"
python3 "$HERE/prepare.py" --out "$WORK/fixtures" --cboinc "$cboinc" --app-binary "$app" --project "$PROJECT" > /dev/null
nice -n 10 sh "$HERE/../differential/stock.sh" --action start --image "$image" --name "$STOCK_NAME" \
    --port "$port" --project "$PROJECT" --url-base "http://$PROJECT_HOST/" \
    --fixtures "$WORK/fixtures" --state "$WORK/stock-state" > /dev/null
stock_exec sh -c 'rm -f reread_db'
for d in "transitioner -d 3" "file_deleter -d 3" "sample_trivial_validator -d 3 --app upper" \
        "sample_dummy_assimilator -d 3 --app upper"; do
    n="${d%% *}"
    podman exec -d -u boincadm -e USER=boincadm -e HOME=/home/boincadm -w /state/project "$STOCK_NAME" \
        sh -c "exec nice -n 10 bin/$d >> log_localhost/$n.out 2>&1" > /dev/null
done
# The second account's hash in the old form: the client-side MD5 itself.
stock_sql "UPDATE user SET passwd_hash = md5(concat('$(secret 2 password)', lower(email_addr))) WHERE id = 2"
log "stock server up at $MASTER_URL"
