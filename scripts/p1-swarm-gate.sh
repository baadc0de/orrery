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
# one — and is reproducible from its seed. Rates are therefore bytes per
# simulated second, which is what the budget is about: the send cadence is 20 Hz
# of sim ticks, not of wall seconds.
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
  # The conviction leg, by the flag that arms it. `--cheat` is what fields a
  # modified client, takes every witness out of shadow mode and turns the filed
  # reports over to an adjudicator; without it the six clauses of P4's demo
  # criterion are guarded by a `None` and pass by never being asked, exactly as
  # the three witnessing clauses did before `--witness` ran anywhere.
  has '--cheat speed' \
    || die 'self-test: the conviction leg is absent; P4 demo-criterion clauses are dead code without it'
  # The population the criterion names, and the only place in this file it
  # appears. The witnessed legs above run 32.
  has '--peers 8' || die 'self-test: the demo criterion 8-peer island is not run'
  # The control, by the flag that makes it one. Shadow mode files nothing on
  # every other leg here, so an honest island run *without* this proves nothing
  # about false-positive filing that shadow mode had not already decided.
  has '--witness --enforce' \
    || die 'self-test: the armed honest control is absent; "files nothing" would be shadow mode restating itself'
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
# the count is *identical* at five simulated minutes and at one hour. So the
# allowance is the measured number exactly, and not a round one: it is a ratchet,
# and a run that moves it has found something.
#
# It moved once, from 206 to 230, and the thing it found is recorded in
# docs/11-roadmap.md §P4: watches that lost their first frame used to go blind
# for the rest of the session, asking for no repair at all. Repairing them
# instead is more traffic on the unsheddable control lane, so the cheap lane is
# shed to afford it — the same mechanism this comment already describes, at a
# slightly higher level. 230 at 3% loss, 255 at 5%; the allowance tracks the
# leg that runs here.
"$BIN" --peers 32 --seconds 3600 --min-cells 1 --max-pops 0 --max-shed 230 \
  --late-join-at 1800 --impaired --witness --stamp-wall-clock \
  --json "$OUT/witnessed.json" \
  || die 'the P4 witnessing clauses did not hold over the impaired hour'

# The conviction leg: P4's demo criterion stated literally — "a modified client
# applying a 1.5× speed multiplier joins an 8-peer island: detected, escalated,
# replay-adjudicated with a deviation verdict within one adjudication window of
# the violation". The three legs above measure the *other* half, the
# false-positive rate over honest play; this one measures whether the pipeline
# catches anybody at all. Neither is evidence without the other: a witness tuned
# until it accuses nobody passes the first three trivially.
#
# `--cheat` implies `--witness` and takes every peer out of shadow mode, so this
# is the only leg in the tree where a report is actually filed and adjudicated.
# Six clauses live here and nowhere else — that the cheat diverges at all, that
# it is convicted on replay, that no honest peer is reported, that the
# conviction lands inside the 180-tick window, that an unmodified swarm files
# nothing, and that every witness holds a key it can sign with.
#
# Eight peers and five minutes, which is the criterion's own population and
# about seven wall seconds — the cheapest leg here by two orders of magnitude,
# because detection happens 32 ticks in and everything after it is confirmation.
# The roaming and shed allowances are open for the same reason the witnessed leg
# relaxes them: this leg is about the conviction, and the interest clauses are
# measured on the cruise-only runs above.
note 'conviction run: an 8-peer island with one modified client, impaired link'
"$BIN" --peers 8 --seconds 300 --min-cells 1 --max-shed 64 \
  --late-join-at 150 --impaired --witness --cheat speed --stamp-wall-clock \
  --json "$OUT/conviction.json" \
  || die "P4's demo criterion did not hold: the modified client was not convicted, or an honest peer was"

# And the control: the same island, every witness still armed, and nobody
# modified. It must file *nothing*.
#
# `--enforce` without `--cheat` is what makes this a measurement rather than a
# restatement of shadow mode. The legs above all file nothing because shadow
# mode forbids it; this one files nothing having been allowed to. Without it the
# conviction leg proves the pipeline accuses somebody, not that it accuses the
# right somebody — a witness that filed against everyone would pass every clause
# that only ever looks at the cheat.
note 'control run: the same island, witnesses armed, nobody modified — must file nothing'
"$BIN" --peers 8 --seconds 300 --min-cells 1 --max-shed 64 \
  --late-join-at 150 --impaired --witness --enforce \
  --json "$OUT/control.json" \
  || die 'an entirely honest island filed a report with enforcement on'

date -u +%Y-%m-%dT%H:%M:%SZ > "$OUT/PASSED"
note "every clause held on all five legs, the witnessed and conviction ones included; reports in $OUT"
