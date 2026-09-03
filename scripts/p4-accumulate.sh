#!/usr/bin/env bash
# P4's player-hour accumulation leg: one witnessed hour per night, at a point of
# the criterion's loss band that the night before did not run.
#
# P4 does not exit until ≥ 500 honest player-hours under injected impairment
# produce zero false positives (docs/11-roadmap.md §P4). The nightly swarm gate
# already runs a witnessed hour — and runs the *same* one every night. Its leg
# is fixed at `--seed 1` (the harness default; no seed flag appears anywhere in
# `scripts/` or `.github/`) and at the 3% floor of the band, and the report's
# `RunIdentity` is seed + impairment + target + commit with no wall clock in it,
# so two nightlies on one commit produce byte-identical identities. That is
# correct for a *gate* — a regression harness that changed its parameters every
# night would be measuring something new each time instead of noticing changes —
# and it is why the gate cannot be the thing that accumulates. Thirty-two hours
# re-run three hundred times are thirty-two hours.
#
# So this is a separate leg with the opposite property: it varies. The seed
# changes every night and the loss sweeps 3% → 4% → 5% on a three-day cycle, so
# consecutive nights are distinct runs of the same pipeline over distinct
# samples of the band the criterion names, and `scripts/p4-ledger.sh` banks each
# one exactly once.
#
# ── Why a sweep and not just a new seed ─────────────────────────────────────
#
# The criterion says 3–5% loss, and until #40 only the floor had ever been run;
# running the other end is what found the dead-watch defect (coverage 93.8% at
# 5% against a 95% floor). Both ends read 100.0% now. Accumulating 500 hours
# entirely at 3% would re-open exactly that gap — a band exercised at one point
# is a band in name — so the hours are spread across it by construction rather
# than by anybody remembering to.
#
# ── The shed band, and §9.3's own signal ───────────────────────────────────
#
# This leg and the gate's witnessed leg now pass the *same* two numbers, and
# `scripts/p1-swarm-gate.sh` carries the derivation in full. The short version,
# because the numbers here have been wrong twice for the same reason:
#
# The gate's leg used to pass `--max-shed` as an exact per-seed ratchet on the
# premise that at a fixed seed and a fixed loss point the shed count is a single
# number. This leg's own header used to say that too, and used to say the
# ratchet stood at 162 — #788 moved it to 278 on 2026-08-31 and never came back
# here, so that sentence described a world a fortnight gone. Both statements
# were wrong in the same way, and #974 measured why.
#
# The impairment model hashes the *payload bytes* into packet identity
# (`gates/p1-swarm/src/router.rs`, `PacketFate::of`) and draws loss and jitter
# from `(seed, packet, draw)`. That is deliberate — it buys order-independence,
# so an A/B comparison is not corrupted by send order — but it means any change
# to any byte on the wire re-rolls the entire loss realisation. Holding the
# simulation completely fixed and re-rolling only that realisation 24 times,
# `total_shed` ranged **32 to 420**. The seed carries almost no information
# about this count; the impairment realisation carries nearly all of it. A
# number pinned at an observation was never measuring health.
#
# So both legs pass a band derived from one healthy reference population of 228
# runs at HEAD (180 (seed, loss) cells over the criterion's 3–5% band, plus 48
# impairment realisations at two fixed configurations):
#
#   total_shed                min 32   p50 238   p95 345   max 420   mean 233.1   sd 66.1
#   unsheddable_over_budget   min  1   p50  16   p95  29   max  42   mean  16.1   sd  7.7
#
# at `mean + 4·sd`, which is about one false failure per decade across the
# nightly's ~3300 judgements a year:
#
#   MAX_SHED                  233.1 + 4 × 66.1 = 497.6  →  500
#   MAX_UNSHEDDABLE_OVER_BUDGET 16.1 + 4 × 7.7 =  47.1  →   48
#
# Both still fail a genuine overrun by two orders of magnitude: starving the
# send path to 850 kbps while judging against the 1 Mbps criterion reads 2528
# shed and 285 unsheddable, and 700 kbps reads 21185 and 1495.
#
# `MAX_UNSHEDDABLE_OVER_BUDGET` is the half that follows from the record.
# docs/03-replication.md §9.3 names exactly two overrun signals —
# `UploadMeter::unsheddable_over_budget` and the windowed rate — and explicitly
# refuses a "did we shed" boolean. A shed count is neither of them, so no number
# for `--max-shed` follows from §9.3 at all; it is a harness-local convention.
# This counter is the document's own signal, and until #974 it was measured,
# printed on every run and judged by nothing. Every banked line carries its own
# `shed`, so both distributions stay auditable and a shift is visible without
# re-running anything.
#
# The transient settles early, which is what makes a 30-second cell a valid
# sample of an hour and is why the population above was affordable to measure:
# seed 1 at 3% sheds 345 with 20 unsheddable at 30 simulated seconds and at five
# minutes; seed 7 at 4% sheds 270 with 12 unsheddable at 30 s, 5 min and 15 min.
# A starved run is flat too (850 kbps: 2528/285 at both 30 s and 5 min), so
# flatness says *formation transient*, and magnitude is what says overrun.
#
# Nothing else is relaxed. `--min-cells` and `--max-pops` carry the same values
# the gate's witnessed leg gives them and for the same reasons: the witness leg
# deals idle/burst/stall profiles, so its least-travelled peer legitimately
# never leaves its cell, and the interest clauses are measured on the gate's
# cruise-only legs.
set -euo pipefail

