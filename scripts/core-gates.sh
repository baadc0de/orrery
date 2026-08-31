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
#   5. ADR-0043 clause (e)'s Tier-H host battery, over any crate hosting
#      canonical state in a `bevy_ecs::World`: a review-required allowlist, a
#      `bevy_ecs`-only dependency rule, the whole Tier V source battery plus an
#      async ban and an RNG-construction ban over the host's canonical modules,
#      and the declared existence of the ambiguity canary, the projection
#      differential and the world-of-one harness — which clause (e)(4) makes
#      "preconditions of admitting the host, not follow-ups".
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

# ── 6. Tier H — the conditional host battery (D43 (e)) ──────────────────
# Armed by a crate hosting canonical state in a `bevy_ecs::World`. D42 (d)
# admitted exactly one such host (#757, `orrery_sim_host`'s `EcsBackend`) under
# a direct owner sanction, ahead of this battery; D43 (e)'s amendment records
# that as a debt and confines the host until the battery is enforced and
# demonstrated mutation-style. This section is that battery.
#
# What "hosting canonical state" is, mechanically. A crate is a Tier-H host
# when it (i) implements `orrery_core::TickBackend` — the trait through which
# canonical state is stored and stepped — and (ii) has `bevy_ecs` in its
# runtime graph. Both halves matter: `Executor` implements the trait without an
# ECS, and `orrery_net`, `orrery_spatial`, `orrery_authority`, `orrery_predict`,
# `orrery_witness`, `orrery_persist_client` and `orrery` all use `bevy_ecs`
# without ever holding canonical state. Neither is a host; the conjunction is.
#
# Clause (e)(1) says "no discovery here, because hosting ECS is always a
# decision, never an accident". So the *membership* mechanism is the declared
# allowlist below — a one-line diff a reviewer cannot miss. The conjunction
# above is not a second membership mechanism; it is the escape check that makes
# a new host fail by name instead of arriving silently, in the same two-source
# idiom Tier V's role discovery uses against DECLARED_GATED_CRATES.

# The allowlist. Adding a row here is a review decision that this crate takes
# the whole battery below.
DECLARED_HOST_CRATES=(orrery_sim_host)

# Clause (e)(2): the only Bevy crate a host may depend on is `bevy_ecs`. These
# are the spellings the record names as hard failures — app coupling, the
# umbrella, and the ambient clock. `bevy_ecs`'s own transitive crates
# (`bevy_platform`, `bevy_ptr`, `bevy_tasks`, `bevy_reflect`, the macro crates)
# are not app coupling and are not listed.
readonly FORBIDDEN_HOST_BEVY_CRATES=(bevy_app bevy_internal bevy_time)

# Clause (e)(4) and (e)(5): the harnesses that are preconditions of admitting a
# host, declared by name so removing or renaming one fails here rather than
# reducing the suite by one silent test. `cargo test --workspace` runs them.
DECLARED_HOST_HARNESSES=(
  'crates/orrery_sim_host/tests/tier_h_projection_differential.rs::the_canonical_schedule_composes_unambiguously_and_the_unordered_mutant_does_not'
  'crates/orrery_sim_host/tests/tier_h_projection_differential.rs::permuted_insertion_orders_agree_on_the_sorted_projection_and_the_executor_chain'
  'crates/orrery_sim_host/tests/tier_h_world_of_one.rs::the_verdict_holds_in_a_world_of_one'
)

# The synthetic host fixtures at the end of this file are their own tiny
# workspaces with no `orrery_sim_host` in them, so the declarations above are
# the repository's and cannot be theirs. The override is empty by default,
# which is what makes a fixture's rogue host fail the (e)(1) escape check
# rather than the staleness check.
if [[ ${_CORE_GATES_INTERNAL_SELF_TEST:-0} == 1 ]]; then
  DECLARED_HOST_CRATES=()
  DECLARED_HOST_HARNESSES=()
  if [[ -n ${_CORE_GATES_TEST_HOSTS:-} ]]; then
    read -r -a DECLARED_HOST_CRATES <<<"$_CORE_GATES_TEST_HOSTS"
  fi
