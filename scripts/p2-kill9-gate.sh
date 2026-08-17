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
  has 'start_follower' || die 'self-test: follower startup absent'
  has '--promote-from' || die 'self-test: promotion absent'
  has 'verify-recovery' || die 'self-test: recovery verifier absent'
  has 'zombie' || die 'self-test: zombie fence proof absent'
  has 'prove_epoch_fork_refused' || die 'self-test: primary-restart epoch-fork proof absent'
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
    --chain-listen "127.0.0.1:$fork_port" --dir "$out/follower-data" \
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
  "$PERSISTD_BIN" --node-id 2 --chain-epoch 2 --chain-primary 1 --promote-from 1 \
    --chain-listen "127.0.0.1:$chain_port" --bind "127.0.0.1:$gateway_port" \
    --dir "$out/follower-data" --secret-key "$secret_follower" \
    --issuer-key "$issuer_key_id@$issuer_public" \
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
  --issuer-secret "$secret_issuer" --issuer-key-id "$issuer_key_id" \
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
  --issuer-secret "$secret_issuer" --issuer-key-id "$issuer_key_id" \
  --fdb-cluster-file "$ORRERY_FDB_CLUSTER_FILE" --recovery-cutoff "$recovery_cutoff" \
  --output "$out/recovery-verification.json"

# A stale owner must fail admission before it can open a gateway.  This is a
# stronger check than merely observing the old PID dead: it proves the FDB
# actor fence rejects a fresh process carrying the old owner identity.
note 'proving old primary is fenced (zombie admission)'
"$PERSISTD_BIN" --node-id 1 --chain-epoch 1 --chain-follower "2@$follower_chain" \
  --bind "127.0.0.1:$zombie_port" --dir "$out/primary-data" \
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
  'proofs': {'recovery': v, 'latency': l, 'zombie_primary_fenced': True,
             'bumped_chain_epoch_refused': True},
}, indent=2) + '\n')
PY
note "PASS artifact: $out/artifact.json"
