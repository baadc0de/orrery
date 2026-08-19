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
  #
  # Searched against the script body rather than the whole file, and this is not
  # tidiness. Every pattern below also appears, literally, in the line that looks
  # for it, so `grep -Fq 'X' "$0"` matches its own source and can only pass.
  # Verified 2026-08-17: with the entire body of this script deleted, the checks
  # that predate this comment still reported the stages present. Comment lines
  # are stripped too, so prose naming a stage cannot stand in for the stage.
  body="$(sed -n '/^: /,$p' "$0" | grep -v '^[[:space:]]*#')"
  has() { grep -Fq -- "$1" <<<"$body"; }
  # A shell function definition contains its own name, so `has 'seed_world'`
  # is satisfied by `seed_world() {` alone — the stage can be defined and never
  # invoked and the clause still reports it present. Measured 2026-08-17 by
  # deleting each call site in turn: seven of the fifteen clauses passed on a
  # script that no longer ran the stage at all (follower startup, promotion,
  # the epoch-fork proof, the zombie fence, seeding, the fresh-cluster
  # pre-flight, and the server-span attribution check). `runs` matches a bare
  # call line instead — `name` on a line of its own, at any indentation — and
  # never `name() {`, because the definition has a `(` where this wants
  # end-of-line. It is deliberately literal: wrapping a stage in an `if` would
  # trip it. That is the right direction for a structural check — a false
  # alarm here is one grep to read, while a silent one cost the run.
  runs() { grep -Eq -- "^[[:space:]]*$1[[:space:]]*$" <<<"$body"; }
  runs 'start_follower' || die 'self-test: follower startup absent'
  # The fenced primary never had a clause of its own — the `--issuer-key` one
  # below is about its flags, not about it running — so deleting the call left
  # every clause green on a script that starts no writable node at all. That is
  # precisely the "incomplete script" this offline guard exists to catch.
  runs 'start_primary' || die 'self-test: fenced primary startup absent'
  has '--promote-from' || die 'self-test: promotion absent'
  runs 'start_promoted_follower' || die 'self-test: promotion is defined but never run'
  has 'verify-recovery' || die 'self-test: recovery verifier absent'
  # `zombie` alone matched `zombie_port`, `zombie_pid` and
  # `zombie-metrics.jsonl` — all of which survive deleting the entire stale-
  # owner admission proof. These two strings exist only inside it, and they are
  # its two halves: the process must be *expected to fail*, and the failure
  # must be *recognizably a fence*.
  has 'zombie primary unexpectedly passed startup admission' \
    || die 'self-test: zombie fence proof absent'
  has 'zombie failed, but not with recognizable fence admission evidence' \
    || die 'self-test: zombie fence evidence assertion absent'
  runs 'prove_epoch_fork_refused' || die 'self-test: primary-restart epoch-fork proof absent'
  has '--gate --json' || die 'self-test: latency gate absent'
  # `persistd` has refused to start without an identity issuer key since
  # `f33568b feat(p3): complete strict persistence authority path`, and this
  # script did not pass one. Nothing noticed, because the only thing that runs
  # it is nightly: the P2 durability criterion was simply unrunnable, and the
  # first nightly said so with `Error: authority requires at least one
  # --issuer-key <key-id>@<public-key>` in `primary.stderr`. Two clauses,
  # because the gap has two halves and either alone still fails: the gateway
  # must trust an issuer, and the rig must be able to mint a token that issuer
  # signed.
  # Matched with their operands attached, and that is load-bearing: plain
  # `--issuer-key` is a prefix of the rig's `--issuer-key-id`, so the shorter
  # pattern passes on a script that has lost every gateway key and kept only
  # the rig's key id.
  has '--issuer-key "$issuer_key_id@$issuer_public"' || die 'self-test: gateway identity issuer key absent'
  has '--issuer-secret "$secret_issuer"' || die 'self-test: load rig session-token minting absent'
  # The merge below concatenates the client rig's samples with persistd's into
  # one fold, and the dashboard folds by series name with no source field. A
  # server-internal span is strictly shorter than the client round trip it
  # attributes, so if persistd ever writes one under a gated name the merged
  # p99 drops and this gate passes on a measurement it never made. The check
  # is cheap and belongs here, where the two files meet.
  # Matched at its call site, with the operand attached: the bare name is also
  # the `def` line, which survives deleting the call.
  has "server_side_spans_never_gate(l.get('series')" \
    || die 'self-test: server-span attribution check absent'
  # Every bulk write the gateway routes is fenced (`strict_authority: true`,
  # unconditionally, in `route_session_diff`), and a lease claim names a cell
  # the registrar can already resolve for the entity — so the world has to be
  # seeded before anything can be claimed, and therefore before anything can be
  # written. Without this stage the run drives 500k rejections and calls the
  # refusals durability.
  # The gate consumes its cluster (see the pre-flight's own comment). Losing
  # this check does not make the gate pass wrongly — it makes a rerun's
  # startup refusal unreadable, which is how a whole debugging pass went into
  # the wrong subsystem once already.
  runs 'refuse_an_already_activated_cluster' || die 'self-test: fresh-cluster pre-flight absent'
  runs 'seed_world' || die 'self-test: durable world seeding absent'
  has '--emit-manifest' || die 'self-test: seeder manifest emission absent'
  # Every persistd here used to start with no `--shard`, and persistd's
  # `resolve_shards` defaults an absent flag to `vec![CellId::ROOT]`: one
  # shard, one single-writer actor for the whole 10 000-entity world. The
  # measured cost was `router_apply` at 721 ms of a 723 ms acknowledged diff
  # (99.7 %) while `journal_commit_ms` sat at 1.03 ms — the mailbox, not the
  # disk — and the run ended with 8 760 leases withdrawn mid-flight. The
  # deployment under test is 128 actors (docs/11-roadmap.md §P2); running one
  # measures a topology the criterion never describes.
  runs 'derive_shard_set' || die 'self-test: shard-set derivation absent'
  # Matched with the operand attached: a bare `--shard` also appears in prose,
  # and `shard_flags` alone survives deleting every use of the array.
  has '"${shard_flags[@]}"' || die 'self-test: derived shard set is not passed to persistd'
  # The shard set is part of `DurableChainId` (persistd's `canonical_shard_set`
  # feeds `Topology::chain_id`), so a follower or a promoted node started on a
  # different set is a different chain and the mirror handshake fails. All four
  # persistd invocations must carry the same list; count them here rather than
  # discovering it as an unreadable handshake error minutes in.
  # Five: passive follower, fenced primary, the epoch-fork probe, the promoted
  # follower, the zombie. `body` starts at the first `: ` line, i.e. after this
  # block, so these clauses cannot match their own source.
  [[ $(grep -cF -- '"${shard_flags[@]}"' <<<"$body") -ge 5 ]] \
    || die 'self-test: not every persistd invocation carries the shard set'
  has '--manifest "$out/manifest.json"' || die 'self-test: rig is not driven from the seeded inventory'
  # The evidence check. `[[ -s acks.jsonl ]]` passed on 1024 lines of
  # `IntentOutcome::Rejected` and zero durable writes: a non-empty file is not
  # evidence of a durable acknowledgement. Count the `"type":"diff"` records —
  # p2-load writes one only for a *non-provisional* bulk ack — and require
  # more than zero. Matched with the operand attached: bare `durable_acks`
  # appears in prose and in the rig's own log line, so the shorter pattern
  # would pass on a script that had lost the assertion entirely.
  has 'durable_acks=$(grep -c' || die 'self-test: durable-acknowledgement count absent'
  has 'load produced no durable bulk acknowledgement' \
    || die 'self-test: durable-acknowledgement assertion absent'
  echo 'self-test: two-process proof stages present'
  exit 0
