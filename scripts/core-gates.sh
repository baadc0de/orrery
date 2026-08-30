#!/usr/bin/env bash
# The verifiable core's static gates (docs/06-verifiable-core.md §8).
#
# Determinism is a property you lose silently. These are the checks that can be
# made mechanically, so drift is caught at commit time rather than in the strike
# pipeline months later:
#
#   1. Every crate that defines or implements canonical rules builds with no
#      Bevy anywhere in its graph.
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
SCRIPT_PATH="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/$(basename "${BASH_SOURCE[0]}")"
# The override is used only by the synthetic discovery fixtures at the end of
# this script. Ordinary invocations always scan the repository containing it.
if [[ ${_CORE_GATES_INTERNAL_SELF_TEST:-0} == 1 ]]; then
  ROOT=${_CORE_GATES_TEST_ROOT:?internal self-test requires _CORE_GATES_TEST_ROOT}
fi
die() { echo "$NAME: $*" >&2; exit 1; }
note() { echo "$NAME: $*" >&2; }

# The accepted set is a floor, not the membership mechanism. Discovery below
# adds every workspace crate that defines the trait, implements it, or defines
# a rules supertrait. Keeping this independent declaration makes weakening
# discovery fail by name instead of silently shrinking coverage.
readonly DECLARED_GATED_CRATES=(orrery_core orrery_games orrery_conformance)

# Crates whose sources are a `Ruleset` rather than the machinery around one.
# The neighbour clause below applies to these only; `orrery_core` defines and
# tests `StateView::neighbor`, so it must be able to name it.
readonly DECLARED_RULES_CRATES=(orrery_games orrery_conformance)

# Cargo metadata is the workspace-membership authority. A directory walk would
# accidentally adopt standalone tools (each is its own workspace) and could
# miss a member whose path does not sit under crates/.
declare -A CRATE_DIRS=()
workspace_rows="$({
  cd "$ROOT"
  cargo metadata --no-deps --format-version 1
} | python3 -c '
import json
import os
import sys

metadata = json.load(sys.stdin)
members = set(metadata["workspace_members"])
for package in sorted(metadata["packages"], key=lambda item: item["name"]):
    if package["id"] not in members:
        continue
    if not any("lib" in target["kind"] or "proc-macro" in target["kind"] for target in package["targets"]):
        continue
    print(package["name"], os.path.dirname(package["manifest_path"]), sep="\t")
')" || die "could not read workspace crates from cargo metadata"
[[ -n $workspace_rows ]] || die "cargo metadata reported no workspace library crates"
while IFS=$'\t' read -r crate crate_dir; do
  [[ -n $crate && -n $crate_dir ]] || die "malformed workspace crate row from cargo metadata"
  CRATE_DIRS["$crate"]=$crate_dir
done <<<"$workspace_rows"

# Library sources of a crate: every `.rs` under `src/`, minus a binary entry
# point. `orrery_conformance/src/main.rs` is the corpus CLI — it reads argv and
# writes report files, which VC-8 forbids *in a rule* and requires in a tool.
# Excluding one file keeps the ambient-input pattern narrow instead of carving
# `std::fs` and `std::env::args` out of it for everybody. Neither `orrery_core`
# nor `orrery_games` has a `main.rs`, so this is a no-op there.
lib_sources() {
  local crate src
  for crate in "$@"; do
    [[ -n ${CRATE_DIRS[$crate]:-} ]] || die "no workspace library crate named $crate"
    src="${CRATE_DIRS[$crate]}/src"
    [[ -d $src ]] || die "no $crate sources at $src"
    find "$src" -type f -name '*.rs' ! -name 'main.rs' | sort
  done
}