fi
readonly -a DECLARED_HOST_CRATES
readonly -a DECLARED_HOST_HARNESSES

# A crate's runtime dependency graph, dev-dependencies excluded. `-e normal` is
# load-bearing for clause (e)(2): `orrery_sim_host` legitimately carries
# `bevy_app` as a dev-dependency for a test that builds an `App` it never gives
# canonical state to, and a check that could not tell the two apart would have
# to be either vacuous or wrong.
runtime_tree() {
  (cd "$ROOT" && cargo tree -p "$1" -e normal 2>/dev/null)
}

# Crates that host canonical state on an ECS, by the conjunction above.
ecs_host_crates() {
  local crate sources=()
  for crate in "${!CRATE_DIRS[@]}"; do
    sources=()
    while IFS= read -r file; do sources+=("$file"); done < <(lib_sources "$crate")
    (( ${#sources[@]} > 0 )) || continue
    # Do not use grep -q under pipefail: an early match can SIGPIPE awk and
    # turn a successful discovery into a silent miss.
    strip_cfg_test_items "${sources[@]}" \
      | grep -E '(^|[[:space:]])impl[^;{}]*([[:alnum:]_]+::)*TickBackend[[:space:]<][^;{}]*[[:space:]]for([[:space:]<{]|$)' \
        >/dev/null || continue
    runtime_tree "$crate" | grep -Eq '(^|[^[:alnum:]_])bevy_ecs v' || continue
    echo "$crate"
  done | sort -u
}

HOST_CRATES=()
while IFS= read -r crate; do
  [[ -n $crate ]] && HOST_CRATES+=("$crate")
done < <(ecs_host_crates)

# (e)(1), both directions. An undeclared host is an accident; a declared crate
# that is no longer a host is a stale allowlist, and the two must not report as
# each other.
for crate in "${DECLARED_HOST_CRATES[@]}"; do
  if ! printf '%s\n' "${HOST_CRATES[@]}" | grep -Fxq "$crate"; then
    die "declared Tier-H host '$crate' no longer hosts canonical state in a bevy_ecs::World — remove it from DECLARED_HOST_CRATES in this file (D43 (e)(1))"
  fi
done
for crate in "${HOST_CRATES[@]}"; do
  if ! printf '%s\n' "${DECLARED_HOST_CRATES[@]}" | grep -Fxq "$crate"; then
    die "undeclared ECS host crate '$crate' — hosting canonical state in a bevy_ecs::World is always a decision, never an accident: add it to DECLARED_HOST_CRATES in this file and take the whole Tier-H battery (D43 (e)(1))"
  fi
done
note "Tier-H host allowlist: ${DECLARED_HOST_CRATES[*]}"

# Everything below governs a *declared* host. With none declared — the state
# D43 (e) was written for, and the state every synthetic fixture in this file
# is in — Tier H is unarmed and only the escape check above runs.
if (( ${#DECLARED_HOST_CRATES[@]} == 0 )); then
  note "Tier H: no declared ECS host, battery unarmed"
else

# (e)(2): bevy_ecs only.
for crate in "${DECLARED_HOST_CRATES[@]}"; do
  host_tree="$(runtime_tree "$crate")"
  [[ -n $host_tree ]] || die "could not read $crate's runtime dependency graph"
  for forbidden in "${FORBIDDEN_HOST_BEVY_CRATES[@]}"; do
    if grep -Eq "(^|[^[:alnum:]_])${forbidden} v" <<<"$host_tree"; then
      die "Tier-H host '$crate' has a runtime dependency on $forbidden — a host may depend on bevy_ecs only, which is what keeps SubApp-style app coupling out (D43 (e)(2))"
    fi
  done
  if grep -Eq '(^|[^[:alnum:]_])bevy v' <<<"$host_tree"; then
    die "Tier-H host '$crate' has a runtime dependency on the full bevy crate — a host may depend on bevy_ecs only (D43 (e)(2))"
  fi
  if grep -Eq '(^|[[:space:]│├└─])(tokio|async-std) v' <<<"$host_tree"; then
    die "Tier-H host '$crate' has an async runtime in its dependency graph — canonical execution is synchronous end-to-end within a tick (D43 (e)(3), (c)(7))"
  fi
done
note "Tier-H hosts depend on bevy_ecs only: ${DECLARED_HOST_CRATES[*]}"

# (e)(3): the full Tier V source battery over the host's canonical modules,
# plus the async ban and the RNG-construction ban.
#
# "Canonical modules" is taken here as every library source of the host crate.
# A declared subset would be a second allowlist governing which files are
# allowed to hold canonical state, and a host that moved a `HashMap` into an
# undeclared module would pass — so the whole `src/` tree carries the battery,
# and a host crate that wants a HashMap for something non-canonical must put it
# in a crate that is not a host.
HOST_SOURCES=()
while IFS= read -r file; do HOST_SOURCES+=("$file"); done < <(lib_sources "${DECLARED_HOST_CRATES[@]}")
[[ ${#HOST_SOURCES[@]} -gt 0 ]] || die "no Tier-H host sources found"

scan '\b(HashMap|HashSet)\b' \
  'VC-4 (Tier H): std HashMap/HashSet in a host canonical module — use BTreeMap/BTreeSet or a sorted Vec (D43 (e)(3))' \
  "${HOST_SOURCES[@]}"
scan '\b(Instant::now|SystemTime::now|thread_rng|from_entropy|rand::random|OsRng|from_os_rng|std::env::var)\b|\.elapsed\(\)' \
  'VC-8 (Tier H): ambient input in a host canonical module — time is Tick, randomness is seeded (D43 (e)(3))' \
  "${HOST_SOURCES[@]}"
scan "\\bf(32|64)::($TRANSCENDENTALS)\\b" \
  'VC-6 (Tier H): std float transcendental (path form) in a host canonical module — route through libm (D43 (e)(3))' \
  "${HOST_SOURCES[@]}"
scan "\\.($TRANSCENDENTALS)\\(" \
  'VC-6 (Tier H): std float transcendental (method form) in a host canonical module — route through libm (D43 (e)(3))' \
  "${HOST_SOURCES[@]}"
scan '(^|[^[:alnum:]_])async[[:space:]]+(fn|move|\{)|\.await([^[:alnum:]_]|$)|#\[(tokio|async_std)::' \
  'D43 (e)(3): async in a host canonical module — canonical execution is synchronous end-to-end within a tick, and nothing spawned during it may outlive the schedule run (D43 (c)(7))' \
  "${HOST_SOURCES[@]}"
# The RNG ban is construction, not use: a host receives `&mut TickRng` and
# passes it along. `orrery_core::tick_rng` is the only canonical constructor and
# it does not live in a host crate, so any construction site here is one too
# many.
scan '\b(StdRng|SmallRng|ChaCha8Rng|ChaCha12Rng|ChaCha20Rng|ChaChaRng|Pcg[[:alnum:]]*|Xoshiro[[:alnum:]]*)\b|\b(from_entropy|from_seed|seed_from_u64|from_rng|thread_rng|rng\(\))[[:space:]]*\(|\bSeedableRng\b' \
  'D43 (e)(3): RNG construction in a host canonical module — the per-entity, per-tick stream is orrery_core::tick_rng and a host never builds one (D43 (c)(5))' \
  "${HOST_SOURCES[@]}"
note "Tier-H source battery (VC-4/VC-6/VC-8 + async + RNG construction) over ${#HOST_SOURCES[@]} host source(s)"

# (e)(5): the host must implement single-entity stepping itself.
#
# `TickBackend::step_entity` has no default, so a host cannot inherit one — but
# it can implement it by delegating to a whole-population step, which is the
# expensive lie the clause is about ("the schedule was deterministic" is never
# a substitute for per-entity replay). The gate can only see that the site
# exists and is named; that the site is *honest* is what the declared
# world-of-one harness below proves, in a populated world where the difference
# is visible.
for crate in "${DECLARED_HOST_CRATES[@]}"; do
  host_sources=()
  while IFS= read -r file; do host_sources+=("$file"); done < <(lib_sources "$crate")
  strip_cfg_test_items "${host_sources[@]}" \
    | grep -E '(^|[[:space:]])fn[[:space:]]+step_entity([[:space:]<(]|$)' >/dev/null \
    || die "Tier-H host '$crate' does not implement TickBackend::step_entity — the verdict must hold in a world of one, and a host that cannot step one entity cannot be adjudicated per-entity (D43 (e)(5))"
done

# (e)(4) and (e)(5): the declared harnesses exist, by name.
#
# Staleness first, in the AUDITED_NEIGHBOR_PREDICATES idiom: a renamed test
# must report as a stale declaration pointing at the name that moved, not as
# something else.
for harness in "${DECLARED_HOST_HARNESSES[@]}"; do
  harness_file=${harness%%::*}
  harness_fn=${harness##*::}
  [[ -f $ROOT/$harness_file ]] \
    || die "declared Tier-H harness file '$harness_file' does not exist — clause (e)(4)'s canary and projection differential are preconditions of admitting a host, not follow-ups (D43 (e)(4))"
  grep -qE "fn ${harness_fn}\b" "$ROOT/$harness_file" 2>/dev/null \
    || die "declared Tier-H harness '$harness' does not exist — fix the name or restore the test; clause (e)(4)'s checks are preconditions of admitting a host, not follow-ups (D43 (e)(4), (e)(5))"
done
note "Tier-H harnesses declared and present: ${#DECLARED_HOST_HARNESSES[@]}"
fi

# ── The clause (e)(5) boundary this battery does not cross ──────────────
# Recorded here rather than left to be rediscovered. The clause asks for
# single-entity semantics exposed "to witnesses and adjudication". The host
# half is enforced above and demonstrated by
# `tier_h_world_of_one.rs::the_verdict_holds_in_a_world_of_one`, which builds
# one `EcsBackend` per entity and reproduces every recorded hash from
# per-entity replay on the ECS itself.
#
# The adjudicator half is not closed and is not claimed: `verify_bundle`
# (orrery_core/src/replay.rs:331) builds its harness around `Executor::new`
# (replay.rs:106) and `authored_bundles` (orrery_games/src/diff.rs:918)
# re-executes each side's signed log through an `Executor` whatever authored
# it. On the ECS path the D-4 frames are therefore executor-authored while the
# claim values are ECS-derived. Conviction power survives — a diverging ECS
# fails D-1/D-2/D-3 and the claim values independently of the frames — but
# "the verdict must hold in a world of one" is demonstrated by a harness rather
# than embodied in the adjudicator's substrate. Closing it means making
# `verify_bundle` and `authored_bundles` backend-parametric, which is a change
# to `orrery_core` and `orrery_games`; that is a separate lane and this file
# does not pretend otherwise.

echo "$NAME: verifiable-core static gates pass"

# ── Role-discovery self-tests ───────────────────────────────────────────
# These run through the ordinary CI invocation. A --self-test mode would need
# a scripts/check.sh registration owned by another lane; keeping the fixtures
# here makes them impossible to add without also running them per commit.
make_discovery_fixture() {
  local fixture=$1 host_mode=$2 conformance_violation=$3 tier_h_app_dep=${4:-no}
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
pub trait TickBackend<R> {}
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

  if [[ $host_mode == tier_h ]]; then
    # A Tier-H shaped crate: canonical state on an ECS (`impl TickBackend` plus
    # `bevy_ecs` in the runtime graph) and no `impl Ruleset` at all, so Tier V's
    # role discovery does not see it and only the host allowlist can.
    mkdir -p "$fixture/crates/orrery_rogue_host/src" "$fixture/crates/bevy_ecs/src"
    cat >"$fixture/crates/bevy_ecs/Cargo.toml" <<'EOF'
[package]
name = "bevy_ecs"
version = "0.0.0"
edition = "2024"
EOF
    cat >"$fixture/crates/bevy_ecs/src/lib.rs" <<'EOF'
pub struct World;
EOF
    if [[ $tier_h_app_dep == yes ]]; then
      mkdir -p "$fixture/crates/bevy_app/src"
      cat >"$fixture/crates/bevy_app/Cargo.toml" <<'EOF'
[package]
name = "bevy_app"
version = "0.0.0"
edition = "2024"
EOF
      cat >"$fixture/crates/bevy_app/src/lib.rs" <<'EOF'
pub struct App;
EOF
      cat >"$fixture/crates/orrery_rogue_host/Cargo.toml" <<'EOF'
[package]
name = "orrery_rogue_host"
version = "0.0.0"
edition = "2024"

[dependencies]
bevy_app = { path = "../bevy_app" }
bevy_ecs = { path = "../bevy_ecs" }
orrery_core = { path = "../orrery_core" }
EOF
    else
      cat >"$fixture/crates/orrery_rogue_host/Cargo.toml" <<'EOF'
[package]
name = "orrery_rogue_host"
version = "0.0.0"
edition = "2024"

[dependencies]
bevy_ecs = { path = "../bevy_ecs" }
orrery_core = { path = "../orrery_core" }
EOF
    fi
    cat >"$fixture/crates/orrery_rogue_host/src/lib.rs" <<'EOF'
pub struct RogueHost {
    world: bevy_ecs::World,
}
impl orrery_core::TickBackend<()> for RogueHost {}
fn step_entity() {}
EOF
  fi

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

  # ── Tier H (D43 (e)) ──────────────────────────────────────────────────
  # (e)(1): a crate that hosts canonical state on an ECS and is not on the
  # allowlist must fail by name. The fixture's host carries no `impl Ruleset`,
  # so Tier V's role discovery cannot see it — if the allowlist check were
  # removed, this crate would sail through every other clause in this file.
  fixture="$scratch/tier-h-rogue-host"
  make_discovery_fixture "$fixture" tier_h no
  status=0
  output="$(_CORE_GATES_INTERNAL_SELF_TEST=1 \
    _CORE_GATES_TEST_ROOT="$fixture" \
    _CORE_GATES_SKIP_SELF_TESTS=1 \
    "$SCRIPT_PATH" 2>&1)" || status=$?
  if [[ ${CORE_GATES_SHOW_SELF_TEST_OUTPUT:-0} == 1 ]]; then
    printf '%s\n' '--- synthetic ECS host with no allowlist entry ---' "$output" >&2
  fi
  (( status == 1 )) \
    || die "Tier-H self-test: synthetic ECS host returned $status, expected 1"
  grep -Fq "core-gates: undeclared ECS host crate 'orrery_rogue_host'" <<<"$output" \
    || die "Tier-H self-test: an undeclared ECS host did not fail the allowlist check by name"
  note "Tier-H self-test: an unallowlisted crate hosting canonical state on an ECS fails clause (e)(1) by name"

  # (e)(2): the same host, allowlisted, with a `bevy_app` runtime dependency.
  # Declared this time, so the escape check above is satisfied and the
  # dependency gate is the only thing that can fail.
  fixture="$scratch/tier-h-app-coupled-host"
  make_discovery_fixture "$fixture" tier_h no yes
  status=0
  output="$(_CORE_GATES_INTERNAL_SELF_TEST=1 \
    _CORE_GATES_TEST_ROOT="$fixture" \
    _CORE_GATES_TEST_HOSTS=orrery_rogue_host \
    _CORE_GATES_SKIP_SELF_TESTS=1 \
    "$SCRIPT_PATH" 2>&1)" || status=$?
  if [[ ${CORE_GATES_SHOW_SELF_TEST_OUTPUT:-0} == 1 ]]; then
    printf '%s\n' '--- allowlisted ECS host with a bevy_app runtime dependency ---' "$output" >&2
  fi
  (( status == 1 )) \
    || die "Tier-H self-test: app-coupled ECS host returned $status, expected 1"
  grep -Fq "core-gates: Tier-H host allowlist: orrery_rogue_host" <<<"$output" \
    || die "Tier-H self-test: the declared fixture host was not allowlisted, so the failure below is the wrong one"
  grep -Fq "core-gates: Tier-H host 'orrery_rogue_host' has a runtime dependency on bevy_app" <<<"$output" \
    || die "Tier-H self-test: a bevy_app runtime dependency did not fail the dependency gate by name"
  note "Tier-H self-test: an allowlisted host taking bevy_app fails clause (e)(2) by name"
}

if [[ ${_CORE_GATES_SKIP_SELF_TESTS:-0} != 1 ]]; then
  run_discovery_self_tests
fi
