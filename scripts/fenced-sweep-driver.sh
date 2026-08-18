#!/usr/bin/env bash
# Drive scripts/p2-capacity-sweep.sh over a list of points, interleaving the
# before/after binaries so a run-to-run swing on this box (2-4x on per-flush
# fsync cost) cannot be mistaken for the change.
#
# Usage: fenced-sweep-driver.sh <repeat> <point> [point...]
#   point := <label>:<sessions>:<diff_hz>[:<duration>]
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
: "${P2_CAP_OUT:?}" "${ORRERY_FDB_CLUSTER_FILE:?}" "${P2_LOAD_BIN:?}" "${FDB_PID:?}"
: "${BEFORE_BIN:?}" "${AFTER_BIN:?}" "${SEED_BIN:?}"

repeat=${1:?repeat count}; shift
for r in $(seq 1 "$repeat"); do
  for point in "$@"; do
    IFS=: read -r label sessions hz duration <<<"$point"
    duration=${duration:-30}
    for arm in before after; do
      bin=$([[ $arm == before ]] && echo "$BEFORE_BIN" || echo "$AFTER_BIN")
      out="$arm-$label-r$r"
      if [[ -d $P2_CAP_OUT/$out ]]; then continue; fi
      echo "=== $out (sessions=$sessions hz=$hz ${duration}s)" >&2
      PERSISTD_BIN="$bin" ORRERY_SEED_BIN="$SEED_BIN" \
        "$ROOT/scripts/p2-capacity-sweep.sh" "$out" "$sessions" "$hz" "$duration" \
        >>"$P2_CAP_OUT/driver.log" 2>&1 || echo "!! $out failed" >&2
    done
  done
done
