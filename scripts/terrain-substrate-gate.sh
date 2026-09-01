#!/usr/bin/env bash
# Refuse the removed v1 terrain substrate from returning as an unused surface.
#
#   ./scripts/terrain-substrate-gate.sh              check the production tree
#   ./scripts/terrain-substrate-gate.sh --self-test  prove both mutations fail
#
# D51 is deliberately proposed: this implementation follows the owner's #830
# decision while the ADR remains awaiting acceptance. The guarded stage is
# concrete nevertheless. A terrain record or a `k` key family must not return
# as a variant/key that folds to nothing and consumes a byte nobody can afford.
set -euo pipefail

readonly NAME=terrain-substrate-gate
die() { echo "$NAME: $*" >&2; exit 1; }

# The scan below deliberately turns an empty result into a passing result, so
# every external command that produces that result must be present before we
# inspect the tree.  Otherwise a missing command can masquerade as a source
# finding (or, worse, an empty scan).
require_tool() {
    command -v "$1" >/dev/null 2>&1 || die "required tool is not on PATH: $1"
}

for tool in basename dirname find grep mkdir mktemp rm sed sort; do
    require_tool "$tool"
done

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SCRIPT_PATH="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/$(basename "${BASH_SOURCE[0]}")"
if [[ ${_TERRAIN_SUBSTRATE_INTERNAL_SELF_TEST:-0} == 1 ]]; then
    ROOT=${_TERRAIN_SUBSTRATE_TEST_ROOT:?internal self-test requires a fixture root}
fi

note() { echo "$NAME: $*" >&2; }

usage() {
    sed -n '2,/^set -euo/p' "$0" | sed -e 's/^# \{0,1\}//' -e '/^set -euo/d' >&2
}

require_anchor() {
    local file=$1 pattern=$2 label=$3
    [[ -f $file ]] || die "anti-vacuity: missing $label source: ${file#"$ROOT/"}"
    grep -qE -- "$pattern" "$file" \
        || die "anti-vacuity: $label source no longer carries its expected anchor"
}

# Read only the production half of each Rust module. Unit tests may name a
# retired spelling to prove it is rejected; they are not a returned surface.
scan_production_sources() {
    local pattern=$1 file
    while IFS= read -r file; do
        sed '/^#\[cfg(test)\]/,$d' "$file" \
            | grep -nE -- "$pattern" \
            | sed "s|^|${file#"$ROOT/"}:|" || true
    done < <(find \
        "$ROOT/crates/orrery_protocol/src" \
        "$ROOT/crates/orrery_persistd/src" \
        "$ROOT/crates/orrery_seed/src" \
        -type f -name '*.rs' | sort)
}

check_absence() {
    local protocol="$ROOT/crates/orrery_protocol/src/persist.rs"
    local keyspace="$ROOT/crates/orrery_persistd/src/keyspace.rs"
    local wipe="$ROOT/crates/orrery_seed/src/wipe.rs"

    # These anchors prove the scan reaches the live three seams, rather than a
    # deleted path or an empty fixture passing by agreement with the guard.
    require_anchor "$protocol" 'pub enum RecordKind' 'RecordKind'
    require_anchor "$keyspace" 'fn registered_families' 'keyspace registry'
    require_anchor "$wipe" 'pub async fn run' 'seeder wipe'

    local matches
    if matches=$(scan_production_sources '\bTerrainDelta\b'); [[ -n $matches ]]; then
        die "removed RecordKind::TerrainDelta reintroduced:\n$matches"
    fi
    if matches=$(scan_production_sources '\bSectionCtx\b|\bencode_section\b|chunk/'); [[ -n $matches ]]; then
        die "removed chunk/ seeder surface reintroduced:\n$matches"
    fi

    # Unit tests are deliberately excluded: a test may mention a retired byte
    # as a negative fixture. This production-only scan catches all registered
    # and constructor forms the keyspace completeness guard recognizes, so a
    # `k` family cannot come back under a less obvious function name.
    if matches=$(sed '/^#\[cfg(test)\]/,$d' "$keyspace" \
        | grep -nE "chunk_(key|range_start|range_end)|name: \"chunk\"|prefix: b'k'|key\\[0\\] = b'k'|key\\.push\\(b'k'\\)|key\\[\\.\\.2\\]\\.copy_from_slice\\(b\"k"); then
        die "removed chunk/ key prefix b'k' reintroduced:\n$matches"
    fi

    note 'terrain substrate absent: scanned RecordKind, seeder surface, and production keyspace constructors'
}

