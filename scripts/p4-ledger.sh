#!/usr/bin/env bash
# P4's append-only player-hour ledger (docs/11-roadmap.md §P4).
#
# The phase does not exit until ≥ 500 honest player-hours under injected
# impairment produce zero false-positive reports. `p1-swarm --witness` produces
# the hours; nothing until now added them up, and a nightly that re-ran one
# identical hour every night would have accumulated 32 hours forever. This
# script is the other half of `scripts/p4-accumulate.sh`: one JSONL line per
# banked run, deduplicated on the run's own identity, and refusing to bank a run
# whose witnessing clauses did not hold.
#
# ── What a line is evidence of ───────────────────────────────────────────────
#
# `RunIdentity` — seed, the full impairment profile, target triple, commit — is
# what makes two runs the same run, and it is the dedup key verbatim. A re-run
# of a night that already banked adds nothing; a run with a different seed, or
# at a different point of the loss band, is a different line. The identity
# carries no wall clock on purpose (`--stamp-wall-clock` puts that outside it),
# which is exactly what makes the key stable across a re-dispatch of the same
# nightly rather than making every dispatch look new.
#
# Hours are only comparable within a pipeline version, so every line also
# carries a `pipeline` digest: the git tree hashes of `orrery_witness`,
# `orrery_core`, `orrery_games` and `p1-swarm` at the run's own commit, hashed
# together. That is the subtree the false-positive rate is a property of, and it
# makes one boundary auditable that a commit sha alone does not — hours banked
# before `orrery_games` became the swarm's ruleset ran stage 1 against an empty
# invariant slice (docs/11-roadmap.md §P4), so they are not hours of the same
# thing. `total` groups by this digest rather than summing across it.
#
# ── Banking clauses ─────────────────────────────────────────────────────────
#
# `p1-swarm` already exits non-zero unless every clause holds, so a failed run
# never reaches this script through `p4-accumulate.sh`. The clauses are checked
# again here anyway, against the report rather than against an exit code: the
# ledger is the evidence, `--report-only` exists and exits zero on a failed run,
# and a hand-appended report is exactly the case where "the caller checked" is
# not a fact about the file.
set -euo pipefail

readonly NAME=p4-ledger
die() { echo "$NAME: $*" >&2; exit 2; }
note() { echo "$NAME: $*" >&2; }

usage() {
  cat >&2 <<'USAGE'
usage: p4-ledger.sh append <report.json>   bank one p1-swarm report
       p4-ledger.sh total                  running totals, grouped by pipeline
       p4-ledger.sh --self-test            structural + functional self-check

  P4_LEDGER_FILE   ledger path (default: target/p4-ledger/hours.jsonl)
  P4_PIPELINE_ID   override the pipeline digest (self-test only)
USAGE
}

# The floor `p1-swarm` itself judges coverage against; restated because a report
# is banked on what it says, not on who handed it over.
readonly MIN_COVERAGE=0.95
# The criterion's injected impairment: 3–5% packet loss, 100 ms jitter spikes.
readonly BAND_LO=0.03
readonly BAND_HI=0.05

