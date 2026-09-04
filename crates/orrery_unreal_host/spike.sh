#!/usr/bin/env bash
# Spike #1043, engine-independent half: build the staticlib in release,
# compile and link the C consumer with clang (or $CC), run the measurement
# three times, and render each report with scripts/ipc-report.py.
#
#   crates/orrery_unreal_host/spike.sh [DATE] [TICKS]
#
# Runs, each N=24 at 60 Hz, 600 warmup ticks, 36,000 sampled ticks (10 min):
#   manual   the App with Bevy's clock fed by the C accumulator
#   auto     the App with Bevy on the wall clock
#   no-app   the host alone — the control
#
# Reports land in docs/data/inproc-linux-<date>-n24-<run>.json. They are
# informational: Linux, not the in-process Unreal number.
set -euo pipefail

cd "$(dirname "$0")/../.."
date_label=${1:-$(date +%F)}
ticks=${2:-36000}
cc=${CC:-clang}

cargo build --release -p orrery_unreal_host
# What the link actually needs, from rustc, not assumed (#1043 output 4).
native=$(cargo rustc --release -p orrery_unreal_host --lib --crate-type staticlib -- \
    --print native-static-libs 2>&1 | sed -n 's/.*native-static-libs: //p' | tail -1)
if [[ -z $native ]]; then
    native='-lgcc_s -lutil -lrt -lpthread -lm -ldl -lc'
fi
echo "native-static-libs: $native"

mkdir -p target/spike-1043
# shellcheck disable=SC2086 # $native is a list of linker flags
"$cc" -std=c11 -O2 -Wall -Wextra -Werror \
    -I crates/orrery_unreal_host/include -I crates/orrery_sim_host/include \
    crates/orrery_unreal_host/examples/c/spike_consumer.c \
    target/release/liborrery_unreal_host.a $native \
    -o target/spike-1043/spike_consumer

toolchain="rustc $(rustc --version | awk '{print $2}'), $($cc --version | head -1), $(uname -sr), commit $(git rev-parse --short HEAD)"
size=$(stat -c %s target/release/liborrery_unreal_host.a)

run() {
    local name=$1
    shift
    local report="docs/data/inproc-linux-${date_label}-n24-${name}.json"
    echo "== $name -> $report"
    target/spike-1043/spike_consumer bench --entities 24 --ticks "$ticks" --warmup 600 \
        --report "$report" \
        --note "toolchain: $toolchain" \
        --note "staticlib: liborrery_unreal_host.a, $size bytes, native-static-libs: $native" \
        "$@"
    python3 scripts/ipc-report.py "$report"
}

run manual --clock manual
run auto --clock auto
run no-app --no-app