readonly NAME=p4-accumulate
die() { echo "$NAME: $*" >&2; exit 2; }
note() { echo "$NAME: $*" >&2; }

usage() {
  cat >&2 <<'USAGE'
usage: p4-accumulate.sh [--index N] [--seconds S] [--dry-run]
       p4-accumulate.sh --probe
       p4-accumulate.sh --self-test

  --index N     which point of the sweep to run (default: days since the epoch,
                so consecutive nightlies differ without any stored state)
  --seconds S   simulated seconds (default 3600, the criterion's hour)
  --dry-run     print the run this index selects and exit
  --probe       run the short fixed-seed comparability run and write its report
                to $P4_ACCUM_OUT/probe-<target>.json; banks nothing

  P4_LEDGER_FILE  ledger path, passed through to scripts/p4-ledger.sh
  P1_SWARM_BIN    prebuilt harness; built from gates/p1-swarm/ when unset
  P4_ACCUM_OUT    evidence directory (default: target/p4-accumulate)
USAGE
}

self_test() {
  # The haystack is the script *body* below, with comments stripped: every
  # pattern here also appears in the line searching for it, so grepping the
  # whole file would match this block and pass unconditionally — the anti-
  # pattern fixed repo-wide in #35, and the commentary above names half of
  # these flags while explaining them.
  local body
  body="$(sed -n '/^readonly ROOT=/,$p' "$0" | grep -v '^[[:space:]]*#')"
  has() { grep -Fq -- "$1" <<<"$body"; }

  # Banking is downstream of the run's own exit status and must stay there: a
  # failed hour is a finding, and a finding that banks hours is a false record.
  # Checked first because the `if !` is also what delimits the leg below.
  has 'if ! "$bin"' \
    || die 'self-test: the leg no longer branches on the harness exit status; a failed run could bank'

  # The banking leg's own invocation, continuations folded into one line.
  # Reading these off the whole body is not enough and this was measured
  # 2026-08-17: `run_probe` passes `--peers 32`, `--impaired`, `--witness` and
  # `--max-shed` too, so the banking leg could lose any of them and every one of
  # these clauses still matched — on the probe, which banks nothing. A clause
  # that is satisfied by a different leg than the one it names is not a check.
  # Folded with awk and not with `sed -e :a ... ta`: this self-test runs on the
  # macOS and Windows legs of the nightly too, and BSD sed rejects a label or a
  # branch followed by `;` — shipping a GNU-only expression in the one script
  # whose job is to be portable is the bug this file was already caught by once.
  local leg
  leg="$(sed -n '/^  if ! "$bin"/,/^  fi$/p' "$0" \
    | awk '{ sub(/^[ \t]+/, ""); buf = buf $0
             if (buf ~ /\\$/) { sub(/\\$/, " ", buf); next }
             print buf; buf = "" }
           END { if (buf != "") print buf }' \
    | grep -F 'if ! "$bin"' || true)"
  [[ -n $leg ]] || die 'self-test: the banking leg could not be located; the parse has drifted'
  on_leg() { grep -Fq -- "$1" <<<"$leg"; }

  on_leg '--witness' \
    || die 'self-test: the leg is not witnessed; it would accumulate hours no witness watched'
  on_leg '--impaired' \
    || die "self-test: the leg runs a clean link; the criterion's hours are impaired hours"
  on_leg '--loss "$loss"' \
    || die 'self-test: the loss is no longer swept; every night would re-run one point of the band'
  on_leg '--seed "$seed"' \
    || die 'self-test: the seed is no longer varied; consecutive nights would share a RunIdentity'
  on_leg '--peers 32' \
    || die 'self-test: the criterion population is not 32'
  on_leg '--stamp-wall-clock' \
    || die 'self-test: the run is no longer stamped, so a banked line cannot be placed in time'
  # By the append specifically. `p4-ledger.sh total` prints a running figure and
  # is not what banks anything, so the bare script name would keep matching a
  # leg that had stopped recording its hour.
  has 'p4-ledger.sh" append' \
    || die 'self-test: the leg banks nothing; the hours would die with the runner'
  on_leg '--max-shed' \
    || die 'self-test: the shed allowance is gone; the budget backstop would go unjudged'
  # §9.3's own overrun signal. The shed band above is wide by construction —
  # the quantity it bounds moves 32–420 for reasons unrelated to health — and
  # this is the bound that carries the document's meaning. Without it the leg
  # banks hours judged only against a harness-local convention.
  on_leg '--max-unsheddable-over-budget' \
    || die 'self-test: the leg no longer judges unsheddable_over_budget; §9.3 overrun signal unenforced'
  has 'BAND=' \
    || die 'self-test: the loss band is gone'
  # Off-Linux, both of these: cargo emits `p1-swarm.exe` on a Windows runner,
  # and a leg that only knows the unsuffixed name dies before it runs a tick.
  has 'p1-swarm.exe' \
    || die 'self-test: the Windows binary name is gone; the leg cannot find its harness on a Windows runner'
  has 'PROBE_SEED' \
    || die 'self-test: the comparability probe is gone; hours would be banked per platform with nothing saying the platforms agree'
  has '--probe) probe=1' \
    || die 'self-test: --probe is unreachable; the nightly step that compares the platforms would run the hour instead'

  # The probe's report has to be a function of its parameters and of nothing
  # else — it is compared byte for byte against another platform's. A wall-clock
  # stamp inside it would make every such comparison fail for the one reason
  # that means nothing. Checked on the probe's own body, since the banking leg
  # legitimately stamps and a whole-file search would match that.
  local probe_body
  probe_body="$(sed -n '/^run_probe() {/,/^}/p' "$0" | grep -v '^[[:space:]]*#')"
  [[ -n $probe_body ]] || die 'self-test: run_probe is gone'
  grep -Fq -- '--stamp-wall-clock' <<<"$probe_body" \
    && die 'self-test: the comparability probe stamps a wall clock; no two platforms could ever agree'
  grep -Fq -- '--witness' <<<"$probe_body" \
    || die 'self-test: the comparability probe does not run the witness, so it compares the wrong pipeline'

  # Functional half: the sweep must actually sweep. Consecutive indices have to
  # produce distinct (seed, loss) pairs — that is the whole property this leg
  # exists for — and every loss it produces has to be inside the criterion's
  # band, since scripts/p4-ledger.sh refuses to bank anything outside it.
  local i seed loss seen='' point
  for i in 0 1 2 3 4 5; do
    read -r seed loss <<<"$(sweep_point "$i")"
    point="$seed@$loss"
    grep -Fqx "$point" <<<"$seen" \
      && die "self-test: indices repeat a run identity at $point; consecutive nights would not accumulate"
    seen+="$point"$'\n'
    awk -v l="$loss" 'BEGIN { exit !(l >= 0.03 && l <= 0.05) }' \
      || die "self-test: index $i sweeps to loss $loss, outside the criterion's 3–5% band"
  done
  # And the band is swept rather than merely varied: six consecutive nights must
  # visit at least three distinct loss points, or the sweep is a seed change
  # with extra steps and 500 hours accumulate at one point of a band whose other
  # end is where the last defect was found. Both ends by name, since a sweep of
  # three points inside 3.0–3.2% would satisfy a bare count.
  local points
  points=$(cut -d@ -f2 <<<"$seen" | sort -u | grep -c .)
  (( points >= 3 )) \
    || die "self-test: six consecutive indices visited only $points loss point(s); the band is not swept"
  grep -Fq '@0.03' <<<"$seen" || die "self-test: the sweep never runs the band's 3% floor"
  grep -Fq '@0.05' <<<"$seen" || die "self-test: the sweep never runs the band's 5% ceiling"

  # The seed is a function of the sweep index alone. That is what makes the
  # three platform legs run the *same* hour on the same night, which is what
  # makes a divergence between their reports visible at all; a seed derived from
  # the runner would bank three unrelated hours that could never be compared.
  local first second
  read -r first _ <<<"$(sweep_point 12345)"
  read -r second _ <<<"$(sweep_point 12345)"
  [[ $first == "$second" ]] \
    || die 'self-test: one index produced two seeds; the platform legs would not share an hour'

  echo "$NAME: self-test passed"
}

