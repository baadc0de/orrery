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
# `gates/p1-swarm` exits non-zero unless every clause holds, and this wrapper writes no
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
  # And one record per `"$BIN"` invocation, continuations folded in. `has` can
  # only say that a flag appears on *some* leg, and there are five of them: the
  # checks that are about a particular leg have to read that leg. Measured
  # 2026-08-17 — with `--witness` deleted from the witnessed hour below, `has
  # '--impaired --witness'` still passed, because the conviction and control
  # legs pair the same two flags on 8 peers for five minutes. The clause read as
  # a guard on the hour and was a guard on nothing.
  # awk rather than `sed -e :a ... ta`, which is GNU-only: BSD sed rejects a
  # label or a branch followed by `;`, and the sibling P4 scripts run their
  # self-tests on macOS and Windows runners. Same idiom everywhere is worth more
  # than a shorter one here.
  flat="$(awk '{ sub(/^[ \t]+/, ""); buf = buf $0
                 if (buf ~ /\\$/) { sub(/\\$/, " ", buf); next }
                 print buf; buf = "" }
               END { if (buf != "") print buf }' <<<"$legs" \
    | grep '^"\$BIN"' || true)"
  [[ -n $flat ]] || die 'self-test: no harness invocations found; the leg parse has drifted'
  has '--peers 32' || die 'self-test: the criterion population is not 32'
  has '--seconds 3600' || die 'self-test: the criterion hour is not run'
  has '--min-cells 64' || die 'self-test: the ≥64-cell roam is not required'
  has '--late-join-at' || die 'self-test: the late-join check is absent'
  has '--impaired' || die 'self-test: the impaired link run is absent'
  # The witnessed leg, identified as the criterion *hour* that runs a witness —
  # the conviction and control legs are five simulated minutes on 8 peers and
  # cannot stand in for it. Without `--witness` on this leg,
  # `SwarmConfig.witnessing` is false and P4's three witnessing clauses are
  # guarded by a false flag and pass by never being asked.
  # `|| true` because a leg that has lost the flag is exactly what this is
  # looking for, and under `set -e` an empty grep would exit 1 with no message.
  witnessed="$(grep -F -- '--seconds 3600' <<<"$flat" | grep -F -- '--witness' || true)"
  [[ -n $witnessed ]] \
    || die 'self-test: the witnessed criterion hour is absent; the P4 clauses are dead code without it'
  grep -Fq -- '--impaired' <<<"$witnessed" \
    || die 'self-test: the witnessed hour runs a clean link; P4 measures the witness under impairment'
  grep -Fq -- '--max-shed' <<<"$witnessed" \
    || die 'self-test: the witnessed leg has lost its own shed allowance'
  # §9.3's own overrun signal, and the reason the shed band above can afford to
  # be wide: without this flag the leg judges only a harness-local convention
  # and the document's normative counter goes back to being printed and ignored,
  # which is the state #974 found it in.
  grep -Fq -- '--max-unsheddable-over-budget' <<<"$witnessed" \
    || die 'self-test: the witnessed leg no longer judges unsheddable_over_budget; §9.3 overrun signal unenforced'
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
  # The exterior leg is not a `"$BIN"` invocation, so `has` cannot see it: it is
  # a two-process wall-clock proof driven through the ignored integration test.
  # Without it no leg here ever passes `--external-peer`, and with no leg passing
  # it the campaign's non-craft bodies are never replicated under the nightly —
  # which is exactly how #961 hid.
  grep -Fq -- 'an_external_peer_joins_witnesses_and_moves_frames' <<<"$legs" \
    || die 'self-test: the exterior leg is absent; no leg then runs --external-peer and the campaign bodies are never on the wire'
  cargo run -q --manifest-path "$(dirname "$0")/../gates/p1-swarm/Cargo.toml" -- --self-test \
    || die 'self-test: the harness no longer covers every criterion clause'
  echo "$NAME: self-test passed"
  exit 0
fi

readonly ROOT="$(cd "$(dirname "$0")/.." && pwd)"
readonly OUT="${P1_SWARM_OUT:-$ROOT/target/gates/p1-swarm}"
mkdir -p "$OUT"