fi

: "${ORRERY_FDB_CLUSTER_FILE:?set ORRERY_FDB_CLUSTER_FILE}"
: "${PERSISTD_BIN:?set PERSISTD_BIN to an fdb-enabled persistd binary}"
: "${P2_LOAD_BIN:?set P2_LOAD_BIN to the p2-load binary}"
: "${P2_DASHBOARD_BIN:?set P2_DASHBOARD_BIN to the p2-dashboard binary}"
# The seeder. Fenced writes need a claimable world and a claim needs a
# committed cell, so this binary is now on the critical path of the durability
# proof, not an optional convenience. Build it with
# `cargo build --release -p orrery_seed --features orrery_seed/fdb`.
ORRERY_SEED_BIN=${ORRERY_SEED_BIN:-"$(pwd)/target/release/orrery-seed"}
P2_SCENARIO=${P2_SCENARIO:-"$(pwd)/crates/orrery_seed/scenarios/p2demo.toml"}
P2_SEED_PROFILE=${P2_SEED_PROFILE:-demo}
[[ -r $ORRERY_FDB_CLUSTER_FILE ]] || die "FDB cluster file is not readable: $ORRERY_FDB_CLUSTER_FILE"
for tool in "$PERSISTD_BIN" "$P2_LOAD_BIN" "$P2_DASHBOARD_BIN" "$ORRERY_SEED_BIN"; do
  [[ -x $tool ]] || die "not an executable: $tool"