readonly ROOT="$(cd "$(dirname "$0")/.." && pwd)"
readonly OUT="${P4_ACCUM_OUT:-$ROOT/target/p4-accumulate}"

# The criterion's band, sampled at its ends and its middle. Sweeping in this
# order means any three consecutive nights cover all three.
readonly BAND=(0.03 0.04 0.05)
# The band's upper edge and §9.3's own overrun bound, both derived in the header
# from 228 healthy runs at `mean + 4·sd`. The gate's witnessed leg passes the
# same two numbers: #974 established that the shed count is a function of the
# impairment realisation rather than of the seed, so there is nothing for a
# per-leg number to say. A night above either fails the leg and banks nothing.
readonly MAX_SHED=500
readonly MAX_UNSHEDDABLE_OVER_BUDGET=48

# index → "seed loss". The seed is the index itself: the default index is days
# since the epoch, which is monotone, needs no stored state, and is the same on
# a re-dispatch of the same night — so a re-dispatch is a duplicate the ledger
# skips rather than a new hour banked twice.
sweep_point() {
  local index=$1
  echo "$index ${BAND[index % ${#BAND[@]}]}"
}

# Where the release harness lands, built if it is not already there.
#
# `.exe` is not a detail: on a `windows-latest` runner these scripts run under
# Git Bash and cargo emits `p1-swarm.exe`, so the unsuffixed path this used to
# hard-code does not exist and the leg would die at "harness binary missing"
# before it ran a tick. Both spellings are tried rather than switching on
# `$OSTYPE` — the question is which file cargo produced, and the filesystem
# answers it.
ensure_bin() {
  local bin=${P1_SWARM_BIN:-}
  if [[ -n $bin ]]; then
    [[ -x $bin ]] || die "harness binary missing at $bin"
    echo "$bin"
    return
  fi
  note 'building the harness (release: a simulated hour is not a debug workload)'
  cargo build --release -q --manifest-path "$ROOT/gates/p1-swarm/Cargo.toml" 1>&2
  local candidate
  for candidate in "$ROOT/gates/p1-swarm/target/release/p1-swarm" \
                   "$ROOT/gates/p1-swarm/target/release/p1-swarm.exe"; do
    if [[ -x $candidate ]]; then echo "$candidate"; return; fi
  done
  die "harness binary missing at $ROOT/gates/p1-swarm/target/release/p1-swarm[.exe]"
}

