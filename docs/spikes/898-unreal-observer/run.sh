#!/usr/bin/env bash
# Spike #898 step 3: two sidecars, one Unreal observer, and the A9 P-4 kill.
#
#   run.sh build              cargo staticlib, then UnrealBuildTool
#   run.sh map                author /Game/Maps/OrreryObserver headlessly
#   run.sh observe [TICKS]    two sidecars + the Unreal observer, TICKS ticks
#   run.sh paced [TICKS] [N] [HZ]
#                             ONE sidecar carrying N predicted entities, and an
#                             observer paced to HZ by sleeping to a deadline.
#                             The comparable shape (#1106): N and the tick rate
#                             match what examples/extract_cost.rs measures, so
#                             the crossing figure sits beside #1100's rather
#                             than beside a free-running poll of an idle link.
#   run.sh kill [TICKS]       the same, then SIGKILL the *editor* mid-run and
#                             check the sidecars' canonical run is untouched
#
# Every run is headless (-NullRHI) and both display variables are unset, so
# nothing can reach the owner's Wayland session — xvfb-run does not hide a
# window on this host, and clients have appeared on the desktop that way.
# RHI=offscreen renders with Vulkan and still opens no window.
set -uo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
root="$(cd "$here/../../.." && pwd)"
proj="$here/unreal/OrreryObserver"
ue="${UE_ROOT:-$HOME/UnrealEngine/5.8}"
editor="$ue/Engine/Binaries/Linux/UnrealEditor-Cmd"
out="${OUT:-$here/results}"
rhi="${RHI:-null}"
export ORRERY_REPO_ROOT="$root"

mkdir -p "$out"

rhi_args() {
    if [[ $rhi == offscreen ]]; then
        echo "-RenderOffScreen"
    else
        echo "-NullRHI"
    fi
}

# The sidecars. Each prints the port it took on its first line; the port is
# OS-chosen, which is why this script needs no port lease.
sidecar_pids=()
sidecar_addrs=()
start_sidecars() {
    local log
    for spec in "21 1 --stand-in-remote 42" "22 7"; do
        # shellcheck disable=SC2086 # the spec is a deliberate word list
        set -- $spec
        local seed=$1 entity=$2
        shift 2
        log="$out/sidecar-$seed.log"
        : >"$log"
        "$root/target/release/orrery-sidecar" --serve 127.0.0.1:0 --seed "$seed" \
            --entity "$entity" "$@" >"$log" 2>&1 &
        sidecar_pids+=("$!")
        local addr=""
        for _ in $(seq 1 100); do
            sleep 0.2
            addr=$(grep -m1 -o 'serving ipc on .*' "$log" | cut -d' ' -f4)
            [[ -n $addr ]] && break
        done
        [[ -n $addr ]] || { echo "sidecar seed=$seed never bound a port" >&2; exit 1; }
        sidecar_addrs+=("$addr")
        echo "sidecar seed=$seed pid=${sidecar_pids[-1]} addr=$addr"
    done
}

# One sidecar carrying a whole population, which is the shape #1100's
# `extract_cost` harness measures: 24 predicted entities in one app, ids 1..24.
# Two sidecars of 12 would be two extractions and two links, and would not be
# the same measurement.
start_population() {
    local entities=$1 log="$out/sidecar-pop.log"
    : >"$log"
    "$root/target/release/orrery-sidecar" --serve 127.0.0.1:0 --seed 21 \
        --entity 1 --entities "$entities" >"$log" 2>&1 &
    sidecar_pids+=("$!")
    local addr=""
    for _ in $(seq 1 100); do
        sleep 0.2
        addr=$(grep -m1 -o 'serving ipc on .*' "$log" | cut -d' ' -f4)
        [[ -n $addr ]] && break
    done
    [[ -n $addr ]] || { echo "the population sidecar never bound a port" >&2; exit 1; }
    sidecar_addrs+=("$addr")
    echo "sidecar entities=$entities pid=${sidecar_pids[-1]} addr=$addr"
}

stop_sidecars() {
    for pid in "${sidecar_pids[@]:-}"; do
        [[ -n $pid ]] && kill "$pid" 2>/dev/null
    done
    wait 2>/dev/null
}

# The canonical run, read out of a sidecar's own frames rather than out of its
# logs: the observer binary is the instrument, and it is a different process
# from the one being checked.
sample_tick() {
    "$root/target/release/orrery-observer" --addr "$1" --frames 1 --print-every 1 2>/dev/null \
        | sed -n 's/.*tick=\([0-9]*\).*/\1/p' | head -1
}

case "${1:-}" in
build)
    (cd "$root" && cargo build --release -p orrery_unreal_observer -p orrery_sidecar) || exit 1
    "$ue/Engine/Build/BatchFiles/Linux/Build.sh" OrreryObserverEditor Linux Development \
        -Project="$proj/OrreryObserver.uproject" -WaitMutex -NoHotReload
    ;;
