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
# provenance for one run. It is the append key, so restoring a shard or
# re-dispatching the exact same run adds no second line. The criterion counts
# measurements, however: pipeline digest + seed + impairment + target. A
# re-measurement at a different commit of the same pipeline keeps its useful
# provenance line but must not add a second hour of evidence. The identity
# carries no wall clock on purpose (`--stamp-wall-clock` puts that outside it).
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

  has 'measurement_key' \
    || die 'self-test: the measurement key is gone; a re-measurement could count as a second hour'
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
  has 'unique_by(.run_key)' \
    || die 'self-test: total no longer dedups provenance across shards; a restored shard would double its hours'
  has 'unique_by(measurement)' \
    || die 'self-test: total no longer counts distinct measurements; a re-measurement would double its hours'
  has 'def platform' \
    || die 'self-test: the target-to-platform fold is gone; the criterion is counted per platform'
  has 'MISSING' \
    || die 'self-test: total no longer names the platforms at zero; a Linux-only ledger would read as progress'
  has 'command -v shasum' \
    || die 'self-test: the coreutils-free digest fallback is gone; this cannot run on a macOS runner'
  has 'mkdir "$LEDGER.lock.d"' \
    || die 'self-test: the flock-free lock is gone; the append cannot be serialized off Linux'

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
  # `awk END{print NR}`, not `wc -l`: BSD wc pads its count to a fixed width,
  # so on macOS this returned "       1" and every `[[ $(st_lines) == 1 ]]`
  # below compared a padded string against a bare one and failed. That is
  # exactly how the nightly's macOS leg failed while Linux stayed green.
  st_lines() { if [[ -r $P4_LEDGER_FILE ]]; then awk 'END { print NR }' "$P4_LEDGER_FILE"; else echo 0; fi; }
  st_bank() { "$0" append "$(st_report "$1" "$2")" >/dev/null 2>&1; }

  st_bank 1 '' || die 'self-test: a passing witnessed hour was refused'
  [[ $(st_lines) == 1 ]] || die 'self-test: a passing hour did not append exactly one line'

  # The same identity again — a re-dispatched nightly on the same commit.
  st_bank 1 '' || die 'self-test: a duplicate was reported as a failure rather than skipped'
  [[ $(st_lines) == 1 ]] || die 'self-test: the same run identity banked twice'

  # A re-measurement at another commit is useful provenance but not another
  # measured hour: the pipeline, seed, impairment and target have not changed.
  st_bank 1 '.identity.commit = "1111111111111111111111111111111111111111"' \
    || die 'self-test: a re-measurement on another commit was refused'
  [[ $(st_lines) == 2 ]] || die 'self-test: a re-measurement did not keep its provenance line'

  local total
  total="$("$0" total | grep -F 'pipeline selftestpipeline  target x86_64-unknown-linux-gnu' | head -1)"
  grep -q 'banked_runs 2.*measurements 1.*distinct_hours 32' <<<"$total" \
    || die "self-test: one measurement at two commits did not count as 32 distinct hours ('$total')"

  # A different seed is a different measurement, and that is the whole point of
  # the sweep in p4-accumulate.sh. This guards the measurement-key call site:
  # dropping seed from it makes this check fail even though both lines append.
  st_bank 2 '' || die 'self-test: a second seed was refused'
  [[ $(st_lines) == 3 ]] || die 'self-test: a distinct seed did not append a distinct line'
  total="$("$0" total | grep -F 'pipeline selftestpipeline  target x86_64-unknown-linux-gnu' | head -1)"
  grep -q 'banked_runs 3.*measurements 2.*distinct_hours 64' <<<"$total" \
    || die "self-test: two distinct seeds did not count as 64 distinct hours ('$total')"

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

  # ── The platform half of the criterion ────────────────────────────────────
  #
  # Two hours on one platform are not the criterion's hours however many there
  # are, so `total` has to say which platforms the figure is made of and name
  # the ones at zero. Both directions are checked, because a report that always
  # says MISSING is as useless as one that never does.
  local view
  view="$("$0" total 2>&1)"
  grep -q 'macos: 0 hours — MISSING' <<<"$view" \
    || die 'self-test: a Linux-only ledger did not name macOS as missing'
  grep -q '1 of 3 platforms represented' <<<"$view" \
    || die 'self-test: a Linux-only ledger did not say how many platforms it covers'

  st_bank 4 '.identity.target = "x86_64-pc-windows-msvc"' \
    || die 'self-test: a Windows-target run was refused'
  st_bank 5 '.identity.target = "aarch64-apple-darwin"' \
    || die 'self-test: a macOS-target run was refused'
  view="$("$0" total 2>&1)"
  grep -q 'windows: 32 distinct hours (1 measurement(s), 32 banked; x86_64-pc-windows-msvc)' <<<"$view" \
    || die "self-test: hours on a Windows target were not attributed to the windows platform ('$view')"
  grep -q 'macos: 32 distinct hours (1 measurement(s), 32 banked; aarch64-apple-darwin)' <<<"$view" \
    || die "self-test: hours on aarch64-apple-darwin were not attributed to the macos platform ('$view')"
  grep -q 'all 3 platforms the criterion names have banked hours' <<<"$view" \
    || die "self-test: a ledger covering all three platforms did not say so ('$view')"

  # The two digest spellings have to be the same number: a run banked on a macOS
  # runner and the same run banked on Linux would otherwise carry two different
  # `run_key`s, and the deduplication that makes this ledger append-only would
  # silently stop working across platforms. Checked where both are installed,
  # which is every Linux runner and developer box.
  if command -v sha256sum >/dev/null && command -v shasum >/dev/null; then
    local gnu perl
    gnu=$(printf 'orrery' | sha256sum | cut -c1-16)
    perl=$(printf 'orrery' | shasum -a 256 | cut -c1-16)
    [[ $gnu == "$perl" ]] \
      || die "self-test: sha256sum and shasum disagree ($gnu vs $perl); a run key would depend on its platform"
  fi

  # The nightly banks on three runners, each carrying its own shard, and `total`
  # runs over the concatenation. A shard restored twice must not double its
  # hours — `append` dedups within a file and cannot see across them.
  local merged doubled
  merged="$(cat "$P4_LEDGER_FILE" "$P4_LEDGER_FILE")"
  printf '%s\n' "$merged" > "$dir/merged.jsonl"
  doubled="$(P4_LEDGER_FILE="$dir/merged.jsonl" "$0" total 2>&1)"
  [[ $(grep -c . <<<"$doubled") == $(grep -c . <<<"$view") ]] \
    || die 'self-test: a doubled shard changed the shape of the total'
  diff <(echo "$view") <(echo "$doubled") >/dev/null \
    || die "self-test: concatenating a shard with itself changed the totals; run_key dedup is not applied"

  # The measurement key includes its target, rather than collapsing the three
  # platform legs that deliberately run the same seed on the same night. Last,
  # because it adds a line the shard comparison above is holding still.
  before=$(st_lines)
  st_bank 1 '.identity.target = "x86_64-pc-windows-msvc"' \
    || die 'self-test: the same seed run on a second platform was refused'
  (( $(st_lines) == before + 1 )) \
    || die 'self-test: one seed on two targets banked once; the measurement key lost its target'

  rm -rf "$dir"
  trap - EXIT
  echo "$NAME: self-test passed"
}

