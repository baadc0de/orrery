#!/usr/bin/env bash
# Diagnostic ABBA runner for comparing two p2-load binaries against one
# identical, chain-enabled persistd build. This deliberately stops before
# promotion: it isolates the live durability/ack path and preserves every raw
# system and journal-stage signal under the output directory.
set -euo pipefail

: "${ORRERY_FDB_CLUSTER_FILE:?set ORRERY_FDB_CLUSTER_FILE}"
: "${PERSISTD_BIN:?set PERSISTD_BIN}"
: "${P2_LOAD_OLD_BIN:?set P2_LOAD_OLD_BIN}"
: "${P2_LOAD_CURRENT_BIN:?set P2_LOAD_CURRENT_BIN}"

out=${P2_ABBA_OUT:-"$(pwd)/p2-abba-$(date -u +%Y%m%dT%H%M%SZ)"}
sets=${P2_ABBA_SETS:-3}
duration=${P2_ABBA_DURATION_SECS:-10}
base_port=${P2_ABBA_PORT:-17800}
mkdir -p "$out"

primary_pid=''
follower_pid=''
iostat_pid=''
pidstat_pid=''
cleanup() {
  for pid in "$primary_pid" "$follower_pid" "$iostat_pid" "$pidstat_pid"; do
    [[ -n $pid ]] && kill "$pid" 2>/dev/null || true
  done
}
trap cleanup EXIT

sha256sum "$PERSISTD_BIN" "$P2_LOAD_OLD_BIN" "$P2_LOAD_CURRENT_BIN" >"$out/binaries.sha256"
git rev-parse HEAD >"$out/repository-head.txt"
uname -a >"$out/uname.txt"

trial=0
for ((set=1; set<=sets; set++)); do
  for variant in old current current old; do
    trial=$((trial + 1))
    trial_dir="$out/$(printf '%02d-set%d-%s' "$trial" "$set" "$variant")"
    mkdir -p "$trial_dir/primary-data" "$trial_dir/follower-data"
    gateway_port=$((base_port + trial * 3))
    chain_port=$((gateway_port + 1))
    load_bin=$P2_LOAD_CURRENT_BIN
    [[ $variant == old ]] && load_bin=$P2_LOAD_OLD_BIN

    fdbcli -C "$ORRERY_FDB_CLUSTER_FILE" --exec 'writemode on; clearrange \x00 \xff' \
      >"$trial_dir/fdb-clear.txt" 2>&1
    cp /proc/pressure/cpu "$trial_dir/cpu-pressure-before.txt"
    cp /proc/pressure/io "$trial_dir/io-pressure-before.txt"

    "$PERSISTD_BIN" --node-id 2 --chain-epoch 1 --chain-primary 1 \
      --chain-listen "127.0.0.1:$chain_port" --dir "$trial_dir/follower-data" \
      --metrics-jsonl "$trial_dir/follower-metrics.jsonl" \
      >"$trial_dir/follower.json" 2>"$trial_dir/follower.stderr" &
    follower_pid=$!
    for _ in $(seq 1 300); do
      [[ -s $trial_dir/follower.json ]] && break
      kill -0 "$follower_pid" 2>/dev/null || break
      sleep .1
    done
    [[ -s $trial_dir/follower.json ]]

    "$PERSISTD_BIN" --node-id 1 --chain-epoch 1 \
      --chain-follower "2@127.0.0.1:$chain_port" --bind "127.0.0.1:$gateway_port" \
      --dir "$trial_dir/primary-data" \
      --secret-key 000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f \
      --fdb-cluster-file "$ORRERY_FDB_CLUSTER_FILE" \
      --metrics-jsonl "$trial_dir/primary-metrics.jsonl" \
      >"$trial_dir/primary.json" 2>"$trial_dir/primary.stderr" &
    primary_pid=$!
    for _ in $(seq 1 300); do
      [[ -s $trial_dir/primary.json ]] && break
      kill -0 "$primary_pid" 2>/dev/null || break
      sleep .1
    done
    [[ -s $trial_dir/primary.json ]]

    gateway_id=$(sed -n 's/.*"node_id":"\([^"]*\)".*/\1/p' "$trial_dir/primary.json")
    iostat -dx 1 >"$trial_dir/iostat.txt" & iostat_pid=$!
    pidstat -durw -p "$primary_pid,$follower_pid" 1 >"$trial_dir/pidstat.txt" & pidstat_pid=$!

    set +e
    "$load_bin" --gateway "$gateway_id" --addr "127.0.0.1:$gateway_port" \
      --entities 10000 --cells 128 --sessions 125 --duration-secs "$duration" \
      --json --ack-log "$trial_dir/acks.jsonl" \
      >"$trial_dir/load.jsonl" 2>"$trial_dir/load.stderr"
    load_status=$?
    set -e
    echo "$load_status" >"$trial_dir/load.exit"

    kill -TERM "$primary_pid" "$follower_pid" 2>/dev/null || true
    wait "$primary_pid" 2>/dev/null || true
    wait "$follower_pid" 2>/dev/null || true
    kill "$iostat_pid" "$pidstat_pid" 2>/dev/null || true
    wait "$iostat_pid" 2>/dev/null || true
    wait "$pidstat_pid" 2>/dev/null || true
    primary_pid=''; follower_pid=''; iostat_pid=''; pidstat_pid=''
    cp /proc/pressure/cpu "$trial_dir/cpu-pressure-after.txt"
    cp /proc/pressure/io "$trial_dir/io-pressure-after.txt"
    printf '%s\n' "$variant" >"$trial_dir/variant.txt"
    printf '%s\n' "$load_bin" >"$trial_dir/load-binary.txt"
  done
done

printf '%s\n' "$out"
