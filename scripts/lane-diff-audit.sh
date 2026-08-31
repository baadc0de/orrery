#!/usr/bin/env bash
# Refuse a lane push whose diff reverts merged work, truncates a file, or is cut
# from a stale checkout (#779).
#
#   ./scripts/lane-diff-audit.sh              audit HEAD against origin/main
#   ./scripts/lane-diff-audit.sh --self-test  prove the checks still bite both ways
#
# A lane runs this before pushing. It compares the current branch to the common
# ancestor with origin/main, not to the tip, because the push is what would land
# on main; the diff from the merge-base is what the merge commit will contain.
#
# The five checks are intentionally mechanical and overridable. A lane whose
# real job is deleting a large file or reverting a recent commit can pass
# --waive CHECK; the waiver is printed so it cannot be accidental.
#
# Thresholds were measured on this repository's history, not guessed. The
# measurements are recorded in the PR that introduced this script (#779).
set -euo pipefail

readonly NAME=lane-diff-audit

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
readonly ROOT

die() { echo "$NAME: $*" >&2; exit 2; }
note() { echo "$NAME: $*" >&2; }

# ── Thresholds ──────────────────────────────────────────────────────────────
#
# Each number is chosen to separate the six known incidents from ordinary work
# on origin/main. The distributions live in #779's description; the short form
# is that no ordinary first-parent commit on main has ever reached these values.

# Merge-base distance: how many first-parent commits on origin/main are not in
# the branch's base. Every merged PR in this repository so far had a base within
# 8 commits of main tip (p100 = 8); a stale checkout is tens or hundreds behind.
readonly BASE_DISTANCE_THRESHOLD=20

# Total deletions that exceed additions by enough to look like a bulk revert.
# The largest ordinary commit on main deleted 864 lines (p100); the six known
# incidents start at ~2,100. The ratio threshold is also extreme: among commits
# with both additions and deletions on main, only one in 625 had d/a > 5.
readonly DELETION_SURGE_LINES=1000
readonly DELETION_SURGE_RATIO=5

# A tracked file that keeps existing but loses this fraction of its lines.
# Across all file changes on main, 0.42% reached 0.90; the two truncation
# incidents were 1.00. Whole-file emptying is checked separately and always.
readonly FILE_TRUNCATION_FRACTION=0.90

# A deletion-only block this long is worth checking for a merged-source revert.
# Smaller hunks are too noisy to attribute confidently.
readonly REVERT_HUNK_MIN_LINES=3

# The branch the audit treats as "main". Overridden only by the self-test,
# which builds a synthetic repository where there is no origin/main yet.
MAIN_REF="${LANE_DIFF_AUDIT_MAIN:-origin/main}"

# The checkout containing the branch under audit. Capture it once instead of
# letting individual git commands inherit whichever directory the script or a
# helper happens to be in. The self-test overrides this by invoking a new copy
# of the script from its synthetic repository.
AUDIT_REPO="${LANE_DIFF_AUDIT_REPO:-$PWD}"
readonly AUDIT_REPO

# ── Override state ───────────────────────────────────────────────────────────

WAIVE_FILE_TRUNC=0
WAIVE_STALE_BASE=0
WAIVE_REVERT=0
WAIVE_DELETION_SURGE=0
WAIVE_MUTATION_COMMIT=0

# ── Usage ────────────────────────────────────────────────────────────────────

usage() {
    sed -n '2,/^set -euo/p' "$0" | sed -e 's/^# \{0,1\}//' -e '/^set -euo/d' >&2
    cat >&2 <<'USAGE'

Options:
  --self-test                   run the synthetic mutation checks and exit
  --waive file-truncation       do not fail on file truncation / emptying
  --waive stale-base            do not fail on a merge-base far behind main
  --waive revert-hunk           do not fail on reverts of merged hunks
  --waive deletion-surge        do not fail on deletions exceeding additions
  --waive mutation-commit       do not fail on an unreverted mutation commit
  --waive-all                   waive all five checks
  -h, --help                    show this help and exit

A waiver is printed to stderr exactly once per check that would have fired.
USAGE
}

# ── Helper: run git in the repository under audit ────────────────────────────

