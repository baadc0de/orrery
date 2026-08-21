#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(git -C "$script_dir" rev-parse --show-toplevel)"
output="${1:-$script_dir/intent-attestation-v1.json}"

if [[ $# -gt 1 ]]; then
    echo "usage: $0 [output.json]" >&2
    exit 2
fi

build_dir="$(mktemp -d "${TMPDIR:-/tmp}/orrery-wire-vectors.XXXXXX")"
json_tmp="$(mktemp "${TMPDIR:-/tmp}/orrery-wire-vectors-json.XXXXXX")"
mkdir "$build_dir/src"
sed "s|@ORRERY_PROTOCOL_PATH@|$repo_root/crates/orrery_protocol|" \
    "$script_dir/Cargo.toml.template" >"$build_dir/Cargo.toml"
cp "$script_dir/main.rs" "$build_dir/src/main.rs"

CARGO_HOME="$build_dir/cargo-home" \
    cargo run --quiet --manifest-path "$build_dir/Cargo.toml" >"$json_tmp"
mv "$json_tmp" "$output"
echo "wrote $output" >&2
