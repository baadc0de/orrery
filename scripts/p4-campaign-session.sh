#!/usr/bin/env bash
# Host-side campaign report assembly for human sessions (#387), repaired for
# multi-human attempts (#576).
#
# The owner decision on #375 (2026-08-24) puts the human client on the network
# as an ordinary island member and puts **report assembly on the host**: the
# hosting harness (`gates/p1-swarm --external-peer --witness --impaired`) produces
# every report-level field around the run exactly as it does for pure-bot
# runs, and the external participants contribute only their own `SessionRecord`s.
# This script is the assembly seam between the two: it takes the host's raw
# attempt report and the clients' recorded rows and emits the ledger inputs that
# `scripts/p4-ledger.sh append` banks — through the same path, against every
# existing refusal, with no humans-are-special branch.
#
# ── What an attempt assembles into (#576, and the contract it honours) ───────
#
# `docs/plans/multi-human-attempt-accounting.md` (#572) is normative here, and
# `scripts/p4-attempt-accounting.py` is its executable half. This script does not
# restate that arithmetic; it *delegates* to it, because two implementations of a
# denominator disagree exactly when it matters. What it emits is **one ledger
# input per actor contribution**:
#
#   bot contribution     player_hours = B * valid_attempt_seconds / 3600
#   human contribution   player_hours = banked_minutes / 60   (one per signed interval)
#
# Never the attempt total copied onto a participant. A one-hour attempt with four
# bots and two humans who bank 50 and 42 minutes is 4.000 + 0.8333 + 0.7000 =
# 5.5333 player-hours over three ledger inputs — not 6.000 banked on each of the
# two human rows, which is what this seam did before #576 and which would have
# claimed sixteen player-hours from one hour of play.
#
# Every human input binds to exactly one exterior `(attempt_id, slot,
# session_id, node)`; no two inputs bind to one seat, and no interval appears
# twice. A human's target triple is **that participant's signed
# `platform_triple`**, so an honest Windows client on a Linux host assembles and
# banks into the `windows` bucket the criterion counts from; the host's own
# triple survives verbatim as `attempt.host_target` on every row.
#
# ── The return path, deliberately manual (#387) ─────────────────────────────
#
# The clients write their `SessionRecord`s to `campaign-records.jsonl` beside
# their telemetry streams. At shakedown scale (§#345: ~16 sessions, operator
# present for every one) the volunteers hand those files — or, on a localhost
# session, the operator already has them — and the operator concatenates them,
# runs `assemble`, and appends each emitted input with `p4-ledger.sh append` by
# hand. That is sufficient: the ledger's own validation is what makes a row
# bankable, not the channel it travelled. The later replacement is an S3 upload
# from the client (named in #375's decision record), which can replace this
# hand-off without touching anything upstream of it.
#
# ── Where the identity comes from, and the two-copy reconciliation ──────────
#
# Each session id is **pre-minted offline** by `orrery-invite mint` (see
# `crates/orrery_identity/src/invite.rs`, "Where invites are minted"): a
# UUIDv7, unique under the operator's invite ledger, satisfying the ledger's
# `identity.human_session_id` constraint.
#
# #476 is explicit that the client's uploaded copy does **not** make the host's
# redundant: the two are checked against each other, and assembly refuses when
# they disagree. With several humans in one attempt that reconciliation is **per
# seat, not per attempt**. Three copies exist, and each pair that can be
# compared is compared:
#
#   * the host's, one per seat — `attempt.exteriors[].session_id` when a report
#     carries #572's contract field, and otherwise the QUIC-authenticated
#     `node` the host admitted at that seat;
#   * the client's, `records.jsonl`'s `session_id`, in a row signed by that
#     same node;
#   * the operator's, the session ids given on the command line — the ids that
#     were minted and pinned with `--require-session`.
#
# **What the host records, verified rather than assumed.** `ExteriorReport`
# (`gates/p1-swarm/src/swarm.rs`, after #579) carries `index`, `node`,
# `connected_ticks`, the frame counters, `said_goodbye`, `connected` and
# `witness_anchored` — and **no `session_id`**. So on every report the host
# writes today the host half of the id comparison is empty, and pretending
# otherwise would make this check vacuous. What the host does record per seat is
# the node it admitted, which the client signs into its own row. So:
#
#   * seated ids present  → host ids must equal the operator's ids, and each
#     row binds to the seat holding its id;
#   * seated ids absent   → each row binds to the seat whose admitted node it
#     signed, and the operator's ids must equal the ids the client rows carry;
#   * either way          → a seat that does seat an id must agree with the row
#     that lands on it by node, or the two copies disagree and it is refused.
#
# **A node is not a seat (#1028).** #579 read the admitted node as unique per
# seat, and it is not: the key is persistent per *install*, so a volunteer who
# relaunches inside one attempt is readmitted under it at a second seat, against
# a second pre-minted id — which is exactly what #1015's 45-second eviction hold
# and #1002's next-launch upload retry tell them to do. The seat is therefore
# `(node, session_id)`, and the projection onto `node` alone is allowed to
# collide. When a report seats no ids the node is all there is, so a node it
# admitted twice is genuinely ambiguous and still refused.
#
# A row whose id is not seated, or whose node this attempt admitted nowhere, is
# refused; a seat no row claims contributes nothing; an id seated twice, or
# appearing in two rows, is refused as one interval attributed twice. Giving the
# operator's copy is optional for the cohort form and required for the one-seat
# form.
#
# ── What assembly checks, and why it refuses ────────────────────────────────
#
#   * the host report actually hosted external participants, witnessed;
#   * the operator's pinned ids and the host's seated ids agree, seat by seat;
#   * the client rows name exactly those pre-minted session ids;
#   * each row's signature verifies under the QUIC-authenticated NodeId the host
#     recorded **at that seat**, binding every client-owned field to the key
#     admitted there — the session token authenticates a NodeId but does not by
#     itself reserve a seat;
#   * each row's `impairment_mismatch` flag agrees with the row's own
#     observed/configured numbers (telemetry honesty: the flag must have
#     *fired* when observation disagreed with configuration — an assembled
#     row whose flag contradicts its numbers is refused here, and again by
#     `p4-ledger.sh` if edited after assembly);
#   * no interval exceeds its own seat's connected span, one tick of tolerance.
#     That span is the host's own wall bracket between binding the seat and
#     releasing it, not a tick count scaled at the nominal rate: the host's
#     metronome never makes up an overrun, so the scaled count is short by
#     however much it lagged and refused seven honest attempts (#971). A
#     report carrying no bracket still falls back to the tick count, which
#     is the stricter of the two;
#   * the pipeline digest is computed from the report's own commit over the
#     same four trees `p4-ledger.sh` hashes, and stamped into each row, so the
#     ledger's cross-check has something true to hold.
#
# **Nothing is written until every row binds and validates.** A refusal that has
# already emitted the bot contribution is not a refusal: it leaves the operator
# holding a directory of bankable-looking inputs for an attempt this seam
# rejected. The derivation runs into a private temporary directory and its
# results are moved into place only once all of them exist.
#
# ── Hosting a session, end to end (the shakedown runbook) ──────────────────
#
# Ahead of time (offline, operator's machine), once per volunteer:
#   orrery-issuer-key generate --key-id 41 --output <outside-repo>/issuer.cred
#   orrery-invite mint --ledger invites.tsv --label "<volunteer>"
#     → account=N  invite_code=…  session_id=<UUIDv7>
# Minutes before the session (tokens live one hour):
#   orrery_regolith_client --print-slot-key <peers>          → <persistent client key>
#   orrery-invite session-token --issuer-credential issuer.cred \
#     --account N --node <slot key>                          → session_token=…
# Or, preferred for a volunteer handoff (the existing argv form below remains
# compatible with campaign automation): add --join-file volunteer.join.json
# --host-node <node> --slot 8 --session-id <session_id>, then launch the client
# with --join volunteer.join.json (and --host-direct when discovery is absent).
# The session (peers=8, seconds as desired; one client process per volunteer):
#   gates/p1-swarm --external-peer --peers 8 --seconds 3600 --min-cells 1 \
#     --impaired --witness --stamp-wall-clock --json attempt.json \
#     --listening-file listening.txt \
#     --require-client-rev <pinned rev> --require-session <session_id> \
#     --issuer-key 41:<issuer public key>
#   orrery_regolith_client --campaign --campaign-consent \
#     --host-node <node> --host-direct <ip:port> --slot 8 \
#     --session-id <session_id> --session-token <token> \
#     --expect-loss 3 --expect-jitter-ms 100 \
#     --telemetry-jsonl out/session.jsonl
# Afterwards (the manual return path), with every volunteer's rows concatenated
# into one JSONL:
#   p4-campaign-session.sh assemble attempt.json campaign-records.jsonl inputs/ \
#     <session_id> [<session_id> …]
#   for input in inputs/*.json; do p4-ledger.sh append "$input"; done
#
# usage:
#   p4-campaign-session.sh assemble <attempt-report.json> <campaign-records.jsonl> \
#       <out-dir> [<pinned-session-uuid> …]
#   p4-campaign-session.sh assemble <attempt-report.json> <campaign-records.jsonl> \
#       <session-uuid> <out.json>
#   p4-campaign-session.sh --self-test
set -euo pipefail