done
[[ -r $P2_SCENARIO ]] || die "seed scenario is not readable: $P2_SCENARIO"

# This gate consumes its cluster. `activate_shards` bumps `actor/{shard}` on
# every activation and `start_primary` asserts `--chain-epoch 1` against the
# epoch that bump produces, so a second run against the same FDB cannot get
# past startup — it dies with `--chain-epoch 1 is an assertion; FDB fence
# activation would produce epoch 2`, or, if the promoted node still owns the
# shard, `owned by node 2 at epoch 2`. Both are true statements about the
# cluster that read like a defect in the code under test; that misreading cost
# a full debugging pass on 2026-08-17. CI gets a fresh cluster per job and
# never sees this. Advisory only: without `fdbcli` the run proceeds exactly as
# it did before.
refuse_an_already_activated_cluster() {
  command -v fdbcli >/dev/null 2>&1 || {
    note 'fdbcli not on PATH; skipping the fresh-cluster pre-flight'
    return 0
  }
  local rows
  rows=$(timeout 30 fdbcli -C "$ORRERY_FDB_CLUSTER_FILE" --exec 'getrangekeys a b 1' 2>/dev/null \
    | grep -c '^`') || true
  [[ ${rows:-0} -eq 0 ]] || die "this cluster already carries an actor/ activation row from an earlier run, and the primary below asserts --chain-epoch 1 against a fence that only ever moves forward; point ORRERY_FDB_CLUSTER_FILE at a fresh cluster"
}
refuse_an_already_activated_cluster

out=${P2_GATE_OUT:-"$(pwd)/p2-kill9-$(date -u +%Y%m%dT%H%M%SZ)"}
[[ ! -e $out ]] || die "refusing to overwrite existing output directory: $out"
# Where the *journals* live, separable from where the evidence lives.
#
# docs/08-persistence.md §4.4 measured this run's own output as a contaminant
# of the number the run reports: the gate writes `acks.jsonl` (~110 MB),
# `telemetry.jsonl` and its stdout into this tree at ~4.7 MB/s, and buffered
# writeback at that rate reproduces barrier stalls of exactly the size and
# rarity that set `journal_commit_ms` p99 -- where the same barriers without it
# top out at 0.46 ms. Until the two are separable, no gate number is taken with
# the journal's device to itself.
#
# Defaults to `$out`, so an unset variable reproduces every previous run
# exactly. Pointing it at another filesystem is what §4.4's first follow-up
# asks for.
data=${P2_GATE_DATA_DIR:-"$out"}
[[ $data == "$out" ]] || mkdir -p "$data"
mkdir -p "$out" "$data/primary-data" "$data/follower-data"
# `SIGKILL` can land before a reporter's first tick.  Pre-create the files so
# the merge step remains deterministic; the dashboard still rejects the run
# if this leaves `journal_commit_ms` without samples.
: >"$out/primary-metrics.jsonl"
: >"$out/follower-metrics.jsonl"
: >"$out/promoted-metrics.jsonl"
: >"$out/zombie-metrics.jsonl"
# The gateway's transport-boundary spans (`gateway_ingress_queue_ms`,
# `gateway_reply_handoff_ms`, `gateway_send_buffer_bytes`) do not ride
# `--metrics-jsonl`: that reporter lives in persistd's binary, which was frozen
# to another lane when the boundary was instrumented, so the gateway writes
# them to its own sink instead (`ORRERY_GATEWAY_BOUNDARY_JSONL`). The records
# are the same `sample_batch` contract, and they are merged into the same
# telemetry stream below. When the drain moves into persistd's reporter, delete
# these two files, the two env assignments and the two merge entries.
: >"$out/primary-boundary.jsonl"
: >"$out/promoted-boundary.jsonl"

