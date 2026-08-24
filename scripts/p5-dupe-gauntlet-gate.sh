#!/usr/bin/env bash
# P5's single-gateway dupe gauntlet (issue #151): replay, forged and
# self-chosen attestations, and the quarantined full-validation ordering pin.
#
# The harness is two OS processes. The first assembles the real GatewayServer,
# enforcing BaselineIntentValidator, witness-epoch cache and FDB executor. The
# second speaks the live iroh wire, then reads the durable ledger, intent,
# attestation and receipt rows back from the same FoundationDB cluster.
#
# No PASSED marker is written unless all three arms and the honest attested
# control agree in report.json. This gate consumes fixed ledger ids and must be
# pointed at a fresh throwaway cluster.
set -euo pipefail

readonly NAME=p5-dupe-gauntlet-gate
die() { echo "$NAME: $*" >&2; exit 2; }
note() { echo "$NAME: $*" >&2; }

if [[ ${1:-} == --self-test ]]; then
  # Search only the executable body below `:`, with comments removed. Every
  # literal also appears in the checks themselves, so searching the whole file
  # would make each clause match its own source and pass vacuously.
  body="$(sed -n '/^: /,$p' "$0" | grep -v '^[[:space:]]*#')"
  launch() { # occurrence
    awk -v want="$1" '
      BEGIN { bin = "\"$P5_DUPE_BIN\" " "\\" }
      $0 == bin { n++; inside = (n == want); if (inside) { print; next } }
      inside { print; if ($0 !~ /\\$/) inside = 0 }
    ' <<<"$body"
  }
  runs() { # occurrence literal
    local invocation
    invocation=$(launch "$1")
    [[ -n $invocation ]] || die "self-test: P5_DUPE_BIN invocation $1 is absent"
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

  runs 1 'gateway' \
    || die 'self-test: no separate enforcing gateway process is launched'
  runs 2 '--replay' \
    || die 'self-test: replay arm is not requested from the harness'
  runs 2 '--attestation' \
    || die 'self-test: forged/self-chosen attestation arm is not requested'
  runs 2 '--quarantine' \
    || die 'self-test: quarantine arm is not requested'
  runs 2 '--cluster-file "$ORRERY_FDB_CLUSTER_FILE"' \
    || die 'self-test: harness cannot read its durable FDB evidence'
  runs 2 '--audit-log "$out/gateway.log"' \
    || die 'self-test: attributable refusal causes are not handed to the harness'
  runs 2 '--report "$out/report.json"' \
    || die 'self-test: harness writes no machine-readable arm report'
  grep -Fq 'harness_status -ne 0' <<<"$body" \
    || die 'self-test: harness exit status is not load-bearing'
  grep -Fq 'jq -e '\''.result == "pass"'\'' "$out/report.json"' <<<"$body" \
    || die 'self-test: report verdict is not checked before success'
  grep -Fq 'touch "$out/PASSED"' <<<"$body" \
    || die 'self-test: no final success artifact exists'
  echo 'self-test: separate gateway, replay, attestation, quarantine, durable audit and final verdict stages present'
  exit 0
fi

: "${ORRERY_FDB_CLUSTER_FILE:?set ORRERY_FDB_CLUSTER_FILE to a fresh throwaway FoundationDB cluster}"
: "${P5_DUPE_BIN:?set P5_DUPE_BIN to the gates/p5-dupe-gauntlet binary}"
[[ ${P5_DUPE_CLUSTER_IS_THROWAWAY:-0} == 1 ]] \
  || die 'set P5_DUPE_CLUSTER_IS_THROWAWAY=1 to assert this cluster may receive fixed gauntlet rows'
[[ -r $ORRERY_FDB_CLUSTER_FILE ]] \
  || die "FDB cluster file is not readable: $ORRERY_FDB_CLUSTER_FILE"
[[ -x $P5_DUPE_BIN ]] || die "not an executable: $P5_DUPE_BIN"
command -v fdbcli >/dev/null || die 'fdbcli is not on PATH'
timeout 20 fdbcli -C "$ORRERY_FDB_CLUSTER_FILE" --exec 'status minimal' 2>/dev/null \
  | grep -q 'is available' \
  || die "FoundationDB cluster is not available: $ORRERY_FDB_CLUSTER_FILE"

out=${P5_DUPE_GATE_OUT:-"$(pwd)/p5-dupe-gauntlet-$(date -u +%Y%m%dT%H%M%SZ)"}
[[ ! -e $out ]] || die "refusing to overwrite existing output directory: $out"
mkdir -p "$out" "$out/gateway-data"

GATEWAY_PID=''
cleanup() {
  if [[ -n $GATEWAY_PID ]] && kill -0 "$GATEWAY_PID" 2>/dev/null; then
    kill -INT "$GATEWAY_PID" 2>/dev/null || true
    wait "$GATEWAY_PID" 2>/dev/null || true
  fi
}
trap cleanup EXIT

note 'starting a separate K-of-N-enforcing gateway process'
"$P5_DUPE_BIN" \
  gateway \
  --cluster-file "$ORRERY_FDB_CLUSTER_FILE" \
  --data-dir "$out/gateway-data" \
  >"$out/gateway.json" 2>"$out/gateway.log" &
GATEWAY_PID=$!

for _ in $(seq 1 300); do
  [[ -s $out/gateway.json ]] && break
  kill -0 "$GATEWAY_PID" 2>/dev/null \
    || die "gateway exited before readiness; see $out/gateway.log"
  sleep 0.1
done
[[ -s $out/gateway.json ]] || die "gateway did not become ready; see $out/gateway.log"
GATEWAY_NODE=$(jq -er '.node_id' "$out/gateway.json") \
  || die 'gateway readiness has no node_id'
GATEWAY_ADDR=$(jq -er '.bind_addr' "$out/gateway.json") \
  || die 'gateway readiness has no bind_addr'
[[ $(jq -r '.enforcement' "$out/gateway.json") == required ]] \
  || die 'gateway readiness does not attest required K-of-N enforcement'

set +e
"$P5_DUPE_BIN" \
  run \
  --gateway-node "$GATEWAY_NODE" \
  --gateway-addr "$GATEWAY_ADDR" \
  --cluster-file "$ORRERY_FDB_CLUSTER_FILE" \
  --audit-log "$out/gateway.log" \
  --report "$out/report.json" \
  --replay \
  --attestation \
  --quarantine \
  >"$out/harness.log" 2>&1
harness_status=$?
set -e

[[ -r $out/report.json ]] || die "harness wrote no report; see $out/harness.log"
jq . "$out/report.json"
if [[ $harness_status -ne 0 ]]; then
  die "dupe gauntlet FAILED; see $out/report.json and $out/harness.log"
fi
jq -e '.result == "pass"' "$out/report.json" >/dev/null \
  || die 'harness exited zero without a passing report'

touch "$out/PASSED"
note "all three arms held; evidence in $out/report.json"
