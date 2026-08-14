#!/usr/bin/env bash
# Deliberately a single-node restart slice, not the full distributed P2 gate.
set -euo pipefail
die() { echo "p2-kill9-gate: $*" >&2; exit 2; }
if [[ ${1:-} == --self-test ]]; then
  if ORRERY_FDB_CLUSTER_FILE= PERSISTD_BIN=true P2_LOAD_BIN=true "$0" >/dev/null 2>&1; then die "self-test failed"; fi
  echo "self-test: prerequisite guard passed"; exit 0
fi
: "${ORRERY_FDB_CLUSTER_FILE:?set ORRERY_FDB_CLUSTER_FILE}"
: "${PERSISTD_BIN:?set PERSISTD_BIN to an fdb-enabled persistd binary}"
: "${P2_LOAD_BIN:?set P2_LOAD_BIN to the p2-load binary}"
[[ -r $ORRERY_FDB_CLUSTER_FILE && -x $PERSISTD_BIN && -x $P2_LOAD_BIN ]] || die "unreadable FDB file or non-executable binary"
out=${P2_GATE_OUT:-"$(pwd)/p2-kill9-$(date -u +%Y%m%dT%H%M%SZ)"}; data=${P2_GATE_DATA_DIR:-"$out/persistd-data"}
port=${P2_GATE_PORT:-7777}; secret=${P2_GATE_SECRET_KEY:-000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f}
mkdir -p "$out" "$data"; pid=''; trap '[[ -n $pid ]] && kill "$pid" 2>/dev/null || true' EXIT
start() {
  "$PERSISTD_BIN" --nodes 1 --bind "127.0.0.1:$port" --dir "$data" --secret-key "$secret" --fdb-cluster-file "$ORRERY_FDB_CLUSTER_FILE" >"$out/persistd-$1.json" 2>"$out/persistd-$1.stderr" & pid=$!
  for _ in $(seq 1 100); do [[ -s $out/persistd-$1.json ]] && break; kill -0 "$pid" 2>/dev/null || die "persistd exited; see stderr"; sleep .1; done
  gateway=$(python3 -c "import json; print(json.load(open('$out/persistd-$1.json'))['node_id'])")
}
start before
"$P2_LOAD_BIN" --gateway "$gateway" --addr "127.0.0.1:$port" --entities 10000 --cells 128 --sessions 125 --duration-secs "${P2_GATE_DURATION_SECS:-30}" --json --ack-log "$out/acks.jsonl" >"$out/load-before.jsonl" 2>"$out/load-before.stderr"
kill -KILL "$pid"; wait "$pid" 2>/dev/null || true; pid=''; start after
"$P2_LOAD_BIN" --gateway "$gateway" --addr "127.0.0.1:$port" --entities 10000 --cells 128 --sessions 125 --duration-secs 1 --json >"$out/load-after.jsonl" 2>"$out/load-after.stderr"
python3 - "$out/artifact.json" "$out/acks.jsonl" <<'PY'
import datetime,json,pathlib,sys
a=pathlib.Path(sys.argv[2]); json.dump({'kind':'p2_single_node_kill9_slice','created_at':datetime.datetime.now(datetime.timezone.utc).isoformat(),'ack_records':sum(1 for _ in a.open()) if a.exists() else 0,'result':'incomplete','limitations':['No checked-in post-restart state reader compares every acked record.','Append journal_commit_ms from persistd operator metrics before dashboard gating.','Single node only: this does not prove chain replication or failover.']},open(sys.argv[1],'w'),indent=2)
PY
echo "artifact: $out/artifact.json" >&2
die "restart slice completed; full P2 gate remains unavailable (see artifact limitations)"