# ── The comparability probe ──────────────────────────────────────────────────
#
# The accumulation leg runs on three platforms now, and hours banked on three
# platforms are only worth banking separately if the runs are *comparable*.
#
# `SwarmReport` is a pure function of its parameters. Everything in it is
# computed from the seeded simulation; the wall-clock phase timings go to stderr
# and never into the report, and the one field that is not a function of the
# parameters — `started_at_unix_secs` — exists only when `--stamp-wall-clock` is
# passed, which this mode does not pass. Two identical-seed runs on one box
# produce byte-identical JSON, measured.
#
# So the cross-platform question has a byte-exact answer: run the same seed on
# each platform and every field except `identity.target` must match. That is a
# far stronger check than comparing a handful of summary numbers, and it costs
# two simulated minutes rather than an hour — the same 32 peers, the same
# witnessed pipeline, the same impairment, fewer ticks of it.
#
# It is deliberately not banked. Two simulated minutes are not one of the
# criterion's hours.
readonly PROBE_SEED=7
readonly PROBE_LOSS=0.04
readonly PROBE_SECONDS=120

run_probe() {
  local bin
  bin=$(ensure_bin)
  mkdir -p "$OUT"
  local staged="$OUT/probe.json"
  note "comparability probe: seed $PROBE_SEED, loss $PROBE_LOSS, $PROBE_SECONDS simulated seconds at 32 peers"
  # No `--stamp-wall-clock` here, unlike the banking leg: the whole point is a
  # report that is a function of its parameters and of nothing else, so that a
  # diff against another platform's means what it looks like it means.
  "$bin" --peers 32 --seconds "$PROBE_SECONDS" --min-cells 1 --max-pops 0 \
    --max-shed "$MAX_SHED" \
    --max-unsheddable-over-budget "$MAX_UNSHEDDABLE_OVER_BUDGET" \
    --late-join-at "$(( PROBE_SECONDS / 2 ))" \
    --impaired --loss "$PROBE_LOSS" --seed "$PROBE_SEED" --witness \
    --json "$staged" \
    || die 'the comparability probe did not hold'

  # Named after the triple the binary was *compiled* for, read back out of the
  # report rather than derived from `uname`: the report is what will be
  # compared, and a name taken from the runner would disagree with it on any
  # cross-built leg. Naming the artifact this way is also what lets the
  # comparison job tell three uploaded reports apart without being told which
  # runner produced which.
  local target
  target=$(jq -r '.identity.target' "$staged")
  [[ -n $target && $target != null ]] || die 'the probe report does not name its target triple'
  mv "$staged" "$OUT/probe-$target.json"
  note "probe report written to $OUT/probe-$target.json"
}