readonly NAME=p4-campaign-session
die() { echo "$NAME: $*" >&2; exit 2; }
note() { echo "$NAME: $*" >&2; }

readonly ROOT="$(cd "$(dirname "$0")/.." && pwd)"

need() { command -v "$1" >/dev/null || die "$1 is required and not on PATH"; }

readonly UUID_V7_RE='^[0-9a-f]{8}-[0-9a-f]{4}-7[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$'

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

# The host's own account of which invite it seated where, when it has one.
#
# `exteriors` is #572's contract field and carries `session_id`. `external` is
# what `gates/p1-swarm` emits, and since #579 it is a slot-ordered array of
# `ExteriorReport` — `index`, `node`, `connected_ticks`, the frame counters, the
# close flags, and **no invite id**. So this is empty for every report the host
# writes today, and that is a fact about the host rather than a shortcut here:
# see `reconcile_pinned_sessions` for what is checked in its place.
seated_session_ids() {
  jq -r '
    ((.exteriors // .external // []) | if type == "array" then . else [.] end)
    | map(.session_id | select(. != null)) | sort | .[]
  ' "$1"
}

# The client's own copy, one per signed row.
recorded_session_ids() {
  jq -r '.session_id // empty' "$1" | sort
}

# ── The stages every form runs, before anything is derived ──────────────────
#
# Read against the file rather than against the caller: `assemble` is the case
# where "the operator checked" is not a fact about the report on disk.
assemble_preflight() {
  local attempt=$1 records=$2
  [[ -n $attempt && -r $attempt ]] || die "assemble: unreadable host report '${attempt:-<none>}'"
  [[ -n $records && -r $records ]] || die "assemble: unreadable client records '${records:-<none>}'"
  need jq
  need openssl
  need python3

  # The host must actually have hosted this run: external participants in a
  # witnessed island. A report with no exterior rows is a pure-bot run and needs
  # no assembly; handing one here is a mistake worth naming.
  #
  # Two spellings, and **both must be non-empty arrays**. `exteriors` is #572's
  # contract field; `external` is what `gates/p1-swarm` emits since #579 turned
  # `ExteriorReport` into a slot-ordered `Vec`. #579's `type == "array"` check is
  # retained rather than relaxed to "present": a report carrying the pre-#579
  # singular *object* is not this host's output, and reading it as a one-element
  # cohort by accident is how an unrecognised report shape banks anyway.
  #
  # The UUIDv7 check on the session id that used to sit here has not been
  # dropped — it moved to where the ids now arrive: `reconcile_pinned_sessions`
  # checks every operator-pinned id, and `cmd_assemble_one_seat` checks its own
  # before anything else. `assemble_preflight` has no session id in scope.
  jq -e '(.exteriors // .external) | type == "array"' "$attempt" >/dev/null \
    || die 'assemble: the host report carries no external participant; nothing to assemble'
  jq -e '(.exteriors // .external) | length > 0' "$attempt" >/dev/null \
    || die 'assemble: the host report seated no external participant; nothing to assemble'
  jq -e '.witnessing == true' "$attempt" >/dev/null \
    || die 'assemble: the host run was not witnessed; an unwitnessed hour banks nothing'
}

# Authenticity, not arithmetic, retained from #579 and made precise per seat
# (#1028). The host records the remote identity that completed QUIC
# authentication at each seat, and the client signs every client-owned row field
# with that same persistent key. What #579 added, and what a per-attempt check
# would lose, is **exactly once**: a report that names one row's seat twice is an
# ambiguous seat map, and a row bound into it is bound to nobody in particular.
# The signature itself is verified per seat against the bound node inside the
# derivation, and again by `p4-ledger.sh` before the row banks.
#
# The seat that must be named exactly once is `(node, session_id)`, not the node
# alone. A persistent identity key belongs to an *install*, not to a seat: a
# volunteer who closes the client and launches it again inside one attempt —
# which #1015's 45-second eviction hold and #1002's next-launch upload retry both
# invite them to do — is readmitted under that same key at a second seat, against
# a second pre-minted session id. That is two signed intervals, not one
# ambiguity, and the host's own report tells them apart: each exterior entry
# carries the `session_id` it seated, and each client row names the session it
# belongs to. Keying on the projection onto `node` refused the first honest
# four-seat human attempt outright, costing three uninvolved seats their hours
# along with the rejoining one.
#
# So the projection onto `node` alone is allowed to collide, and every refusal
# either side of it is retained verbatim: a row signed by a key this attempt
# admitted nowhere is still somebody else's hour, and a key the host seated
# twice *under one session id* — or twice with no id to tell the two seats
# apart — is still the ambiguous seat map #579 named.
require_each_row_names_one_seat() {
  local attempt=$1 records=$2 node session seats
  while IFS=$'\t' read -r node session; do
    [[ -n $node && $node != null ]] \
      || die 'assemble: a client row does not name its measurement node'
    seats=$(jq --arg node "$node" \
      '[(.exteriors // .external)[] | select(.node == $node)] | length' "$attempt")
    # Zero and several are different mistakes and get different names: a row
    # signed by a key this attempt never admitted is somebody else's hour, while
    # a seat this row cannot be told apart from is an ambiguous seat map.
    [[ $seats != 0 ]] \
      || die "assemble: the host report admitted no seat for the node a client row is signed by; that row is not seated in attempt"
    if [[ $seats != 1 ]]; then
      # The key is seated more than once, which is what a rejoin looks like. It
      # is unambiguous only if this row's own session id picks exactly one of
      # those seats out; the same key seated twice under one id, or twice under
      # none, still has no seat to offer this row in particular.
      seats=$(jq --arg node "$node" --arg session "$session" \
        '[(.exteriors // .external)[]
          | select(.node == $node and .session_id == $session)] | length' "$attempt")
      [[ $seats == 1 ]] \
        || die 'assemble: the host report does not name the authenticated external node exactly once'
    fi
  done < <(jq -r '[.measurement_node // "null", .session_id // "null"] | @tsv' "$records")
}

# The operator's copy, checked against the host's, seat by seat. This is #476's
# two-copy reconciliation generalized: with one human it is the check that has
# always been here; with several it must not collapse into "the attempt has the
# right set of people somewhere".
reconcile_pinned_sessions() {
  local attempt=$1 records=$2
  shift 2
  local pinned=("$@")
  local id
  for id in "${pinned[@]}"; do
    [[ $id =~ $UUID_V7_RE ]] \
      || die "assemble: session id '$id' is not a UUIDv7; mint one with orrery-invite"
  done
  local host_ids row_ids operator_ids
  host_ids=$(seated_session_ids "$attempt")
  row_ids=$(recorded_session_ids "$records")
  operator_ids=$(printf '%s\n' "${pinned[@]}" | sort)

  # Against the host, when the host has an id to offer. This is #476's check in
  # its original form and it is not weakened; it simply has nothing to compare
  # against on a report whose `ExteriorReport`s carry no invite id.
  if [[ -n $host_ids ]]; then
    diff <(echo "$host_ids") <(echo "$operator_ids") >/dev/null || die \
      "assemble: the host's record of what it seated and the session ids pinned at join disagree (host: $(tr '\n' ' ' <<<"$host_ids"); operator: $(tr '\n' ' ' <<<"$operator_ids"))"
  fi

  # Against the client, always. When the host records no ids this is the copy
  # that keeps the operator's list load-bearing: a records file carrying a row
  # nobody pinned, or missing one that was, is refused here rather than quietly
  # assembling whatever turned up. The remaining pair — the host's seat and the
  # client's row — is checked per seat inside the derivation, by invite id when
  # the host seats one and by the admitted node when it does not.
  diff <(echo "$row_ids") <(echo "$operator_ids") >/dev/null || die \
    "assemble: the client rows and the session ids pinned at join disagree (rows: $(tr '\n' ' ' <<<"$row_ids"); operator: $(tr '\n' ' ' <<<"$operator_ids"))"
}

# One client row per pinned session, and exactly one. A records file the
# volunteer appended to twice, or one carrying a row for somebody else's
# session, is refused before anything derives — retained verbatim from #387,
# and now per seat.
require_one_row_per_session() {
  local records=$1
  shift
  local id matches
  for id in "$@"; do
    matches=$(jq -c --arg session "$id" 'select(.session_id == $session)' "$records" \
      | awk 'END { print NR }')
    [[ $matches == 1 ]] \
      || die "assemble: expected exactly one client row for session $id, found $matches"
  done
}

# The arithmetic, the binding and the per-seat refusals all live in #572's
# contract script. Deriving through it rather than beside it is what keeps the
# denominator single-sourced: a second implementation here would disagree with
# the contract exactly when a cohort over-counted.
derive_into() {
  local attempt=$1 records=$2 out=$3
  python3 "$ROOT/scripts/p4-attempt-accounting.py" derive "$attempt" "$records" "$out"
}

cmd_assemble() {
  # Two spellings, and the one-seat form is a special case rather than a second
  # path: it derives the whole attempt — so every cross-row refusal still fires —
  # and then places the seat the operator asked about.
  local attempt=${1:-} records=${2:-}
  if [[ $# -eq 4 && ${3:-} =~ $UUID_V7_RE && ${4:-} == *.json ]]; then
    cmd_assemble_one_seat "$attempt" "$records" "$3" "$4"
    return
  fi
  local out=${3:-}
  [[ -n $out ]] || die 'assemble: no output directory given'
  shift 3 || true
  assemble_preflight "$attempt" "$records"
  if [[ $# -gt 0 ]]; then
    # Exactly-one-row first, as in the one-seat form: a records file the
    # volunteer appended to twice has a precise name for what is wrong with it,
    # and the set comparison below would report it as a mismatched list.
    require_one_row_per_session "$records" "$@"
    reconcile_pinned_sessions "$attempt" "$records" "$@"
  fi
  require_each_row_names_one_seat "$attempt" "$records"

  local staging manifest
  staging="$(mktemp -d)"
  # shellcheck disable=SC2064  # expand now: $staging is what must be removed.
  trap "rm -rf '$staging'" EXIT
  manifest="$(derive_into "$attempt" "$records" "$staging/inputs")"

  mkdir -p "$out"
  local input name
  for input in "$staging/inputs"/*.json; do
    name=$(basename "$input")
    [[ -e "$out/$name" ]] \
      && die "assemble: $out/$name already exists; assembling an attempt twice into one directory is how one interval is banked twice"
  done
  for input in "$staging/inputs"/*.json; do
    mv "$input" "$out/$(basename "$input")"
  done
  rm -rf "$staging"
  trap - EXIT

  note "assembled $(jq -r '.inputs | length' <<<"$manifest") ledger input(s) into $out"
  jq --arg out "$out" '.inputs = (.inputs | map($out + "/" + (split("/") | last)))' <<<"$manifest"
}

cmd_assemble_one_seat() {
  local attempt=$1 records=$2 session=$3 out=$4
  assemble_preflight "$attempt" "$records"
  [[ $session =~ $UUID_V7_RE ]] \
    || die "assemble: session id '$session' is not a UUIDv7; mint one with orrery-invite"
  # Retained from #387 and checked before anything else about this seat: the
  # client may have recorded several sessions into one JSONL, and the invite
  # session id selects exactly one row.
  require_one_row_per_session "$records" "$session"
  reconcile_pinned_sessions "$attempt" "$records" "$session"
  require_each_row_names_one_seat "$attempt" "$records"

  # #579's authenticity stage ran here, inline, for the one row this form used
  # to handle. It now runs for **every** row in `assemble_preflight`
  # (`require_each_row_names_one_seat`), because with a cohort the node-to-seat
  # question is per seat; the signature verification it guarded is performed per
  # seat against the bound node inside the derivation below, and once more by
  # `p4-ledger.sh` before the row banks.
  local staging
  staging="$(mktemp -d)"
  # shellcheck disable=SC2064  # expand now: $staging is what must be removed.
  trap "rm -rf '$staging'" EXIT
  derive_into "$attempt" "$records" "$staging/inputs" >/dev/null

  local human bot
  human=$(ls "$staging/inputs" | grep "^contribution-human-.*-$session\.json$" || true)
  [[ -n $human ]] \
    || die "assemble: the attempt derived no human contribution for session $session"
  bot=$(ls "$staging/inputs" | grep '^contribution-bot\.json$' || true)

  mkdir -p "$(dirname "$out")"
  mv "$staging/inputs/$human" "$out"
  local bot_out=''
  if [[ -n $bot ]]; then
    # The bot cohort is its own ledger input, not a number folded into somebody's
    # hour. Emitting it beside the seat keeps "one input per actor" true of the
    # one-seat form too; the operator appends both.
    bot_out="${out%.json}-bot-contribution.json"
    mv "$staging/inputs/$bot" "$bot_out"
  fi
  rm -rf "$staging"
  trap - EXIT

  note "assembled $out: session $session, slot $(jq -r '.binding.slot' "$out"), $(jq -r '.player_hours' "$out") player-hours${bot_out:+; bot contribution $bot_out}"
}

# ── Self-test ────────────────────────────────────────────────────────────────
#
# Named cases, because a mutation check reports a name. The properties are
# exactly-once attribution and the `(attempt, slot, session_id, node)` binding —
# not the shape of an assembled row, which a report can satisfy while charging
# one interval to two people.
ST_PASSED=0
ST_DIR=''
st_ok() { ST_PASSED=$(( ST_PASSED + 1 )); echo "$NAME: PASS $1"; }

# A signed client row. Each seat signs with its own key ($3), so a row cannot be
# moved between seats without the signature stage noticing.
st_row() {
  local session=$1 minutes=$2 secret=$3 platform=${4:-x86_64-unknown-linux-gnu}
  jq -n --arg session "$session" --argjson minutes "$minutes" --arg platform "$platform" '{
    session_id: $session,
    wall_start: "2026-08-27T12:00:00Z", wall_end: "2026-08-27T13:00:00Z",
    distinct_play_minutes: $minutes, banked_minutes: $minutes,
    platform_triple: $platform, client_rev: "self-test",
    ruleset_id: "52", ruleset_version: 16, pipeline_digest: "unavailable-client-side",
    actor: "human",
    configured_impairment_profile: {loss_pct: 3, jitter_p50_ms: 100, jitter_p99_ms: 100},
    observed_loss_pct: 3, observed_jitter_p50_ms: 100, observed_jitter_p99_ms: 100,
    afk_seconds: 0, afk_capped: false, impairment_mismatch: false
  }' | python3 "$ROOT/scripts/sign-campaign-measurement-fixture.py" --secret-byte "$secret"
}

# Directed links per seat, carrying enough packets for the three-sigma loss band
# to mean anything: 60 dropped of 2000 is the configured 3%.
st_links() {
  jq -n --argjson slots "$1" --argjson bots "$2" '
    [ $slots[] as $slot
      | (([range(0; $bots)] + ($slots | map(select(. != $slot))))[]) as $other
      | ({from_slot: $slot, to_slot: $other}, {from_slot: $other, to_slot: $slot})
      | . + {lane: "state", delivered: 1940, dropped: 60, delayed: 200, bytes: 993280} ]'
}

# $1 exteriors JSON, $2 bots, $3 host target
st_attempt() {
  local exteriors=$1 bots=${2:-4} host=${3:-x86_64-unknown-linux-gnu}
  local slots
  slots=$(jq -c 'map(.slot)' <<<"$exteriors")
  jq -n --argjson exteriors "$exteriors" --argjson bots "$bots" --arg host "$host" \
        --argjson links "$(st_links "$slots" "$bots")" '{
    attempt_id: "018f9000-0000-7000-8000-00000000e001",
    identity: {
      seed: 5,
      impairment: { loss: 0.03, jitter_ticks: 6, jitter_rate: 0.1, retransmit_ticks: 3 },
      target: $host, commit: "0000000000000000000000000000000000000000"
    },
    started_at_unix_secs: 1750000000,
    bots: $bots, peers: $bots, seconds: 3600, ticks: 108000,
    valid_attempt_seconds: 3600, completed: true,
    player_hours: 6.0,
    witnessing: true, total_false_positives: 0, observation_coverage: 1.0,
    deferral_ledger_balances: true, total_gaps: 164022, total_shed: 162,
    exteriors: $exteriors, per_link_impairment: $links
  }'
}

st_exterior() {
  jq -n --argjson slot "$1" --arg session "$2" --arg node "$3" --argjson minutes "$4" \
        --arg close "${5:-goodbye}" '{
    slot: $slot, session_id: $session, node: $node,
    connected_ticks: ($minutes * 60 * 30),
    frames: {uplink: 100000, downlink: 400000, downlink_dropped: 0},
    close: $close
  }'
}

# Runs the real script. $1 case name; the rest is argv after `assemble`.
st_assemble() {
  local case=$1; shift
  rm -rf "${ST_DIR:?}/case"
  mkdir -p "$ST_DIR/case"
  "$0" assemble "$@" >"$ST_DIR/case/out" 2>"$ST_DIR/case/err"
}

st_refuses() {
  local case=$1 fragment=$2; shift 2
  local out=$1  # the output directory or file the refusal must not have written
  if st_assemble "$case" "${@:2}"; then
    die "self-test [$case]: this must not assemble, and it did"
  fi
  grep -q "$fragment" "$ST_DIR/case/err" \
    || die "self-test [$case]: refused for the wrong reason; wanted '$fragment', got '$(tr '\n' ' ' <"$ST_DIR/case/err")'"
  if [[ -d $out ]]; then
    [[ -z $(ls -A "$out") ]] \
      || die "self-test [$case]: a refusal left ledger inputs behind in $out"
  else
    [[ ! -e $out ]] || die "self-test [$case]: a refusal wrote $out"
  fi
  st_ok "$case"
}

self_test() {
  need jq
  need openssl
  need python3
  need git
  # The digest arithmetic must be `p4-ledger.sh`'s, tree for tree, in order.
  local ours theirs
  ours=$(printf '%s\n' "${PIPELINE_TREES[@]}")
  theirs=$(sed -n '/^readonly PIPELINE_TREES=($/,/^)$/p' "$ROOT/scripts/p4-ledger.sh" \
    | sed -n 's/^  \([a-z0-9_/-]*\)$/\1/p')
  [[ -n $theirs ]] || die 'self-test: cannot read PIPELINE_TREES out of p4-ledger.sh'
  diff <(echo "$ours") <(echo "$theirs") >/dev/null \
    || die 'self-test: the pipeline tree list drifted from p4-ledger.sh; the stamped digest would fail its cross-check'
  st_ok the_pipeline_digest_is_still_the_ledgers_arithmetic

  # Structural half, in the house style: the haystack is the script body below
  # the shebang comment, because every pattern also appears in the line that
  # looks for it.
  local body
  body="$(sed -n '/^readonly NAME=/,$p' "$0" | grep -v '^[[:space:]]*#')"
  has() { grep -Fq -- "$1" <<<"$body"; }
  has 'p4-attempt-accounting.py' \
    || die 'self-test: assembly no longer derives through the accounting contract; the denominator would be implemented twice'
  has 'reconcile_pinned_sessions' \
    || die 'self-test: the two-copy reconciliation between the host'\''s seated ids and the operator'\''s pinned ids is gone'
  has 'require_one_row_per_session' \
    || die 'self-test: nothing refuses two client rows for one session; one interval could be assembled twice'
  has 'mktemp -d' \
    || die 'self-test: the derivation no longer stages into a private directory; a refusal could leave bankable-looking inputs behind'
  has 'require_each_row_names_one_seat' \
    || die 'self-test: #579'\''s exactly-once node stage is gone; a row could bind into an ambiguous seat map'
  has 'recorded_session_ids' \
    || die 'self-test: the client'\''s copy of the session ids is no longer compared with the operator'\''s; on a host that seats no ids nothing would check the pinned list'

  local dir
  dir="$(mktemp -d)"
  ST_DIR="$dir"
  # shellcheck disable=SC2064  # expand now: $dir is what must be removed.
  trap "rm -rf '$dir'" EXIT
  export P4_PIPELINE_ID=selftestpipeline

  local sid_a=018f9000-0000-7000-8000-0000000000e1
  local sid_b=018f9000-0000-7000-8000-0000000000e2
  local sid_c=018f9000-0000-7000-8000-0000000000e3

  st_row "$sid_a" 50 21 > "$dir/row-a.json"
  st_row "$sid_b" 42 22 x86_64-pc-windows-msvc > "$dir/row-b.json"
  local node_a node_b
  node_a=$(jq -r .measurement_node "$dir/row-a.json")
  node_b=$(jq -r .measurement_node "$dir/row-b.json")
  cat "$dir/row-a.json" "$dir/row-b.json" > "$dir/records.jsonl"

  local cohort
  cohort=$(st_attempt "[$(st_exterior 4 "$sid_a" "$node_a" 55), $(st_exterior 5 "$sid_b" "$node_b" 55)]")
  echo "$cohort" > "$dir/attempt.json"

  # ── One input per actor contribution ──────────────────────────────────────
  st_assemble ok "$dir/attempt.json" "$dir/records.jsonl" "$dir/inputs" "$sid_a" "$sid_b" \
    || die "self-test [a_cohort_attempt_assembles_one_input_per_actor]: an honest attempt refused ('$(tr '\n' ' ' <"$dir/case/err")')"
  [[ $(ls "$dir/inputs" | wc -l) == 3 ]] \
    || die "self-test [a_cohort_attempt_assembles_one_input_per_actor]: expected three inputs, got $(ls "$dir/inputs" | tr '\n' ' ')"
  st_ok a_cohort_attempt_assembles_one_input_per_actor

  # 4.000 bot + 50/60 + 42/60 = 5.5333, over three inputs. The pre-#576 defect
  # banked the attempt's own 6.0 on each human row instead.
  local total
  total=$(jq -s 'map(.player_hours) | add | (. * 10000 | round) / 10000' "$dir/inputs"/*.json)
  [[ $total == 5.5333 ]] \
    || die "self-test [each_human_row_banks_its_own_interval_not_the_cohort_total]: assembled $total, not 5.5333"
  jq -es 'map(select(.identity.actor == "human"))
          | all(.player_hours == (.session.banked_minutes / 60)) and all(.player_hours != 6.0)' \
    "$dir/inputs"/*.json >/dev/null \
    || die 'self-test [each_human_row_banks_its_own_interval_not_the_cohort_total]: a human row did not bank its own signed interval'
  st_ok each_human_row_banks_its_own_interval_not_the_cohort_total

  jq -es --arg a "$sid_a" --arg b "$sid_b" --arg na "$node_a" --arg nb "$node_b" '
    map(select(.identity.actor == "human")) as $h
    | ($h | map(.binding.slot) | sort == [4, 5])
    and ($h | map(.binding.session_id) | sort == ([$a, $b] | sort))
    and ($h | all(.binding.attempt_id == .identity.attempt_id))
    and ($h | all(.binding.slot == .identity.slot))
    and (($h | map(select(.binding.slot == 4)) | .[0].binding.node) == $na)
    and (($h | map(select(.binding.slot == 5)) | .[0].binding.node) == $nb)
  ' "$dir/inputs"/*.json >/dev/null \
    || die 'self-test [each_row_binds_to_its_matching_exterior]: a row is not bound to the seat that admitted it'
  st_ok each_row_binds_to_its_matching_exterior

  # The signed human platform, not the host's: a Windows client on a Linux host
  # assembles, and the host's triple survives on every row.
  jq -es '
    (map(select(.identity.actor == "human" and .identity.target == "x86_64-pc-windows-msvc")) | length == 1)
    and all(.attempt.host_target == "x86_64-unknown-linux-gnu")
    and (map(select(.identity.actor == "bot")) | all(.identity.target == "x86_64-unknown-linux-gnu"))
    and all(.session == null or .session.platform_triple == .identity.target)
  ' "$dir/inputs"/*.json >/dev/null \
    || die 'self-test [a_human_row_carries_its_own_signed_platform]: the mixed-platform rule was not applied'
  st_ok a_human_row_carries_its_own_signed_platform

  # The whole point: the assembled inputs are exactly what the ledger banks, and
  # they go through the real `append` rather than a restatement of its rules.
  local input
  for input in "$dir/inputs"/*.json; do
    P4_LEDGER_FILE="$dir/hours.jsonl" "$ROOT/scripts/p4-ledger.sh" append "$input" >/dev/null 2>&1 \
      || die "self-test [assembled_inputs_bank_through_the_real_ledger]: p4-ledger.sh refused $(basename "$input")"
  done
  [[ $(awk 'END { print NR }' "$dir/hours.jsonl") == 3 ]] \
    || die 'self-test [assembled_inputs_bank_through_the_real_ledger]: three inputs did not bank three lines'
  jq -es --arg attempt 018f9000-0000-7000-8000-00000000e001 '
    (map(.run_key) | unique | length == 3)
    and (map(.measurement_key) | unique | length == 3)
    and all(.attempt_id == $attempt)
    and ((map(.player_hours) | add | . * 10000 | round) == 55333)
  ' "$dir/hours.jsonl" >/dev/null \
    || die 'self-test [assembled_inputs_bank_through_the_real_ledger]: the banked lines do not reconcile with the attempt'
  st_ok assembled_inputs_bank_through_the_real_ledger

  # ── Exactly-once attribution ──────────────────────────────────────────────
  #
  # One human's interval, uploaded twice. This is the mutation target for
  # `require_one_row_per_session` and for the contract's own duplicate check.
  cat "$dir/row-a.json" "$dir/row-a.json" "$dir/row-b.json" > "$dir/twice.jsonl"
  st_refuses one_interval_may_not_be_attributed_twice 'exactly one client row' \
    "$dir/twice" "$dir/attempt.json" "$dir/twice.jsonl" "$dir/twice" "$sid_a" "$sid_b"

  # The same, from the host's side: one invite seated at two seats. The two
  # seats carry *different* nodes, so #579's exactly-once node stage passes and
  # the clause under test is the one that has to fire.
  st_attempt "[$(st_exterior 4 "$sid_a" "$node_a" 55), $(st_exterior 5 "$sid_a" "$node_b" 55)]" \
    > "$dir/two-seats.json"
  st_refuses one_session_may_not_occupy_two_seats 'seated at two slots' \
    "$dir/two-seats-out" "$dir/two-seats.json" "$dir/records.jsonl" "$dir/two-seats-out"

  # #579's clause, retained and made precise (#1028): the seat a row names must
  # be named exactly once, and the seat is `(node, session_id)`. Here one node
  # holds two seats *under one session id*, so the row cannot say which of them
  # it played — the ambiguity #579 named, in the only shape that is still one.
  # The records file is that node's row alone, so nothing else can refuse first.
  st_attempt "[$(st_exterior 4 "$sid_a" "$node_a" 55), $(st_exterior 5 "$sid_a" "$node_a" 55)]" \
    > "$dir/two-seats-one-id.json"
  st_refuses the_host_must_name_each_rows_seat_exactly_once 'exactly once' \
    "$dir/two-seats-one-id-out" "$dir/two-seats-one-id.json" "$dir/row-a.json" \
    "$dir/two-seats-one-id-out"

  # The same key at two seats with **no** id on either to tell them apart. A
  # host that seats no invite ids has only the node to offer, so a node it
  # admitted twice binds this row to nobody in particular and is still refused.
  jq 'del(.exteriors[].session_id)' \
    <(st_attempt "[$(st_exterior 4 "$sid_a" "$node_a" 55), $(st_exterior 5 "$sid_b" "$node_a" 55)]") \
    > "$dir/two-seats-no-id.json"
  st_refuses a_node_seated_twice_with_no_session_id_is_still_ambiguous 'exactly once' \
    "$dir/two-seats-no-id-out" "$dir/two-seats-no-id.json" "$dir/row-a.json" \
    "$dir/two-seats-no-id-out"

  # And the case the precise rule exists to admit (#1028): one volunteer closed
  # their client and launched it again inside the attempt. Same install, so the
  # same persistent key, and the host correctly admitted it at a second seat
  # against a second pre-minted invite id. Two signed intervals, banked
  # separately, and the projection onto `node` collides without ambiguity —
  # each seat carries the session id its row names. Both legs land on slot 4,
  # which is what the ledger's seat clash has to tolerate too.
  st_row "$sid_c" 4 21 > "$dir/row-rejoin.json"
  cat "$dir/row-a.json" "$dir/row-b.json" "$dir/row-rejoin.json" > "$dir/rejoin.jsonl"
  st_attempt "[$(st_exterior 4 "$sid_a" "$node_a" 50), \
               $(st_exterior 5 "$sid_b" "$node_b" 55), \
               $(st_exterior 4 "$sid_c" "$node_a" 5)]" > "$dir/rejoin.json"
  st_assemble rejoin "$dir/rejoin.json" "$dir/rejoin.jsonl" "$dir/rejoin-out" \
    "$sid_a" "$sid_b" "$sid_c" \
    || die "self-test [a_rejoining_player_assembles_as_two_signed_intervals]: an honest rejoin refused ('$(tr '\n' ' ' <"$dir/case/err")')"
  jq -es --arg a "$sid_a" --arg c "$sid_c" --arg na "$node_a" '
    (length == 4)
    and (map(select(.identity.actor == "human")) | length == 3)
    and (map(select(.binding.node == $na)) | length == 2)
    and (map(select(.binding.node == $na)) | all(.binding.slot == 4))
    and (map(select(.binding.session_id == $a)) | .[0].player_hours * 60 | round == 50)
    and (map(select(.binding.session_id == $c)) | .[0].player_hours * 60 | round == 4)
    and ((map(.player_hours) | add | . * 10000 | round) == 56000)
  ' "$dir/rejoin-out"/*.json >/dev/null \
    || die 'self-test [a_rejoining_player_assembles_as_two_signed_intervals]: the two legs did not assemble as separate signed intervals on one slot'
  # Through the real ledger, because the seat clash there keys on the slot too:
  # a fix that assembles and then dies at `append` is not a fix.
  for input in "$dir/rejoin-out"/*.json; do
    P4_LEDGER_FILE="$dir/rejoin-hours.jsonl" "$ROOT/scripts/p4-ledger.sh" append "$input" \
      >/dev/null 2>&1 \
      || die "self-test [a_rejoining_player_assembles_as_two_signed_intervals]: p4-ledger.sh refused $(basename "$input")"
  done
  [[ $(awk 'END { print NR }' "$dir/rejoin-hours.jsonl") == 4 ]] \
    || die 'self-test [a_rejoining_player_assembles_as_two_signed_intervals]: four inputs did not bank four lines'
  st_ok a_rejoining_player_assembles_as_two_signed_intervals

  # ── The per-seat two-copy reconciliation (#476) ───────────────────────────
  #
  # The host's record of the id it pinned at that seat, against the id the client
  # put in its row. With several humans this must be per seat: an attempt that
  # seated A and B while the client rows name A and C has the right *count* and
  # the wrong people.
  # Signed with seat B's own key, so the row lands on seat 5 by node and the two
  # copies of that seat's session id then disagree. This is #476's check in its
  # exact per-seat form: the host pinned B there, the client's row says C.
  st_row "$sid_c" 42 22 x86_64-pc-windows-msvc > "$dir/row-c.json"
  cat "$dir/row-a.json" "$dir/row-c.json" > "$dir/other-session.jsonl"
  st_refuses the_hosts_and_the_clients_session_ids_must_agree_per_seat \
    "the host's copy and the client's copy of the session id disagree" \
    "$dir/other-out" "$dir/attempt.json" "$dir/other-session.jsonl" "$dir/other-out"

  # Signed with a key the host admitted at no seat: the row belongs to no seat
  # of this attempt at all, by session id or by node.
  st_row "$sid_c" 42 23 x86_64-pc-windows-msvc > "$dir/row-stranger.json"
  cat "$dir/row-a.json" "$dir/row-stranger.json" > "$dir/stranger.jsonl"
  st_refuses a_client_row_the_host_never_seated_is_refused 'is not seated in attempt' \
    "$dir/stranger-out" "$dir/attempt.json" "$dir/stranger.jsonl" "$dir/stranger-out"
  # The records file really does carry a row for each pinned id, so the
  # one-row-per-session stage passes and only the reconciliation can refuse
  # this: the host seated A and B, the operator pinned A and C.
  st_refuses the_operators_pinned_ids_must_match_the_hosts 'disagree' \
    "$dir/pinned-out" "$dir/attempt.json" "$dir/other-session.jsonl" "$dir/pinned-out" \
    "$sid_a" "$sid_c"

  # A row signed by a key the host did not admit at that seat. The two rows are
  # swapped between seats, so each is internally honest and bound to the wrong
  # participant — the case a per-attempt check would pass.
  st_attempt "[$(st_exterior 4 "$sid_a" "$node_b" 55), $(st_exterior 5 "$sid_b" "$node_a" 55)]" \
    > "$dir/swapped.json"
  st_refuses a_row_bound_to_another_seats_node_is_refused 'not by the node the host admitted' \
    "$dir/swapped-out" "$dir/swapped.json" "$dir/records.jsonl" "$dir/swapped-out"

  # ── The non-constant denominator ─────────────────────────────────────────
  #
  # A human seated for ten minutes of a sixty-minute attempt banks ten minutes,
  # and may not bank fifty.
  st_attempt "[$(st_exterior 4 "$sid_a" "$node_a" 10), $(st_exterior 5 "$sid_b" "$node_b" 55)]" \
    > "$dir/short.json"
  st_refuses an_interval_may_not_exceed_its_seats_connected_span 'connected span' \
    "$dir/short-out" "$dir/short.json" "$dir/records.jsonl" "$dir/short-out"

  st_row "$sid_a" 8 21 > "$dir/row-short.json"
  cat "$dir/row-short.json" "$dir/row-b.json" > "$dir/short-records.jsonl"
  st_assemble partial "$dir/short.json" "$dir/short-records.jsonl" "$dir/partial" \
    || die "self-test [a_partial_seat_assembles_its_own_span]: an honest partial seat refused ('$(tr '\n' ' ' <"$dir/case/err")')"
  jq -es 'map(select(.identity.slot == 4)) | length == 1 and (.[0].player_hours * 60 | round) == 8' \
    "$dir/partial"/*.json >/dev/null \
    || die 'self-test [a_partial_seat_assembles_its_own_span]: an eight-minute seat did not assemble eight minutes'
  st_ok a_partial_seat_assembles_its_own_span

  # A seat that closed with its downlink backlog is not evidence of the declared
  # profile; the rest of the cohort keeps its contributions.
  st_attempt "[$(st_exterior 4 "$sid_a" "$node_a" 55 queue_overflow), $(st_exterior 5 "$sid_b" "$node_b" 55)]" \
    > "$dir/overflow.json"
  st_refuses a_queue_overflow_seat_assembles_nothing 'does not bank' \
    "$dir/overflow-out" "$dir/overflow.json" "$dir/records.jsonl" "$dir/overflow-out"

  # ── Retained refusals ────────────────────────────────────────────────────
  jq '.witnessing = false' "$dir/attempt.json" > "$dir/unwitnessed.json"
  st_refuses an_unwitnessed_attempt_assembles_nothing 'was not witnessed' \
    "$dir/unwitnessed-out" "$dir/unwitnessed.json" "$dir/records.jsonl" "$dir/unwitnessed-out"

  jq 'del(.exteriors)' "$dir/attempt.json" > "$dir/nobody.json"
  st_refuses an_attempt_with_no_exterior_assembles_nothing 'no external participant' \
    "$dir/nobody-out" "$dir/nobody.json" "$dir/records.jsonl" "$dir/nobody-out"

  # #409's careful forgery: rewrite every observation to configuration and clear
  # the flag, then re-derive the payload so the row is internally consistent.
  # The admitted client's signature no longer covers it, so the signature stage
  # must name and refuse it.
  st_row "$sid_a" 50 21 \
    | jq '.observed_loss_pct = 9 | .impairment_mismatch = true' \
    | python3 -c 'import json,sys
row=json.load(sys.stdin)
unsigned=dict(row)
for field in ("pipeline_digest", "measurement_payload", "measurement_signature"):
    unsigned.pop(field, None)
row["measurement_payload"]=json.dumps(unsigned,sort_keys=True,separators=(",",":")).encode().hex()
json.dump(row,sys.stdout,separators=(",",":")); print()' > "$dir/forged.json"
  cat "$dir/forged.json" "$dir/row-b.json" > "$dir/forged.jsonl"
  st_refuses a_self_consistent_forged_row_is_refused 'signature did not verify' \
    "$dir/forged-out" "$dir/attempt.json" "$dir/forged.jsonl" "$dir/forged-out"

  # A row whose flag did not fire while its numbers disagree.
  st_row "$sid_a" 50 21 | jq '.observed_loss_pct = 9' \
    | python3 -c 'import json,sys
row=json.load(sys.stdin)
unsigned=dict(row)
for field in ("pipeline_digest", "measurement_payload", "measurement_signature"):
    unsigned.pop(field, None)
row["measurement_payload"]=json.dumps(unsigned,sort_keys=True,separators=(",",":")).encode().hex()
json.dump(row,sys.stdout,separators=(",",":")); print()' > "$dir/unflagged.json"
  cat "$dir/unflagged.json" "$dir/row-b.json" > "$dir/unflagged.jsonl"
  st_refuses a_row_whose_mismatch_flag_did_not_fire_is_refused 'refusing to derive' \
    "$dir/unflagged-out" "$dir/attempt.json" "$dir/unflagged.jsonl" "$dir/unflagged-out"

  # ── Nothing is written until every row validates ─────────────────────────
  #
  # #572's first cut wrote the bot contribution before the human rows validated,
  # so a refusal left a bankable-looking input behind. Every `st_refuses` above
  # asserts the output directory is empty; this one names the failure mode, with
  # a refusal that can only fire *after* the bot contribution would have been
  # written — the second human row of a two-human attempt.
  st_row "$sid_b" 900 22 x86_64-pc-windows-msvc > "$dir/row-b-long.json"
  cat "$dir/row-a.json" "$dir/row-b-long.json" > "$dir/late-refusal.jsonl"
  st_refuses a_refusal_leaves_no_bot_contribution_behind 'connected span' \
    "$dir/late-out" "$dir/attempt.json" "$dir/late-refusal.jsonl" "$dir/late-out"

  # And a directory that already holds this attempt's inputs is refused rather
  # than merged into: assembling one attempt twice is how one interval banks
  # twice.
  local before_reassemble
  before_reassemble=$(ls "$dir/inputs" | sort)
  if st_assemble reassemble "$dir/attempt.json" "$dir/records.jsonl" "$dir/inputs" "$sid_a" "$sid_b"; then
    die 'self-test [assembling_one_attempt_twice_into_one_directory_is_refused]: it assembled anyway'
  fi
  grep -q 'already exists' "$dir/case/err" \
    || die "self-test [assembling_one_attempt_twice_into_one_directory_is_refused]: refused for the wrong reason ('$(tr '\n' ' ' <"$dir/case/err")')"
  [[ $(ls "$dir/inputs" | sort) == "$before_reassemble" ]] \
    || die 'self-test [assembling_one_attempt_twice_into_one_directory_is_refused]: the refusal changed the directory it refused to write into'
  st_ok assembling_one_attempt_twice_into_one_directory_is_refused

  # ── The one-seat form ────────────────────────────────────────────────────
  #
  # Still a documented spelling, still contract-honouring: it derives the whole
  # attempt so every cross-row refusal fires, then places the seat asked about
  # and the cohort's bot contribution beside it.
  st_attempt "[$(st_exterior 4 "$sid_a" "$node_a" 55)]" > "$dir/one.json"
  cp "$dir/row-a.json" "$dir/one-records.jsonl"
  st_assemble one-seat "$dir/one.json" "$dir/one-records.jsonl" "$sid_a" "$dir/r.json" \
    || die "self-test [the_one_seat_form_assembles_and_banks]: an honest single seat refused ('$(tr '\n' ' ' <"$dir/case/err")')"
  jq -e --arg session "$sid_a" '
    .identity.actor == "human"
    and .identity.human_session_id == $session
    and .identity.slot == 4
    and .binding.session_id == $session
    and .session.pipeline_digest == "selftestpipeline"
    and ((.player_hours * 60 | round) == 50)
  ' "$dir/r.json" >/dev/null \
    || die 'self-test [the_one_seat_form_assembles_and_banks]: the assembled seat is missing its identity, binding or interval'
  [[ -r "$dir/r-bot-contribution.json" ]] \
    || die 'self-test [the_one_seat_form_assembles_and_banks]: the bot contribution was dropped rather than emitted'
  P4_LEDGER_FILE="$dir/one-hours.jsonl" "$ROOT/scripts/p4-ledger.sh" append "$dir/r.json" >/dev/null 2>&1 \
    || die 'self-test [the_one_seat_form_assembles_and_banks]: p4-ledger.sh refused the assembled seat'
  P4_LEDGER_FILE="$dir/one-hours.jsonl" "$ROOT/scripts/p4-ledger.sh" append "$dir/r-bot-contribution.json" >/dev/null 2>&1 \
    || die 'self-test [the_one_seat_form_assembles_and_banks]: p4-ledger.sh refused the bot contribution'
  st_ok the_one_seat_form_assembles_and_banks

  # Retained verbatim from #387, and depended on by `scripts/admission.py`: a
  # records file with no row for the pinned session refuses.
  st_refuses the_one_seat_form_refuses_a_records_file_with_no_matching_row \
    'exactly one client row' "$dir/r2.json" \
    "$dir/one.json" "$dir/row-b.json" "$sid_a" "$dir/r2.json"

  # ── The shape the host actually emits (#579) ─────────────────────────────
  #
  # `SwarmReport.external` is a `Vec<ExteriorReport>` ordered by swarm slot, and
  # `ExteriorReport` carries `index`, `node`, `connected_ticks`, the frame
  # counters and the close flags — and **no `session_id`**. So a real attempt
  # report cannot supply the host half of #476's reconciliation by invite id.
  # What it does supply is the QUIC-authenticated node it admitted at each seat,
  # which the client signs into its own row, so the seat identity is a signed
  # value either way and the operator's pinned ids are checked against it. This
  # fixture is that path end to end: host-shaped seats, no seated ids, two
  # humans, assembled and banked.
  local host_shaped
  host_shaped=$(jq -n --arg na "$node_a" --arg nb "$node_b" \
    --argjson links "$(st_links '[4,5]' 4)" '{
    attempt_id: "018f9000-0000-7000-8000-00000000e001",
    identity: {
      seed: 5,
      impairment: { loss: 0.03, jitter_ticks: 6, jitter_rate: 0.1, retransmit_ticks: 3 },
      target: "x86_64-unknown-linux-gnu",
      commit: "0000000000000000000000000000000000000000"
    },
    started_at_unix_secs: 1750000000,
    bots: 4, peers: 4, seconds: 3600, ticks: 108000,
    valid_attempt_seconds: 3600, completed: true, player_hours: 6.0,
    witnessing: true, total_false_positives: 0, observation_coverage: 1.0,
    deferral_ledger_balances: true, total_gaps: 164022, total_shed: 162,
    external: [
      { index: 4, node: $na, said_goodbye: true, connected: false,
        connected_ticks: 99000, uplink_frames: 100000, uplink_delivered: 97000,
        uplink_dropped: 3000, downlink_frames: 400000, downlink_dropped: 0,
        witness_anchored: false },
      { index: 5, node: $nb, said_goodbye: false, connected: true,
        connected_ticks: 99000, uplink_frames: 100000, uplink_delivered: 97000,
        uplink_dropped: 3000, downlink_frames: 400000, downlink_dropped: 0,
        witness_anchored: false }
    ],
    per_link_impairment: $links
  }')
  echo "$host_shaped" > "$dir/host-shaped.json"
  st_assemble host-shaped "$dir/host-shaped.json" "$dir/records.jsonl" "$dir/host-inputs" \
    "$sid_a" "$sid_b" \
    || die "self-test [the_hosts_own_exterior_array_assembles]: the shape gates/p1-swarm emits refused ('$(tr '\n' ' ' <"$dir/case/err")')"
  jq -es --arg a "$sid_a" --arg b "$sid_b" --arg na "$node_a" --arg nb "$node_b" '
    (length == 3)
    and (map(select(.identity.actor == "human")) | length == 2)
    and (map(select(.binding.slot == 4)) | .[0].binding.node == $na)
    and (map(select(.binding.slot == 5)) | .[0].binding.node == $nb)
    and (map(select(.binding.session_id == $a)) | length == 1)
    and (map(select(.binding.session_id == $b)) | length == 1)
    and ((map(.player_hours) | add | . * 10000 | round) == 55333)
  ' "$dir/host-inputs"/*.json >/dev/null \
    || die 'self-test [the_hosts_own_exterior_array_assembles]: the seats were not bound by the node the host admitted'
  # `connected: true` at report time means that seat closed *with* the attempt,
  # which is a bankable close and not a disconnect.
  jq -es 'map(select(.binding.slot == 5)) | .[0].binding.close == "attempt_end"' \
    "$dir/host-inputs"/*.json >/dev/null \
    || die 'self-test [the_hosts_own_exterior_array_assembles]: a seat still connected at report time was read as a disconnect'
  for input in "$dir/host-inputs"/*.json; do
    P4_LEDGER_FILE="$dir/host-hours.jsonl" "$ROOT/scripts/p4-ledger.sh" append "$input" >/dev/null 2>&1 \
      || die "self-test [the_hosts_own_exterior_array_assembles]: p4-ledger.sh refused $(basename "$input")"
  done
  st_ok the_hosts_own_exterior_array_assembles

  # The host's own per-seat `connected_ticks` is the ceiling, and it is real now
  # rather than contract-shaped: 99,000 ticks at 30 tps is 55 minutes, so a row
  # claiming more than that is refused against a number the host measured.
  st_row "$sid_b" 900 22 x86_64-pc-windows-msvc > "$dir/row-b-huge.json"
  cat "$dir/row-a.json" "$dir/row-b-huge.json" > "$dir/host-over.jsonl"
  st_refuses the_hosts_own_connected_ticks_bound_the_interval 'connected span' \
    "$dir/host-over-out" "$dir/host-shaped.json" "$dir/host-over.jsonl" "$dir/host-over-out"

  # ── The two clocks disagreeing, which is what a real host does (#971) ────
  #
  # The fixture above cannot catch #971 and never could: its `connected_ticks`
  # and its client rows were written from the same nominal 30 tps, so the two
  # clocks agree *by construction*. A real host's do not. `gates/p1-swarm`'s
  # metronome sleeps out the remainder of a tick and accumulates no deadline, so
  # an overrun is lost for good and the host runs at or below its nominal rate —
  # 55.3 to 59.8 Hz measured against 60 Hz nominal. Seven consecutive honest
  # attempts with the shipped client were refused for banking 60.02 s against a
  # seat the accounting scaled to 59.77 s.
  #
  # So this case builds the disagreement instead of assuming it away: both seats
  # were connected for a real 55.02 wall minutes, and a host lagging ~1.1% ticked
  # only 98,000 times over it — 54.444 min at the nominal rate. The rows bank 55
  # minutes each, which the wall bracket contains and the tick count does not.
  local lagging
  lagging=$(jq '
    .external = (.external | map(
      . + { connected_ticks: 98000,
            connected_since_unix_millis: 1750000000000,
            connected_until_unix_millis: 1750003301200 }))' "$dir/host-shaped.json")
  echo "$lagging" > "$dir/host-lagging.json"
  st_row "$sid_a" 55 21 > "$dir/row-a-55.json"
  st_row "$sid_b" 55 22 x86_64-pc-windows-msvc > "$dir/row-b-55.json"
  cat "$dir/row-a-55.json" "$dir/row-b-55.json" > "$dir/lagging.jsonl"
  st_assemble lagging "$dir/host-lagging.json" "$dir/lagging.jsonl" "$dir/lagging-out" \
    "$sid_a" "$sid_b" \
    || die "self-test [a_lagging_hosts_wall_bracket_banks_what_its_tick_count_would_refuse]: refused an interval the host's own wall bracket contains ('$(tr '\n' ' ' <"$dir/case/err")')"
  jq -es '
    (length == 3)
    and (map(select(.identity.actor == "human")) | length == 2)
    and (map(select(.identity.actor == "human")
             | .binding.connected_minutes * 1000 | round) | unique == [55020])
  ' "$dir/lagging-out"/*.json >/dev/null \
    || die 'self-test [a_lagging_hosts_wall_bracket_banks_what_its_tick_count_would_refuse]: the span was not taken from the host wall bracket'
  for input in "$dir/lagging-out"/*.json; do
    P4_LEDGER_FILE="$dir/lagging-hours.jsonl" "$ROOT/scripts/p4-ledger.sh" append "$input" >/dev/null 2>&1 \
      || die "self-test [a_lagging_hosts_wall_bracket_banks_what_its_tick_count_would_refuse]: p4-ledger.sh refused $(basename "$input")"
  done
  st_ok a_lagging_hosts_wall_bracket_banks_what_its_tick_count_would_refuse

  # The control, and the proof that the bracket is what carried the case above
  # rather than a widened tolerance: strip the stamps off the same report and
  # the same rows, and the tick basis refuses it. That is the conservative
  # direction, so a report without a bracket is never the easier one to bank.
  jq 'del(.external[].connected_since_unix_millis, .external[].connected_until_unix_millis)' \
    "$dir/host-lagging.json" > "$dir/host-lagging-bare.json"
  st_refuses the_tick_count_alone_still_refuses_the_same_lagging_attempt 'connected span' \
    "$dir/lagging-bare-out" "$dir/host-lagging-bare.json" "$dir/lagging.jsonl" \
    "$dir/lagging-bare-out"

  # Half a bracket is not a bracket.
  jq 'del(.external[].connected_until_unix_millis)' "$dir/host-lagging.json" \
    > "$dir/host-half-bracket.json"
  st_refuses one_end_of_a_connected_span_is_not_a_bracket 'one end of its connected span' \
    "$dir/half-bracket-out" "$dir/host-half-bracket.json" "$dir/lagging.jsonl" \
    "$dir/half-bracket-out"

  # A bracket that runs backwards is a broken clock, not a long seat.
  jq '.external = (.external | map(.connected_until_unix_millis = 1749999999000))' \
    "$dir/host-lagging.json" > "$dir/host-backwards.json"
  st_refuses a_connected_span_cannot_run_backwards 'cannot run backwards' \
    "$dir/backwards-out" "$dir/host-backwards.json" "$dir/lagging.jsonl" \
    "$dir/backwards-out"

  # A report that seated nobody, in the array spelling the host emits.
  jq '.external = []' "$dir/host-shaped.json" > "$dir/host-empty.json"
  st_refuses an_attempt_whose_seat_list_is_empty_assembles_nothing 'seated no external participant' \
    "$dir/host-empty-out" "$dir/host-empty.json" "$dir/records.jsonl" "$dir/host-empty-out"

  # #579's type assertion, retained: the pre-#579 singular object is not this
  # host's output, and must not be read as a one-element cohort by accident.
  jq '.external = .external[0]' "$dir/host-shaped.json" > "$dir/host-object.json"
  st_refuses a_pre_579_singular_exterior_object_assembles_nothing \
    'carries no external participant' \
    "$dir/host-object-out" "$dir/host-object.json" "$dir/records.jsonl" "$dir/host-object-out"

  rm -rf "$dir"
  trap - EXIT
  echo "$NAME: self-test passed ($ST_PASSED fixtures)"
}

case ${1:-} in
  assemble) shift; cmd_assemble "$@" ;;
  --self-test) self_test ;;
  *)
    sed -n '/^# usage:/,/^set -euo/p' "$0" | sed '$d' >&2
    die "unknown command '${1:-<none>}'"
    ;;
esac
