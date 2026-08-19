#!/usr/bin/env bash
# One point of the P2 capacity sweep: seed a world, run persistd (primary +
# passive follower, the p2-kill9 topology minus the crash legs), drive it with
# p2-load at a given (--sessions, --diff-hz), and record per-process CPU for
# persistd, the fdbserver container and the rig separately.
#
# This is a measurement harness, not a gate: nothing here asserts. It exists
# because the capacity question ("how much offered load does this box absorb
# before it stops keeping up") needs the rig's own CPU share subtracted, and
# no existing script splits that out.
#
# Usage:
#   P2_CAP_OUT=<dir> ORRERY_FDB_CLUSTER_FILE=... PERSISTD_BIN=... P2_LOAD_BIN=...
#   ORRERY_SEED_BIN=... FDB_PID=<pid of the fdbserver under test>
#     scripts/p2-capacity-sweep.sh <label> <sessions> <diff_hz> [duration_secs]
set -euo pipefail
NAME=p2-capacity-sweep
die() { echo "$NAME: $*" >&2; exit 2; }
note() { echo "$NAME: $*" >&2; }

label=${1:?label}; sessions=${2:?sessions}; diff_hz=${3:?diff_hz}; duration=${4:-30}
: "${ORRERY_FDB_CLUSTER_FILE:?}" "${PERSISTD_BIN:?}" "${P2_LOAD_BIN:?}" "${ORRERY_SEED_BIN:?}"
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SCENARIO=${P2_SCENARIO:-"$ROOT/crates/orrery_seed/scenarios/p2demo.toml"}
PROFILE=${P2_SEED_PROFILE:-demo}
base=${P2_CAP_OUT:?set P2_CAP_OUT}
out="$base/$label"
[[ ! -e $out ]] || die "refusing to overwrite $out"
mkdir -p "$out/primary-data" "$out/follower-data"

gateway_port=${P2_CAP_PORT:-17911}
chain_port=$((gateway_port + 1))
secret_primary=000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f
secret_issuer=202122232425262728292a2b2c2d2e2f303132333435363738393a3b3c3d3e3f
issuer_key_id=1

primary_pid=''; follower_pid=''; load_pid=''; pidstat_pid=''; iostat_pid=''; vmstat_pid=''; threadstat_pid=''
cleanup() {
  for p in "$load_pid" "$primary_pid" "$follower_pid" "$pidstat_pid" "$iostat_pid" "$vmstat_pid" "$threadstat_pid"; do
    [[ -n $p ]] && kill "$p" 2>/dev/null || true
  done
}
trap cleanup EXIT

wait_json() {
  local file=$1 pid=$2 lbl=$3
  for _ in $(seq 1 1200); do
    [[ -s $file ]] && return 0
    kill -0 "$pid" 2>/dev/null || die "$lbl exited before readiness; see ${file%.json}.stderr"
    sleep .1
  done
  die "timed out waiting for $lbl"
}
json_field() { python3 -c 'import json,sys; print(json.loads(open(sys.argv[1]).readline())[sys.argv[2]])' "$1" "$2"; }
public_key_of() {
  python3 - "$1" <<'PY'
import binascii, subprocess, sys
P = binascii.unhexlify('302e020100300506032b657004220420')
S = binascii.unhexlify('302a300506032b6570032100')
seed = binascii.unhexlify(sys.argv[1].strip())
der = subprocess.run(['openssl', 'pkey', '-inform', 'DER', '-pubout', '-outform', 'DER'],
                     input=P + seed, capture_output=True, check=True).stdout
print(binascii.hexlify(der[len(S):]).decode())
PY
}
issuer_public=$(public_key_of "$secret_issuer")

# -- fresh cluster, seeded world ---------------------------------------------
fdbcli -C "$ORRERY_FDB_CLUSTER_FILE" --exec 'writemode on; clearrange \x00 \xff' >"$out/fdb-clear.txt" 2>&1
seed_start=$(date +%s)
ORRERY_FDB_CLUSTER_FILE="$ORRERY_FDB_CLUSTER_FILE" "$ORRERY_SEED_BIN" apply "$SCENARIO" \
  --profile "$PROFILE" --allow-opaque --single-grid >"$out/seed-apply.log" 2>&1 \
  || die "seeding failed; see $out/seed-apply.log"
ORRERY_FDB_CLUSTER_FILE="$ORRERY_FDB_CLUSTER_FILE" "$ORRERY_SEED_BIN" verify "$SCENARIO" \
  --profile "$PROFILE" --single-grid --emit-manifest "$out/manifest.json" \
  >"$out/seed-verify.log" 2>&1 || die "seed verify failed; see $out/seed-verify.log"
seed_end=$(date +%s)
"$ORRERY_SEED_BIN" shards "$out/manifest.json" --grid 0 >"$out/shard-set.txt" || die 'shard derivation failed'
mapfile -t shards <"$out/shard-set.txt"
shard_flags=(); for s in "${shards[@]}"; do shard_flags+=(--shard "$s"); done
note "$label: ${#shards[@]} shards, seeded in $((seed_end - seed_start))s"

: >"$out/primary-metrics.jsonl"; : >"$out/follower-metrics.jsonl"; : >"$out/primary-boundary.jsonl"

"$PERSISTD_BIN" --node-id 2 --chain-epoch 1 --chain-primary 1 "${shard_flags[@]}" \
  --chain-listen "127.0.0.1:$chain_port" --dir "$out/follower-data" \
  --metrics-jsonl "$out/follower-metrics.jsonl" \
  >"$out/follower.json" 2>"$out/follower.stderr" & follower_pid=$!
