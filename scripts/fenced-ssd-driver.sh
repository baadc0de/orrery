#!/usr/bin/env bash
# Drive scripts/p2-capacity-sweep.sh over a list of points against TWO private
# FoundationDB clusters that differ only in storage engine — one `configure
# new single ssd`, one `configure new single memory` — interleaving the arms
# so this box's tens-of-seconds fsync-cost regime (docs/08 §4.3, docs/14 §7)
# cannot be mistaken for the engine.
#
# It is the engine-arm sibling of fenced-sweep-driver.sh, whose arms are two
# persistd binaries instead. Same output layout (`<arm>-<label>-r<repeat>`), so
# scripts/fenced-sweep-report.py folds either.
#
# Usage: fenced-ssd-driver.sh <repeat> <point> [point...]
#   point := <label>:<sessions>:<diff_hz>[:<duration>[:<intent_mix>]]
#
# Required env, per arm:
#   SSD_CLUSTER_FILE MEM_CLUSTER_FILE    cluster files of the two clusters
#   SSD_FDB_CONTAINER MEM_FDB_CONTAINER  container names (for the fdbserver PID)
# plus P2_CAP_OUT, PERSISTD_BIN, P2_LOAD_BIN, ORRERY_SEED_BIN as in
# p2-capacity-sweep.sh. Both arms run the SAME persistd binary: the storage
# engine is the only variable.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
: "${P2_CAP_OUT:?}" "${P2_LOAD_BIN:?}" "${PERSISTD_BIN:?}" "${ORRERY_SEED_BIN:?}"
: "${SSD_CLUSTER_FILE:?}" "${MEM_CLUSTER_FILE:?}"
: "${SSD_FDB_CONTAINER:?}" "${MEM_FDB_CONTAINER:?}"

pid_of() { docker top "$1" | awk '/fdbserver/{print $2}' | head -1; }
SSD_FDB_PID=$(pid_of "$SSD_FDB_CONTAINER")
MEM_FDB_PID=$(pid_of "$MEM_FDB_CONTAINER")
[[ -n $SSD_FDB_PID && -n $MEM_FDB_PID ]] || { echo "no fdbserver pid" >&2; exit 2; }

repeat=${1:?repeat count}; shift
for r in $(seq 1 "$repeat"); do
  for point in "$@"; do
    IFS=: read -r label sessions hz duration mix <<<"$point"
    duration=${duration:-30}
    # Arm order alternates with the repeat so neither arm is always the one
    # that runs first into a cold page cache or a fresh device regime.
    arms=(ssd memory)
    if (( r % 2 == 0 )); then arms=(memory ssd); fi
    for arm in "${arms[@]}"; do
      if [[ $arm == ssd ]]; then
        cluster=$SSD_CLUSTER_FILE; fdbpid=$SSD_FDB_PID
      else
        cluster=$MEM_CLUSTER_FILE; fdbpid=$MEM_FDB_PID
      fi
      out="$arm-$label-r$r"
      if [[ -d $P2_CAP_OUT/$out ]]; then continue; fi
      echo "=== $out (sessions=$sessions hz=$hz ${duration}s mix=${mix:-default})" >&2
      env ORRERY_FDB_CLUSTER_FILE="$cluster" FDB_PID="$fdbpid" \
        ${mix:+P2_CAP_INTENT_MIX="$mix"} \
        "$ROOT/scripts/p2-capacity-sweep.sh" "$out" "$sessions" "$hz" "$duration" \
        >>"$P2_CAP_OUT/driver.log" 2>&1 || echo "!! $out failed" >&2
    done
  done
done