self_test() {
    local scratch fixture output status
    scratch=$(mktemp -d)
    trap 'rm -rf "$scratch"' RETURN
    fixture="$scratch/fixture"
    mkdir -p \
        "$fixture/crates/orrery_protocol/src" \
        "$fixture/crates/orrery_persistd/src" \
        "$fixture/crates/orrery_seed/src"

    printf '%s\n' 'pub enum RecordKind { Spawn }' \
        >"$fixture/crates/orrery_protocol/src/persist.rs"
    printf '%s\n' \
        'pub fn registered_families() {}' \
        "fn world_key() { let mut key = [0; 1]; key[0] = b'w'; }" \
        >"$fixture/crates/orrery_persistd/src/keyspace.rs"
    printf '%s\n' 'pub async fn run() {}' >"$fixture/crates/orrery_seed/src/wipe.rs"

    output="$(_TERRAIN_SUBSTRATE_INTERNAL_SELF_TEST=1 \
        _TERRAIN_SUBSTRATE_TEST_ROOT="$fixture" \
        "$SCRIPT_PATH" 2>&1)" || die "self-test: clean fixture failed:\n$output"
    grep -Fq 'terrain substrate absent: scanned RecordKind, seeder surface, and production keyspace constructors' <<<"$output" \
        || die 'self-test: clean fixture did not report the guarded stage'

    printf '%s\n' 'pub enum RecordKind { Spawn, TerrainDelta }' \
        >"$fixture/crates/orrery_protocol/src/persist.rs"
    status=0
    output="$(_TERRAIN_SUBSTRATE_INTERNAL_SELF_TEST=1 \
        _TERRAIN_SUBSTRATE_TEST_ROOT="$fixture" \
        "$SCRIPT_PATH" 2>&1)" || status=$?
    (( status == 1 )) \
        || die "self-test: TerrainDelta mutation returned $status, expected 1"
    grep -Fq 'removed RecordKind::TerrainDelta reintroduced:' <<<"$output" \
        || die 'self-test: TerrainDelta mutation did not fail by name'
    note 'self-test: TerrainDelta mutation fails by name'

    printf '%s\n' 'pub enum RecordKind { Spawn }' \
        >"$fixture/crates/orrery_protocol/src/persist.rs"
    printf '%s\n' \
        'pub fn registered_families() {}' \
        "fn terrain_key() { let mut key = [0; 1]; key[0] = b'k'; }" \
        >"$fixture/crates/orrery_persistd/src/keyspace.rs"
    status=0
    output="$(_TERRAIN_SUBSTRATE_INTERNAL_SELF_TEST=1 \
        _TERRAIN_SUBSTRATE_TEST_ROOT="$fixture" \
        "$SCRIPT_PATH" 2>&1)" || status=$?
    (( status == 1 )) \
        || die "self-test: chunk-key mutation returned $status, expected 1"
    grep -Fq "removed chunk/ key prefix b'k' reintroduced:" <<<"$output" \
        || die 'self-test: chunk-key mutation did not fail by name'
    note "self-test: chunk/ key prefix b'k' mutation fails by name"
}

case "${1:-}" in
    '') check_absence ;;
    --self-test) self_test ;;
    -h | --help) usage ;;
    *) usage; die "unknown argument: $1" ;;
esac
