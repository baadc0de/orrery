#!/usr/bin/env bash
# Two sibling persistd gateways over disjoint shards, and the single-writer
# invariant stated across both of them (issue #118, D26).
#
# Nothing in this repository had ever run two active gateways at once: the
# binary calls itself a single-node harness, `Cluster` is an in-process fixture
# with no node-to-node transport, and the only two-process topology is a
# primary plus a passive journal follower over the *same* shard set, where the
# follower is documented as "never a gateway". This gate is the observable
# baseline that was missing.
#
# The criterion, in the same clause structure as `scripts/p3-island-gate.sh`
# and enforced in `p3-siblings/src/main.rs` rather than here:
#
#   1. two persistd + one coordinator, disjoint `--shard` subtrees of one grid,
#      one FoundationDB cluster carrying one fence and one lease tier;
#   2. at least eight peers, each holding a session to *both* gateways, with
#      rows on both;
#   3. `kill -9` one peer: every row it held — on either gateway — reassigned
#      or parked inside the settle budget, nothing lost;
#   4. `kill -9` one *gateway*: the survivor's leases neither expire nor
#      duplicate, and its disposition counters do not move;
#   5. `duplicate_authority` is the **sum** over both gateways' exports, which
#      is the point — `AuthorityMetrics` is per-`GatewayServer` and nothing in
#      the tree aggregates it.
#
# Like the P2 and P3 gates this is a *proof harness*: it writes no success
# artifact unless every clause holds. Unlike the P3 island gate it needs
# FoundationDB and cannot be run without one — two gateways that did not share
# a durable fence and lease tier would be two unrelated processes, and the
# whole question is what they do to each other's rows.
set -euo pipefail

readonly NAME=p3-siblings-gate
die() { echo "$NAME: $*" >&2; exit 2; }
note() { echo "$NAME: $*" >&2; }

