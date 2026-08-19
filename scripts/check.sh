#!/usr/bin/env bash
# One definition of the per-commit checks, runnable locally and by CI.
#
#   ./scripts/check.sh              every lane, in CI's order
#   ./scripts/check.sh fmt          rustfmt over all eight workspaces
#   ./scripts/check.sh clippy       both feature sets, -D warnings
#   ./scripts/check.sh gates        static gates, harness self-tests, tool tests
#   ./scripts/check.sh test         the root workspace's test suite
#   ./scripts/check.sh doctor       delegate to dev-cache.sh: is the cache wired up?
#   ./scripts/check.sh --self-test  the lane table and self-test coverage hold
#   ./scripts/check.sh --list       what each lane would run, without running it
#
# Why this exists. `.github/workflows/ci.yml` used to be the only place the
# commands existed, so the only way to find out whether a change passed was to
# push and wait — one agent round-trip per miss. The four per-commit jobs now
# carry environment (runner selection, toolchain, apt, cache, the rustc
# wrapper, the FoundationDB client) and delegate their bodies here.
#
# **Scope the claim.** This script is the whole body of ci.yml's `fmt`,
# `clippy`, `gates` and `test` jobs and nothing else. `determinism` and
# `determinism-verdict` keep four more cargo commands of their own — they are a
# cross-platform matrix, and running one leg locally proves nothing the matrix
# is for — and `.github/workflows/nightly.yml` carries six of its own plus the
# four gate scripts it runs for real rather than in `--self-test`. Neither
# workflow is reproduced here.
#
# What running it locally actually buys, honestly stated: ci.yml already tests
# and checks the standalone tools, so this is not new coverage for them. The two
# things that are new are (i) reproducing any lane locally at all, and (ii) the
# `fmt` lane, which in CI reached only the root workspace — `cargo fmt --all`
# stops at the workspace boundary, and the seven standalone tools each declare
# their own `[workspace]`, so 27 first-party `.rs` files — every one under
# p0-nat-test, p0-dashboard, p1-swarm, p2-dashboard, p2-load, p3-island and
# p4-streams-bench — had never been rustfmt-checked by anything. Exactly one
# of the seven was dirty when the lane was widened (p0-nat-test).
set -euo pipefail

readonly NAME=check
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
readonly ROOT
die() { echo "$NAME: $*" >&2; exit 2; }
note() { echo "$NAME: $*" >&2; }

# ── The lane table ──────────────────────────────────────────────────────────
#
# Every cargo workspace in the repository, and what the lanes do with it. This
# is the single source the four lanes iterate; `--self-test` holds it against
# the filesystem, which is the other source and cannot be derived from it.
#
# Roles:
#   root   the workspace at the repository root. Its clippy and test commands
#          are spelled out in the lanes below because they carry the vendored
#          `--exclude` set; `fmt` treats it like any other workspace.
#   test   a standalone tool with tests of its own: `cargo test`. Four of them,
#          carrying 87 tests between them — every one of which `cargo test
#          --workspace` at the root runs zero of. `p4-streams-bench` is a measurement
#          rather than a gate — its figures are in its README and the
#          channel-policy decision they justify is in docs/02-networking.md §7 —
#          so what CI owes it is that it still builds and still self-tests.
#   check  a standalone tool with no tests at all: `cargo check --all-targets`.
#          `cargo test` on one of these would be a build dressed up as a gate,
#          which reads as coverage that does not exist. `p3-island`'s behaviour
#          is asserted by the nightly island gate instead, and the two p0 tools
#          by the NAT lab they were written for.
#
# The three vendored crates are members of the root workspace rather than
# workspaces of their own, so they are not listed — and note the consequence
# for `fmt`: `cargo fmt --all` at the root reaches `vendor/`, holding third-
# party code to default rustfmt even though clippy deliberately excludes it.
# That is the status quo, not something this table introduced.
readonly WORKSPACES=(
    '.               root'
    'p0-nat-test     check'
    'p0-dashboard    check'
    'p1-swarm        test'
    'p2-dashboard    test'
    'p2-load         test'
    'p3-island       check'
    'p4-streams-bench test'
)

