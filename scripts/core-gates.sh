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
#   4. No live neighbour reads inside a `Ruleset` (docs/06 §3 implementation
#      status) — cross-entity effects travel as events.
#
# The scans cover `orrery_core`, **the game core crates**, and the conformance
# corpus's reference ruleset, which is what docs/06 §4 asks for: the rules are
# where a `HashMap` or a `SystemTime::now` actually costs an adjudication, and a
# gate that only watched the trait definition would be watching the one file
# least likely to break it.
#
# What these cannot catch is covered by the crates' own tests, which run an
# identical tick twice in-process and compare state hashes: that is what
# actually surfaces a VC-4 or VC-8 violation, because it fails on the symptom
# rather than on a spelling.
set -euo pipefail

readonly NAME=core-gates
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
die() { echo "$NAME: $*" >&2; exit 1; }
note() { echo "$NAME: $*" >&2; }

# Every crate held to the determinism rules: the core itself, the games whose
# `Ruleset` implementations are the code those rules are actually about, and
# `orrery_conformance` — the crate that *is* the cross-platform determinism
# matrix, and which until now was scanned for nothing at all.
readonly GATED_CRATES=(orrery_core orrery_games orrery_conformance)

# Crates whose sources are a `Ruleset` rather than the machinery around one.
# The neighbour clause below applies to these only; `orrery_core` defines and
# tests `StateView::neighbor`, so it must be able to name it.
readonly RULES_CRATES=(orrery_games orrery_conformance)

# Library sources of a crate: every `.rs` under `src/`, minus a binary entry
# point. `orrery_conformance/src/main.rs` is the corpus CLI — it reads argv and
# writes report files, which VC-8 forbids *in a rule* and requires in a tool.
# Excluding one file keeps the ambient-input pattern narrow instead of carving
# `std::fs` and `std::env::args` out of it for everybody. Neither `orrery_core`
# nor `orrery_games` has a `main.rs`, so this is a no-op there.
lib_sources() {
  local crate src
  for crate in "$@"; do
    src="$ROOT/crates/$crate/src"
    [[ -d $src ]] || die "no $crate sources at $src"
    find "$src" -type f -name '*.rs' ! -name 'main.rs' | sort
  done
}

GATED_SOURCES=()
while IFS= read -r file; do GATED_SOURCES+=("$file"); done < <(lib_sources "${GATED_CRATES[@]}")
[[ ${#GATED_SOURCES[@]} -gt 0 ]] || die "no gated sources found"

RULES_SOURCES=()
while IFS= read -r file; do RULES_SOURCES+=("$file"); done < <(lib_sources "${RULES_CRATES[@]}")
[[ ${#RULES_SOURCES[@]} -gt 0 ]] || die "no ruleset sources found"

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
# violation of it, so comment lines are stripped before scanning. The source
# set is per-scan: not every clause applies to every gated crate.
scan() {
  local pattern=$1 rule=$2
  shift 2
  local hits
  hits=$(grep -nE "$pattern" "$@" \
    | grep -vE '^\s*[^:]+:[0-9]+:\s*(//|//!|/\*|\*)' \
    || true)
  if [[ -n $hits ]]; then
    echo "$hits" >&2
    die "$rule"
  fi
}

# ── 2. VC-4: no unordered iteration ─────────────────────────────────────
scan '\b(HashMap|HashSet)\b' \
  'VC-4: std HashMap/HashSet in a gated crate — use BTreeMap/BTreeSet or a sorted Vec' \
  "${GATED_SOURCES[@]}"
note 'VC-4: no unordered collections'

# ── 3. VC-8: no ambient inputs ──────────────────────────────────────────
# Time is `Tick`, randomness is seeded from `(universe_seed, entity, tick)`, and
# the environment reaches a rule only as a logged input.
scan '\b(Instant::now|SystemTime::now|thread_rng|from_entropy|rand::random|OsRng|from_os_rng|std::env::var)\b|\.elapsed\(\)' \
  'VC-8: ambient input in a gated crate — time is Tick, randomness is seeded' \
  "${GATED_SOURCES[@]}"

# ── 4. VC-6: no std float transcendentals ───────────────────────────────
# Both spellings, because the path form is the one nobody writes: `f64::sqrt(x)`
# is rare and `x.sqrt()` is what actually appears in rules code. Until the
# method arm existed this gate matched the first and was blind to the second.
#
# `round`/`floor`/`ceil`/`trunc`/`abs`/`mul_add` are deliberately absent, and
# must stay absent: they are IEEE-754 exact operations with a single correct
# result on every platform, and the quantization lattice (VC-7) is built out of
# them. Gating them would forbid the very code that makes continuous state
# comparable.
readonly TRANSCENDENTALS='sin|cos|tan|asin|acos|atan|atan2|exp|ln|log|log2|log10|powf|sqrt|cbrt|hypot'
scan "\\bf(32|64)::($TRANSCENDENTALS)\\b" \
  'VC-6: std float transcendental (path form) in a gated crate — route through libm' \
  "${GATED_SOURCES[@]}"
scan "\\.($TRANSCENDENTALS)\\(" \
  'VC-6: std float transcendental (method form) in a gated crate — route through libm' \
  "${GATED_SOURCES[@]}"
note 'VC-6/VC-8: no ambient inputs, no std transcendentals'

# ── 5. Neighbour reads inside a Ruleset ─────────────────────────────────
# `StateView::neighbor` records the read, but nothing yet *replays* it: a
# `NeighborFrame` producer does not exist (docs/06 §3, implementation status),
# and `ReplayHarness::load_claimed_snapshot` installs exactly one entity — so at
# replay every neighbour read returns `None` and a rule that branched on one
# adjudicates differently than it executed. Rules keep cross-entity effects in
# events until that gap closes.
#
# Matched on the receiver rather than as a bare `.neighbor(`: `CellId::neighbor`
# is an unrelated method on the spatial type, and `view` is the binding every
# gated `Ruleset::step` gives its `StateView`.
scan '\bview\.neighbor\s*\(' \
  'live neighbour read in a Ruleset — cross-entity effects travel as events (docs/06 §3)' \
  "${RULES_SOURCES[@]}"
note "no live neighbour reads: ${RULES_CRATES[*]}"

echo "$NAME: verifiable-core static gates pass"
