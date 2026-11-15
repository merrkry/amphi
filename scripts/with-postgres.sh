#!/usr/bin/env bash
# Runs a command with DATABASE_URL pointing at a temporary Postgres server,
# which is removed afterwards. Needs initdb and pg_ctl on PATH.
set -euo pipefail

dir=$(mktemp -d)
cleanup() {
  pg_ctl --pgdata "$dir/data" --mode immediate stop >/dev/null 2>&1 || true
  rm -rf "$dir"
}
trap cleanup EXIT

initdb --pgdata "$dir/data" --username postgres --auth trust --encoding UTF8 >/dev/null
pg_ctl --pgdata "$dir/data" --log "$dir/log" --wait \
  --options "-c listen_addresses='' -c unix_socket_directories='$dir'" start >/dev/null
export DATABASE_URL="postgres:///postgres?host=$dir&user=postgres"
"$@"