note 'building the harness (release: an hour of simulation is not a debug workload)'
cargo build --release -q --manifest-path "$ROOT/gates/p1-swarm/Cargo.toml"
readonly BIN="$ROOT/gates/p1-swarm/target/release/p1-swarm"
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
# ── The shed band, and §9.3's own signal ──────────────────────────────────────
#
# The shed allowance is the same kind of thing. The witness lane makes a
# transient real that the cruise-only runs do not have: at island formation a
# peer recovering from a hitch serves its witnesses' repair burst on the
# unsheddable control lane and sheds the cheap lane to afford it
# (docs/03-replication.md §5.3a). What says transient rather than overrun is that
# the count is *identical* at 30 simulated seconds, at five minutes and at one
# hour — measured again for #974 and still true, of the healthy run and of a
# deliberately starved one alike.
#
# It used to be a ratchet: the measured number exactly, 206 → 230 → 162 → 278,
# on the contract "a run that moves it has found something". **#974 measured the
# premise underneath that contract and it is false.** The premise was that at a
# fixed seed and a fixed loss point the shed count is a single number. It is
# not, and the reason is in the harness rather than in the sender:
# `gates/p1-swarm/src/router.rs` hashes the *payload bytes* into packet identity
# (`PacketFate::of`) and draws loss and jitter from `(seed, packet, draw)`
# (`draws`). That is deliberate and worth keeping — it buys order-independence,
# so an A/B comparison is not silently corrupted by send order — but it means
# any change to any byte on the wire re-rolls the whole loss realisation, and a
# different realisation drops different packets, provokes different repair
# bursts and sheds a different number.
#
# Holding the simulation *completely* fixed — seed 1, 3% loss, 32 peers,
# witnessed, one hour's worth of formation — and re-rolling only that
# realisation 24 times, `total_shed` ranged **32 to 420**, sd 81. The entire
# 180-cell seed × loss sweep over the criterion's band ranged 58–399, sd 64. The
# two distributions are the same one: the seed carries almost no information
# about this count and the impairment realisation carries nearly all of it. So
# #816's ruleset-digest change (real blake3 build hashes replacing
# compressible placeholder fill — legitimate traffic, and `replication_bytes`
# actually *fell*) did not break a bound. It collected on one that was asserting
# a precision the harness never had, and cost two investigations to unwind.
#
# ── Deriving the band ────────────────────────────────────────────────────────
#
# Healthy reference population, all at HEAD, 32 peers, witnessed, impaired, on
# the 1 Mbps budget, at a point of the criterion's 3–5% loss band: 180
# (seed, loss) cells plus 48 impairment realisations at two fixed
# configurations = **228 runs**.
#
#   total_shed                min 32   p50 238   p95 345   max 420   mean 233.1   sd 66.1
#   unsheddable_over_budget   min  1   p50  16   p95  29   max  42   mean  16.1   sd  7.7
#
# The nightly judges these two clauses on three legs across three platforms —
# about 3300 judgements a year — so a bound wants a per-run false-failure rate
# well under 1/3300. `mean + 4·sd` gives a one-sided normal tail of 3.2e-5, or
# roughly one false failure per decade, and that is the rule used here:
#
#   shed ceiling         233.1 + 4 × 66.1 = 497.6  →  500
#   unsheddable ceiling   16.1 + 4 × 7.7  =  47.1  →   48
#
# Both sit above the observed healthy maximum with room (500 vs 420, 48 vs 42),
# and the *lower* edge of the same construction lands below zero for both
# (-31.4 and -14.8), which is why the band is stated as a ceiling: on a quantity
# this dispersed there is no floor to assert, and asserting one would be the
# ratchet's mistake again in the other direction.
#
# ── What the band still catches ──────────────────────────────────────────────
#
# Sensitivity, measured by starving the send path with `--budget-kbps` while
# judging against the same 1 Mbps criterion (seed 1, 3% loss, 30 s):
#
#   budget   1000    990    975    950    925    900    875    850    700
#   shed      345    412    494    650    790   1031   1453   2528  21185
#   unshed     20     28     42     75     87     97    159    285   1495
#
# Both clauses cross between a 2.5% and a 5% budget shortfall, and both are two
# orders of magnitude clear of a real one. A bound that fires on a 5% shortfall
# and never on 228 healthy runs is a gate; 278 was a tripwire on a coin flip.
#
# `--max-unsheddable-over-budget` is the substantive half. §9.3 names exactly
# two overrun signals — `UploadMeter::unsheddable_over_budget` and the windowed
# rate — and explicitly refuses a "did we shed" boolean, because a quiet frame
# would clear it. A shed *count* is neither: it says the backstop worked. This
# counter says the lanes that could not be shed were sent and charged anyway,
# so the overrun is real rather than an artefact of shedding. It was measured,
# surfaced in `SwarmReport`, printed on every run with its own explanatory line
# — and judged by nothing until #974. Zero is the criterion and zero is what
# every other leg in this file measures, cruise and conviction alike; only the
# 32-peer witnessed legs need the formation allowance, and they cannot assert
# zero because they read 1–42 on healthy runs and never 0.
"$BIN" --peers 32 --seconds 3600 --min-cells 1 --max-pops 0 --max-shed 500 \
  --max-unsheddable-over-budget 48 \
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