self_test() {
  # Structural half, in the house style: the haystack is the script *body*, not
  # the whole file, because every pattern below also appears in the line that
  # looks for it and an unrestricted `grep -F -- "$1" "$0"` would match its own
  # source and pass unconditionally — the anti-pattern fixed repo-wide in #35.
  # Comment lines are stripped for the same reason: the commentary below names
  # the clauses it guards while explaining them.
  local body
  body="$(sed -n '/^readonly ROOT=/,$p' "$0" | grep -v '^[[:space:]]*#')"
  has() { grep -Fq -- "$1" <<<"$body"; }

  has '.identity' \
    || die 'self-test: the dedup key is no longer the run identity'
  has 'witnessing' \
    || die 'self-test: an unwitnessed run is no longer refused; it would bank hours no witness watched'
  has 'total_false_positives' \
    || die 'self-test: the false-positive clause is gone; a run that accused an honest peer would bank'
  has 'observation_coverage' \
    || die 'self-test: the coverage clause is gone; a blind witness reports zero findings too'
  has 'deferral_ledger_balances' \
    || die 'self-test: the deferral-ledger clause is gone; an unattributable deficit would bank'
  has 'BAND_LO' \
    || die 'self-test: the 3–5% impairment band is no longer checked; a clean-link hour would bank'
  has 'pipeline' \
    || die 'self-test: the pipeline digest is gone; hours across incomparable pipelines would sum'
  has 'run_key' \
    || die 'self-test: the dedup lookup is gone; a re-dispatched nightly would double-count'
  has 'flock' \
    || die 'self-test: the append is no longer serialized'

  # Functional half. The structural checks above cannot tell a clause that is
  # read from one that is read and ignored, and every case below costs
  # milliseconds: the reports are synthetic, so nothing here runs a swarm.
  local dir
  dir="$(mktemp -d)"
  # shellcheck disable=SC2064  # expand now: $dir is what must be removed.
  trap "rm -rf '$dir'" EXIT
  export P4_LEDGER_FILE="$dir/hours.jsonl"
  export P4_PIPELINE_ID=selftestpipeline

  # A passing witnessed hour, with the jq expression in $2 applied on top.
  st_report() {
    jq -n --argjson seed "$1" '{
      identity: {
        seed: $seed,
        impairment: { loss: 0.03, jitter_ticks: 6, jitter_rate: 0.1, retransmit_ticks: 3 },
        target: "x86_64-unknown-linux-gnu",
        commit: "0000000000000000000000000000000000000000"
      },
      started_at_unix_secs: 1750000000,
      peers: 32, seconds: 3600, player_hours: 32.0,
      witnessing: true, total_false_positives: 0, observation_coverage: 1.0,
      deferral_ledger_balances: true, total_gaps: 164022, total_shed: 162
    }' > "$dir/r.json"
    if [[ -n $2 ]]; then
      jq "$2" "$dir/r.json" > "$dir/r.next.json"
      mv "$dir/r.next.json" "$dir/r.json"
    fi
    echo "$dir/r.json"
  }
  st_lines() { if [[ -r $P4_LEDGER_FILE ]]; then wc -l < "$P4_LEDGER_FILE"; else echo 0; fi; }
  st_bank() { "$0" append "$(st_report "$1" "$2")" >/dev/null 2>&1; }

  st_bank 1 '' || die 'self-test: a passing witnessed hour was refused'
  [[ $(st_lines) == 1 ]] || die 'self-test: a passing hour did not append exactly one line'

  # The same identity again — a re-dispatched nightly on the same commit.
  st_bank 1 '' || die 'self-test: a duplicate was reported as a failure rather than skipped'
  [[ $(st_lines) == 1 ]] || die 'self-test: the same run identity banked twice'

  # A different seed is a different run, and that is the whole point of the
  # sweep in p4-accumulate.sh.
  st_bank 2 '' || die 'self-test: a second seed was refused'
  [[ $(st_lines) == 2 ]] || die 'self-test: a distinct seed did not append a distinct line'

  # Each of these is a run that must add no hours at all. The count is checked
  # as well as the exit status: a refusal that has already written the line is
  # not a refusal.
  local before refusal
  before=$(st_lines)
  for refusal in \
    '.total_false_positives = 1' \
    '.witnessing = false' \
    '.observation_coverage = 0.90' \
    '.deferral_ledger_balances = false' \
    '.identity.impairment.loss = 0.0' \
    '.identity.impairment.loss = 0.20' \
    '.identity.impairment.jitter_rate = 0.0' \
    '.player_hours = 0'
  do
    if st_bank 3 "$refusal"; then
      die "self-test: a run with '$refusal' was banked; a failed run must add no hours"
    fi
    [[ $(st_lines) == "$before" ]] \
      || die "self-test: a refused run ('$refusal') still touched the ledger"
  done

  # And the total is a sum of what was banked, scoped to its pipeline.
  local total
  total="$("$0" total | grep -F 'pipeline selftestpipeline' | head -1)"
  grep -q 'hours 64' <<<"$total" \
    || die "self-test: two banked 32-hour runs did not total 64 ('$total')"

  rm -rf "$dir"
  trap - EXIT
  echo "$NAME: self-test passed"
}

