#!/usr/bin/env bash
# P1's permanent replication regression harness (docs/11-roadmap.md §P1).
#
# The phase's demo criterion: 32 synthetic peers, scripted roaming across ≥64
# interest cells, run for one hour — every peer's sustained upload stays
# ≤ 1 Mbps; interest-set churn is absorbed without visible proxy pops; no entity
# thrashes cells at a boundary; a late-joining peer receives only its 27-cell
# neighborhood.
#
# Like the P2 and P3 gates this is a *proof harness*, not a convenience script:
# `p1-swarm` exits non-zero unless every clause holds, and this wrapper writes no
# success artifact unless it does.
#
# The hour is *simulated*. Each peer's clock advances one 60 Hz tick per frame
# rather than reading a wall clock, so the run costs what it costs to compute —
# about three minutes — and is reproducible from its seed. Rates are therefore
# bytes per simulated second, which is what the budget is about: the send cadence
# is 20 Hz of sim ticks, not of wall seconds.
set -euo pipefail

readonly NAME=p1-swarm-gate
die() { echo "$NAME: $*" >&2; exit 2; }
note() { echo "$NAME: $*" >&2; }

if [[ ${1:-} == --self-test ]]; then
  # Offline guard for CI images with no time to run a swarm. Deliberately
  # structural: it catches regression to a script that no longer proves the
  # criterion, without pretending to run 32 peers for an hour.
  grep -Fq -- '--peers 32' "$0" || die 'self-test: the criterion population is not 32'
  grep -Fq -- '--seconds 3600' "$0" || die 'self-test: the criterion hour is not run'
  grep -Fq -- '--min-cells 64' "$0" || die 'self-test: the ≥64-cell roam is not required'
  grep -Fq -- '--late-join-at' "$0" || die 'self-test: the late-join check is absent'
  grep -Fq -- '--impaired' "$0" || die 'self-test: the impaired link run is absent'
  cargo run -q --manifest-path "$(dirname "$0")/../p1-swarm/Cargo.toml" -- --self-test \
    || die 'self-test: the harness no longer covers every criterion clause'
  echo "$NAME: self-test passed"
  exit 0
fi

readonly ROOT="$(cd "$(dirname "$0")/.." && pwd)"
readonly OUT="${P1_SWARM_OUT:-$ROOT/target/p1-swarm}"
mkdir -p "$OUT"

note 'building the harness (release: an hour of simulation is not a debug workload)'
cargo build --release -q --manifest-path "$ROOT/p1-swarm/Cargo.toml"
readonly BIN="$ROOT/p1-swarm/target/release/p1-swarm"
[[ -x $BIN ]] || die "harness binary missing at $BIN"

# The criterion run: a clean link, because P1's demo criterion is about interest
# management rather than loss tolerance. The impaired profile below is P4's
# input, run second so a failure names which link it failed on.
note 'clause run: 32 peers, one simulated hour, clean link'
"$BIN" --peers 32 --seconds 3600 --min-cells 64 --late-join-at 1800 \
  --json "$OUT/clean.json" \
  || die 'the P1 criterion did not hold on a clean link'

# P4 needs the same population to survive 3–5% loss and 100 ms jitter without
# the interest machinery degrading. Running it here means a regression in either
# shows up in one place.
note 'impaired run: the same hour under 3% loss and 100 ms jitter spikes'
# No pop allowance: the impairment is seeded, so this run is as reproducible as
# the clean one and there is nothing to absorb. `--max-pops` exists for exploring
# other seeds; if a seed ever needs it, that number is the finding rather than a
# knob to turn.
"$BIN" --peers 32 --seconds 3600 --min-cells 64 --late-join-at 1800 --impaired --max-pops 0 \
  --json "$OUT/impaired.json" \
  || die 'the P1 criterion did not hold under the P4 impairment profile'

date -u +%Y-%m-%dT%H:%M:%SZ > "$OUT/PASSED"
note "every clause held on both links; reports in $OUT"
