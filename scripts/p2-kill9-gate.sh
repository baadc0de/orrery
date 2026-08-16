#!/usr/bin/env bash
# P2's permanent two-process crash/recovery regression harness.
#
# This is intentionally a *proof harness*, not a convenience restart script:
# no success artifact is written until the promoted follower has been checked
# against every pre-crash acknowledgement, the old owner has failed fenced
# admission, and the four D16 latency series have passed the dashboard gate.
set -euo pipefail

readonly NAME=p2-kill9-gate
die() { echo "$NAME: $*" >&2; exit 2; }
note() { echo "$NAME: $*" >&2; }

if [[ ${1:-} == --self-test ]]; then
  # Offline guard: useful in CI images without FDB or release binaries.  Keep
  # this deliberately structural; it catches accidental regression to the old
  # single-node/incomplete script without pretending to execute a durability
  # test locally.
  grep -Fq 'start_follower' "$0" || die 'self-test: follower startup absent'
  grep -Fq -- '--promote-from' "$0" || die 'self-test: promotion absent'
  grep -Fq 'verify-recovery' "$0" || die 'self-test: recovery verifier absent'
  grep -Fq 'zombie' "$0" || die 'self-test: zombie fence proof absent'
  grep -Fq 'p2-dashboard --gate' "$0" || die 'self-test: latency gate absent'
  echo 'self-test: two-process proof stages present'
  exit 0
fi

: "${ORRERY_FDB_CLUSTER_FILE:?set ORRERY_FDB_CLUSTER_FILE}"
: "${PERSISTD_BIN:?set PERSISTD_BIN to an fdb-enabled persistd binary}"
: "${P2_LOAD_BIN:?set P2_LOAD_BIN to the p2-load binary}"
: "${P2_DASHBOARD_BIN:?set P2_DASHBOARD_BIN to the p2-dashboard binary}"
[[ -r $ORRERY_FDB_CLUSTER_FILE ]] || die "FDB cluster file is not readable: $ORRERY_FDB_CLUSTER_FILE"
for tool in "$PERSISTD_BIN" "$P2_LOAD_BIN" "$P2_DASHBOARD_BIN"; do
  [[ -x $tool ]] || die "not an executable: $tool"
done

out=${P2_GATE_OUT:-"$(pwd)/p2-kill9-$(date -u +%Y%m%dT%H%M%SZ)"}
[[ ! -e $out ]] || die "refusing to overwrite existing output directory: $out"
mkdir -p "$out" "$out/primary-data" "$out/follower-data"
# `SIGKILL` can land before a reporter's first tick.  Pre-create the files so
# the merge step remains deterministic; the dashboard still rejects the run
# if this leaves `journal_commit_ms` without samples.
: >"$out/primary-metrics.jsonl"
: >"$out/follower-metrics.jsonl"
: >"$out/promoted-metrics.jsonl"
: >"$out/zombie-metrics.jsonl"

# Use explicit, non-overlapping ports to make logs and a failed rerun easy to
# diagnose.  The defaults are only for a dedicated local P2 runner.
gateway_port=${P2_GATE_PORT:-7777}
chain_port=${P2_GATE_CHAIN_PORT:-7778}
zombie_port=${P2_GATE_ZOMBIE_PORT:-7779}
[[ $gateway_port =~ ^[0-9]+$ && $chain_port =~ ^[0-9]+$ && $zombie_port =~ ^[0-9]+$ ]] || die 'ports must be numeric'
secret_primary=${P2_GATE_PRIMARY_SECRET_KEY:-000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f}
secret_follower=${P2_GATE_FOLLOWER_SECRET_KEY:-101112131415161718191a1b1c1d1e1f000102030405060708090a0b0c0d0e0f}
duration=${P2_GATE_DURATION_SECS:-30}
entities=${P2_GATE_ENTITIES:-10000}
cells=${P2_GATE_CELLS:-128}
sessions=${P2_GATE_SESSIONS:-125}

primary_pid=''
follower_pid=''
zombie_pid=''
cleanup() {
  for pid in "$zombie_pid" "$primary_pid" "$follower_pid"; do
    [[ -n $pid ]] && kill "$pid" 2>/dev/null || true
  done
}
trap cleanup EXIT

wait_json() {
  local file=$1 pid=$2 label=$3
  for _ in $(seq 1 1200); do
    [[ -s $file ]] && return 0
    kill -0 "$pid" 2>/dev/null || die "$label exited before readiness; see ${file%.json}.stderr"
    sleep .1
  done
  die "timed out waiting for $label readiness; see ${file%.json}.stderr"
}
json_field() {
  python3 - "$1" "$2" <<'PY'
import json,sys
with open(sys.argv[1], encoding='utf-8') as f:
    value=json.loads(f.readline())
field=sys.argv[2]
if field not in value or value[field] in (None, ''):
    raise SystemExit(f'missing startup field {field!r}')
print(value[field])
PY
}

