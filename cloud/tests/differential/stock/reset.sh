#!/bin/bash
# Puts the running project back into its starting state: the database as
# snapshotted after setup, no uploaded files, a fresh feeder.
set -eu
STATE="${STATE:-/state}"
. /opt/differential/lib.sh
stop_feeder
mariadb --socket="$SOCKET" < "$STATE/snapshot.sql"
find "$PROJECT_DIR/upload" -type f -delete
rm -rf "$PROJECT_DIR"/cache/* "$PROJECT_DIR"/log_localhost/*.log 2>/dev/null || true
start_feeder
echo reset
