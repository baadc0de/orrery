#!/usr/bin/env bash
# Where every gate in this repository actually stands, with numbers.
#
# The status of a gate was, until this script, spread across five places: the
# scripts in `scripts/`, the jobs in `.github/workflows/nightly.yml` and
# `ci.yml`, the evidence directories those jobs upload, and whoever last
# looked. Answering "is P3 green, and what did it settle in?" meant reading all
# of them by hand, so nobody did — which is how a vacuous self-test and an
# unrun `p4-*` self-test both survived for weeks (AGENTS.md §A `--self-test`
# nothing runs is not a check).
#
# ── The two rules this report is built on ────────────────────────────────────
#
# **Nothing here is a typed list of gates.** The gates are discovered from the
# filesystem (`scripts/*.sh`) and from the workflows (`jobs:` in nightly.yml and
# ci.yml). A hardcoded inventory is the thing that rots: it stays green while
# the tree moves under it. What *is* written down is how to run and how to read
# each gate this reporter knows about, and a discovered gate with no such entry
# is reported `UNKNOWN` and exits 2. So adding a gate and not teaching this
# script about it breaks the report loudly rather than dropping the gate from
# it silently.
#
# **A partial evaluation is never a pass.** Six statuses, and they are not
# collapsible:
#
#   PASSED       executed here, or an evidence artifact that says it held
#   FAILED       executed here and did not hold, or evidence that says so
#   UNQUALIFIED  correctness was evaluated, but the device could not support a
#                latency verdict; the whole gate therefore did not pass
#   NOT RUN      runnable, but this mode did not run it and no evidence exists
#   SKIPPED      a prerequisite is missing — no cluster, no hosted runner, no
#                binary. The gate was not evaluated and nothing is claimed.
#   UNKNOWN      discovered, but this reporter cannot run or read it
#
# The exit status distinguishes them too: 0 = nothing failed, 1 = a gate
# failed, 2 = the report is incomplete (an UNKNOWN gate). A run that skipped
# every heavy harness exits 0 and says so in every line of its summary; it does
# not say the gates passed.
#
# ── Modes ────────────────────────────────────────────────────────────────────
#
#   --fast     (default) static gates and every `--self-test` in `scripts/`.
#              Seconds. This is the per-commit shape, and it is the one that
#              must never be mistaken for a full run — so the mode is printed
#              in the banner, in the summary, and in every JSONL record.
#   --full     the above plus every real harness whose prerequisites are
#              satisfied on this machine. Minutes to hours.
#   --inspect  runs nothing at all; reports from evidence on disk only.
#
# ── Numbers ──────────────────────────────────────────────────────────────────
#
# Read out of the reports the gates already emit — `target/p1-swarm/*.json`,
# `p3-island-*/report.json`, `p2-kill9-*/artifact.json`, the P4 ledger,
# `target/fdb-tests.log` — never re-derived. A figure this script computed
# itself would be a second implementation of the gate, and the two would
# disagree exactly when it mattered.
set -euo pipefail

readonly NAME=gate-status
die() { echo "$NAME: $*" >&2; exit 2; }
note() { echo "$NAME: $*" >&2; }

usage() {
  cat >&2 <<'USAGE'
usage: gate-status.sh [--fast|--full|--inspect] [--jsonl PATH]
       gate-status.sh --self-test

  --fast       (default) static gates + every --self-test in scripts/
  --full       also run the real harnesses whose prerequisites are present here
  --inspect    run nothing; report from evidence on disk
  --jsonl P    machine-readable output path (default: $OUT/gate-status.jsonl)

  GATE_STATUS_OUT   evidence/scratch directory (default: target/gate-status)
  GATE_STATUS_ROOT  repository root override (self-test only)
  GATE_STATUS_FDB_IS_THROWAWAY=1
                    assert that ORRERY_FDB_CLUSTER_FILE points at a cluster
                    that may be written to and wiped. Without it the fdb tier
                    is SKIPPED rather than run: those suites wipe key ranges
                    (docs/11-roadmap.md C-8) and the box's development cluster
                    is shared.

exit: 0 nothing failed · 1 a gate failed · 2 the report is incomplete
USAGE
}

# ─────────────────────────────────────────────────────────────────────────────
# Discovery. Three sources, none of them a list in this file.
# ─────────────────────────────────────────────────────────────────────────────

# Every shell script in scripts/. The unit of discovery, because a gate that
# exists is a file.
discover_scripts() {
  find "$ROOT/scripts" -maxdepth 1 -type f -name '*.sh' -printf '%f\n' | sort
}

# Does the script *dispatch* on --self-test, as opposed to merely mentioning
# it? Same idiom as scripts/check.sh's coverage clause, deliberately: two
# scripts disagreeing about what "has a self-test" means is a gap between them.
script_has_self_test() {
  grep -qE '(==[[:space:]]*"?--self-test"?|^[[:space:]]*"?--self-test"?\))' "$ROOT/scripts/$1"
}

# Job keys of a workflow: exactly two spaces of indent under `jobs:`.
workflow_jobs() {
  local wf="$ROOT/.github/workflows/$1"
  [[ -r $wf ]] || return 0
  awk '
    /^jobs:[[:space:]]*$/ { in_jobs = 1; next }
    /^[^[:space:]#]/      { in_jobs = 0 }
    in_jobs && /^  [a-zA-Z0-9_-]+:[[:space:]]*$/ {
      key = $1; sub(/:$/, "", key); print key
    }
  ' "$wf"
}

# The body of one job, for the two questions asked of it below: which scripts
# does it run, and on what kind of runner.
workflow_job_block() {
  local wf="$ROOT/.github/workflows/$1" job=$2
  [[ -r $wf ]] || return 0
  awk -v job="$job" '
    /^jobs:[[:space:]]*$/ { in_jobs = 1; next }
    /^[^[:space:]#]/      { in_jobs = 0 }
    in_jobs && /^  [a-zA-Z0-9_-]+:[[:space:]]*$/ {
      key = $1; sub(/:$/, "", key)
      here = (key == job)
      next
    }
    here { print }
  ' "$wf"
}

# Scripts a job invokes for real — a `--self-test` invocation is a different
# gate and is discovered from the filesystem instead.
job_real_scripts() {
  workflow_job_block "$1" "$2" \
    | grep -v -- '--self-test' \
    | grep -oE 'scripts/[a-zA-Z0-9._-]+\.sh' \
    | sed 's|scripts/||' | sort -u
}

# Scripts scripts/check.sh's gates lane runs bare — the static gates. Read out
# of check.sh's body rather than named here, for the same reason as everything
# else on this page.
discover_static_gates() {
  [[ -r $ROOT/scripts/check.sh ]] || return 0
  awk '/^lane_gates\(\)/ { in_lane = 1 } in_lane && /^}/ { exit } in_lane' "$ROOT/scripts/check.sh" \
    | grep -v -- '--self-test' \
    | sed -n 's|^[[:space:]]*run \(scripts/[a-zA-Z0-9._-]*\.sh\)[[:space:]]*$|\1|p' \
    | sed 's|scripts/||' | sort -u || true
}

# A matrix, a GitHub-hosted image, or a legacy persistent-runner label. The
# distinction is a skip reason: a `macos-latest` leg cannot be evaluated on
# this box at all. The legacy classification is deliberately retained even
# though no workflow currently produces it: an accidental reintroduction is
# useful source-column evidence, and this function never branches on it.
job_runner_kind() {
  local block runs_on
  block=$(workflow_job_block "$1" "$2")
  # `runs-on` is not one value in this repository. Two jobs take it from a
  # matrix, and the rest name a label directly — so a matrix leg is reported as
  # such rather than as whichever of
  # its three `include:` entries happens to be listed first. All of it is
  # labelling for the source column; nothing branches on it.
  runs_on=$(sed -n 's|^[[:space:]]*runs-on:[[:space:]]*||p' <<<"$block" | tail -1)
  case "$runs_on" in
    *matrix*)      echo matrix ;;
    *self-hosted*) echo self-hosted ;;
    *'${{'*)       echo dynamic ;;
    '')            echo unknown ;;
    *)             echo hosted ;;
  esac
}

# ─────────────────────────────────────────────────────────────────────────────
# What this reporter knows how to run and read.
#
# One function trio per gate it understands, found by name. A gate discovered
# above with no `gate_<key>_evidence` is UNKNOWN — that is the whole anti-rot
# mechanism, and it is why these are functions rather than a table: `declare
# -F` is the lookup.
#
#   gate_<key>_tier      fast | full   — which mode runs it
#   gate_<key>_prereq    0 = runnable here; else print the reason and return 1
#   gate_<key>_run       run it; exit status is the verdict
#   gate_<key>_evidence  print {"status":…,"evidence":…,"numbers":{…}}
# ─────────────────────────────────────────────────────────────────────────────

# `jq -n` shorthand for an evidence answer.
ev() { # status evidence-path numbers-json
  jq -cn --arg s "$1" --arg e "$2" --argjson n "${3:-\{\}}" \
    '{status: $s, evidence: $e, numbers: $n}'
}
ev_none() { ev 'NOT RUN' '' '{}'; }

have_cargo() { command -v cargo >/dev/null; }

# ── static gates and self-tests ──────────────────────────────────────────────
#
# Generic by construction: a static gate is "run the script", a self-test is
# "run the script with --self-test". Both are known for every script the
# discovery above turns up, so neither can go UNKNOWN — which is correct. What
# a new script's *real* run needs is a trio below.

gate_static_tier() { echo fast; }
gate_static_prereq() { have_cargo || { echo 'cargo is not on PATH'; return 1; }; }
gate_static_run() { "$ROOT/scripts/$1" >"$OUT/logs/static-$1.log" 2>&1; }
gate_static_evidence() { ev_none; }

