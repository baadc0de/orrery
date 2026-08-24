#!/usr/bin/env bash
# Paired P5 honest-trade latency measurement (issue #153).
#
# This is deliberately separate from p5-dupe-gauntlet-gate.sh. It reuses the
# same binary's additive `measure` command, but it neither runs inside the live
# nightly gate nor changes any refusal arm or gate report. The control gateway
# runs C1 in shadow so an unattested trade can commit; the paired gateway runs
# C1 required and verifies exactly K pre-built, valid, non-party signatures.
# Consequently this measures gateway-side verification overhead, not end-to-
# end attestation overhead.
set -euo pipefail

readonly NAME=p5-honest-trade-measure
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
die() { echo "$NAME: $*" >&2; exit 2; }
note() { echo "$NAME: $*" >&2; }

if [[ ${1:-} == --self-test ]]; then
  # Search only executable code below the environment guards, with comments
  # removed. Every literal appears above too; whole-file grep would therefore
  # let each check satisfy itself.
  body="$(sed -n '/^: /,$p' "$0" | grep -v '^[[:space:]]*#')"
  launch() { # occurrence
    awk -v want="$1" '
      BEGIN { bin = "\"$P5_MEASURE_BIN\" " "\\" }
      $0 == bin { n++; inside = (n == want); if (inside) { print; next } }
      inside { print; if ($0 !~ /\\$/) inside = 0 }
    ' <<<"$body"
  }
  runs() { # occurrence literal
    local invocation
    invocation=$(launch "$1")
    [[ -n $invocation ]] || die "self-test: binary invocation $1 is absent"
    awk -v want="$2" '
      {
        line = $0
        sub(/^[[:space:]]+/, "", line)
        sub(/[[:space:]]+\\$/, "", line)
        if (line == want) found = 1
      }
      END { exit !found }
    ' <<<"$invocation"
  }

  grep -Fq 'ORRERY_FDB_DEV_PORT="$FDB_PORT"' <<<"$body" \
    || die 'self-test: the measurement does not start a private non-default FDB instance'
  grep -Fq '[[ $FDB_PORT != 4500 ]]' <<<"$body" \
    || die 'self-test: the shared development FDB port is not excluded'
  runs 1 '--enforcement shadow' \
    || die 'self-test: no separate unattested-control gateway is launched'
  runs 2 '--enforcement required' \
    || die 'self-test: no separate K-of-N-enforcing gateway is launched'
  runs 3 'measure' \
    || die 'self-test: the additive measurement path is not invoked'
  runs 3 '--samples "$SAMPLES"' \
    || die 'self-test: the report population is not fixed by sample count'
  runs 3 '--concurrency "$CONCURRENCY"' \
    || die 'self-test: paired concurrency is not handed to the harness'
  runs 3 '--control-stages "$out/control-stages.jsonl"' \
    || die 'self-test: control admission stages are not handed to the harness'
  runs 3 '--attested-stages "$out/attested-stages.jsonl"' \
    || die 'self-test: attested admission stages are not handed to the harness'
  grep -Fq '.measurement_valid == true' <<<"$body" \
    || die 'self-test: report validity is not checked'
  grep -Fq '.populations.control.samples == $n' <<<"$body" \
    || die 'self-test: the control population can be empty or incomplete'
  grep -Fq '.populations.attested.samples == $n' <<<"$body" \
    || die 'self-test: the attested population can be empty or incomplete'
  grep -Fq '.method.cryptographically_verified_attestations == ($n * 3)' <<<"$body" \
    || die 'self-test: K valid signatures per attested sample are not required'
  grep -Fq 'touch "$out/MEASURED"' <<<"$body" \
    || die 'self-test: no final measurement artifact exists'
  echo 'self-test: private FDB, opposed gateways, paired populations, K-signature proof, stage attribution and final report checks present'
  exit 0
fi

: "${SAMPLES:=10000}"
: "${CONCURRENCY:=16}"
command -v jq >/dev/null || die 'jq is not on PATH'
command -v fdbcli >/dev/null || die 'fdbcli is not on PATH'
command -v fdbserver >/dev/null || die 'fdbserver is not on PATH'
command -v python3 >/dev/null || die 'python3 is not on PATH'

out=${P5_MEASURE_OUT:-"$ROOT/p5-honest-trade-$(date -u +%Y%m%dT%H%M%SZ)"}
[[ ! -e $out ]] || die "refusing to overwrite existing output directory: $out"
mkdir -p "$out" "$out/control-data" "$out/attested-data"

FDB_PORT=${P5_MEASURE_FDB_PORT:-$(python3 - <<'PY'
import socket
with socket.socket() as sock:
    sock.bind(("127.0.0.1", 0))
    print(sock.getsockname()[1])
PY
)}
[[ $FDB_PORT != 4500 ]] || die 'port 4500 is the shared development cluster'
FDB_DIR="$out/fdb"
FDB_CLUSTER="$FDB_DIR/fdb.cluster"
P5_MEASURE_BIN=${P5_MEASURE_BIN:-"$ROOT/gates/p5-dupe-gauntlet/target/release/p5-dupe-gauntlet"}

CONTROL_PID=''
ATTESTED_PID=''
FDB_STARTED=0
cleanup() {
  local pid
  for pid in "$CONTROL_PID" "$ATTESTED_PID"; do
    if [[ -n $pid ]] && kill -0 "$pid" 2>/dev/null; then
      kill -INT "$pid" 2>/dev/null || true
      wait "$pid" 2>/dev/null || true
    fi
  done
  if (( FDB_STARTED )); then
    ORRERY_FDB_DEV_PORT="$FDB_PORT" \
    ORRERY_FDB_DEV_DIR="$FDB_DIR" \
      "$ROOT/scripts/fdb-dev.sh" stop >>"$out/fdb-control.log" 2>&1 || true
  fi
}
trap cleanup EXIT

