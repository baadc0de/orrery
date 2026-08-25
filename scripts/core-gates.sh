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
# The recorded-input closure is now complete: Executor emits canonical state
# plus the neighbour tick actually observed, ReplayHarness serves those frames
# without installing a live world, enforces the ruleset's read/staleness caps,
# and compares the replay's reads with the signed frame set. Cross-checking is
# against the declared tick; a stale frame is refused, never turned into a
# deviation against an honest lagged reader.
#
# What is bounded, and where. The quantities that matter are enforced at the
# replay layer, not here: `max_neighbor_reads` caps how many frames a tick may
# pull in and `max_neighbor_staleness_ticks` bounds how old one may be (both in
# orrery_core/src/replay.rs), and `log::cross_check_neighbor_record` verifies
# each frame against the neighbour authority's signed claim. This gate restates
# none of that.
#
# What this gate buys is the tripwire: no code starts reading neighbours without
# a human seeing it. So it checks *where* reads live, by name, against a declared
# list. It deliberately does NOT count occurrences. A count measures text, not
# behaviour — one expression can read a hundred neighbours, and a hundred
# expressions reading one each are identically safe — and a count is satisfied by
# reformatting, which is exactly how #441 widened the audited predicate to a
# third entity while the "exactly one site" check still passed. Counting also
# pushes every future neighbour-reading feature into a single god-predicate,
# which is harder to review than several small named ones, not easier.
#
# Adding a predicate here is a one-line diff a reviewer cannot miss. That is the
# whole mechanism, and it is honest about being a review trigger rather than a
# safety property.

# Comment lines are not reads. Matched on the receiver rather than a bare
# `.neighbor(`: `CellId::neighbor` is an unrelated method on the spatial type,
# and `view` is the binding every gated `Ruleset::step` gives its `StateView`.
neighbor_hits=$(grep -nE '\bview\.neighbor\s*\(' "${RULES_SOURCES[@]}" \
  | grep -vE '^\s*[^:]+:[0-9]+:\s*(//|//!|/\*|\*)' \
  || true)

readonly AUDITED_NEIGHBOR_PREDICATES=(
  'crates/orrery_games/src/regolith/visibility.rs::verify_claims'
)

# Staleness first: a declared predicate that no longer exists must report as a
# stale declaration, not as an undeclared read somewhere else. Renaming the
# function otherwise produces a message pointing at the wrong problem.
for allowed in "${AUDITED_NEIGHBOR_PREDICATES[@]}"; do
  allowed_file=${allowed%%::*}
  allowed_fn=${allowed##*::}
  grep -qE "fn ${allowed_fn}\b" "$ROOT/$allowed_file" 2>/dev/null \
    || die "declared audited predicate '$allowed' does not exist — remove it from AUDITED_NEIGHBOR_PREDICATES or fix the name"
done

neighbor_violations=()
while IFS= read -r hit; do
  [[ -n $hit ]] || continue
  hit_file=${hit%%:*}
  hit_rest=${hit#*:}
  hit_line=${hit_rest%%:*}
  rel_file=${hit_file#"$ROOT"/}
  # The enclosing item is the nearest `fn <name>` at or above the hit.
  enclosing=$(awk -v n="$hit_line" '
    NR<=n && match($0, /fn [a-z_][a-z0-9_]*/) {
      name=substr($0, RSTART+3, RLENGTH-3)
    }
    END { print name }
  ' "$hit_file")
  if [[ -z $enclosing ]]; then
    neighbor_violations+=("$rel_file:$hit_line: neighbour read outside any function")
    continue
  fi
  declared=no
  for allowed in "${AUDITED_NEIGHBOR_PREDICATES[@]}"; do
    [[ "$rel_file::$enclosing" == "$allowed" ]] && declared=yes && break
  done
  [[ $declared == yes ]] \
    || neighbor_violations+=("$rel_file:$hit_line: in undeclared predicate '$enclosing'")
done <<<"$neighbor_hits"

if (( ${#neighbor_violations[@]} > 0 )); then
  printf '%s\n' "${neighbor_violations[@]}" >&2
  die 'neighbour read outside a declared audited predicate — add it to AUDITED_NEIGHBOR_PREDICATES in this file, and say in review why the read is adjudicable (docs/06 §3)'
fi

note "recorded neighbour reads confined to ${#AUDITED_NEIGHBOR_PREDICATES[@]} declared audited predicate(s)"

echo "$NAME: verifiable-core static gates pass"