# Use explicit, non-overlapping ports to make logs and a failed rerun easy to
# diagnose.  The defaults are only for a dedicated local P2 runner.
gateway_port=${P2_GATE_PORT:-7777}
chain_port=${P2_GATE_CHAIN_PORT:-7778}
zombie_port=${P2_GATE_ZOMBIE_PORT:-7779}
fork_port=${P2_GATE_FORK_PORT:-7780}
[[ $gateway_port =~ ^[0-9]+$ && $chain_port =~ ^[0-9]+$ && $zombie_port =~ ^[0-9]+$ && $fork_port =~ ^[0-9]+$ ]] || die 'ports must be numeric'
secret_primary=${P2_GATE_PRIMARY_SECRET_KEY:-000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f}
secret_follower=${P2_GATE_FOLLOWER_SECRET_KEY:-101112131415161718191a1b1c1d1e1f000102030405060708090a0b0c0d0e0f}
# The identity issuer. Its public half is what the gateway trusts and its
# private half is what the rig signs its session token with, so the two come
# from one place — a secret here and a derivation below — rather than a pair of
# constants that can silently drift apart.
secret_issuer=${P2_GATE_ISSUER_SECRET_KEY:-202122232425262728292a2b2c2d2e2f303132333435363738393a3b3c3d3e3f}
issuer_key_id=${P2_GATE_ISSUER_KEY_ID:-1}
duration=${P2_GATE_DURATION_SECS:-30}
# Entity count and cell coverage now come from the seeded scenario
# (`P2_SEED_PROFILE` selects the rung: `demo` is the 10 000-entity P2 criterion,
# `ci` is the same topology 100x smaller). They are no longer rig arguments:
# the rig must claim leases at the cells the seeder actually committed.
sessions=${P2_GATE_SESSIONS:-125}

primary_pid=''
follower_pid=''
zombie_pid=''
# The shard set every persistd below owns. Derived from the seeded manifest by
# `derive_shard_set`; never a literal, because the number of shards is a
# property of the scenario, not of this script.
shard_flags=()
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

# Derive an ed25519 public key from a 32-byte secret, hex in and hex out.
#
# `iroh::SecretKey::public()` is plain ed25519, and `--issuer-key` wants the
# public half in the same hex a `NodeId` parses from. No binary this job builds
# will derive one — `p3-island` has `--print-keys`, but that is a different
# tool in a different workspace — so the derivation happens here, through
# `openssl`, which is the one dependency both a GitHub-hosted runner and the
# self-hosted box are certain to have.
#
# The known-answer check is not decoration. A silently wrong public key starts
# `persistd` happily and then fails the run minutes later as an unanswered
# hello, which reads like a registrar defect and is not one.
public_key_of() {
  python3 - "$1" <<'PY'
import binascii, subprocess, sys

PKCS8_ED25519_PREFIX = binascii.unhexlify('302e020100300506032b657004220420')
SPKI_ED25519_PREFIX = binascii.unhexlify('302a300506032b6570032100')


def public_of(secret_hex):
    seed = binascii.unhexlify(secret_hex.strip())
    if len(seed) != 32:
        raise SystemExit('an issuer secret is 32 hex-encoded bytes')
    der = subprocess.run(
        ['openssl', 'pkey', '-inform', 'DER', '-pubout', '-outform', 'DER'],
        input=PKCS8_ED25519_PREFIX + seed,
        capture_output=True,
        check=True,
    ).stdout
    if not der.startswith(SPKI_ED25519_PREFIX) or len(der) != len(SPKI_ED25519_PREFIX) + 32:
        raise SystemExit('openssl returned an unexpected ed25519 public key encoding')
    return binascii.hexlify(der[len(SPKI_ED25519_PREFIX):]).decode()


# RFC 8032 section 7.1, test vector 1.
if public_of('9d61b19deffd5a60ba844af492ec2cc44449c5697b326919703bac031cae7f60') != (
    'd75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a'
):
    raise SystemExit('ed25519 derivation failed its RFC 8032 known-answer check')

print(public_of(sys.argv[1]))
PY
}
issuer_public=$(public_key_of "$secret_issuer") || die 'could not derive the identity issuer public key'
note "identity issuer $issuer_key_id@$issuer_public"