if [[ ! -x $P5_MEASURE_BIN ]]; then
  note 'building the P5 harness in release mode'
  (cd "$ROOT/gates/p5-dupe-gauntlet" && cargo build --release)
fi

note "starting a private FoundationDB instance on 127.0.0.1:$FDB_PORT"
ORRERY_FDB_DEV_PORT="$FDB_PORT" \
ORRERY_FDB_DEV_DIR="$FDB_DIR" \
ORRERY_FDB_DEV_MEMORY=512MiB \
ORRERY_FDB_DEV_CACHE_MEMORY=128MiB \
  "$ROOT/scripts/fdb-dev.sh" start >"$out/fdb-control.log" 2>&1
FDB_STARTED=1
timeout 20 fdbcli -C "$FDB_CLUSTER" --exec 'status minimal' 2>/dev/null \
  | grep -q 'is available' \
  || die "private FoundationDB cluster is unavailable; see $out/fdb-control.log"

export ORRERY_GATEWAY_BOUNDARY_JSONL="$out/control-stages.jsonl"
export ORRERY_INTENT_SLOW_US=10000
note 'starting the unattested control gateway (shadow posture)'
"$P5_MEASURE_BIN" \
  gateway \
  --cluster-file "$FDB_CLUSTER" \
  --data-dir "$out/control-data" \
  --enforcement shadow \
  >"$out/control-gateway.json" 2>"$out/control-gateway.log" &
CONTROL_PID=$!

export ORRERY_GATEWAY_BOUNDARY_JSONL="$out/attested-stages.jsonl"
note 'starting the K-of-N verification gateway (required posture)'
"$P5_MEASURE_BIN" \
  gateway \
  --cluster-file "$FDB_CLUSTER" \
  --data-dir "$out/attested-data" \
  --enforcement required \
  >"$out/attested-gateway.json" 2>"$out/attested-gateway.log" &
ATTESTED_PID=$!
unset ORRERY_GATEWAY_BOUNDARY_JSONL ORRERY_INTENT_SLOW_US

await_ready() { # readiness-file pid label
  local file=$1 pid=$2 label=$3
  for _ in $(seq 1 300); do
    [[ -s $file ]] && return 0
    kill -0 "$pid" 2>/dev/null \
      || die "$label gateway exited before readiness; see $out"
    sleep 0.1
  done
  die "$label gateway did not become ready; see $out"
}
await_ready "$out/control-gateway.json" "$CONTROL_PID" control
await_ready "$out/attested-gateway.json" "$ATTESTED_PID" attested

CONTROL_NODE=$(jq -er '.node_id' "$out/control-gateway.json") \
  || die 'control readiness has no node_id'
CONTROL_ADDR=$(jq -er '.bind_addr' "$out/control-gateway.json") \
  || die 'control readiness has no bind_addr'
ATTESTED_NODE=$(jq -er '.node_id' "$out/attested-gateway.json") \
  || die 'attested readiness has no node_id'
ATTESTED_ADDR=$(jq -er '.bind_addr' "$out/attested-gateway.json") \
  || die 'attested readiness has no bind_addr'
[[ $(jq -r '.enforcement' "$out/control-gateway.json") == shadow ]] \
  || die 'control gateway is not in shadow posture'
[[ $(jq -r '.enforcement' "$out/attested-gateway.json") == required ]] \
  || die 'attested gateway is not enforcing required K-of-N'
[[ $CONTROL_NODE != "$ATTESTED_NODE" ]] \
  || die 'the paired gateway processes published the same identity'

note "measuring $SAMPLES paired commits per population at concurrency $CONCURRENCY"
"$P5_MEASURE_BIN" \
  measure \
  --control-node "$CONTROL_NODE" \
  --control-addr "$CONTROL_ADDR" \
  --attested-node "$ATTESTED_NODE" \
  --attested-addr "$ATTESTED_ADDR" \
  --cluster-file "$FDB_CLUSTER" \
  --control-stages "$out/control-stages.jsonl" \
  --attested-stages "$out/attested-stages.jsonl" \
  --samples "$SAMPLES" \
  --concurrency "$CONCURRENCY" \
  --report "$out/report.json" \
  >"$out/harness.log" 2>&1

[[ -r $out/report.json ]] || die "harness wrote no report; see $out/harness.log"
jq -e --argjson n "$SAMPLES" '
  .measurement_valid == true
  and .method.claim == "verification overhead"
  and .method.attestations == "pre-built"
  and .method.attestations_per_attested_intent == 3
  and .method.cryptographically_verified_attestations == ($n * 3)
  and .method.distinct_non_party_witness_accounts >= 5
  and .populations.control.samples == $n
  and .populations.control.committed == $n
  and .populations.control.durable_receipts == $n
  and .populations.control.gateway_stages.intents == $n
  and .populations.attested.samples == $n
  and .populations.attested.committed == $n
  and .populations.attested.durable_receipts == $n
  and .populations.attested.gateway_stages.intents == $n
' "$out/report.json" >/dev/null \
  || die "measurement validity checks failed; see $out/report.json"

touch "$out/MEASURED"
jq . "$out/report.json"
note "measurement complete; evidence in $out/report.json"
