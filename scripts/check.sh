#!/usr/bin/env bash
# One definition of the per-commit checks, runnable locally and by CI.
#
#   ./scripts/check.sh              every lane, in CI's order
#   ./scripts/check.sh fmt          rustfmt over all twelve workspaces
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
# gates/p0-nat-test, gates/p0-dashboard, gates/p1-swarm, gates/p2-dashboard, gates/p2-load, gates/p3-island and
# gates/p4-streams-bench — had never been rustfmt-checked by anything. Exactly one
# of the seven was dirty when the lane was widened (gates/p0-nat-test).
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
#   test   a standalone tool with tests of its own: `cargo test`. Every test
#          they carry is a test `cargo test --workspace` at the root runs zero
#          of, which is the whole reason this loop exists. Deliberately not a
#          count: the previous wording said "four of them, carrying 87 tests"
#          and there were five before #129 added a sixth, so the sentence was
#          wrong in a way nothing could catch. `gates/p4-streams-bench` is a measurement
#          rather than a gate — its figures are in its README and the
#          channel-policy decision they justify is in docs/02-networking.md §7 —
#          so what CI owes it is that it still builds and still self-tests.
#          `gates/p2-journal-bench` is `check` for a second reason worth stating: its
#          RocksDB arm is behind a non-default feature because `librocksdb-sys`
#          compiles RocksDB from C++ source, minutes per cold build. The lane
#          checks the default (fjall) build only, so adding this comparison
#          costs CI seconds rather than minutes, and the RocksDB arm is built
#          by hand when someone is running the measurement.
#   check  a standalone tool with no tests at all: `cargo check --all-targets`.
#          `cargo test` on one of these would be a build dressed up as a gate,
#          which reads as coverage that does not exist. The two p0 tools are
#          asserted by the NAT lab they were written for.
#
#          `gates/p3-island` sat here until #129, on the stated grounds that the
#          nightly island gate asserts its behaviour instead. The gate does
#          assert the *harness*; what it cannot assert is the harness's own
#          unit tests, and by then there were two of them — including the one
#          pinning the wire name of the counter the parked half of the P3
#          criterion is read from — that nothing in this repository ran. The
#          role was right when it was written and quietly wrong for months
#          after, which is this file's own §"a `--self-test` nothing runs is
#          not a check" with the roles swapped. A tool grows tests; the table
#          has to notice.
#
# The three vendored crates are members of the root workspace rather than
# workspaces of their own, so they are not listed — and note the consequence
# for `fmt`: `cargo fmt --all` at the root reaches `vendor/`, holding third-
# party code to default rustfmt even though clippy deliberately excludes it.
# That is the status quo, not something this table introduced.
readonly WORKSPACES=(
    '.               root'
    'gates/p0-nat-test     check'
    'gates/p0-dashboard    check'
    'gates/p1-swarm        test'
    'gates/p2-dashboard    test'
    'gates/p2-load         test'
    'gates/p3-island       test'
    'gates/p3-siblings     test'
    'gates/p5-dupe-gauntlet test'
    'gates/p4-streams-bench test'
    'gates/p2-journal-bench check'
    'gates/migration-bench  check'
    'clients/regolith test'
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

# Run one named test behind a wall-clock bound. `timeout` exits 124 when the
# command reaches the bound; translating that otherwise anonymous status here
# is what makes the CI failure identify the test instead of leaving the job's
# outer timeout to kill an undifferentiated `cargo test` process.
run_bounded_test() {
    local bound="$1" test_name="$2"; shift 2
    printf '%s: [%s] (root) $ timeout --kill-after=5s %s %s\n' \
        "$NAME" "$LANE" "$bound" "$*" >&2
    (( DRY_RUN )) && return 0

    local status=0
    (cd "$ROOT" && timeout --kill-after=5s "$bound" "$@") || status=$?
    if (( status == 124 )); then
        note "test timed out after $bound: $test_name"
    fi
    return "$status"
}

# Build one package's library test binary and echo its path. The build is
# deliberately *outside* any wall-clock bound, and that is the whole point of
# the function.
#
# `cargo test -p <pkg> --lib` unifies features differently from the workspace
# invocation `lane_test` runs before it, so cargo rebuilds the dependency graph
# for it rather than reusing what the workspace build produced. On a
# GitHub-hosted runner that rebuild is over a minute on its own, and the `test`
# job caches no `target/`. So a bound wrapped around `cargo test` measures the
# compiler and not the test — which is exactly what happened: both runs that
# reported #292's guard firing spent the entire sixty seconds compiling (the
# last line before each timeout was `Compiling prost v0.14.4`), never linked
# the binary, and still printed a diagnostic naming a test that had not
# started (#293).
#
# Handing `run_bounded_test` a path closes that by construction: there is no
# build tool inside the bound left to measure.
build_test_binary() {
    local package="$1"
    printf '%s: [%s] (root) $ cargo test -p %s --lib --no-run\n' \
        "$NAME" "$LANE" "$package" >&2
    if (( DRY_RUN )); then
        printf '<%s library test binary>\n' "$package"
        return 0
    fi

    local artifacts path
    artifacts="$(cd "$ROOT" && cargo test -p "$package" --lib --no-run \
        --message-format=json-render-diagnostics)" || return 1
    # Every non-test artifact carries the unquoted `"executable":null`, which
    # this pattern cannot match, so the only hit is the test binary itself.
    path="$(grep -o '"executable":"[^"]*"' <<<"$artifacts" | tail -1 | cut -d'"' -f4)"
    [[ -x $path ]] || return 1
    printf '%s\n' "$path"
}

# `CARGO_TARGET_DIR` is deliberately never exported unconditionally. Locally,
# an agent harness sets `CARGO_TARGET_DIR` per task to keep concurrent
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

    # D19 keeps Fjall as a mutually exclusive fallback. The workspace command
    # exercises the default raw backend; this invocation prevents the fallback
    # from becoming compile-only archaeology behind an unvisited feature.
    run cargo clippy -p orrery_persistd --all-targets --no-deps \
        --no-default-features --features journal-fjall,chain-grpc \
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

    # The clause-link shape check, both halves. ADR-0046 shipped six
    # `[D43](f)`s — a reference-style id with the clause letter glued into
    # what Markdown parses as an inline link destination — after twenty of
    # the same shape had been repaired out of ADR-0049 before merge. The bare
    # invocation scans `docs/` (milliseconds; grep over ~85 files); its
    # self-test runs the same scanner against fixture forests where eight
    # planted defects must fire by name and a good forest — repaired forms,
    # real paths, anchors, refdefs, code spans, fences — must stay clean.
    run scripts/docs-clause-links.sh
    run scripts/docs-clause-links.sh --self-test

    # The phase harnesses need FoundationDB and/or multiple real processes, so
    # the real runs are nightly. Their `--self-test` modes are the per-commit
    # half: they assert the scripts still contain the stages that make them
    # proofs.
    run scripts/p2-kill9-gate.sh --self-test
    run scripts/p3-island-gate.sh --self-test
    run scripts/p3-siblings-gate.sh --self-test
    run scripts/p5-dupe-gauntlet-gate.sh --self-test
    run scripts/p5-honest-trade-measure.sh --self-test
    run scripts/ramp-shadow-gate.sh --self-test
    run scripts/p1-swarm-gate.sh --self-test
    run scripts/fdb-tests.sh --self-test

    # #344's client wrapper. Its real runs happen wherever the Regolith skin's
    # tests run — this lane's tool loop below for Linux, ci.yml's
    # `client-platforms` job for Windows and macOS — and this self-test proves
    # that job's floor assertions still assert on every platform's behalf:
    # vacuous zero-executed passes, thin runs, red-with-passes, logs with no
    # result line at all, and another workspace's healthy suite standing in.
    # Needs no client checkout and no display stack; well under a second.
    run scripts/client-tests.sh --self-test

    # The CI path filter. It decides whether clippy, this lane and the test
    # suite run at all on a pull request, so a wrong "documentation only" here
    # is not a slow build — it is a PR that goes green having checked nothing.
    # The rule cannot live inline in ci.yml because YAML cannot be tested: a
    # condition replaced by `if false` still parses. It lives in a script so
    # this line can exist.
    run scripts/ci-changed-code.sh --self-test

    # #476's read-only before/after instrument for the relay migration. Its
    # fixtures prove that the clean-start/QAD-on-loopback trap fails by its
    # named verdict, that an expiring served certificate fails, and that an
    # unavailable probe is UNKNOWN rather than a vacuous pass.
    run scripts/relay-preflight.sh --self-test

    # #486's admission-box preflight. Its synthetic releases prove a matching
    # pin passes, an unmatched pin fails by its campaign's named verdict, and
    # both an unavailable GitHub probe and an empty listing are UNKNOWN rather
    # than a pass that learned nothing.
    run scripts/campaign-release-preflight.sh --self-test

    # #587's built-client/deployed-service preflight. The live run belongs to
    # package-client's Linux leg; these fixtures prove that a green process
    # with no seated client, and a trio with only one seated client, both fail
    # their named checks. It also hides the third craft from the first two and
    # requires both directed third-seat checks to fail by name.
    run scripts/client-campaign-preflight.sh --self-test

    # #774's packaging smoke, which exercises the archive rather than the
    # build. Its live runs belong to package-client's three matrix legs; these
    # fixtures build real archives around a stand-in client and prove that
    # #768's two shapes (an internal `stage/` prefix, a Windows binary without
    # its `.exe`), a README naming a file the archive lacks, a digest that does
    # not check, and #766's CWD-relative artifact path each fail by name — the
    # last of them while the client still exits 0, which is why an exit-status
    # check would not have caught it. Needs tar, a zip tool and python3; a few
    # seconds.
    run scripts/package-artifact-smoke.sh --self-test

    # #478's admission service. Its suite was never run here at all, so the
    # thirteen tests #478 landed had never executed in CI — and when the
    # identity binaries are absent the suite skips every one of them and still
    # exits 0, which reads as a pass that learned nothing. Build them first so
    # the skip cannot happen silently.
    run cargo build --quiet -p orrery_identity --bins
    run python3 scripts/admission.py --self-test

    # #474's standing-host supervisor. Its process test proves a failed
    # harness child is reaped before a fresh attempt starts, with a separate
    # report directory; the real binary is deliberately not needed here.
    run python3 scripts/p1-swarm-always-on.py --self-test

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
    # #387's host-side assembly seam for human sessions: same cost profile as
    # the two above (jq only, sub-second), so it runs per commit with them.
    run scripts/p4-campaign-session.sh --self-test
    # #572's attempt and accounting contract, which lands before the cohort
    # hours it accounts for. Its fixtures are the two failure modes a
    # multi-human attempt introduces — one interval banked twice, and a row
    # bound to the wrong seat or the wrong platform — and the last of them runs
    # the derived rows through the real `p4-ledger.sh append`. openssl and jq
    # only; about two seconds.
    run python3 scripts/p4-attempt-accounting.py --self-test

    # #173's compute-role smoke. Its real run needs AWS credentials and
    # happens nightly (nightly.yml `compute-identity-smoke`); this structural
    # half runs per-commit against the checkout alone and asserts the
    # Terraform still says what the nightly probes enforce at runtime — trust
    # conditions pinned, pull_request absent, the tag chain intact, no grant
    # outside EC2 — and that the workflow plumbing still reaches the script.
    run scripts/aws-compute-smoke.sh --self-test

    # The gate reporter's own. It is the script that says which gates ran and
    # what they measured, so a report that has quietly stopped discovering a
    # gate is exactly as bad as a gate that has quietly stopped failing. Its
    # self-test runs nothing and takes about a second.
    run scripts/gate-status.sh --self-test

    # #332's provenance guard over `assets/`: every asset file manifested with
    # an allowlisted, redistribution-permitting licence; every entry naming an
    # existing file whose bytes still match its recorded sha256; no loadable
    # model outside the managed root; the weight ceilings enforced. Its
    # self-test breaks one thing per synthetic forest — both directions
    # included — against the same check the live invocation runs, then proves
    # the committed tree passes. Needs nothing but python3 (3.11+, stdlib
    # tomllib) and coreutils; about a second.
    run scripts/asset-provenance.sh --self-test

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

    # docs/08 §2.2.5's, for the same reason and on the same terms: its ten
    # interleaved gate runs reduce to a 19 KB file in the tree, so the section
    # is re-derivable from a clean checkout. What its self-test holds is the
    # part an edit could quietly invert — that the pre and post GRV populations
    # are still *disjoint*, still in that direction, and still disjoint with
    # the one device-divergent pair dropped — plus the durability properties
    # the ten runs exist to exercise at all.
    run scripts/p2-locate-removal-report.py --self-test

    # docs/08 §2.2.6's. Its data is 30 metric records from one gate run, in
    # the tree at 8 KB. What its self-test holds is the section's conclusion
    # rather than its decimals: that the router still dominates the served
    # span, that everything above it is small, that both peer-state lock
    # acquisitions are still free, and that the stage identity is exact. A
    # decomposition whose stages stopped adding up to the span they decompose
    # would be an instrument measuring nothing, and it would say so quietly.
    run scripts/p2-lease-stage-report.py --self-test

    # docs/08 §2.2.7's. Its self-test holds the section's *hedges* as much as
    # its numbers: that exactly one run passed `intent_commit_ms` and that not
    # every post run did, because the section's caution is a claim about this
    # data and a later edit that made it read like a passing gate would be
    # wrong in a direction no number would catch.
    run scripts/p2-intent-fence-report.py --self-test

    # docs/08 §4.5's. Its self-test pins a *null* and the reason the null is
    # uninformative rather than negative -- that on the reference box the bare
    # barrier's own tail is the size of the effect. A negative decays quietly
    # and a null decays quieter still, so the check asserts the arms are still
    # tied and the ranges still overlap, not merely that some number is stable.
    run scripts/p2-evidence-split-report.py --self-test

    # docs/08 §4.4's. That section is the only one in this file whose argument
    # is a *negative* — a barrier 40x better than §4.3's bought no p99 — and a
    # negative decays quietly: every link of its elimination chain has to keep
    # holding for the conclusion to mean anything, and none of them is visible
    # in the headline. So the self-test pins the chain, not the number: the
    # device cleared at the gate's own barrier shape, the filesystem cleared
    # under saturation, CPU cleared by pressure and run queue, and writeback
    # still reproducing the stall while leaving p99.9 alone. It also pins the
    # two hedges — that the engines are not separable and that
    # `intent_commit_ms` passes in some runs and not all — because an edit that
    # turned either into a clean result would be wrong in a direction no
    # aggregate would catch.
    run scripts/p2-nvme-report.py --self-test

    # docs/08 §4.6's, and it guards the opposite risk from §4.4's. That section
    # attributed the journal's stalls to writeback and this one removes every
    # co-tenant in turn — the harness's evidence, then FoundationDB — and finds
    # the stall still there on two filesystems. Its claims are therefore all
    # *negatives*, and a negative is what a well-meaning data edit quietly
    # turns into a positive: make one arm come out clean and the section reads
    # as a fix rather than an elimination. So the self-test pins each arm still
    # stalling, the per-run `df` proof that each layout was actually in effect,
    # and the one comparison that carries the filesystem half — that xfs is far
    # more writeback-resistant at the device and stalls the gate anyway.
    run scripts/p2-nvme-isolation-report.py --self-test

    # docs/08 §4.7's, which ends the sequence §4.3 started by naming a cause
    # rather than eliminating one. Three of its clauses are load-bearing in
    # ways a later edit could quietly reverse: the worst barrier carrying an
    # *ordinary* batch (which is what refutes the volume story), the tmpfs arm
    # stalling at all (which is what removes storage), and the 256 MiB point
    # reading clean at 60 s while stalling at 180 s (which is what stops the
    # section claiming a fix it does not have). Losing any one of them leaves
    # the conclusion standing on nothing, so all three are pinned.
    run scripts/p2-barrier-shape-report.py --self-test

    # docs/08 §4.8's. That section compares three stores, and the risk it runs
    # is the opposite of §4.7's: a comparison is the easiest thing to flatter.
    # So the self-test pins the two controls that make it a comparison at all --
    # every arm's barrier collapsing without its fsync, and the arms having
    # written comparable bytes -- alongside the asymmetry itself and the two
    # honest halves the section would be wrong without: that RocksDB and wal-db
    # do still stall on a real device, and that wal-db's smaller on-disk
    # footprint is recorded, because that gap IS its no-index caveat.
    run scripts/p2-journal-store-report.py --self-test

    # The journal-raw spike's indexed, gate-level comparison. Its self-test
    # pins the qualified device, five alternating pairs, every crash/recovery
    # proof, tail mass and on-disk work; it also mutation-checks each class of
    # guarded fact so a vacuous clause cannot bless a flattering edit.
    run scripts/p2-journal-raw-report.py --self-test

    # D20's journal-open curve, which is the evidence that retention had to
    # exist. Its self-test holds the shape the ADR's argument reads from --
    # equally spaced steps of one journal grown in place, a fit that stays
    # linear to within 5%, a sweep that actually reaches the D16 budget it
    # motivates -- and the two disclosures the number is worthless without:
    # that the page cache was warm, so the slope is a floor, and which device
    # the journal was on. The uptime extrapolation reads its arrival rate from
    # the D19 gate evidence beside it, so a change to one cannot silently
    # leave the other's conclusion standing.
    run scripts/p2-journal-open-report.py --self-test

    # D32 clause (e)'s promotion evidence, over the artifact the shadow arm's
    # meter writes (`docs/data/ramp-shadow-*.json`, 40 KB, in the tree). Its
    # self-test is two halves. The first mutation-checks seventeen guarded
    # facts about the artifact — schema, the five-control inventory, and every
    # arithmetic relation the artifact's own counters owe each other, because a
    # reader that cannot tell a coherent artifact from an incoherent one is not
    # reading it. The second is functional and is the reason this is a
    # per-commit line rather than a nightly one: it renders `0 of 10 000` and
    # `0 of 0` and asserts they do not come out looking alike. A ramp report
    # that cannot distinguish those is not a weaker report, it is the specific
    # failure D32 names — "a false-positive rate of 0 over a cohort nobody
    # watched is not evidence, it is blindness with a clean conscience" — and
    # it would read as a control ready to promote.
    run scripts/ramp-report.py --self-test

    # The reclaimer (#781) runs `rm -rf` over 100 GiB-class worktree
    # directories with no human in the loop, on the SessionEnd hook. Its
    # self-test is functional in both directions and pins the two gates that
    # make that defensible: that a squash-merged lane is recognised as landed
    # (an ancestry test would report every landed lane as unmerged, and reclaim
    # nothing), and that liveness is read from /proc/<pid>/cwd rather than
    # grepped out of a command line — the negative case is a live `cargo` whose
    # argv names the tree and whose working directory is elsewhere, which
    # `pgrep -f` finds and the reclaimer must not.
    run scripts/dev-cache.sh --self-test

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
    local torn_tail_test='journal::raw::tests::a_torn_final_frame_recovers_the_last_intact_record'
    run cargo test --workspace \
        --exclude bevy_replicon \
        --exclude aeronet_iroh \
        --exclude aeronet_tokio_runtime \
        -- --skip "$torn_tail_test"

    # This proptest normally completes with the rest of persistd's library
    # tests in under three seconds, but it was the sole test still running in
    # both 30-minute workspace-test cancellations (#290). Keep the job timeout
    # as an outer backstop and give this known unbounded wait a local failure
    # that names it. Sixty seconds is more than 20x the two successful reruns'
    # complete persistd-library times (2.32 s and 2.89 s).
    #
    # Two things changed after #293. The test now bounds its own journal
    # closes, so a wedged shutdown handshake fails there, named, in thirty
    # seconds — this bound is the backstop for a wedge somewhere the test does
    # not bound, not the first line of defence any more. And the binary is
    # built *before* the bound rather than inside it: see `build_test_binary`
    # for the two runs where the bound timed out on the compiler without the
    # test ever starting.
    local torn_tail_bin
    torn_tail_bin="$(build_test_binary orrery_persistd)" \
        || die 'test: could not build the orrery_persistd library test binary'
    run_bounded_test 60s "$torn_tail_test" \
        "$torn_tail_bin" "$torn_tail_test" --exact --nocapture

    # The workspace test above is D19's default indexed raw journal. Exercise
    # the retained Fjall implementation's unit tests as well; clippy compiles
    # all fallback targets above. Its full integration suite retains Fjall's
    # known shutdown-hang exposure and is not a second shipping-path gate.
    run cargo test -p orrery_persistd --lib \
        --no-default-features --features journal-fjall,chain-grpc
}

# Not a CI lane: CI clears `RUSTC_WRAPPER`. Locally kache is a build prerequisite (AGENTS.md
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
# Scripts whose `--self-test` is deliberately not per-commit, each with the
# reason it cannot be. Modelled on §2.2.1's allow-lists: an exemption states
# why, and it fails when the thing it exempts stops existing, so it cannot
# outlive its subject and quietly widen.
readonly SELF_TEST_NOT_PER_COMMIT=(
    # Its three checks re-derive docs/08 §2.2.1 from ~10 GB of sweep
    # artifacts that are not version-controlled and not reproducible from a
    # checkout, so there is nothing for a per-commit lane to run them against.
    scripts/intent-tail-derive.py
)

scripts_supporting_self_test() {
    local script
    while IFS= read -r script; do
        # Three idioms, because two languages are in scope: shell's `case`
        # arm and `[[ $1 == --self-test ]]`, and Python's `"--self-test" in
        # argv` -- spelled `sys.argv` or a local rebinding of it, both of
        # which are in this tree. Matching the flag anywhere in the file
        # instead would call every script that merely documents it supported.
        grep -qE '(==[[:space:]]*"?--self-test"?|^[[:space:]]*"?--self-test"?\)|"--self-test"[[:space:]]+in[[:space:]]+(sys\.)?argv)' "$script" \
            || continue
        echo "scripts/${script##*/}"
        # `.py` as well as `.sh`, since 2026-08-19. The clause was written when
        # every self-test was a shell script and it kept looking only at those,
        # so the three Python reporters docs/08 §2.2.2, §2.2.5 and §2.2.6 read
        # their numbers from were outside it: their `--self-test` modes were
        # invoked by the lane and their *registration* was enforced by nobody.
        # A fourth would have been added unregistered and silently unrun, which
        # is the exact failure this clause exists to make loud.
    done < <(find "$ROOT/scripts" -maxdepth 1 \( -name '*.sh' -o -name '*.py' \) | sort)
}

# The self-tests `lane_gates` actually invokes, read out of its body. Scoped to
# that function for the usual reason — the rest of this file is full of the
# strings it is looking for, and a whole-file grep would find this very
# expression and report the coverage complete.
lane_gates_self_tests() {
    sed -n '/^lane_gates() {/,/^}/p' "$0" \
        | grep -v '^[[:space:]]*#' \
        | sed -n 's|^[[:space:]]*run \(scripts/[a-z0-9._-]*\) --self-test.*|\1|p' \
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

    # D19 retains Fjall as a real fallback, not dead source. Both the clippy
    # and test lanes must name its mutually exclusive feature set explicitly;
    # the default workspace commands cover journal-raw.
    grep -Fq 'journal-fjall,chain-grpc' <(sed -n '/^lane_clippy() {/,/^}/p' "$0") \
        || die 'self-test: the clippy lane no longer checks D19 journal-fjall fallback'
    grep -Fq 'journal-fjall,chain-grpc' <(sed -n '/^lane_test() {/,/^}/p' "$0") \
        || die 'self-test: the test lane no longer exercises D19 journal-fjall fallback'
    note 'self-test: both D19 journal backends are covered by clippy and tests'

    # Functional, not structural: exercise the guarded stage with a command
    # that cannot finish inside the bound, then require both timeout's status
    # and the diagnostic a cancelled job was missing. If `timeout` is removed
    # from run_bounded_test, the sleep succeeds and this clause fails.
    local bound_output bound_status=0 mutation_name='self-test-deliberate-timeout'
    bound_output="$(run_bounded_test 0.01s "$mutation_name" sleep 1 2>&1)" \
        || bound_status=$?
    (( bound_status == 124 )) \
        || die "self-test: bounded-test guard returned $bound_status, expected timeout status 124"
    grep -Fq "test timed out after 0.01s: $mutation_name" <<<"$bound_output" \
        || die 'self-test: bounded-test guard did not name the timed-out test'
    note 'self-test: bounded-test guard fires with timeout status 124 and names the test'

    # …and the bound has to measure the *test*, not the toolchain. Both runs
    # that reported the guard firing (#293) had `cargo test` inside the bound:
    # `-p orrery_persistd --lib` unifies features differently from the
    # workspace build above, the `test` job caches no `target/`, so cargo
    # rebuilt the whole graph inside the sixty seconds and the binary was never
    # linked — yet the diagnostic still named a test that had not started.
    #
    # Functional, not structural: this reads the commands the lane would
    # actually issue, and requires the build to be listed before the bound and
    # nothing that builds to appear inside it.
    local listed build_line bound_line bounded_cmd
    listed="$(DRY_RUN=1 lane_test 2>&1)"
    # `|| true` so a missing line reaches the diagnostics below instead of
    # tripping `set -e` on grep's empty-match status with nothing said.
    build_line="$(grep -nF -- '--lib --no-run' <<<"$listed" | head -1 | cut -d: -f1 || true)"
    bound_line="$(grep -nF -- 'timeout --kill-after=5s 60s ' <<<"$listed" | head -1 | cut -d: -f1 || true)"
    [[ -n $bound_line ]] \
        || die 'self-test: the test lane no longer bounds the torn-tail proptest'
    [[ -n $build_line ]] \
        || die 'self-test: the test lane no longer builds the bounded test binary'
    (( build_line < bound_line )) \
        || die 'self-test: the bounded test binary is not built before the bound'
    bounded_cmd="$(sed -n "${bound_line}p" <<<"$listed")"
    bounded_cmd="${bounded_cmd#*timeout --kill-after=5s 60s }"
    bounded_cmd="${bounded_cmd%% *}"
    [[ $bounded_cmd != cargo && $bounded_cmd != */cargo ]] \
        || die "self-test: the bounded test command builds inside the bound ($bounded_cmd)"
    note 'self-test: the bounded test is a prebuilt binary, not a build command'

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
    local exempt
    exempt="$(printf '%s\n' "${SELF_TEST_NOT_PER_COMMIT[@]}" | sort -u)"
    # An exemption for a script that no longer accepts `--self-test` (or no
    # longer exists) is stale, and a stale exemption is how a list like this
    # grows to cover things nobody decided to exempt.
    local stale
    stale="$(comm -13 <(echo "$supported") <(echo "$exempt"))"
    if [[ -n $stale ]]; then
        note 'self-test: these exemptions name scripts that do not accept --self-test:'
        sed 's/^/  /' <<<"$stale" >&2
        die 'self-test: an exemption must not outlive the self-test it exempts'
    fi
    supported="$(comm -23 <(echo "$supported") <(echo "$exempt"))"
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
    note "self-test: all $(wc -l <<<"$supported") per-commit self-tests in scripts/ run in the \
gates lane ($(wc -l <<<"$exempt") exempted)"

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