# The workspace directories, in table order.
ws_dirs() {
    local entry
    for entry in "${WORKSPACES[@]}"; do echo "${entry%% *}"; done
}

# The directories carrying a given role.
ws_with_role() {
    local entry dir role
    for entry in "${WORKSPACES[@]}"; do
        dir="${entry%% *}"
        role="${entry##* }"
        [[ $role == "$1" ]] && echo "$dir"
    done
    return 0
}

# ── Running ─────────────────────────────────────────────────────────────────

DRY_RUN=0

# Echo a command and run it, from a workspace directory. The echo is what makes
# a CI log readable now that a job is one step: without it a failure names the
# script and not the command inside it.
run_in() {
    local dir="$1"; shift
    local where="$dir"
    [[ $where == . ]] && where='(root)'
    printf '%s: [%s] %s $ %s\n' "$NAME" "$LANE" "$where" "$*" >&2
    (( DRY_RUN )) && return 0
    (cd "$ROOT/$dir" && "$@")
}

run() { run_in . "$@"; }

# `CARGO_TARGET_DIR` is deliberately never exported unconditionally, and the two
# hazards are both real rather than theoretical.
#
# On the self-hosted box each of the three runners is pinned to one job and
# keeps one warm `target/` for it (AGENTS.md § CI). Relocating the directory
# from inside the script would make the first run after this merge cold on all
# three, and every run after it cold again whenever the script and the workflow
# disagreed.
#
# Locally, an agent harness sets `CARGO_TARGET_DIR` per task to keep concurrent
# worktrees off each other's builds. Overwriting that collapses isolated lanes
# onto one directory, and cargo takes an *exclusive* lock on a target
# directory — so the lanes would not merely share a cache, they would queue.
#
# So: an already-set value always wins, and per-lane directories are opt-in via
# `--isolate` for local use only. CI never passes it.
ISOLATE=0
# Captured once, before any lane runs. Consulting `CARGO_TARGET_DIR` itself per
# lane would be wrong the moment two lanes run in one invocation: the first
# lane's export is still in the environment when the second asks, so every lane
# after the first would silently reuse the first lane's directory.
readonly INHERITED_TARGET_DIR="${CARGO_TARGET_DIR:-}"
lane_target_dir() {
    (( ISOLATE )) || return 0
    if [[ -n $INHERITED_TARGET_DIR ]]; then
        note "CARGO_TARGET_DIR is already set; --isolate defers to it"
        return 0
    fi
    export CARGO_TARGET_DIR="$ROOT/target/check-$LANE"
    mkdir -p "$CARGO_TARGET_DIR"
    note "isolating this lane: CARGO_TARGET_DIR=$CARGO_TARGET_DIR"
}

# ── The lanes ───────────────────────────────────────────────────────────────

# ci.yml `fmt`, widened. The job ran `cargo fmt --all --check` once, at the
# root; `--all` means "every member of *this* workspace", and the standalone
# tools are members of none of it.
lane_fmt() {
    local dir
    for dir in $(ws_dirs); do
        run_in "$dir" cargo fmt --all --check
    done
}

# ci.yml `clippy`, verbatim. Both steps, in the workflow's order.
lane_clippy() {
    lane_target_dir
    # Vendored crates are excluded — their findings are upstream's to fix — and
    # `--no-deps` is what makes `--exclude` mean anything: the vendored crates
    # are still path dependencies, and clippy lints those too.
    run cargo clippy --workspace --all-targets --no-deps \
        --exclude bevy_replicon \
        --exclude aeronet_iroh \
        --exclude aeronet_tokio_runtime \
        -- -D warnings

    # The fdb feature compiles code the default build never sees. It needs the
    # FoundationDB *client* installed even though clippy never links, because
    # `foundationdb-gen`'s build does `include_bytes!` on
    # /usr/include/foundationdb/fdb.options — a compile input, not a link one.
    run cargo clippy -p orrery_persistd -p orrery_seed --all-targets --no-deps \
        --features orrery_persistd/fdb,orrery_seed/fdb \
        -- -D warnings
}