start_follower() {
  "$PERSISTD_BIN" --node-id 2 --chain-epoch 1 --chain-primary 1 "${shard_flags[@]}" \
    --chain-listen "127.0.0.1:$chain_port" --dir "$data/follower-data" \
    --metrics-jsonl "$out/follower-metrics.jsonl" \
    >"$out/follower.json" 2>"$out/follower.stderr" & follower_pid=$!
  wait_json "$out/follower.json" "$follower_pid" follower
  follower_chain=$(json_field "$out/follower.json" chain_addr)
}
start_primary() {
  ORRERY_GATEWAY_BOUNDARY_JSONL="$out/primary-boundary.jsonl" \
  "$PERSISTD_BIN" --node-id 1 --chain-epoch 1 --chain-follower "2@$follower_chain" \
    "${shard_flags[@]}" \
    --bind "127.0.0.1:$gateway_port" --dir "$data/primary-data" \
    --secret-key "$secret_primary" --fdb-cluster-file "$ORRERY_FDB_CLUSTER_FILE" \
    --issuer-key "$issuer_key_id@$issuer_public" \
    --metrics-jsonl "$out/primary-metrics.jsonl" >"$out/primary.json" 2>"$out/primary.stderr" & primary_pid=$!
  wait_json "$out/primary.json" "$primary_pid" primary
  primary_gateway=$(json_field "$out/primary.json" node_id)
  primary_addr=$(json_field "$out/primary.json" bind_addr)
}
prove_epoch_fork_refused() {
  # The primary-restart leg. `activate_shards` bumps the ownership epoch on
  # every activation, an ordinary clean restart of the same owner included,
  # and the epoch is part of `DurableChainId` — which keys the follower's
  # durable dedupe index. A follower reopened at the bumped epoch therefore
  # used to rebuild an empty cursor and take a silent full re-stream into a
  # second physical copy of every mirrored record, reporting a healthy
  # zero-byte lag throughout and leaving promotion permanently ambiguous.
  #
  # This runs in the one window where nothing holds the mirror open: the
  # passive follower has been stopped and the promoted instance has not
  # started. It is a *passive* follower start, not a promotion, so it takes
  # neither `--promote-from` nor an FDB cluster file.
  note 'proving a bumped chain epoch is refused, not forked'
  if timeout 120 "$PERSISTD_BIN" --node-id 2 --chain-epoch 2 --chain-primary 1 \
    "${shard_flags[@]}" \
    --chain-listen "127.0.0.1:$fork_port" --dir "$data/follower-data" \
    >"$out/epoch-fork.json" 2>"$out/epoch-fork.stderr"; then
    die 'follower accepted a bumped chain epoch on an already-mirrored journal'
  fi
  # A refusal happens before the readiness line. A follower that printed one
  # opened the mirror, accepted the bumped epoch, and then sat there serving it
  # until `timeout` killed it — which leaves the same non-zero status a refusal
  # does. Without this check the two are indistinguishable and the run reports
  # the weaker `not as a chain-epoch fork` instead of what actually happened.
  if [[ -s $out/epoch-fork.json ]]; then
    die 'follower accepted a bumped chain epoch: it opened the mirror and served it until the harness killed it'
  fi
  grep -Fq 'restart handshake' "$out/epoch-fork.stderr" \
    || die 'follower rejected the bumped epoch, but not as a chain-epoch fork'
}
start_promoted_follower() {
  # The follower process was passive and is deliberately stopped before
  # promotion: the promoted instance adopts the same on-disk mirror.
  kill -TERM "$follower_pid"; wait "$follower_pid" || true; follower_pid=''
  prove_epoch_fork_refused
  ORRERY_GATEWAY_BOUNDARY_JSONL="$out/promoted-boundary.jsonl" \
  "$PERSISTD_BIN" --node-id 2 --chain-epoch 2 --chain-primary 1 --promote-from 1 \
    "${shard_flags[@]}" \
    --chain-listen "127.0.0.1:$chain_port" --bind "127.0.0.1:$gateway_port" \
    --dir "$data/follower-data" --secret-key "$secret_follower" \
    --issuer-key "$issuer_key_id@$issuer_public" \
    --fdb-cluster-file "$ORRERY_FDB_CLUSTER_FILE" --metrics-jsonl "$out/promoted-metrics.jsonl" \
    >"$out/promoted.json" 2>"$out/promoted.stderr" & follower_pid=$!
  wait_json "$out/promoted.json" "$follower_pid" promoted-follower
  promoted_gateway=$(json_field "$out/promoted.json" node_id)
  promoted_addr=$(json_field "$out/promoted.json" bind_addr)
  recovery_cutoff=$(json_field "$out/promoted.json" recovery_cutoff)
}

