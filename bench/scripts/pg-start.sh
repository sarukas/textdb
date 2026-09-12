#!/usr/bin/env bash
# Start a throwaway PostgreSQL 16 cluster for the benchmark harness.
# Usage: bench/scripts/pg-start.sh [datadir] [port]
# Prints the connection URL. Runs as user "pgbench" when invoked as root.
set -euo pipefail
DATADIR="${1:-/var/tmp/textdb-pg}"
PORT="${2:-54329}"
PGBIN="$(dirname "$(pg_config --bindir)/postgres")"
if [ "$(id -u)" = "0" ]; then
  id pgbench >/dev/null 2>&1 || useradd -m pgbench
  mkdir -p "$DATADIR" && chown pgbench "$DATADIR"
  RUN="runuser -u pgbench --"
else
  RUN=""
fi
if [ ! -f "$DATADIR/PG_VERSION" ]; then
  $RUN "$PGBIN/initdb" -D "$DATADIR" -U postgres --auth=trust -E UTF8 --locale=C >/dev/null
  cat >> "$DATADIR/postgresql.conf" <<CONF
port = $PORT
listen_addresses = 'localhost'
unix_socket_directories = '/tmp'
shared_buffers = 4GB
work_mem = 64MB
max_connections = 1200
synchronous_commit = on
CONF
fi
$RUN "$PGBIN/pg_ctl" -D "$DATADIR" -l "$DATADIR/server.log" -w start >/dev/null
echo "postgres://postgres@localhost:$PORT/postgres"
