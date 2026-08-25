#!/usr/bin/env bash
# D32's enforcement ramp, proved rather than asserted (issue #222).
#
# Three claims, and the middle one is the one that rots quietly:
#
#   1. Enforcing acts. A synthetic offender — K cryptographically valid
#      co-signatures from announced witnesses, deliberately not the subset the
#      durable draw key names — is refused, with the matching cause.
#   2. Shadow observes. The same offender, put to a gateway launched in shadow,
#      produces `would_act = true` on the stable `orrery::ramp::shadow` target
#      carrying that same verdict label.
#   3. Shadow does not act. That intent's ack comes back committed, its
#      `attest/` row carries `enforced: false`, and across the whole shadow run
#      nothing was refused at all.
#
# A gate that proved only (1) and (2) is the one #222 exists to prevent: a
# shadow arm that had quietly started refusing would pass it. So would a
# control that had quietly stopped observing, if the gate read only outcomes —
# which is why (3) asserts a *pair*, refusals zero and would-have-refused
# non-zero, that no single counter can express and that "off" cannot satisfy.
#
# A fourth arm is reversibility, and it is the one a second process cannot
# prove: the enforcing gateway is demoted *while it runs*, the offender it
# refused a moment ago commits inside D32 clause (c)'s 2 s bound, and promoting
# it back refuses again.
#
# ── Why a sibling script and not a fourth arm of the dupe gauntlet ──────────
#
# #222 puts that fork first and asks for the reason in the PR. It is a
# mechanical one. `p5-dupe-gauntlet-gate.sh`'s structural self-test indexes
# `$P5_DUPE_BIN` invocations *by occurrence* — `runs 1 gateway`, `runs 2
# --replay` — so inserting a second gateway launch between them renumbers every
# clause and forces a rewrite of the arms #222 lists as explicitly out of
# scope. The two gates also differ in shape: that one is one gateway and one
# harness pass, this one is two concurrently running gateways with opposite
# postures plus a posture change applied mid-run. The *harness* is shared —
# this gate runs the same `gates/p5-dupe-gauntlet` binary, whose `ramp` subcommand is
# additive — so nothing is duplicated but the process supervision, which is the
# part that genuinely differs.
#
# ── The observation surface ────────────────────────────────────────────────
#
# #217 left three. The in-process `CountingShadowObserver` is the cheapest and
# proves the least: it would live in the *harness*, and a gate that builds its
# own validator has proved something about a validator rather than about a
# gateway anybody could deploy. This gate reads the two out-of-process ones —
# the gateway's `tracing` log, which is what an operator actually has and needs
# no wiring, and the durable `attest/` row, which a shadow arm that had started
# acting could not fake.
#
# This gate writes fixed ledger ids and must be pointed at a fresh throwaway
# cluster — never the shared development instance.
set -euo pipefail

readonly NAME=ramp-shadow-gate
die() { echo "$NAME: $*" >&2; exit 2; }
note() { echo "$NAME: $*" >&2; }