git_() { git -C "$AUDIT_REPO" "$@"; }

# ── Check 1: tracked file truncated to empty or nearly empty ─────────────────
#
# Compares the branch tree against the merge-base tree: what a squash merge
# would land. A file that is simply deleted in the branch is reported by the
# deletion-surge check, not here; only files that still exist in HEAD but have
# been gutted are flagged.

check_file_truncation() {
    local left_ref="$1"
    local file_status file

    while IFS=$'\t' read -r status file new_path; do
        [[ -n $status ]] || continue
        # Renames are listed as "R100\told\tnew"; take the new path.
        if [[ $status == R* ]]; then
            file="$new_path"
        fi

        # We only care about files that still exist in HEAD.
        case "$status" in
            A|D|T) continue ;;
        esac

        # Skip binary files: line counts are meaningless.
        if git_ diff --numstat "$left_ref" HEAD -- "$file" 2>/dev/null \
            | awk -v f="$file" '$3 == f { exit ($1 == "-" || $2 == "-") ? 0 : 1 }'; then
            continue
        fi

        local base_lines head_lines
        base_lines=$(git_ show "$left_ref:$file" 2>/dev/null | wc -l) || base_lines=0
        head_lines=$(git_ show "HEAD:$file" 2>/dev/null | wc -l) || head_lines=0

        if (( base_lines > 0 && head_lines == 0 )); then
            printf 'file-truncation\tempty\t%s\tbase=%s\n' "$file" "$base_lines"
        elif (( base_lines > 0 )); then
            local removed=$((base_lines - head_lines))
            if (( removed > 0 )); then
                local frac
                frac=$(awk "BEGIN { printf \"%.4f\", $removed / $base_lines }")
                if awk "BEGIN { exit ($frac >= $FILE_TRUNCATION_FRACTION) ? 0 : 1 }"; then
                    printf 'file-truncation\tfrac=%s\t%s\tremoved=%s/%s\n' \
                        "$frac" "$file" "$removed" "$base_lines"
                fi
            fi
        fi
    done < <(git_ diff --name-status --find-renames "$left_ref" HEAD)
}

# ── Check 2: merge-base far behind main ──────────────────────────────────────

check_stale_base() {
    local _base="$1"
    local distance
    distance=$(git_ rev-list --count --first-parent "$_base..$MAIN_REF")
    if (( distance > BASE_DISTANCE_THRESHOLD )); then
        printf 'stale-base\tdistance=%s\n' "$distance"
    fi
}

# ── Check 4: deletions far exceed additions ──────────────────────────────────
#
# Numbered four because check three is the revert-hunk detector in the Python
# helper below. The order here follows the order they are reported.

check_deletion_surge() {
    local left_ref="$1"
    local total_del=0 total_ins=0

    while IFS=$'\t' read -r added deleted _file; do
        [[ -n $added ]] || continue
        [[ $added == '-' || $deleted == '-' ]] && continue
        total_ins=$((total_ins + added))
        total_del=$((total_del + deleted))
    done < <(git_ diff --numstat --find-renames "$left_ref" HEAD)

    if (( total_del > DELETION_SURGE_LINES )); then
        local ratio_label="inf"
        if (( total_ins > 0 )); then
            ratio_label=$(awk "BEGIN { printf \"%.2f\", $total_del / $total_ins }")
            if awk "BEGIN { exit ($total_del > $DELETION_SURGE_RATIO * $total_ins) ? 0 : 1 }"; then
                printf 'deletion-surge\tdel=%s\tins=%s\tratio=%s\n' \
                    "$total_del" "$total_ins" "$ratio_label"
            fi
        else
            printf 'deletion-surge\tdel=%s\tins=%s\tratio=%s\n' \
                "$total_del" "$total_ins" "$ratio_label"
        fi
    fi
}

# ── Check 3: deletion-only hunks that revert merged additions ────────────────
#
# Implemented in Python because parsing unified diff and running pickaxe searches
# is exactly the kind of structured work bash is bad at. The helper is fed the
# left, base, and main refs as arguments and prints tab-separated violation
# records.

