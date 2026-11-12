#!/bin/bash
# Records what a stock BOINC client sends to the stock server.
#
#   capture.sh --out DIR [--state DIR] [--port N] [--backend-port N] [--client-port N]
#              [--image NAME] [--scenarios LIST]
#
# For each scenario the project is put back into its starting state (see
# fixtures in project/), a client with an empty data directory is started, and
# a recording proxy (record-proxy.py) stands between the client and the
# server. DIR/<scenario>/ receives every exchange: NNNN.request, NNNN.reply
# and exchanges.jsonl. build-corpus.py turns that into the corpus.
#
# Needs a built image (stock.sh --action build), python3, and the stock
# client: the `boinc` and `boinccmd` programs (Debian: boinc-client).
#
# Scenarios (all of them by default):
#   attach     web RPCs the attach wizard uses
#   work       attach, a first work request, more work, uploads, the report of
#              four results, then requests for work when there is none
#   report     attach, no more work, one task, then a report-only request
#   anon       a client with an app_info.xml (anonymous platform), attached by its
#              account file
#   lost       attach, then a request while the first task is still running,
#              so that the request lists it under other_results
set -eu
here="$(cd "$(dirname "$0")" && pwd)"
out="" state="" port=18480 backend=18481 client_port=31650 image=cloud-boinc-stock
scenarios="attach work report anon lost"
while [ $# -gt 0 ]; do
    [ $# -ge 2 ] || { sed -n '2,25p' "$0" >&2; exit 2; }
    case "$1" in
        --out) out="$2" ;;
        --state) state="$2" ;;
        --port) port="$2" ;;
        --backend-port) backend="$2" ;;
        --client-port) client_port="$2" ;;
        --image) image="$2" ;;
        --scenarios) scenarios="$2" ;;
        *) sed -n '2,25p' "$0" >&2; exit 2 ;;
    esac
    shift 2
done
[ -n "$out" ] || { sed -n '2,25p' "$0" >&2; exit 2; }
mkdir -p "$out"
out="$(cd "$out" && pwd)"
work="$(mktemp -d)"
[ -n "$state" ] || state="$work/state"
mkdir -p "$state"
url="http://127.0.0.1:$port/diff/"
auth=5a1f0c3b9d7e42a6b8c0d1e2f3a4b5c6   # user 1 of project/project.json
proxy_pid="" client_pid=""

cleanup() {
    stop_client
    stop_proxy
    "$here/stock.sh" --action stop >/dev/null 2>&1 || true
    rm -rf "$work"
}
trap cleanup EXIT

bc() {
    boinccmd --host "127.0.0.1:$client_port" --passwd "$(cat "$work/client/gui_rpc_auth.cfg")" "$@"
}

start_proxy() {
    python3 "$here/record-proxy.py" --listen-port "$port" --backend-port "$backend" \
        --out "$out/$1" >/dev/null 2>&1 &
    proxy_pid=$!
    for _ in $(seq 1 30); do
        (echo > "/dev/tcp/127.0.0.1/$port") 2>/dev/null && break
        sleep 0.2
    done
}

stop_proxy() {
    [ -z "$proxy_pid" ] || kill "$proxy_pid" 2>/dev/null || true
    proxy_pid=""
}

# A client with an empty data directory. The override file keeps it from
# holding tasks back because the machine is busy.
start_client() {
    rm -rf "$work/client"
    mkdir -p "$work/client"
    cat > "$work/client/cc_config.xml" <<XML
<cc_config><options><allow_multiple_clients>1</allow_multiple_clients><no_gpus>1</no_gpus></options></cc_config>
XML
    cat > "$work/client/global_prefs_override.xml" <<XML
<global_preferences><suspend_cpu_usage>0</suspend_cpu_usage><run_if_user_active>1</run_if_user_active><max_ncpus_pct>100</max_ncpus_pct><work_buf_min_days>0.1</work_buf_min_days><work_buf_additional_days>0.1</work_buf_additional_days></global_preferences>
XML
    if [ -n "${1:-}" ]; then "$1" "$work/client"; fi
    boinc --dir "$work/client" --allow_multiple_clients --gui_rpc_port "$client_port" --no_gpus \
        >"$work/client.log" 2>&1 < /dev/null &
    client_pid=$!
    for _ in $(seq 1 50); do
        [ -s "$work/client/gui_rpc_auth.cfg" ] && bc --get_state >/dev/null 2>&1 && return
        sleep 0.2
    done
    echo "capture.sh: the client did not start" >&2; exit 1
}

