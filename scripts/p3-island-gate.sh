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
  #
  # Searched against the script body rather than the whole file, and this is not
  # tidiness. Every pattern below also appears, literally, in the line that looks
  # for it, so `grep -Fq 'X' "$0"` matches its own source and can only pass.
  # Verified 2026-08-17: with the entire body of this script deleted, the checks
  # that predate this comment still reported the stages present. Comment lines
  # are stripped too, so prose naming a stage cannot stand in for the stage.
  body="$(sed -n '/^: /,$p' "$0" | grep -v '^[[:space:]]*#')"
  has() { grep -Fq -- "$1" <<<"$body"; }
  # A stage is asserted against *its own invocation*, not against the body at
  # large, because a pattern free-floating in the body proves nothing about the
  # process it is supposed to configure. Two clauses were vacuous exactly this
  # way until 2026-08-17: `orrery-coordinator` and `p3-island` were matched
  # anywhere in the body, and both appear in the `: "${VAR:?...}"` usage
  # messages at the top — so deleting the coordinator launch, or the harness
  # invocation, left the self-test reporting both stages present. The launches
  # also share flag spellings (`--issuer-key "1@$ISSUER_PUBLIC"` is passed to
  # both daemons), which a body-wide grep cannot tell apart either.
  #
  # One invocation: from the line that runs the named binary through the last
  # of its continuation lines.
  launch() {
    awk -v bin="\"\$$1\" \\" '$0 == bin { inside = 1 }
                                 inside { print; if ($0 !~ /\\$/) exit }' <<<"$body"
  }
  runs() { grep -Fq -- "$2" <<<"$(launch "$1")"; }

  # persistd: the registrar under test, and the two flags it refuses to start
  # without in this configuration. Neither key was checked here once, and the
  # sibling P2 gate shows what that costs: it omitted `--issuer-key` from the
  # day `f33568b` began requiring one and nothing said so until a nightly run
  # months of commits later. Matched with their operands attached — bare
  # `--issuer-key` is a prefix of the harness's own `--issuer-secret` argument
  # name and would pass on a script that had lost the gateway's key entirely.
  runs PERSISTD_BIN '--dev-seed' || die 'self-test: entity seeding absent'
  runs PERSISTD_BIN '--coordinator-key' || die 'self-test: interest handout absent'
  runs PERSISTD_BIN '--issuer-key "1@$ISSUER_PUBLIC"' || die 'self-test: gateway identity issuer key absent'
  runs PERSISTD_BIN '--allow-volatile-leases' || die 'self-test: volatile lease store absent'
  # Half the criterion — an entity that *parks* rather than being reassigned —
  # is observable nowhere but the registrar's own exported authority counters,
  # so a gate whose persistd exports none cannot measure it, and the harness
  # below has to be pointed at the same file.
  runs PERSISTD_BIN '--metrics-jsonl "$out/metrics.jsonl"' || die 'self-test: registrar authority counters not exported'

  # The coordinator: interest is minted by a live one, not by the harness.
  runs COORDINATOR_BIN '--interest-secret "$COORDINATOR_SECRET"' || die 'self-test: live coordinator absent'
  runs COORDINATOR_BIN '--issuer-key "1@$ISSUER_PUBLIC"' || die 'self-test: coordinator identity issuer key absent'

  # The harness. The SIGKILL itself is issued inside the p3-island binary, not
  # here, so this script cannot assert it structurally — what it can assert is
  # that it drives the criterion's shape. The old check looked for the literal
  # `kill -9`, which appears in this file only in prose.
  runs P3_ISLAND_BIN '--peers "$PEERS"' || die 'self-test: island harness absent'
  runs P3_ISLAND_BIN '--entities-per-peer' || die 'self-test: per-peer entity load absent'
  runs P3_ISLAND_BIN '--duration-secs' || die 'self-test: run duration absent'
  runs P3_ISLAND_BIN '--metrics-jsonl' || die 'self-test: duplicate-authority and disposition read absent'
  runs P3_ISLAND_BIN '--victim-claim-kind' || die 'self-test: victim claim tier not selectable'

  # A proof harness is only a proof if its verdict is load-bearing: the gate
  # must die on a non-zero harness exit, and the success artifact must be
  # written nowhere else.
  has 'island_status -ne 0' || die 'self-test: harness verdict not enforced'
  has 'touch "$out/PASSED"' || die 'self-test: success artifact absent'
  echo 'self-test: island, seeding, interest, and invariant stages present'
  exit 0
fi

: "${PERSISTD_BIN:?set PERSISTD_BIN to a persistd binary}"
: "${P3_ISLAND_BIN:?set P3_ISLAND_BIN to the p3-island binary}"
: "${COORDINATOR_BIN:?set COORDINATOR_BIN to the orrery-coordinator binary}"
for tool in "$PERSISTD_BIN" "$P3_ISLAND_BIN" "$COORDINATOR_BIN"; do
  [[ -x $tool ]] || die "not an executable: $tool"
done

