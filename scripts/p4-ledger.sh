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
  has 'elif .identity.attempt_id then {attempt_id: .identity.attempt_id}' \
    || die 'self-test: a campaign bot contribution no longer carries its attempt into the measurement key; every generation of a standing host would collapse into one distinct bot measurement and the human-mix denominator would stop growing with wall time'
  has 'actor' \
    || die 'self-test: the bot|human dimension is gone; P4 cannot check its required mix'
  has 'validate_session_record' \
    || die 'self-test: campaign session rows are no longer validated before banking'
  has 'impairment_mismatch ==' \
    || die 'self-test: the mismatch flag is no longer checked against the row'\''s own numbers; a post-hoc edit of the measured impairment would bank'
  has 'verify-campaign-measurement.py' \
    || die 'self-test: the client measurement signature is no longer verified before banking'
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
  has 'validate_attempt_binding' \
    || die 'self-test: the attempt binding is no longer checked; several humans in one attempt would be indistinguishable in the ledger'
  has 'refuse_a_second_claim_on_one_seat' \
    || die 'self-test: nothing refuses a second claim on one seat; one interval could be banked twice across appends'
  has '.binding.banked_minutes // .session.banked_minutes) / 60' \
    || die 'self-test: player_hours is no longer cross-checked against the banked interval; the attempt total could be copied onto a participant'
  has '$banked <= .session.banked_minutes' \
    || die 'self-test: nothing holds the banked interval under the signed one; a clamp could invent play instead of discarding it'
  has 'connected_ticks * $per / 60' \
    || die 'self-test: the per-seat connected span is no longer recomputed; a human seated for part of an attempt could bank all of it'
  has '$banked <= $connected + $slack' \
    || die 'self-test: the banked interval is no longer held under the seat'\''s connected span'
  has '(1000 + 100e-6 * ($connected * 60000)) / 60000' \
    || die 'self-test: the #1032 clock-disagreement allowance is gone from the append path; an inflated claim would bank'

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
      deferral_ledger_balances: true, total_gaps: 164022, total_shed: 162,
      external: []
    } | if $session == "" then . else .identity.human_session_id = $session end' > "$dir/r.json"
    if [[ -n $2 ]]; then
      jq "$2" "$dir/r.json" > "$dir/r.next.json"
      mv "$dir/r.next.json" "$dir/r.json"
    fi
    if jq -e '.session? != null' "$dir/r.json" >/dev/null; then
      jq -c '.session' "$dir/r.json" \
        | python3 "$ROOT/scripts/sign-campaign-measurement-fixture.py" > "$dir/session.json"
      local fixture_node
      fixture_node=$(jq -r .measurement_node "$dir/session.json")
      jq --slurpfile session "$dir/session.json" --arg node "$fixture_node" \
        '.session = $session[0] | .external = [{node: $node}]' \
        "$dir/r.json" > "$dir/r.next.json"
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
  # says so is flagged evidence, not a refusal. The mismatch is a jitter
  # *shortfall* (#1030) — a p99 of 20 ms against a configured 100 is a seat
  # whose spike never arrived, which is the direction that is evidence. A p99
  # above the configured figure is the volunteer's own path adding to the
  # profile and is no longer a disagreement.
  st_report 11 '.player_hours = 1 | .seconds = 3600 | .peers = 1
    | .session = {
        session_id: "018f8f4e-5c90-7abc-8123-000000000011",
        wall_start: "2026-08-23T12:00:00Z", wall_end: "2026-08-23T13:00:00Z",
        distinct_play_minutes: 60, banked_minutes: 60,
        platform_triple: "x86_64-unknown-linux-gnu", client_rev: "self-test",
        ruleset_id: "52", ruleset_version: 2, pipeline_digest: "selftestpipeline",
        actor: "human", configured_impairment_profile: {loss_pct: 3, jitter_p50_ms: 100, jitter_p99_ms: 100},
        observed_loss_pct: 3.4, observed_jitter_p50_ms: 96, observed_jitter_p99_ms: 20,
        afk_seconds: 0, afk_capped: false, impairment_mismatch: true
      }
    | .identity.human_session_id = "018f8f4e-5c90-7abc-8123-000000000011"' human '018f8f4e-5c90-7abc-8123-000000000011' >/dev/null
  "$0" append "$dir/r.json" >/dev/null 2>&1 \
    || die 'self-test: an honestly flagged mismatching row was refused'
  # #973: the ordinary case. A real link configured at 3.0% loss and 100 ms
  # jitter measures 2.94% and a few milliseconds off, and its flag is correctly
  # clear. Recomputing by exact equality refused this row -- every honest row --
  # so the campaign could bank only rows a fixture had written observed ==
  # configured, and the exit criterion below could only pass on those.
  st_report 12 '.player_hours = 1 | .seconds = 3600 | .peers = 1
    | .session = {
        session_id: "018f8f4e-5c90-7abc-8123-000000000012",
        wall_start: "2026-08-23T12:00:00Z", wall_end: "2026-08-23T13:00:00Z",
        distinct_play_minutes: 60, banked_minutes: 60,
        platform_triple: "x86_64-unknown-linux-gnu", client_rev: "self-test",
        ruleset_id: "52", ruleset_version: 2, pipeline_digest: "selftestpipeline",
        actor: "human", configured_impairment_profile: {loss_pct: 3, jitter_p50_ms: 100, jitter_p99_ms: 100},
        observed_loss_pct: 2.94, observed_jitter_p50_ms: 103, observed_jitter_p99_ms: 96,
        afk_seconds: 0, afk_capped: false, impairment_mismatch: false
      }
    | .identity.human_session_id = "018f8f4e-5c90-7abc-8123-000000000012"' human '018f8f4e-5c90-7abc-8123-000000000012' >/dev/null
  "$0" append "$dir/r.json" >/dev/null 2>&1 \
    || die 'self-test: an honest measurement of a correctly-impaired link was refused'
  # #1030: the first real cohort, banked. Session 01a06b05-52e9 (macOS,
  # 2026-09-04) measured 3.17% loss with a jitter p50 of 17 ms and a p99 of
  # 151 ms against a campaign configured at 3% loss and a 100 ms spike — the
  # host holds a tenth of datagrams for the full spike and the rest not at all,
  # so the advertised profile is p50 0 / p99 100 and the volunteer's own path
  # adds on top of it. Its flag is correctly clear, and this row must bank.
  st_report 14 '.player_hours = 1 | .seconds = 3600 | .peers = 1
    | .session = {
        session_id: "018f8f4e-5c90-7abc-8123-000000000014",
        wall_start: "2026-08-23T12:00:00Z", wall_end: "2026-08-23T13:00:00Z",
        distinct_play_minutes: 60, banked_minutes: 60,
        platform_triple: "x86_64-unknown-linux-gnu", client_rev: "self-test",
        ruleset_id: "52", ruleset_version: 2, pipeline_digest: "selftestpipeline",
        actor: "human", configured_impairment_profile: {loss_pct: 3, jitter_p50_ms: 0, jitter_p99_ms: 100},
        observed_loss_pct: 3.17, observed_jitter_p50_ms: 17, observed_jitter_p99_ms: 151,
        afk_seconds: 0, afk_capped: false, impairment_mismatch: false
      }
    | .identity.human_session_id = "018f8f4e-5c90-7abc-8123-000000000014"' human '018f8f4e-5c90-7abc-8123-000000000014' >/dev/null
  "$0" append "$dir/r.json" >/dev/null 2>&1 \
    || die 'self-test: an honest volunteer session was refused as an impairment mismatch (#1030)'

  # And the band has not swallowed the property the flag exists to provide: a
  # seat that never received its impairment reads ~0 against a configured 3.0
  # and 100, and a clear flag over those numbers is still refused.
  #
  # Built through `st_report`, which *signs* the row, rather than edited into
  # shape afterwards: an edited row is refused by the signature stage before
  # the arithmetic ever runs, and a fixture refused for the wrong reason is
  # not evidence that this check works. The refusal message is checked too.
  st_report 13 '.player_hours = 1 | .seconds = 3600 | .peers = 1
    | .session = {
        session_id: "018f8f4e-5c90-7abc-8123-000000000013",
        wall_start: "2026-08-23T12:00:00Z", wall_end: "2026-08-23T13:00:00Z",
        distinct_play_minutes: 60, banked_minutes: 60,
        platform_triple: "x86_64-unknown-linux-gnu", client_rev: "self-test",
        ruleset_id: "52", ruleset_version: 2, pipeline_digest: "selftestpipeline",
        actor: "human", configured_impairment_profile: {loss_pct: 3, jitter_p50_ms: 100, jitter_p99_ms: 100},
        observed_loss_pct: 0.02, observed_jitter_p50_ms: 1, observed_jitter_p99_ms: 2,
        afk_seconds: 0, afk_capped: false, impairment_mismatch: false
      }
    | .identity.human_session_id = "018f8f4e-5c90-7abc-8123-000000000013"' human '018f8f4e-5c90-7abc-8123-000000000013' >/dev/null
  local st_unapplied
  if st_unapplied=$("$0" append "$dir/r.json" 2>&1); then
    die 'self-test: a session that never received its impairment banked with a clear flag'
  fi
  case $st_unapplied in
    *'impairment_mismatch contradicts'*) ;;
    *) die "self-test: the unapplied-impairment row was refused for the wrong reason: $st_unapplied" ;;
  esac

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
  local criterion
  for criterion in \
    'goldens green on all three platforms' \
    'client launches on all three platforms' \
    'zero discrepancy reports triaged real over those hours' \
    'no tolerance band moved during them' \
    'impairment verified applied in every sampled session'
  do
    grep -q "$criterion: PASS" <<<"$shakedown" \
      || die "self-test: passing shakedown fixture did not pass '$criterion' ('$shakedown')"
  done
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

  # ── The attempt binding (#576) ────────────────────────────────────────────
  #
  # Named cases, because "a named fixture fails" is the only useful report a
  # mutation check can produce. The property under test is exactly-once
  # attribution and the `(attempt, slot, session_id, node)` binding — not the
  # shape of a row, which a report can satisfy while charging one interval to
  # two people or charging a human interval to a bot seat.
  local bind_dir="$dir/binding" bind_passed=0
  mkdir -p "$bind_dir"
  export P4_LEDGER_FILE="$bind_dir/hours.jsonl"

  local st_attempt=018f9000-0000-7000-8000-00000000d001
  local st_sid_a=018f9000-0000-7000-8000-0000000000d1
  local st_sid_b=018f9000-0000-7000-8000-0000000000d2

  # A derived human contribution, in the shape `p4-attempt-accounting.py derive`
  # emits: identity carries the attempt and the seat, `binding` names the
  # exterior, and `player_hours` is this participant's own signed interval.
  # $1 slot  $2 session id  $3 banked minutes  $4 secret byte  $5 platform
  # $6 extra jq applied last, so a mutation can land on exactly one field.
  st_derived() {
    local slot=$1 session=$2 minutes=$3 secret=$4 platform=$5 extra=${6:-}
    local out="$bind_dir/human-$slot-$secret.json"
    jq -n --arg session "$session" --argjson minutes "$minutes" --arg platform "$platform" '{
      session_id: $session,
      wall_start: "2026-08-27T12:00:00Z", wall_end: "2026-08-27T13:00:00Z",
      distinct_play_minutes: $minutes, banked_minutes: $minutes,
      platform_triple: $platform, client_rev: "self-test",
      ruleset_id: "52", ruleset_version: 16, pipeline_digest: "selftestpipeline",
      actor: "human",
      configured_impairment_profile: {loss_pct: 3, jitter_p50_ms: 100, jitter_p99_ms: 100},
      observed_loss_pct: 3, observed_jitter_p50_ms: 100, observed_jitter_p99_ms: 100,
      afk_seconds: 0, afk_capped: false, impairment_mismatch: false
    }' | python3 "$ROOT/scripts/sign-campaign-measurement-fixture.py" --secret-byte "$secret" \
       > "$bind_dir/row-$slot-$secret.json"
    local node
    node=$(jq -r .measurement_node "$bind_dir/row-$slot-$secret.json")
    jq -n --slurpfile row "$bind_dir/row-$slot-$secret.json" \
      --arg attempt "$st_attempt" --arg session "$session" --arg node "$node" \
      --argjson slot "$slot" --argjson minutes "$minutes" --arg platform "$platform" '
      ($row[0]) as $r
      | {
        identity: {
          seed: 5,
          impairment: { loss: 0.03, jitter_ticks: 6, jitter_rate: 0.1, retransmit_ticks: 3 },
          target: $platform,
          commit: "0000000000000000000000000000000000000000",
          actor: "human", human_session_id: $session,
          attempt_id: $attempt, slot: $slot
        },
        started_at_unix_secs: 1750000000,
        peers: 4, bots: 4, seconds: 3600, ticks: 108000,
        valid_attempt_seconds: 3600, completed: true,
        player_hours: ($minutes / 60),
        witnessing: true, total_false_positives: 0, observation_coverage: 1.0,
        deferral_ledger_balances: true, total_gaps: 164022, total_shed: 162,
        session: $r,
        external: [{ index: $slot, node: $node, connected_ticks: ($minutes * 60 * 30), said_goodbye: true, connected: false }],
        attempt: { attempt_id: $attempt, host_target: "x86_64-unknown-linux-gnu",
                   bots: 4, valid_attempt_seconds: 3600 },
        binding: { attempt_id: $attempt, slot: $slot, session_id: $session, node: $node,
                   connected_ticks: ($minutes * 60 * 30),
                   connected_minutes: $minutes, close: "goodbye" }
      }' > "$out"
    if [[ -n $extra ]]; then
      jq "$extra" "$out" > "$out.next" && mv "$out.next" "$out"
    fi
    echo "$out"
  }
  # The bot contribution: the cohort's `B * valid_attempt_seconds / 3600`, one
  # per attempt, binding no seat.
  st_bot_contribution() {
    local out="$bind_dir/bot${1:+-$1}.json"
    jq -n --arg attempt "${1:-$st_attempt}" '{
      identity: {
        seed: 5,
        impairment: { loss: 0.03, jitter_ticks: 6, jitter_rate: 0.1, retransmit_ticks: 3 },
        target: "x86_64-unknown-linux-gnu",
        commit: "0000000000000000000000000000000000000000",
        actor: "bot", attempt_id: $attempt
      },
      started_at_unix_secs: 1750000000,
      peers: 4, bots: 4, seconds: 3600, ticks: 108000,
      valid_attempt_seconds: 3600, completed: true, player_hours: 4.0,
      witnessing: true, total_false_positives: 0, observation_coverage: 1.0,
      deferral_ledger_balances: true, total_gaps: 164022, total_shed: 162,
      attempt: { attempt_id: $attempt, host_target: "x86_64-unknown-linux-gnu",
                 bots: 4, valid_attempt_seconds: 3600 },
      contribution: { actor: "bot", player_hours: 4.0, derivation: "4 * 3600 / 3600" }
    }' > "$out"
    echo "$out"
  }
  st_bind_ok() { bind_passed=$(( bind_passed + 1 )); echo "$NAME: PASS $1"; }
  # $1 fixture name, $2 report path. Must refuse *and* leave no line behind: a
  # refusal that has already written is not a refusal.
  st_bind_refuses() {
    local name=$1 path=$2 before
    before=$(st_lines)
    if "$0" append "$path" >/dev/null 2>&1; then
      die "self-test [$name]: this must not bank, and it did"
    fi
    [[ $(st_lines) == "$before" ]] \
      || die "self-test [$name]: a refused contribution still touched the ledger"
    st_bind_ok "$name"
  }

  local bot_input human_a human_b
  bot_input=$(st_bot_contribution)
  human_a=$(st_derived 4 "$st_sid_a" 50 7 x86_64-unknown-linux-gnu)
  human_b=$(st_derived 5 "$st_sid_b" 42 8 x86_64-pc-windows-msvc)

  "$0" append "$bot_input" >/dev/null 2>&1 \
    || die 'self-test [a_cohort_attempt_banks_one_input_per_actor]: the bot contribution was refused'
  "$0" append "$human_a" >/dev/null 2>&1 \
    || die 'self-test [a_cohort_attempt_banks_one_input_per_actor]: the first human contribution was refused'
  "$0" append "$human_b" >/dev/null 2>&1 \
    || die 'self-test [a_cohort_attempt_banks_one_input_per_actor]: the second human contribution was refused'
  [[ $(st_lines) == 3 ]] \
    || die "self-test [a_cohort_attempt_banks_one_input_per_actor]: expected three ledger inputs, got $(st_lines)"
  st_bind_ok a_cohort_attempt_banks_one_input_per_actor

  # 4 + 50/60 + 42/60 = 5.5333. The defect this repairs would have banked the
  # cohort total on each human row: 3 * 6.0 = 18 hours from one hour of play.
  local banked_total
  banked_total=$(jq -rs 'map(.player_hours) | add | (. * 10000 | round) / 10000' "$P4_LEDGER_FILE")
  [[ $banked_total == 5.5333 ]] \
    || die "self-test [each_row_banks_its_own_interval_not_the_cohort_total]: banked $banked_total, not 5.5333"
  jq -es 'map(select(.actor == "human"))
          | all(.player_hours == ((.binding.banked_minutes // .session.banked_minutes) / 60))' \
    "$P4_LEDGER_FILE" >/dev/null \
    || die 'self-test [each_row_banks_its_own_interval_not_the_cohort_total]: a human row did not bank its own interval'
  jq -es 'map(select(.actor == "human")) | all(.player_hours != 6.0)' "$P4_LEDGER_FILE" >/dev/null \
    || die 'self-test [each_row_banks_its_own_interval_not_the_cohort_total]: a human row banked the cohort total'
  st_bind_ok each_row_banks_its_own_interval_not_the_cohort_total

  # The binding reaches the ledger *line*, so reconciling a seat is an audit of
  # the ledger rather than of a directory the operator may no longer have.
  jq -es --arg attempt "$st_attempt" --arg a "$st_sid_a" --arg b "$st_sid_b" '
    map(select(.actor == "human")) as $h
    | ($h | all(.attempt_id == $attempt))
    and ($h | map(.slot) | sort == [4, 5])
    and ($h | map(.binding.session_id) | sort == ([$a, $b] | sort))
    and ($h | all(.binding.node == .session.measurement_node))
  ' "$P4_LEDGER_FILE" >/dev/null \
    || die 'self-test [the_ledger_line_carries_the_seat_binding]: the banked lines do not name their exterior'
  st_bind_ok the_ledger_line_carries_the_seat_binding

  # The signed human platform, not the host's. The Windows client's half hour
  # has to reach the `windows` bucket #240 counts "across all three platforms"
  # from, and `attempt.host_target` keeps the host's own triple on every line.
  jq -es '
    (map(select(.actor == "human" and .target == "x86_64-pc-windows-msvc")) | length == 1)
    and all(.attempt.host_target == "x86_64-unknown-linux-gnu")
    and (map(select(.actor == "bot")) | all(.target == "x86_64-unknown-linux-gnu"))
  ' "$P4_LEDGER_FILE" >/dev/null \
    || die 'self-test [a_human_row_banks_on_its_own_signed_platform]: the platform fold lost the signed triple'
  grep -q 'windows: 0.7 distinct hours' <<<"$("$0" total 2>&1)" \
    || die "self-test [a_human_row_banks_on_its_own_signed_platform]: the Windows interval did not reach the windows bucket ('$("$0" total 2>&1)')"
  st_bind_ok a_human_row_banks_on_its_own_signed_platform

  # ── Exactly-once attribution ──────────────────────────────────────────────
  #
  # The same interval, re-derived at another commit so its `run_key` differs and
  # the existing duplicate check cannot see it. This is the mutation target for
  # `refuse_a_second_claim_on_one_seat`.
  st_bind_refuses one_interval_may_not_be_banked_twice \
    "$(st_derived 4 "$st_sid_a" 50 7 x86_64-unknown-linux-gnu \
        '.identity.commit = "1111111111111111111111111111111111111111"')"

  # A different person's row re-stamped onto a seat that already banked.
  st_bind_refuses two_rows_may_not_bind_one_seat \
    "$(st_derived 4 "$st_sid_b" 42 8 x86_64-pc-windows-msvc \
        '.session.session_id = .identity.human_session_id
         | .binding.session_id = .identity.human_session_id')"

  # The bot cohort is one contribution per attempt, not one per participant.
  st_bind_refuses a_bot_cohort_banks_once_per_attempt \
    "$(jq '.identity.commit = "2222222222222222222222222222222222222222"' "$bot_input" \
        > "$bind_dir/bot-again.json"; echo "$bind_dir/bot-again.json")"

  # ── The binding itself ────────────────────────────────────────────────────
  #
  # Every case below takes a **fresh seat and a fresh session id**, so the only
  # thing that can refuse it is the binding clause it names. Reusing a seat that
  # has already banked would let `refuse_a_second_claim_on_one_seat` refuse it
  # for the wrong reason, and the fixture would then pass with its own clause
  # deleted — which is exactly what the first cut of this block did.
  local st_fresh_slot=7 st_fresh_n=0
  st_fresh() {
    st_fresh_slot=$(( st_fresh_slot + 1 ))
    st_fresh_n=$(( st_fresh_n + 1 ))
    st_derived "$st_fresh_slot" \
      "$(printf '018f9000-0000-7000-8000-0000000000f%x' "$st_fresh_n")" \
      "${1:-50}" "$(( 20 + st_fresh_n ))" x86_64-unknown-linux-gnu "${2:-}"
  }

  # A row bound to the wrong slot, in both directions: a human interval charged
  # to a bot seat, and a binding whose seat disagrees with the identity that
  # carries it into the ledger line.
  st_bind_refuses a_human_row_bound_to_a_bot_seat_is_refused \
    "$(st_fresh 50 '.identity.slot = 2 | .binding.slot = 2')"
  st_bind_refuses a_binding_that_disagrees_with_its_identity_is_refused \
    "$(st_fresh 50 '.binding.slot = (.binding.slot + 20)')"
  st_bind_refuses a_binding_naming_another_attempt_is_refused \
    "$(st_fresh 50 '.binding.attempt_id = "018f9000-0000-7000-8000-00000000d999"')"
  # Only the binding's own copy of the node moves here. Moving `.external.node`
  # with it would be caught by the retained signature refusal instead, and this
  # fixture would then survive its own clause being deleted.
  st_bind_refuses a_row_bound_to_a_node_the_host_did_not_admit_is_refused \
    "$(st_fresh 50 '.binding.node = (.binding.node | sub("^.."; "ff"))')"
  # And the retained refusal, on its own: the signature must verify for the node
  # the host admitted, whatever the binding says about it.
  st_bind_refuses a_row_signed_by_an_unadmitted_key_is_still_refused \
    "$(st_fresh 50 '.external[0].node = (.external[0].node | sub("^.."; "ff"))
                    | .binding.node = .external[0].node
                    | .session.measurement_node = .external[0].node')"
  # #579's clause, retained: the admitted node must name **one** seat. A cohort
  # report listing it twice is an ambiguous seat map, and a row bound into it is
  # bound to nobody in particular.
  st_bind_refuses a_node_naming_two_seats_is_refused \
    "$(st_fresh 50 '.external = [.external[0], (.external[0] | .index = (.index + 1))]')"
  st_bind_refuses a_binding_naming_another_participants_session_is_refused \
    "$(st_fresh 50 '.binding.session_id = "018f9000-0000-7000-8000-00000000d888"')"
  st_bind_refuses a_seat_that_was_never_connected_banks_nothing \
    "$(st_fresh 50 '.binding.connected_ticks = 0')"
  st_bind_refuses a_human_contribution_with_no_binding_is_refused \
    "$(st_fresh 50 'del(.binding)')"
  st_bind_refuses a_seat_binding_with_no_attempt_id_is_refused \
    "$(st_fresh 50 'del(.identity.attempt_id)')"
  # A fresh attempt, so the only clause that can refuse this is the one that
  # says a bot contribution occupies no seat. Pointing it at the attempt whose
  # bot contribution has already banked would refuse it as a duplicate instead.
  st_bind_refuses a_bot_contribution_claiming_a_seat_is_refused \
    "$(jq --arg s "$st_sid_a" --arg a 018f9000-0000-7000-8000-00000000d777 '
        .identity.attempt_id = $a | .attempt.attempt_id = $a | .identity.slot = 4
        | .binding = {attempt_id: $a, slot: 4, session_id: $s}' \
        "$bot_input" > "$bind_dir/bot-seated.json"; echo "$bind_dir/bot-seated.json")"
  st_bind_refuses a_leg_that_overflowed_its_queue_banks_nothing \
    "$(st_fresh 50 '.binding.close = "queue_overflow"')"

  # ── A rejoin inside one attempt (#1028) ───────────────────────────────────
  #
  # A volunteer closed their client and launched it again, so the host readmitted
  # the same QUIC-authenticated key at the seat it held, against a second
  # pre-minted invite id. Two signed intervals land on one slot, and the seat
  # clash has to let both bank — a fix that assembles the attempt upstream and
  # then dies here is not a fix. What the clause is *for* is unchanged and
  # asserted immediately below: the seat stays closed to any other identity.
  local rejoin_leg
  rejoin_leg=$(st_derived 40 018f9000-0000-7000-8000-0000000000c1 30 41 \
    x86_64-unknown-linux-gnu)
  "$0" append "$rejoin_leg" >/dev/null 2>&1 \
    || die 'self-test [a_rejoining_identity_banks_both_of_its_intervals]: the first leg was refused'
  rejoin_leg=$(st_derived 40 018f9000-0000-7000-8000-0000000000c2 5 41 \
    x86_64-unknown-linux-gnu)
  "$0" append "$rejoin_leg" >/dev/null 2>&1 \
    || die 'self-test [a_rejoining_identity_banks_both_of_its_intervals]: the second leg of one identity was refused on the seat it rejoined'
  jq -es 'map(select(.slot == 40)) as $legs
    | ($legs | length == 2)
    and ($legs | map(.binding.node) | unique | length == 1)
    and ($legs | map(.human_session_id) | unique | length == 2)
    and ((($legs | map(.player_hours) | add) * 60 | round) == 35)' \
    "$P4_LEDGER_FILE" >/dev/null \
    || die 'self-test [a_rejoining_identity_banks_both_of_its_intervals]: the two legs did not bank as separate intervals on one seat'
  st_bind_ok a_rejoining_identity_banks_both_of_its_intervals

  # And the seat that a rejoin reopened is still closed to everybody else: a
  # third row on that slot, signed by a key the seat never admitted, is a
  # re-stamp and is refused exactly as it was before #1028.
  st_bind_refuses another_identity_may_not_join_a_rejoined_seat \
    "$(st_derived 40 018f9000-0000-7000-8000-0000000000c3 5 42 x86_64-unknown-linux-gnu)"

  # ── The non-constant denominator ──────────────────────────────────────────
  #
  # A human seated for part of an attempt played less than the attempt lasted.
  # Both spellings of the over-claim are refused: banking more minutes than the
  # seat was connected, and inflating the seat's own recorded span to match.
  st_bind_refuses an_interval_may_not_exceed_its_seats_connected_span \
    "$(st_fresh 50 '.binding.connected_ticks = (10 * 60 * 30) | .binding.connected_minutes = 10')"
  # `connected_minutes` alone: the ticks and the interval are both honest, so
  # only the clause that re-derives the span from the ticks can refuse this.
  st_bind_refuses a_connected_span_that_contradicts_its_own_ticks_is_refused \
    "$(st_fresh 50 '.binding.connected_minutes = 500')"
  # And the honest partial seat still banks: 10 minutes of a 60-minute attempt
  # is 10 minutes, not an hour and not a refusal.
  local partial
  partial=$(st_derived 6 018f9000-0000-7000-8000-0000000000d3 10 11 x86_64-unknown-linux-gnu)
  "$0" append "$partial" >/dev/null 2>&1 \
    || die 'self-test [a_partial_seat_banks_its_own_span]: an honest partial interval was refused'
  jq -es 'map(select(.slot == 6)) | length == 1 and (.[0].player_hours * 6 | round) == 1' \
    "$P4_LEDGER_FILE" >/dev/null \
    || die 'self-test [a_partial_seat_banks_its_own_span]: a ten-minute seat did not bank ten minutes'
  st_bind_ok a_partial_seat_banks_its_own_span

  # ── The clamp to the host's wall bracket (#1032) ──────────────────────────
  #
  # The real 2026-09-04 shape: a client whose own tick count claims 168 ms more
  # than the host's bracket, which is two clocks disagreeing rather than an
  # inflated hour. What banks is the bracket, and the sliver is visible in the
  # row rather than absorbed by a tolerance.
  local st_since=1750000000000 st_bracket
  # 30 minutes of bracket, and a claim 168 ms past it.
  st_bracket=$(( st_since + 30 * 60000 ))
  local clamp_stamps=".binding.connected_since_unix_millis = $st_since
      | .binding.connected_until_unix_millis = $st_bracket
      | .binding.connected_ticks = (30 * 60 * 30)
      | .binding.connected_minutes = 30"
  local clamped
  clamped=$(st_derived 9 018f9000-0000-7000-8000-0000000000e1 30.0028 51 \
      x86_64-unknown-linux-gnu "$clamp_stamps
      | .binding.claimed_minutes = 30.0028
      | .binding.banked_minutes = 30
      | .binding.clamped_minutes = 0.0028
      | .binding.span_basis = \"host wall bracket\"
      | .player_hours = 0.5")
  "$0" append "$clamped" >/dev/null 2>&1 \
    || die 'self-test [a_clamped_interval_banks_the_host_bracket]: a clamped honest interval was refused'
  jq -es '
    map(select(.slot == 9))
    | length == 1
      and (.[0].player_hours == 0.5)
      and (.[0].binding.clamped_minutes > 0)' "$P4_LEDGER_FILE" >/dev/null \
    || die 'self-test [a_clamped_interval_banks_the_host_bracket]: the bracket was not what banked'
  st_bind_ok a_clamped_interval_banks_the_host_bracket

  # A clamp only ever discards. A binding claiming to have banked *more* than
  # the row its client signed is the inflation the clamp exists to prevent,
  # wearing the clamp's own field.
  st_bind_refuses a_clamp_may_not_bank_more_than_the_signed_interval \
    "$(st_fresh 30 "$clamp_stamps
      | .binding.banked_minutes = 40 | .player_hours = (40 / 60)")"

  # And on the bracket basis the ceiling is exact: one millisecond past the
  # host's own span is past it, because the derivation had a clamp available
  # and did not use it. The old one-tick tolerance is gone from this path.
  st_bind_refuses a_banked_interval_past_its_wall_bracket_is_refused \
    "$(st_fresh 30.0028 "$clamp_stamps
      | .binding.banked_minutes = 30.0000167
      | .player_hours = (30.0000167 / 60)")"

  # The allowance is a bound on two clocks disagreeing, not a discount. A claim
  # a full minute past its bracket is refused even though the binding clamps it
  # to something bankable: the *row* is what disagrees with itself.
  st_bind_refuses a_claim_far_past_its_wall_bracket_is_refused \
    "$(st_fresh 31 "$clamp_stamps
      | .binding.claimed_minutes = 31
      | .binding.banked_minutes = 30
      | .binding.clamped_minutes = 1
      | .player_hours = 0.5")"

  # ── The cross-check #576 asks for, on its own ─────────────────────────────
  st_bind_refuses player_hours_must_equal_the_signed_interval \
    "$(st_derived 7 018f9000-0000-7000-8000-0000000000d4 30 12 x86_64-unknown-linux-gnu \
        '.player_hours = 6.0')"

  # ── The banking unit is a seat interval, on both sides (#1048) ────────────
  #
  # A standing host runs generation after generation against the same seed,
  # impairment and target, and each generation's bot seats occupy a **disjoint
  # wall interval**. `run_key` already told the two apart, because it hashes
  # the whole identity and the identity carries the attempt. `measurement_key`
  # did not, so `total`'s `distinct` fold — which is what the ≥25% human-mix
  # line is computed over — pinned the bot denominator at one generation's
  # hours however long the campaign ran.
  #
  # These two fixtures are the mutation target for that clause. The first is
  # what fails if the attempt is dropped from the bot measurement key; the
  # second is what fails if it is added to the *provenance* key instead, or if
  # the CI collapse it must preserve is broken.
  local mix_dir="$dir/mix"
  mkdir -p "$mix_dir"
  local gen2=018f9000-0000-7000-8000-00000000d002
  local bot_gen2
  bot_gen2=$(st_bot_contribution "$gen2")
  "$0" append "$bot_gen2" >/dev/null 2>&1 \
    || die 'self-test [two_generations_are_two_bot_measurements]: a second generation was refused'
  jq -es 'map(select(.actor == "bot"))
          | (length == 2)
          and ((map(.run_key) | unique | length) == 2)
          and ((map(.measurement_key) | unique | length) == 2)' \
    "$P4_LEDGER_FILE" >/dev/null \
    || die 'self-test [two_generations_are_two_bot_measurements]: two generations of bots collapsed into one distinct measurement; the mix denominator stops growing with wall time'
  # Both generations reach the distinct fold `total` prints, so the bot side of
  # the mix is the two generations' hours and not one of them.
  grep -q 'bot: 8 distinct hours = 4.0 + 4.0 (2 distinct measurement(s))' <<<"$("$0" total 2>&1)" \
    || die "self-test [two_generations_are_two_bot_measurements]: the distinct fold did not carry both generations' bot hours ('$("$0" total 2>&1)')"
  # And the direction is the one that matters: collapsing the two generations
  # back into one measurement — what this ledger did before #1048 — reports a
  # *higher* human mix off the same evidence, which is the floor being cleared
  # by running longer rather than by anyone playing more.
  jq -es "$JQ_PRELUDE"'
    (distinct | map(.player_hours) | add) as $all
    | (distinct | map(select(actor == "human") | .player_hours) | add) as $human
    | (distinct | unique_by([(actor), .seed, .impairment, .target,
                             (.human_session_id // null)])) as $collapsed
    | ($collapsed | map(.player_hours) | add) as $collapsed_all
    | ($human / $all) < ($human / $collapsed_all)
  ' "$P4_LEDGER_FILE" >/dev/null \
    || die 'self-test [two_generations_are_two_bot_measurements]: collapsing the generations did not raise the reported human mix; the fixture has stopped measuring the defect it names'
  st_bind_ok two_generations_are_two_bot_measurements

  # The restart case, stated as the ledger sees it. A host that dies and comes
  # back mints a fresh `attempt_id` (`scripts/p1-swarm-always-on.py`,
  # `mint_attempt_id` — a UUIDv7 materialised as a directory created without
  # `exist_ok`, so a reused id raises rather than overwrites), so its next
  # generation banks as its own interval. What must never happen is the *same*
  # generation banking twice: a replayed report, a restored shard, or a
  # re-derivation at another commit.
  local before_restart
  before_restart=$(st_lines)
  "$0" append "$bot_gen2" >/dev/null 2>&1 \
    || die 'self-test [a_restart_banks_no_second_copy_of_one_generation]: re-appending an identical report errored instead of deduping'
  [[ $(st_lines) == "$before_restart" ]] \
    || die 'self-test [a_restart_banks_no_second_copy_of_one_generation]: an identical replay of a generation banked a second time'
  st_bind_refuses a_restart_banks_no_second_copy_of_one_generation \
    "$(jq --arg a "$gen2" '.identity.commit = "3333333333333333333333333333333333333333"' \
        "$bot_gen2" > "$mix_dir/bot-gen2-recommit.json"; echo "$mix_dir/bot-gen2-recommit.json")"

  # And the collapse the key exists for is untouched: a leg with no attempt —
  # every CI bot leg — is still one measurement however many times it is
  # re-run, because a deterministic re-run of one seed re-measures one
  # simulated hour rather than measuring a second one.
  local no_attempt_a no_attempt_b
  no_attempt_a=$(jq 'del(.identity.attempt_id) | del(.attempt)
                     | .identity.commit = "4444444444444444444444444444444444444444"' \
                   "$bot_gen2" > "$mix_dir/plain-a.json"; echo "$mix_dir/plain-a.json")
  no_attempt_b=$(jq '.identity.commit = "5555555555555555555555555555555555555555"' \
                   "$no_attempt_a" > "$mix_dir/plain-b.json"; echo "$mix_dir/plain-b.json")
  P4_LEDGER_FILE="$mix_dir/plain.jsonl" "$0" append "$no_attempt_a" >/dev/null 2>&1 \
    || die 'self-test [a_deterministic_rerun_is_still_one_measurement]: an attempt-less bot leg was refused'
  P4_LEDGER_FILE="$mix_dir/plain.jsonl" "$0" append "$no_attempt_b" >/dev/null 2>&1 \
    || die 'self-test [a_deterministic_rerun_is_still_one_measurement]: its re-run was refused'
  jq -es '(length == 2) and ((map(.measurement_key) | unique | length) == 1)' \
    "$mix_dir/plain.jsonl" >/dev/null \
    || die 'self-test [a_deterministic_rerun_is_still_one_measurement]: two runs of one seed with no attempt stopped collapsing; a re-measurement would count as a second hour'
  st_bind_ok a_deterministic_rerun_is_still_one_measurement

  # ── Retained: nothing above loosened the pre-cohort path ──────────────────
  #
  # A swarm report that names no attempt still banks, and the whole functional
  # half above ran against exactly that shape.
  local plain
  plain=$(st_report 42 '')
  P4_LEDGER_FILE="$bind_dir/plain.jsonl" "$0" append "$plain" >/dev/null 2>&1 \
    || die 'self-test [a_report_with_no_attempt_still_banks]: the pre-cohort path was loosened shut'
  st_bind_ok a_report_with_no_attempt_still_banks

  rm -rf "$dir"
  trap - EXIT
  echo "$NAME: self-test passed ($bind_passed attempt-binding fixtures)"
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
      and ($s.measurement_node | type == "string" and length == 64)
      and ($s.measurement_payload | type == "string" and length > 0)
      and ($s.measurement_signature | type == "string" and length == 128)
      and (if $actor == "human" then $s.session_id == $session else true end)
    end
  ' "$report" >/dev/null \
    || die 'refusing to bank: incomplete or inconsistent campaign session row'
  if jq -e '.session? != null' "$report" >/dev/null; then
    local measurement_node
    measurement_node=$(jq -er '.session.measurement_node | select(type == "string" and length > 0)' "$report") \
      || die 'refusing to bank: campaign session does not name its measurement node'
    jq -e --arg node "$measurement_node" \
      '[.external[] | select(.node == $node)] | length == 1' "$report" >/dev/null \
      || die 'refusing to bank: host report does not name the authenticated external node exactly once'
    jq -c '.session' "$report" \
      | python3 "$ROOT/scripts/verify-campaign-measurement.py" "$measurement_node" >/dev/null \
      || die 'refusing to bank: client measurement signature did not verify for the admitted node'
  fi
  # The mismatch flag is recomputable from the row's own numbers, and #387
  # requires that it *fired* whenever observation disagrees with
  # configuration. Checking the arithmetic here is what makes the flag
  # tamper-evident: a post-hoc edit of observed_loss_pct (to hide a mismatch,
  # or to fake one) leaves the flag contradicting the numbers next to it, and
  # a row whose own fields disagree with each other is not evidence.
  #
  # Recomputed within the band the *client* computed the flag with (#973), not
  # by exact float equality: a measurement never lands exactly on its
  # configuration, so equality here refused every honest row the client ever
  # signed. The band and its derivation live at IMPAIRMENT_LOSS_TOLERANCE_PCT
  # in scripts/p4-attempt-accounting.py, which mirrors
  # clients/regolith/src/session.rs; that file's --self-test holds all three
  # copies of these numbers against each other.
  jq -e '
    if .session? == null then true else
      .session as $s
      | $s.configured_impairment_profile as $c
      # Loss straddles its configuration; jitter is a floor, not a target
      # (#1030). The client measures the injected spike composed with the
      # path the volunteer plays over, and delays add rather than cancel, so
      # only a shortfall below the configured percentile is evidence.
      | ((((($s.observed_loss_pct - $c.loss_pct) | fabs) > 2.0)
          or (($c.jitter_p50_ms - $s.observed_jitter_p50_ms) > 40)
          or (($c.jitter_p99_ms - $s.observed_jitter_p99_ms) > 40)) as $outside
        # The client suppresses the flag below 200 observed packets, which the
        # signed row does not carry; 200 packets is 200/20/60 minutes of play
        # at the 20 Hz send cadence, and below that a clear flag stands.
        | ($s.impairment_mismatch == true and $outside)
          or ($s.impairment_mismatch == false
              and (($outside | not) or ($s.distinct_play_minutes < (200 / 20 / 60)))))
    end
  ' "$report" >/dev/null \
    || die 'refusing to bank: session impairment_mismatch contradicts the row'\''s own observed/configured impairment'
}

# ── The attempt binding (#576) ───────────────────────────────────────────────
#
# `docs/plans/multi-human-attempt-accounting.md` §3–§4. A cohort attempt emits
# one ledger input per *actor contribution*, and every human input binds to
# exactly one exterior `(attempt_id, slot, session_id, node)`. Without that
# binding several humans in one attempt are indistinguishable in the ledger, and
# nothing here could tell one interval banked twice from two intervals.
#
# The ledger re-derives what it can rather than trusting the derived report: the
# connected span is recomputed from `connected_ticks` and the attempt's own tick
# rate, so inflating `binding.connected_minutes` is not a way past the per-seat
# ceiling. `scripts/p4-attempt-accounting.py` checks the same clauses at
# derivation time; both check the file rather than the caller, and this is the
# one that still holds when a derived report is edited after assembly.
readonly UUID_V7_RE='^[0-9a-f]{8}-[0-9a-f]{4}-7[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$'
readonly BANKABLE_CLOSES='["goodbye","attempt_end","disconnected"]'

validate_attempt_binding() {
  local report=$1 actor=$2 attempt_id=$3

  if [[ -z $attempt_id ]]; then
    # A pre-cohort swarm report binds no seat, and stays bankable. What is
    # refused is a *binding with nothing to bind to*: a row carrying seat
    # evidence for an attempt it does not name is not reconcilable afterwards.
    jq -e '.binding? == null and (.identity.slot? == null)' "$report" >/dev/null \
      || die 'refusing to bank: a row carries a seat binding but names no identity.attempt_id'
    return
  fi
  [[ $attempt_id =~ $UUID_V7_RE ]] \
    || die 'refusing to bank: identity.attempt_id is not a coordinator-issued UUIDv7'

  if [[ $actor == bot ]]; then
    # The bot contribution is the cohort's, not a seat's:
    # `B * valid_attempt_seconds / 3600`. It binds no exterior and carries no
    # signed session row; a bot input claiming a seat is a human interval
    # charged to the bot cohort.
    jq -e '.binding? == null and (.identity.slot? == null) and (.session? == null)' "$report" >/dev/null \
      || die 'refusing to bank: a bot contribution occupies no exterior seat and carries no signed session row'
    return
  fi

  # All four binding clauses, against the row the file actually carries.
  # `slot >= attempt.bots` is what keeps a human interval off a bot seat; the
  # node equality is what makes this seat's row *this seat's* row, because the
  # session token authenticates a NodeId without reserving a seat.
  jq -e --arg attempt "$attempt_id" '
    .binding as $b
    | $b != null
    and ($b.attempt_id == $attempt)
    and ($b.slot | type == "number")
    and ($b.slot == .identity.slot)
    and ($b.session_id == .identity.human_session_id)
    and ($b.session_id == .session.session_id)
    and ($b.node | type == "string" and length == 64)
    # .external is a list of seats since #579, and a derived human row carries
    # the one seat it is bound to. The bound node must name that seat exactly
    # once: "somewhere in the cohort" would let a row bound to slot 4 satisfy a
    # check against the node admitted at slot 5.
    and ([.external[] | select(.node == $b.node)] | length == 1)
    and ($b.node == .session.measurement_node)
    and ($b.connected_ticks | type == "number" and . > 0)
    and (.attempt.bots | type == "number")
    and ($b.slot >= .attempt.bots)
  ' "$report" >/dev/null \
    || die 'refusing to bank: the human contribution does not bind to one exterior (attempt, slot, session_id, node) of the attempt it names'
  jq -e --argjson bankable "$BANKABLE_CLOSES" \
    '.binding.close as $c | $bankable | index($c) != null' "$report" >/dev/null \
    || die "refusing to bank: slot $(jq -r '.binding.slot' "$report") closed as $(jq -r '.binding.close' "$report"); that leg's evidence does not bank"

  # The non-constant denominator, as a refusal rather than an assumption. A
  # human seated for part of an attempt played less than the attempt lasted, so
  # the ceiling is *this seat's* connected span and never the attempt's length —
  # assuming presence for the whole attempt is exactly how a cohort over-counts.
  #
  # The span is the host's own wall bracket when the seat carries one (#971):
  # a tick count scaled at the *nominal* rate is not a duration, because the
  # host's metronome sleeps out a remainder and never makes up an overrun, so
  # it runs at or below 60 Hz and the scaled count understates a real seat by
  # however much it lagged. The tick basis stays as the fallback for a report
  # without stamps, and it is the conservative one — shorter for a lagging
  # host — so omitting the bracket refuses more readily, never less.
  #
  # Three numbers, not one, since #1032. What banks is
  # `binding.banked_minutes`, and it is held under *both* ends at once:
  #
  #   banked <= connected      exactly, no tolerance at all on the bracket
  #                            basis, because the derivation clamps to it
  #   banked <= claimed        a clamp only ever discards; it never invents
  #   claimed <= connected + allowance
  #
  # The allowance is `1000 ms + 100 ppm * span`, derived in full at
  # `CLOCK_BOUNDARY_SLACK_MS` in `scripts/p4-attempt-accounting.py` — a bound
  # on how far two independently-kept clocks with independently-detected
  # endpoints may honestly disagree, not a number fitted to a session. It is
  # the *claim* it bounds, never the banked figure: past it the row is
  # evidence of a client disagreeing with itself rather than with a clock.
  # `--self-test` holds the two copies together.
  #
  # `$slack` is zero only where the derivation actually clamped — a report that
  # carries `binding.banked_minutes` *and* a wall bracket. An input derived
  # before #1032 carries neither field nor clamp, so it keeps its original one
  # tick of boundary rounding and stays bankable; on the tick fallback, which
  # nothing clamps to because it understates the span, the allowance stands.
  # `// .session.banked_minutes` is the same backwards compatibility on the
  # value: without a clamp recorded, what banks is what was signed.
  jq -e '
    (.seconds / .ticks) as $per
    | .binding as $b
    | (($b.connected_since_unix_millis | type == "number")
       and ($b.connected_until_unix_millis | type == "number")) as $bracketed
    | (if $bracketed
       then ($b.connected_until_unix_millis - $b.connected_since_unix_millis) / 60000
       else ($b.connected_ticks * $per / 60) end) as $connected
    | ((1000 + 100e-6 * ($connected * 60000)) / 60000) as $allowance
    | ($b.banked_minutes // .session.banked_minutes) as $banked
    | (if ($b.banked_minutes | type == "number") and $bracketed then 0
       elif $bracketed then $per / 60
       else $allowance end) as $slack
    | $connected >= 0
      and (((.binding.connected_minutes // $connected) - $connected) | . * . < 1e-9)
      and $banked <= .session.banked_minutes
      and ($banked <= $connected + $slack)
      and (.session.banked_minutes <= $connected + $allowance)
  ' "$report" >/dev/null \
    || die "refusing to bank: slot $(jq -r '.binding.slot' "$report") banks $(jq -r '.binding.banked_minutes // .session.banked_minutes' "$report") min, more than the seat's own connected span"
}

# One ledger input per actor contribution, and the ledger is the only place that
# sees across appends. `run_key` already refuses the *same* identity twice; this
# refuses a *different* identity claiming a seat, or an interval, this attempt
# has already banked — the same interval re-derived at another commit, or a row
# re-stamped onto a seat another row is already bound to.
#
# The seat a human interval claims is `(slot, node)` — the slot the host bound
# and the QUIC-authenticated identity it admitted there — rather than the slot
# alone (#1028). Keying on the slot alone made a legitimate rejoin unbankable
# here for the same reason it made the attempt unassemblable upstream: one
# install readmitted at the seat it held, under a second pre-minted invite id,
# banks two signed intervals on one slot. What the slot clause is *for* survives
# untouched, because it is the one case a rejoin is not: a row re-stamped onto a
# seat some **other** admitted identity already banked. A ledger line that
# carries no node of its own — anything written before the binding travelled
# into the line — is treated as a clash, because it cannot prove it is not one.
refuse_a_second_claim_on_one_seat() {
  local actor=$1 attempt_id=$2 slot=$3 session=$4 node=$5
  [[ -n $attempt_id ]] || return 0
  [[ -r $LEDGER ]] || return 0
  local clash
  clash=$(jq -rs --arg attempt "$attempt_id" --arg actor "$actor" \
                 --arg slot "$slot" --arg session "$session" --arg node "$node" '
    [ .[] | select(.attempt_id == $attempt) ] as $rows
    | if $actor == "bot" then
        (if ([ $rows[] | select((.actor // "bot") == "bot") ] | length) > 0
         then "attempt \($attempt) has already banked its bot contribution"
         else "" end)
      else
        (if ([ $rows[]
               | select((.slot | tostring) == $slot)
               | select($node == "" or (.binding.node // "") != $node) ] | length) > 0
         then "slot \($slot) of attempt \($attempt) already carries a banked interval for another admitted identity"
         elif ([ $rows[] | select(.human_session_id == $session) ] | length) > 0
         then "session \($session) already banked an interval in attempt \($attempt)"
         else "" end)
      end
  ' "$LEDGER")
  [[ -z $clash ]] || die "refusing to bank: $clash; an interval is attributed exactly once"
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
  local attempt_id slot bound_node
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
  attempt_id=$(jq -r '.identity.attempt_id // empty' "$report")
  slot=$(jq -r '.identity.slot // empty' "$report")
  # `validate_attempt_binding` has already tied this to `.external[]` and to the
  # signed `.session.measurement_node`, so by the time the seat clash is decided
  # it is the admitted identity and not a free-text field.
  bound_node=$(jq -r '.binding.node // empty' "$report")
  validate_attempt_binding "$report" "$actor" "$attempt_id"
  # The cross-check #576 asks for, and the one that would have caught the defect
  # this whole piece exists to repair: a human contribution's `player_hours` is
  # *its own interval*, `banked_minutes / 60`, never the attempt total copied
  # onto a participant. A report whose two numbers disagree is banking a figure
  # its signed row does not attest.
  #
  # Since #1032 the banked interval may be the signed one clamped down to the
  # host's wall bracket, so the equality is against `binding.banked_minutes`
  # when the derivation recorded one — and `validate_attempt_binding` above has
  # already refused any report where that figure exceeds either the signed
  # interval or the seat's span. A clamp can only ever lower this number, so
  # the property #576 named — a participant banks no more than it signed — is
  # kept, not relaxed.
  jq -e '
    if .session? == null then true else
      ((.player_hours - ((.binding.banked_minutes // .session.banked_minutes) / 60))
       | . * . < 1e-9)
    end
  ' "$report" >/dev/null \
    || die "refusing to bank: player_hours $(jq -r '.player_hours' "$report") is not the banked interval $(jq -r '.binding.banked_minutes // .session.banked_minutes' "$report") / 60"
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
  #
  # ── Why a *campaign* bot contribution also carries its attempt (#1048) ─────
  #
  # The collapse above is right for the thing it was written for: a nightly
  # that re-runs one deterministic seed is re-measuring one simulated hour, and
  # that is provenance rather than a second hour of evidence. It is wrong for a
  # standing campaign, and the mix criterion is what it breaks.
  #
  # `cmd_total` computes "human mix" over `distinct`, i.e. over
  # `measurement_key`. A bot contribution is `B * valid_attempt_seconds / 3600`
  # for one generation of the standing host, and every generation runs the same
  # seed, the same impairment and the same target — the supervisor passes no
  # `--seed` (`scripts/p1-swarm-always-on.py`, `Supervisor.command`). So without
  # the attempt in this key, *every* generation of the campaign collapses into
  # one distinct bot measurement, while every human seat interval stays distinct
  # because `human_session_id` is minted per interval.
  #
  # Measured on the 2026-09-04 evidence: a second generation of the same five
  # bots banks its 1.25 provenance hours and adds **zero** distinct bot hours,
  # so the reported mix stays 27% where the truth is 2.5 bot against 0.483
  # human, i.e. 16% — under the floor. The denominator stops growing with wall
  # time while the numerator keeps growing, so the ≥25% floor is cleared by
  # running longer rather than by anyone playing more. That is the criterion
  # measuring something other than what it says.
  #
  # Two generations are two *different wall intervals with different people in
  # them*, never a re-run of one another; the attempt id is what says so. Legs
  # that carry no `attempt_id` — every CI bot leg — are untouched and still
  # collapse, so the reason the collapse exists is preserved exactly.
  #
  # Stated plainly, because it moves a number in the direction that needs
  # saying out loud: this makes distinct bot hours **larger**, so the 25% floor
  # becomes *harder* and the raw 500-hour figure rises faster with bot time. It
  # adds no human hour and banks no interval that was not measured; it stops
  # discarding bot-hours that were separately measured on disjoint wall time.
  measurement_key=$(jq -cS --arg pipeline "$pipeline" --arg actor "$actor" \
    '{pipeline: $pipeline, actor: $actor, seed: .identity.seed,
      impairment: .identity.impairment, target: .identity.target}
     + (if $actor == "human" then {human_session_id: .identity.human_session_id}
        elif .identity.attempt_id then {attempt_id: .identity.attempt_id}
        else {} end)' \
    "$report" | sha256_hex | cut -c1-16)

  mkdir -p "$(dirname "$LEDGER")"
  # One writer at a time. The nightly is a single job today, and a ledger whose
  # append is not atomic is a ledger that loses a line the first time it is not.
  ledger_lock

  if [[ -r $LEDGER ]] && grep -Fq "\"run_key\":\"$key\"" "$LEDGER"; then
    note "already banked: run_key $key (seed $seed, commit ${commit:0:12}); nothing appended"
    return 0
  fi
  # Held under the same lock as the append it guards: a second claim on one seat
  # decided outside the lock could still be written by a concurrent appender.
  refuse_a_second_claim_on_one_seat "$actor" "$attempt_id" "$slot" "$human_session_id" \
    "$bound_node"

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
      # #576: the binding travels into the ledger *line*, not only the derived
      # report beside it. Reconciling which seat of which attempt an hour came
      # from is an audit of the ledger, and an audit cannot reach for a file the
      # operator may no longer have.
      attempt_id: (.identity.attempt_id // null),
      slot: (.identity.slot // null),
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
    }
    + (if .session? == null then {} else {session: .session} end)
    + (if .attempt? == null then {} else {attempt: .attempt} end)
    + (if .binding? == null then {} else {binding: .binding} end)
    + (if .contribution? == null then {} else {contribution: .contribution} end)
    + (if .link_impairment? == null then {} else {link_impairment: .link_impairment} end)' \
    "$report" >> "$LEDGER"

  note "banked $hours $actor player-hours: run_key $key, measurement_key $measurement_key, seed $seed, loss $loss, target $target, pipeline $pipeline${attempt_id:+, attempt $attempt_id${slot:+ slot $slot}}"
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
                          .target, (.human_session_id // null),
                          (.attempt_id // null)] | tojson));
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