# Print source with cfg(test)-only items removed. The accepted discovery is a
# source-role check, not a test-fixture check: persistd's test-only Ruleset impl
# must not turn persistd into a canonical rules crate. Brace counting is scoped
# to the attributed item and handles both modules and item-level attributes.
strip_cfg_test_items() {
  awk '
    function structural(line, cleaned) {
      cleaned = line
      gsub(/\\\\./, "", cleaned)
      gsub(/"[^"]*"/, "", cleaned)
      sub(/\/\/.*/, "", cleaned)
      return cleaned
    }
    function brace_delta(line, cleaned, opens, closes) {
      cleaned = structural(line)
      opens = gsub(/\{/, "{", cleaned)
      closes = gsub(/\}/, "}", cleaned)
      return opens - closes
    }
    FNR == 1 { pending = 0; skipping = 0; depth = 0 }
    skipping {
      depth += brace_delta($0)
      if (depth <= 0) { skipping = 0; depth = 0 }
      next
    }
    pending {
      if ($0 ~ /^[[:space:]]*$/ || $0 ~ /^[[:space:]]*\/\//) next
      if ($0 ~ /^[[:space:]]*#\[/) next
      depth = brace_delta($0)
      if (depth > 0) skipping = 1
      pending = 0
      next
    }
    $0 ~ /^[[:space:]]*#\[[[:space:]]*cfg[[:space:]]*\([[:space:]]*test[[:space:]]*\)[[:space:]]*\]/ {
      attributed = $0
      sub(/^[^]]*\]/, "", attributed)
      if (attributed ~ /^[[:space:]]*$/) {
        pending = 1
      } else {
        depth = brace_delta(attributed)
        if (depth > 0) skipping = 1
      }
      next
    }
    { print }
  ' "$@"
}

