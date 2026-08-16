#!/usr/bin/env bash
# P1's permanent replication regression harness (docs/11-roadmap.md §P1).
#
# The phase's demo criterion: 32 synthetic peers, scripted roaming across ≥64
# interest cells, run for one hour — every peer's sustained upload stays
# ≤ 1 Mbps; interest-set churn is absorbed without visible proxy pops; no entity
# thrashes cells at a boundary; a late-joining peer receives only its 27-cell
# neighborhood.
#
# It also carries P4's witnessing clauses, which no other harness runs: the
# third leg below is the only place `--witness` is passed anywhere in the tree,
# and the three clauses guarded by it are dead code without it.
#
# Like the P2 and P3 gates this is a *proof harness*, not a convenience script:
# `p1-swarm` exits non-zero unless every clause holds, and this wrapper writes no
# success artifact unless it does. All three legs block, the witnessed one
# included — the harness reads no clock and opens no socket, so a leg that holds
# once holds every night until the code under it changes, which is the only
# thing a nightly gate is meant to notice.
#
# The hour is *simulated*. Each peer's clock advances one 60 Hz tick per frame
# rather than reading a wall clock, so the run costs what it costs to compute —
# a couple of minutes for each cruise-only leg and about ten for the witnessed
# one — and is reproducible from its seed. Rates are therefore
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
  #
  # Searched against the *invocations* below rather than against the whole file,
  # and this is not tidiness. Every pattern here also appears, literally, in the
  # line that looks for it, so `grep -F -- '--peers 32' "$0"` matches its own
  # source and can only pass — which is what the five checks that predate this
  # comment were doing. Comment lines are stripped for the same reason: the
  # commentary below names `--min-cells 64` while explaining why the witnessed
  # leg does not use it.
  legs="$(sed -n '/^readonly ROOT=/,$p' "$0" | grep -v '^[[:space:]]*#')"
  has() { grep -Fq -- "$1" <<<"$legs"; }
  has '--peers 32' || die 'self-test: the criterion population is not 32'
  has '--seconds 3600' || die 'self-test: the criterion hour is not run'
  has '--min-cells 64' || die 'self-test: the ≥64-cell roam is not required'
  has '--late-join-at' || die 'self-test: the late-join check is absent'
  has '--impaired' || die 'self-test: the impaired link run is absent'
  # The witnessed leg by its signature rather than by the bare `--witness`
  # token. Nothing else here pairs impairment with the witness, and that pairing
  # is what brings P4's three witnessing clauses to life: without `--witness`
  # they are guarded by a false flag and pass by never being asked.
  has '--impaired --witness' \
    || die 'self-test: the witnessed impaired leg is absent; the P4 clauses are dead code without it'
  has '--max-shed' || die 'self-test: the witnessed leg has lost its own shed allowance'
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

# The witnessed leg, and the only one in which P4's three witnessing clauses are
# alive at all: without `--witness`, `SwarmConfig.witnessing` is false and every
# clause guarded by it — no false positive against an honest peer, the witness
# keeps watching, the witness sees the stream it is judging — is dead code that
# passes by never being asked. This leg is what makes the P4 half of the harness
# a gate rather than a capability.
#
# Run last because it is the expensive one: every peer re-executes its witness
# set's logs as well as its own, which is about ten wall minutes for the hour
# against roughly two for each leg above.
note 'witnessed run: the same impaired hour, every peer re-executing its witness set'
# Its own clause parameters, and not because a witness is allowed to be worse.
# `--witness` deals the awkward behavioural profiles — idle, burst, stall —
# where the runs above are all cruise, so the least-travelled peer is an idle one
# that legitimately never leaves its cell. Judging this leg at `--min-cells 64`
# fails the roaming clause by construction and reads as a witnessing regression
# when it is a parameterization mistake. The interest clauses are measured on the
# cruise-only runs above; this leg is about the witness.
#
# The shed allowance is the same kind of thing. The witness lane makes a
# transient real that the cruise-only runs do not have: at island formation a
# peer recovering from a hitch serves its witnesses' repair burst on the
# unsheddable control lane and sheds the cheap lane to afford it
# (docs/03-replication.md §5.3a). What says transient rather than overrun is that
# the count is *identical* at five simulated minutes and at one hour — 206 both
# times. So the allowance is the measured number exactly, and not a round one: it
# is a ratchet, and a run that moves it has found something.
"$BIN" --peers 32 --seconds 3600 --min-cells 1 --max-pops 0 --max-shed 206 \
  --late-join-at 1800 --impaired --witness --stamp-wall-clock \
  --json "$OUT/witnessed.json" \
  || die 'the P4 witnessing clauses did not hold over the impaired hour'

date -u +%Y-%m-%dT%H:%M:%SZ > "$OUT/PASSED"
note "every clause held on both links; reports in $OUT"
