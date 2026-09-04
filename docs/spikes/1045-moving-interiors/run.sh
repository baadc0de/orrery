#!/usr/bin/env bash
# Spike #1045, the Unreal half, on the Linux box (UE 5.8 at ~/UnrealEngine/5.8).
#
#   docs/spikes/1045-moving-interiors/run.sh build      build the staticlib (release) and the editor target
#   docs/spikes/1045-moving-interiors/run.sh maps       author the two maps (editor Python, headless)
#   docs/spikes/1045-moving-interiors/run.sh scene SCENE VARIANT [INTERIOR] [TICKS] [extra args...]
#                                                       one run under -game, fixed 60 Hz timestep
#   docs/spikes/1045-moving-interiors/run.sh all        the matrix the README reports
#
# Every run is headless by default (-NullRHI). RHI=offscreen renders with
# Vulkan and no window (-RenderOffScreen); the display variables are unset
# either way so nothing can reach the owner's Wayland session.
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
root="$(cd "$here/../../.." && pwd)"
proj="$here/unreal/MovingInteriors"
ue=${UE_ROOT:-$HOME/UnrealEngine/5.8}
editor="$ue/Engine/Binaries/Linux/UnrealEditor-Cmd"
out=${OUT:-$here/results/unreal}
rhi=${RHI:-null}
export ORRERY_REPO_ROOT="$root"
export PATH="$HOME/.cargo/bin:$PATH"

rhi_args() {
    if [[ $rhi == offscreen ]]; then
        echo "-RenderOffScreen"
    else
        echo "-NullRHI"
    fi
}

build() {
    (cd "$root" && cargo build --release -p orrery_unreal_interiors)
    "$ue/Engine/Build/BatchFiles/Linux/Build.sh" MovingInteriorsEditor Linux Development \
        -Project="$proj/MovingInteriors.uproject" -WaitMutex -NoHotReload
}

maps() {
    env -u DISPLAY -u WAYLAND_DISPLAY "$editor" "$proj/MovingInteriors.uproject" \
        -run=pythonscript -script="$proj/Scripts/make_maps.py" -NullRHI -unattended -nosplash -nopause \
        -log -stdout -FullStdOutLogOutput 2>&1 | grep -E "spike 1045|Error|error|Warning: Script" | tail -20
    ls -la "$proj/Content/Maps/"
}

scene() {
    local scene=$1 variant=$2 interior=${3:-resident} ticks=${4:-0}
    shift 4 || shift $#
    mkdir -p "$out"
    local log="$out/log-$scene-$variant-$interior.txt"
    echo "== $scene/$variant/$interior ticks=$ticks rhi=$rhi (loadavg $(cut -d' ' -f1-3 /proc/loadavg)) -> $log"
    env -u DISPLAY -u WAYLAND_DISPLAY "$editor" "$proj/MovingInteriors.uproject" /Game/Maps/MovingInteriors \
        -game "$(rhi_args)" -unattended -nosplash -nopause -nosound -windowed -ResX=960 -ResY=540 \
        -UseFixedTimeStep -FPS=60 -log -stdout -FullStdOutLogOutput \
        -InteriorsScene="$scene" -InteriorsVariant="$variant" -InteriorsInterior="$interior" \
        -InteriorsTicks="$ticks" -InteriorsOut="$out" "$@" >"$log" 2>&1 || true
    grep -E "spike 1045|LogInteriors" "$log" | head -60
}

all() {
    for variant in mirror cmc cmc_nobase cmc_drive; do
        for s in rest straight roll mech; do
            scene "$s" "$variant" resident 0
        done
    done
    for interior in resident spawn stream; do
        scene transitions mirror "$interior" 0 -InteriorsRollback=1
    done
    scene transitions cmc resident 0 -InteriorsRollback=1
}

case ${1:-} in
build) build ;;
maps) maps ;;
scene) shift; scene "$@" ;;
all) all ;;
*) sed -n 2,14p "$0"; exit 2 ;;
esac