has_ruleset_role() {
  local crate=$1
  local sources=()
  while IFS= read -r file; do sources+=("$file"); done < <(lib_sources "$crate")
  (( ${#sources[@]} > 0 )) || return 1

  # Three role sites: the trait definition; qualified or unqualified impls;
  # and a trait whose supertraits include Ruleset (`Game: Ruleset`). Generic
  # consumers such as `struct Client<R: Ruleset>` are intentionally not a
  # canonical role and therefore do not pull the Bevy-facing facade into Tier V.
  # Do not use grep -q here: with pipefail, an early match can close the pipe
  # while awk is still writing and turn a successful discovery into SIGPIPE.
  strip_cfg_test_items "${sources[@]}" | grep -E \
    '(^|[[:space:]])trait[[:space:]]+Ruleset([[:space:]:<{]|$)|(^|[[:space:]])impl([[:space:]<][^;{}]*)?([[:alnum:]_]+::)*Ruleset[[:space:]]+for([[:space:]<{]|$)|(^|[[:space:]])trait[[:space:]]+[[:alnum:]_]+[^;{}]*:[^;{}]*([[:alnum:]_]+::)*Ruleset([[:space:]+<{]|$)' \
    >/dev/null
}

discover_role_crates() {
  local crate
  for crate in "${!CRATE_DIRS[@]}"; do
    has_ruleset_role "$crate" && echo "$crate"
  done | sort -u
}

# Deliberately broader than has_ruleset_role: this is the second source for
# D43(d)(3)'s two-way check. If the precise discovery pattern ever regresses,
# an impl-shaped crate must fail as undiscovered instead of vanishing from both
# the discovered and scanned sets by agreement with the same bug.
impl_bearing_crates() {
  local crate sources=()
  for crate in "${!CRATE_DIRS[@]}"; do
    sources=()
    while IFS= read -r file; do sources+=("$file"); done < <(lib_sources "$crate")
    (( ${#sources[@]} > 0 )) || continue
    if strip_cfg_test_items "${sources[@]}" \
      | grep -E '(^|[[:space:]])impl[^;{}]*([[:alnum:]_]+::)*Ruleset[[:space:]]+for([[:space:]<{]|$)' \
        >/dev/null; then
      echo "$crate"
    fi
  done | sort -u
}

DISCOVERED_CRATES=()
while IFS= read -r crate; do
  [[ -n $crate ]] && DISCOVERED_CRATES+=("$crate")
done < <(discover_role_crates)
(( ${#DISCOVERED_CRATES[@]} > 0 )) || die "role discovery found no Ruleset crates"

GATED_CRATES=()
while IFS= read -r crate; do GATED_CRATES+=("$crate"); done < <(
  printf '%s\n' "${DECLARED_GATED_CRATES[@]}" "${DISCOVERED_CRATES[@]}" | sort -u
)
readonly -a GATED_CRATES

RULES_CRATES=()
while IFS= read -r crate; do RULES_CRATES+=("$crate"); done < <(
  printf '%s\n' "${DECLARED_RULES_CRATES[@]}" "${DISCOVERED_CRATES[@]}" \
    | grep -vFx orrery_core \
    | sort -u
)
readonly -a RULES_CRATES

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

# D43(d)(4): canonical execution stays synchronous. This is structural on the
# current tree, but making it a dependency check prevents a future role crate
# from quietly introducing an async runtime.
for crate in "${GATED_CRATES[@]}"; do
  if (cd "$ROOT" && cargo tree -p "$crate" 2>/dev/null \
    | grep -Eqi '(^|[[:space:]│├└─])(tokio|async-std) v'); then
    die "$crate has an async runtime in its dependency graph"
  fi
done
note "async-runtime-free: ${GATED_CRATES[*]}"

# Discovery and the declared floor are deliberately independent sources. The
# union above keeps coverage intact while this check makes a weakened scanner
# fail by the exact missing crate. An extra discovered crate is valid and is
# already present in GATED_CRATES; a stale declaration is not.
note "role discovery: ${DISCOVERED_CRATES[*]}"
for crate in "${DECLARED_GATED_CRATES[@]}"; do
  if ! printf '%s\n' "${DISCOVERED_CRATES[@]}" | grep -Fxq "$crate"; then
    die "role discovery missed declared ruleset crate '$crate'"
  fi
done
while IFS= read -r crate; do
  [[ -n $crate ]] || continue
  if ! printf '%s\n' "${GATED_CRATES[@]}" | grep -Fxq "$crate"; then
    die "undiscovered ruleset crate '$crate' — add it to the gate or justify"
  fi
done < <(impl_bearing_crates)

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

# ── Role-discovery self-tests ───────────────────────────────────────────
# These run through the ordinary CI invocation. A --self-test mode would need
# a scripts/check.sh registration owned by another lane; keeping the fixtures
# here makes them impossible to add without also running them per commit.
make_discovery_fixture() {
  local fixture=$1 host_mode=$2 conformance_violation=$3
  mkdir -p \
    "$fixture/crates/orrery_core/src" \
    "$fixture/crates/orrery_games/src/regolith" \
    "$fixture/crates/orrery_conformance/src" \
    "$fixture/crates/orrery_persistd/src"

  cat >"$fixture/Cargo.toml" <<'EOF'
[workspace]
resolver = "3"
members = ["crates/*"]
EOF
  cat >"$fixture/crates/orrery_core/Cargo.toml" <<'EOF'
[package]
name = "orrery_core"
version = "0.0.0"
edition = "2024"
EOF
  cat >"$fixture/crates/orrery_core/src/lib.rs" <<'EOF'
pub trait Ruleset {}
EOF
  cat >"$fixture/crates/orrery_games/Cargo.toml" <<'EOF'
[package]
name = "orrery_games"
version = "0.0.0"
edition = "2024"

[dependencies]
orrery_core = { path = "../orrery_core" }
EOF
  cat >"$fixture/crates/orrery_games/src/lib.rs" <<'EOF'
pub struct Game;
impl orrery_core::Ruleset for Game {}
EOF
  cat >"$fixture/crates/orrery_games/src/regolith/visibility.rs" <<'EOF'
fn verify_claims() {}
EOF
  cat >"$fixture/crates/orrery_conformance/Cargo.toml" <<'EOF'
[package]
name = "orrery_conformance"
version = "0.0.0"
edition = "2024"

[dependencies]
orrery_core = { path = "../orrery_core" }
EOF
  if [[ $conformance_violation == yes ]]; then
    cat >"$fixture/crates/orrery_conformance/src/lib.rs" <<'EOF'
pub struct Reference;
impl orrery_core::Ruleset for Reference {}
use std::collections::HashMap;
EOF
  else
    cat >"$fixture/crates/orrery_conformance/src/lib.rs" <<'EOF'
pub struct Reference;
impl orrery_core::Ruleset for Reference {}
EOF
  fi
  cat >"$fixture/crates/orrery_persistd/Cargo.toml" <<'EOF'
[package]
name = "orrery_persistd"
version = "0.0.0"
edition = "2024"

[dependencies]
orrery_core = { path = "../orrery_core" }
EOF
  cat >"$fixture/crates/orrery_persistd/src/lib.rs" <<'EOF'
#[cfg(test)]
mod tests {
    struct TestOnly;
    impl orrery_core::Ruleset for TestOnly {}
}
EOF

  if [[ $host_mode == bevy ]]; then
    mkdir -p "$fixture/crates/orrery_host/src" "$fixture/crates/bevy_ecs/src"
    cat >"$fixture/crates/bevy_ecs/Cargo.toml" <<'EOF'
[package]
name = "bevy_ecs"
version = "0.0.0"
edition = "2024"
EOF
    cat >"$fixture/crates/bevy_ecs/src/lib.rs" <<'EOF'
pub struct World;
EOF
    cat >"$fixture/crates/orrery_host/Cargo.toml" <<'EOF'
[package]
name = "orrery_host"
version = "0.0.0"
edition = "2024"

[dependencies]
bevy_ecs = { path = "../bevy_ecs" }
orrery_core = { path = "../orrery_core" }
EOF
    cat >"$fixture/crates/orrery_host/src/lib.rs" <<'EOF'
pub struct Host;
impl orrery_core::Ruleset for Host {}
EOF
  fi
}

run_discovery_self_tests() {
  local scratch fixture output status mutant
  scratch=$(mktemp -d)
  trap 'rm -rf "$scratch"' RETURN

  # Keep "bevy" out of the fixture path: cargo tree prints package paths and
  # clause 1 intentionally matches that dependency spelling in its output.
  fixture="$scratch/synthetic-role-violation"
  make_discovery_fixture "$fixture" bevy no
  status=0
  output="$(_CORE_GATES_INTERNAL_SELF_TEST=1 \
    _CORE_GATES_TEST_ROOT="$fixture" \
    _CORE_GATES_SKIP_SELF_TESTS=1 \
    "$SCRIPT_PATH" 2>&1)" || status=$?
  if [[ ${CORE_GATES_SHOW_SELF_TEST_OUTPUT:-0} == 1 ]]; then
    printf '%s\n' '--- synthetic undeclared Ruleset crate ---' "$output" >&2
  fi
  (( status == 1 )) \
    || die "discovery self-test: synthetic Bevy ruleset returned $status, expected 1"
  grep -Fxq "core-gates: orrery_host has Bevy in its dependency graph" <<<"$output" \
    || die "discovery self-test: synthetic violation did not name orrery_host"
  note "discovery self-test: undeclared synthetic Ruleset crate is discovered and fails Bevy-free by name"

  fixture="$scratch/removed-floor-entry"
  make_discovery_fixture "$fixture" none yes
  mutant="$scratch/core-gates-with-one-floor-entry-removed.sh"
  sed 's/readonly DECLARED_GATED_CRATES=(orrery_core orrery_games orrery_conformance)/readonly DECLARED_GATED_CRATES=(orrery_core orrery_games)/' \
    "$SCRIPT_PATH" >"$mutant"
  chmod +x "$mutant"
  status=0
  output="$(_CORE_GATES_INTERNAL_SELF_TEST=1 \
    _CORE_GATES_TEST_ROOT="$fixture" \
    _CORE_GATES_SKIP_SELF_TESTS=1 \
    "$mutant" 2>&1)" || status=$?
  if [[ ${CORE_GATES_SHOW_SELF_TEST_OUTPUT:-0} == 1 ]]; then
    printf '%s\n' '--- declared floor entry removed, discovered violation retained ---' "$output" >&2
  fi
  (( status == 1 )) \
    || die "discovery self-test: removed-floor mutant returned $status, expected 1"
  grep -Fq "core-gates: role discovery: orrery_conformance orrery_core orrery_games" <<<"$output" \
    || die "discovery self-test: removed floor entry also removed orrery_conformance from discovery"
  grep -Fq "orrery_conformance/src/lib.rs:3:use std::collections::HashMap;" <<<"$output" \
    || die "discovery self-test: planted orrery_conformance VC-4 violation was not scanned"
  grep -Fq "core-gates: VC-4: std HashMap/HashSet in a gated crate" <<<"$output" \
    || die "discovery self-test: removed-floor mutant did not fail VC-4"
  note "discovery self-test: removing a floor entry cannot remove a discovered crate from VC-4"
}

if [[ ${_CORE_GATES_SKIP_SELF_TESTS:-0} != 1 ]]; then
  run_discovery_self_tests
fi
