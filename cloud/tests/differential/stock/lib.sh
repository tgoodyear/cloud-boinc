# Shared by serve.sh and reset.sh. Expects STATE, PORT and PUBLIC_PORT.
PROJECT_NAME="${PROJECT_NAME:-diff}"
DB_NAME="$PROJECT_NAME"
PROJECT_DIR="$STATE/project"
SOCKET=/run/mysqld/mysqld.sock
APACHE_DIR="$STATE/apache"

as_boincadm() {
    su boincadm -s /bin/bash -c "export USER=boincadm HOME=/home/boincadm; $*"
}

start_mariadb() {
    mkdir -p /run/mysqld "$STATE/mysql"
    chown mysql:mysql /run/mysqld "$STATE/mysql"
    if [ ! -d "$STATE/mysql/mysql" ]; then
        mariadb-install-db --user=mysql --datadir="$STATE/mysql" \
            --auth-root-authentication-method=socket >/dev/null
    fi
    mariadbd --user=mysql --datadir="$STATE/mysql" --socket="$SOCKET" --skip-networking \
        --innodb-buffer-pool-size=64M --performance-schema=OFF --max-connections=40 \
        >"$STATE/mariadb.log" 2>&1 &
    echo $! > "$STATE/mariadb.pid"
    for _ in $(seq 1 60); do
        mariadb-admin --socket="$SOCKET" ping >/dev/null 2>&1 && break
        sleep 1
    done
    mariadb-admin --socket="$SOCKET" ping >/dev/null
    mariadb --socket="$SOCKET" -e "CREATE USER IF NOT EXISTS 'boincadm'@'localhost'; \
        GRANT ALL ON *.* TO 'boincadm'@'localhost';"
}

setup_project() {
    local base="${URL_BASE:-http://127.0.0.1:$PUBLIC_PORT/}"
    rm -rf "$PROJECT_DIR"
    (cd /opt/boinc/tools && as_boincadm "./make_project --no_query --delete_prev_inst \
        --drop_db_first --project_root '$PROJECT_DIR' --url_base '$base' \
        --db_name $DB_NAME --db_user boincadm --user_name boincadm \
        --project_host localhost $PROJECT_NAME 'Differential test project'") \
        >"$STATE/make_project.log" 2>&1 || { cat "$STATE/make_project.log" >&2; exit 1; }
    as_boincadm "python3 /opt/differential/load_fixture.py --project '$PROJECT_DIR' \
        --fixtures /fixtures --socket '$SOCKET' --db $DB_NAME"
    # The state every replay starts from; reset.sh loads it again.
    mariadb-dump --socket="$SOCKET" --databases $DB_NAME > "$STATE/snapshot.sql"
}

write_apache() {
    rm -rf "$APACHE_DIR"
    mkdir -p "$APACHE_DIR/run" "$STATE/apache-log"
    cp -a /etc/apache2/. "$APACHE_DIR/"
    chown -R boincadm:boincadm "$STATE/apache-log" "$APACHE_DIR/run"
    echo "Listen 127.0.0.1:$PORT" > "$APACHE_DIR/ports.conf"
    rm -f "$APACHE_DIR"/sites-enabled/*
    cp "$PROJECT_DIR/$PROJECT_NAME.httpd.conf" "$APACHE_DIR/sites-enabled/project.conf"
    cat >> "$APACHE_DIR/apache2.conf" <<CONF
ServerName 127.0.0.1
User boincadm
Group boincadm
PidFile $APACHE_DIR/run/apache2.pid
ErrorLog $STATE/apache-log/error.log
LogLevel warn
CustomLog $STATE/apache-log/access.log combined
DocumentRoot $PROJECT_DIR/html/user
<Directory $PROJECT_DIR/html/user>
    Require all granted
</Directory>
CONF
    # One server, a handful of workers: the host is shared.
    cat > "$APACHE_DIR/mods-enabled/mpm_prefork.conf" <<CONF
<IfModule mpm_prefork_module>
    StartServers 2
    MinSpareServers 1
    MaxSpareServers 3
    MaxRequestWorkers 8
</IfModule>
CONF
}

start_apache() {
    write_apache
    # apache2.conf names its own User, Group and PidFile, but envvars sets
    # the variables the stock file refers to.
    APACHE_CONFDIR="$APACHE_DIR" APACHE_RUN_USER=boincadm APACHE_RUN_GROUP=boincadm \
        APACHE_PID_FILE="$APACHE_DIR/run/apache2.pid" APACHE_RUN_DIR="$APACHE_DIR/run" \
        APACHE_LOG_DIR="$STATE/apache-log" APACHE_LOCK_DIR="$APACHE_DIR/run" \
        apache2 -d "$APACHE_DIR" -f "$APACHE_DIR/apache2.conf"
    for _ in $(seq 1 30); do
        curl -s -o /dev/null "http://127.0.0.1:$PORT/$PROJECT_NAME/" && return
        sleep 1
    done
    echo "apache did not start; see $STATE/apache-log/error.log" >&2
    exit 1
}

start_feeder() {
    # The scheduler reads the jobs the feeder keeps in shared memory. The
    # subshell execs the feeder, so the recorded PID is the feeder's own.
    as_boincadm "( cd '$PROJECT_DIR' && rm -f reread_db && exec nohup bin/feeder -d 3 \
        >> log_localhost/feeder.out 2>&1 ) & echo \$! > '$STATE/feeder.pid'"
    for _ in $(seq 1 60); do
        as_boincadm "cd '$PROJECT_DIR' && bin/show_shmem 2>/dev/null | grep -q '^ready: 1'" && return 0
        sleep 1
    done
    echo "the feeder did not fill the shared memory; see $PROJECT_DIR/log_localhost/feeder.out" >&2
    return 1
}

stop_feeder() {
    if [ -f "$STATE/feeder.pid" ]; then
        pid="$(cat "$STATE/feeder.pid")"
        kill "$pid" 2>/dev/null || true
        # Wait for it to end, so that no feeder is left to fill a segment.
        for _ in $(seq 1 20); do
            kill -0 "$pid" 2>/dev/null || break
            sleep 0.5
        done
        kill -9 "$pid" 2>/dev/null || true
        rm -f "$STATE/feeder.pid"
    fi
    # Remove the shared memory segment the feeder leaves behind.
    ipcs -m | awk 'NR>3 && $1 ~ /^0x/ {print $2}' | while read -r id; do ipcrm -m "$id" 2>/dev/null || true; done
}

stop_apache() {
    [ -f "$APACHE_DIR/run/apache2.pid" ] && kill "$(cat "$APACHE_DIR/run/apache2.pid")" 2>/dev/null || true
}

stop_all() {
    stop_apache
    stop_feeder
    [ -f "$STATE/mariadb.pid" ] && kill "$(cat "$STATE/mariadb.pid")" 2>/dev/null || true
    sleep 1
    exit 0
}