# ci.yml `gates`, verbatim: the static gates, every harness self-test in
# `scripts/`, and the standalone tools. The gate scripts are owned elsewhere and
# invoked here, never reimplemented.
#
# This block is one half of `--self-test`'s coverage clause, and the other half
# is a `find` over `scripts/`. Adding a `--self-test` to a script and not adding
# a line here now fails the clause rather than passing silently, so the shape of
# this list is load-bearing: one `run scripts/<name>.sh --self-test` per line.
lane_gates() {
    lane_target_dir
    # docs/06-verifiable-core.md §8's static gates.
    run scripts/core-gates.sh

    # The P2 and P3 harnesses need a FoundationDB cluster and eight peer
    # processes respectively, so the real runs are nightly. Their `--self-test`
    # modes are the per-commit half: they assert the scripts still contain the
    # stages that make them proofs.
    run scripts/p2-kill9-gate.sh --self-test
    run scripts/p3-island-gate.sh --self-test
    run scripts/p1-swarm-gate.sh --self-test
    run scripts/fdb-tests.sh --self-test

    # The two P4 scripts, which until now ran their self-tests only in
    # nightly.yml. That is where the cost of an uncovered self-test was
    # actually paid: `p4-ledger.sh --self-test` counted ledger lines with `wc
    # -l`, BSD `wc` pads its output to a fixed width, and the macOS leg of the
    # nightly failed on `[[ "       1" == 1 ]]` on every run until it was
    # found. A per-commit invocation would have caught it on the commit that
    # introduced it. Together they cost about 0.6s and need no cluster, no
    # binaries and no network — there was never a reason for them to be nightly.
    run scripts/p4-accumulate.sh --self-test
    run scripts/p4-ledger.sh --self-test

    # The gate reporter's own. It is the script that says which gates ran and
    # what they measured, so a report that has quietly stopped discovering a
    # gate is exactly as bad as a gate that has quietly stopped failing. Its
    # self-test runs nothing and takes about a second.
    run scripts/gate-status.sh --self-test

    # The one Python self-test that can run per-commit. docs/08 §2.2.2's
    # numbers come from `scripts/p2-baseline-report.py` reading
    # `docs/data/p2-phase-baseline-2026-08-19.jsonl`, which is in the tree — so
    # unlike `intent-tail-derive.py`, whose three checks need ~10 GB of
    # unversioned sweep artifacts and stay nightly, this one needs nothing but
    # the checkout. It asserts the shape the section's argument reads from: both
    # arms present, every run carrying all four gated series, a recovery verdict
    # and the journal fsync the regime split is computed from. A summary that
    # lost one of those would turn a sentence in that section into an assertion
    # about nothing.
    run scripts/p2-baseline-report.py --self-test

    # And this script's own, which nothing ran either: ci.yml calls the four
    # lanes and never `--self-test`, so the lane table's agreement with the tree
    # — and, now, the coverage clause below — were checked only when a human
    # remembered to. It is a few `find`s and a `grep`; it belongs in the gate.
    run scripts/check.sh --self-test

    # Each standalone tool declares its own `[workspace]`, so the `test` lane's
    # `cargo test --workspace` reaches none of them.
    local dir
    for dir in $(ws_with_role test); do
        run_in "$dir" cargo test
    done
    for dir in $(ws_with_role check); do
        run_in "$dir" cargo check --all-targets
    done
}

# ci.yml `test`, verbatim. The vendored crates are excluded for the same reason
# clippy excludes them, plus one of its own: bevy_replicon's tests and doctests
# do not compile under this workspace's feature unification.
lane_test() {
    lane_target_dir
    run cargo test --workspace \
        --exclude bevy_replicon \
        --exclude aeronet_iroh \
        --exclude aeronet_tokio_runtime
}

# Not a CI lane: CI clears `RUSTC_WRAPPER` for GitHub-hosted runners, which are
# ephemeral and have nothing to hit; the self-hosted jobs put it back. Locally
# kache is a build prerequisite (AGENTS.md
# § Build cache), and `scripts/dev-cache.sh` already knows how to prove it is
# taking effect — including running a build and watching the request count
# move. Delegated rather than reimplemented.
lane_doctor() {
    run scripts/dev-cache.sh doctor
}