readonly ROOT="$(cd "$(dirname "$0")/.." && pwd)"
readonly LEDGER="${P4_LEDGER_FILE:-$ROOT/target/p4-ledger/hours.jsonl}"

need() { command -v "$1" >/dev/null || die "$1 is required and not on PATH"; }

# The trees the false-positive rate is a property of: the witness that judges,
# the executor it re-executes on, the rules it re-executes, and the harness that
# drives all three. A change in any of them makes the hours before it hours of a
# different pipeline, which is why `total` groups by this rather than summing.
readonly PIPELINE_TREES=(
  crates/orrery_witness
  crates/orrery_core
  crates/orrery_games
  p1-swarm
)

pipeline_id() {
  local commit=$1
  if [[ -n ${P4_PIPELINE_ID:-} ]]; then
    echo "$P4_PIPELINE_ID"
    return
  fi
  git -C "$ROOT" rev-parse --verify --quiet "$commit^{commit}" >/dev/null \
    || die "commit $commit is not in this checkout; cannot hash the pipeline subtree it ran"
  local tree hashes='' hash
  for tree in "${PIPELINE_TREES[@]}"; do
    hash=$(git -C "$ROOT" rev-parse "$commit:$tree") || die "no tree $tree at $commit"
    hashes+="$tree=$hash"$'\n'
  done
  printf '%s' "$hashes" | sha256sum | cut -c1-16
}

cmd_append() {
  local report=${1:-}
  [[ -n $report && -r $report ]] || die "append: unreadable report '${report:-<none>}'"
  need jq; need sha256sum; need flock

  # Every clause read out of the report first, so a refusal can name the number
  # it refused on rather than saying "invalid".
  local witnessing fp coverage balances hours loss jitter_ticks jitter_rate
  witnessing=$(jq -r '.witnessing // false' "$report")
  fp=$(jq -r '.total_false_positives // 0' "$report")
  coverage=$(jq -r '.observation_coverage // 0' "$report")
  balances=$(jq -r '.deferral_ledger_balances // false' "$report")
  hours=$(jq -r '.player_hours // 0' "$report")
  loss=$(jq -r '.identity.impairment.loss // 0' "$report")
  jitter_ticks=$(jq -r '.identity.impairment.jitter_ticks // 0' "$report")
  jitter_rate=$(jq -r '.identity.impairment.jitter_rate // 0' "$report")

  # An unwitnessed hour is not one of the criterion's hours: what is being
  # measured is the witness pipeline's false-positive rate, and a run with
  # `witnessing: false` measured nothing.
  [[ $witnessing == true ]] \
    || die 'refusing to bank: the witness did not run, so this hour measured no false-positive rate'
  # Every signal against an honest peer is a false positive and the criterion is
  # zero of them. One is not fewer hours; it is no hours.
  [[ $fp == 0 ]] \
    || die "refusing to bank: $fp signal(s) raised against honest peers"
  # A witness that stopped watching also reports zero. Coverage is what tells
  # the two apart, and it is part of the exit gate for that reason.
  awk -v c="$coverage" -v m="$MIN_COVERAGE" 'BEGIN { exit !(c >= m) }' \
    || die "refusing to bank: observation coverage $coverage is below the $MIN_COVERAGE floor"
  # A coverage figure is only attributable if the deferral path's own arithmetic
  # closes; if it does not, some frame left by a door the report cannot name.
  [[ $balances == true ]] \
    || die 'refusing to bank: the deferral ledger does not balance, so coverage is a lower bound'
  # The criterion's hours are hours *under injected impairment*. A clean-link
  # hour is a fine run and is not one of these 500.
  awk -v l="$loss" -v lo="$BAND_LO" -v hi="$BAND_HI" 'BEGIN { exit !(l >= lo && l <= hi) }' \
    || die "refusing to bank: loss $loss is outside the criterion's $BAND_LO–$BAND_HI band"
  awk -v t="$jitter_ticks" -v r="$jitter_rate" 'BEGIN { exit !(t > 0 && r > 0) }' \
    || die "refusing to bank: no jitter was injected ($jitter_ticks ticks at rate $jitter_rate)"
  awk -v h="$hours" 'BEGIN { exit !(h > 0) }' \
    || die "refusing to bank: the run accumulated $hours player-hours"

  local commit key pipeline seed target
  commit=$(jq -r '.identity.commit // "unknown"' "$report")
  seed=$(jq -r '.identity.seed' "$report")
  target=$(jq -r '.identity.target' "$report")
  # The identity verbatim, canonicalized so that key stability does not depend
  # on the field order serde happens to emit.
  key=$(jq -cS '.identity' "$report" | sha256sum | cut -c1-16)
  pipeline=$(pipeline_id "$commit")

  mkdir -p "$(dirname "$LEDGER")"
  # One writer at a time. The nightly is a single job today, and a ledger whose
  # append is not atomic is a ledger that loses a line the first time it is not.
  exec 9>>"$LEDGER.lock"
  flock 9

  if [[ -r $LEDGER ]] && grep -Fq "\"run_key\":\"$key\"" "$LEDGER"; then
    note "already banked: run_key $key (seed $seed, commit ${commit:0:12}); nothing appended"
    return 0
  fi

  jq -c \
    --arg key "$key" \
    --arg pipeline "$pipeline" \
    --arg banked_at "$(date -u +%Y-%m-%dT%H:%M:%SZ)" \
    '{
      schema: 1,
      run_key: $key,
      pipeline: $pipeline,
      banked_at: $banked_at,
      seed: .identity.seed,
      impairment: .identity.impairment,
      target: .identity.target,
      commit: .identity.commit,
      started_at_unix_secs: .started_at_unix_secs,
      peers: .peers,
      seconds: .seconds,
      player_hours: .player_hours,
      observation_coverage: .observation_coverage,
      false_positives: .total_false_positives,
      gaps_repaired: .total_gaps,
      shed: .total_shed
    }' "$report" >> "$LEDGER"

  note "banked $hours player-hours: run_key $key, seed $seed, loss $loss, target $target, pipeline $pipeline"
}

