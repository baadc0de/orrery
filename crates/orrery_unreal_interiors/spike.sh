#!/usr/bin/env bash
# Spike #1045, the engine-free half: build the staticlib in release, compile
# and link the C consumer with clang (or $CC), then
#   - trace every scene at full length (36,000 ticks; the transitions scene
#     14,400) and record the hash chain each produced, for the Unreal run to
#     be checked against;
#   - roll back across every frame change in the transitions, roll and mech
#     scenes against the stand-in authority, hash for hash, and write the
#     reports.
#
#   crates/orrery_unreal_interiors/spike.sh [OUT_DIR]
#
# Reports land in OUT_DIR (default docs/spikes/1045-moving-interiors/results).
set -euo pipefail

cd "$(dirname "$0")/../.."
out_dir=${1:-docs/spikes/1045-moving-interiors/results}
cc=${CC:-clang}

cargo build --release -p orrery_unreal_interiors
native=$(cargo rustc --release -p orrery_unreal_interiors --lib --crate-type staticlib -- \
    --print native-static-libs 2>&1 | sed -n 's/.*native-static-libs: //p' | tail -1)
if [[ -z $native ]]; then
    native='-lgcc_s -lutil -lrt -lpthread -lm -ldl -lc'
fi
echo "native-static-libs: $native"

mkdir -p target/spike-1045 "$out_dir"
# shellcheck disable=SC2086 # $native is a list of linker flags
"$cc" -std=c11 -O2 -Wall -Wextra -Werror \
    -I crates/orrery_unreal_interiors/include -I crates/orrery_unreal_interiors/examples/c \
    -I crates/orrery_sim_host/include \
    crates/orrery_unreal_interiors/examples/c/interiors_consumer.c \
    target/release/liborrery_unreal_interiors.a $native \
    -o target/spike-1045/interiors_consumer

echo "staticlib: $(stat -c %s target/release/liborrery_unreal_interiors.a) bytes"
echo "toolchain: rustc $(rustc --version | awk '{print $2}'), $($cc --version | head -1), $(uname -sr), commit $(git rev-parse --short HEAD)"

: >"$out_dir/chains.txt"
for scene in rest straight roll mech transitions; do
    echo "== trace $scene (loadavg $(cut -d' ' -f1-3 /proc/loadavg))"
    target/spike-1045/interiors_consumer trace "$scene" "" "$out_dir/trace-$scene.csv" |
        tee -a "$out_dir/chains.txt"
done

for scene in transitions roll mech straight; do
    echo "== rollback $scene (loadavg $(cut -d' ' -f1-3 /proc/loadavg))"
    target/spike-1045/interiors_consumer rollback "$scene" --report "$out_dir/rollback-$scene.json" |
        tee "$out_dir/rollback-$scene.txt"
done
# The mech scene has four frame changes, so the shape cycle (one shape per
# nine transitions) would leave mount and dismount with the identity shape
# only; run it once per divergent shape as well.
for shape in ship avatar; do
    echo "== rollback mech --shape $shape"
    target/spike-1045/interiors_consumer rollback mech --shape "$shape" --control-every 0 \
        --report "$out_dir/rollback-mech-$shape.json" | tee "$out_dir/rollback-mech-$shape.txt"
done