# Seed the durable world, then take its manifest as the rig's inventory.
#
# This is not a convenience: the gateway fences every bulk write
# (`strict_authority: true`), a fenced write needs a lease, and the registrar
# only grants a lease for an entity whose *committed cell* it can already
# resolve. An unseeded world is therefore unwritable, which is exactly how a
# 30-second run produced 541 408 rejections and zero journal appends while the
# summary line said nothing about it.
seed_world() {
  note "seeding the durable world from $(basename "$P2_SCENARIO") (profile $P2_SEED_PROFILE)"
  ORRERY_FDB_CLUSTER_FILE="$ORRERY_FDB_CLUSTER_FILE" "$ORRERY_SEED_BIN" apply "$P2_SCENARIO" \
    --profile "$P2_SEED_PROFILE" --allow-opaque --single-grid \
    >"$out/seed-apply.log" 2>&1 || die "seeding failed; see $out/seed-apply.log"
  ORRERY_FDB_CLUSTER_FILE="$ORRERY_FDB_CLUSTER_FILE" "$ORRERY_SEED_BIN" verify "$P2_SCENARIO" \
    --profile "$P2_SEED_PROFILE" --single-grid --emit-manifest "$out/manifest.json" \
    >"$out/seed-verify.log" 2>&1 || die "seed verification failed; see $out/seed-verify.log"
  [[ -s $out/manifest.json ]] || die 'seeder emitted no manifest'
}