readonly LANES=(fmt clippy gates test)

# ── --self-test ─────────────────────────────────────────────────────────────
#
# Two clauses, and both compare the table against a source that is not the
# table. A check that greps this file for its own strings can only pass; the
# haystack is restricted to the table's own lines for exactly the reason
# scripts/p1-swarm-gate.sh restricts its own — every workspace name below also
# appears in the code that looks for it.

# Every directory in this repository whose Cargo.toml declares `[workspace]`.
# Pruned hard: `.claude/worktrees/` holds full checkouts of this same repo in
# the main clone, and descending into them would find eight workspaces per
# worktree.
discovered_workspaces() {
    local manifest dir
    while IFS= read -r manifest; do
        grep -q '^\[workspace\]' "$manifest" || continue
        dir="${manifest%/Cargo.toml}"
        dir="${dir#"$ROOT"/}"
        [[ $dir == "$ROOT" ]] && dir=.
        echo "$dir"
    done < <(
        find "$ROOT" -maxdepth 4 \
            \( -name target -o -name .git -o -name .claude -o -name node_modules \) -prune \
            -o -name Cargo.toml -print
    ) | sort
}

# Every script in `scripts/` that *dispatches* on `--self-test`, as opposed to
# merely mentioning it. The distinction matters both ways: this file's own prose
# says `--self-test` a dozen times, and a script that documented the flag in its
# usage without handling it would be recorded as covered while dying on an
# unknown argument. So the pattern is the dispatch itself — a `[[ $1 == ... ]]`
# comparison or a `case` arm — which is the thing that makes the flag work.
scripts_supporting_self_test() {
    local script
    while IFS= read -r script; do
        grep -qE '(==[[:space:]]*"?--self-test"?|^[[:space:]]*"?--self-test"?\))' "$script" \
            || continue
        echo "scripts/${script##*/}"
    done < <(find "$ROOT/scripts" -maxdepth 1 -name '*.sh' | sort)
}

# The self-tests `lane_gates` actually invokes, read out of its body. Scoped to
# that function for the usual reason — the rest of this file is full of the
# strings it is looking for, and a whole-file grep would find this very
# expression and report the coverage complete.
lane_gates_self_tests() {
    sed -n '/^lane_gates() {/,/^}/p' "$0" \
        | grep -v '^[[:space:]]*#' \
        | sed -n 's|^[[:space:]]*run \(scripts/[a-z0-9.-]*\.sh\) --self-test.*|\1|p' \
        | sort -u
}

