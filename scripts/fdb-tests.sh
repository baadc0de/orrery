#!/usr/bin/env bash
# Run the FoundationDB-gated test surface, and refuse to call a skipped suite a
# pass.
#
#   ./scripts/fdb-tests.sh                 run the suite and assert on its output
#   ./scripts/fdb-tests.sh --check <log>   assert on an already-captured log
#
# ── Why this exists ──────────────────────────────────────────────────────────
#
# `orrery_persistd` and `orrery_seed` carry a whole tier of tests that only
# compile under `--features fdb` — checkpoint write and restore, the
# `actor/{shard}` fence CAS, the lease CAS, intent commit, seed apply. Every one
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
# reported 341 on 2026-08-16.
FLOOR="${ORRERY_FDB_TEST_FLOOR:-320}"

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

  # 7 targets × 32 + 120 unit tests = 344, over the 320 floor.
  fixture="$tmp/good.log";    emit_log "$fixture" none 32;            expect "a real run passes" pass "$fixture"
  # The same log with `CARGO_TERM_COLOR=always` escapes through it.
  sed -e 's/^     Running/     \x1b[1;32mRunning\x1b[0m/' \
      -e 's/result: ok\./result: \x1b[32mok\x1b[0m./' "$tmp/good.log" > "$tmp/colour.log"
  expect "a colourised run still parses" pass "$tmp/colour.log"

  fixture="$tmp/skipped.log"; emit_log "$fixture" skip 32;            expect "a skipped run is red" fail "$fixture"
  fixture="$tmp/thin.log";    emit_log "$fixture" none 1;             expect "a run under the floor is red" fail "$fixture"
  # 40 apiece so the six remaining targets still clear the floor: this case has
  # to fail because fence_split is missing, not because the total is thin.
  fixture="$tmp/absent.log";  emit_log "$fixture" none 40 fence_split; expect "a missing fdb target is red" fail "$fixture"
  # A target that ran and asserted nothing. `check_log` has always refused this
  # — "a target that is present with a zero count is as dark as one that never
  # ran" — and until now no fixture exercised the clause: measured 2026-08-17,
  # relaxing `(( count > 0 ))` to `(( count >= 0 ))` left all five cases green.
  # 60 apiece keeps the total at 480, well over the floor, so this can only be
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
  cargo test -p orrery_persistd -p orrery_seed \
    --features orrery_persistd/fdb,orrery_seed/fdb \
    -- --nocapture
) 2>&1 | tee "$LOG"
status="${PIPESTATUS[0]}"
set -e

[[ "$status" -eq 0 ]] || die "cargo test failed (exit $status); see $LOG"

check_log "$LOG"