gate_selftest_tier() { echo fast; }
gate_selftest_prereq() { return 0; }
gate_selftest_run() { "$ROOT/scripts/$1" --self-test >"$OUT/logs/selftest-$1.log" 2>&1; }
gate_selftest_evidence() { ev_none; }

# ── P1 swarm ─────────────────────────────────────────────────────────────────

gate_p1_swarm_tier() { echo full; }
gate_p1_swarm_prereq() { have_cargo || { echo 'cargo is not on PATH'; return 1; }; }
gate_p1_swarm_run() {
  P1_SWARM_OUT="$OUT/p1-swarm" "$ROOT/scripts/p1-swarm-gate.sh" \
    >"$OUT/logs/p1-swarm.log" 2>&1
}
# Five legs, and the report distinguishes them: `PASSED` is written only after
# all five, so its absence beside a full set of reports means a leg failed.
gate_p1_swarm_evidence() {
  local dir
  for dir in "$OUT/p1-swarm" "$ROOT/target/p1-swarm"; do
    [[ -r $dir/clean.json ]] || continue
    local numbers
    numbers=$(jq -s -c '
      def leg($n): (.[$n] // {});
      {
        peers:                 (leg(0).peers),
        simulated_seconds:     (leg(0).seconds),
        clean_boundary_flips:  (leg(0).total_boundary_flips),
        clean_proxy_pops:      (leg(0).total_proxy_pops),
        clean_min_cells:       (leg(0).min_cells_visited),
        clean_worst_p99_up_bits: (leg(0).worst_p99_upload_bits),
        impaired_shed:         (leg(1).total_shed),
        impaired_undecodable:  (leg(1).total_undecodable),
        witnessed_player_hours:(leg(2).player_hours),
        witnessed_shed:        (leg(2).total_shed),
        witnessed_false_positives: (leg(2).total_false_positives),
        witnessed_coverage:    (leg(2).observation_coverage),
        conviction_false_positives: (leg(3).total_false_positives),
        control_false_positives:    (leg(4).total_false_positives)
      } | with_entries(select(.value != null))' \
      "$dir/clean.json" "$dir/impaired.json" "$dir/witnessed.json" \
      "$dir/conviction.json" "$dir/control.json" 2>/dev/null || echo '{}')
    if [[ -e $dir/PASSED ]]; then ev PASSED "$dir" "$numbers"; else ev FAILED "$dir" "$numbers"; fi
    return 0
  done
  ev_none
}

# ── P3 island ────────────────────────────────────────────────────────────────

gate_p3_island_tier() { echo full; }
gate_p3_island_prereq() {
  have_cargo || { echo 'cargo is not on PATH'; return 1; }
  return 0
}
# **Both legs, because a leg nothing runs is not a gate** (#129, AGENTS.md §A
# `--self-test` nothing runs is not a check). `P3_VICTIM_CLAIM_KIND` selects
# which half of the criterion a run exercises — `weak` redistributes, `strong`
# parks — and only the weak one was ever run anywhere. The strong leg was
# broken for months: every parked row was reported lost, the leg could not
# pass, and nothing said so because nothing ran it. The build dominates the
# cost; a second 30 s island is the cheapest insurance in this file.
gate_p3_island_run() {
  {
    cargo build --release --manifest-path "$ROOT/Cargo.toml" \
      -p orrery_persistd -p orrery_coordinator
    (cd "$ROOT/p3-island" && cargo build --release)
    local leg
    for leg in weak strong; do
      PERSISTD_BIN="$ROOT/target/release/persistd" \
      COORDINATOR_BIN="$ROOT/target/release/orrery-coordinator" \
      P3_ISLAND_BIN="$ROOT/p3-island/target/release/p3-island" \
      P3_VICTIM_CLAIM_KIND="$leg" \
      P3_GATE_OUT="$OUT/p3-island-$leg-$(date -u +%Y%m%dT%H%M%SZ)" \
        "$ROOT/scripts/p3-island-gate.sh" || return 1
    done
  } >"$OUT/logs/p3-island.log" 2>&1
}
# The gate writes to `$(pwd)/p3-island-<stamp>` by default and to $OUT when
# this script drives it; both are searched, because an evidence reader that
# only knew about its own runs would report NOT RUN on a directory a human
# produced ten minutes earlier.
#
# **One row per leg, not one row for the newest run.** With two legs, "newest
# directory wins" would report whichever ran last and silently drop the other
# — and dropping the strong leg is exactly the failure this gate just came
# out of. Which leg a directory holds is read from `victim_claim_kind` inside
# the report rather than off the directory name: the harness records the tier
# it actually claimed at, and a name is a label somebody typed.
gate_p3_island_evidence() {
  local dir leg
  declare -A leg_dir=()
  while read -r dir; do
    [[ -n $dir && -r $dir/report.json ]] || continue
    leg=$(jq -r '(.victim_claim_kind // empty) | ascii_downcase' "$dir/report.json" 2>/dev/null) || continue
    [[ -n $leg ]] || continue
    # Ascending, so the last directory seen for a leg is its newest run.
    leg_dir[$leg]=$dir
  done < <(ls -1d "$OUT"/p3-island-* "$ROOT"/p3-island-* 2>/dev/null | sort)
  (( ${#leg_dir[@]} )) || { ev_none; return 0; }

  local numbers='{}' evidence='' status=PASSED one
  for leg in "${!leg_dir[@]}"; do
    dir=${leg_dir[$leg]}
    # Every figure is lifted from the gate's own report and prefixed with the
    # leg it belongs to; nothing here recomputes a number the harness already
    # published, including the new disposition split — `parked_and_reserved`
    # is the count of the victim's rows the registrar refused to regrant
    # because they are reserved for it (D7 §4.3), and it is neither a
    # reassignment nor a loss.
    one=$(jq -c --arg leg "$leg" '{
      peers, entities_total, victim_entities,
      reassigned, parked, successors,
      parked_and_reserved, claimable_after_settle,
      unreachable_after_settle,
      refused: (.refused_after_settle | length),
      lost: (.lost | length),
      settled_in_ms, settle_budget_ms, lease_ttl_ms,
      reassigned_in_ms, parked_observed_in_ms,
      duplicate_authority, survivor_leases_lost,
      drain_leases_held_at_start, drain_parked_at_quiescence,
      drain_reassigned_during_close, drain_accounted_at_quiescence,
      drain_outstanding_at_quiescence, drain_quiesced,
      drain_last_disposition_in_ms, drain_quiescence_observed_in_ms,
      drain_observation_timeout_ms, drain_passed
    } | with_entries(select(.value != null)) | with_entries(.key |= ($leg + "_" + .))' \
      "$dir/report.json" 2>/dev/null || echo '{}')
    numbers=$(jq -cs 'add' <<<"$numbers
$one")
    evidence="${evidence:+$evidence }$dir"
    # A leg with no success artifact drags the row down: the gate writes it
    # last and writes it nowhere else, so its absence is the leg saying no.
    [[ -e $dir/PASSED ]] || status=FAILED
  done
  ev "$status" "$evidence" "$numbers"
}

# ── P3 sibling gateways ──────────────────────────────────────────────────────
#
# Two live gateways over disjoint shards, so unlike the island gate this one
# cannot run without FoundationDB: the whole question is what two processes
# sharing one fence and one lease tier do to each other's rows, and a volatile
# lease store would make them unrelated. It also *seeds* a world and activates
# two shard sets against the fence, so it consumes its cluster exactly as
# `p2-kill9` does — pointing it at the box's shared development database would
# either fail on the seeder's offline-load refusal or write into a database
# three runners and a developer are using. Refused here with a reason, so the
# answer is SKIPPED rather than FAILED.
gate_p3_siblings_tier() { echo full; }
gate_p3_siblings_prereq() {
  # `--inspect` runs nothing, so every clause below would only hide the
  # evidence already on disk behind a SKIPPED row that claimed the cluster was
  # the problem. The prerequisite is about *running* this gate.
  [[ ${MODE:-} == inspect ]] && return 0
  local cf=${ORRERY_FDB_CLUSTER_FILE:-}
  [[ -n $cf ]] || { echo 'ORRERY_FDB_CLUSTER_FILE is not set; two gateways must share one durable fence and lease tier'; return 1; }
  [[ -r $cf ]] || { echo "ORRERY_FDB_CLUSTER_FILE=$cf is not readable"; return 1; }
  command -v fdbcli >/dev/null || { echo 'fdbcli is not on PATH; cannot establish that the cluster is fresh'; return 1; }
  timeout 20 fdbcli -C "$cf" --exec 'status minimal' 2>/dev/null | grep -q 'is available' \
    || { echo "the cluster at $cf is not available"; return 1; }
  local rows
  rows=$(timeout 30 fdbcli -C "$cf" --exec 'getrangekeys a b 1' 2>/dev/null | grep -c '^`') || true
  [[ ${rows:-0} -eq 0 ]] \
    || { echo "the cluster at $cf already carries an actor/ activation row; this gate seeds a world and activates two shard sets, and needs a fresh throwaway cluster"; return 1; }
  have_cargo || { echo 'cargo is not on PATH'; return 1; }
  return 0
}
gate_p3_siblings_run() {
  {
    cargo build --release --manifest-path "$ROOT/Cargo.toml" -p orrery_persistd --features fdb
    cargo build --release --manifest-path "$ROOT/Cargo.toml" -p orrery_seed --features orrery_seed/fdb
    cargo build --release --manifest-path "$ROOT/Cargo.toml" -p orrery_coordinator
    (cd "$ROOT/p3-siblings" && cargo build --release)
    PERSISTD_BIN="$ROOT/target/release/persistd" \
    COORDINATOR_BIN="$ROOT/target/release/orrery-coordinator" \
    ORRERY_SEED_BIN="$ROOT/target/release/orrery-seed" \
    P3_SIBLINGS_BIN="$ROOT/p3-siblings/target/release/p3-siblings" \
    P3_SIBLINGS_GATE_OUT="$OUT/p3-siblings-$(date -u +%Y%m%dT%H%M%SZ)" \
      "$ROOT/scripts/p3-siblings-gate.sh"
  } >"$OUT/logs/p3-siblings.log" 2>&1
}
# Every number is read out of the gate's own `report.json` and none is
# re-derived here: a figure this script computed itself would be a second
# implementation of the gate, and the two would disagree exactly when it
# mattered. `duplicate_authority` in that file is already the sum over both
# registrars' exports; the two halves ride beside it so the sum can be checked
# rather than trusted.
gate_p3_siblings_evidence() {
  local dir
  dir=$(ls -1d "$OUT"/p3-siblings-* "$ROOT"/p3-siblings-2* 2>/dev/null | sort | tail -1) || true
  [[ -n ${dir:-} && -r $dir/report.json ]] || { ev_none; return 0; }
  local numbers status
  numbers=$(jq -c '{
    peers, entities_total, entities_gateway_a, entities_gateway_b,
    shards_gateway_a, shards_gateway_b,
    victim_entities, victim_entities_gateway_a, victim_entities_gateway_b,
    reassigned, parked, successors,
    settled_in_ms, settle_budget_ms, lease_ttl_ms,
    duplicate_authority, duplicate_authority_gateway_a, duplicate_authority_gateway_b,
    misrouted_claims, wrong_owner_probe,
    survivor_leases_lost,
    gateway_killed: .gateway_kill.killed,
    gateway_kill_clean: .gateway_kill.clean,
    survivor_leases_held_before: .gateway_kill.survivor_leases_held_before,
    survivor_leases_held_after: .gateway_kill.survivor_leases_held_after,
    survivor_leases_expired_after: .gateway_kill.survivor_leases_expired_after,
    lost: (.lost | length),
    handover_shards: .handover.shards_moved,
    handover_holders_divested: .handover.holders_divested,
    handover_expires_undelivered: .handover.expires_undelivered,
    handover_heartbeats_wrong_owner: .handover.heartbeats_rejected_wrong_owner,
    handover_duplicate_in_window: .handover.duplicate_authority_in_window,
    handover_worst_window_ms: .handover.worst_window_ms,
    handover_budget_ms: .handover.budget_ms,
    handover_within_split_target: .handover.within_split_handover_target,
    handover_clean: .handover.passed,
    race_rounds: .race.rounds,
    race_attempts_a: .race.attempts_gateway_a,
    race_attempts_b: .race.attempts_gateway_b,
    race_commits: .race.commits,
    race_commits_a: .race.commits_gateway_a,
    race_commits_b: .race.commits_gateway_b,
    race_rounds_one_owner: .race.rounds_with_one_owner,
    race_rounds_one_receipt: .race.rounds_with_one_receipt,
    race_rounds_loser_refused: .race.rounds_loser_definitively_refused,
    race_rounds_overlapped: .race.rounds_overlapped,
    race_max_dispatch_skew_us: .race.max_dispatch_skew_us,
    race_conflicts_observed: .race.conflicts_observed,
    race_unanswered: .race.unanswered_attempts,
    race_value_conserved: .race.value_conserved,
    race_clean: .race.passed
  }' "$dir/report.json" 2>/dev/null || echo '{}')
  if [[ -e $dir/PASSED ]]; then status=PASSED; else status=FAILED; fi
  ev "$status" "$dir" "$numbers"
}

# ── P5 dupe gauntlet ────────────────────────────────────────────────────────

gate_p5_dupe_gauntlet_tier() { echo full; }
gate_p5_dupe_gauntlet_prereq() {
  [[ ${MODE:-} == inspect ]] && return 0
  local cf=${ORRERY_FDB_CLUSTER_FILE:-}
  [[ -n $cf ]] \
    || { echo 'ORRERY_FDB_CLUSTER_FILE is not set; replay evidence requires a live FoundationDB cluster'; return 1; }
  [[ -r $cf ]] || { echo "ORRERY_FDB_CLUSTER_FILE=$cf is not readable"; return 1; }
  [[ ${P5_DUPE_CLUSTER_IS_THROWAWAY:-0} == 1 ]] \
    || { echo "set P5_DUPE_CLUSTER_IS_THROWAWAY=1 to assert $cf may receive the gauntlet's fixed ledger rows"; return 1; }
  command -v fdbcli >/dev/null || { echo 'fdbcli is not on PATH'; return 1; }
  timeout 20 fdbcli -C "$cf" --exec 'status minimal' 2>/dev/null | grep -q 'is available' \
    || { echo "the cluster at $cf is not available"; return 1; }
  have_cargo || { echo 'cargo is not on PATH'; return 1; }
  return 0
}
gate_p5_dupe_gauntlet_run() {
  {
    (cd "$ROOT/p5-dupe-gauntlet" && cargo build --release)
    P5_DUPE_BIN="$ROOT/p5-dupe-gauntlet/target/release/p5-dupe-gauntlet" \
    P5_DUPE_GATE_OUT="$OUT/p5-dupe-gauntlet-$(date -u +%Y%m%dT%H%M%SZ)" \
      "$ROOT/scripts/p5-dupe-gauntlet-gate.sh"
  } >"$OUT/logs/p5-dupe-gauntlet.log" 2>&1
}
gate_p5_dupe_gauntlet_evidence() {
  local dir
  dir=$(ls -1d "$OUT"/p5-dupe-gauntlet-* "$ROOT"/p5-dupe-gauntlet-2* 2>/dev/null | sort | tail -1) || true
  [[ -n ${dir:-} && -r $dir/report.json ]] || { ev_none; return 0; }
  local numbers status
  numbers=$(jq -c '{
    result,
    replay_passed: .arms.replay.passed,
    replay_submissions: .arms.replay.submissions,
    replay_intent_rows: .arms.replay.intent_rows,
    replay_receipts: .arms.replay.ledger_receipts,
    honest_control_passed: .arms.attestation.honest_control.passed,
    legacy_preimage_passed: .arms.attestation.legacy_preimage.passed,
    legacy_preimage_cause: .arms.attestation.legacy_preimage.audit_cause,
    issuer_as_witness_passed: .arms.attestation.issuer_as_witness.passed,
    issuer_as_witness_cause: .arms.attestation.issuer_as_witness.audit_cause,
    outside_set_passed: .arms.attestation.outside_announced_set.passed,
    outside_set_cause: .arms.attestation.outside_announced_set.audit_cause,
    non_required_subset_passed: .arms.attestation.non_required_subset.passed,
    non_required_subset_cause: .arms.attestation.non_required_subset.audit_cause,
    quarantine_passed: .arms.quarantine.passed,
    quarantine_cause: .arms.quarantine.audit_cause,
    quarantine_full_validation: .arms.quarantine.full_validation_path_proved_by_ordering
  }' "$dir/report.json" 2>/dev/null || echo '{}')
  if [[ -e $dir/PASSED ]]; then status=PASSED; else status=FAILED; fi
  ev "$status" "$dir" "$numbers"
}

# ── The enforcement ramp: shadow observes and does not act (#222) ───────────
#
# Same cluster posture as the dupe gauntlet — fixed ledger ids, a receipt-range
# read-back — so the prerequisite is that one's, with its own assertion
# variable. It runs two gateway processes with opposite postures against one
# cluster, which is why it is a row of its own rather than a fourth arm there.

gate_ramp_shadow_tier() { echo full; }
gate_ramp_shadow_prereq() {
  [[ ${MODE:-} == inspect ]] && return 0
  local cf=${ORRERY_FDB_CLUSTER_FILE:-}
  [[ -n $cf ]] \
    || { echo 'ORRERY_FDB_CLUSTER_FILE is not set; the ramp gate needs a live FoundationDB cluster'; return 1; }
  [[ -r $cf ]] || { echo "ORRERY_FDB_CLUSTER_FILE=$cf is not readable"; return 1; }
  [[ ${RAMP_SHADOW_CLUSTER_IS_THROWAWAY:-0} == 1 ]] \
    || { echo "set RAMP_SHADOW_CLUSTER_IS_THROWAWAY=1 to assert $cf may receive the ramp gate's fixed ledger rows"; return 1; }
  command -v fdbcli >/dev/null || { echo 'fdbcli is not on PATH'; return 1; }
  timeout 20 fdbcli -C "$cf" --exec 'status minimal' 2>/dev/null | grep -q 'is available' \
    || { echo "the cluster at $cf is not available"; return 1; }
  have_cargo || { echo 'cargo is not on PATH'; return 1; }
  return 0
}
gate_ramp_shadow_run() {
  {
    (cd "$ROOT/p5-dupe-gauntlet" && cargo build --release)
    P5_DUPE_BIN="$ROOT/p5-dupe-gauntlet/target/release/p5-dupe-gauntlet" \
    RAMP_SHADOW_GATE_OUT="$OUT/ramp-shadow-$(date -u +%Y%m%dT%H%M%SZ)" \
      "$ROOT/scripts/ramp-shadow-gate.sh"
  } >"$OUT/logs/ramp-shadow.log" 2>&1
}
gate_ramp_shadow_evidence() {
  local dir
  dir=$(ls -1d "$OUT"/ramp-shadow-* "$ROOT"/ramp-shadow-2* 2>/dev/null | sort | tail -1) || true
  [[ -n ${dir:-} && -r $dir/report.json ]] || { ev_none; return 0; }
  local numbers status
  # Every figure read out of the harness's own report. The pair in the middle
  # is the one that carries the gate's argument: refusals zero *and*
  # would-have-refused non-zero. Either number alone is satisfied by a control
  # that is simply off, so a reader given only one of them learns nothing.
  numbers=$(jq -c '{
    result,
    enforcing_acts: .arms.enforcing_acts.passed,
    enforcing_cause: .arms.enforcing_acts.audit_cause,
    enforcing_intent_rows: .arms.enforcing_acts.intent_rows,
    shadow_observes: .arms.shadow_observes.passed,
    shadow_verdict: .arms.shadow_observes.diagnostics.offender_observation.verdict,
    shadow_verdict_matches:
      .arms.shadow_observes.diagnostics.cross_gateway_verdict_matches_enforcing_audit_cause,
    shadow_does_not_act: .arms.shadow_does_not_act.passed,
    shadow_outcome_committed: (.arms.shadow_does_not_act.outcome | startswith("Committed")),
    shadow_attest_enforced: .arms.shadow_does_not_act.attest_row_enforced,
    shadow_refusals: .arms.shadow_does_not_act.refusals_in_shadow_run,
    shadow_would_act: .arms.shadow_does_not_act.would_act_observations,
    shadow_observations: .arms.shadow_does_not_act.observations,
    reversible: .arms.reversibility.passed,
    demote_apply_ms: .arms.reversibility.demotion.apply_ms,
    promote_apply_ms: .arms.reversibility.promotion.apply_ms,
    apply_bound_ms: .arms.reversibility.apply_bound_ms
  }' "$dir/report.json" 2>/dev/null || echo '{}')
  if [[ -e $dir/PASSED ]]; then status=PASSED; else status=FAILED; fi
  ev "$status" "$dir" "$numbers"
}

# ── P2 kill-9 ────────────────────────────────────────────────────────────────

gate_p2_kill9_tier() { echo full; }
# Two prerequisites, and the second is the interesting one. The gate's own
# pre-flight refuses a cluster that already carries an `actor/` activation row,
# because `--chain-epoch 1` is an assertion against a fence that only moves
# forward. Running it against the box's shared development cluster would either
# die on that assertion or — worse — succeed and consume a database three CI
# runners and a developer are using. Checked here so the answer is SKIPPED with
# a reason rather than FAILED with a misleading one.
gate_p2_kill9_prereq() {
  local cf=${ORRERY_FDB_CLUSTER_FILE:-}
  [[ -n $cf ]] || { echo 'ORRERY_FDB_CLUSTER_FILE is not set; the gate needs a FoundationDB cluster'; return 1; }
  [[ -r $cf ]] || { echo "ORRERY_FDB_CLUSTER_FILE=$cf is not readable"; return 1; }
  command -v fdbcli >/dev/null || { echo 'fdbcli is not on PATH; cannot establish that the cluster is fresh'; return 1; }
  timeout 20 fdbcli -C "$cf" --exec 'status minimal' 2>/dev/null | grep -q 'is available' \
    || { echo "the cluster at $cf is not available"; return 1; }
  local rows
  rows=$(timeout 30 fdbcli -C "$cf" --exec 'getrangekeys a b 1' 2>/dev/null | grep -c '^`') || true
  [[ ${rows:-0} -eq 0 ]] \
    || { echo "the cluster at $cf already carries an actor/ activation row; this gate consumes a fresh throwaway cluster and must not be pointed at a shared one"; return 1; }
  have_cargo || { echo 'cargo is not on PATH'; return 1; }
  return 0
}
gate_p2_kill9_run() {
  {
    cargo build --release --manifest-path "$ROOT/Cargo.toml" -p orrery_persistd --features fdb
    cargo build --release --manifest-path "$ROOT/Cargo.toml" -p orrery_seed --features orrery_seed/fdb
    (cd "$ROOT/p2-load" && cargo build --release)
    (cd "$ROOT/p2-dashboard" && cargo build --release)
    PERSISTD_BIN="$ROOT/target/release/persistd" \
    P2_LOAD_BIN="$ROOT/p2-load/target/release/p2-load" \
    P2_DASHBOARD_BIN="$ROOT/p2-dashboard/target/release/p2-dashboard" \
    ORRERY_SEED_BIN="$ROOT/target/release/orrery-seed" \
    P2_GATE_OUT="$OUT/p2-kill9-$(date -u +%Y%m%dT%H%M%SZ)" \
      "$ROOT/scripts/p2-kill9-gate.sh"
  } >"$OUT/logs/p2-kill9.log" 2>&1
}
gate_p2_kill9_evidence() {
  local dir
  dir=$(ls -1d "$OUT"/p2-kill9-* "$ROOT"/p2-kill9-* 2>/dev/null | sort | tail -1) || true
  [[ -n ${dir:-} ]] || { ev_none; return 0; }
  # The artifact is written last, after every correctness proof. Its result is
  # the whole-gate verdict: an unqualified latency clause is neither a pass nor
  # a failure. A directory without the final artifact is still a failed run.
  if [[ ! -r $dir/artifact.json ]]; then ev FAILED "$dir" '{}'; return 0; fi
  local numbers result status
  result=$(jq -r '.result // empty' "$dir/artifact.json" 2>/dev/null || true)
  case "$result" in
    pass)        status=PASSED ;;
    unqualified) status=UNQUALIFIED ;;
    *)           status=FAILED ;;
  esac
  numbers=$(jq -c '{
    result, recovery_cutoff,
    durable_acks: (.proofs.recovery.durable_acks // .proofs.recovery.acked // null),
    recovery_pass: .proofs.recovery.pass,
    latency_gate: .proofs.latency.gate,
    journal_commit_p99_ms: (.proofs.latency.series.journal_commit_ms.p99 // null),
    bulk_ack_p99_ms:       (.proofs.latency.series.bulk_ack_ms.p99 // null),
    intent_commit_p99_ms:  (.proofs.latency.series.intent_commit_ms.p99 // null),
    area_first_page_p99_ms:(.proofs.latency.series.area_first_page_ms.p99 // null),
    zombie_primary_fenced: .proofs.zombie_primary_fenced
  } | with_entries(select(.value != null))' "$dir/artifact.json" 2>/dev/null || echo '{}')
  ev "$status" "$dir" "$numbers"
}

# ── P4 accumulation and its ledger ───────────────────────────────────────────
#
# Not a gate and reported as one anyway, because "how many player-hours has P4
# banked" is the question this report exists to answer and the ledger is where
# the answer lives. The banking leg is a witnessed hour and needs the previous
# night's shard restored from a workflow artifact, so it is not something this
# box can honestly run; the comparability probe is, and it banks nothing.
gate_p4_accumulate_tier() { echo full; }
gate_p4_accumulate_prereq() { have_cargo || { echo 'cargo is not on PATH'; return 1; }; }
gate_p4_accumulate_run() {
  P4_ACCUM_OUT="$OUT/p4-accumulate" "$ROOT/scripts/p4-accumulate.sh" --probe \
    >"$OUT/logs/p4-accumulate.log" 2>&1
}
gate_p4_accumulate_evidence() {
  local probe
  probe=$(ls -1 "$OUT"/p4-accumulate/probe-*.json "$ROOT"/target/p4-accumulate/probe-*.json 2>/dev/null | tail -1) || true
  local ledger="${P4_LEDGER_FILE:-$ROOT/target/p4-ledger/hours.jsonl}"
  local banked=0
  if [[ -r $ledger ]]; then banked=$(awk 'END { print NR+0 }' "$ledger"); fi
  if [[ -z ${probe:-} ]]; then
    # No probe here, but the ledger is still worth reporting: a nightly's
    # shard restored into this tree would show up.
    ev 'NOT RUN' "$ledger" "$(jq -cn --argjson b "$banked" '{banked_ledger_lines: $b}')"
    return 0
  fi
  local numbers
  numbers=$(jq -c --argjson b "$banked" '{
    probe_target: .identity.target,
    probe_seed: .identity.seed,
    probe_peers: .peers,
    probe_seconds: .seconds,
    probe_player_hours: .player_hours,
    witnessing, observation_coverage, total_false_positives,
    total_shed, total_boundary_flips,
    banked_ledger_lines: $b
  } | with_entries(select(.value != null))' "$probe" 2>/dev/null || echo '{}')
  ev PASSED "$probe" "$numbers"
}

# The cross-platform comparison job. It needs three platforms' probe reports,
# two of which can only be produced on a hosted runner.
gate_p4_platform_ledger_tier() { echo full; }
gate_p4_platform_ledger_prereq() {
  echo 'needs probe reports from windows-latest and macos-latest runners; only a Linux probe can be produced here'
  return 1
}
gate_p4_platform_ledger_run() { return 0; }
gate_p4_platform_ledger_evidence() { ev_none; }

# ── the fdb tier ─────────────────────────────────────────────────────────────
#
# `orrery_seed`'s gates wipe key ranges outright and every suite writes at
# fixed keys, so this must never be pointed at a shared cluster (C-8). The
# opt-in variable is the whole prerequisite: there is no way to ask a cluster
# whether anybody minds it being wiped.
gate_fdb_tests_tier() { echo full; }
gate_fdb_tests_prereq() {
  local cf=${ORRERY_FDB_CLUSTER_FILE:-}
  [[ -n $cf ]] || { echo 'ORRERY_FDB_CLUSTER_FILE is not set'; return 1; }
  [[ -r $cf ]] || { echo "ORRERY_FDB_CLUSTER_FILE=$cf is not readable"; return 1; }
  [[ ${GATE_STATUS_FDB_IS_THROWAWAY:-0} == 1 ]] \
    || { echo "these suites wipe key ranges; set GATE_STATUS_FDB_IS_THROWAWAY=1 to assert $cf is a throwaway cluster"; return 1; }
  have_cargo || { echo 'cargo is not on PATH'; return 1; }
  return 0
}
gate_fdb_tests_run() {
  ORRERY_FDB_TEST_LOG="$OUT/fdb-tests.log" "$ROOT/scripts/fdb-tests.sh" \
    >"$OUT/logs/fdb-tests.log" 2>&1
}
# The count of tests that actually executed is the number this tier is about —
# the whole point of the wrapper is that `cargo test` is green on an
# unreachable cluster. Read off the log the same way the script's own
# `--check` reads it.
gate_fdb_tests_evidence() {
  local log
  for log in "$OUT/fdb-tests.log" "$ROOT/target/fdb-tests.log"; do
    [[ -r $log ]] || continue
    local executed skips status
    executed=$(grep -oE '^test result: [a-z]+\. [0-9]+ passed' "$log" \
      | awk '{ n += $5 } END { print n+0 }')
    skips=$(grep -c 'skipping:' "$log" || true)
    if "$ROOT/scripts/fdb-tests.sh" --check "$log" >/dev/null 2>&1; then status=PASSED; else status=FAILED; fi
    ev "$status" "$log" "$(jq -cn --argjson e "$executed" --argjson s "${skips:-0}" \
      '{tests_executed: $e, skip_lines: $s}')"
    return 0
  done
  ev_none
}

# ── #173 compute-identity smoke ──────────────────────────────────────────────
#
# Nightly.yml's `compute-identity-smoke` assumes orrery-ci-compute from the
# workflow's OIDC token and proves what the credential may and may not do.
# Delegated to scripts/aws-compute-smoke.sh rather than restated here — the
# determinism-soak lesson: a gate whose logic lives in a workflow drifts out
# of its own report. Tier is full, not fast: it needs resolvable AWS
# credentials and the network, neither of which a per-commit run may assume.
# The script's structural half runs per-commit in check.sh's gates lane.

gate_compute_identity_smoke_tier() { echo full; }
gate_compute_identity_smoke_prereq() {
  [[ ${MODE:-} == inspect ]] && return 0
  command -v aws >/dev/null || { echo 'aws CLI is not on PATH'; return 1; }
  # The probes are read-only or dry-run refusals, so a credential probe here
  # is the same class of prerequisite check the FDB gates make of their
  # cluster: cheap, honest, and the difference between SKIPPED-with-a-reason
  # and a misleading failure.
  timeout 30 aws sts get-caller-identity --output text --query Arn >/dev/null 2>&1 \
    || { echo 'no resolvable AWS credentials (aws sts get-caller-identity failed)'; return 1; }
  return 0
}
gate_compute_identity_smoke_run() {
  COMPUTE_SMOKE_OUT="$OUT/compute-identity-smoke" \
    "$ROOT/scripts/aws-compute-smoke.sh" >"$OUT/logs/compute-identity-smoke.log" 2>&1
}
gate_compute_identity_smoke_evidence() {
  local dir="$OUT/compute-identity-smoke"
  [[ -r $dir/result.json ]] || { ev_none; return 0; }
  local status=FAILED
  if [[ -e $dir/PASSED ]]; then status=PASSED; fi
  local numbers='{}'
  numbers=$(jq -c '{
    principal, account, region,
    candidates_found, images_found,
    positives_passed, denials_proved
  }' "$dir/result.json" 2>/dev/null) || numbers='{}'
  ev "$status" "$dir" "$numbers"
}

# ── the determinism soak ─────────────────────────────────────────────────────
#
# The one nightly gate with no script of its own: its body is inline in
# nightly.yml. Reproduced here rather than delegated, and that is a cost worth
# naming — this is the single place in this file where a gate's logic is
# restated instead of invoked, so a change to the workflow's loop does not
# reach it. Ten repeats in one process, requiring byte-identical digests; the
# failure it targets is per-process nondeterminism (VC-4/VC-8), which a single
# run cannot see.
gate_determinism_soak_tier() { echo full; }
gate_determinism_soak_prereq() { have_cargo || { echo 'cargo is not on PATH'; return 1; }; }
gate_determinism_soak_run() {
  local d="$OUT/soak"
  rm -rf "$d"; mkdir -p "$d"
  {
    cargo build --release --manifest-path "$ROOT/Cargo.toml" -p orrery_conformance
    local bin="$ROOT/target/release/orrery-conformance" i
    "$bin" emit --out "$d/soak-0.json" --compact
    for i in $(seq 1 9); do
      "$bin" emit --out "$d/soak-$i.json" --compact
      diff -q "$d/soak-0.json" "$d/soak-$i.json" >/dev/null \
        || { echo "run $i differs from run 0 — per-process nondeterminism (VC-4/VC-8)"; return 1; }
    done
    echo '10 runs produced byte-identical digests'
  } >"$OUT/logs/determinism-soak.log" 2>&1
}
gate_determinism_soak_evidence() {
  local d="$OUT/soak"
  [[ -r $d/soak-0.json ]] || { ev_none; return 0; }
  local runs identical
  runs=$(find "$d" -name 'soak-*.json' | wc -l)
  identical=$(find "$d" -name 'soak-*.json' -exec sha256sum {} + | awk '{ print $1 }' | sort -u | wc -l)
  local status=FAILED
  if [[ $identical -eq 1 && $runs -ge 2 ]]; then status=PASSED; fi
  ev "$status" "$d" "$(jq -cn --argjson r "$runs" --argjson u "$identical" \
    '{corpus_runs: $r, distinct_digests: $u}')"
}

# ── the per-commit lanes ─────────────────────────────────────────────────────
#
# ci.yml's fmt/clippy/gates/test have no bodies of their own; they invoke one
# lane of scripts/check.sh. Delegated here for the same reason.
_lane_gate() { # lane
  "$ROOT/scripts/check.sh" "$1" >"$OUT/logs/lane-$1.log" 2>&1
}
for _lane in fmt clippy gates test; do
  eval "gate_ci_${_lane}_tier() { echo full; }"
  eval "gate_ci_${_lane}_prereq() { have_cargo || { echo 'cargo is not on PATH'; return 1; }; }"
  eval "gate_ci_${_lane}_run() { _lane_gate ${_lane}; }"
  eval "gate_ci_${_lane}_evidence() { ev_none; }"
done
unset _lane

# The cross-platform determinism matrix and its verdict job. Three runners by
# construction; one machine cannot produce the comparison, and a Linux-only
# leg passing says nothing about the claim.
gate_ci_determinism_tier() { echo full; }
gate_ci_determinism_prereq() {
  echo 'a cross-platform matrix: needs ubuntu-latest, windows-latest and macos-latest runners'
  return 1
}
gate_ci_determinism_run() { return 0; }
gate_ci_determinism_evidence() { ev_none; }

gate_ci_determinism_verdict_tier() { echo full; }
gate_ci_determinism_verdict_prereq() {
  echo 'compares the matrix legs above; needs their three artifacts'
  return 1
}
gate_ci_determinism_verdict_run() { return 0; }
gate_ci_determinism_verdict_evidence() { ev_none; }

# ─────────────────────────────────────────────────────────────────────────────
# Reporting
# ─────────────────────────────────────────────────────────────────────────────

# `p4-accumulate` → `p4_accumulate`; `p1-swarm-gate.sh` → `p1_swarm`.
gate_key() {
  local k=$1
  k=${k%.sh}; k=${k%-gate}
  echo "${k//[-.]/_}"
}

ROWS=()

# One gate, start to finish: prerequisite, then either a run or a read.
evaluate() { # id kind fn_prefix source arg
  local id=$1 kind=$2 prefix=$3 source=$4 arg=${5:-}
  local status reason='' evidence='' numbers='{}' tier began=0 elapsed=0

  if ! declare -F "${prefix}_evidence" >/dev/null; then
    ROWS+=("$(jq -cn --arg id "$id" --arg k "$kind" --arg s "$source" \
      '{gate: $id, kind: $k, source: $s, status: "UNKNOWN", reason: "discovered, but scripts/gate-status.sh does not know how to run or read it; teach it a gate_<key>_{tier,prereq,run,evidence} trio", evidence: "", numbers: {}, duration_s: 0}')")
    return 0
  fi

  tier=$("${prefix}_tier")
  if reason=$("${prefix}_prereq" 2>&1); then
    reason=''
    local will_run=no
    case "$MODE" in
      inspect) will_run=no ;;
      fast)    if [[ $tier == fast ]]; then will_run=yes; fi ;;
      full)    will_run=yes ;;
    esac
    if [[ $will_run == yes ]]; then
      note "running $id"
      began=$(date +%s)
      if "${prefix}_run" "$arg"; then status=PASSED; else status=FAILED; fi
      elapsed=$(( $(date +%s) - began ))
    else
      status=''
    fi
    local e
    e=$("${prefix}_evidence" "$arg")
    evidence=$(jq -r '.evidence' <<<"$e")
    numbers=$(jq -c '.numbers' <<<"$e")
    local evidence_status
    evidence_status=$(jq -r '.status' <<<"$e")
    if [[ -z $status ]]; then
      status=$evidence_status
      if [[ $status == 'NOT RUN' ]]; then reason="mode '$MODE' does not run this gate and no evidence is on disk"; fi
    elif [[ $status == PASSED && $evidence_status == UNQUALIFIED ]]; then
      # The harness completed successfully so that its correctness evidence
      # could be retained, but the evidence is authoritative about whether the
      # whole criterion was qualified to pass.
      status=UNQUALIFIED
    fi
  else
    status=SKIPPED
  fi

  ROWS+=("$(jq -cn --arg id "$id" --arg k "$kind" --arg s "$source" --arg st "$status" \
    --arg r "$reason" --arg e "$evidence" --argjson n "$numbers" --argjson d "$elapsed" \
    '{gate: $id, kind: $k, source: $s, status: $st, reason: $r, evidence: $e, numbers: $n, duration_s: $d}')")
}

# The discovery pass, in one place so the self-test can watch it find things
# that were not here when it was written.
collect() {
  local script job key statics
  statics=$(discover_static_gates)

  for script in $(discover_scripts); do
    if grep -qxF "$script" <<<"$statics"; then
      evaluate "static:$script" static gate_static "scripts/$script" "$script"
    fi
    if script_has_self_test "$script"; then
      evaluate "selftest:$script" self-test gate_selftest "scripts/$script" "$script"
    fi
  done

  # Nightly jobs. A job is named by its own key and evaluated through the trio
  # for that key; the scripts it runs are recorded as the source so a reader
  # can go from a row to a file.
  for job in $(workflow_jobs nightly.yml); do
    key=$(gate_key "$job")
    local scripts kindnote
    # `|| true`: a job that runs no script at all — the determinism soak's
    # body is inline in the workflow — makes the `grep -o` in the pipeline exit
    # 1, and under `set -e` a bare assignment from it kills the report three
    # jobs before the end. Measured: that is exactly how the first real run of
    # this script died, with no output at all.
    scripts=$(job_real_scripts nightly.yml "$job" | paste -sd, - || true)
    kindnote="nightly.yml:$job@$(job_runner_kind nightly.yml "$job")"
    if [[ -n $scripts ]]; then kindnote="$kindnote (${scripts})"; fi
    evaluate "nightly:$job" nightly "gate_${key}" "$kindnote"
  done

  for job in $(workflow_jobs ci.yml); do
    key=$(gate_key "$job")
    evaluate "ci:$job" per-commit "gate_ci_${key}" "ci.yml:$job@$(job_runner_kind ci.yml "$job")"
  done
}

render() {
  local total=0 passed=0 failed=0 unqualified=0 notrun=0 skipped=0 unknown=0 row st

  echo
  echo "gate status — MODE: $MODE"
  case "$MODE" in
    fast)    echo "  fast: static gates and --self-test modes only. Every harness row below is" ;;
    full)    echo "  full: every harness whose prerequisites are satisfied on this machine was" ;;
    inspect) echo "  inspect: nothing was executed. Every row below is read from evidence on" ;;
  esac
  case "$MODE" in
    fast)    echo "  read from evidence or NOT RUN. A fast report is NOT a full one." ;;
    full)    echo "  actually executed. Rows marked SKIPPED were still not evaluated." ;;
    inspect) echo "  disk; an absent artifact reads as NOT RUN, never as a pass." ;;
  esac
  echo "  commit $COMMIT · $(date -u +%Y-%m-%dT%H:%M:%SZ) · jsonl: $JSONL"
  echo

  printf '  %-11s  %-34s  %s\n' STATUS GATE SOURCE
  printf '  %-11s  %-34s  %s\n' '-----------' '----------------------------------' '------'
  for row in "${ROWS[@]}"; do
    st=$(jq -r '.status' <<<"$row")
    printf '  %-11s  %-34s  %s\n' "$st" "$(jq -r '.gate' <<<"$row")" "$(jq -r '.source' <<<"$row")"
    total=$((total + 1))
    case "$st" in
      PASSED)      passed=$((passed + 1)) ;;
      FAILED)      failed=$((failed + 1)) ;;
      UNQUALIFIED) unqualified=$((unqualified + 1)) ;;
      'NOT RUN')   notrun=$((notrun + 1)) ;;
      SKIPPED)     skipped=$((skipped + 1)) ;;
      UNKNOWN)     unknown=$((unknown + 1)) ;;
    esac
  done

  # Reasons and numbers, only where there are any: a table of empty cells is
  # how a report stops being read.
  echo
  for row in "${ROWS[@]}"; do
    local reason numbers gate
    gate=$(jq -r '.gate' <<<"$row")
    reason=$(jq -r '.reason' <<<"$row")
    numbers=$(jq -r '.numbers | to_entries | map("\(.key)=\(.value)") | join("  ")' <<<"$row")
    st=$(jq -r '.status' <<<"$row")
    if [[ -z $reason && -z $numbers ]]; then continue; fi
    echo "  $gate [$st]"
    if [[ -n $reason ]]; then echo "    why: $reason"; fi
    if [[ -n $numbers ]]; then echo "    numbers: $numbers"; fi
  done

  echo
  echo "  $total gates · $passed passed · $failed failed · $unqualified unqualified · $notrun not run · $skipped skipped · $unknown unknown"
  echo "  produced by MODE=$MODE — skipped and not-run gates were not evaluated; unqualified gates were only partially evaluated; none are passes."
  echo

  # Precedence, and it is a judgement: a gate that actually failed outranks an
  # incomplete report, because a red gate is the more urgent fact and burying
  # it under "this report has a hole in it" is how it gets ignored. `2` is
  # therefore "nothing failed, and I could not evaluate everything".
  # `if`, not `&&`: under `set -e` a trailing `(( 0 > 0 )) && EXIT=1` is a
  # false compound whose non-zero status kills the script before it can report
  # the very zero it just computed. Measured — it exited 1 on a clean run.
  EXIT=0
  if (( unknown > 0 )); then EXIT=2; fi
  if (( failed > 0 )); then EXIT=1; fi
}

