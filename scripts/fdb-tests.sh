#!/usr/bin/env bash
# Run the FoundationDB-gated test surface, and refuse to call a skipped suite a
# pass.
#
#   ./scripts/fdb-tests.sh                 run the suite and assert on its output
#   ./scripts/fdb-tests.sh --check <log>   assert on an already-captured log
#
# ── Why this exists ──────────────────────────────────────────────────────────
#
# `orrery_persistd`, `orrery_seed` and `orrery_identity` carry a whole tier of
# tests that only compile under `--features fdb` — checkpoint write and restore,
# the `actor/{shard}` fence CAS, the lease CAS, intent commit, seed apply, and
# the `id/` subspace's one-transaction bind. Every one
# of them opens with a guard that looks for a cluster and, not finding one,
# `eprintln!("skipping: ...")` and returns `Ok`.
#
# That guard is right for a developer's `cargo test`, and it is a trap for CI:
# adding `--features fdb` to a job on a runner with no cluster turns 27 tests
# into 27 passes that assert nothing. So the runner, not the test files, is
# where "a real cluster was required" is enforced, and it is enforced three
# ways:
#
#   1. the cargo run must succeed;
#   2. no `skipping:` line may appear anywhere in its output;
#   3. the number of tests that actually executed must clear a floor, and every
#      fdb-gated test target must have contributed some.
#
# (2) is what makes an unreachable cluster red rather than green. (3) is what
# catches the other direction — a feature flag that silently stopped selecting
# the fdb code, or a filter that ran none of it.
#
# Two details make (2) work at all. The skip messages go to **stderr**, so the
# run is captured with `2>&1`; and cargo swallows a passing test's output unless
# `--nocapture` is passed, so it is.
#
# ── Which cluster ────────────────────────────────────────────────────────────
#
# `ORRERY_FDB_CLUSTER_FILE` is required and is never defaulted. Most of these
# suites walk upward for `.fdb-dev/fdb.cluster` when the variable is absent
# (`crates/orrery_persistd/tests/checkpoint_restore.rs`,
# `crates/orrery_seed/tests/fdb_gates.rs`) but `tests/lease_fdb.rs` reads only
# the variable, so an implicit cluster gets a partial run. More importantly,
# these suites write to whatever cluster they find (docs/11-roadmap.md, C-8):
# the target must be a throwaway, never a shared development cluster.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

