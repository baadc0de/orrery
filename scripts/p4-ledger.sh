#!/usr/bin/env bash
# P4's append-only player-hour ledger (docs/11-roadmap.md §P4).
#
# The phase does not exit until ≥ 500 honest player-hours under injected
# impairment produce zero false-positive reports. `gates/p1-swarm --witness` produces
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
# measurements, however: bots are deterministic, so their key is pipeline
# digest + bot + seed + impairment + target. A re-measurement at a different
# commit of the same pipeline keeps its useful provenance line but must not add
# a second hour of evidence. Human input is not deterministic, so a human key
# adds its session identity rather than collapsing two people who used one seed.
# The identity carries no wall clock on purpose (`--stamp-wall-clock` puts that
# outside it).
#
# A human report must carry `identity.human_session_id`, a coordinator-issued
# UUIDv7 allocated once under the coordinator's unique session-id constraint.
# It is not derived from the seed. The coordinator cannot issue it twice; the
# ledger also rejects a malformed value, so a weak timestamp or display name
# cannot quietly become the distinguishing field. #328 must retain this field
# verbatim in its session record when it sends the report here.
#
# Hours are only comparable within a pipeline version, so every line also
# carries a `pipeline` digest: the git tree hashes of `orrery_witness`,
# `orrery_core`, `orrery_games` and `gates/p1-swarm` at the run's own commit, hashed
# together. That is the subtree the false-positive rate is a property of, and it
# makes one boundary auditable that a commit sha alone does not — hours banked
# before `orrery_games` became the swarm's ruleset ran stage 1 against an empty
# invariant slice (docs/11-roadmap.md §P4), so they are not hours of the same
# thing. `total` groups by this digest rather than summing across it.
#
# ── Banking clauses ─────────────────────────────────────────────────────────
#
# `gates/p1-swarm` already exits non-zero unless every clause holds, so a failed run
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
usage: p4-ledger.sh append <report.json>   bank one gates/p1-swarm report
       p4-ledger.sh total                  running totals, grouped by pipeline
       p4-ledger.sh shakedown              #329's unbanked-shakedown evidence report
       p4-ledger.sh freeze <baseline> [candidate]
                                            verify PIPELINE_TREES stayed frozen;
                                            candidate defaults to HEAD
       p4-ledger.sh --self-test            structural + functional self-check

  P4_LEDGER_FILE   ledger path (default: target/p4-ledger/hours.jsonl)
  P4_PIPELINE_ID   override the pipeline digest (self-test only)
USAGE
}