emit_jsonl() {
  local row
  : >"$JSONL"
  for row in "${ROWS[@]}"; do
    jq -c --arg mode "$MODE" --arg commit "$COMMIT" --arg at "$(date -u +%Y-%m-%dT%H:%M:%SZ)" \
      '{schema: "orrery.gate-status/1", mode: $mode, commit: $commit, run_at: $at} + .' <<<"$row" >>"$JSONL"
  done
}

# ─────────────────────────────────────────────────────────────────────────────
# --self-test
#
# Structural half in the house style — the haystack is this file's own text
# with comment lines stripped, because every pattern also appears in the clause
# that looks for it and an unrestricted grep over `$0` matches its own source
# and passes unconditionally (the anti-pattern fixed repo-wide in #35,
# AGENTS.md).
#
# Stripping comments is not enough on its own, and #135 is the proof: `body`
# used to run from the `Reporting` banner to EOF, and `self_test` lives below
# that banner — so every `has` clause found its own `has '…'` line and could
# only pass. Measured on the way in: a guarded stage was rewritten
# (`(( unknown > 0 ))` → `(( unknown >= 1 ))`) while its check line was left
# alone, and the self-test still reported green. `has_head`, bounded *at* the
# banner and therefore excluding the checks, was sound throughout.
#
# So both haystacks are now bounded on both ends, and neither contains this
# function:
#
#   head   `readonly NAME=` … the `Reporting` banner   — discovery
#   body   the `Reporting` banner … *this* banner      — evaluate/collect/render/emit
#
# The searched text and the checking text are disjoint by construction, which
# is the property that makes a clause capable of failing. A clause that needs
# to reach something below this banner — the argument dispatch, the final
# `exit "$EXIT"` — needs a third region, not a wider `body`; widening `body`
# back to EOF puts the checks inside the haystack again and re-creates #135.
#
# Functional half runs this script against a synthetic repository: a
# `scripts/` directory and a `nightly.yml` this file has never seen. That is
# the only way to check the claim that actually matters — that the report is
# discovered rather than typed — and it is also where the skip/pass distinction
# is exercised, because a synthetic tree can be made to skip on demand.
# ─────────────────────────────────────────────────────────────────────────────

