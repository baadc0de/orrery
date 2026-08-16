#!/usr/bin/env bash
# Manage a local single-node FoundationDB dev cluster (slice 3 test harness).
#
#   ./scripts/fdb-dev.sh start     start fdbserver (idempotent)
#   ./scripts/fdb-dev.sh stop      stop this instance's fdbserver
#   ./scripts/fdb-dev.sh status    show cluster status
#   ./scripts/fdb-dev.sh reset     stop, wipe data, re-init a fresh single-node cluster
#
# The cluster file is $ORRERY_FDB_DEV_DIR/fdb.cluster. This is a dev harness
# only — not the deployment posture (the ADR tracks k8s-operator / systemd for
# prod).
#
# ── Instances ────────────────────────────────────────────────────────────────
#
# Everything that made this harness single-instance is now an environment
# variable, because two things need it: agents working the repository in
# parallel worktrees, and the nightly gates, which were pinned to
# GitHub-hosted runners purely because a hardcoded 127.0.0.1:4500 would have
# collided with the development cluster on the self-hosted box.
#
#   ORRERY_FDB_DEV_PORT           listen/public port           (default 4500)
#   ORRERY_FDB_DEV_DIR            instance directory           (default $ROOT/.fdb-dev)
#   ORRERY_FDB_DEV_DESC           cluster description          (default dev$PORT)
#   ORRERY_FDB_DEV_ID             cluster id                   (default test$PORT)
#   ORRERY_FDB_DEV_MEMORY         --memory                     (default 1GiB)
#   ORRERY_FDB_DEV_CACHE_MEMORY   --cache_memory               (default 256MiB)
#   FDBSERVER                     fdbserver binary             (default: first found on PATH
#                                                               or in the usual sbin/bin dirs)
#
# An instance is identified by its **data directory**, never by its port. That
# distinction is the whole safety property: `stop` and `reset` used to
# `pkill -f "fdbserver.*:4500"`, which on a machine that also runs a system
# fdbserver on 4500 kills the wrong server. They now only ever signal a process
# whose `--datadir` is this instance's.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

PORT="${ORRERY_FDB_DEV_PORT:-4500}"
DIR="${ORRERY_FDB_DEV_DIR:-$ROOT/.fdb-dev}"
DESC="${ORRERY_FDB_DEV_DESC:-dev$PORT}"
ID="${ORRERY_FDB_DEV_ID:-test$PORT}"
MEMORY="${ORRERY_FDB_DEV_MEMORY:-1GiB}"
CACHE_MEMORY="${ORRERY_FDB_DEV_CACHE_MEMORY:-256MiB}"

CLUSTER="$DIR/fdb.cluster"
DATA="$DIR/data"
LOGS="$DIR/logs"
ADDR="127.0.0.1:$PORT"
PIDFILE="$DIR/fdbserver.pid"

# The binary moves between distributions: Debian's package puts it in
# /usr/sbin, some images in /usr/lib/foundationdb, and a tarball install
# wherever it was unpacked. Take an explicit override first so a caller can
# point at a build that is on none of those paths.
resolve_fdbserver() {
  if [[ -n "${FDBSERVER:-}" ]]; then
    printf '%s\n' "$FDBSERVER"
    return 0
  fi
  local candidate
  candidate="$(command -v fdbserver 2>/dev/null || true)"
  if [[ -n "$candidate" ]]; then
    printf '%s\n' "$candidate"
    return 0
  fi
  for candidate in /usr/sbin/fdbserver /usr/bin/fdbserver /usr/lib/foundationdb/fdbserver; do
    if [[ -x "$candidate" ]]; then
      printf '%s\n' "$candidate"
      return 0
    fi
  done
  echo "no fdbserver found; set FDBSERVER to its path" >&2
  return 1
}

# Is this pid a live fdbserver serving *our* data directory? Reading
# /proc/<pid>/cmdline rather than matching `ps` output keeps the predicate
# exact: a pid file left behind by a crash whose number has since been reused
# must not be killed.
is_our_server() {
  local pid="$1" cmdline
  [[ -n "$pid" ]] || return 1
  [[ -r "/proc/$pid/cmdline" ]] || return 1
  cmdline="$(tr '\0' ' ' < "/proc/$pid/cmdline")"
  [[ "$cmdline" == *"fdbserver"* ]] || return 1
  [[ "$cmdline" == *"--datadir $DATA "* || "$cmdline" == *"--datadir $DATA" ]]
}