stop_client() {
    if [ -n "$client_pid" ]; then
        bc --quit >/dev/null 2>&1 || kill "$client_pid" 2>/dev/null || true
        wait "$client_pid" 2>/dev/null || true
        client_pid=""
    fi
}

count() { grep -c "$1" "$out/$scenario/exchanges.jsonl" 2>/dev/null || echo 0; }

# Waits until the proxy has seen N requests matching the pattern.
await() {
    local pattern="$1" n="$2" i=0
    while [ "$(count "$pattern" | head -n 1)" -lt "$n" ]; do
        i=$((i + 1))
        [ "$i" -le 600 ] || { echo "capture.sh: waited too long for $n x $pattern" >&2; cat "$work/client.log" >&2; exit 1; }
        sleep 0.5
    done
}

# The client reads app_info.xml when it starts, for the projects it already
# has, so the project is made known to it by its account file.
anon_setup() {
    local dir="$1/projects/127.0.0.1_${port}_diff"
    mkdir -p "$dir"
    cp "$here/project/app/upper_1.00_x86_64-pc-linux-gnu" "$dir/"
    cat > "$1/account_127.0.0.1_${port}_diff.xml" <<XML
<account>
    <master_url>$url</master_url>
    <authenticator>$auth</authenticator>
    <project_name>Differential test project</project_name>
</account>
XML
    cat > "$dir/app_info.xml" <<XML
<app_info>
    <app><name>upper</name></app>
    <file_info>
        <name>upper_1.00_x86_64-pc-linux-gnu</name>
        <executable/>
    </file_info>
    <app_version>
        <app_name>upper</app_name>
        <version_num>100</version_num>
        <avg_ncpus>1</avg_ncpus>
        <flops>2e9</flops>
        <file_ref>
            <file_name>upper_1.00_x86_64-pc-linux-gnu</file_name>
            <main_program/>
        </file_ref>
    </app_version>
</app_info>
XML
}

"$here/stock.sh" --action start --image "$image" --port "$backend" --public-port "$port" \
    --state "$state" >/dev/null
for scenario in $scenarios; do
    echo "== $scenario"
    "$here/stock.sh" --action reset >/dev/null
    rm -rf "$out/$scenario"
    mkdir -p "$out/$scenario"
    start_proxy "$scenario"
    case "$scenario" in
    attach)
        start_client
        bc --get_project_config "$url" >/dev/null
        bc --lookup_account "$url" tester@example.org secretpw >/dev/null
        bc --lookup_account "$url" tester@example.org wrongpass >/dev/null
        bc --lookup_account "$url" nobody@example.org secretpw >/dev/null
        bc --create_account "$url" new@example.org newpass1 "New Person" >/dev/null
        bc --create_account "$url" tester@example.org secretpw "Test User" >/dev/null
        bc --create_account "$url" tester@example.org otherpw "Test User" >/dev/null
        bc --create_account "$url" bad-email newpass1 "Bad" >/dev/null
        bc --create_account "$url" short@example.org abc "Short" >/dev/null
        ;;
    work)
        start_client
        bc --project_attach "$url" "$auth" >/dev/null
        await "file_upload_handler" 4
        sleep 3
        bc --project "$url" update
        await "diff_cgi/cgi" 3
        sleep 5
        bc --project "$url" update
        await "diff_cgi/cgi" 4
        ;;
    report)
        start_client
        bc --project_attach "$url" "$auth" >/dev/null
        # The first request gets one task; stop the client asking for more
        # before its second request, a few seconds later.
        await "diff_cgi/cgi" 1
        bc --project "$url" nomorework
        await "file_upload_handler" 1
        sleep 3
        bc --project "$url" update
        await "diff_cgi/cgi" 2
        ;;
    anon)
        start_client anon_setup
        await "file_upload_handler" 4
        sleep 3
        bc --project "$url" update
        await "diff_cgi/cgi" 3
        ;;
    lost)
        start_client
        bc --project_attach "$url" "$auth" >/dev/null
        await "diff_cgi/cgi" 1
        sleep 4
        bc --project "$url" update
        await "diff_cgi/cgi" 2
        ;;
    *)
        echo "capture.sh: unknown scenario $scenario" >&2; exit 2 ;;
    esac
    sleep 2
    stop_client
    stop_proxy
done
echo "captured into $out"