self_test() {
  local body
  # Bounded at the banner above this function, not at EOF: see the note there.
  body="$(sed -n '/^# ─* *Reporting/,/^# ─* *--self-test/p' "$0" | grep -v '^[[:space:]]*#')"
  local head
  head="$(sed -n '/^readonly NAME=/,/^# ─* *Reporting/p' "$0" | grep -v '^[[:space:]]*#')"
  has() { grep -Fq -- "$1" <<<"$body"; }
  has_head() { grep -Fq -- "$1" <<<"$head"; }

  has_head 'find "$ROOT/scripts"' \
    || die 'self-test: the gate list is no longer discovered from the filesystem; a typed list is the thing that rots'
  has_head 'workflow_jobs() {' \
    || die 'self-test: workflow jobs are no longer discovered from the yml'
  has_head "awk '/^lane_gates" \
    || die 'self-test: the static gates are no longer read out of check.sh'
  has 'declare -F "${prefix}_evidence"' \
    || die 'self-test: a gate this reporter cannot read no longer reports UNKNOWN; it would vanish from the report'
  has 'status=SKIPPED' \
    || die 'self-test: the prerequisite branch no longer yields SKIPPED; a skip would read as something else'
  has 'MODE: $MODE' \
    || die 'self-test: the mode is no longer named in the human report; a fast run could be read as a full one'
  has '--arg mode "$MODE"' \
    || die 'self-test: the mode is no longer carried in the machine-readable records'
  has 'unknown > 0' \
    || die 'self-test: an incomplete report no longer changes the exit status'
  # Not structural: the string appears in this script's own usage text, so a
  # `grep` for it passes on a prerequisite that no longer consults it —
  # measured, that mutation survived. The prerequisite is called instead, in
  # both directions, because a clause that always refuses is as useless as one
  # that never does.
  local fdb_probe why
  fdb_probe="$(mktemp)"
  echo 'selftest:selftest@127.0.0.1:1' >"$fdb_probe"
  why=$(ORRERY_FDB_CLUSTER_FILE="$fdb_probe" GATE_STATUS_FDB_IS_THROWAWAY=0 \
        gate_fdb_tests_prereq 2>&1) \
    && die 'self-test: the fdb tier is runnable without the throwaway assertion; those suites wipe key ranges'
  grep -q 'throwaway' <<<"$why" \
    || die "self-test: the fdb tier was refused for some reason other than the missing throwaway assertion ('$why')"
  why=$(ORRERY_FDB_CLUSTER_FILE="$fdb_probe" GATE_STATUS_FDB_IS_THROWAWAY=1 \
        gate_fdb_tests_prereq 2>&1) || true
  if grep -q 'throwaway' <<<"$why"; then
    die 'self-test: the throwaway assertion is ignored; the opt-in can never be satisfied'
  fi
  rm -f "$fdb_probe"
  has_head "grep -c '^\`'" \
    || die "self-test: the P2 fresh-cluster pre-check is gone; the gate would be pointed at an already-activated cluster and its failure read as a defect"
  # The P3 island criterion has two halves and one environment variable that
  # picks between them, and for months only the `weak` half was ever run —
  # which is how #129 (the strong leg reporting every correctly-parked entity
  # as lost) survived undetected. Asserted against the runner in the head
  # region, where the string occurs exactly once and only as the loop that
  # drives the legs; deleting the strong leg from it fails here.
  has_head 'for leg in weak strong' \
    || die 'self-test: the island gate no longer runs both claim tiers; a leg nothing runs is not a gate'

  # ── Functional half ────────────────────────────────────────────────────────
  local dir
  dir="$(mktemp -d)"
  # shellcheck disable=SC2064  # expand now: $dir is what must be removed.
  trap "rm -rf '$dir'" EXIT

  mkdir -p "$dir/scripts" "$dir/.github/workflows"

  # A static gate that passes, one self-test that passes, one that fails, and
  # a gate script invented for this test that this file has never heard of.
  cat >"$dir/scripts/core-gates.sh" <<'EOF'
#!/usr/bin/env bash
echo 'synthetic static gate'
EOF
  cat >"$dir/scripts/happy-gate.sh" <<'EOF'
#!/usr/bin/env bash
if [[ ${1:-} == --self-test ]]; then echo ok; exit 0; fi
EOF
  cat >"$dir/scripts/sad-gate.sh" <<'EOF'
#!/usr/bin/env bash
if [[ ${1:-} == --self-test ]]; then echo broken; exit 1; fi
EOF
  # No `--self-test`, invoked by the synthetic nightly below: nothing here can
  # run or read it, so it must be UNKNOWN.
  cat >"$dir/scripts/p9-invented-gate.sh" <<'EOF'
#!/usr/bin/env bash
echo 'a gate that appeared after gate-status.sh was written'
EOF
  chmod +x "$dir"/scripts/*.sh

  # `lane_gates` is where the static gates are read from.
  cat >"$dir/scripts/check.sh" <<'EOF'
#!/usr/bin/env bash
lane_gates() {
    run scripts/core-gates.sh
    run scripts/happy-gate.sh --self-test
    run scripts/sad-gate.sh --self-test
}
EOF
  chmod +x "$dir/scripts/check.sh"

  cat >"$dir/.github/workflows/nightly.yml" <<'EOF'
name: synthetic
jobs:
  p2-kill9:
    runs-on: ubuntu-latest
    steps:
      - run: scripts/p2-kill9-gate.sh
  p9-invented:
    runs-on: ubuntu-latest
    steps:
      - run: scripts/p9-invented-gate.sh
  p9-inline:
    runs-on: ubuntu-latest
    steps:
      - run: echo "a gate whose body is inline in the workflow, like determinism-soak"
EOF

  # Not a command substitution: the exit status matters as much as the text,
  # and a subshell would take `ST_STATUS` with it when it ended.
  ST_STATUS=0
  st_run() { # mode -> $dir/report holds the human report, $ST_STATUS its exit
    set +e
    GATE_STATUS_ROOT="$dir" GATE_STATUS_OUT="$dir/out" \
      ORRERY_FDB_CLUSTER_FILE= "$0" "$1" >"$dir/report" 2>/dev/null
    ST_STATUS=$?
    set -e
  }

  local report
  st_run --fast; report=$(cat "$dir/report")

  # 1. Discovery: a gate that exists only in the synthetic tree is reported.
  grep -q 'selftest:happy-gate.sh' <<<"$report" \
    || die 'self-test: a gate script present in the tree did not appear in the report; the list is not discovered'
  grep -q 'nightly:p9-invented' <<<"$report" \
    || die 'self-test: a job present in nightly.yml did not appear in the report'
  # A job that runs no script at all — the determinism soak's body is inline in
  # the workflow — is still a gate, and the pipeline that lists a job's scripts
  # exits non-zero on one. The first real run of this script died exactly
  # there, three jobs from the end and with no output at all; this is the
  # regression guard for it.
  grep -q 'nightly:p9-inline' <<<"$report" \
    || die 'self-test: a workflow job with no script of its own did not appear in the report'

  # 2. A gate nothing here can run or read is UNKNOWN, and says so in the exit
  #    status. A report that quietly omits it is the failure this clause is for.
  grep -qE '^  UNKNOWN +nightly:p9-invented' <<<"$report" \
    || die 'self-test: an unteachable gate was not reported UNKNOWN'
  # This run also contains a gate that genuinely failed, and that outranks the
  # hole in the report: 1, not 2. The 2 is checked on the --inspect run below,
  # where nothing is executed and so nothing can fail.
  [[ $ST_STATUS == 1 ]] \
    || die "self-test: a run containing a failed gate exited $ST_STATUS rather than 1"

  # 3. Pass and failure are both real, and neither is the other.
  grep -qE '^  PASSED +selftest:happy-gate.sh' <<<"$report" \
    || die 'self-test: a passing self-test was not reported PASSED'
  grep -qE '^  FAILED +selftest:sad-gate.sh' <<<"$report" \
    || die 'self-test: a failing self-test was not reported FAILED'
  grep -qE '^  PASSED +static:core-gates.sh' <<<"$report" \
    || die 'self-test: a passing static gate was not reported PASSED'

  # 4. A missing prerequisite is SKIPPED and never PASSED. The synthetic
  #    nightly's p2-kill9 has no cluster, which is exactly the case that has
  #    bitten this repository before.
  grep -qE '^  SKIPPED +nightly:p2-kill9' <<<"$report" \
    || die 'self-test: a gate whose prerequisite is missing was not reported SKIPPED'
  grep -qE '^  (PASSED|FAILED|UNQUALIFIED) +nightly:p2-kill9' <<<"$report" \
    && die 'self-test: a skipped gate was also reported as a verdict; a skip must never read as a pass'

  # 5. The skip is counted apart from the passes in the summary line.
  grep -qE '1 skipped' <<<"$report" \
    || die 'self-test: the summary does not count skipped gates separately'

  # 6. The mode is stated in the human report, in both directions — a clause
  #    that always finds "fast" would pass on a full run mislabelled.
  grep -q 'MODE: fast' <<<"$report" \
    || die 'self-test: the fast report does not name its mode'
  grep -q 'A fast report is NOT a full one' <<<"$report" \
    || die 'self-test: the fast report does not say it is not a full one'
  local inspect_report
  st_run --inspect; inspect_report=$(cat "$dir/report")
  grep -q 'MODE: inspect' <<<"$inspect_report" \
    || die 'self-test: --inspect reported some other mode'
  grep -q 'MODE: fast' <<<"$inspect_report" \
    && die 'self-test: the mode banner is a constant; it does not follow the mode'

  # 7. --inspect executes nothing: the failing self-test above cannot be
  #    FAILED here, because nothing ran it.
  grep -qE '^  FAILED +selftest:sad-gate.sh' <<<"$inspect_report" \
    && die 'self-test: --inspect ran a gate; it must report from evidence only'
  grep -qE '^  NOT RUN +selftest:sad-gate.sh' <<<"$inspect_report" \
    || die 'self-test: --inspect did not report an unexecuted gate as NOT RUN'
  # Nothing ran, so nothing failed, and the only thing left to report is the
  # hole: exit 2.
  [[ $ST_STATUS == 2 ]] \
    || die "self-test: a report with an UNKNOWN gate and no failures exited $ST_STATUS rather than 2"

  # 8. The machine-readable half exists, is one record per reported row, and
  #    carries the mode. A JSONL that disagrees with the table is worse than
  #    none: it is what a diff between two nights would be built on.
  local jsonl human_rows json_rows
  jsonl="$dir/out/gate-status.jsonl"
  [[ -r $jsonl ]] || die 'self-test: no JSONL was emitted'
  json_rows=$(wc -l <"$jsonl")
  human_rows=$(grep -cE '^  (PASSED|FAILED|UNQUALIFIED|NOT RUN|SKIPPED|UNKNOWN) ' <<<"$inspect_report")
  [[ $json_rows -eq $human_rows ]] \
    || die "self-test: the JSONL has $json_rows records and the table $human_rows rows; they must be the same report"
  jq -e 'select(.mode != "inspect")' "$jsonl" >/dev/null \
    && die 'self-test: a JSONL record does not carry the mode that produced it'
  jq -e 'select(.gate == "nightly:p2-kill9" and .status == "SKIPPED" and (.reason | length) > 0)' "$jsonl" >/dev/null \
    || die 'self-test: the JSONL does not record why a gate was skipped'

  # 9. Numbers come out of the evidence file rather than from a run. A
  #    fabricated P1 report in the synthetic tree must be read back verbatim.
  mkdir -p "$dir/out/p1-swarm"
  local leg
  for leg in clean impaired witnessed conviction control; do
    jq -n '{peers: 32, seconds: 3600, total_boundary_flips: 0, total_proxy_pops: 0,
            min_cells_visited: 71, worst_p99_upload_bits: 123456, total_shed: 162,
            total_undecodable: 0, player_hours: 32.0, total_false_positives: 0,
            observation_coverage: 0.97}' >"$dir/out/p1-swarm/$leg.json"
  done
  touch "$dir/out/p1-swarm/PASSED"
  cat >>"$dir/.github/workflows/nightly.yml" <<'EOF'
  p1-swarm:
    runs-on: ubuntu-latest
    steps:
      - run: scripts/p1-swarm-gate.sh
EOF
  local with_numbers
  st_run --inspect; with_numbers=$(cat "$dir/report")
  grep -q 'clean_min_cells=71' <<<"$with_numbers" \
    || die 'self-test: a number in the gate report was not read back into the status report'
  grep -qE '^  PASSED +nightly:p1-swarm' <<<"$with_numbers" \
    || die "self-test: a gate's own success artifact was not read as a pass"
  rm -f "$dir/out/p1-swarm/PASSED"
  local without_marker
  st_run --inspect; without_marker=$(cat "$dir/report")
  grep -qE '^  FAILED +nightly:p1-swarm' <<<"$without_marker" \
    || die 'self-test: reports without the success artifact read as a pass; the artifact is what the gate writes last'

  # 10. A completed P2 run whose device was unqualified has real correctness
  #     evidence, but it did not evaluate the whole criterion. Exercise the
  #     live-run path, not only evidence inspection: a zero harness exit used
  #     to stamp PASSED before the artifact's latency non-verdict was read.
  mkdir -p "$dir/out/p2-kill9-unqualified"
  jq -n '{
      kind: "p2_two_process_kill9_gate", result: "unqualified",
      recovery_cutoff: "0:42",
      proofs: {
        recovery: {pass: true, durable_acks: 100},
        latency: {gate: "unqualified", series: {
          journal_commit_ms: {p99_us: 7000, threshold_us: 2000, gate: "unqualified"}
        }},
        zombie_primary_fenced: true,
        bumped_chain_epoch_refused: true,
        device_qualification: {qualified: false}
      }
    }' >"$dir/out/p2-kill9-unqualified/artifact.json"

  # Dynamic scope gives the real evaluator synthetic prerequisites and a
  # successful harness without weakening the production prerequisite or run.
  local OUT="$dir/out" MODE=full JSONL="$dir/out/p2-projection.jsonl" COMMIT=selftest EXIT=0
  local -a ROWS=()
  gate_p2_kill9_prereq() { return 0; }
  gate_p2_kill9_run() { return 0; }
  evaluate 'nightly:p2-kill9' nightly gate_p2_kill9 \
    'nightly.yml:p2-kill9@hosted (p2-kill9-gate.sh)'
  local p2_projection
  p2_projection=$(render)
  emit_jsonl
  grep -qE '^  PASSED +nightly:p2-kill9' <<<"$p2_projection" \
    && die 'self-test: an unqualified P2 report was rendered PASSED'
  grep -qE '^  UNQUALIFIED +nightly:p2-kill9' <<<"$p2_projection" \
    || die 'self-test: an unqualified P2 report did not get its own top-line state'
  grep -q '1 unqualified' <<<"$p2_projection" \
    || die 'self-test: the summary counted an unqualified P2 report as another state'
  jq -e 'select(.gate == "nightly:p2-kill9" and .status == "UNQUALIFIED")' "$JSONL" >/dev/null \
    || die 'self-test: the machine-readable P2 projection lost the unqualified state'

  # A later qualified artifact follows the unchanged path: successful process,
  # complete verdict, PASSED at the top line.
  mkdir -p "$dir/out/p2-kill9-zz-qualified"
  jq '.result = "pass"
      | .proofs.latency.gate = "pass"
      | .proofs.device_qualification.qualified = true' \
    "$dir/out/p2-kill9-unqualified/artifact.json" \
    >"$dir/out/p2-kill9-zz-qualified/artifact.json"
  ROWS=()
  evaluate 'nightly:p2-kill9' nightly gate_p2_kill9 \
    'nightly.yml:p2-kill9@hosted (p2-kill9-gate.sh)'
  p2_projection=$(render)
  grep -qE '^  PASSED +nightly:p2-kill9' <<<"$p2_projection" \
    || die 'self-test: a qualified passing P2 report no longer renders PASSED'

  # 11. The P3 island gate has two legs, and the report must carry both of them
  #     — with the disposition each leg actually produced. Fabricated here in
  #     the synthetic tree for the same reason as clause 9: it is the only way
  #     to check that these numbers are *read* rather than re-derived.
  #
  #     The strong leg is the one this clause exists for. Its rows park and
  #     stay reserved for the peer that died (D7 §4.3), the gate reported all
  #     fifty of them lost until #129, and a reader that showed only the newest
  #     run would have shown whichever leg happened to finish last.
  mkdir -p "$dir/out/p3-island-weak-20260101T000000Z" \
           "$dir/out/p3-island-strong-20260101T000100Z"
  jq -n '{peers: 8, entities_total: 400, victim_entities: 50,
          victim_claim_kind: "Weak", reassigned: 50, parked: 0, successors: 7,
          parked_and_reserved: 0, claimable_after_settle: 0,
          unreachable_after_settle: 0, refused_after_settle: [], lost: [],
          settled_in_ms: 9938, settle_budget_ms: 12050, lease_ttl_ms: 10000,
          duplicate_authority: 0, survivor_leases_lost: 0,
          drain_leases_held_at_start: 400, drain_parked_at_quiescence: 589,
          drain_reassigned_during_close: 378, drain_accounted_at_quiescence: 400,
          drain_outstanding_at_quiescence: 0, drain_quiesced: true,
          drain_last_disposition_in_ms: 10951, drain_quiescence_observed_in_ms: 12951,
          drain_observation_timeout_ms: 60000, drain_passed: true}' \
    >"$dir/out/p3-island-weak-20260101T000000Z/report.json"
  jq -n '{peers: 8, entities_total: 400, victim_entities: 50,
          victim_claim_kind: "Strong", reassigned: 0, parked: 50, successors: 0,
          parked_and_reserved: 50, claimable_after_settle: 0,
          unreachable_after_settle: 0, refused_after_settle: [], lost: [],
          settled_in_ms: 10700, settle_budget_ms: 12050, lease_ttl_ms: 10000,
          duplicate_authority: 0, survivor_leases_lost: 0,
          drain_leases_held_at_start: 350, drain_parked_at_quiescence: 350,
          drain_reassigned_during_close: 0, drain_accounted_at_quiescence: 350,
          drain_outstanding_at_quiescence: 0, drain_quiesced: true,
          drain_last_disposition_in_ms: 10950, drain_quiescence_observed_in_ms: 12950,
          drain_observation_timeout_ms: 60000, drain_passed: true}' \
    >"$dir/out/p3-island-strong-20260101T000100Z/report.json"
  touch "$dir/out/p3-island-weak-20260101T000000Z/PASSED" \
        "$dir/out/p3-island-strong-20260101T000100Z/PASSED"
  cat >>"$dir/.github/workflows/nightly.yml" <<'EOF'
  p3-island:
    runs-on: ubuntu-latest
    steps:
      - run: scripts/p3-island-gate.sh
EOF
  local both_legs
  st_run --inspect; both_legs=$(cat "$dir/report")
  grep -q 'weak_reassigned=50' <<<"$both_legs" \
    || die 'self-test: the weak leg vanished from the island row; the newest run is not the only leg'
  grep -q 'strong_parked_and_reserved=50' <<<"$both_legs" \
    || die "self-test: the strong leg's parked-and-reserved count is not read out of its report; it is the number #129 was about"
  grep -q 'weak_drain_reassigned_during_close=378' <<<"$both_legs" \
    || die 'self-test: the drain reassignment count was not read from the island report'
  grep -q 'weak_drain_accounted_at_quiescence=400' <<<"$both_legs" \
    || die 'self-test: reassigned-then-parked leases vanished from the drain accounting'
  grep -qE '^  PASSED +nightly:p3-island' <<<"$both_legs" \
    || die 'self-test: two passing island legs were not reported as a pass'
  # And one failing leg is a failing gate, however green the other one is.
  rm -f "$dir/out/p3-island-strong-20260101T000100Z/PASSED"
  local one_leg_down
  st_run --inspect; one_leg_down=$(cat "$dir/report")
  grep -qE '^  FAILED +nightly:p3-island' <<<"$one_leg_down" \
    || die 'self-test: a failed island leg was hidden by a passing one; a per-leg row that cannot go red is not a report'

  rm -rf "$dir"
  trap - EXIT
  echo "$NAME: self-test passed"
}

# ─────────────────────────────────────────────────────────────────────────────

readonly ROOT="${GATE_STATUS_ROOT:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}"

MODE=fast
JSONL=''
while [[ $# -gt 0 ]]; do
  case "$1" in
    --fast)      MODE=fast ;;
    --full)      MODE=full ;;
    --inspect)   MODE=inspect ;;
    --jsonl)     shift; JSONL=${1:?--jsonl needs a path} ;;
    --self-test) self_test; exit 0 ;;
    -h|--help)   usage; exit 0 ;;
    *)           usage; die "unknown argument '$1'" ;;
  esac
  shift
done

command -v jq >/dev/null || die 'jq is required and not on PATH'

OUT="${GATE_STATUS_OUT:-$ROOT/target/gate-status}"
mkdir -p "$OUT/logs"
JSONL=${JSONL:-$OUT/gate-status.jsonl}
COMMIT=$(git -C "$ROOT" rev-parse --short HEAD 2>/dev/null || echo unknown)

EXIT=0
collect
emit_jsonl
render
exit "$EXIT"