readonly ROOT="$(cd "$(dirname "$0")/.." && pwd)"
readonly LEDGER="${P4_LEDGER_FILE:-$ROOT/target/p4-ledger/hours.jsonl}"

need() { command -v "$1" >/dev/null || die "$1 is required and not on PATH"; }

# ── Running off Linux ────────────────────────────────────────────────────────
#
# The criterion is "across all three platforms", so this script has to run on a
# `macos-latest` and a `windows-latest` runner as well as on the box. Two of the
# tools it reached for are GNU coreutils/util-linux and are not on either:
#
#   * `sha256sum` — a stock macOS ships `shasum` instead. Both are SHA-256 over
#     stdin printing the hash first, so the only difference is the name; the
#     digest is the dedup key, and a key that differed by platform would bank
#     every hour twice.
#   * `flock` — util-linux, absent on macOS and on the Git Bash that runs this
#     on a Windows runner.
#
# Neither fallback weakens anything: the digest is the same number, and the
# `mkdir` lock is the atomic primitive every platform's filesystem provides.
sha256_hex() {
  if command -v sha256sum >/dev/null; then
    sha256sum
  elif command -v shasum >/dev/null; then
    shasum -a 256
  else
    die 'neither sha256sum nor shasum is on PATH; cannot compute the run key'
  fi
}

ledger_lock() {
  if command -v flock >/dev/null; then
    exec 9>>"$LEDGER.lock"
    flock 9
    return
  fi
  # A stale directory blocks rather than corrupts, which is the right way round
  # for an append-only ledger; the wait is bounded so a stale one is a loud
  # failure and not a hung nightly.
  local waited=0
  until mkdir "$LEDGER.lock.d" 2>/dev/null; do
    waited=$(( waited + 1 ))
    (( waited > 60 )) && die "the ledger lock $LEDGER.lock.d is still held after ${waited}s"
    sleep 1
  done
  # shellcheck disable=SC2064  # expand now: this is the directory to remove.
  trap "rmdir '$LEDGER.lock.d' 2>/dev/null || true" EXIT
}

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
  printf '%s' "$hashes" | sha256_hex | cut -c1-16
}

