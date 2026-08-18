#!/usr/bin/env bash
# Shared environment for the fdb-off-bulk-path capacity runs.
#
# A PRIVATE FoundationDB on its own port and data dir: the P2 path consumes
# its cluster (`activate_shards` bumps `actor/{shard}` epochs) and every point
# clears the keyspace, so it must never be the shared dev cluster on :4500.
export ORRERY_FDB_CLUSTER_FILE=/home/baadc0de/fdb-fenced.cluster
export P2_CAP_OUT=/home/baadc0de/fenced-sweep
export P2_LOAD_BIN=/home/baadc0de/orrery/.claude/worktrees/wf_63dc4c2b-13d-9/p2-load/target/release/p2-load
export BEFORE_BIN=/home/baadc0de/fenced-bins/before/persistd
export AFTER_BIN=/home/baadc0de/fenced-bins/after/persistd
export SEED_BIN=/home/baadc0de/fenced-bins/after/orrery-seed
export ORRERY_SEED_BIN="$SEED_BIN"
FDB_PID=$(docker top orrery-fdb-fenced | awk '/fdbserver/{print $2}' | head -1)
export FDB_PID
mkdir -p "$P2_CAP_OUT"
