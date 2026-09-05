#!/bin/bash
# Cook one editor-saved body map by the book for the Linux target, loose files (bUseZenStore=False):
#   TAG=same-a MAP=$HOME/Development/orrery-onebody/out-a/Body_2.umap bash cook-linux.sh
# Result: $R/cooked-$TAG/ (a copy of Saved/Cooked/Linux) and $R/cook-$TAG.log.
set -o pipefail
UE="$HOME/UnrealEngine/5.8"
PROJ="$HOME/Development/orrery-onebody/OneBodyCook"
R="${R:-$HOME/Development/orrery-onebody/s1082}"
TAG="${TAG:?TAG}"
MAP="${MAP:?MAP}"
mkdir -p "$R" "$PROJ/Content/Bodies"
rm -f "$PROJ/Content/Bodies"/*
cp "$MAP" "$PROJ/Content/Bodies/$(basename "$MAP")"
NAME="$(basename "$MAP" .umap)"
rm -rf "$PROJ/Saved/Cooked"
t0=$(date +%s.%N)
env -u WAYLAND_DISPLAY -u DISPLAY "$UE/Engine/Binaries/Linux/UnrealEditor-Cmd" "$PROJ/OneBodyCook.uproject" -run=cook -targetplatform=Linux -map="/Game/Bodies/$NAME" -unattended -nopause -nosplash -NullRHI -stdout -NoLogTimes -unversioned > "$R/cook-$TAG.log" 2>&1
rc=$?
t1=$(date +%s.%N)
pkill -f CrashReportClient 2>/dev/null
echo "cook $TAG ($NAME, Linux) rc=$rc wall=$(python3 -c "print(round($t1 - $t0, 1))") s"
grep -E "Cook by the book total time|Cooked packages|ShadersCompiled|Fatal|Error:" "$R/cook-$TAG.log" | grep -v -E "LogShaderCompilers: Display: Worker" | tail -8 | cut -c1-200
rm -rf "$R/cooked-$TAG"; cp -R "$PROJ/Saved/Cooked/Linux" "$R/cooked-$TAG"
ls -l "$R/cooked-$TAG/OneBodyCook/Content/Bodies/"
rm -f "$PROJ/Content/Bodies"/*