check_revert_hunks() {
    local left_ref="$1"
    local _base="$2"
    LANE_DIFF_AUDIT_REPO="$AUDIT_REPO" python3 - "$left_ref" "$_base" "$MAIN_REF" <<'PYEOF'
import os
import subprocess
import sys

REPO = os.environ["LANE_DIFF_AUDIT_REPO"]
LEFT = sys.argv[1]
BASE = sys.argv[2]
MAIN = sys.argv[3]
MIN_BLOCK = int(os.environ.get("REVERT_HUNK_MIN_LINES", "3"))


def run(args, check=True):
    r = subprocess.run(args, cwd=REPO, capture_output=True, text=True)
    if check and r.returncode not in (0, 1):
        sys.stderr.write(f"git failed: {' '.join(args)}\n{r.stderr}\n")
        sys.exit(2)
    return r.stdout


def file_exists_in(ref, path):
    r = subprocess.run(
        ["git", "cat-file", "-e", f"{ref}:{path}"],
        cwd=REPO, capture_output=True, text=True,
    )
    return r.returncode == 0


def content_lines(ref, path):
    return run(["git", "show", f"{ref}:{path}"]).splitlines()


def deletion_blocks(diff_text):
    """Yield (path, [deleted lines]) for contiguous deletion-only blocks.

    A deletion run that additions immediately follow inside the same hunk is a
    replacement: the branch rewrote the content in place. That is the shape of
    an ordinary refactor, which must pass. The #742 sweep deletes regions with
    nothing written back over them, and that is the shape this yields.
    """
    current_path = None
    in_hunk = False
    block = []

    def flush():
        nonlocal block
        if block and len(block) >= MIN_BLOCK and current_path is not None:
            yield (current_path, block)
        block = []

    for raw in diff_text.splitlines():
        if raw.startswith("diff --git "):
            yield from flush()
            current_path = None
            in_hunk = False
        elif raw.startswith("deleted file mode"):
            current_path = None  # whole-file deletions are handled elsewhere
        elif raw.startswith("+++ b/"):
            current_path = raw[6:]
            yield from flush()
            in_hunk = False
        elif raw.startswith("@@"):
            yield from flush()
            in_hunk = True
        elif in_hunk:
            if raw.startswith("-") and not raw.startswith("---"):
                block.append(raw[1:])
            elif raw.startswith("+") and not raw.startswith("+++"):
                block = []  # a replacement, not a revert: drop the deletion run
            else:
                yield from flush()
        else:
            pass
    yield from flush()


def good_sample(lines):
    """Prefer non-trivial lines so pickaxe does not match generic noise."""
    candidates = [ln for ln in lines if len(ln.strip()) >= 4]
    if not candidates:
        candidates = [ln for ln in lines if ln.strip()]
    if not candidates:
        return None
    # first, middle, last
    for idx in (0, len(candidates) // 2, -1):
        if 0 <= idx < len(candidates):
            return candidates[idx]
    return candidates[0]


def merged_adding_commits(line, path):
    """Commits at or below BASE whose diff changed the count of this line.

    The audited deletions come from the BASE..HEAD diff, so a deleted line was
    present in BASE's tree: the commit that introduced it is reachable from
    BASE, and searching BASE..MAIN can never attribute anything — a line added
    after BASE is not in BASE's tree, so the branch cannot be deleting it.
    Pairing a BASE..HEAD diff with a BASE..MAIN pickaxe made the check silently
    stop firing (#801). We deliberately do *not* pass --diff-filter=A: that
    filter restricts to commits that created the file, not commits that added a
    line inside an existing file. If the line is still present on main, the
    most recent commit at or below BASE that changed its count is the one that
    (re-)introduced it.
    """
    out = run(
        [
            "git", "log", "-S", line,
            "--format=%H", BASE,
            "--", path,
        ],
        check=False,
    )
    return [c for c in out.splitlines() if c]


def line_present_on_main(line, path):
    if not file_exists_in(MAIN, path):
        return False
    # git show is cheaper than repeated pickaxe for the present check.
    return line in content_lines(MAIN, path)


def main():
    diff_text = run(["git", "diff", "-p", "--find-renames", LEFT, "HEAD"])
    seen = set()
    for path, block in deletion_blocks(diff_text):
        if not file_exists_in(MAIN, path):
            continue
        sample = good_sample(block)
        if sample is None:
            continue
        commits = merged_adding_commits(sample, path)
        if not commits:
            continue
        if not line_present_on_main(sample, path):
            continue
        key = (path, commits[0])
        if key in seen:
            continue
        seen.add(key)
        subject = run(
            ["git", "log", "-1", "--format=%s", commits[0]], check=False
        ).strip()
        print(f"revert-hunk\t{path}\tcommits={commits[0]}\t{subject}")


if __name__ == "__main__":
    main()
PYEOF
}

# ── Check 5: deliberate compile-break commit has not been reverted ──────────
#
# Mutation checks deliberately commit a broken tree so CI can prove that a
# guard bites. That is safe only when the branch also contains the genuine
# `git revert` commit. Detect the intent from the commit subject (rather than a
# single fixture string), then pair each candidate with Git's unambiguous
# "This reverts commit <sha>." trailer in a later branch-only commit.

check_mutation_commits() {
    LANE_DIFF_AUDIT_REPO="$AUDIT_REPO" python3 - "$MAIN_REF" <<'PYEOF'
import os
import re
import subprocess
import sys

REPO = os.environ["LANE_DIFF_AUDIT_REPO"]
MAIN = sys.argv[1]

# A mutation commit must say that it is *breaking* something, not merely that
# it exercises a mutation check. The load-bearing signal is a break verb aimed
# at a build, test, or CI target ("break the compile", "break CI"), an explicit
# "deliberately break", or a mutation/fault/failure check whose subject also
# names the break ("for CI", "breaks the parser"). That is what separates
# "test: break regolith compile for CI mutation check" (a deliberate break)
# from "test: mutation-check every criterion" (a test that mutates fixtures)
# and "fix: repair broken CI" (a fix). The word "broken" is deliberately not
# a break verb, so an ordinary repair never trips this.
#
# The break verb must be the subject's *action*, i.e. the imperative that
# conventional-commit subjects open with after the optional `type(scope):`
# prefix. Matching "break" anywhere alongside a build word is too loose: it
# flags "fix(lane-diff-audit): a break verb, not a bare mutation-check
# mention", where "break" is a noun inside prose about the check itself.
# That subject is this very commit, and an earlier draft of this pattern
# refused it.
SUBJECT_ACTION = r"^(?:[a-z]+(?:\([^)]*\))?!?:\s*)?(?:deliberately\s+|intentionally\s+|purposefully\s+)?"
MUTATION_SUBJECT = re.compile(
    r"(?:"
    + SUBJECT_ACTION + r"break(?:s|ing)?\b.*\b(?:compile|build|ci|check|test|guard)\b"
    r"|\b(?:deliberately|intentionally|purposefully)\s+\w*break\w*\b"
    r"|\b(?:mutation|fault|failure)[ -]?(?:check|test|injection)\b.*"
    r"(?:\bbreak(?:s|ing)?\b|\bfor\s+(?:the\s+)?ci\b)"
    r")",
    re.IGNORECASE,
)
REVERT_TRAILER = re.compile(r"^This reverts commit ([0-9a-f]{40})\.$", re.MULTILINE)


def run(args):
    return subprocess.run(
        ["git", "-C", REPO, *args], check=True, capture_output=True, text=True
    ).stdout


def main():
    records = run(["log", "--format=%H%x00%s%x00%B%x00", f"{MAIN}..HEAD"]).split("\x00")
    candidates = {}
    reverted = set()

    for index in range(0, len(records) - 1, 3):
        commit, subject, body = records[index : index + 3]
        commit = commit.strip()
        subject = subject.strip()
        if not commit:
            continue
        reverted.update(REVERT_TRAILER.findall(body))
        if not subject.lower().startswith("revert ") and MUTATION_SUBJECT.search(subject):
            candidates[commit] = subject

    for commit, subject in candidates.items():
        if commit not in reverted:
            print(f"mutation-commit\tcommit={commit}\tsubject={subject}")


if __name__ == "__main__":
    main()
PYEOF
}

# ── Collect and report violations ────────────────────────────────────────────

collect_violations() {
    local left_ref="$1"
    local base_for_stale="$2"
    check_file_truncation "$left_ref"
    check_stale_base "$base_for_stale"
    check_deletion_surge "$left_ref"
    check_revert_hunks "$left_ref" "$base_for_stale"
    check_mutation_commits
}

report_violation() {
    local kind="$1"
    local detail="$2"
    case "$kind" in
        file-truncation)
            if (( WAIVE_FILE_TRUNC )); then
                note "WAIVED file-truncation: $detail"
                return 0
            fi
            note "FAIL file-truncation: $detail (threshold empty or >= ${FILE_TRUNCATION_FRACTION})"
            ;;
        stale-base)
            if (( WAIVE_STALE_BASE )); then
                note "WAIVED stale-base: $detail"
                return 0
            fi
            note "FAIL stale-base: $detail (threshold ${BASE_DISTANCE_THRESHOLD})"
            ;;
        deletion-surge)
            if (( WAIVE_DELETION_SURGE )); then
                note "WAIVED deletion-surge: $detail"
                return 0
            fi
            note "FAIL deletion-surge: $detail (threshold del>${DELETION_SURGE_LINES} and ratio>${DELETION_SURGE_RATIO}:1)"
            ;;
        revert-hunk)
            if (( WAIVE_REVERT )); then
                note "WAIVED revert-hunk: $detail"
                return 0
            fi
            note "FAIL revert-hunk: $detail"
            ;;
        mutation-commit)
            if (( WAIVE_MUTATION_COMMIT )); then
                note "WAIVED mutation-commit: $detail"
                return 0
            fi
            note "FAIL mutation-commit: $detail (unreverted deliberate compile-break commit)"
            ;;
        *)
            note "FAIL unknown: $kind $detail"
            ;;
    esac
    return 1
}