# The floor is deliberately below the current count rather than equal to it —
# it is a "did the fdb tier run at all" tripwire, not a census that has to be
# edited every time a test is added. `cargo test -p orrery_persistd -p
# orrery_seed --features orrery_persistd/fdb,orrery_seed/fdb -- --list`
# reported 341 on 2026-08-16. A full run measured 513 executed tests on
# 2026-08-20, after the live shard handover (issue #119, D26 rule 3) added 11:
# `shard_handover_fdb` (1, and the only one that needs the cluster),
# `shard_handover` (5), `shard_handover_gateway` (2) and three fence unit
# tests. The floor rose by those 11 — 320 -> 331 — because a floor that never
# moves stops being a tripwire and becomes a number: every test added widens
# the gap the tier can go dark inside without tripping it.
#
# A full run measured 527 executed tests on 2026-08-21, after the item
# ownership transfer (issue #145, D11 §7) added 14: five in `intent_commit`
# that need the cluster — the trade itself, the four named durable refusals,
# the replay, the two-transfer race and the conflicting-key assertion — and
# nine `orrery_persistd` unit tests covering the op's args layout, its
# admission arms and the `MemIntentExecutor` half of the same contract. The
# floor rises by those 14: 331 -> 345.
#
# A full run measured 566 executed tests on 2026-08-21, after K-of-N
# attestation enforcement (issue #147, D27/D28) added 29: six in the new
# `intent_witness_epoch` file — the only ones that need the cluster, because
# they are about the durable `epoch/` record, the draw commitment, the
# recorded eligible vector and the two ways a stale draw key can survive a
# gateway restart or a sibling handover — two in `gateway_witness_epoch` for
# the courier hop, ten witness-epoch cache unit tests, nine
# admission-predicate unit tests and two keyspace tests for the three new
# families. The floor rises by those 29: 345 -> 374.
#
# The measured total is 39 above the last recorded one rather than 29. The
# remaining 10 landed between the two measurements and were never recorded, so
# they are deliberately not claimed here: the floor tracks what a change can
# account for, and inflating it with tests nobody attributed would make the
# next person's arithmetic wrong instead of this one's.
#
# A full run measured 605 executed tests on 2026-08-21, after D29's
# low-population path (issue #150). It adds 38, and unusually for this list
# only four of them need the cluster. Those four are new `intent::fdb` unit
# tests in `orrery_persistd`'s lib target rather than a new `tests/` file, and
# they are here because each one asserts something only a real serializable
# transaction can show: that a provisional commit writes the intent row, the
# hold and the ledger effect together; that a held balance row is refused as an
# input by a `get` inside the intent's own transaction; that annulment's
# inverse, finality flip, restamped deadline and compensating receipt are one
# transaction, and that a replay of an annulled intent re-applies nothing; and
# that the per-account cap is enforced against the durable row.
#
# The remaining 34 need no cluster — 25 in `intent::provisional` covering the
# classifier, the quarantine, the sweep, the verdict table, the deadline rule
# and the GC interlock against the in-memory tier, four new keyspace tests for
# the `provisional/` family and `sweepable`, four admission-predicate tests
# including the bypass check, and one client test for the non-terminal
# `IntentStatus` — but they run in the same invocation and count the same, so
# the floor rises by all 38: 374 -> 412.
#
# The measured total is 39 above the last recorded one rather than 38, for the
# same reason the previous entry's arithmetic was one short of its measurement:
# one test landed between the two runs and was never attributed. It is
# deliberately not claimed here.
#
# Account-level party exclusion (issue #211, D31) adds seven. One needs the
# cluster: `intent_witness_epoch`'s proof that a party account's NodeId is
# absent from the **recorded** `AttestRow.eligible`, which is the only place
# the executor's re-derivation of `E(I)` can be observed at all. The other six
# are `intent::tests` admission-predicate tests — the issuer's second device,
# the counterparty's device, two devices filling one slot, D31 clause (f)'s
# miss semantics, the honest-witness regression guard and announced-order
# preservation — and need no cluster, but run in the same invocation and count
# the same. The floor rises by all seven: 412 -> 419.
#
# The `orrery_identity` crate (issue #210, D12/D31) adds eighteen and joins the
# invocation as a third package. Five need the cluster and live in the library
# target: they are what makes D31 clause (b) — `db` written in the same
# transaction as `da`, so the two are never observed disagreeing — a proof
# rather than a comment. The load-bearing one aborts a bind after staging all
# three rows and asserts that *none* of them landed, which is the observable
# form of the window a two-transaction writer would open, and there is no such
# transaction without a cluster. The other thirteen are the `issuance` target's
# in-memory mint/refresh/rotation round-trips against the protocol verifier;
# they need no cluster but run in the same invocation and count the same. The
# floor rises by all eighteen: 419 -> 437.
# The attestation shadow arm and its deployment switch (issue #217, D32
# clauses (b)–(d)) add eighteen. Two need the cluster, and they are the two
# durable consequences the record decides: a shadow commit's `attest/` row
# carries `enforced: false` while an enforced one carries `true` (a false
# audit trail is worse than none), and the commit-time required-subset
# re-proof is disarmed under shadow, so the executor does not refuse what
# admission admitted. Neither is observable without a transaction. The other
# sixteen are `intent::tests`' shadow pair-tests (10, each written against one
# acting validator and one watching one), `intent::shadow`'s verdict and
# bounded-log unit tests (3) and `bin/persistd`'s flag-reaches-the-validator
# tests (3); they need no cluster but run in the same invocation and count the
# same. The floor rises by all eighteen: 437 -> 455.
#
# Strike-ledger filing (issue #215, D33) adds one executor unit test that needs
# the cluster. It writes distinct subject and reporter bindings, files a
# confirmed verdict, and proves the `ya` row exists only under the subject's
# account. The floor rises by one more: 455 -> 456.
# tests (3); they need no cluster but run in the same invocation and count
#
# D35's key-format change (issue #226) nets three. One needs the cluster: the
# `l`-family audit in `tests/lease_fdb.rs`, which scans `[b'l', b'm')` and
# fails on any key whose byte 1 is not a registered sub-discriminator — the
# loud half of the no-migration posture, expected count zero. The other two
# are keyspace unit tests that run in the same invocation: the pair-model
# completeness guard and the inverted acceptance test, replacing
# `lease_key_overlaps_the_ledger_family` (so the unit delta is +2, not +3 —
# one recording test retired). The floor rises by all three: 456 -> 459.
#
# The binding-rate window (issue #255, D36) adds ten, all in `orrery_identity`'
# library target. One needs the cluster: the durable ninth-event refusal,
# which asserts against the real `dw` row read back off its raw key. The other
# nine need no cluster but run in the same invocation and count the same —
# five window-semantics unit tests against the shared prune/check/append logic
# and four enforcement tests through `MemAccountStore`. Two existing tests were
# extended rather than added (the keyspace `id/` width and sub-span guards, and
# the bind atomicity proof, which now also asserts `dw` unchanged across an
# injected abort), so they move nothing. A full run measured 687 executed tests
# on 2026-08-22. The floor rises by those ten: 459 -> 469.
# The ramp measurement (issue #221, D32 clause (e)) adds twelve, and **none**
# of them needs the cluster: the whole instrument is counters on the admission
# path, which is the gateway-side fast filter that performs no FDB round trip
# by design. Nine are `intent::ramp`'s unit tests — the 0-of-10000 against
# 0-of-0 distinction, coverage falling when qualifying activity goes
# unobserved, the unevaluated split, account cardinality against event volume,
# the cause vocabulary, the unattributed bucket, truncation reporting, the
# cohort union, and the artifact round trip — and three are `intent::tests`'
# validator-level pair tests (the would-have-acted counter's two arms, the
# denominator counting what the shadow arm never saw, and an `Off` validator
# reporting no coverage rather than a clean sheet). They run in the same
# invocation and count the same. The thirteenth, `emit_ramp_artifact`, is
# `#[ignore]`d — it regenerates a committed artifact — so it does not.
# The floor rises by all twelve: 469 -> 481.
FLOOR="${ORRERY_FDB_TEST_FLOOR:-481}"