cmd_append() {
  local report=${1:-}
  [[ -n $report && -r $report ]] || die "append: unreadable report '${report:-<none>}'"
  need jq

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

  local commit key measurement_key pipeline seed target
  commit=$(jq -r '.identity.commit // "unknown"' "$report")
  seed=$(jq -r '.identity.seed' "$report")
  target=$(jq -r '.identity.target' "$report")
  # `run_key` is provenance for an individual run. It intentionally includes
  # commit, so an independently re-run report is retained for reproducibility.
  key=$(jq -cS '.identity' "$report" | sha256_hex | cut -c1-16)
  pipeline=$(pipeline_id "$commit")
  # P4's denominator is measurements rather than runs. The digest is the
  # comparable-pipeline boundary; within it, these are the seeded inputs that
  # choose a simulated hour. Canonicalize before hashing so JSON field order
  # cannot alter the count.
  measurement_key=$(jq -cS --arg pipeline "$pipeline" \
    '{pipeline: $pipeline, seed: .identity.seed, impairment: .identity.impairment,
      target: .identity.target}' "$report" | sha256_hex | cut -c1-16)

  mkdir -p "$(dirname "$LEDGER")"
  # One writer at a time. The nightly is a single job today, and a ledger whose
  # append is not atomic is a ledger that loses a line the first time it is not.
  ledger_lock

  if [[ -r $LEDGER ]] && grep -Fq "\"run_key\":\"$key\"" "$LEDGER"; then
    note "already banked: run_key $key (seed $seed, commit ${commit:0:12}); nothing appended"
    return 0
  fi

  jq -c \
    --arg key "$key" \
    --arg measurement_key "$measurement_key" \
    --arg pipeline "$pipeline" \
    --arg banked_at "$(date -u +%Y-%m-%dT%H:%M:%SZ)" \
    '{
      schema: 1,
      run_key: $key,
      measurement_key: $measurement_key,
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

  note "banked $hours player-hours: run_key $key, measurement_key $measurement_key, seed $seed, loss $loss, target $target, pipeline $pipeline"
}

# The criterion's figure, and the thing `total` is progress against.
readonly HOURS_GOAL=500

