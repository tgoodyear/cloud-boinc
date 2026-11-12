#!/bin/sh
# Runs a stock BOINC client of one version in its own data directory, with
# its own GUI RPC port, in a UTS and mount namespace (unshare) with a hosts file
# that maps the project's made-up host name to 127.0.0.1 and a host name of
# its own. Needs root, for unshare and the bind mount.
#
#   client.sh --action start  --dir DIR --boinc FILE --port N --name HOSTNAME
#                             [--lib DIRS] [--hosts-name NAME]
#   client.sh --action cmd    --dir DIR --boinccmd FILE [--lib DIRS] -- ARGS...
#   client.sh --action stop   --dir DIR --boinccmd FILE [--lib DIRS]
#
# --lib is a LD_LIBRARY_PATH for a client extracted from a package.
# --hosts-name is the project's host name (default project.test).
# `cmd` prints the boinccmd output with every run of 32 or more hex digits
# replaced, because the output of some commands holds the account key.
# The data directory's cc_config.xml turns on the log flags the check reads
# (task, file_xfer, file_xfer_debug, sched_ops, sched_op_debug, http_debug,
# unparsed_xml) and limits the client to one CPU; the log is DIR/client.log
# and holds the account key (http_debug): it is never to be published.
usage() { sed -n '2,/^usage()/p' "$0" | sed '$d' >&2; exit 2; }
set -eu
action="" dir="" boinc="" boinccmd="" port="" name="" lib="" hosts_name=project.test
while [ $# -gt 0 ]; do
    case "$1" in
        --) shift; break ;;
        --action) action="$2" ;;
        --dir) dir="$2" ;;
        --boinc) boinc="$2" ;;
        --boinccmd) boinccmd="$2" ;;
        --port) port="$2" ;;
        --name) name="$2" ;;
        --lib) lib="$2" ;;
        --hosts-name) hosts_name="$2" ;;
        *) usage ;;
    esac
    shift 2
done
[ -n "$dir" ] || { echo "client.sh: --dir is required" >&2; exit 2; }
redact() { sed -E 's/[0-9a-f]{32,}/<redacted>/g'; }
# redacted CMD ARGS...: as in lib.sh, the output is redacted and the status is the command's.
redacted() {
    _st="$(mktemp)"
    { _rc=0; "$@" 2>&1 || _rc=$?; echo "$_rc" > "$_st"; } | redact
    _rc="$(cat "$_st")"
    rm -f "$_st"
    return "$_rc"
}
case "$action" in
start)
    [ -n "$boinc" ] && [ -n "$port" ] && [ -n "$name" ] || { usage; }
    mkdir -p "$dir"
    dir="$(cd "$dir" && pwd)"
    boinc="$(cd "$(dirname "$boinc")" && pwd)/$(basename "$boinc")"
    if [ ! -f "$dir/cc_config.xml" ]; then
        cat > "$dir/cc_config.xml" <<XML
<cc_config>
 <log_flags>
  <task>1</task><file_xfer>1</file_xfer><file_xfer_debug>1</file_xfer_debug>
  <sched_ops>1</sched_ops><sched_op_debug>1</sched_op_debug>
  <http_debug>1</http_debug><unparsed_xml>1</unparsed_xml>
 </log_flags>
 <options>
  <no_info_fetch>1</no_info_fetch>
  <dont_contact_ref_site>1</dont_contact_ref_site>
  <allow_multiple_clients>1</allow_multiple_clients>
  <ncpus>1</ncpus>
 </options>
</cc_config>
XML
        printf '<global_preferences><suspend_cpu_usage>0</suspend_cpu_usage><run_if_user_active>1</run_if_user_active></global_preferences>\n' \
            > "$dir/global_prefs_override.xml"
    fi
    printf '127.0.0.1 localhost\n127.0.0.1 %s\n127.0.0.1 %s\n' "$name" "$hosts_name" > "$dir/.hosts"
    echo "=== start $(date -u +%FT%TZ) port=$port" >> "$dir/client.log"
    cd "$dir"
    nohup setsid unshare -u -m sh -c "mount --bind '$dir/.hosts' /etc/hosts; hostname '$name'; exec nice -n 10 env LD_LIBRARY_PATH='$lib' '$boinc' --dir '$dir' --allow_multiple_clients --gui_rpc_port '$port' --no_gpus" \
        >> "$dir/client.log" 2>&1 < /dev/null &
    echo $! > "$dir/pid"
    echo "$port" > "$dir/port"
    echo "started pid $(cat "$dir/pid")"
    ;;
cmd)
    [ -n "$boinccmd" ] || { usage; }
    redacted env LD_LIBRARY_PATH="$lib" "$boinccmd" --host "127.0.0.1:$(cat "$dir/port")" \
        --passwd "$(cat "$dir/gui_rpc_auth.cfg")" "$@"
    ;;
stop)
    pid="$(cat "$dir/pid" 2>/dev/null || true)"
    redacted env LD_LIBRARY_PATH="$lib" "$boinccmd" --host "127.0.0.1:$(cat "$dir/port")" \
        --passwd "$(cat "$dir/gui_rpc_auth.cfg")" --quit || true
    i=0
    while [ -n "$pid" ] && kill -0 "$pid" 2>/dev/null && [ $i -lt 100 ]; do
        i=$((i + 1)); /bin/sleep 0.5
    done
    if [ -n "$pid" ] && kill -0 "$pid" 2>/dev/null; then kill "$pid"; fi
    echo stopped
    ;;
*)
    usage ;;
esac