# ── Self-test: synthetic repository, both directions ─────────────────────────

self_test() {
    local tmp
    tmp=$(mktemp -d)
    # shellcheck disable=SC2064
    trap "rm -rf '$tmp'" EXIT

    note "self-test: building synthetic repository in $tmp"
    (
        audit_synthetic() {
            # Run from outside the synthetic repository. This pins the target
            # selection independently of both the self-test's cwd and its
            # caller's cwd; changing git_ back to ambient git makes this fail.
            (
                cd /
                LANE_DIFF_AUDIT_REPO="$tmp" LANE_DIFF_AUDIT_MAIN=main \
                    "$ROOT/scripts/lane-diff-audit.sh" "$@"
            )
        }

        cd "$tmp"
        # `-b main` explicitly, because the fixture's assertions name `main~1`
        # and `git init`'s default branch is *ambient*: it is whatever
        # `init.defaultBranch` says, and unset means `master`. Every developer
        # box here sets it, so this self-test passed locally for months and
        # failed the first time it ran on a runner that does not
        # (`static gates`, which is nightly-only since 2026-08-28 — so the gap
        # this closes is exactly the environment-dependent class ci.yml warns
        # that move trades away). The fixture must not read configuration it is
        # not the subject of.
        git init -q -b main
        git config user.email "lane-diff-audit@orrery.local"
        git config user.name "Lane Diff Audit"

        cat > Cargo.toml <<'EOF'
[package]
name = "lane-diff-audit-synthetic"
version = "0.1.0"
edition = "2021"
EOF
        mkdir -p src
        cat > src/lib.rs <<'EOF'
#[allow(dead_code)]
pub struct MutationCheckRecord {
    expected_field: (),
}
EOF
        cat > src/lib.txt <<'EOF'
common line one
common line two
common line three
EOF
        git add .
        git commit -q -m "initial"
        cargo check --quiet

        {
            echo "merged feature A"
            echo "merged feature B"
            echo "merged feature C"
        } >> src/lib.txt
        git add src/lib.txt
        git commit -q -m "merged feature"

        echo "later addition" > src/other.txt
        git add src/other.txt
        git commit -q -m "other file"

        # #742's shape: the branch's own commit deletes already-merged work.
        # The branch is cut from a commit that contains the merged feature;
        # its tree then goes stale (pre-feature content), and `git commit -a`
        # sweeps the difference into the branch as its own deletions. They
        # live in merge-base..HEAD — exactly what a squash merge would land —
        # so the audit must refuse and name the reverted commit. (The branch
        # is cut *at* the feature commit, not before it: a branch can only
        # revert a line its own merge-base already contained.)
        local initial feature_commit
        initial=$(git rev-list --max-parents=0 HEAD)
        feature_commit=$(git rev-parse main~1)
        git checkout -q -b stale-revert "$feature_commit"
        git show "$initial:src/lib.txt" > src/lib.txt
        git commit -q -a -m "stale checkout sweeps the difference as deletions"

        local stale_output
        if stale_output=$(audit_synthetic 2>&1); then
            echo "$NAME: self-test: stale-revert branch was NOT refused" >&2
            exit 1
        elif [[ $stale_output != *"FAIL revert-hunk: src/lib.txt"*"commits=$feature_commit"* ]]; then
            echo "$NAME: self-test: stale-revert refusal did not name revert-hunk for $feature_commit" >&2
            echo "$stale_output" >&2
            exit 1
        fi
        echo "$NAME: self-test: stale-revert branch correctly refused (revert-hunk named $feature_commit)" >&2

        # The merely-behind branch (#801): cut before the merged feature and
        # authoring one change to a file main never touched. It deletes
        # nothing; it simply lacks the feature lines, which main added after
        # the branch point. Diffing against main made that absence look like
        # a revert of the feature commit; the merge-base diff shows only the
        # authored change and must pass without a waiver.
        git checkout -q -b merely-behind "$initial"
        echo "# an unrelated change" >> Cargo.toml
        git add Cargo.toml
        git commit -q -m "chore: unrelated change while main moved on"

        local behind_output behind_distance
        behind_distance=$(git rev-list --count HEAD..main)
        if behind_output=$(audit_synthetic 2>&1); then
            echo "$NAME: self-test: merely-behind branch ($behind_distance behind) correctly passed without waiver" >&2
        else
            echo "$NAME: self-test: merely-behind branch ($behind_distance behind) was wrongly refused" >&2
            echo "$behind_output" >&2
            exit 1
        fi

        # #782's shape: the mutation commit leaves a real compile error, and
        # its subject marks why. The audit must name that exact commit. A later
        # genuine git-revert must then clear the finding and restore compilation.
        git checkout -q main
        git checkout -q -b unreverted-mutation
        cat >> src/lib.rs <<'EOF'

pub fn mutation_check_deliberate_compile_break() {
    let _ = MutationCheckRecord {
        mutation_check_missing_field: (),
    };
}
EOF
        git add src/lib.rs
        git commit -q -m "test: break synthetic compile for CI mutation check"
        local mutation_commit audit_output
        mutation_commit=$(git rev-parse HEAD)
        if cargo check --quiet >/dev/null 2>&1; then
            echo "$NAME: self-test: mutation compile break unexpectedly compiled" >&2
            exit 1
        fi
        if audit_output=$(audit_synthetic 2>&1); then
            echo "$NAME: self-test: unreverted mutation branch was NOT refused" >&2
            exit 1
        elif [[ $audit_output != *"FAIL mutation-commit: commit=$mutation_commit"* ]]; then
            echo "$NAME: self-test: mutation failure did not name $mutation_commit" >&2
            exit 1
        fi
        echo "$NAME: self-test: unreverted mutation branch correctly refused ($mutation_commit)" >&2
        if audit_synthetic --waive mutation-commit >/dev/null 2>&1; then
            echo "$NAME: self-test: mutation-commit waiver correctly passed" >&2
        else
            echo "$NAME: self-test: mutation-commit waiver was wrongly refused" >&2
            exit 1
        fi

        git revert --no-edit "$mutation_commit" >/dev/null
        cargo check --quiet
        if audit_synthetic >/dev/null 2>&1; then
            echo "$NAME: self-test: reverted mutation branch correctly passed" >&2
        else
            echo "$NAME: self-test: reverted mutation branch was wrongly refused" >&2
            exit 1
        fi

        # Ordinary refactor from a recent base: replace the merged feature with
        # new text, keeping comparable additions and deletions.
        git checkout -q main
        git checkout -q -b refactor main~1
        cat > src/lib.txt <<'EOF'
common line one
common line two
common line three
refactored feature X
refactored feature Y
refactored feature Z
EOF
        git add src/lib.txt
        git commit -q -a -m "refactor feature text"

        if audit_synthetic >/dev/null 2>&1; then
            echo "$NAME: self-test: refactor branch correctly passed" >&2
        else
            echo "$NAME: self-test: refactor branch was wrongly refused" >&2
            exit 1
        fi

        # A non-breaking commit that mentions the practice must not trip the
        # mutation check: "mutation-check every criterion" describes a test,
        # not a deliberate compile break. An over-broad pattern flags it.
        # Prose *about* breaking is not a break. "a break verb, not a bare
        # mutation-check mention" uses "break" as a noun and names the check
        # itself; an earlier pattern that looked for "break" anywhere beside a
        # build word refused exactly this subject — the commit that introduced
        # the check. The break verb must be the subject's action.
        git commit -q --allow-empty -m "fix(audit): a break verb, not a bare mutation-check mention"
        if audit_synthetic >/dev/null 2>&1; then
            echo "$NAME: self-test: prose about breaking correctly passed" >&2
        else
            echo "$NAME: self-test: prose about breaking was wrongly refused" >&2
            exit 1
        fi

        git commit -q --allow-empty -m "test: mutation-check the synthetic fixture"
        if audit_synthetic >/dev/null 2>&1; then
            echo "$NAME: self-test: mutation-check practice commit correctly passed" >&2
        else
            echo "$NAME: self-test: mutation-check practice commit was wrongly refused" >&2
            exit 1
        fi
    )
}

