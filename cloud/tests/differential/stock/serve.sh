#!/bin/bash
# Container entrypoint: sets up the stock project on first start, then runs
# MariaDB, Apache and the feeder until the container is stopped.
#   serve.sh --port N [--public-port N] [--state DIR]
# --port is where Apache listens (127.0.0.1); --public-port is the port the
# project's URLs name, which differs only when a recording proxy sits between
# the clients and Apache.
set -eu
port=18480 public_port="" state=/state
while [ $# -gt 0 ]; do
    case "$1" in
        --port) port="$2" ;;
        --public-port) public_port="$2" ;;
        --state) state="$2" ;;
        *) echo "serve.sh: unknown flag $1" >&2; exit 2 ;;
    esac
    shift 2
done
public_port="${public_port:-$port}"
cd /
export STATE="$state" PORT="$port" PUBLIC_PORT="$public_port"
. /opt/differential/lib.sh

id boincadm >/dev/null 2>&1 || useradd --create-home --uid 1000 boincadm
trap 'stop_all' TERM INT
chown boincadm:boincadm "$STATE"
# stock.sh makes a default state directory with mktemp, mode 0700. MariaDB runs
# as another user, and needs to pass through it to its own directory inside.
chmod 0711 "$STATE"
start_mariadb
if [ ! -f "$STATE/project-ready" ]; then
    setup_project
    touch "$STATE/project-ready"
fi
start_apache
start_feeder
touch "$STATE/serving"
echo "stock server serving on 127.0.0.1:$PORT (URLs name port $PUBLIC_PORT)"
while true; do sleep 3600 & wait $! || true; done
