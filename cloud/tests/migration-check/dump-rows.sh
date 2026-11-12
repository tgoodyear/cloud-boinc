#!/bin/sh
# Copies the stock project's rows into DIR/rows.sql (see copy_rows.py), from inside the
# running stock container.
#   dump-rows.sh --work DIR [--wu-like PATTERN]... [--skip-host ID]...
# Uses DIR/cloud-state/pepper (made if it is missing; cloud-up.sh uses the same one), so
# that an account with the old form of password hash gets the form cloud-boinc stores.
usage() { sed -n '2,/^usage()/p' "$0" | sed '$d' >&2; exit 2; }
set -eu
work="" likes=""
while [ $# -gt 0 ]; do
    case "$1" in
        --work) work="$2" ;;
        --wu-like) likes="$likes --wu-like $2" ;;
        --skip-host) likes="$likes --skip-host $2" ;;
        *) usage ;;
    esac
    shift 2
done
[ -n "$work" ] || { usage; }
WORK="$work"
. "$(dirname "$0")/lib.sh"
ensure_cloud_secrets
mig="$WORK/stock-state/migrate"
rm -rf "$mig"
mkdir -p "$mig"
cp "$HERE/copy_rows.py" "$mig/"
cp -r "$HERE/../../migrations" "$mig/migrations"
cp "$WORK/cloud-state/pepper" "$mig/pepper"
chmod -R a+rwX "$mig"
chmod 644 "$mig/pepper"
# shellcheck disable=SC2086
stock_exec python3 /state/migrate/copy_rows.py --db "$PROJECT" --socket /run/mysqld/mysqld.sock \
    --migrations /state/migrate/migrations --project-dir /state/project \
    --pepper-file /state/migrate/pepper --out /state/migrate/rows.sql $likes
mv "$mig/rows.sql" "$WORK/rows.sql"
rm -f "$mig/pepper"