# ── Entry point ──────────────────────────────────────────────────────────────

while (($#)); do
    case "$1" in
        --self-test)
            self_test
            note "self-test passed"
            exit 0
            ;;
        --waive)
            case "${2:-}" in
                file-truncation) WAIVE_FILE_TRUNC=1 ;;
                stale-base) WAIVE_STALE_BASE=1 ;;
                revert-hunk) WAIVE_REVERT=1 ;;
                deletion-surge) WAIVE_DELETION_SURGE=1 ;;
                mutation-commit) WAIVE_MUTATION_COMMIT=1 ;;
                *) die "unknown waiver '${2:-}'; expected file-truncation, stale-base, revert-hunk, deletion-surge, or mutation-commit" ;;
            esac
            shift 2
            ;;
        --waive-all)
            WAIVE_FILE_TRUNC=1
            WAIVE_STALE_BASE=1
            WAIVE_REVERT=1
            WAIVE_DELETION_SURGE=1
            WAIVE_MUTATION_COMMIT=1
            shift
            ;;
        -h | --help)
            usage
            exit 0
            ;;
        *)
            die "unknown argument '$1'; expected --self-test, --waive CHECK, --waive-all, -h, --help"
            ;;
    esac
done

command -v git >/dev/null || die 'git is required'
command -v python3 >/dev/null || die 'python3 is required'

if ! git_ rev-parse --verify "$MAIN_REF" >/dev/null 2>&1; then
    die "ref '$MAIN_REF' does not exist; this script audits against origin/main"
fi

merge_base=$(git_ merge-base HEAD "$MAIN_REF")
readonly merge_base

# The truncation, deletion-surge, and revert-hunk checks take the merge base as
# the left ref: a squash merge lands merge-base..HEAD, so that is the diff this
# script owns. Diffing against $MAIN_REF instead counted every commit that
# landed after the branch point as a deletion authored by the branch, so a
# branch that was merely behind looked like a revert (#801). check_stale_base
# measures the base against main itself and is unaffected.
failures=0
while IFS=$'\t' read -r kind rest; do
    [[ -n $kind ]] || continue
    if ! report_violation "$kind" "$rest"; then
        failures=$((failures + 1))
    fi
done < <(collect_violations "$merge_base" "$merge_base")

if (( failures )); then
    note "$failures audit violation(s); push refused (waive with --waive CHECK or --waive-all)"
    exit 1
fi

note "audit passed: no revert, truncation, stale-base, deletion-surge, or unreverted mutation commit detected"
exit 0
