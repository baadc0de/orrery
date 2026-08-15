#!/usr/bin/env bash
# P3's permanent authority regression harness (docs/11-roadmap.md §P3).
#
# The phase's demo criterion: an 8-peer island with contested entities,
# `kill -9` one peer holding ~50 of them, and every entity must be reassigned
# or parked within the 10 s lease TTL, with no duplicate-authority tick and no
# entity lost.
#
# Like the P2 kill-9 gate this is a *proof harness*, not a convenience script:
# it writes no success artifact unless every clause of the criterion holds.
# Unlike the P2 gate it needs no FoundationDB — authority lives in the
# registrar, and the criterion is about what the registrar does when a peer
# dies, so the volatile lease store is the honest configuration here.
set -euo pipefail

readonly NAME=p3-island-gate
die() { echo "$NAME: $*" >&2; exit 2; }
note() { echo "$NAME: $*" >&2; }

if [[ ${1:-} == --self-test ]]; then
  # Offline guard for CI images without built binaries. Deliberately
  # structural: it catches regression to a script that no longer proves the
  # criterion, without pretending to run an island locally.
  grep -Fq -- '--dev-seed' "$0" || die 'self-test: entity seeding absent'
  grep -Fq -- '--coordinator-key' "$0" || die 'self-test: interest handout absent'
  grep -Fq -- '--metrics-jsonl' "$0" || die 'self-test: duplicate-authority read absent'
  grep -Fq 'p3-island' "$0" || die 'self-test: island harness absent'
  grep -Fq 'kill -9' "$0" || die 'self-test: kill -9 documentation absent'
  echo 'self-test: island, seeding, interest, and invariant stages present'
  exit 0
fi

: "${PERSISTD_BIN:?set PERSISTD_BIN to a persistd binary}"
: "${P3_ISLAND_BIN:?set P3_ISLAND_BIN to the p3-island binary}"
for tool in "$PERSISTD_BIN" "$P3_ISLAND_BIN"; do
  [[ -x $tool ]] || die "not an executable: $tool"
done

PEERS=${P3_PEERS:-8}
ENTITIES_PER_PEER=${P3_ENTITIES_PER_PEER:-50}
DURATION_SECS=${P3_DURATION_SECS:-30}
# The island's cell. Level 21 origin, matching the harness default.
CELL=${P3_CELL:-0x8000000000000000}

out=${P3_GATE_OUT:-"$(pwd)/p3-island-$(date -u +%Y%m%dT%H%M%SZ)"}
[[ ! -e $out ]] || die "refusing to overwrite existing output directory: $out"
mkdir -p "$out" "$out/data" "$out/peers"
# `kill -9` can land before the reporter's first tick; pre-create so the
# duplicate-authority read never fails on a missing file.
: > "$out/metrics.jsonl"

# Deterministic key material. These are harness keys, generated here and used
# nowhere else: the identity issuer signs peer session tokens, the coordinator
# signs interest grants, and persistd is configured to trust exactly these two.
ISSUER_SECRET=${P3_ISSUER_SECRET:-1111111111111111111111111111111111111111111111111111111111111111}
COORDINATOR_SECRET=${P3_COORDINATOR_SECRET:-2222222222222222222222222222222222222222222222222222222222222222}

# persistd must trust exactly these two keys; derive the public halves with
# the harness itself rather than carrying an ed25519 implementation in shell.
keys=$("$P3_ISLAND_BIN" --print-keys \
  --gateway-addr 127.0.0.1:1 --gateway-node "$(printf '0%.0s' {1..64})" \
  --issuer-secret "$ISSUER_SECRET" --coordinator-secret "$COORDINATOR_SECRET") \
  || die 'could not derive public keys from the supplied secrets'
ISSUER_PUBLIC=$(python3 -c 'import json,sys;print(json.loads(sys.argv[1])["issuer_public"])' "$keys")
COORDINATOR_PUBLIC=$(python3 -c 'import json,sys;print(json.loads(sys.argv[1])["coordinator_public"])' "$keys")

cleanup() {
  if [[ -n ${PERSISTD_PID:-} ]] && kill -0 "$PERSISTD_PID" 2>/dev/null; then
    kill "$PERSISTD_PID" 2>/dev/null || true
    wait "$PERSISTD_PID" 2>/dev/null || true
  fi
  pkill -f "$P3_ISLAND_BIN --peer-spec" 2>/dev/null || true
}
trap cleanup EXIT

total_entities=$((PEERS * ENTITIES_PER_PEER))
note "starting persistd with $total_entities seeded entities in $CELL"

# `--allow-volatile-leases` is correct here and not a shortcut: the criterion
# measures live redistribution, and durability across a cluster restart is the
# P2 gate's job, not this one.
"$PERSISTD_BIN" \
  --dir "$out/data" \
  --bind 127.0.0.1:0 \
  --shard "$CELL" \
  --allow-volatile-leases \
  --issuer-key "1@$ISSUER_PUBLIC" \
  --coordinator-key "1@$COORDINATOR_PUBLIC" \
  --dev-seed "$total_entities@$CELL" \
  --metrics-jsonl "$out/metrics.jsonl" \
  > "$out/persistd.json" 2> "$out/persistd.log" &
PERSISTD_PID=$!

for _ in $(seq 1 100); do
  [[ -s "$out/persistd.json" ]] && break
  kill -0 "$PERSISTD_PID" 2>/dev/null || die "persistd exited early; see $out/persistd.log"
  sleep 0.2
done
[[ -s "$out/persistd.json" ]] || die "persistd never printed its readiness line"

GATEWAY_NODE=$(python3 -c 'import json,sys;print(json.load(open(sys.argv[1]))["node_id"])' "$out/persistd.json")
GATEWAY_ADDR=$(python3 -c 'import json,sys;print(json.load(open(sys.argv[1]))["bind_addr"])' "$out/persistd.json")
SEEDED=$(python3 -c 'import json,sys;print(len(json.load(open(sys.argv[1]))["dev_seeded_entities"]))' "$out/persistd.json")
[[ $SEEDED -eq $total_entities ]] || die "seeded $SEEDED entities, expected $total_entities"
note "gateway $GATEWAY_NODE at $GATEWAY_ADDR, $SEEDED entities seeded"

# The island forms, one peer is `kill -9`ed, and the harness proves every
# entity it held was reassigned or parked.
set +e
"$P3_ISLAND_BIN" \
  --gateway-addr "$GATEWAY_ADDR" \
  --gateway-node "$GATEWAY_NODE" \
  --issuer-secret "$ISSUER_SECRET" \
  --coordinator-secret "$COORDINATOR_SECRET" \
  --peers "$PEERS" \
  --entities-per-peer "$ENTITIES_PER_PEER" \
  --cell "$CELL" \
  --duration-secs "$DURATION_SECS" \
  --metrics-jsonl "$out/metrics.jsonl" \
  --out "$out/peers" \
  > "$out/report.json" 2> "$out/island.log"
island_status=$?
set -e

cat "$out/report.json"
if [[ $island_status -ne 0 ]]; then
  die "island criterion FAILED; report in $out/report.json, logs in $out"
fi

touch "$out/PASSED"
note "criterion held: report $out/report.json"