# Shared between all three views below.
#
#   * `banked` — `append` refuses a duplicate provenance run within one ledger file and cannot
#     see across files. The nightly banks on three runners now, each keeping its
#     own shard artifact, and `total` is run over the concatenation; without this
#     a shard downloaded twice would double every hour it holds. `run_key` is the
#     run identity verbatim, so this is the same de-duplication `append` performs.
#   * `distinct` — the criterion's denominator. Multiple provenance runs can
#     re-measure one deterministic hour; `measurement_key` collapses those on
#     pipeline + seed + impairment + target. Old ledger lines predate that field,
#     so their equivalent canonical tuple is used while they remain readable.
#   * `platform` — the report stamps a target *triple*; the criterion speaks of
#     *platforms*. aarch64 and x86_64 macOS are one platform and two triples. The
#     per-triple lines keep the distinction visible; this fold is what the
#     criterion is actually counted against.
readonly JQ_PRELUDE='
  def banked: unique_by(.run_key);
  def measurement:
    (.measurement_key // ([.pipeline, .seed, .impairment, .target] | tojson));
  def distinct: banked | unique_by(measurement);
  # serde emits 32.0 where the swarm accumulated exactly 32 hours, and jq keeps
  # the literal. A ledger read by a human should not print two spellings of the
  # same number next to each other.
  def hrs: (. * 1000 | round) / 1000;
  def platform:
    if test("linux") then "linux"
    elif test("windows") then "windows"
    elif test("darwin") then "macos"
    else "other" end;
'

cmd_total() {
  need jq
  [[ -r $LEDGER ]] || { note "no ledger at $LEDGER; nothing banked yet"; return 0; }

  # Grouped by pipeline *and* target, and never summed across the first: hours
  # are only comparable within a pipeline version, and the criterion's "across
  # all three platforms" is a statement about the second. Show provenance and
  # measurement counts together so a re-measurement cannot masquerade as a new
  # hour of evidence.
  jq -rs "$JQ_PRELUDE"'
    banked
    | group_by(.pipeline + " " + .target)
    | map({
        pipeline: .[0].pipeline,
        target: .[0].target,
        banked_runs: length,
        banked_hours: (map(.player_hours) | add | hrs),
        measurements: (unique_by(measurement) | length),
        distinct_hours: (unique_by(measurement) | map(.player_hours) | add | hrs),
        commits: (map(.commit[0:12]) | unique | length)
      })
    | sort_by(.pipeline, .target)[]
    | "pipeline \(.pipeline)  target \(.target)  banked_runs \(.banked_runs)  commits \(.commits)  measurements \(.measurements)  distinct_hours \(.distinct_hours)  banked_hours \(.banked_hours)"
  ' "$LEDGER"

  jq -rs "$JQ_PRELUDE"'
    banked as $banked
    | distinct as $distinct
    | "— " + ($banked | length | tostring) + " banked provenance run(s), "
    + ($banked | map(.player_hours) | add | hrs | tostring) + " banked player-hours; "
    + ($distinct | length | tostring) + " distinct measurement(s), "
    + ($distinct | map(.player_hours) | add | hrs | tostring) + " distinct player-hours across pipeline versions (not a figure against the 500); "
    + ($banked | map(.target) | unique | length | tostring) + " target(s), "
    + ($banked | map(.target | platform) | unique | length | tostring) + " platform(s), "
    + ($banked | map(.pipeline) | unique | length | tostring) + " pipeline version(s)"
  ' "$LEDGER"

  # ── Progress, per platform, because that is the shape of the criterion ──────
  #
  # "≥ 500 honest player-hours across all three platforms" is not one number: a
  # pipeline holding 500 hours of which every one is Linux has not met it, and a
  # single figure says it has. So each pipeline prints its running total *and*
  # the platforms that total is made of, naming the ones at zero — the missing
  # platform is the binding constraint on this criterion and it should not take
  # arithmetic to see it.
  #
  # What is deliberately *not* asserted here is how the 500 divide. The roadmap
  # says "≥ 500 … across all three platforms" and does not say whether that is
  # 500 in total with every platform represented or 500 apiece; this prints both
  # halves and leaves the reading to the record rather than inventing a gate.
  jq -rs --argjson goal "$HOURS_GOAL" "$JQ_PRELUDE"'
    banked as $banked
    | distinct
    | group_by(.pipeline)
    | map(. as $measurements | {
        p: .[0].pipeline,
        measurements: length,
        h: (map(.player_hours) | add | hrs),
        terms: (map(.player_hours | tostring) | join(" + ")),
        banked: ($banked | map(select(.pipeline == $measurements[0].pipeline))),
        by: (group_by(.target | platform)
             | map(. as $platform_measurements | {
                     k: (.[0].target | platform),
                     h: (map(.player_hours) | add | hrs),
                     measurements: length,
                     banked_h: ($banked
                                | map(select(.pipeline == $platform_measurements[0].pipeline
                                             and ((.target | platform) == ($platform_measurements[0].target | platform))))
                                | map(.player_hours) | add | hrs),
                     triples: (map(.target) | unique | join(", ")) }))
      })
    | sort_by(-.h)[]
    | . as $g
    | "  pipeline \($g.p): \($g.h) distinct hours = \($g.terms) (\($g.measurements) distinct measurement(s)); \($g.banked | length) provenance run(s) / \($g.banked | map(.player_hours) | add | hrs) banked hours; \($g.h) of \($goal) hours (\(($g.h * 100 / $goal) | floor)%)",
      ( ["linux", "windows", "macos"]
        | map(. as $want | { k: $want, hit: ($g.by | map(select(.k == $want)) | first) })
        | map(if .hit
              then "    \(.k): \(.hit.h) distinct hours (\(.hit.measurements) measurement(s), \(.hit.banked_h) banked; \(.hit.triples))"
              else "    \(.k): 0 hours — MISSING, and the criterion names it"
              end)[] ),
      ( ($g.by | map(select(.k == "other")) | .[]
         | "    unrecognised target(s): \(.triples) — \(.h) hours counted against no platform") ),
      ( ($g.by | map(select(.k != "other")) | length) as $covered
        | if $covered == 3
          then "    all 3 platforms the criterion names have banked hours"
          else "    \($covered) of 3 platforms represented; the criterion cannot be met until all 3 are"
          end )
  ' "$LEDGER"
}

case ${1:-} in
  --self-test) self_test ;;
  append) shift; cmd_append "$@" ;;
  total) shift; cmd_total "$@" ;;
  -h | --help) usage ;;
  *) usage; die "unknown command '${1:-<none>}'" ;;
esac