if [[ ${1:-} == --self-test ]]; then
  # Search only the executable body below `:`, with comments removed. Every
  # literal also appears in the checks themselves, so searching the whole file
  # would make each clause match its own source and pass vacuously.
  body="$(sed -n '/^: /,$p' "$0" | grep -v '^[[:space:]]*#')"
  launch() { # occurrence
    awk -v want="$1" '
      BEGIN { bin = "\"$P5_DUPE_BIN\" " "\\" }
      $0 == bin { n++; inside = (n == want); if (inside) { print; next } }
      inside { print; if ($0 !~ /\\$/) inside = 0 }
    ' <<<"$body"
  }
  runs() { # occurrence literal
    local invocation
    invocation=$(launch "$1")
    [[ -n $invocation ]] || die "self-test: P5_DUPE_BIN invocation $1 is absent"
    awk -v want="$2" '
      {
        line = $0
        sub(/^[[:space:]]+/, "", line)
        sub(/[[:space:]]+\\$/, "", line)
        if (line == want) found = 1
      }
      END { exit !found }
    ' <<<"$invocation"
  }

  # The two gateways. Two OS processes with *opposite* postures is the whole
  # comparison: one process asked to answer both questions would be a mode
  # switch, not a deployment.
  runs 1 '--enforcement required' \
    || die 'self-test: no separate enforcing gateway process is launched'
  runs 2 '--enforcement shadow' \
    || die 'self-test: no separate shadow gateway process is launched'
  # The runtime lever, on the enforcing process. Without it the reversibility
  # arm degrades to restarting a process under a different flag, which proves
  # nothing about a control demoted in a running fleet.
  runs 1 '--posture-file "$out/posture"' \
    || die 'self-test: the enforcing gateway takes no runtime posture lever'

  # The harness, and the four inputs without which an arm silently measures
  # nothing rather than failing.
  runs 3 'ramp' \
    || die 'self-test: the ramp arm is not requested from the harness'
  runs 3 '--shadow-log "$out/shadow-gateway.log"' \
    || die 'self-test: the shadow observation surface is not handed to the harness'
  runs 3 '--enforcing-log "$out/enforcing-gateway.log"' \
    || die 'self-test: attributable refusal causes are not handed to the harness'
  runs 3 '--cluster-file "$ORRERY_FDB_CLUSTER_FILE"' \
    || die 'self-test: harness cannot read its durable FDB evidence'
  runs 3 '--posture-file "$out/posture"' \
    || die 'self-test: the harness cannot move the posture it measures against'
  runs 3 '--report "$out/report.json"' \
    || die 'self-test: harness writes no machine-readable arm report'

  # Both readiness handshakes are checked, and each against the posture it
  # claims. A gate that took the shadow process on faith would pass on a second
  # enforcing gateway, which is the configuration under which every arm here is
  # trivially satisfiable.
  grep -Fq "jq -r '.enforcement' \"\$out/enforcing-gateway.json\") == required" <<<"$body" \
    || die 'self-test: the enforcing gateway is not held to its posture'
  grep -Fq "jq -r '.enforcement' \"\$out/shadow-gateway.json\") == shadow" <<<"$body" \
    || die 'self-test: the shadow gateway is not held to its posture'

  grep -Fq 'harness_status -ne 0' <<<"$body" \
    || die 'self-test: harness exit status is not load-bearing'
  grep -Fq 'jq -e '\''.result == "pass"'\'' "$out/report.json"' <<<"$body" \
    || die 'self-test: report verdict is not checked before success'
  # The three named arms, read out of the report rather than re-derived here. A
  # `result == "pass"` alone would survive a harness that stopped computing one
  # of them, because a conjunction of two booleans is still a boolean.
  grep -Fq 'jq -e '\''.arms.enforcing_acts.passed == true'\'' "$out/report.json"' <<<"$body" \
    || die 'self-test: the report is not held to the enforcing-acts arm'
  grep -Fq 'jq -e '\''.arms.shadow_observes.passed == true'\'' "$out/report.json"' <<<"$body" \
    || die 'self-test: the report is not held to the shadow-observes arm'
  grep -Fq 'jq -e '\''.arms.shadow_does_not_act.passed == true'\'' "$out/report.json"' <<<"$body" \
    || die 'self-test: the report is not held to the shadow-does-not-act arm'
  grep -Fq 'jq -e '\''.arms.reversibility.passed == true'\'' "$out/report.json"' <<<"$body" \
    || die 'self-test: the report is not held to the reversibility arm'
  # `gate-status.sh` reads three field paths out of this report to render the
  # ramp row. A jq path that no longer exists returns **null**, not an error, so
  # renaming a report field breaks the operator's view silently and every
  # self-test still passes. #301 moved the shadow diagnostics under a
  # `diagnostics` object for exactly the legibility reason this gate exists to
  # serve; that move would have left `shadow_verdict: null` on every run.
  #
  # So assert the two ends agree: every `.arms.shadow_observes...` path
  # gate-status reads must name a key the harness actually emits.
  repo_root=$(cd "$(dirname "$0")/.." && pwd -P)
  ramp_reader=$(sed -n 's/.*\(\.arms\.shadow_observes[A-Za-z_.]*\).*/\1/p' \
    "$repo_root/scripts/gate-status.sh" | sort -u)
  [[ -n $ramp_reader ]] \
    || die 'self-test: gate-status.sh reads no shadow_observes path; the ramp row cannot be rendered'
  while read -r path; do
    [[ -n $path ]] || continue
    leaf=${path##*.}
    grep -Fq "\"$leaf\"" "$repo_root/gates/p5-dupe-gauntlet/src/main.rs" \
      || die "self-test: gate-status.sh reads $path but the harness emits no '$leaf' key; \
a renamed report field returns null rather than failing, so the ramp row would go blank"
  done <<<"$ramp_reader"

  grep -Fq 'touch "$out/PASSED"' <<<"$body" \
    || die 'self-test: no final success artifact exists'
  echo 'self-test: two opposed gateways, runtime posture lever, ramp arm, both readiness postures, all four report arms and a final verdict present'
  exit 0
fi

: "${ORRERY_FDB_CLUSTER_FILE:?set ORRERY_FDB_CLUSTER_FILE to a fresh throwaway FoundationDB cluster}"
: "${P5_DUPE_BIN:?set P5_DUPE_BIN to the gates/p5-dupe-gauntlet binary}"
[[ ${RAMP_SHADOW_CLUSTER_IS_THROWAWAY:-0} == 1 ]] \
  || die 'set RAMP_SHADOW_CLUSTER_IS_THROWAWAY=1 to assert this cluster may receive fixed ramp rows'
[[ -r $ORRERY_FDB_CLUSTER_FILE ]] \
  || die "FDB cluster file is not readable: $ORRERY_FDB_CLUSTER_FILE"
[[ -x $P5_DUPE_BIN ]] || die "not an executable: $P5_DUPE_BIN"
command -v jq >/dev/null || die 'jq is not on PATH'
command -v fdbcli >/dev/null || die 'fdbcli is not on PATH'
timeout 20 fdbcli -C "$ORRERY_FDB_CLUSTER_FILE" --exec 'status minimal' 2>/dev/null \
  | grep -q 'is available' \
  || die "FoundationDB cluster is not available: $ORRERY_FDB_CLUSTER_FILE"

out=${RAMP_SHADOW_GATE_OUT:-"$(pwd)/ramp-shadow-$(date -u +%Y%m%dT%H%M%SZ)"}
[[ ! -e $out ]] || die "refusing to overwrite existing output directory: $out"
mkdir -p "$out" "$out/enforcing-data" "$out/shadow-data"

ENFORCING_PID=''
SHADOW_PID=''
cleanup() {
  local pid
  for pid in "$ENFORCING_PID" "$SHADOW_PID"; do
    if [[ -n $pid ]] && kill -0 "$pid" 2>/dev/null; then
      kill -INT "$pid" 2>/dev/null || true
      wait "$pid" 2>/dev/null || true
    fi
  done
}
trap cleanup EXIT

# The posture file is D32 clause (c)'s runtime lever, transported by a file
# because the durable `ramp/{control}` row it specifies is not in the tree yet.
# Everything downstream of the process reading it is the production path.
echo required >"$out/posture"

note 'starting the enforcing gateway (C1 required, with a runtime posture lever)'
"$P5_DUPE_BIN" \
  gateway \
  --cluster-file "$ORRERY_FDB_CLUSTER_FILE" \
  --data-dir "$out/enforcing-data" \
  --enforcement required \
  --posture-file "$out/posture" \
  >"$out/enforcing-gateway.json" 2>"$out/enforcing-gateway.log" &
ENFORCING_PID=$!

note 'starting the shadow gateway (C1 shadow)'
"$P5_DUPE_BIN" \
  gateway \
  --cluster-file "$ORRERY_FDB_CLUSTER_FILE" \
  --data-dir "$out/shadow-data" \
  --enforcement shadow \
  >"$out/shadow-gateway.json" 2>"$out/shadow-gateway.log" &
SHADOW_PID=$!

await_ready() { # readiness-file pid label
  local file=$1 pid=$2 label=$3
  for _ in $(seq 1 300); do
    [[ -s $file ]] && return 0
    kill -0 "$pid" 2>/dev/null \
      || die "$label gateway exited before readiness; see $out"
    sleep 0.1
  done
  die "$label gateway did not become ready; see $out"
}
await_ready "$out/enforcing-gateway.json" "$ENFORCING_PID" enforcing
await_ready "$out/shadow-gateway.json" "$SHADOW_PID" shadow

ENFORCING_NODE=$(jq -er '.node_id' "$out/enforcing-gateway.json") \
  || die 'enforcing readiness has no node_id'
ENFORCING_ADDR=$(jq -er '.bind_addr' "$out/enforcing-gateway.json") \
  || die 'enforcing readiness has no bind_addr'
SHADOW_NODE=$(jq -er '.node_id' "$out/shadow-gateway.json") \
  || die 'shadow readiness has no node_id'
SHADOW_ADDR=$(jq -er '.bind_addr' "$out/shadow-gateway.json") \
  || die 'shadow readiness has no bind_addr'

# Each process is held to the posture it claims. Two enforcing gateways would
# make every arm below trivially satisfiable in the wrong direction.
[[ $(jq -r '.enforcement' "$out/enforcing-gateway.json") == required ]] \
  || die 'the enforcing gateway does not attest required K-of-N enforcement'
[[ $(jq -r '.enforcement' "$out/shadow-gateway.json") == shadow ]] \
  || die 'the shadow gateway does not attest shadow-mode observation'
[[ $ENFORCING_NODE != "$SHADOW_NODE" ]] \
  || die 'both gateways published the same node id; the comparison is with itself'

set +e
"$P5_DUPE_BIN" \
  ramp \
  --enforcing-node "$ENFORCING_NODE" \
  --enforcing-addr "$ENFORCING_ADDR" \
  --shadow-node "$SHADOW_NODE" \
  --shadow-addr "$SHADOW_ADDR" \
  --cluster-file "$ORRERY_FDB_CLUSTER_FILE" \
  --enforcing-log "$out/enforcing-gateway.log" \
  --shadow-log "$out/shadow-gateway.log" \
  --posture-file "$out/posture" \
  --report "$out/report.json" \
  >"$out/harness.log" 2>&1
harness_status=$?
set -e

[[ -r $out/report.json ]] || die "harness wrote no report; see $out/harness.log"
jq . "$out/report.json"
if [[ $harness_status -ne 0 ]]; then
  die "ramp gate FAILED; see $out/report.json and $out/harness.log"
fi
jq -e '.result == "pass"' "$out/report.json" >/dev/null \
  || die 'harness exited zero without a passing report'
jq -e '.arms.enforcing_acts.passed == true' "$out/report.json" >/dev/null \
  || die 'the enforcing gateway did not act on the synthetic offender'
jq -e '.arms.shadow_observes.passed == true' "$out/report.json" >/dev/null \
  || die 'the shadow gateway did not observe the offender with the matching verdict'
jq -e '.arms.shadow_does_not_act.passed == true' "$out/report.json" >/dev/null \
  || die 'the shadow gateway acted, or observed nothing at all'
jq -e '.arms.reversibility.passed == true' "$out/report.json" >/dev/null \
  || die 'a control demoted to shadow did not stop acting within the bound'

touch "$out/PASSED"
note "shadow observed and acted on nothing; enforcing acted; evidence in $out/report.json"
