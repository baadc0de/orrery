#!/usr/bin/env bash
# Run the Regolith client's test suite on whatever platform this is, and refuse
# to call a suite that executed nothing a pass.
#
#   ./scripts/client-tests.sh                  run the suite and assert on its output
#   ./scripts/client-tests.sh --check <log>    assert on an already-captured log
#
# ── Why this exists ──────────────────────────────────────────────────────────
#
# `clients/regolith` is a standalone workspace, so `cargo test --workspace` at
# the root runs zero of its tests; scripts/check.sh's gates lane covers Linux,
# and ci.yml's `client-platforms` job (#344) covers Windows and macOS. On every
# one of those platforms the invocation below is the whole of the assertion,
# and plain `cargo test` alone is green in exactly the ways that make it mean
# nothing:
#
#   1. a compile failure fails loudly on its own — that half needs no help;
#   2. but a working-directory mistake that runs *another* workspace's tests
#      instead of this one is green;
#   3. and any filter or cfg accident that leaves zero tests executed is green
#      too — `0 passed; N filtered out` reads as success.
#
# So the runner, not the test files, is where "the client's own tests executed"
# is enforced, in the same three-way shape scripts/fdb-tests.sh uses for the
# fdb tier:
#
#   1. the cargo invocation must succeed;
#   2. at least one `test result:` line must exist (a compile failure emits
#      none, and so does a run that never reached a test binary);
#   3. the executed total must clear a floor, and the log must show *this
#      workspace's* library unittest binary having run — not merely some
#      workspace's.
#
# ── Portability ──────────────────────────────────────────────────────────────
#
# This script runs on windows-latest (Git Bash) and macos-latest (/bin/bash is
# 3.2 there), as well as locally. That forbids everything bash 3.2 lacks —
# `mapfile`, associative arrays — and GNU-only sed escapes: the ANSI strip
# embeds a literal escape byte produced by `printf '\033'`, which BSD sed
# accepts. The awk that sums results is POSIX throughout; the p4-ledger macOS
# failure was a BSD/coreutils mismatch, and nothing here may repeat it.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

die() { echo "::error::$*" >&2; exit 1; }

# The floor is deliberately below the current count rather than equal to it —
# it is a "did the client's tests execute at all" tripwire, not a census that
# has to be edited every time a test is added. `cargo test` in
# clients/regolith measured 5 executed tests on 2026-08-23 (one in src/lib.rs,
# three in src/intent.rs, one in src/telemetry.rs; no integration targets).
# A floor of 4 catches every vacuous shape below while tolerating a single
# test's retirement without a floor edit.
FLOOR=4

# The package name, spelled out because clause 3 of check_log asserts against
# it. If the package is renamed, this gate fails loudly rather than silently
# guarding a workspace that no longer exists.
PACKAGE=orrery_regolith_client

# ── Assertions over a captured log ───────────────────────────────────────────
check_log() {
  local raw="$1" log esc
  [[ -r "$raw" ]] || die "no test log at $raw"

  # The workflows set `CARGO_TERM_COLOR: always`, which survives a pipe and
  # would put escape sequences through the middle of everything read below.
  # The live run asks for `never`; this strips them anyway, so --check works
  # on a log captured by anything. printf produces the literal byte, because
  # BSD sed has no \x1b.
  esc="$(printf '\033')"
  log="$(mktemp)"
  trap 'rm -f "$log"' RETURN
  sed -e "s/${esc}\[[0-9;]*m//g" "$raw" > "$log"

  # 2. At least one verdict line. A compile failure emits none; so would any
  #    invocation that somehow never reached a test binary.
  if ! grep -q '^test result:' "$log"; then
    die "no 'test result:' lines in $raw — the client's test binary never ran"
  fi

  # A failing suite is red however many passed beside the failure; checked
  # separately from the floor so neither clause can mask the other.
  if grep -q '^test result: FAILED' "$log"; then
    die "the client's test suite reported failures — see $raw"
  fi

  # 3a. The right suite. `Running unittests` names the library unittest
  #     binary; the package name beside it says whose. A working directory
  #     pointed at another workspace passes cargo green while running none of
  #     these five tests, which is exactly the lie this clause exists to catch.
  if ! grep -q 'Running unittests' "$log" || ! grep -q "$PACKAGE" "$log"; then
    die "the log does not show ${PACKAGE}'s own library unittest binary having run"
  fi

  # 3b. The floor. Summed over every `test result:` line the way fdb-tests.sh
  #     sums its own, which is sturdier than counting per-test lines
  #     interleaved with output.
  local executed
  executed="$(awk '
    /^test result:/ {
      for (i = 1; i <= NF; i++) if ($(i + 1) == "passed;") { n += $i }
      next
    }
    END { print n + 0 }
  ' "$log")"

  echo "executed $executed tests (floor $FLOOR)"
  if (( executed < FLOOR )); then
    die "only $executed tests executed, below the floor of $FLOOR — the client's suite did not really run"
  fi

  echo "regolith client suite ran: $executed tests, all green"
}

