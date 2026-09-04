#!/usr/bin/env bash
# Spike #1052, the non-App prong: build the staticlib in release, compile and
# link the C consumer with clang (or $CC), run the measurement twice, and
# render each report with scripts/ipc-report.py.
#
#   crates/orrery_unreal_direct/spike.sh [DATE] [TICKS] [OUT_DIR]
#
# Runs, each N=24 at 60 Hz, 600 warmup ticks, 36,000 sampled ticks (10 min):
#   predict   the host with the D8 ring, a stand-in authority and a
#             correction every 12 ticks (5 Hz), depth cycling 1..9
#   control   the host alone — no ring, the exact shape of #1043's no-app run
#
# Reports land in OUT_DIR (default docs/data) as
# direct-linux-<date>-n24-<run>.json. They are informational: Linux, not the
# in-process Unreal number. The loadavg at start and end is inside each
# report; a run taken on a loaded box is not evidence and should not be
# committed under docs/data.
set -euo pipefail

cd "$(dirname "$0")/../.."
date_label=${1:-$(date +%F)}
ticks=${2:-36000}
out_dir=${3:-docs/data}
cc=${CC:-clang}

cargo build --release -p orrery_unreal_direct
# What the link actually needs, from rustc, not assumed.
native=$(cargo rustc --release -p orrery_unreal_direct --lib --crate-type staticlib -- \
    --print native-static-libs 2>&1 | sed -n 's/.*native-static-libs: //p' | tail -1)
if [[ -z $native ]]; then
    native='-lgcc_s -lutil -lrt -lpthread -lm -ldl -lc'
fi
echo "native-static-libs: $native"

mkdir -p target/spike-1052 "$out_dir"
# shellcheck disable=SC2086 # $native is a list of linker flags
"$cc" -std=c11 -O2 -Wall -Wextra -Werror \
    -I crates/orrery_unreal_direct/include -I crates/orrery_sim_host/include \
    crates/orrery_unreal_direct/examples/c/direct_consumer.c \
    target/release/liborrery_unreal_direct.a $native \
    -o target/spike-1052/direct_consumer

toolchain="rustc $(rustc --version | awk '{print $2}'), $($cc --version | head -1), $(uname -sr), commit $(git rev-parse --short HEAD)"
size=$(stat -c %s target/release/liborrery_unreal_direct.a)
echo "staticlib: $size bytes"

run() {
    local name=$1
    shift
    local report="$out_dir/direct-linux-${date_label}-n24-${name}.json"
    echo "== $name -> $report (loadavg $(cut -d' ' -f1-3 /proc/loadavg))"
    target/spike-1052/direct_consumer bench --entities 24 --ticks "$ticks" --warmup 600 \
        --report "$report" \
        --note "toolchain: $toolchain" \
        --note "staticlib: liborrery_unreal_direct.a, $size bytes, native-static-libs: $native" \
        "$@"
    python3 scripts/ipc-report.py "$report"
}

run predict
run control --no-ring