# Every test file whose contents only mean anything against a real cluster. If
# one of these reports no executed tests, the tier is dark again whatever the
# totals say.
REQUIRED_TARGETS=(
  checkpoint_restore
  fence_split
  lease_fdb
  intent_commit
  area_load
  persistd_binary
  fdb_gates
  # A live shard handover is two processes taking turns owning one durable
  # row, so it is exactly the tier a memory store cannot speak for — and the
  # `PreHandover` checkpoint's fence read is where that bit: the in-memory
  # suite passed while a real cluster refused the checkpoint outright.
  shard_handover_fdb
  # The durable half of D27's K-of-N enforcement. A memory store cannot speak
  # for it at all: what these assert is that the draw commitment and the
  # eligible vector land in the *same serializable transaction* as the intent's
  # effects, which is the property that makes a retrospective audit of the draw
  # non-vacuous, and there is no such transaction without a cluster.
  intent_witness_epoch
)

die() { echo "::error::$*" >&2; exit 1; }

# ── Assertions over a captured log ───────────────────────────────────────────
check_log() {
  local raw="$1" log
  [[ -r "$raw" ]] || die "no test log at $raw"

  # The workflows set `CARGO_TERM_COLOR: always`, which survives a pipe and
  # would put escape sequences through the middle of every line these
  # assertions read. The run below asks for `never`; this strips them anyway,
  # so `--check` works on a log captured by anything.
  log="$(mktemp)"
  trap 'rm -f "$log"' RETURN
  sed -e 's/\x1b\[[0-9;]*m//g' "$raw" > "$log"

  # 1. Skips. Anything that announced itself as skipped means the cluster was
  #    not there, and a suite that did not run is not a suite that passed.
  if grep -n 'skipping:' "$log"; then
    die "the fdb suite skipped tests — the cluster was unreachable, so this run proves nothing"
  fi

  # 2 and 3 read the same shape. cargo prints `Running tests/<name>.rs (…)`
  # before each test binary, and libtest closes each binary with
  # `test result: ok. <n> passed; …`, so one pass over the log attributes every
  # pass count to the target that produced it. Summing those counts is a more
  # honest "how many tests executed" than counting per-test lines, which
  # `--nocapture` interleaves with the tests' own output.
  local counts
  counts="$(awk '
    /Running .*tests\/[A-Za-z0-9_]+\.rs/ {
      match($0, /tests\/[A-Za-z0-9_]+\.rs/)
      t = substr($0, RSTART + 6, RLENGTH - 9)
      next
    }
    /Running unittests/ { t = "<unittests>"; next }
    /Doc-tests/         { t = "<doctests>";  next }
    /^test result:/ {
      for (i = 1; i <= NF; i++) if ($(i + 1) == "passed;") { passed[t] += $i; seen[t] = 1 }
    }
    END { for (k in seen) printf "%s %d\n", k, passed[k] }
  ' "$log")"

  local executed=0 target count
  while read -r target count; do
    [[ -n "$target" ]] || continue
    executed=$(( executed + count ))
  done <<<"$counts"

  echo "executed $executed tests (floor $FLOOR)"
  if (( executed < FLOOR )); then
    die "only $executed tests executed, below the floor of $FLOOR — the fdb tier did not run"
  fi

  # A target that is present with a zero count is as dark as one that never
  # ran, so both are failures.
  for target in "${REQUIRED_TARGETS[@]}"; do
    count="$(awk -v t="$target" '$1 == t { print $2 }' <<<"$counts")"
    [[ -n "$count" ]] || die "test target '$target' never ran"
    (( count > 0 )) || die "test target '$target' executed 0 tests"
    echo "  $target: $count passed"
  done

  echo "fdb suite ran against a real cluster: $executed tests, no skips"
}

