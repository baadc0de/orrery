#!/usr/bin/env bash
# Shared environment for the fdb-off-bulk-path capacity runs.
#
# A PRIVATE FoundationDB on its own port and data dir: the P2 path consumes
# its cluster (`activate_shards` bumps `actor/{shard}` epochs) and every point
# clears the keyspace, so it must never be the shared dev cluster on :4500.
# See docs/14-capacity.md §11 for how the cluster and the two binaries are
# built. Every path below is overridable; the defaults are the ones that
# study used.
: "${FENCED_ROOT:=$HOME}"
export ORRERY_FDB_CLUSTER_FILE=${ORRERY_FDB_CLUSTER_FILE:-$FENCED_ROOT/fdb-fenced.cluster}
export P2_CAP_OUT=${P2_CAP_OUT:-$FENCED_ROOT/fenced-sweep}
export BEFORE_BIN=${BEFORE_BIN:-$FENCED_ROOT/fenced-bins/before/persistd}
export AFTER_BIN=${AFTER_BIN:-$FENCED_ROOT/fenced-bins/after/persistd}
export SEED_BIN=${SEED_BIN:-$FENCED_ROOT/fenced-bins/after/orrery-seed}
export ORRERY_SEED_BIN="$SEED_BIN"
repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
export P2_LOAD_BIN=${P2_LOAD_BIN:-$repo/p2-load/target/release/p2-load}
export FENCED_FDB_CONTAINER=${FENCED_FDB_CONTAINER:-orrery-fdb-fenced}
FDB_PID=${FDB_PID:-$(docker top "$FENCED_FDB_CONTAINER" | awk '/fdbserver/{print $2}' | head -1)}
export FDB_PID
mkdir -p "$P2_CAP_OUT"