self_test() {
    # The table's own lines, and nothing else in this file.
    local table
    table="$(sed -n '/^readonly WORKSPACES=(/,/^)$/p' "$0" | grep -v '^[[:space:]]*#')"
    [[ -n $table ]] || die 'self-test: the lane table could not be located in this script'

    local declared discovered
    declared="$(sed -e "s/^ *'//" -e "s/'.*$//" -e 's/ .*$//' <<<"$table" \
        | grep -v '^readonly' | grep -v '^)$' | grep -v '^$' | sort)"
    discovered="$(discovered_workspaces)"

    local missing extra
    missing="$(comm -13 <(echo "$declared") <(echo "$discovered"))"
    extra="$(comm -23 <(echo "$declared") <(echo "$discovered"))"

    if [[ -n $missing ]]; then
        note 'self-test: these directories declare [workspace] and are not in the lane table:'
        sed 's/^/  /' <<<"$missing" >&2
        die 'self-test: a workspace no lane visits is a workspace nothing checks'
    fi
    if [[ -n $extra ]]; then
        note 'self-test: the lane table names directories that are not workspaces:'
        sed 's/^/  /' <<<"$extra" >&2
        die 'self-test: the lane table has drifted from the tree'
    fi
    note "self-test: all $(wc -l <<<"$discovered") workspaces appear in the lane table"

    # Every table entry carries a role a lane knows how to run.
    local entry role
    for entry in "${WORKSPACES[@]}"; do
        role="${entry##* }"
        case "$role" in
            root | test | check) ;;
            *) die "self-test: '${entry%% *}' has unknown role '$role'" ;;
        esac
    done

    # And ci.yml still delegates rather than re-inlining. The workflow is the
    # second source here; if a job grows a cargo body of its own again, the
    # local reproduction quietly stops being one.
    local wf="$ROOT/.github/workflows/ci.yml"
    if [[ -f $wf ]]; then
        local lane
        for lane in "${LANES[@]}"; do
            grep -Fq "scripts/check.sh $lane" "$wf" \
                || die "self-test: ci.yml does not invoke 'scripts/check.sh $lane'"
        done
        note "self-test: ci.yml delegates all ${#LANES[@]} per-commit jobs"
    else
        note 'self-test: no .github/workflows/ci.yml here; skipped the delegation clause'
    fi

    # ── Coverage: every --self-test in scripts/ is invoked by a lane ──────────
    #
    # The defect this exists for: `scripts/p4-accumulate.sh` and
    # `scripts/p4-ledger.sh` both grew a `--self-test` and neither was ever run
    # per commit, so a portability bug in one of them reached the nightly and
    # failed the macOS leg on every run for weeks. Writing a self-test nobody
    # calls is the easy mistake — nothing about it looks wrong — so the check
    # has to be structural rather than a list somebody remembers to update.
    #
    # The two sides come from sources that cannot drift together: one is a
    # `find` over `scripts/`, the other is the text of `lane_gates` above. A
    # clause that read both out of one array would pass on a script nothing
    # runs, which is precisely the state being fixed.
    local supported invoked uncovered phantom
    supported="$(scripts_supporting_self_test)"
    invoked="$(lane_gates_self_tests)"
    [[ -n $invoked ]] || die 'self-test: no --self-test invocations found in lane_gates; the parse has drifted'

    uncovered="$(comm -23 <(echo "$supported") <(echo "$invoked"))"
    phantom="$(comm -13 <(echo "$supported") <(echo "$invoked"))"
    if [[ -n $uncovered ]]; then
        note 'self-test: these scripts accept --self-test and no lane runs it:'
        sed 's/^/  /' <<<"$uncovered" >&2
        die 'self-test: a self-test nothing invokes is not a check, it is a comment'
    fi
    if [[ -n $phantom ]]; then
        note 'self-test: lane_gates runs --self-test on scripts that do not accept it:'
        sed 's/^/  /' <<<"$phantom" >&2
        die 'self-test: the gates lane would fail on an unrecognized argument'
    fi
    note "self-test: all $(wc -l <<<"$supported") self-tests in scripts/ run in the gates lane"

    echo "$NAME: self-test passed"
}

# ── Entry point ─────────────────────────────────────────────────────────────

usage() {
    sed -n '2,/^set -euo/p' "$0" | sed -e 's/^# \{0,1\}//' -e '/^set -euo/d'
}

LANE=''
requested=()
while (($#)); do
    case "$1" in
        --self-test) LANE=self-test; self_test; exit 0 ;;
        --list) DRY_RUN=1 ;;
        --isolate) ISOLATE=1 ;;
        -h | --help) usage; exit 0 ;;
        all) requested+=("${LANES[@]}") ;;
        fmt | clippy | gates | test | doctor) requested+=("$1") ;;
        *) die "unknown argument '$1'; expected one of ${LANES[*]}, all, doctor, --list, --isolate, --self-test" ;;
    esac
    shift
done
((${#requested[@]})) || requested=("${LANES[@]}")

started=$SECONDS
declare -a timings=()
for LANE in "${requested[@]}"; do
    lane_started=$SECONDS
    "lane_$LANE"
    timings+=("$LANE $((SECONDS - lane_started))s")
done

if ((DRY_RUN)); then
    note 'listed only; nothing was run'
    exit 0
fi

note '─────────────────────────────────────────'
for t in "${timings[@]}"; do note "$t"; done
note "total $((SECONDS - started))s"
echo "$NAME: ${requested[*]} passed"