# ── Self-test ────────────────────────────────────────────────────────────────
#
# The same idiom as the P2 and P3 gates: prove the assertions still assert,
# per-commit, on a runner with no cluster anywhere near it. Six synthetic logs
# in the shape cargo and libtest actually emit — one healthy, one healthy but
# colourised, and one for each way the tier can go dark: skipped, thin, a target
# missing, and a target present having asserted nothing.
self_test() {
  local tmp fixture rc failures=0
  tmp="$(mktemp -d)"
  trap 'rm -rf "$tmp"' RETURN

  emit_log() {
    local out="$1" skip="$2" per="$3" omit="${4:-}" zero="${5:-}"
    : > "$out"
    {
      echo "   Compiling orrery_persistd v0.1.0"
      echo "    Finished \`test\` profile [unoptimized + debuginfo] target(s)"
      echo "     Running unittests src/lib.rs (target/debug/deps/orrery_persistd-1)"
      echo "test result: ok. 120 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 1.00s"
      local t
      for t in "${REQUIRED_TARGETS[@]}"; do
        [[ "$t" == "$omit" ]] && continue
        echo "     Running tests/${t}.rs (target/debug/deps/${t}-2)"
        [[ "$skip" == "skip" ]] && echo "skipping: ORRERY_FDB_CLUSTER_FILE is absent"
        # `$zero` is the target that ran and asserted nothing — a compiled-away
        # `#[cfg]` block, or a file whose tests all moved out of it.
        local n="$per"
        [[ "$t" == "$zero" ]] && n=0
        echo "test result: ok. ${n} passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 1.00s"
      done
    } >> "$out"
  }

  expect() {
    local name="$1" want="$2" file="$3"
    rc=0
    ( check_log "$file" ) >/dev/null 2>&1 || rc=$?
    if [[ "$want" == "pass" && "$rc" -ne 0 ]]; then
      echo "FAIL: $name should have passed, exited $rc"; failures=$(( failures + 1 ))
    elif [[ "$want" == "fail" && "$rc" -eq 0 ]]; then
      echo "FAIL: $name should have failed, exited 0"; failures=$(( failures + 1 ))
    else
      echo "  ok: $name"
    fi
  }

  # 9 targets × 40 + 120 unit tests = 480, over the 455 floor. The per-target
  # count moves with the floor: this fixture has to stay comfortably above it
  # or the healthy case starts failing for the reason the thin case is
  # supposed to.
  fixture="$tmp/good.log";    emit_log "$fixture" none 40;            expect "a real run passes" pass "$fixture"
  # The same log with `CARGO_TERM_COLOR=always` escapes through it.
  sed -e 's/^     Running/     \x1b[1;32mRunning\x1b[0m/' \
      -e 's/result: ok\./result: \x1b[32mok\x1b[0m./' "$tmp/good.log" > "$tmp/colour.log"
  expect "a colourised run still parses" pass "$tmp/colour.log"

  fixture="$tmp/skipped.log"; emit_log "$fixture" skip 32;            expect "a skipped run is red" fail "$fixture"
  fixture="$tmp/thin.log";    emit_log "$fixture" none 1;             expect "a run under the floor is red" fail "$fixture"
  # 45 apiece so the eight remaining targets still clear the floor: this case has
  # to fail because fence_split is missing, not because the total is thin.
  fixture="$tmp/absent.log";  emit_log "$fixture" none 45 fence_split; expect "a missing fdb target is red" fail "$fixture"
  # A target that ran and asserted nothing. `check_log` has always refused this
  # — "a target that is present with a zero count is as dark as one that never
  # ran" — and until now no fixture exercised the clause: measured 2026-08-17,
  # relaxing `(( count > 0 ))` to `(( count >= 0 ))` left all five cases green.
  # 60 apiece keeps the total at 600, well over the floor, so this can only be
  # red for the reason it names.
  fixture="$tmp/silent.log";  emit_log "$fixture" none 60 '' fence_split; expect "a target that ran zero tests is red" fail "$fixture"

  (( failures == 0 )) || die "$failures self-test case(s) failed"
  echo "fdb-tests self-test: 6/6"
}

