#!/usr/bin/env bash
# The verifiable core's static gates (docs/06-verifiable-core.md §8).
#
# Determinism is a property you lose silently. These are the checks that can be
# made mechanically, so drift is caught at commit time rather than in the strike
# pipeline months later:
#
#   1. `orrery_core` builds with no Bevy anywhere in its graph.
#   2. No std `HashMap`/`HashSet` in core sources (VC-4) — their iteration order
#      is randomized per process, so any behaviour depending on it differs
#      between two runs of the same binary.
#   3. No ambient inputs (VC-8) or std float transcendentals (VC-6).
#
# The scans cover `orrery_core` **and the game core crates**, which is what
# docs/06 §4 asks for: the rules are where a `HashMap` or a `SystemTime::now`
# actually costs an adjudication, and a gate that only watched the trait
# definition would be watching the one file least likely to break it.
#
# What these cannot catch is covered by the crate's own tests, which run an
# identical tick twice in-process and compare state hashes: that is what
# actually surfaces a VC-4 or VC-8 violation, because it fails on the symptom
# rather than on a spelling.
set -euo pipefail

readonly NAME=core-gates
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
die() { echo "$NAME: $*" >&2; exit 1; }
note() { echo "$NAME: $*" >&2; }

# Every crate held to the determinism rules: the core itself, and the games
# whose `Ruleset` implementations are the code those rules are actually about.
readonly GATED_CRATES=(orrery_core orrery_games)

SOURCES=()
for crate in "${GATED_CRATES[@]}"; do
  src="$ROOT/crates/$crate/src"
  [[ -d $src ]] || die "no $crate sources at $src"
  SOURCES+=("$src")
done

# ── 1. Bevy-free ────────────────────────────────────────────────────────
# The same build links into game clients, field hosts and persistd. A Bevy
# dependency would not just be weight — it would make those three disagree,
# which is the exact failure the crate exists to detect.
for crate in "${GATED_CRATES[@]}"; do
  if (cd "$ROOT" && cargo tree -p "$crate" 2>/dev/null | grep -qi bevy); then
    die "$crate has Bevy in its dependency graph"
  fi
done
note "Bevy-free: ${GATED_CRATES[*]}"

# Sources only — a match inside a doc comment explaining the rule is not a
# violation of it, so comment lines are stripped before scanning.
scan() {
  local pattern=$1 rule=$2
  local hits
  hits=$(grep -rnE "$pattern" "${SOURCES[@]}" --include='*.rs' \
    | grep -vE '^\s*[^:]+:[0-9]+:\s*(//|//!|/\*|\*)' \
    || true)
  if [[ -n $hits ]]; then
    echo "$hits" >&2
    die "$rule"
  fi
}

# ── 2. VC-4: no unordered iteration ─────────────────────────────────────
scan '\b(HashMap|HashSet)\b' \
  'VC-4: std HashMap/HashSet in a gated crate — use BTreeMap/BTreeSet or a sorted Vec'
note 'VC-4: no unordered collections'

# ── 3. VC-8 / VC-6: no ambient inputs, no std transcendentals ───────────
scan '\b(Instant::now|SystemTime::now|thread_rng|from_entropy)\b' \
  'VC-8: ambient input in a gated crate — time is Tick, randomness is seeded'
scan '\bf(32|64)::(sin|cos|tan|asin|acos|atan|atan2|exp|ln|log|log2|log10|powf|sqrt|cbrt|hypot)\b' \
  'VC-6: std float transcendental in a gated crate — route through libm'
note 'VC-6/VC-8: no ambient inputs, no std transcendentals'

echo "$NAME: verifiable-core static gates pass"
