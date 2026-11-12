#!/bin/sh
# Copies each scheduler request and reply the clients write into DIR/captures/TAG as
# NNN_request.xml and NNN_reply.xml, until DIR/captures/stop exists.
#   watch-requests.sh --work DIR --tags "TAG TAG..."
# The client overwrites sched_request_<host>_<project>.xml with each request and
# truncates the reply file when the next starts, so this polls every 0.1 second. The
# files hold the account key and the host CPID: they stay in DIR and are never published.
usage() { sed -n '2,/^usage()/p' "$0" | sed '$d' >&2; exit 2; }
set -eu
work="" tags=""
while [ $# -gt 0 ]; do
    case "$1" in
        --work) work="$2" ;; --tags) tags="$2" ;;
        *) usage ;;
    esac
    shift 2
done
[ -n "$work" ] && [ -n "$tags" ] || { usage; }
for t in $tags; do mkdir -p "$work/captures/$t"; done
rm -f "$work/captures/stop"
while [ ! -f "$work/captures/stop" ]; do
    for t in $tags; do
        d="$work/clients/$t"; c="$work/captures/$t"
        f="$d/sched_request_project.test_proj.xml"
        if [ -f "$f" ]; then
            m="$(stat -c %Y.%y.%s "$f")"
            if [ "$m" != "$(cat "$c/.last_request" 2>/dev/null)" ]; then
                n=$(find "$c" -name "*_request.xml" | wc -l); n=$((n + 1))
                cp "$f" "$c/$(printf %03d "$n")_request.xml"
                date -u +%FT%T > "$c/$(printf %03d "$n").time"
                echo "$m" > "$c/.last_request"
            fi
        fi
        f="$d/sched_reply_project.test_proj.xml"
        if [ -s "$f" ]; then
            m="$(stat -c %Y.%y.%s "$f")"
            if [ "$m" != "$(cat "$c/.last_reply" 2>/dev/null)" ]; then
                /bin/sleep 0.05
                n=$(find "$c" -name "*_request.xml" | wc -l)
                cp "$f" "$c/$(printf %03d "$n")_reply.xml"
                echo "$m" > "$c/.last_reply"
            fi
        fi
    done
    /bin/sleep 0.1
done
