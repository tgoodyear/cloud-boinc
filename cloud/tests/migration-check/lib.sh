# Shared by the scripts of the migration check. Source it; it expects WORK (the work
# directory) and sets the names the scripts share.
HERE="$(cd "$(dirname "$0")" && pwd)"
PROJECT=proj
PROJECT_HOST=project.test
MASTER_URL="http://$PROJECT_HOST/$PROJECT/"
STOCK_NAME=cb-migration-stock

log() { echo "### $(date -u +%T) $*"; }
redact() { sed -E 's/[0-9a-f]{32,}/<redacted>/g'; }

# secret USERNO FIELD: a value of fixtures/secrets.json. Never printed by the scripts.
secret() {
    python3 -I -c 'import json,sys; print(json.load(open(sys.argv[1]))[sys.argv[2]][sys.argv[3]])' \
        "$WORK/fixtures/secrets.json" "$1" "$2"
}

# stock_exec ARGS...: a command in the stock project's directory, as the project's user.
stock_exec() {
    podman exec -u boincadm -e USER=boincadm -e HOME=/home/boincadm -w /state/project "$STOCK_NAME" "$@"
}
stock_sql() {
    podman exec "$STOCK_NAME" mariadb --socket=/run/mysqld/mysqld.sock "$PROJECT" -e "$1"
}

# Client table: one line per client, `TAG VERSION USERNO BOINC BOINCCMD LIB PORT MODE`
# (LIB is `-` for none; MODE is what the switch does with the client's rows: `copied` for a
# client whose host and job rows are carried, `notcopied` for one whose host row is carried
# and job rows are not, `nohost` for one whose job rows are carried and host row is not).
# The functions below take a line's fields as globals.
load_client() {
    line="$(grep -E "^$1 " "$CLIENTS" || true)"
    [ -n "$line" ] || { echo "no client $1 in $CLIENTS" >&2; return 1; }
    set -- $line
    C_TAG=$1 C_VER=$2 C_USER=$3 C_BOINC=$4 C_BOINCCMD=$5 C_LIB=$6 C_PORT=$7 C_MODE=$8
    [ "$C_LIB" != - ] || C_LIB=""
    C_DIR="$WORK/clients/$C_TAG"
}
cc() { # boinccmd on the current client
    sh "$HERE/client.sh" --action cmd --dir "$C_DIR" --boinccmd "$C_BOINCCMD" --lib "$C_LIB" -- "$@"
}
client_log() { echo "$C_DIR/client.log"; }
# wait_log PATTERN COUNT SECONDS: until the client's log has COUNT lines matching.
wait_log() {
    i=0
    while [ "$(grep -c -- "$1" "$(client_log)" || true)" -lt "$2" ]; do
        i=$((i + 1))
        [ "$i" -le "$3" ] || { echo "timeout waiting for '$1' x$2 in $C_TAG" >&2; return 1; }
        /bin/sleep 1
    done
}

# ensure_cloud_secrets: the local cloud-boinc's password pepper and admin token, made once.
ensure_cloud_secrets() {
    mkdir -p "$WORK/cloud-state"
    [ -f "$WORK/cloud-state/pepper" ] ||
        (umask 077; od -An -N24 -tx1 /dev/urandom | tr -d ' \n' > "$WORK/cloud-state/pepper")
    [ -f "$WORK/cloud-state/token" ] ||
        (umask 077; od -An -N24 -tx1 /dev/urandom | tr -d ' \n' > "$WORK/cloud-state/token")
}