# ── Entry ────────────────────────────────────────────────────────────────────
if [[ "${1:-}" == "--self-test" ]]; then
  self_test
  exit 0
fi

if [[ "${1:-}" == "--check" ]]; then
  [[ -n "${2:-}" ]] || die "usage: $0 --check <logfile>"
  check_log "$2"
  exit 0
fi

[[ -n "${ORRERY_FDB_CLUSTER_FILE:-}" ]] \
  || die "ORRERY_FDB_CLUSTER_FILE is not set; this suite must be pointed at a throwaway cluster explicitly"
[[ -r "$ORRERY_FDB_CLUSTER_FILE" ]] \
  || die "ORRERY_FDB_CLUSTER_FILE=$ORRERY_FDB_CLUSTER_FILE is not readable"

# A preflight probe so an unreachable cluster is reported as what it is,
# instead of as 27 tests that mysteriously skipped. The skip check above is
# still the backstop: this probe is a courtesy, and `fdbcli` may not be on PATH
# in every environment that can nonetheless link libfdb_c.
#
# `timeout` because fdbcli has none of its own: pointed at a coordinator that
# answers but has no database, it blocks rather than reporting.
if command -v fdbcli >/dev/null 2>&1; then
  timeout 20 fdbcli -C "$ORRERY_FDB_CLUSTER_FILE" --exec 'status minimal' 2>/dev/null \
    | grep -q 'is available' \
    || die "the cluster at $ORRERY_FDB_CLUSTER_FILE is not available"
fi

LOG="${ORRERY_FDB_TEST_LOG:-$ROOT/target/fdb-tests.log}"
mkdir -p "$(dirname "$LOG")"

# One invocation over both packages, not two, and that is a regression guard
# rather than a convenience: C-8 (docs/11-roadmap.md) was a bug whose whole
# signature was "the suites pass separately and fail combined" — persistd's
# split tests left `actor/{shard}` fence rows that then blocked every seeder
# gate's pre-wipe. Splitting this command would stop testing for its return.
set +e
(
  cd "$ROOT"
  # The workflows force colour on; a log that has to be parsed does not want it.
  CARGO_TERM_COLOR=never \
  cargo test -p orrery_persistd -p orrery_seed -p orrery_identity \
    --features orrery_persistd/fdb,orrery_seed/fdb,orrery_identity/fdb \
    -- --nocapture
) 2>&1 | tee "$LOG"
status="${PIPESTATUS[0]}"
set -e

[[ "$status" -eq 0 ]] || die "cargo test failed (exit $status); see $LOG"

check_log "$LOG"