# The floor `gates/p1-swarm` itself judges coverage against; restated because a report
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
  has 'human_session_id' \
    || die 'self-test: the human session identity is gone; same-seed human hours would collapse'
  has 'actor' \
    || die 'self-test: the bot|human dimension is gone; P4 cannot check its required mix'
  has 'validate_session_record' \
    || die 'self-test: campaign session rows are no longer validated before banking'
  has 'impairment_mismatch ==' \
    || die 'self-test: the mismatch flag is no longer checked against the row'\''s own numbers; a post-hoc edit of the measured impairment would bank'
  has 'def platform' \
    || die 'self-test: the target-to-platform fold is gone; the criterion is counted per platform'
  has 'MISSING' \
    || die 'self-test: total no longer names the platforms at zero; a Linux-only ledger would read as progress'
  has 'command -v shasum' \
    || die 'self-test: the coreutils-free digest fallback is gone; this cannot run on a macOS runner'
  has 'mkdir "$LEDGER.lock.d"' \
    || die 'self-test: the flock-free lock is gone; the append cannot be serialized off Linux'
  has 'cmd_shakedown' \
    || die 'self-test: #329 no longer has a shakedown report command'
  has 'cmd_freeze' \
    || die 'self-test: #329 no longer has a freeze-window verifier'

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
  # $3/$4 are actor and the optional human session identity. Human identities
  # below are valid UUIDv7s with different counter-bearing low bits.
  st_report() {
    jq -n --argjson seed "$1" --arg actor "${3:-bot}" --arg session "${4:-}" '{
      identity: {
        seed: $seed,
        impairment: { loss: 0.03, jitter_ticks: 6, jitter_rate: 0.1, retransmit_ticks: 3 },
        target: "x86_64-unknown-linux-gnu",
        commit: "0000000000000000000000000000000000000000",
        actor: $actor
      },
      started_at_unix_secs: 1750000000,
      peers: 32, seconds: 3600, player_hours: 32.0,
      witnessing: true, total_false_positives: 0, observation_coverage: 1.0,
      deferral_ledger_balances: true, total_gaps: 164022, total_shed: 162
    } | if $session == "" then . else .identity.human_session_id = $session end' > "$dir/r.json"
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
  st_bank_as() { "$0" append "$(st_report "$1" "$2" "$3" "$4")" >/dev/null 2>&1; }

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

  # Human input is the counterexample to bot seed deduplication. These have
  # identical seeded conditions but coordinator-issued, distinct session IDs,
  # so they are two measurements. The filter in the mutation proof below names
  # this exact check and the call site it has to break.
  st_bank_as 6 '' human '018f8f4e-5c90-7abc-8123-000000000001' \
    || die 'self-test: first human session was refused'
  st_bank_as 6 '.identity.commit = "2222222222222222222222222222222222222222"' human '018f8f4e-5c90-7abc-8123-000000000002' \
    || die 'self-test: second same-seed human session was refused'
  total="$("$0" total | grep -F 'pipeline selftestpipeline  target x86_64-unknown-linux-gnu' | head -1)"
  grep -q 'banked_runs 5.*measurements 4.*distinct_hours 128' <<<"$total" \
    || die "self-test: two same-seed human sessions did not count as two hours ('$total')"

  # The old behaviour remains deliberate for bots: independent provenance
  # reports for one seed are one deterministic measurement, even if a caller
  # supplies irrelevant session-looking metadata.
  st_bank_as 7 '' bot '018f8f4e-5c90-7abc-8123-000000000003' \
    || die 'self-test: first bot run was refused'
  st_bank_as 7 '.identity.commit = "3333333333333333333333333333333333333333"' bot '018f8f4e-5c90-7abc-8123-000000000004' \
    || die 'self-test: second same-seed bot run was refused'
  total="$("$0" total | grep -F 'pipeline selftestpipeline  target x86_64-unknown-linux-gnu' | head -1)"
  grep -q 'banked_runs 7.*measurements 5.*distinct_hours 160' <<<"$total" \
    || die "self-test: two same-seed bot runs did not count as one hour ('$total')"
  view="$("$0" total 2>&1)"
  grep -q 'bot: 96 distinct hours = 32.0 + 32.0 + 32.0 (3 distinct measurement(s))' <<<"$view" \
    || die "self-test: the pipeline bot-hour breakdown is wrong ('$view')"
  grep -q 'human: 64 distinct hours = 32.0 + 32.0 (2 distinct measurement(s))' <<<"$view" \
    || die "self-test: the pipeline human-hour breakdown is wrong ('$view')"
  grep -q 'human mix: 64 / 160 distinct hours = 40% (requires ≥25%)' <<<"$view" \
    || die "self-test: the pipeline human mix arithmetic is wrong ('$view')"

  # A campaign row rides this append path; it is not a parallel ledger. The
  # malformed `afk_capped` value must refuse before a line is written.
  st_report 10 '.player_hours = 1 | .seconds = 3600 | .peers = 1
    | .session = {
        session_id: "018f8f4e-5c90-7abc-8123-000000000010",
        wall_start: "2026-08-23T12:00:00Z", wall_end: "2026-08-23T13:00:00Z",
        distinct_play_minutes: 60, banked_minutes: 60,
        platform_triple: "x86_64-unknown-linux-gnu", client_rev: "self-test",
        ruleset_id: "52", ruleset_version: 2, pipeline_digest: "selftestpipeline",
        actor: "human", configured_impairment_profile: {loss_pct: 3, jitter_p50_ms: 100, jitter_p99_ms: 100},
        observed_loss_pct: 3, observed_jitter_p50_ms: 100, observed_jitter_p99_ms: 100,
        afk_seconds: 0, afk_capped: false, impairment_mismatch: false
      }
    | .identity.human_session_id = "018f8f4e-5c90-7abc-8123-000000000010"' human '018f8f4e-5c90-7abc-8123-000000000010' >/dev/null
  "$0" append "$dir/r.json" || die 'self-test: complete campaign session row was refused'
  jq '.session.afk_capped = "false"' "$dir/r.json" > "$dir/bad-session.json"
  if "$0" append "$dir/bad-session.json" >/dev/null 2>&1; then
    die 'self-test: malformed campaign session row banked'
  fi
  # Tamper-evidence for the measured impairment (#387): editing the observed
  # figure after the fact leaves the mismatch flag contradicting the numbers,
  # and flipping the flag on an agreeing row is the same lie the other way.
  # Both must refuse. The refusal must be the arithmetic check, not the
  # field-shape one, so each row is otherwise well-formed.
  jq '.session.observed_loss_pct = 0' "$dir/r.json" > "$dir/tampered-observed.json"
  if "$0" append "$dir/tampered-observed.json" >/dev/null 2>&1; then
    die 'self-test: a row whose observed impairment was edited post-hoc banked'
  fi
  jq '.session.impairment_mismatch = true' "$dir/r.json" > "$dir/tampered-flag.json"
  if "$0" append "$dir/tampered-flag.json" >/dev/null 2>&1; then
    die 'self-test: a row claiming a mismatch its own numbers refute banked'
  fi
  # And the honest direction still banks: a genuinely mismatching row that
  # says so is flagged evidence, not a refusal.
  st_report 11 '.player_hours = 1 | .seconds = 3600 | .peers = 1
    | .session = {
        session_id: "018f8f4e-5c90-7abc-8123-000000000011",
        wall_start: "2026-08-23T12:00:00Z", wall_end: "2026-08-23T13:00:00Z",
        distinct_play_minutes: 60, banked_minutes: 60,
        platform_triple: "x86_64-unknown-linux-gnu", client_rev: "self-test",
        ruleset_id: "52", ruleset_version: 2, pipeline_digest: "selftestpipeline",
        actor: "human", configured_impairment_profile: {loss_pct: 3, jitter_p50_ms: 100, jitter_p99_ms: 100},
        observed_loss_pct: 3.4, observed_jitter_p50_ms: 96, observed_jitter_p99_ms: 210,
        afk_seconds: 0, afk_capped: false, impairment_mismatch: true
      }
    | .identity.human_session_id = "018f8f4e-5c90-7abc-8123-000000000011"' human '018f8f4e-5c90-7abc-8123-000000000011' >/dev/null
  "$0" append "$dir/r.json" >/dev/null 2>&1 \
    || die 'self-test: an honestly flagged mismatching row was refused'

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

  # #329's report must name absent evidence as UNKNOWN, rather than treating a
  # ledger with no shakedown annotations as a clean shakedown.  Conversely, an
  # explicitly bad observation must be a named FAIL.  These are planted ledger
  # rows: this test does not run a campaign or fabricate campaign evidence.
  jq -n '
    ["linux", "windows", "macos"] | to_entries | map({
      run_key: ("shake-" + (.key | tostring)), measurement_key: ("shake-" + (.key | tostring)),
      pipeline: "shakedown-test", actor: "human", seed: (.key + 20),
      impairment: {loss: 0.03, jitter_ticks: 6, jitter_rate: 0.1, retransmit_ticks: 3},
      target: (if .value == "linux" then "x86_64-unknown-linux-gnu" elif .value == "windows" then "x86_64-pc-windows-msvc" else "aarch64-apple-darwin" end),
      player_hours: (if .key == 0 then 8 else 9 end),
      session: {actor: "human", configured_impairment_profile: {loss_pct: 3, jitter_p50_ms: 100, jitter_p99_ms: 100}, observed_loss_pct: 3, observed_jitter_p50_ms: 100, observed_jitter_p99_ms: 100, impairment_mismatch: false}
    })[]' > "$dir/shakedown.jsonl"
  local shakedown
  shakedown="$(P4_LEDGER_FILE="$dir/shakedown.jsonl" "$0" shakedown 2>&1 || true)"
  grep -q 'goldens green on all three platforms: UNKNOWN' <<<"$shakedown" \
    || die "self-test: absent golden evidence did not report UNKNOWN ('$shakedown')"
  jq '.session.shakedown = {phase: "unbanked", golden_pass: true, client_launch_pass: true, discrepancy_reports: 0, untriaged_discrepancy_reports: 0, triaged_real_reports: 0, tolerance_band_digest: "bands-v1"}' "$dir/shakedown.jsonl" > "$dir/shakedown.next.jsonl"
  mv "$dir/shakedown.next.jsonl" "$dir/shakedown.jsonl"
  shakedown="$(P4_LEDGER_FILE="$dir/shakedown.jsonl" "$0" shakedown 2>&1 || true)"
  grep -q 'impairment verified applied in every sampled session: PASS' <<<"$shakedown" \
    || die "self-test: agreeing observed impairment did not pass ('$shakedown')"
  cp "$dir/shakedown.jsonl" "$dir/shakedown.good.jsonl"
  st_shakedown_fail() {
    jq "$1" "$dir/shakedown.good.jsonl" > "$dir/shakedown.jsonl"
    shakedown="$(P4_LEDGER_FILE="$dir/shakedown.jsonl" "$0" shakedown 2>&1 || true)"
    grep -q "$2: FAIL" <<<"$shakedown" \
      || die "self-test: mutation '$1' did not fail named criterion '$2' ('$shakedown')"
  }
  st_shakedown_fail '.session.shakedown.golden_pass = false' \
    'goldens green on all three platforms'
  st_shakedown_fail 'if .target | test("windows") then .session.shakedown.client_launch_pass = false else . end' \
    'client launches on all three platforms'
  st_shakedown_fail '.session.shakedown.triaged_real_reports = 1' \
    'zero discrepancy reports triaged real over those hours'
  st_shakedown_fail 'if .target | test("windows") then .session.shakedown.tolerance_band_digest = "bands-v2" else . end' \
    'no tolerance band moved during them'
  st_shakedown_fail '.session.impairment_mismatch = true' \
    'impairment verified applied in every sampled session'

  # Mutation proof for the guarded freeze condition.  The second commit changes
  # a real PIPELINE_TREE, so matching the two endpoint digests would be a
  # surviving mutation, not a passing test.
  local freeze_repo base moved frozen
  freeze_repo="$dir/freeze-repo"
  mkdir -p "$freeze_repo"/{crates/orrery_witness,crates/orrery_core,crates/orrery_games,gates/p1-swarm}
  git -C "$freeze_repo" init -q
  git -C "$freeze_repo" config user.email self-test@invalid
  git -C "$freeze_repo" config user.name self-test
  touch "$freeze_repo"/{crates/orrery_witness,crates/orrery_core,crates/orrery_games,gates/p1-swarm}/kept
  git -C "$freeze_repo" add . && git -C "$freeze_repo" commit -qm baseline
  base=$(git -C "$freeze_repo" rev-parse HEAD)
  printf 'changed\n' > "$freeze_repo/crates/orrery_games/mutated"
  git -C "$freeze_repo" add . && git -C "$freeze_repo" commit -qm mutated-pipeline-tree
  moved=$(git -C "$freeze_repo" rev-parse HEAD)
  if P4_ROOT="$freeze_repo" P4_PIPELINE_ID=forged "$0" freeze "$base" "$moved" >"$dir/freeze.out" 2>&1; then
    die 'self-test: a changed PIPELINE_TREE passed the freeze verifier'
  fi
  grep -q 'freeze window: FAIL' "$dir/freeze.out" \
    || die 'self-test: the changed PIPELINE_TREE did not emit the named freeze failure'
  git -C "$freeze_repo" commit --allow-empty -qm unrelated-change
  frozen=$(git -C "$freeze_repo" rev-parse HEAD)
  P4_ROOT="$freeze_repo" P4_PIPELINE_ID= "$0" freeze "$moved" "$frozen" >"$dir/freeze.out" 2>&1 \
    || die 'self-test: an unchanged PIPELINE_TREE failed the freeze verifier'
  grep -q 'freeze window: PASS' "$dir/freeze.out" \
    || die 'self-test: unchanged endpoint digests did not emit the named freeze pass'

  rm -rf "$dir"
  trap - EXIT
  echo "$NAME: self-test passed"
}