if [[ ${1:-} == --self-test ]]; then
  # Offline guard for CI images with no cluster and no built binaries.
  # Deliberately structural: it catches regression to a script that no longer
  # proves the criterion, without pretending to run two gateways locally.
  #
  # Searched against the script *body* rather than the whole file, and that is
  # not tidiness: every pattern below also appears, literally, in the line that
  # looks for it, so a whole-file `grep -Fq` matches its own source and can
  # only pass. Comment lines are stripped too, so prose naming a stage cannot
  # stand in for the stage.
  body="$(sed -n '/^: /,$p' "$0" | grep -v '^[[:space:]]*#')"
  has() { grep -Fq -- "$1" <<<"$body"; }

  # A stage is asserted against *its own invocation*, never against the body at
  # large. Two clauses in the sibling P3 gate were vacuous exactly that way
  # until 2026-08-17, because the binary names appear in the `${VAR:?...}`
  # usage messages at the top and a body-wide grep cannot tell a usage message
  # from a launch.
  #
  # This gate needs one thing that one does not: **two launches of the same
  # binary**. `$PERSISTD_BIN` is started twice, with different shard sets,
  # different data directories and different metrics files, and a matcher that
  # stopped at the first invocation would report gateway B's stages present on
  # a script that had lost gateway B entirely — which is the single most
  # important thing this gate could lose. So `launch` takes an occurrence
  # number and returns the Nth invocation of that binary, from the line that
  # runs it through the last of its continuation lines.
  launch() { # VAR occurrence
    awk -v bin="\"\$$1\" \\" -v want="$2" '
      $0 == bin { n++; inside = (n == want); if (inside) { print; next } }
      inside { print; if ($0 !~ /\\$/) inside = 0 }
    ' <<<"$body"
  }
  runs() { grep -Fq -- "$3" <<<"$(launch "$1" "$2")"; }
  # Exactly two, and the count is asserted rather than assumed: a third
  # gateway would silently go unchecked by every clause below.
  gateways=$(grep -cFx -- '"$PERSISTD_BIN" \' <<<"$body" || true)
  [[ $gateways -eq 2 ]] \
    || die "self-test: expected exactly two persistd launches, found $gateways"

  # ── Gateway A ──
  runs PERSISTD_BIN 1 '--fdb-cluster-file "$ORRERY_FDB_CLUSTER_FILE"' \
    || die 'self-test: gateway A does not share the durable fence and lease tier'
  runs PERSISTD_BIN 1 '"${shard_flags_a[@]}"' || die 'self-test: gateway A owns no shard set'
  runs PERSISTD_BIN 1 '--metrics-jsonl "$out/metrics-a.jsonl"' \
    || die 'self-test: gateway A exports no authority counters; half the summed invariant is unreadable'
  runs PERSISTD_BIN 1 '--issuer-key "1@$ISSUER_PUBLIC"' \
    || die 'self-test: gateway A identity issuer key absent'
  runs PERSISTD_BIN 1 '--coordinator-key "1@$COORDINATOR_PUBLIC"' \
    || die 'self-test: gateway A trusts no coordinator, so no weak claim can be granted'
  runs PERSISTD_BIN 1 '--dir "$out/data-a"' || die 'self-test: gateway A has no journal of its own'

  # ── Gateway B ──
  runs PERSISTD_BIN 2 '--fdb-cluster-file "$ORRERY_FDB_CLUSTER_FILE"' \
    || die 'self-test: gateway B does not share the durable fence and lease tier'
  runs PERSISTD_BIN 2 '"${shard_flags_b[@]}"' || die 'self-test: gateway B owns no shard set'
  runs PERSISTD_BIN 2 '--metrics-jsonl "$out/metrics-b.jsonl"' \
    || die 'self-test: gateway B exports no authority counters; the sum would be gateway A alone'
  runs PERSISTD_BIN 2 '--issuer-key "1@$ISSUER_PUBLIC"' \
    || die 'self-test: gateway B identity issuer key absent'
  runs PERSISTD_BIN 2 '--coordinator-key "1@$COORDINATOR_PUBLIC"' \
    || die 'self-test: gateway B trusts no coordinator, so no weak claim can be granted'
  runs PERSISTD_BIN 2 '--dir "$out/data-b"' || die 'self-test: gateway B has no journal of its own'

  # A volatile lease store would make the two processes independent, which is
  # the one thing this gate must not let them be.
  if has '--allow-volatile-leases'; then
    die 'self-test: a volatile lease store makes the two gateways unrelated processes'
  fi

  # ── The coordinator ──
  runs COORDINATOR_BIN 1 '--interest-secret "$COORDINATOR_SECRET"' \
    || die 'self-test: live coordinator absent'
  runs COORDINATOR_BIN 1 '--issuer-key "1@$ISSUER_PUBLIC"' \
    || die 'self-test: coordinator identity issuer key absent'

  # ── The seeded world ──
  # `--dev-seed` refuses to run with `--fdb-cluster-file` set, so the rows two
  # gateways share have to be durable ones. The shard set is derived from the
  # manifest by the seeder, never spelled here: re-implementing `shard_of` in
  # shell would duplicate a packed-bit encoding, and a subtly wrong shard set
  # does not fail loudly — it addresses part of the world to nobody.
  # Matched with their operands attached. Bare `apply` was vacuous when this
  # was first mutation-checked: the same invocation redirects into
  # `$out/seed-apply.log`, so deleting the subcommand left the clause passing.
  runs ORRERY_SEED_BIN 1 'apply "$SCENARIO"' || die 'self-test: the durable world is never seeded'
  runs ORRERY_SEED_BIN 2 '--emit-manifest "$out/manifest.json"' \
    || die 'self-test: no manifest, so the harness has no inventory to route'
  runs ORRERY_SEED_BIN 3 'shards "$out/manifest.json"' \
    || die 'self-test: the shard set is not derived from the seeded world'
  has 'split_shard_set' || die 'self-test: the shard set is never split between the two gateways'
  # D26 rule 1: the durable `actor/{grid}/{shard}` row is the ownership rule,
  # so routing is taken from what each gateway *activated* and reported on its
  # readiness line — never from the flag list this script hoped it would take.
  has 'activated_shards "$out/persistd-a.json" "$out/shards-a.txt"' \
    || die 'self-test: routing is not taken from what gateway A activated'
  has 'activated_shards "$out/persistd-b.json" "$out/shards-b.txt"' \
    || die 'self-test: routing is not taken from what gateway B activated'
  has 'refuse_an_already_activated_cluster' \
    || die 'self-test: no fresh-cluster pre-flight; this gate seeds and activates a world'

  # ── The harness ──
  runs P3_SIBLINGS_BIN 1 '--peers "$PEERS"' || die 'self-test: sibling harness absent'
  runs P3_SIBLINGS_BIN 1 '--manifest "$out/manifest.json"' || die 'self-test: harness inventory absent'
  runs P3_SIBLINGS_BIN 1 '--shards-a "$out/shards-a.txt"' \
    || die 'self-test: harness does not route from the flags gateway A was given'
  runs P3_SIBLINGS_BIN 1 '--shards-b "$out/shards-b.txt"' \
    || die 'self-test: harness does not route from the flags gateway B was given'
  runs P3_SIBLINGS_BIN 1 '--metrics-a "$out/metrics-a.jsonl"' \
    || die 'self-test: summed invariant is missing gateway A'
  runs P3_SIBLINGS_BIN 1 '--metrics-b "$out/metrics-b.jsonl"' \
    || die 'self-test: summed invariant is missing gateway B'
  # The gateway `kill -9` is issued inside the harness, not here, because the
  # clause is about *when* the survivor's leases did not end and only the
  # process holding the clock can subtract two instants. What this script can
  # assert is that it hands over the pid to kill.
  runs P3_SIBLINGS_BIN 1 '--gateway-b-pid "$PERSISTD_B_PID"' \
    || die 'self-test: the gateway kill -9 is never armed; two coexisting processes would pass'
  runs P3_SIBLINGS_BIN 1 '--duration-secs' || die 'self-test: run duration absent'

  # A proof harness is only a proof if its verdict is load-bearing.
  has 'sibling_status -ne 0' || die 'self-test: harness verdict not enforced'
  has 'touch "$out/PASSED"' || die 'self-test: success artifact absent'
  echo 'self-test: two gateways, disjoint shards, seeded world, both kills and the summed invariant present'
  exit 0
fi

: "${ORRERY_FDB_CLUSTER_FILE:?set ORRERY_FDB_CLUSTER_FILE: two gateways must share one durable fence and lease tier}"
: "${PERSISTD_BIN:?set PERSISTD_BIN to an fdb-enabled persistd binary}"
: "${P3_SIBLINGS_BIN:?set P3_SIBLINGS_BIN to the p3-sibling binary}"
: "${COORDINATOR_BIN:?set COORDINATOR_BIN to the orrery-coordinator binary}"
: "${ORRERY_SEED_BIN:?set ORRERY_SEED_BIN to an fdb-enabled orrery-seed binary}"
[[ -r $ORRERY_FDB_CLUSTER_FILE ]] || die "FDB cluster file is not readable: $ORRERY_FDB_CLUSTER_FILE"
for tool in "$PERSISTD_BIN" "$P3_SIBLINGS_BIN" "$COORDINATOR_BIN" "$ORRERY_SEED_BIN"; do
  [[ -x $tool ]] || die "not an executable: $tool"
done

PEERS=${P3_SIBLINGS_PEERS:-8}
DURATION_SECS=${P3_SIBLINGS_DURATION_SECS:-75}
SCENARIO=${P3_SIBLINGS_SCENARIO:-"$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)/crates/orrery_seed/scenarios/p2demo.toml"}
# The `ci` rung of the P2 demo scenario: 100 rows, hash-placed, one per shard.
# Small on purpose and not arbitrarily so — an interest grant may cover at most
# `MAX_INTEREST_GRANT_CELLS` (64) cells, and this harness gives each of its two
# overlapping interest zones half the world, so the seeded row count is bounded
# by 2 x 64 at any peer count.
SEED_PROFILE=${P3_SIBLINGS_SEED_PROFILE:-ci}
[[ -r $SCENARIO ]] || die "seed scenario is not readable: $SCENARIO"

# This gate seeds a world and activates two shard sets against the fence. The
# seeder's `[load] mode = "offline"` refuses to write into a range whose
# `actor/{shard}` rows are live, and a second run against a cluster the last
# one left rows in reads as a defect in the code under test rather than as a
# true statement about the database. Advisory only: without `fdbcli` the run
# proceeds exactly as it would have.
refuse_an_already_activated_cluster() {
  command -v fdbcli >/dev/null 2>&1 || {
    note 'fdbcli not on PATH; skipping the fresh-cluster pre-flight'
    return 0
  }
  local rows
  rows=$(timeout 30 fdbcli -C "$ORRERY_FDB_CLUSTER_FILE" --exec 'getrangekeys a b 1' 2>/dev/null \
    | grep -c '^`') || true
  [[ ${rows:-0} -eq 0 ]] || die "this cluster already carries an actor/ activation row from an earlier run; this gate seeds and activates a world and must be pointed at a fresh throwaway cluster, never the shared development one"
}
refuse_an_already_activated_cluster

out=${P3_SIBLINGS_GATE_OUT:-"$(pwd)/p3-siblings-$(date -u +%Y%m%dT%H%M%SZ)"}
[[ ! -e $out ]] || die "refusing to overwrite existing output directory: $out"
mkdir -p "$out" "$out/data-a" "$out/data-b" "$out/peers"
# Either `kill -9` can land before a reporter's first tick; pre-create both so
# the summed read never fails on a missing file.
: > "$out/metrics-a.jsonl"
: > "$out/metrics-b.jsonl"

# Deterministic harness key material, generated here and used nowhere else.
ISSUER_SECRET=${P3_SIBLINGS_ISSUER_SECRET:-1111111111111111111111111111111111111111111111111111111111111111}
COORDINATOR_SECRET=${P3_SIBLINGS_COORDINATOR_SECRET:-2222222222222222222222222222222222222222222222222222222222222222}

keys=$("$P3_SIBLINGS_BIN" --print-keys \
  --gateway-a-addr 127.0.0.1:1 --gateway-a-node "$(printf '0%.0s' {1..64})" \
  --gateway-b-addr 127.0.0.1:1 --gateway-b-node "$(printf '0%.0s' {1..64})" \
  --coordinator-addr 127.0.0.1:1 --coordinator-node "$(printf '0%.0s' {1..64})" \
  --issuer-secret "$ISSUER_SECRET") \
  || die 'could not derive the issuer public key from the supplied secret'
ISSUER_PUBLIC=$(python3 -c 'import json,sys;print(json.loads(sys.argv[1])["issuer_public"])' "$keys")

PERSISTD_A_PID=''
PERSISTD_B_PID=''
COORDINATOR_PID=''
cleanup() {
  for pid in $PERSISTD_A_PID $PERSISTD_B_PID $COORDINATOR_PID; do
    if kill -0 "$pid" 2>/dev/null; then
      kill "$pid" 2>/dev/null || true
      wait "$pid" 2>/dev/null || true
    fi
  done
  pkill -f "$P3_SIBLINGS_BIN --peer-spec" 2>/dev/null || true
}
trap cleanup EXIT

wait_ready() { # file pid label
  local file=$1 pid=$2 label=$3
  for _ in $(seq 1 600); do
    [[ -s $file ]] && return 0
    kill -0 "$pid" 2>/dev/null || die "$label exited before readiness; see ${file%.json}.log"
    sleep 0.2
  done
  die "timed out waiting for $label readiness; see ${file%.json}.log"
}
json_field() { python3 -c 'import json,sys;print(json.load(open(sys.argv[1]))[sys.argv[2]])' "$1" "$2"; }

# ── The durable world ────────────────────────────────────────────────────
# A claim needs an entity whose committed cell the registrar can resolve, and
# `--dev-seed` — the single-gateway island gate's affordance — refuses to run
# with `--fdb-cluster-file` set. So the rows the two gateways share are seeded
# ones, exactly as the P2 gate seeds its own.
note "seeding the durable world from $(basename "$SCENARIO") (profile $SEED_PROFILE)"
"$ORRERY_SEED_BIN" \
  apply "$SCENARIO" \
  --profile "$SEED_PROFILE" \
  --allow-opaque \
  --single-grid \
  > "$out/seed-apply.log" 2>&1 || die "seeding failed; see $out/seed-apply.log"
"$ORRERY_SEED_BIN" \
  verify "$SCENARIO" \
  --profile "$SEED_PROFILE" \
  --single-grid \
  --emit-manifest "$out/manifest.json" \
  > "$out/seed-verify.log" 2>&1 || die "seed verification failed; see $out/seed-verify.log"
[[ -s $out/manifest.json ]] || die 'seeder emitted no manifest'

# The shard set the seeded world implies, collapsed by the seeder rather than
# by a `jq` expression here.
"$ORRERY_SEED_BIN" \
  shards "$out/manifest.json" \
  --grid 0 \
  > "$out/shard-set.txt" || die "could not derive the shard set from $out/manifest.json"

# Two disjoint subtrees of one grid, as two halves of the sorted shard list.
#
# Contiguous halves rather than an interleave, because "disjoint `--shard`
# subtrees" is what the topology is about and a `CellId` list in Morton order
# is already in subtree order.
#
# What this writes is the *intent*. What the harness routes from is each
# gateway's readiness line — see `activated_shards` below — because the flags a
# process was given and the shard rows it actually activated against the fence
# are two different facts, and D26 rule 1 makes the second one the ownership
# rule. Any row whose shard neither gateway activated is refused by the harness
# rather than addressed to nobody.
shard_flags_a=()
shard_flags_b=()
split_shard_set() {
  mapfile -t shards <"$out/shard-set.txt"
  local total=${#shards[@]}
  [[ $total -ge 2 ]] || die "the seeded manifest collapsed to $total shard(s); a sibling topology needs at least two"
  local half=$((total / 2)) index shard
  : >"$out/shard-flags-a.txt"
  : >"$out/shard-flags-b.txt"
  for index in "${!shards[@]}"; do
    shard=${shards[$index]}
    if [[ $index -lt $half ]]; then
      echo "$shard" >>"$out/shard-flags-a.txt"
      shard_flags_a+=(--shard "$shard")
    else
      echo "$shard" >>"$out/shard-flags-b.txt"
      shard_flags_b+=(--shard "$shard")
    fi
  done
  note "shard set split: $half shard(s) to gateway A, $((total - half)) to gateway B"
}
split_shard_set

# The shard cells one gateway reports having activated, from its own readiness
# line (`persistd` prints `shards: [{cell, epoch}]`). This is the durable
# `actor/{grid}/{shard}` ownership D26 rule 1 names, stated by the process that
# holds it — not the flag list this script hoped it would take.
activated_shards() { # readiness-json out-file
  python3 -c 'import json,sys
rows = json.load(open(sys.argv[1]))["shards"]
if not rows:
    raise SystemExit("readiness line names no activated shards")
with open(sys.argv[2], "w", encoding="utf-8") as out:
    for row in rows:
        out.write(str(row["cell"]) + "\n")' "$1" "$2" \
    || die "could not read the activated shard set from $1"
}

# ── Coordinator ──────────────────────────────────────────────────────────
# Interest is minted by a live coordinator, not by the harness: a fixture that
# signs its own authorization proves nothing about the path production uses.
# One coordinator for both gateways, because two gateways in one grid are two
# shard sets of *one* interest space.
note 'starting the coordinator'
"$COORDINATOR_BIN" \
  --bind 127.0.0.1:0 \
  --issuer-key "1@$ISSUER_PUBLIC" \
  --interest-secret "$COORDINATOR_SECRET" \
  --interest-key-id 1 \
  > "$out/coordinator.json" 2> "$out/coordinator.log" &
COORDINATOR_PID=$!
wait_ready "$out/coordinator.json" "$COORDINATOR_PID" coordinator
COORDINATOR_NODE=$(json_field "$out/coordinator.json" node_id)
COORDINATOR_ADDR=$(json_field "$out/coordinator.json" bind_addr)
COORDINATOR_PUBLIC=$(json_field "$out/coordinator.json" interest_public_key)
note "coordinator $COORDINATOR_NODE at $COORDINATOR_ADDR signing interest with $COORDINATOR_PUBLIC"

# ── Gateway A ────────────────────────────────────────────────────────────
# No `--allow-volatile-leases`: the durable lease tier is the shared thing, and
# a volatile one would make these two processes unrelated.
note "starting gateway A over ${#shard_flags_a[@]} shard flags"
"$PERSISTD_BIN" \
  --dir "$out/data-a" \
  --bind 127.0.0.1:0 \
  --fdb-cluster-file "$ORRERY_FDB_CLUSTER_FILE" \
  "${shard_flags_a[@]}" \
  --issuer-key "1@$ISSUER_PUBLIC" \
  --coordinator-key "1@$COORDINATOR_PUBLIC" \
  --metrics-jsonl "$out/metrics-a.jsonl" \
  > "$out/persistd-a.json" 2> "$out/persistd-a.log" &
PERSISTD_A_PID=$!
wait_ready "$out/persistd-a.json" "$PERSISTD_A_PID" 'gateway A'
GATEWAY_A_NODE=$(json_field "$out/persistd-a.json" node_id)
GATEWAY_A_ADDR=$(json_field "$out/persistd-a.json" bind_addr)
activated_shards "$out/persistd-a.json" "$out/shards-a.txt"
note "gateway A $GATEWAY_A_NODE at $GATEWAY_A_ADDR (pid $PERSISTD_A_PID), $(wc -l <"$out/shards-a.txt") shard(s) activated"

# ── Gateway B ────────────────────────────────────────────────────────────
note "starting gateway B over ${#shard_flags_b[@]} shard flags"
"$PERSISTD_BIN" \
  --dir "$out/data-b" \
  --bind 127.0.0.1:0 \
  --fdb-cluster-file "$ORRERY_FDB_CLUSTER_FILE" \
  "${shard_flags_b[@]}" \
  --issuer-key "1@$ISSUER_PUBLIC" \
  --coordinator-key "1@$COORDINATOR_PUBLIC" \
  --metrics-jsonl "$out/metrics-b.jsonl" \
  > "$out/persistd-b.json" 2> "$out/persistd-b.log" &
PERSISTD_B_PID=$!
wait_ready "$out/persistd-b.json" "$PERSISTD_B_PID" 'gateway B'
GATEWAY_B_NODE=$(json_field "$out/persistd-b.json" node_id)
GATEWAY_B_ADDR=$(json_field "$out/persistd-b.json" bind_addr)
activated_shards "$out/persistd-b.json" "$out/shards-b.txt"
note "gateway B $GATEWAY_B_NODE at $GATEWAY_B_ADDR (pid $PERSISTD_B_PID), $(wc -l <"$out/shards-b.txt") shard(s) activated"
[[ $GATEWAY_A_NODE != "$GATEWAY_B_NODE" ]] || die 'both gateways came up under one node identity'

# ── The proof ────────────────────────────────────────────────────────────
set +e
"$P3_SIBLINGS_BIN" \
  --gateway-a-addr "$GATEWAY_A_ADDR" \
  --gateway-a-node "$GATEWAY_A_NODE" \
  --metrics-a "$out/metrics-a.jsonl" \
  --shards-a "$out/shards-a.txt" \
  --gateway-b-addr "$GATEWAY_B_ADDR" \
  --gateway-b-node "$GATEWAY_B_NODE" \
  --metrics-b "$out/metrics-b.jsonl" \
  --shards-b "$out/shards-b.txt" \
  --gateway-b-pid "$PERSISTD_B_PID" \
  --coordinator-addr "$COORDINATOR_ADDR" \
  --coordinator-node "$COORDINATOR_NODE" \
  --issuer-secret "$ISSUER_SECRET" \
  --manifest "$out/manifest.json" \
  --peers "$PEERS" \
  --duration-secs "$DURATION_SECS" \
  --out "$out/peers" \
  > "$out/report.json" 2> "$out/sibling.log"
sibling_status=$?
set -e

cat "$out/report.json"
if [[ $sibling_status -ne 0 ]]; then
  die "sibling criterion FAILED; report in $out/report.json, logs in $out"
fi

touch "$out/PASSED"
note "criterion held across both gateways: report $out/report.json"