map)
    env -u DISPLAY -u WAYLAND_DISPLAY "$editor" "$proj/OrreryObserver.uproject" \
        -run=pythonscript -script="$proj/Scripts/make_map.py" \
        -NullRHI -unattended -nosplash -nopause -log -stdout -FullStdOutLogOutput 2>&1 \
        | grep -E "spike 898|Error|error|Warning: Script" | tail -20
    ;;
observe)
    ticks="${2:-600}"
    start_sidecars
    sleep 1
    echo "== observer: $ticks ticks against ${sidecar_addrs[*]}"
    env -u DISPLAY -u WAYLAND_DISPLAY "$editor" "$proj/OrreryObserver.uproject" \
        /Game/Maps/OrreryObserver -game "$(rhi_args)" \
        -unattended -nosplash -nopause -nosound -windowed -ResX=960 -ResY=540 \
        -UseFixedTimeStep -FPS=60 -log -stdout -FullStdOutLogOutput \
        -ObserverAddr="${sidecar_addrs[0]}" -ObserverAddr="${sidecar_addrs[1]}" \
        -ObserverTicks="$ticks" -ObserverOut="$out" >"$out/observer.log" 2>&1
    echo "editor exit: $?"
    grep -E "spike 898" "$out/observer.log" | tail -20
    stop_sidecars
    ;;
paced)
    ticks="${2:-36000}"
    entities="${3:-24}"
    hz="${4:-60}"
    echo "loadavg at start: $(cut -d' ' -f1-3 /proc/loadavg)"
    start_population "$entities"
    sleep 1
    echo "== paced observer: $ticks ticks at $hz Hz, N=$entities, against ${sidecar_addrs[0]}"
    env -u DISPLAY -u WAYLAND_DISPLAY "$editor" "$proj/OrreryObserver.uproject" \
        /Game/Maps/OrreryObserver -game "$(rhi_args)" \
        -unattended -nosplash -nopause -nosound -windowed -ResX=960 -ResY=540 \
        -UseFixedTimeStep -FPS="$hz" -log -stdout -FullStdOutLogOutput \
        -ObserverAddr="${sidecar_addrs[0]}" \
        -ObserverTicks="$ticks" -ObserverHz="$hz" -ObserverOut="$out" \
        >"$out/observer-paced.log" 2>&1
    echo "editor exit: $?"
    echo "loadavg at end:   $(cut -d' ' -f1-3 /proc/loadavg)"
    grep -E "spike 898" "$out/observer-paced.log" | tail -25
    stop_sidecars
    ;;
kill)
    ticks="${2:-0}"
    start_sidecars
    sleep 1
    echo "== observer (to be killed) against ${sidecar_addrs[*]}"
    env -u DISPLAY -u WAYLAND_DISPLAY "$editor" "$proj/OrreryObserver.uproject" \
        /Game/Maps/OrreryObserver -game "$(rhi_args)" \
        -unattended -nosplash -nopause -nosound -windowed -ResX=960 -ResY=540 \
        -UseFixedTimeStep -FPS=60 -log -stdout -FullStdOutLogOutput \
        -ObserverAddr="${sidecar_addrs[0]}" -ObserverAddr="${sidecar_addrs[1]}" \
        -ObserverTicks="$ticks" -ObserverOut="$out" >"$out/observer-kill.log" 2>&1 &
    editor_pid=$!

    # Wait until it is genuinely observing, so the kill lands on a live
    # renderer rather than on a process still loading the engine.
    for _ in $(seq 1 300); do
        sleep 0.5
        grep -q "spike 898: capsule for" "$out/observer-kill.log" && break
    done
    grep -q "spike 898: capsule for" "$out/observer-kill.log" \
        || { echo "the observer never rendered a capsule" >&2; stop_sidecars; exit 1; }

    before_a=$(sample_tick "${sidecar_addrs[0]}")
    before_b=$(sample_tick "${sidecar_addrs[1]}")
    echo "before the kill: sidecar ticks $before_a and $before_b"

    echo "== SIGKILL $editor_pid"
    kill -9 "$editor_pid"
    wait "$editor_pid" 2>/dev/null
    echo "editor is gone: $(kill -0 "$editor_pid" 2>/dev/null && echo NO || echo yes)"

    sleep 3
    after_a=$(sample_tick "${sidecar_addrs[0]}")
    after_b=$(sample_tick "${sidecar_addrs[1]}")
    echo "after the kill:  sidecar ticks $after_a and $after_b"

    status=0
    for pair in "$before_a $after_a A" "$before_b $after_b B"; do
        # shellcheck disable=SC2086
        set -- $pair
        if [[ -z ${1:-} || -z ${2:-} ]]; then
            echo "FAIL: sidecar $3 could not be sampled across the kill"
            status=1
        elif (( $2 > $1 )); then
            echo "PASS: sidecar $3 advanced $1 -> $2 with its observer dead"
        else
            echo "FAIL: sidecar $3 did not advance ($1 -> $2)"
            status=1
        fi
    done
    stop_sidecars
    exit "$status"
    ;;
*)
    sed -n '2,12p' "$0"
    exit 2
    ;;
esac