readonly ROOT="${P4_ROOT:-$(cd "$(dirname "$0")/.." && pwd)}"
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
  gates/p1-swarm
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

# A campaign report has all the ordinary witnessing evidence plus one session
# row. Keep it on this append path so every existing refusal still applies.
validate_session_record() {
  local report=$1 actor=$2 human_session_id=$3 target=$4
  jq -e --arg actor "$actor" --arg session "$human_session_id" --arg target "$target" '
    if .session? == null then true else
      .session as $s
      | ($s.session_id | type == "string" and length > 0)
      and ($s.wall_start | type == "string" and length > 0)
      and ($s.wall_end | type == "string" and length > 0)
      and ($s.distinct_play_minutes | type == "number" and . >= 0)
      and ($s.banked_minutes | type == "number" and . >= 0 and . <= $s.distinct_play_minutes)
      and ($s.platform_triple == $target)
      and ($s.client_rev | type == "string" and length > 0)
      and ($s.ruleset_id | type == "string" and length > 0)
      and ($s.ruleset_version | type == "number")
      and ($s.pipeline_digest | type == "string" and length > 0)
      and ($s.actor == $actor)
      and ($s.configured_impairment_profile.loss_pct | type == "number")
      and ($s.configured_impairment_profile.jitter_p50_ms | type == "number")
      and ($s.configured_impairment_profile.jitter_p99_ms | type == "number")
      and ($s.observed_loss_pct | type == "number")
      and ($s.observed_jitter_p50_ms | type == "number")
      and ($s.observed_jitter_p99_ms | type == "number")
      and ($s.afk_seconds | type == "number" and . >= 0)
      and ($s.afk_capped | type == "boolean")
      and ($s.impairment_mismatch | type == "boolean")
      and (if $actor == "human" then $s.session_id == $session else true end)
    end
  ' "$report" >/dev/null \
    || die 'refusing to bank: incomplete or inconsistent campaign session row'
  # The mismatch flag is recomputable from the row's own numbers, and #387
  # requires that it *fired* whenever observation disagrees with
  # configuration. Checking the arithmetic here is what makes the flag
  # tamper-evident: a post-hoc edit of observed_loss_pct (to hide a mismatch,
  # or to fake one) leaves the flag contradicting the numbers next to it, and
  # a row whose own fields disagree with each other is not evidence.
  jq -e '
    if .session? == null then true else
      .session as $s
      | ($s.impairment_mismatch ==
          (($s.observed_loss_pct != $s.configured_impairment_profile.loss_pct)
           or ($s.observed_jitter_p50_ms != $s.configured_impairment_profile.jitter_p50_ms)
           or ($s.observed_jitter_p99_ms != $s.configured_impairment_profile.jitter_p99_ms)))
    end
  ' "$report" >/dev/null \
    || die 'refusing to bank: session impairment_mismatch contradicts the row'\''s own observed/configured impairment'
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

  local commit key measurement_key pipeline seed target actor human_session_id
  commit=$(jq -r '.identity.commit // "unknown"' "$report")
  seed=$(jq -r '.identity.seed' "$report")
  target=$(jq -r '.identity.target' "$report")
  # Existing swarm reports predate the dimension and are all deterministic bot
  # runs. Keep them readable and bankable as bots; human reports must opt in.
  actor=$(jq -r '.identity.actor // "bot"' "$report")
  [[ $actor == bot || $actor == human ]] \
    || die "refusing to bank: identity.actor must be bot or human, got '$actor'"
  human_session_id=$(jq -r '.identity.human_session_id // empty' "$report")
  if [[ $actor == human ]]; then
    [[ $human_session_id =~ ^[0-9a-f]{8}-[0-9a-f]{4}-7[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$ ]] \
      || die 'refusing to bank: a human hour needs a coordinator-issued UUIDv7 identity.human_session_id'
  fi
  validate_session_record "$report" "$actor" "$human_session_id" "$target"
  # `run_key` is provenance for an individual run. It intentionally includes
  # commit, so an independently re-run report is retained for reproducibility.
  key=$(jq -cS '.identity' "$report" | sha256_hex | cut -c1-16)
  pipeline=$(pipeline_id "$commit")
  if jq -e '.session? != null and .session.pipeline_digest != $pipeline' \
    --arg pipeline "$pipeline" "$report" >/dev/null; then
    die 'refusing to bank: session pipeline_digest does not name the pipeline this report ran'
  fi
  # P4's denominator is measurements rather than runs. The digest is the
  # comparable-pipeline boundary; within it bot inputs choose a deterministic
  # simulated hour. A human session is deliberately an additional input: two
  # humans on the same seed are two pieces of false-positive evidence.
  # Canonicalize before hashing so JSON field order cannot alter the count.
  measurement_key=$(jq -cS --arg pipeline "$pipeline" --arg actor "$actor" \
    '{pipeline: $pipeline, actor: $actor, seed: .identity.seed,
      impairment: .identity.impairment, target: .identity.target}
     + (if $actor == "human" then {human_session_id: .identity.human_session_id} else {} end)' \
    "$report" | sha256_hex | cut -c1-16)

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
      schema: 2,
      run_key: $key,
      measurement_key: $measurement_key,
      pipeline: $pipeline,
      actor: (.identity.actor // "bot"),
      human_session_id: (if (.identity.actor // "bot") == "human" then .identity.human_session_id else null end),
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
    } + (if .session? == null then {} else {session: .session} end)' "$report" >> "$LEDGER"

  note "banked $hours $actor player-hours: run_key $key, measurement_key $measurement_key, seed $seed, loss $loss, target $target, pipeline $pipeline"
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
    (.measurement_key // ([.pipeline, (.actor // "bot"), .seed, .impairment,
                          .target, (.human_session_id // null)] | tojson));
  def actor: (.actor // "bot");
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
        commits: (map(.commit[0:12]) | unique | length),
        bot_measurements: (unique_by(measurement) | map(select(actor == "bot")) | length),
        human_measurements: (unique_by(measurement) | map(select(actor == "human")) | length)
      })
    | sort_by(.pipeline, .target)[]
    | "pipeline \(.pipeline)  target \(.target)  banked_runs \(.banked_runs)  commits \(.commits)  measurements \(.measurements) (bot \(.bot_measurements), human \(.human_measurements))  distinct_hours \(.distinct_hours)  banked_hours \(.banked_hours)"
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
        actors: (group_by(actor)
                 | map(. as $actor_measurements | {
                         k: (.[0] | actor),
                         h: (map(.player_hours) | add | hrs),
                         terms: (map(.player_hours | tostring) | join(" + ")),
                         measurements: length })),
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
      ( ["bot", "human"]
        | map(. as $want | { k: $want, hit: ($g.actors | map(select(.k == $want)) | first) })
        | map(if .hit
              then "    \(.k): \(.hit.h) distinct hours = \(.hit.terms) (\(.hit.measurements) distinct measurement(s))"
              else "    \(.k): 0 distinct hours = 0 (0 distinct measurement(s))"
              end)[] ),
      ( ($g.actors | map(select(.k == "human")) | first | .h // 0) as $human
        | "    human mix: \($human) / \($g.h) distinct hours = \(($human * 100 / $g.h) | floor)% (requires ≥25%)" ),
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

# #329 is deliberately an instrument, not a campaign runner.  Its annotations
# are not present in the current ledger schema, so every missing field below is
# surfaced as UNKNOWN with the exact field an operator/evidence writer must add.
# It uses `distinct` from JQ_PRELUDE: provenance re-measurements never become
# fresh shakedown hours, and no result ever combines pipeline digests.
cmd_shakedown() {
  need jq
  [[ -r $LEDGER ]] || {
    echo 'shakedown gate: no ledger evidence — every criterion is UNKNOWN'
    return 1
  }
  local report
  report=$(jq -rs "$JQ_PRELUDE"'
    def state($known; $bad):
      if $known == 0 then "UNKNOWN" elif $bad then "FAIL" else "PASS" end;
    distinct | group_by(.pipeline) | .[] | . as $rows
    | ($rows | map(select(.session.shakedown.phase? == "unbanked"))) as $sessions
    | "shakedown gate, pipeline \($rows[0].pipeline): \($rows | map(.player_hours) | add | hrs) distinct ledger hours (never combined with another pipeline)",
      (if ($sessions | length) == 0 then
         "  unbanked mixed cohort: UNKNOWN — needs session.shakedown.phase = \"unbanked\" to distinguish shakedown evidence from banked campaign rows"
       else
         ($sessions | map(.player_hours) | add | hrs) as $hours
         | ($sessions | map(select(actor == "human") | .player_hours) | add // 0 | hrs) as $human
         | ($sessions | map(.target | platform) | unique | length) as $platforms
         | "  unbanked mixed cohort: " + (if $hours >= 25 and $human >= 8 and $platforms == 3 then "PASS" else "FAIL" end)
           + " — \($hours) distinct hours; \($human) human; \($platforms) of 3 platforms (requires ≥25, ≥8 human, all 3)"
       end),
      ($sessions | map(select(.session.shakedown.golden_pass? != null))) as $goldens
      | "  goldens green on all three platforms: " + state(($goldens | length); (($goldens | any(.session.shakedown.golden_pass != true)) or (($goldens | map(.target | platform) | unique | length) != 3)))
        + (if ($goldens | length) == 0 then " — needs session.shakedown.golden_pass per platform" else " — \($goldens | length) sampled session(s)" end),
      ($sessions | map(select(.session.shakedown.client_launch_pass? != null))) as $launches
      | "  client launches on all three platforms: " + state(($launches | length); (($launches | any(.session.shakedown.client_launch_pass != true)) or (($launches | map(.target | platform) | unique | length) != 3)))
        + (if ($launches | length) == 0 then " — needs session.shakedown.client_launch_pass per platform" else " — \($launches | length) sampled session(s)" end),
      ($sessions | map(select(.session.shakedown.discrepancy_reports? != null and .session.shakedown.untriaged_discrepancy_reports? != null and .session.shakedown.triaged_real_reports? != null))) as $triage
      | "  zero discrepancy reports triaged real over those hours: " + state(($triage | length); ($triage | any(.session.shakedown.triaged_real_reports != 0 or .session.shakedown.untriaged_discrepancy_reports != 0)))
        + (if ($triage | length) == 0 then " — needs session.shakedown.discrepancy_reports, .untriaged_discrepancy_reports, and .triaged_real_reports" else " — \($triage | map(.session.shakedown.discrepancy_reports) | add) reports; \($triage | map(.session.shakedown.triaged_real_reports) | add) triaged real; \($triage | map(.session.shakedown.untriaged_discrepancy_reports) | add) untriaged" end),
      ($sessions | map(select(.session.shakedown.tolerance_band_digest? != null))) as $bands
      | "  no tolerance band moved during them: " + state(($bands | length); (($bands | map(.session.shakedown.tolerance_band_digest) | unique | length) != 1))
        + (if ($bands | length) == 0 then " — needs session.shakedown.tolerance_band_digest" else " — \($bands | map(.session.shakedown.tolerance_band_digest) | unique | join(", "))" end),
      ($sessions | map(select(.session.configured_impairment_profile? != null and .session.observed_loss_pct? != null and .session.observed_jitter_p50_ms? != null and .session.observed_jitter_p99_ms? != null and .session.impairment_mismatch? != null))) as $impairment
      | "  impairment verified applied in every sampled session: " + state(($impairment | length); ($impairment | any(.session.impairment_mismatch != false)))
        + (if ($impairment | length) == 0 then " — needs the existing session configured_impairment_profile, observed_* and impairment_mismatch fields" else " — \($impairment | length) sampled session(s); \($impairment | map(select(.session.impairment_mismatch == false)) | length) observed/configured matches" end)
  ' "$LEDGER")
  printf '%s\n' "$report"
  grep -Eq ': (FAIL|UNKNOWN)' <<<"$report" && return 1
}

# The verifier compares a recorded baseline commit to a later commit (HEAD by
# default).  Requiring two different commits prevents a vacuous `HEAD`/`HEAD`
# check from declaring a window frozen.  It deliberately hashes the same four
# trees and ordering as `pipeline_id`; scripts themselves are outside the P4
# pipeline by the accepted definition.
cmd_freeze() {
  local baseline=${1:-} candidate=${2:-HEAD}
  [[ -n $baseline ]] || die 'freeze: provide the recorded baseline commit (candidate defaults to HEAD)'
  local before after
  before=$(git -C "$ROOT" rev-parse --verify "$baseline^{commit}") \
    || die "freeze: baseline '$baseline' is not a commit"
  after=$(git -C "$ROOT" rev-parse --verify "$candidate^{commit}") \
    || die "freeze: candidate '$candidate' is not a commit"
  [[ $before != "$after" ]] || die 'freeze: baseline and candidate resolve to the same commit; compare two points in the window'
  local before_digest after_digest
  # P4_PIPELINE_ID exists only to make fixtures deterministic.  Honour it for
  # append tests, never for a freeze decision: an override would let two moved
  # trees compare equal and open banking on a rules change.
  before_digest=$(P4_PIPELINE_ID= pipeline_id "$before")
  after_digest=$(P4_PIPELINE_ID= pipeline_id "$after")
  echo "freeze window: baseline ${before:0:12} pipeline $before_digest; candidate ${after:0:12} pipeline $after_digest"
  if [[ $before_digest == "$after_digest" ]]; then
    echo 'freeze window: PASS — PIPELINE_TREES digest unchanged'
  else
    echo 'freeze window: FAIL — PIPELINE_TREES digest moved; banking must remain closed'
    return 1
  fi
}

case ${1:-} in
  --self-test) self_test ;;
  append) shift; cmd_append "$@" ;;
  total) shift; cmd_total "$@" ;;
  shakedown) shift; cmd_shakedown "$@" ;;
  freeze) shift; cmd_freeze "$@" ;;
  -h | --help) usage ;;
  *) usage; die "unknown command '${1:-<none>}'" ;;
esac