start_follower() {
  "$PERSISTD_BIN" --node-id 2 --chain-epoch 1 --chain-primary 1 \
    --chain-listen "127.0.0.1:$chain_port" --dir "$out/follower-data" \
    --metrics-jsonl "$out/follower-metrics.jsonl" \
    >"$out/follower.json" 2>"$out/follower.stderr" & follower_pid=$!
  wait_json "$out/follower.json" "$follower_pid" follower
  follower_chain=$(json_field "$out/follower.json" chain_addr)
}
start_primary() {
  "$PERSISTD_BIN" --node-id 1 --chain-epoch 1 --chain-follower "2@$follower_chain" \
    --bind "127.0.0.1:$gateway_port" --dir "$out/primary-data" \
    --secret-key "$secret_primary" --fdb-cluster-file "$ORRERY_FDB_CLUSTER_FILE" \
    --metrics-jsonl "$out/primary-metrics.jsonl" >"$out/primary.json" 2>"$out/primary.stderr" & primary_pid=$!
  wait_json "$out/primary.json" "$primary_pid" primary
  primary_gateway=$(json_field "$out/primary.json" node_id)
  primary_addr=$(json_field "$out/primary.json" bind_addr)
}
start_promoted_follower() {
  # The follower process was passive and is deliberately stopped before
  # promotion: the promoted instance adopts the same on-disk mirror.
  kill -TERM "$follower_pid"; wait "$follower_pid" || true; follower_pid=''
  "$PERSISTD_BIN" --node-id 2 --chain-epoch 2 --chain-primary 1 --promote-from 1 \
    --chain-listen "127.0.0.1:$chain_port" --bind "127.0.0.1:$gateway_port" \
    --dir "$out/follower-data" --secret-key "$secret_follower" \
    --fdb-cluster-file "$ORRERY_FDB_CLUSTER_FILE" --metrics-jsonl "$out/promoted-metrics.jsonl" \
    >"$out/promoted.json" 2>"$out/promoted.stderr" & follower_pid=$!
  wait_json "$out/promoted.json" "$follower_pid" promoted-follower
  promoted_gateway=$(json_field "$out/promoted.json" node_id)
  promoted_addr=$(json_field "$out/promoted.json" bind_addr)
  recovery_cutoff=$(json_field "$out/promoted.json" recovery_cutoff)
}

note "starting passive follower"
start_follower
note "starting fenced primary"
start_primary
note "driving ${entities} entities across ${cells} cells"
"$P2_LOAD_BIN" --gateway "$primary_gateway" --addr "$primary_addr" \
  --entities "$entities" --cells "$cells" --sessions "$sessions" --duration-secs "$duration" \
  --json --ack-log "$out/acks.jsonl" >"$out/load-before.jsonl" 2>"$out/load-before.stderr"
[[ -s $out/acks.jsonl ]] || die 'load completed without durable acknowledgement evidence'

note 'SIGKILL primary and promote follower'
kill -KILL "$primary_pid"; wait "$primary_pid" 2>/dev/null || true; primary_pid=''
start_promoted_follower

# The verifier reads materialized bulk state through the promoted gateway and
# intent idempotency rows directly from FDB.  Its cutoff binds comparison to
# the chain prefix actually adopted during promotion, so a post-cutoff ack is
# never silently demanded from an asynchronous mirror.
"$P2_LOAD_BIN" --verify-recovery --ack-log "$out/acks.jsonl" \
  --gateway "$promoted_gateway" --addr "$promoted_addr" \
  --fdb-cluster-file "$ORRERY_FDB_CLUSTER_FILE" --recovery-cutoff "$recovery_cutoff" \
  --output "$out/recovery-verification.json"

# A stale owner must fail admission before it can open a gateway.  This is a
# stronger check than merely observing the old PID dead: it proves the FDB
# actor fence rejects a fresh process carrying the old owner identity.
note 'proving old primary is fenced (zombie admission)'
"$PERSISTD_BIN" --node-id 1 --chain-epoch 1 --chain-follower "2@$follower_chain" \
  --bind "127.0.0.1:$zombie_port" --dir "$out/primary-data" \
  --secret-key "$secret_primary" --fdb-cluster-file "$ORRERY_FDB_CLUSTER_FILE" \
  --metrics-jsonl "$out/zombie-metrics.jsonl" >"$out/zombie.json" 2>"$out/zombie.stderr" & zombie_pid=$!
if wait "$zombie_pid"; then
  zombie_pid=''
  die 'zombie primary unexpectedly passed startup admission'
fi
zombie_pid=''
grep -Eqi 'fence|owner|activation|epoch' "$out/zombie.stderr" || die 'zombie failed, but not with recognizable fence admission evidence'

# Keep all raw telemetry, then gate the merged evidence in one invocation.
cat "$out/load-before.jsonl" "$out/primary-metrics.jsonl" "$out/promoted-metrics.jsonl" >"$out/telemetry.jsonl"
"$P2_DASHBOARD_BIN" --gate --json "$out/telemetry.jsonl" >"$out/latency-report.json"

python3 - "$out/artifact.json" "$out/recovery-verification.json" "$out/latency-report.json" "$recovery_cutoff" <<'PY'
import datetime,json,pathlib,sys
artifact = pathlib.Path(sys.argv[1]); verification = pathlib.Path(sys.argv[2]); latency = pathlib.Path(sys.argv[3]); cutoff = sys.argv[4]
v=json.loads(verification.read_text()); l=json.loads(latency.read_text())
if not v.get('pass', False): raise SystemExit('recovery verifier returned a non-pass report')
if l.get('gate') != 'pass': raise SystemExit('latency dashboard returned a non-pass report')
# The merged artifact is written by persistd and p2-load and read by the
# dashboard, all three off one series-name definition (orrery_protocol::
# metrics). An unrecognized name here means a producer drifted from it, which
# used to show up as samples silently dropped and a clean report.
if l.get('unknown_series', 0):
    raise SystemExit(f"latency artifact carried unrecognized series: {l.get('unknown_series_names')}")
artifact.write_text(json.dumps({
  'kind':'p2_two_process_kill9_gate',
  'created_at':datetime.datetime.now(datetime.timezone.utc).isoformat(),
  'result':'pass', 'recovery_cutoff':cutoff,
  'proofs': {'recovery': v, 'latency': l, 'zombie_primary_fenced': True},
}, indent=2) + '\n')
PY
note "PASS artifact: $out/artifact.json"