main() {
  local index='' seconds=3600 dry=0 probe=0
  while (( $# )); do
    case $1 in
      --index) index=${2:-}; shift 2 ;;
      --seconds) seconds=${2:-}; shift 2 ;;
      --dry-run) dry=1; shift ;;
      --probe) probe=1; shift ;;
      -h | --help) usage; exit 0 ;;
      *) usage; die "unknown argument '$1'" ;;
    esac
  done

  if (( probe )); then
    run_probe
    return
  fi

  [[ -z $index ]] && index=$(( $(date -u +%s) / 86400 ))
  [[ $index =~ ^[0-9]+$ ]] || die "--index must be a non-negative integer, got '$index'"

  local seed loss
  read -r seed loss <<<"$(sweep_point "$index")"
  note "index $index → seed $seed, loss $loss, $seconds simulated seconds at 32 peers"
  (( dry )) && exit 0

  local bin
  bin=$(ensure_bin)

  mkdir -p "$OUT"
  local report="$OUT/accum-$seed-$loss.json"

  # The witnessed leg, at this night's point of the band. Every clause the gate's
  # witnessed leg blocks on blocks here too — the harness exits non-zero unless
  # they hold, and a leg that fails is a finding, not an hour.
  #
  # The seed comes from the sweep index and not from the runner, so all three
  # platforms run the *same* hour on the same night. `RunIdentity` carries the
  # target triple, so those are three distinct runs the ledger banks separately
  # — and three runs of one seed across three platforms are worth more than
  # three unrelated ones, because a divergence between them is then visible in
  # the reports themselves.
  if ! "$bin" --peers 32 --seconds "$seconds" --min-cells 1 --max-pops 0 \
      --max-shed "$MAX_SHED" \
      --max-unsheddable-over-budget "$MAX_UNSHEDDABLE_OVER_BUDGET" \
      --late-join-at "$(( seconds / 2 ))" \
      --impaired --loss "$loss" --seed "$seed" --witness --stamp-wall-clock \
      --json "$report"
  then
    die "the witnessed hour did not hold at seed $seed, loss $loss; nothing banked"
  fi

  # Banked only now, and only from the report: see scripts/p4-ledger.sh for why
  # it re-checks the clauses rather than trusting this exit status.
  "$ROOT/scripts/p4-ledger.sh" append "$report"
  "$ROOT/scripts/p4-ledger.sh" total
}

if [[ ${1:-} == --self-test ]]; then
  self_test
  exit 0
fi
main "$@"