# ── The multi-process legs, and the accounting seam ────────────────────────
#
# Everything above is a pure-bot island in one process. These are `#[ignore]`d
# because they run several processes at wall clock, and until #961 landed
# nothing dispatched any of them — `tests/external_join.rs` said this script ran
# them and this script did not. That gap is the shape of both defects: a test
# that exists, is described in the record, and never runs.
#
# The exterior leg (#961). `--external-peer` is the only mode that binds a real
# iroh endpoint, and — via `main.rs`'s `campaign: args.external_peer` — the only
# one that seeds campaign rocks, so it is the only leg that puts a body other
# than a `Craft` on the wire. That is how a receiver which discarded every rock
# and charged it to `bad_body` survived: the counter ran at 151 on an 8-second
# run and no nightly ever took one. Kept as its own invocation, with its own
# words, so a regression in the receive path is named as one rather than
# reported as "a multi-process leg failed".
note 'exterior leg: a real dialled peer against a campaign island, rocks on the wire'
cargo test --release -q --manifest-path "$ROOT/gates/p1-swarm/Cargo.toml" \
  --test external_join -- --ignored --exact \
  an_external_peer_joins_witnesses_and_moves_frames \
  || die 'the exterior join did not hold: see bad_body / total_replicas in its output (#961)'

# The accounting seam (#960). This is the one P4 exit waits on: it runs the
# binary above as a reservation-backed host, seats two real remote processes,
# and puts the report the host actually wrote through
# `p4-campaign-session.sh assemble` and `p4-ledger.sh append` unedited. Both
# sides of that join were already tested against fixtures of the shape each
# expected, which is precisely why the suite was green while no human hour could
# bank.
note 'seam leg: a real host report through the real assembler into the real ledger'
cargo test --release -q --manifest-path "$ROOT/gates/p1-swarm/Cargo.toml" \
  --test attempt_report_seam -- --ignored --test-threads 1 \
  || die 'the attempt-report seam failed; no human hour can bank (#960)'

# The remaining wall-clock joins: reserved seating order, late join and rejoin,
# and observed seat release. `--skip` rather than a second `--exact` list so a
# leg added to that file is dispatched by default instead of silently joining
# the set nothing runs, which is what this block exists to end.
note 'lobby legs: reserved seating order, late join, rejoin and observed release'
cargo test --release -q --manifest-path "$ROOT/gates/p1-swarm/Cargo.toml" \
  --test external_join -- --ignored --test-threads 1 \
  --skip an_external_peer_joins_witnesses_and_moves_frames \
  || die 'a multi-process lobby leg failed'

# Last, and only now: every leg above must hold before this file claims it did.
date -u +%Y-%m-%dT%H:%M:%SZ > "$OUT/PASSED"
note "every clause held on all five legs plus the exterior, seam and lobby legs; reports in $OUT"