# The pid file is the primary record; the datadir scan is the fallback for an
# instance whose pid file was deleted. Both agree on the same predicate.
running_pid() {
  local pid
  if [[ -f "$PIDFILE" ]]; then
    pid="$(cat "$PIDFILE" 2>/dev/null || true)"
    if is_our_server "$pid"; then
      printf '%s\n' "$pid"
      return 0
    fi
  fi
  for pid in $(pgrep -f 'fdbserver' 2>/dev/null || true); do
    if is_our_server "$pid"; then
      printf '%s\n' "$pid"
      return 0
    fi
  done
  return 1
}

# Written before the running check, not after it. The old order returned early
# when a server was already up and never reached the write, so a second
# worktree that found *any* fdbserver alive ended up with no cluster file at
# all — and every fdb-gated test silently skipped instead of failing.
#
# An existing file for this address is left alone: the description:id pair has
# to match what the running coordinator recorded, and rewriting it would
# disconnect a cluster someone else provisioned.
write_cluster_file() {
  mkdir -p "$DIR"
  if [[ -s "$CLUSTER" ]] && grep -q "@${ADDR}\$" "$CLUSTER"; then
    return 0
  fi
  printf '%s:%s@%s\n' "$DESC" "$ID" "$ADDR" > "$CLUSTER"
}

# `fdbcli` has no timeout option of its own, and against a coordinator that is
# up but has no database it blocks indefinitely ("WARNING: Long delay") rather
# than reporting. That turns the poll below into a hang, so every call is
# bounded from outside.
fdb_cli() {
  timeout "${ORRERY_FDB_DEV_CLI_TIMEOUT:-10}" fdbcli -C "$CLUSTER" "$@"
}

available() {
  # `is available` and not `available`: `status minimal` answers "The database
  # is unavailable" on an unconfigured cluster, and the substring match this
  # used to do reported that as a healthy start.
  fdb_cli --exec "status minimal" 2>/dev/null | grep -q "is available"
}

start() {
  local server pid fresh=0
  mkdir -p "$DATA" "$LOGS"
  write_cluster_file
  if pid="$(running_pid)"; then
    echo "fdbserver already running for $DATA (pid $pid, $ADDR); cluster file $CLUSTER"
    return 0
  fi
  # An empty data directory is a brand-new instance, and a brand-new instance
  # has no database until one is configured. Remembering that here is what lets
  # `start` stand up a second instance on its own, rather than only `reset`.
  if [[ -z "$(ls -A "$DATA" 2>/dev/null)" ]]; then fresh=1; fi
  server="$(resolve_fdbserver)"
  nohup "$server" \
    --cluster_file "$CLUSTER" \
    --public_address "$ADDR" --listen_address "$ADDR" \
    --datadir "$DATA" --logdir "$LOGS" \
    --memory "$MEMORY" --cache_memory "$CACHE_MEMORY" \
    > "$DIR/fdbserver.out" 2>&1 &
  echo "$!" > "$PIDFILE"

  # A fresh instance is configured before it is polled, not after: a
  # coordinator with no database does not answer `status minimal`, it blocks,
  # so waiting for availability first would burn the whole timeout budget
  # before reaching the one command that can produce it.
  if (( fresh )); then
    echo "new instance at $DATA; configuring a single-node memory database"
    for _ in $(seq 1 10); do
      if fdb_cli --exec "configure new single memory" >/dev/null 2>&1; then
        break
      fi
      sleep 1
    done
  fi

  for _ in $(seq 1 20); do
    if available; then
      echo "fdbserver started (pid $(cat "$PIDFILE")) on $ADDR. Database available."
      return 0
    fi
    sleep 0.5
  done
  echo "WARNING: fdbserver started but the database is not yet available;"
  echo "run: fdbcli -C $CLUSTER --exec 'configure new single memory'"
}

stop() {
  local pid
  if pid="$(running_pid)"; then
    kill "$pid" 2>/dev/null || true
    for _ in $(seq 1 20); do
      is_our_server "$pid" || break
      sleep 0.25
    done
    is_our_server "$pid" && kill -9 "$pid" 2>/dev/null || true
    echo "stopped fdbserver for $DATA (pid $pid)"
  else
    echo "no fdbserver running for $DATA"
  fi
  rm -f "$PIDFILE"
}

status() {
  fdb_cli --exec "status" 2>&1 | head -20
}

reset() {
  stop
  rm -rf "$DATA" "$LOGS"
  mkdir -p "$DATA" "$LOGS"
  rm -f "$CLUSTER"
  # `start` sees an empty data directory and configures the database itself, so
  # there is no second `configure new` here — issuing one against a database
  # that already exists is an error, not a no-op.
  start
  echo "reset complete"
}

case "${1:-}" in
  start) start ;;
  stop) stop ;;
  status) status ;;
  reset) reset ;;
  *) echo "usage: $0 {start|stop|status|reset}"; exit 1 ;;
esac