# The deployment the criterion describes, derived from the world just seeded.
#
# persistd's `resolve_shards` turns an absent `--shard` into `vec![CellId::ROOT]`
# — one shard, and therefore *one* single-writer cell actor for the entire
# world. That is what this harness used to ask for, and the measured effect on
# the 10 000-entity demo profile was a mailbox queue, not a durability problem:
# per acknowledged diff, `router_apply` 721 ms of a 723 ms total (99.7 %) with
# `journal_wait` at 1.5 ms and `journal_commit_ms` averaging 1.03 ms against a
# 2 ms D16 budget. The registrar then withdrew 8 760 of 10 000 leases because
# their holders could not be served inside the lease term.
#
# The designed deployment (docs/11-roadmap.md §P2, docs/08-persistence.md §3.1)
# is one actor per level-`SHARD_LEVEL` cell, and `orrery_protocol::shard_of`
# is the canonical collapse from an entity's interest cell to its shard. The
# seeder owns the manifest format, so the collapse is a seeder subcommand
# (`orrery-seed shards`) rather than a `jq` expression here: reimplementing
# `ancestor_at(SHARD_LEVEL)` in shell would duplicate a packed-bit encoding,
# and a shard set that is subtly wrong does not fail loudly — it leaves part of
# the world addressed to an actor no process owns.
#
# Nothing here is a constant: the demo profile spans 128 shards today and a
# scenario edit moves that without touching this script.
derive_shard_set() {
  local list=$out/shard-set.txt
  "$ORRERY_SEED_BIN" shards "$out/manifest.json" --grid 0 >"$list" \
    || die "could not derive the shard set from $out/manifest.json"
  mapfile -t shards <"$list"
  [[ ${#shards[@]} -gt 0 ]] || die 'the seeded manifest collapsed to an empty shard set'
  shard_flags=()
  local shard
  for shard in "${shards[@]}"; do
    shard_flags+=(--shard "$shard")
  done
  note "shard set derived from the seeded manifest: ${#shards[@]} shard(s)"
}

seed_world
derive_shard_set
note "starting passive follower"
start_follower
note "starting fenced primary"
start_primary
note "driving the seeded inventory (${sessions} sessions, ${duration}s)"
# Inventory comes from the manifest, not `--entities`/`--cells`: the rig claims
# a lease per entity at the cell the seeder committed it to, and a synthesized
# placement would name cells the registrar cannot resolve.
#
# A claim-phase failure here is reported against the *gateway*, and the rig's
# advice ("seed the entity durably") is already satisfied by `seed_world` — so
# name the seam it is actually about. Measured 2026-08-17: the seeder writes
# `world/` rows and no `ckpt/` row by design, `FdbCheckpointStore::load`
# returns `None` without that row, the primary recovers an empty bag, and
# `committed_entity_cell` cannot resolve an entity the cluster demonstrably
# holds. Every claim is then `NotEligible` and the run has nothing to make
# durable (docs/08-persistence.md §3.4, docs/13-chain-replication.md §3.1).
if ! "$P2_LOAD_BIN" --gateway "$primary_gateway" --addr "$primary_addr" \
  --manifest "$out/manifest.json" --sessions "$sessions" --duration-secs "$duration" \
  --issuer-secret "$secret_issuer" --issuer-key-id "$issuer_key_id" \
  --json --ack-log "$out/acks.jsonl" >"$out/load-before.jsonl" 2>"$out/load-before.stderr"; then
  if grep -Fq 'NotEligible' "$out/load-before.stderr"; then
    die "the rig could not claim a lease on a seeded entity: the primary recovered no world state, so the registrar cannot resolve the entity's committed cell. See $out/load-before.stderr and docs/08-persistence.md §3.4"
  fi
  die "the load rig failed; see $out/load-before.stderr"
fi
# A non-empty ack log is not evidence. It was 1024 lines of rejected intents on
# the run this check was supposed to catch. Count the durable bulk records —
# p2-load writes a `"type":"diff"` line only for a non-provisional `BulkAck` —
# and require at least one.
durable_acks=$(grep -c '"type":"diff"' "$out/acks.jsonl" || true)
[[ ${durable_acks:-0} -gt 0 ]] \
  || die "load produced no durable bulk acknowledgement (ack log has $(wc -l <"$out/acks.jsonl") records, none of them a durable diff); see $out/load-before.stderr"
note "durable bulk acknowledgements before the kill: $durable_acks"

note 'SIGKILL primary and promote follower'
kill -KILL "$primary_pid"; wait "$primary_pid" 2>/dev/null || true; primary_pid=''
start_promoted_follower

# The verifier reads materialized bulk state through the promoted gateway and
# intent idempotency rows directly from FDB.  Its cutoff binds comparison to
# the chain prefix actually adopted during promotion, so a post-cutoff ack is
# never silently demanded from an asynchronous mirror.
#
# Deliberately no `--manifest` here, and adding one would be a regression. The
# verifier re-reads each entity's leaf at the cell its *acknowledgement* names,
# which is the claim under proof; it used to reload the rig's inventory and,
# absent a manifest, synthesise a 128-cell lattice instead. Both lattices sit
# at INTEREST_LEVEL and a level-21 request matches only itself, so every leaf
# read landed on an empty cell: measured 2026-08-17, 99 of 100 durable entities
# reported MissingBulk against a promoted node that held all 100.
"$P2_LOAD_BIN" --verify-recovery --ack-log "$out/acks.jsonl" \
  --gateway "$promoted_gateway" --addr "$promoted_addr" \
  --issuer-secret "$secret_issuer" --issuer-key-id "$issuer_key_id" \
  --fdb-cluster-file "$ORRERY_FDB_CLUSTER_FILE" --recovery-cutoff "$recovery_cutoff" \
  --output "$out/recovery-verification.json"

# A stale owner must fail admission before it can open a gateway.  This is a
# stronger check than merely observing the old PID dead: it proves the FDB
# actor fence rejects a fresh process carrying the old owner identity.
note 'proving old primary is fenced (zombie admission)'
"$PERSISTD_BIN" --node-id 1 --chain-epoch 1 --chain-follower "2@$follower_chain" \
  "${shard_flags[@]}" \
  --bind "127.0.0.1:$zombie_port" --dir "$data/primary-data" \
  --secret-key "$secret_primary" --fdb-cluster-file "$ORRERY_FDB_CLUSTER_FILE" \
  --issuer-key "$issuer_key_id@$issuer_public" \
  --metrics-jsonl "$out/zombie-metrics.jsonl" >"$out/zombie.json" 2>"$out/zombie.stderr" & zombie_pid=$!
if wait "$zombie_pid"; then
  zombie_pid=''
  die 'zombie primary unexpectedly passed startup admission'
fi
zombie_pid=''
grep -Eqi 'fence|owner|activation|epoch' "$out/zombie.stderr" || die 'zombie failed, but not with recognizable fence admission evidence'

# Keep all raw telemetry, then gate the merged evidence in one invocation.
cat "$out/load-before.jsonl" "$out/primary-metrics.jsonl" "$out/promoted-metrics.jsonl" \
    "$out/primary-boundary.jsonl" "$out/promoted-boundary.jsonl" >"$out/telemetry.jsonl"
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
# persistd's own spans ride this artifact under their own names
# (gateway_*_server_ms) and must stay attribution-only. If one ever arrives
# under a gated name it is folded into the client's histogram and lowers the
# p99 this gate reads -- the one way this harness can pass without having
# measured anything. The check is named so the offline self-test can find it.
def server_side_spans_never_gate(series):
    gated = {'journal_commit_ms', 'bulk_ack_ms', 'intent_commit_ms', 'area_first_page_ms'}
    for name, summary in series.items():
        # Every `gateway_*` name, not only the `_server_ms` ones: the
        # transport-boundary spans (`gateway_ingress_queue_ms`,
        # `gateway_reply_handoff_ms`, `gateway_send_buffer_bytes`) are
        # server-internal for exactly the same reason and would corrupt a
        # gated histogram in exactly the same way.
        server_span = name.startswith('gateway_')
        if server_span and (name in gated or summary.get('gate') != 'not_gated'):
            raise SystemExit(f'server-internal span {name} is being gated')
        if name in gated and summary.get('gate') == 'not_gated':
            raise SystemExit(f'gated series {name} lost its threshold')
server_side_spans_never_gate(l.get('series') or {})
artifact.write_text(json.dumps({
  'kind':'p2_two_process_kill9_gate',
  'created_at':datetime.datetime.now(datetime.timezone.utc).isoformat(),
  'result':'pass', 'recovery_cutoff':cutoff,
  'proofs': {'recovery': v, 'latency': l, 'zombie_primary_fenced': True,
             'bumped_chain_epoch_refused': True},
}, indent=2) + '\n')
PY
note "PASS artifact: $out/artifact.json"
