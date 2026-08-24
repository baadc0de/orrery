#!/usr/bin/env bash
# Host-side campaign report assembly for human sessions (#387).
#
# The owner decision on #375 (2026-08-24) puts the human client on the network
# as an ordinary island member and puts **report assembly on the host**: the
# hosting harness (`p1-swarm --external-peer --witness --impaired`) produces
# every report-level field around the run exactly as it does for pure-bot
# runs, and the external participant contributes only its own `SessionRecord`.
# This script is the assembly seam between the two: it takes the host's raw
# report and the client's recorded row and emits the one `r.json` that
# `scripts/p4-ledger.sh append` banks — through the same path, against every
# existing refusal, with no human-rows-are-special branch.
#
# ── The return path, deliberately manual (#387) ─────────────────────────────
#
# The client writes its `SessionRecord` to `campaign-records.jsonl` beside its
# telemetry stream. At shakedown scale (§#345: ~16 sessions, operator present
# for every one) the volunteer hands that file — or, on a localhost session,
# the operator already has it — and the operator runs `assemble` and
# `p4-ledger.sh append` by hand. That is sufficient: the ledger's own
# validation is what makes the row bankable, not the channel it travelled.
# The later replacement is an S3 upload from the client (named in #375's
# decision record), which can replace this hand-off without touching anything
# upstream of it. It is deliberately not built here.
#
# ── Where the identity comes from ───────────────────────────────────────────
#
# The session id is **pre-minted offline** by `orrery-invite mint` (see
# `crates/orrery_identity/src/invite.rs`, "Where invites are minted"): a
# UUIDv7, unique under the operator's invite ledger, satisfying the ledger's
# `identity.human_session_id` constraint. The host pins it at join
# (`--require-session`), the client records it in its row, and this script
# refuses to assemble a report whose two copies disagree.
#
# ── What assembly checks, and why it refuses ────────────────────────────────
#
#   * the host report actually hosted an external participant, witnessed;
#   * the client row names exactly the pre-minted session id;
#   * the row's `impairment_mismatch` flag agrees with the row's own
#     observed/configured numbers (telemetry honesty: the flag must have
#     *fired* when observation disagreed with configuration — an assembled
#     row whose flag contradicts its numbers is refused here, and again by
#     `p4-ledger.sh` if edited after assembly);
#   * the pipeline digest is computed from the report's own commit over the
#     same four trees `p4-ledger.sh` hashes, and stamped into the row, so the
#     ledger's cross-check has something true to hold.
#
# ── Hosting a localhost session, end to end (the shakedown runbook) ────────
#
# Ahead of time (offline, operator's machine):
#   orrery-issuer-key generate --key-id 41 --output <outside-repo>/issuer.cred
#   orrery-invite mint --ledger invites.tsv --label "<volunteer>"
#     → account=N  invite_code=…  session_id=<UUIDv7>
# Minutes before the session (tokens live one hour):
#   orrery_regolith_client --print-slot-key <peers>          → <slot key>
#   orrery-invite session-token --issuer-credential issuer.cred \
#     --account N --node <slot key>                          → session_token=…
# The session (two processes; peers=8, seconds as desired):
#   p1-swarm --external-peer --peers 8 --seconds 3600 --min-cells 1 \
#     --impaired --witness --stamp-wall-clock --json raw.json \
#     --listening-file listening.txt \
#     --require-client-rev <pinned rev> --require-session <session_id> \
#     --issuer-key 41:<issuer public key>
#   orrery_regolith_client --campaign --campaign-consent \
#     --host-node <node> --host-direct <ip:port> --slot 8 \
#     --session-id <session_id> --session-token <token> \
#     --expect-loss 3 --expect-jitter-ms 100 \
#     --telemetry-jsonl out/session.jsonl
# Afterwards (the manual return path):
#   p4-campaign-session.sh assemble raw.json out/campaign-records.jsonl \
#     <session_id> r.json
#   p4-ledger.sh append r.json
#
# usage:
#   p4-campaign-session.sh assemble <raw-report.json> <campaign-records.jsonl> \
#       <session-uuid> <out.json>
#   p4-campaign-session.sh --self-test
set -euo pipefail

readonly NAME=p4-campaign-session
die() { echo "$NAME: $*" >&2; exit 2; }
note() { echo "$NAME: $*" >&2; }

readonly ROOT="$(cd "$(dirname "$0")/.." && pwd)"

need() { command -v "$1" >/dev/null || die "$1 is required and not on PATH"; }

