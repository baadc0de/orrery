set -o pipefail
UE="/Users/Shared/Epic Games/UE_5.8"
PROJ="$HOME/Development/orrery-onebody/OneBodyCook"
OUT="${OUT:-$HOME/Development/orrery-onebody/out}"
ARGS="${ARGS:--seed=1 -body=1 -size=64 -spacing=1 -density=0.02}"
mkdir -p "$OUT"
LOG="$OUT/cook-$(date +%s).log"
/usr/bin/time -p "$UE/Engine/Binaries/Mac/UnrealEditor-Cmd" "$PROJ/OneBodyCook.uproject" -run=CookBody $ARGS -out="$OUT" -unattended -nopause -nosplash -NullRHI -stdout -FullStdOutLogOutput -NoLogTimes > "$LOG" 2>&1
echo "exit: $? log: $LOG"
grep -E "LogCookBody|LogOrreryExport|LogMegaMesh.*(Error|Warning)|Error:|Fatal|Assertion|Ensure|LogPCG.*(Error|Warning)|real |user " "$LOG" | grep -v "LogShaderCompilers" | tail -60