# ── Self-test ────────────────────────────────────────────────────────────────
#
# The fdb-tests idiom: prove the assertions still assert, per-commit, on a
# runner with nothing installed at all. Seven synthetic logs in the shapes
# cargo and libtest actually emit — healthy, colourised, and one for each way
# this gate can be lied to: vacuously filtered to zero, thin under the floor,
# red-with-passes, never reaching a result line at all, and another
# workspace's healthy suite standing in for this one.
self_test() {
  local tmp rc failures=0 fixture esc
  tmp="$(mktemp -d)"
  trap 'rm -rf "$tmp"' RETURN
  esc="$(printf '\033')"

  emit_log() { # out flavour [passed]
    local out="$1" flavour="$2" passed="${3:-0}"
    local failed=0 verdict=ok pkg="$PACKAGE"
    case "$flavour" in
      failed)  failed=1; verdict=FAILED ;;
      nosuite) pkg=orrery_persistd ;;
    esac
    {
      echo "   Compiling ${pkg} v0.1.0"
      echo "    Finished \`test\` profile [unoptimized + debuginfo] target(s)"
      if [[ "$flavour" == compile-error ]]; then
        echo "error[E0432]: unresolved import \`bevy::prelude\`"
        echo "error: could not compile \`${pkg}\` due to 1 previous error"
      else
        echo "     Running unittests src/lib.rs (target/debug/deps/${pkg}-a1b2c3d4e5f60718)"
        echo "test result: ${verdict}. ${passed} passed; ${failed} failed; 0 ignored; 0 measured; 0 filtered out; finished in 1.23s"
        echo "   Doc-tests ${pkg}"
        echo "test result: ${verdict}. 0 passed; ${failed} failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.05s"
      fi
    } > "$out"
  }

  expect() { # name want file
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

  fixture="$tmp/good.log";     emit_log "$fixture" healthy 5;          expect "a real run passes" pass "$fixture"

  sed -e "s/^     Running/${esc}[1;32mRunning${esc}[0m/" \
      -e "s/result: ok\./result: ${esc}[32mok${esc}[0m./" \
      "$tmp/good.log" > "$tmp/colour.log"
  expect "a colourised run still parses" pass "$tmp/colour.log"

  fixture="$tmp/vacuous.log";  emit_log "$fixture" healthy 0;          expect "0 passed with N filtered out is red" fail "$fixture"
  fixture="$tmp/thin.log";     emit_log "$fixture" healthy 2;          expect "a run under the floor is red" fail "$fixture"
  fixture="$tmp/red.log";      emit_log "$fixture" failed 4;           expect "a FAILED verdict is red even at the floor" fail "$fixture"
  fixture="$tmp/compile.log";  emit_log "$fixture" compile-error;      expect "no result lines (compile failure) is red" fail "$fixture"
  fixture="$tmp/nosuite.log";  emit_log "$fixture" nosuite 5;          expect "another workspace's healthy suite is red" fail "$fixture"

  (( failures == 0 )) || die "$failures self-test case(s) failed"
  echo "client-tests self-test: 7/7"
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

[[ $# -eq 0 ]] || die "usage: $0 [--check <logfile>|--self-test]"

[[ -f "$ROOT/clients/regolith/Cargo.toml" ]] \
  || die "clients/regolith is absent — the client lands with PR #342; see issue #344"

LOG="${CLIENT_TESTS_LOG:-$ROOT/target/client-platforms.log}"
mkdir -p "$(dirname "$LOG")"

set +e
(
  cd "$ROOT/clients/regolith"
  # The workflows force colour on; a log that has to be parsed does not want
  # it. No filter arguments are passed and none may be: a filter is how this
  # gate gets asked to run a subset and call it coverage.
  CARGO_TERM_COLOR=never cargo test
) 2>&1 | tee "$LOG"
status="${PIPESTATUS[0]}"
set -e

(( status == 0 )) || die "cargo test failed (exit $status); see $LOG"

check_log "$LOG"

if [[ -n "${GITHUB_STEP_SUMMARY:-}" ]]; then
  {
    echo "### Regolith client — $(uname -s)"
    echo '```'
    grep -E '^(executed|regolith client)' "$LOG" || true
    echo '```'
  } >> "$GITHUB_STEP_SUMMARY"
fi
