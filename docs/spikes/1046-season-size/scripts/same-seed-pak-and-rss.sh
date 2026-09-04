#!/bin/bash
# Spike #1046 — two small follow-ups on the Mac.
#  1. The unchanged-body case at UE's container granularity: pak spike 2's two cooks of one seed
#     (out-256a, out-256b) and list each entry's sha1 — UnrealPak's patching is file-granular, so an entry
#     whose bytes differ anywhere is shipped whole.
#  2. Peak RSS of one CookBody process (/usr/bin/time -l), to say whether par8 on 64 GB was core- or memory-bound.
export PATH="$HOME/.cargo/bin:/opt/homebrew/bin:$PATH"
S=$HOME/Development/orrery-onebody
R=$S/s1046
UE="/Users/Shared/Epic Games/UE_5.8"
PAK="$UE/Engine/Binaries/Mac/UnrealPak"
cd "$R" || exit 1
pkill -f "http.server 8046"
for d in a b; do
  {
    echo "\"$S/out-256$d/Body_2.umap\" \"../../../OneBodyCook/Content/Bodies/Body_2.umap\""
    echo "\"$S/out-256$d/body-2.tri.collision\" \"../../../OneBodyCook/Content/Bodies/body-2.tri.collision\""
  } > "same-$d.resp"
  "$PAK" "$R/same-$d.pak" -Create="$R/same-$d.resp" -compress -compressionformats=Oodle -platform=Mac > "same-$d.pak.log" 2>&1 || tail -3 "same-$d.pak.log"
  ls -l "same-$d.pak"
  "$PAK" "$R/same-$d.pak" -List 2>&1 | grep -E "offset:" | sed 's/LogPakFile: Display: //'
done
"$PAK" "$R/same-a.pak" "$R/same-b.pak" -Diff 2>&1 | grep -v -E "Folder|LogInit|LogConfig|LogPlugin|LogPakFile: Display: Diffing" | grep -i -E "uniq|equal|differ|match|byte" | tail -6
echo "== peak RSS of one CookBody process"
export OUT="$R/rss" ARGS="-seed=1001 -body=91 -size=256 -spacing=1 -density=0.03"
mkdir -p "$OUT"
/usr/bin/time -l "$UE/Engine/Binaries/Mac/UnrealEditor-Cmd" "$S/OneBodyCook/OneBodyCook.uproject" -run=CookBody $ARGS -out="$OUT" -unattended -nopause -nosplash -NullRHI -stdout -NoLogTimes > "$OUT/cook.out" 2> "$OUT/time.err"
grep -E "maximum resident|real|user|sys|peak memory" "$OUT/time.err"
pkill -f CrashReportClient
echo "== memory/cpu totals: $(sysctl -n hw.memsize) bytes RAM, $(sysctl -n hw.ncpu) cores"