sha256_hex() {
  if command -v sha256sum >/dev/null; then
    sha256sum
  elif command -v shasum >/dev/null; then
    shasum -a 256
  else
    die 'neither sha256sum nor shasum is on PATH; cannot compute the pipeline digest'
  fi
}

# The same four trees `scripts/p4-ledger.sh` hashes, in the same order, at the
# report's own commit. The self-test asserts the two lists are identical, so
# they cannot drift apart silently.
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

cmd_assemble() {
  local raw=${1:-} records=${2:-} session=${3:-} out=${4:-}
  [[ -n $raw && -r $raw ]] || die "assemble: unreadable host report '${raw:-<none>}'"
  [[ -n $records && -r $records ]] || die "assemble: unreadable client records '${records:-<none>}'"
  [[ -n $session ]] || die 'assemble: no session id given'
  [[ -n $out ]] || die 'assemble: no output path given'
  need jq

  [[ $session =~ ^[0-9a-f]{8}-[0-9a-f]{4}-7[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$ ]] \
    || die "assemble: session id '$session' is not a UUIDv7; mint one with orrery-invite"

  # The host must actually have hosted this run: an external participant in a
  # witnessed island. A report with no external block is a pure-bot run and
  # needs no assembly; handing one here is a mistake worth naming.
  jq -e '.external != null' "$raw" >/dev/null \
    || die 'assemble: the host report carries no external participant; nothing to assemble'
  jq -e '.witnessing == true' "$raw" >/dev/null \
    || die 'assemble: the host run was not witnessed; an unwitnessed hour banks nothing'

  # The client may have recorded several sessions into one JSONL; the invite
  # session id selects exactly one row.
  local row matches
  matches=$(jq -c --arg session "$session" 'select(.session_id == $session)' "$records" | awk 'END { print NR }')
  [[ $matches == 1 ]] \
    || die "assemble: expected exactly one client row for session $session, found $matches"
  row=$(jq -c --arg session "$session" 'select(.session_id == $session)' "$records")

  # Telemetry honesty, asserted in the assembled row (#387): the mismatch
  # flag must equal what the row's own numbers say. This is the client's
  # measured link against the operator's declared profile — if they disagree
  # and the flag did not fire, the row is not evidence and is refused here.
  jq -e '
    .impairment_mismatch ==
      ((.observed_loss_pct != .configured_impairment_profile.loss_pct)
       or (.observed_jitter_p50_ms != .configured_impairment_profile.jitter_p50_ms)
       or (.observed_jitter_p99_ms != .configured_impairment_profile.jitter_p99_ms))
  ' <<<"$row" >/dev/null \
    || die 'assemble: the client row'\''s impairment_mismatch contradicts its own observed/configured numbers'

  jq -e '.actor == "human"' <<<"$row" >/dev/null \
    || die 'assemble: the client row does not name a human actor'

  # The row must have been measured on the platform the report names, or the
  # ledger will refuse it anyway; refusing here names the real cause.
  local target
  target=$(jq -r '.identity.target' "$raw")
  jq -e --arg target "$target" '.platform_triple == $target' <<<"$row" >/dev/null \
    || die "assemble: client platform_triple $(jq -r .platform_triple <<<"$row") is not the host target $target"

  # The digest is computed from the report's own commit — the same arithmetic
  # `p4-ledger.sh` performs at append time — and stamped into the row. The
  # client cannot know it (`unavailable-client-side`); the host does.
  local commit pipeline
  commit=$(jq -r '.identity.commit // "unknown"' "$raw")
  pipeline=$(pipeline_id "$commit")

  jq --argjson row "$row" --arg session "$session" --arg pipeline "$pipeline" '
    .identity.actor = "human"
    | .identity.human_session_id = $session
    | .session = ($row | .pipeline_digest = $pipeline)
  ' "$raw" > "$out"
  note "assembled $out: session $session, pipeline $pipeline, commit ${commit:0:12}"
}

self_test() {
  need jq
  # The digest arithmetic must be `p4-ledger.sh`'s, tree for tree, in order.
  local ours theirs
  ours=$(printf '%s\n' "${PIPELINE_TREES[@]}")
  theirs=$(sed -n '/^readonly PIPELINE_TREES=($/,/^)$/p' "$ROOT/scripts/p4-ledger.sh" \
    | sed -n 's/^  \([a-z0-9_/-]*\)$/\1/p')
  [[ -n $theirs ]] || die 'self-test: cannot read PIPELINE_TREES out of p4-ledger.sh'
  diff <(echo "$ours") <(echo "$theirs") >/dev/null \
    || die 'self-test: the pipeline tree list drifted from p4-ledger.sh; the stamped digest would fail its cross-check'

  local dir
  dir="$(mktemp -d)"
  # shellcheck disable=SC2064  # expand now: $dir is what must be removed.
  trap "rm -rf '$dir'" EXIT
  export P4_PIPELINE_ID=selftestpipeline

  local session=018f8f4e-5c90-7abc-8123-0000000000aa
  jq -n '{
    identity: {
      seed: 5,
      impairment: { loss: 0.03, jitter_ticks: 6, jitter_rate: 0.1, retransmit_ticks: 3 },
      target: "x86_64-unknown-linux-gnu",
      commit: "0000000000000000000000000000000000000000"
    },
    peers: 5, seconds: 60, player_hours: 0.083,
    witnessing: true, total_false_positives: 0, observation_coverage: 1.0,
    deferral_ledger_balances: true, total_gaps: 12, total_shed: 0,
    external: { index: 4, said_goodbye: true, connected: false,
                uplink_frames: 100, downlink_frames: 400, downlink_dropped: 0 }
  }' > "$dir/raw.json"
  st_row() {
    jq -n --arg session "$1" --argjson mismatch "$2" --argjson observed "$3" '{
      session_id: $session,
      wall_start: "2026-08-24T12:00:00Z", wall_end: "2026-08-24T12:20:00Z",
      distinct_play_minutes: 20, banked_minutes: 20,
      platform_triple: "x86_64-unknown-linux-gnu", client_rev: "self-test",
      ruleset_id: "52", ruleset_version: 2, pipeline_digest: "unavailable-client-side",
      actor: "human",
      configured_impairment_profile: {loss_pct: 3, jitter_p50_ms: 100, jitter_p99_ms: 100},
      observed_loss_pct: $observed, observed_jitter_p50_ms: 100, observed_jitter_p99_ms: 100,
      afk_seconds: 0, afk_capped: false, impairment_mismatch: $mismatch
    }'
  }
  st_row "$session" true 3.4 > "$dir/records.jsonl"

  "$0" assemble "$dir/raw.json" "$dir/records.jsonl" "$session" "$dir/r.json" 2>/dev/null \
    || die 'self-test: an honest external session refused to assemble'
  jq -e --arg session "$session" '
    .identity.actor == "human"
    and .identity.human_session_id == $session
    and .session.session_id == $session
    and .session.pipeline_digest == "selftestpipeline"
    and .session.impairment_mismatch == true
  ' "$dir/r.json" >/dev/null \
    || die 'self-test: the assembled report is missing its identity or session stamping'

  # The assembled report must be exactly what the ledger banks: run it
  # through the real `append`, not a re-statement of its rules.
  P4_LEDGER_FILE="$dir/hours.jsonl" "$ROOT/scripts/p4-ledger.sh" append "$dir/r.json" >/dev/null 2>&1 \
    || die 'self-test: p4-ledger.sh refused the assembled report'

  # A row whose flag did not fire while its numbers disagree must refuse.
  st_row "$session" false 3.4 > "$dir/records.jsonl"
  if "$0" assemble "$dir/raw.json" "$dir/records.jsonl" "$session" "$dir/r2.json" 2>/dev/null; then
    die 'self-test: a row whose mismatch flag failed to fire assembled anyway'
  fi

  # A row for some other session must refuse: the invite id is the identity.
  st_row 018f8f4e-5c90-7abc-8123-0000000000bb true 3.4 > "$dir/records.jsonl"
  if "$0" assemble "$dir/raw.json" "$dir/records.jsonl" "$session" "$dir/r3.json" 2>/dev/null; then
    die 'self-test: a record for a different session id assembled anyway'
  fi

  # A report that hosted nobody external must refuse.
  st_row "$session" true 3.4 > "$dir/records.jsonl"
  jq '.external = null' "$dir/raw.json" > "$dir/raw-nobody.json"
  if "$0" assemble "$dir/raw-nobody.json" "$dir/records.jsonl" "$session" "$dir/r4.json" 2>/dev/null; then
    die 'self-test: a report with no external participant assembled anyway'
  fi

  rm -rf "$dir"
  trap - EXIT
  echo "$NAME: self-test passed"
}

case ${1:-} in
  assemble) shift; cmd_assemble "$@" ;;
  --self-test) self_test ;;
  *)
    sed -n '/^# usage:/,/^set -euo/p' "$0" | sed '$d' >&2
    die "unknown command '${1:-<none>}'"
    ;;
esac
