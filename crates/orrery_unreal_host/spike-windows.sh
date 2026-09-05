#!/usr/bin/env bash
# Issue #1084: the in-process predicted-tick latency at N = 24 **on Windows**,
# the platform #920's stand/overturn bands are defined on.
#
#   crates/orrery_unreal_host/spike-windows.sh [DATE] [TICKS] [REF_TICKS]
#
# The Windows twin of `spike.sh`. Same crate, same C consumer, same #920
# `ipc_added` method (N=24, 60 Hz, 600 warmup, 36,000 samples, one input per
# tick, nearest-rank percentiles, real per-frame work) — and, as the sidecar
# job does, the timer resolution reported both ways, because the sidecar pair
# showed the median indifferent to it (136.6 vs 136.8 µs) while p99.9 moved
# from 14.8 ms to 1.03 ms. There is no reason in-process behaves the same and
# no way to find out but to measure it.
#
# Three reports, into $ORRERY_SPIKE_OUT_DIR (default `docs/data/`):
#   inproc-windows-<date>-n24-time-period.json         App prong, timeBeginPeriod(1)
#   inproc-windows-<date>-n24-no-app-time-period.json  the host alone, the control
#   inproc-windows-<date>-n24-default-resolution.json  App prong, default resolution
#
# The App prong (`orrery_unreal_host`) is measured rather than the non-`App`
# prong (`orrery_unreal_direct`) because GD3's chosen configuration — "App
# prong, pool-capped, driver-connected" — is on this side of D53's fork. It is
# **not** that configuration: nothing here caps a pool and nothing connects the
# prediction driver (D53 §5), so the App sits beside the host handle rather
# than in front of it. That gap is stated in the report's notes, in the
# README, and in the nightly job, and it is not for this script to close.
set -euo pipefail

cd "$(dirname "$0")/../.."

# A Proton or Wine run reports a Windows triple while executing on Linux, and
# #1084 is explicit that such a row must never bank. The same rule applies
# here: a report claiming `platform: windows` has to have been produced by a
# Windows kernel, and the C consumer stamps that field from the compiler's
# `_WIN32`, which cross-compilation alone would satisfy. This is the host
# check that the stamp cannot make for itself.
case "$(uname -s)" in
    MINGW* | MSYS* | CYGWIN*) ;;
    *)
        echo "spike-windows.sh measures the WINDOWS leg of #1084 and must run on a Windows" >&2
        echo "host; 'uname -s' said '$(uname -s)'. Use spike.sh for the Linux reports." >&2
        exit 1
        ;;
esac

date_label=${1:-$(date +%F)}
ticks=${2:-36000}
ref_ticks=${3:-7200}
cc=${CC:-clang}
out_dir=${ORRERY_SPIKE_OUT_DIR:-docs/data}
mkdir -p "$out_dir" target/spike-1084

# The link line is *parsed* out of rustc's stderr, so it must not be
# decorated. `.github/workflows/nightly.yml` sets `CARGO_TERM_COLOR: always`
# workflow-wide, which survives the pipe: rustc then ends its
# `native-static-libs` note with an ANSI reset, the last token parses as a
# library name carrying an escape, and the C driver reports it as a library it
# cannot find — a message that sends the reader after link flags that are
# correct. #1080 fixed exactly this in the three spike crates' tests.
export CARGO_TERM_COLOR=never

# This is also the build: `cargo rustc --crate-type staticlib` emits
# target/release/orrery_unreal_host.lib. It is run FIRST and alone, because a
# preceding `cargo build` would satisfy the cache and rustc would then print
# no `native-static-libs` note at all.
print_native_libs() {
    cargo rustc --release -p orrery_unreal_host --lib --crate-type staticlib -- \
        --print native-static-libs 2>&1 | sed -n 's/.*native-static-libs: //p' | tail -1
}

native=$(print_native_libs)
if [[ -z $native ]]; then
    # A cached compile prints no note. Unlike the Linux script there is no
    # honest fallback list to fall back to — the MSVC set is long, version
    # dependent, and guessing it wrong produces link errors that read as
    # source errors — so force the note rather than invent it.
    echo "no native-static-libs note; rebuilding the staticlib to obtain one" >&2
    cargo clean --release -p orrery_unreal_host
    native=$(print_native_libs)
fi
if [[ -z $native ]]; then
    echo "rustc printed no 'native-static-libs' note; refusing to guess the MSVC link line" >&2
    exit 1
fi
echo "native-static-libs: $native"

library=target/release/orrery_unreal_host.lib
if [[ ! -f $library ]]; then
    echo "expected the staticlib at $library" >&2
    exit 1
fi

# `winmm` is not in rustc's list and cannot be: nothing on the Rust side calls
# timeBeginPeriod. The C consumer does, for #920 lie 1, so the C link line
# carries it.
consumer=target/spike-1084/spike_consumer.exe
# shellcheck disable=SC2086 # $native is a list of linker inputs
"$cc" -std=c11 -O2 -Wall -Wextra -Werror \
    -I crates/orrery_unreal_host/include -I crates/orrery_sim_host/include \
    crates/orrery_unreal_host/examples/c/spike_consumer.c \
    "$library" $native winmm.lib \
    -o "$consumer"

toolchain="rustc $(rustc --version | awk '{print $2}'), $("$cc" --version | head -1), \
$(uname -sr), commit $(git rev-parse --short HEAD)"
size=$(stat -c %s "$library")

# A render failure must not cost a measurement. #1025 lost a whole ten-minute
# Windows run to one arrow character that cp1252 could not encode, so the
# reports are all written first and a bad render is remembered and reported at
# the end rather than aborting the run. `scripts/ipc-report.py` reconfigures
# its own stdout to UTF-8 (#1027); PYTHONUTF8 makes the interpreter's default
# encoding agree with it as well, for anything the script did not reconfigure.
export PYTHONUTF8=1
render_failed=0

run() {
    local name=$1 run_ticks=$2
    shift 2
    local report="${out_dir}/inproc-windows-${date_label}-n24-${name}.json"
    echo "== $name -> $report"
    "$consumer" bench --entities 24 --ticks "$run_ticks" --warmup 600 \
        --report "$report" \
        --note "toolchain: $toolchain" \
        --note "staticlib: orrery_unreal_host.lib, $size bytes, native-static-libs: $native winmm.lib" \
        "$@"
    python3 scripts/ipc-report.py "$report" || {
        echo "render of $report failed; the report itself is written and intact" >&2
        render_failed=1
    }
}

# The headline: the App prong at #920's shape, timer resolution raised.
run time-period "$ticks" --clock manual --time-period
# The control the Linux headline (#1069: inproc-no-app, p50 20.08 µs) was
# taken on, so the two sit on one graph rather than one against the other's
# App cost.
run no-app-time-period "$ticks" --no-app --time-period
# The timer-granularity reference: same shape, default resolution, short —
# the same 7,200-tick length the sidecar job's reference leg uses.
run default-resolution "$ref_ticks" --clock manual

exit "$render_failed"
