#!/bin/bash
# Spike #1046 — the interior share I: two hand-authored levels from Epic's UE 5.8 FirstPerson template
# (Variant_Horror/Lvl_Horror = corridors and rooms built from LevelPrototyping cubes, 82 actors;
# FirstPerson/Lvl_FirstPerson = the open arena, 63 actors), copied into spike 2's OneBodyCook project so
# one editor module serves both commandlets. Steps:
#   1. copy template content in (levels, their external actors, LevelPrototyping meshes/materials)
#   2. rebuild the editor module with MeasureLevel added
#   3. MeasureLevel on both levels -> level-<id>.tri.collision + level-<id>.measure.json (ruleset half)
#   4. -run=cook -targetplatform=Mac for each level and for one PCG body (Body_11 copied under /Game/Bodies)
#      -> the *cooked* Unreal half, per asset, in Saved/Cooked/Mac/OneBodyCook (editor-saved .umap bytes are not what ships)
export PATH="$HOME/.cargo/bin:/opt/homebrew/bin:$PATH"
S=$HOME/Development/orrery-onebody
R=$S/s1046
P=$S/OneBodyCook
UE="/Users/Shared/Epic Games/UE_5.8"
T="$UE/Templates/TP_FirstPerson/Content"
LP="$UE/Templates/TemplateResources/High/LevelPrototyping/Content"
LOG="$R/interior.log"
log() { echo "[$(date +%H:%M:%S)] $*" | tee -a "$LOG"; }
quiet() { pkill -f CrashReportClient 2>/dev/null; sleep 1; }

log "step 1: content"
mkdir -p "$P/Content/__ExternalActors__" "$P/Content/__ExternalObjects__" "$P/Content/Bodies" "$P/Content/LevelPrototyping"
cp -R "$T/Variant_Horror" "$P/Content/"
cp -R "$T/FirstPerson" "$P/Content/"
cp -R "$T/__ExternalActors__/Variant_Horror" "$T/__ExternalActors__/FirstPerson" "$P/Content/__ExternalActors__/"
cp -R "$T/__ExternalObjects__/Variant_Horror" "$T/__ExternalObjects__/FirstPerson" "$P/Content/__ExternalObjects__/" 2>/dev/null
cp -R "$LP"/* "$P/Content/LevelPrototyping/"
cp "$R/s1/Body_11.umap" "$P/Content/Bodies/Body_11.umap"
du -sk "$P/Content"/* | tee -a "$LOG"

log "step 2: build editor module"
bash "$S/build.sh" 2>&1 | tail -5 | tee -a "$LOG"
ls -la "$P/Binaries/Mac/" | tail -3

log "step 3: MeasureLevel"
for m in "1 /Game/Variant_Horror/Lvl_Horror" "2 /Game/FirstPerson/Lvl_FirstPerson"; do
  set -- $m
  t0=$(date +%s.%N)
  "$UE/Engine/Binaries/Mac/UnrealEditor-Cmd" "$P/OneBodyCook.uproject" -run=MeasureLevel -map=$2 -id=$1 -out="$R/interior" -unattended -nopause -nosplash -NullRHI -stdout -FullStdOutLogOutput -NoLogTimes > "$R/measure-$1.log" 2>&1
  t1=$(date +%s.%N)
  log "MeasureLevel $2 exit=$? wall=$(echo "$t1 - $t0" | bc)"
  grep -E "LogMeasureLevel" "$R/measure-$1.log" | tail -3 | tee -a "$LOG"
  quiet
done
ls -l "$R/interior/"

log "step 4: cook by the book, Mac target, per map (cold DDC for the interiors' meshes/materials; the body is warm from its own cook)"
cook_map() { # tag map
  rm -rf "$P/Saved/Cooked"
  t0=$(date +%s.%N)
  "$UE/Engine/Binaries/Mac/UnrealEditor-Cmd" "$P/OneBodyCook.uproject" -run=cook -targetplatform=Mac -map=$2 -unattended -nopause -nosplash -NullRHI -stdout -NoLogTimes -unversioned > "$R/cook-$1.log" 2>&1
  local rc=$?
  t1=$(date +%s.%N)
  log "cook $1 ($2) exit=$rc wall=$(echo "$t1 - $t0" | bc)"
  grep -E "Cook by the book total time|Cooked packages|LogCook: Display: Cook by the book" "$R/cook-$1.log" | tail -3 | tee -a "$LOG"
  rm -rf "$R/cooked-$1"; cp -R "$P/Saved/Cooked/Mac" "$R/cooked-$1"
  ( cd "$R/cooked-$1" && find . -type f -printf '%s %p\n' 2>/dev/null || find . -type f -exec stat -f '%z %N' {} \; ) | sort -rn > "$R/cooked-$1.files.txt"
  echo "  total cooked bytes: $(awk '{s+=$1} END {print s}' "$R/cooked-$1.files.txt") in $(wc -l < "$R/cooked-$1.files.txt") files; Game share: $(grep -v '/Engine/' "$R/cooked-$1.files.txt" | awk '{s+=$1} END {print s}'); Engine share: $(grep '/Engine/' "$R/cooked-$1.files.txt" | awk '{s+=$1} END {print s}')" | tee -a "$LOG"
  quiet
}
cook_map horror /Game/Variant_Horror/Lvl_Horror
cook_map horror-warm /Game/Variant_Horror/Lvl_Horror
cook_map firstperson /Game/FirstPerson/Lvl_FirstPerson
cook_map body11 /Game/Bodies/Body_11
log "INTERIOR DONE"
