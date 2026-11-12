#!/bin/sh
# Builds and runs the stock BOINC server for the differential test.
#
#   stock.sh --action build [--source DIR] [--image NAME] [--jobs N]
#   stock.sh --action start [--image NAME] [--name NAME] [--port N] [--public-port N]
#                           [--state DIR]
#   stock.sh --action reset [--name NAME]
#   stock.sh --action stop  [--name NAME]
#   stock.sh --action logs  [--name NAME]
#
# build   Stages the upstream tree (everything of --source outside cloud/ and
#         .git) as a build context and builds the image from Containerfile.
#         --source defaults to the root of this repository. About 10 minutes.
# start   Runs the image with host networking: Apache on 127.0.0.1:--port
#         (default 18480). MariaDB listens on a socket inside the container
#         only. The project, its database and its keys live in --state, a
#         directory made for the purpose (default: a new one under $TMPDIR);
#         it is printed on stdout. Returns when the server answers.
#         --public-port is the port the project's URLs name, when a proxy
#         sits in front of the server (see record.sh); default --port.
# reset   Puts the project back into the starting state of fixtures/ and
#         restarts the feeder, so the next replay begins from the same rows.
# stop    Stops and removes the container.
#
# Works with podman or docker (CONTAINER_ENGINE names one). Everything it
# starts is stopped by `--action stop`.
set -eu
here="$(cd "$(dirname "$0")" && pwd)"
action="" source="$here/../../.." image="cloud-boinc-stock" name="cloud-boinc-stock"
port=18480 public_port="" state="" jobs=2
while [ $# -gt 0 ]; do
    [ $# -ge 2 ] || { sed -n "2,30p" "$0" >&2; exit 2; }
    case "$1" in
        --action) action="$2" ;;
        --source) source="$2" ;;
        --image) image="$2" ;;
        --name) name="$2" ;;
        --port) port="$2" ;;
        --public-port) public_port="$2" ;;
        --state) state="$2" ;;
        --jobs) jobs="$2" ;;
        *) sed -n "2,30p" "$0" >&2; exit 2 ;;
    esac
    shift 2
done
engine="${CONTAINER_ENGINE:-}"
if [ -z "$engine" ]; then
    if command -v podman >/dev/null 2>&1; then engine=podman
    elif command -v docker >/dev/null 2>&1; then engine=docker
    else echo "stock.sh: neither podman nor docker is installed" >&2; exit 1; fi
fi

case "$action" in
build)
    ctx="$(mktemp -d)"
    trap 'rm -rf "$ctx"' EXIT
    mkdir "$ctx/upstream"
    rsync -a --exclude .git --exclude /cloud --exclude /3rdParty --exclude /android \
        --exclude /mac_build --exclude /win_build --exclude /installer --exclude /fastlane \
        "$source/" "$ctx/upstream/"
    cp -r "$here/stock" "$ctx/stock"
    cp "$here/Containerfile" "$ctx/Containerfile"
    "$engine" build --network host --build-arg "BUILD_JOBS=$jobs" -t "$image" -f "$ctx/Containerfile" "$ctx"
    ;;
start)
    [ -n "$state" ] || state="$(mktemp -d "${TMPDIR:-/tmp}/cloud-boinc-stock.XXXXXX")"
    mkdir -p "$state"
    state="$(cd "$state" && pwd)"
    rm -f "$state/serving"
    "$engine" rm -f "$name" >/dev/null 2>&1 || true
    "$engine" run -d --name "$name" --network host \
        -v "$state:/state" -v "$here/project:/fixtures:ro" \
        -v "$here/stock:/opt/differential:ro" \
        "$image" --port "$port" --public-port "${public_port:-$port}" >/dev/null
    i=0
    while [ ! -f "$state/serving" ]; do
        i=$((i + 1))
        if [ "$i" -gt 300 ] || ! "$engine" ps -q --filter "name=^$name\$" | grep -q .; then
            "$engine" logs "$name" >&2 || true
            echo "stock.sh: the server did not start" >&2
            exit 1
        fi
        sleep 1
    done
    echo "$state"
    ;;
reset)
    "$engine" exec "$name" /opt/differential/reset.sh
    ;;
stop)
    "$engine" stop -t 20 "$name" >/dev/null 2>&1 || true
    "$engine" rm -f "$name" >/dev/null 2>&1 || true
    ;;
logs)
    "$engine" logs "$name"
    ;;
*)
    echo "stock.sh: unknown --action '$action'" >&2
    sed -n "2,30p" "$0" >&2
    exit 2
    ;;
esac