cmd_total() {
  need jq
  [[ -r $LEDGER ]] || { note "no ledger at $LEDGER; nothing banked yet"; return 0; }

  # Grouped by pipeline *and* target, and never summed across the first: hours
  # are only comparable within a pipeline version, and the criterion's "across
  # all three platforms" is a statement about the second.
  jq -rs '
    group_by(.pipeline + " " + .target)
    | map({
        pipeline: .[0].pipeline,
        target: .[0].target,
        runs: length,
        hours: (map(.player_hours) | add),
        commits: (map(.commit[0:12]) | unique | length)
      })
    | sort_by(.pipeline, .target)[]
    | "pipeline \(.pipeline)  target \(.target)  runs \(.runs)  commits \(.commits)  hours \(.hours)"
  ' "$LEDGER"

  jq -rs '
    "— " + (length | tostring) + " banked run(s), "
    + (map(.player_hours) | add | tostring) + " player-hours in total; "
    + (map(.target) | unique | length | tostring) + " target(s), "
    + (map(.pipeline) | unique | length | tostring) + " pipeline version(s)"
  ' "$LEDGER"

  # The criterion is 500 hours across all three platforms, so a per-pipeline sum
  # on one target is a progress figure and not the gate. Printed as one.
  jq -rs '
    group_by(.pipeline) | map({p: .[0].pipeline, h: (map(.player_hours) | add)})
    | sort_by(-.h)[]
    | "  pipeline \(.p): \(.h) of 500 hours (\((.h * 100 / 500) | floor)%)"
  ' "$LEDGER"
}

case ${1:-} in
  --self-test) self_test ;;
  append) shift; cmd_append "$@" ;;
  total) shift; cmd_total "$@" ;;
  -h | --help) usage ;;
  *) usage; die "unknown command '${1:-<none>}'" ;;
esac