wait_json "$out/follower.json" "$follower_pid" follower
follower_chain=$(json_field "$out/follower.json" chain_addr)

ORRERY_GATEWAY_BOUNDARY_JSONL="$out/primary-boundary.jsonl" \
"$PERSISTD_BIN" --node-id 1 --chain-epoch 1 --chain-follower "2@$follower_chain" "${shard_flags[@]}" \
  --bind "127.0.0.1:$gateway_port" --dir "$out/primary-data" --secret-key "$secret_primary" \
  --fdb-cluster-file "$ORRERY_FDB_CLUSTER_FILE" --issuer-key "$issuer_key_id@$issuer_public" \
  --metrics-jsonl "$out/primary-metrics.jsonl" >"$out/primary.json" 2>"$out/primary.stderr" & primary_pid=$!
wait_json "$out/primary.json" "$primary_pid" primary
gateway=$(json_field "$out/primary.json" node_id)
addr=$(json_field "$out/primary.json" bind_addr)

# -- drive -------------------------------------------------------------------
cp /proc/pressure/cpu "$out/cpu-pressure-before.txt"; cp /proc/pressure/io "$out/io-pressure-before.txt"
# The rig shares this box with the thing it measures, which a real deployment
# would not. `P2_CAP_LOAD_CPUS` pins it to a CPU subset so the same offered
# load can be re-run with the rig's share of the 16 threads cut, which is how
# a knee that is really the rig's is told apart from one that is the box's.
load_prefix=()
[[ -n ${P2_CAP_LOAD_CPUS:-} ]] && load_prefix=(taskset -c "$P2_CAP_LOAD_CPUS")
# The rig's default intent mix is 3 % of diff sends, which at a 30 s point is
# ~1 000 intent samples — coarse enough that `intent_commit_ms` p99 lands on
# one histogram bucket boundary and moves in factor-of-two steps. A study that
# wants to say something defensible about intents raises it, and pays for it in
# bulk load: an upgraded send is an intent *instead of* a diff, not as well as.
intent_mix_flag=()
[[ -n ${P2_CAP_INTENT_MIX:-} ]] && intent_mix_flag=(--intent-mix "$P2_CAP_INTENT_MIX")
"${load_prefix[@]}" "$P2_LOAD_BIN" --gateway "$gateway" --addr "$addr" --manifest "$out/manifest.json" \
  --sessions "$sessions" --diff-hz "$diff_hz" --duration-secs "$duration" \
  "${intent_mix_flag[@]}" \
  --issuer-secret "$secret_issuer" --issuer-key-id "$issuer_key_id" --json \
  >"$out/load.jsonl" 2>"$out/load.stderr" & load_pid=$!
# Per-process CPU, sampled once a second for every process that competes for
# the box's 16 threads. The rig runs here, which a real deployment would not,
# so its share is measured rather than assumed.
pidstat -u -h -p "$primary_pid,$follower_pid,$load_pid${FDB_PID:+,$FDB_PID}" 1 >"$out/pidstat.txt" 2>&1 & pidstat_pid=$!
# Per-thread, for the primary only. "persistd is at 4 cores of 16" does not
# say whether one thread is pinned at 100% while the other fifteen idle, and
# that distinction is the difference between "add load" and "this will not go
# faster on this box".
pidstat -t -u -h -p "$primary_pid" 1 >"$out/pidstat-threads.txt" 2>&1 & threadstat_pid=$!
iostat -dx 1 >"$out/iostat.txt" 2>&1 & iostat_pid=$!
vmstat 1 >"$out/vmstat.txt" 2>&1 & vmstat_pid=$!
load_status=0
wait "$load_pid" || load_status=$?
load_pid=''
echo "$load_status" >"$out/load.exit"
cp /proc/pressure/cpu "$out/cpu-pressure-after.txt"; cp /proc/pressure/io "$out/io-pressure-after.txt"
for p in "$pidstat_pid" "$iostat_pid" "$vmstat_pid" "$threadstat_pid"; do kill "$p" 2>/dev/null || true; done
pidstat_pid=''; iostat_pid=''; vmstat_pid=''; threadstat_pid=''
du -sb "$out/primary-data" "$out/follower-data" >"$out/journal-bytes.txt" 2>/dev/null || true
kill -TERM "$primary_pid" "$follower_pid" 2>/dev/null || true
wait "$primary_pid" 2>/dev/null || true; wait "$follower_pid" 2>/dev/null || true
primary_pid=''; follower_pid=''
# ~1 GB of journal per run; every number is in the JSONL by now.
rm -rf "$out/primary-data" "$out/follower-data"
# `storage_engine` is recorded, not assumed: a point's cluster is whatever
# `ORRERY_FDB_CLUSTER_FILE` pointed at, and an engine-arm study that infers the
# engine from a directory name is one mislabelled run away from a wrong table.
storage_engine=$(fdbcli -C "$ORRERY_FDB_CLUSTER_FILE" --exec 'status json' 2>/dev/null \
  | python3 -c 'import json,sys
try: print(json.load(sys.stdin)["cluster"]["configuration"]["storage_engine"])
except Exception: print("unknown")' || echo unknown)
printf '{"label":"%s","sessions":%s,"diff_hz":%s,"duration_secs":%s,"load_exit":%s,"intent_mix":"%s","storage_engine":"%s"}\n' \
  "$label" "$sessions" "$diff_hz" "$duration" "$load_status" \
  "${P2_CAP_INTENT_MIX:-trade=0.02,craft=0.01}" "$storage_engine" >"$out/point.json"
note "$label done (load exit $load_status)"