PEERS=${P3_PEERS:-8}
ENTITIES_PER_PEER=${P3_ENTITIES_PER_PEER:-50}
DURATION_SECS=${P3_DURATION_SECS:-30}
# The tier the victim claims at, and with it which half of the criterion the
# run exercises. `weak` is the contested-physics case, which redistributes;
# `strong` is the case D7 §5 refuses to redistribute without consent, so every
# one of the victim's entities parks instead. Both are correct registrar
# behaviour and the criterion accepts both.
#
# **One run is half a gate.** This variable's default is the only reason the
# weak leg is the one anybody sees, and until #129 it was also the only reason
# the strong leg's defect survived: its parked rows were reported lost, the leg
# could not pass, and nothing ran it to say so. Both legs now run in
# `nightly.yml`'s `p3-island` job and in `scripts/gate-status.sh --full`, as
# two invocations of this script with the variable set either way. This script
# still runs exactly one leg per invocation — the choice belongs to whoever
# drives it — but "the default was never changed" is not a reason for a leg to
# go unrun.
VICTIM_CLAIM_KIND=${P3_VICTIM_CLAIM_KIND:-weak}
# The island's cell. Level 21 origin, matching the harness default.
CELL=${P3_CELL:-0x8000000000000000}

out=${P3_GATE_OUT:-"$(pwd)/p3-island-$(date -u +%Y%m%dT%H%M%SZ)"}
[[ ! -e $out ]] || die "refusing to overwrite existing output directory: $out"
mkdir -p "$out" "$out/data" "$out/peers"
# `kill -9` can land before the reporter's first tick; pre-create so the
# duplicate-authority read never fails on a missing file.
: > "$out/metrics.jsonl"

# Deterministic key material. These are harness keys, generated here and used
# nowhere else: the identity issuer signs peer session tokens (both the
# coordinator and persistd verify them), and the coordinator signs the interest
# grants persistd is configured to trust.
ISSUER_SECRET=${P3_ISSUER_SECRET:-1111111111111111111111111111111111111111111111111111111111111111}
COORDINATOR_SECRET=${P3_COORDINATOR_SECRET:-2222222222222222222222222222222222222222222222222222222222222222}

# Derive the issuer's public half with the harness rather than carrying an
# ed25519 implementation in shell. The coordinator's public half comes from the
# coordinator's own readiness line — it is the one that signs, so it is the one
# that says which key to trust.
keys=$("$P3_ISLAND_BIN" --print-keys \
  --gateway-addr 127.0.0.1:1 --gateway-node "$(printf '0%.0s' {1..64})" \
  --coordinator-addr 127.0.0.1:1 --coordinator-node "$(printf '0%.0s' {1..64})" \
  --issuer-secret "$ISSUER_SECRET") \
  || die 'could not derive the issuer public key from the supplied secret'
ISSUER_PUBLIC=$(python3 -c 'import json,sys;print(json.loads(sys.argv[1])["issuer_public"])' "$keys")

cleanup() {
  for pid in ${PERSISTD_PID:-} ${COORDINATOR_PID:-}; do
    if kill -0 "$pid" 2>/dev/null; then
      kill "$pid" 2>/dev/null || true
      wait "$pid" 2>/dev/null || true
    fi
  done
  pkill -f "$P3_ISLAND_BIN --peer-spec" 2>/dev/null || true
}
trap cleanup EXIT

# ── Coordinator ─────────────────────────────────────────────────────────
# Interest is minted here, not by the harness. A fixture that signs its own
# authorization proves nothing about the path production uses.
note 'starting the coordinator'
"$COORDINATOR_BIN" \
  --bind 127.0.0.1:0 \
  --issuer-key "1@$ISSUER_PUBLIC" \
  --interest-secret "$COORDINATOR_SECRET" \
  --interest-key-id 1 \
  > "$out/coordinator.json" 2> "$out/coordinator.log" &
COORDINATOR_PID=$!

for _ in $(seq 1 100); do
  [[ -s "$out/coordinator.json" ]] && break
  kill -0 "$COORDINATOR_PID" 2>/dev/null || die "coordinator exited early; see $out/coordinator.log"
  sleep 0.2
done
[[ -s "$out/coordinator.json" ]] || die 'coordinator never printed its readiness line'

COORDINATOR_NODE=$(python3 -c 'import json,sys;print(json.load(open(sys.argv[1]))["node_id"])' "$out/coordinator.json")
COORDINATOR_ADDR=$(python3 -c 'import json,sys;print(json.load(open(sys.argv[1]))["bind_addr"])' "$out/coordinator.json")
COORDINATOR_PUBLIC=$(python3 -c 'import json,sys;print(json.load(open(sys.argv[1]))["interest_public_key"])' "$out/coordinator.json")
note "coordinator $COORDINATOR_NODE at $COORDINATOR_ADDR signing interest with $COORDINATOR_PUBLIC"

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
  --coordinator-addr "$COORDINATOR_ADDR" \
  --coordinator-node "$COORDINATOR_NODE" \
  --issuer-secret "$ISSUER_SECRET" \
  --peers "$PEERS" \
  --entities-per-peer "$ENTITIES_PER_PEER" \
  --victim-claim-kind "$VICTIM_CLAIM_KIND" \
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
