#!/bin/bash
# One CookBody run (spike 2's commandlet) on the Linux box: an editor save of one seed into $OUT.
#   OUT=... ARGS="-seed=1 -body=2 -size=256 -spacing=1 -density=0.03 [-deterministicguids]" bash cookbody-linux.sh
# No window: -NullRHI, and the display variables are unset so nothing can reach the Wayland session.
set -o pipefail
UE="$HOME/UnrealEngine/5.8"
PROJ="$HOME/Development/orrery-onebody/OneBodyCook"
OUT="${OUT:-$HOME/Development/orrery-onebody/out}"
ARGS="${ARGS:--seed=1 -body=2 -size=256 -spacing=1 -density=0.03}"
mkdir -p "$OUT"
LOG="$OUT/cook-$(date +%s).log"
t0=$(date +%s.%N)
env -u WAYLAND_DISPLAY -u DISPLAY "$UE/Engine/Binaries/Linux/UnrealEditor-Cmd" "$PROJ/OneBodyCook.uproject" -run=CookBody $ARGS -out="$OUT" -unattended -nopause -nosplash -NullRHI -stdout -FullStdOutLogOutput -NoLogTimes > "$LOG" 2>&1
rc=$?
t1=$(date +%s.%N)
pkill -f CrashReportClient 2>/dev/null
echo "exit: $rc wall: $(python3 -c "print(round($t1 - $t0, 1))") s log: $LOG"
grep -E "LogCookBody|Error:|Fatal|Assertion|Ensure condition" "$LOG" | grep -v "LogShaderCompilers" | tail -30
