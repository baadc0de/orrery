#!/usr/bin/env bash
# Manage the local single-node FoundationDB dev cluster (slice 3 test harness).
#
#   ./scripts/fdb-dev.sh start     start fdbserver (idempotent)
#   ./scripts/fdb-dev.sh stop      stop fdbserver
#   ./scripts/fdb-dev.sh status    show cluster status
#   ./scripts/fdb-dev.sh reset     stop, wipe data, re-init a fresh single-node cluster
#
# The cluster file is .fdb-dev/fdb.cluster. This is a dev harness only — not
# the deployment posture (the ADR tracks k8s-operator / systemd for prod).
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CLUSTER="$ROOT/.fdb-dev/fdb.cluster"
DATA="$ROOT/.fdb-dev/data"
LOGS="$ROOT/.fdb-dev/logs"
ADDR="127.0.0.1:4500"
PIDFILE="$ROOT/.fdb-dev/fdbserver.pid"

start() {
  if pgrep -f "fdbserver.*${ADDR}" >/dev/null 2>&1; then
    echo "fdbserver already running on $ADDR"
    return 0
  fi
  mkdir -p "$DATA" "$LOGS"
  printf 'dev:test@%s\n' "$ADDR" > "$CLUSTER"
  nohup /usr/bin/fdbserver \
    --cluster_file "$CLUSTER" \
    --public_address "$ADDR" --listen_address "$ADDR" \
    --datadir "$DATA" --logdir "$LOGS" \
    --memory 1GiB --cache_memory 256MiB \
    > "$ROOT/.fdb-dev/fdbserver.out" 2>&1 &
  echo "$!" > "$PIDFILE"
  for _ in $(seq 1 20); do
    if fdbcli -C "$CLUSTER" --exec "status minimal" 2>/dev/null | grep -q "available"; then
      echo "fdbserver started (pid $(cat "$PIDFILE")). Database available."
      return 0
    fi
    sleep 0.5
  done
  echo "WARNING: fdbserver started but the database is not yet available;"
  echo "run: fdbcli -C $CLUSTER --exec 'configure new single memory'"
}

stop() {
  if [[ -f "$PIDFILE" ]]; then
    kill "$(cat "$PIDFILE")" 2>/dev/null || true
    rm -f "$PIDFILE"
  fi
  pkill -f "fdbserver.*${ADDR}" 2>/dev/null || true
  echo "stopped"
}

status() {
  fdbcli -C "$CLUSTER" --exec "status" 2>&1 | head -20
}

reset() {
  stop
  rm -rf "$DATA" "$LOGS"
  mkdir -p "$DATA" "$LOGS"
  rm -f "$CLUSTER"
  start
  sleep 1
  fdbcli -C "$CLUSTER" --exec "configure new single memory"
  echo "reset complete"
}

case "${1:-}" in
  start) start ;;
  stop) stop ;;
  status) status ;;
  reset) reset ;;
  *) echo "usage: $0 {start|stop|status|reset}"; exit 1 ;;
esac
