#!/bin/bash
# Spike #1046 chain 2 (Mac): after the cold-DDC horror cook of interior.sh has finished, switch the cook
# to loose files (UE 5.8 defaults to bUseZenStore=True, BaseGame.ini:95, which puts cooked packages in
# zenserver rather than Saved/Cooked), rebuild the editor module with MeasureLevel, measure both
# interiors' ruleset halves, re-cook the three maps as loose files, then run the sizes/patch pass.
export PATH="$HOME/.cargo/bin:/opt/homebrew/bin:$PATH"
S=$HOME/Development/orrery-onebody
R=$S/s1046
P=$S/OneBodyCook
UE="/Users/Shared/Epic Games/UE_5.8"
LOG="$R/chain2.log"
log() { echo "[$(date +%H:%M:%S)] $*" | tee -a "$LOG"; }
quiet() { pkill -f CrashReportClient 2>/dev/null; sleep 1; }
while ! grep -q "cook horror (" "$R/interior.log"; do sleep 5; done
log "cold horror cook finished: $(grep 'cook horror (' "$R/interior.log")"
pkill -f chain.sh; pkill -f interior.sh; sleep 1; pkill -f "UnrealEditor-Cmd"; sleep 3; quiet
log "zen cooked dir contents: $(find "$P/Saved/Cooked" -type f | wc -l) files, $(du -sk "$P/Saved/Cooked" | cut -f1) KB"
find "$P/Saved/Cooked" -type f | head -20 >> "$LOG"
printf '\n[/Script/UnrealEd.ProjectPackagingSettings]\nbUseZenStore=False\nbUseIoStore=False\n' >> "$P/Config/DefaultGame.ini"
log "build module"
bash "$S/scripts-from-repo/build.sh" 2>&1 | grep -E "error|Result|Total execution" | tee -a "$LOG"
log "MeasureLevel"
for m in "1 /Game/Variant_Horror/Lvl_Horror" "2 /Game/FirstPerson/Lvl_FirstPerson"; do
  set -- $m
  "$UE/Engine/Binaries/Mac/UnrealEditor-Cmd" "$P/OneBodyCook.uproject" -run=MeasureLevel -map=$2 -id=$1 -out="$R/interior" -unattended -nopause -nosplash -NullRHI -stdout -FullStdOutLogOutput -NoLogTimes > "$R/measure-$1.log" 2>&1
  log "MeasureLevel $2 rc=$?"; grep -E "LogMeasureLevel: (Display|Error)" "$R/measure-$1.log" | tail -2 | tee -a "$LOG"; quiet
done
cook_map() { # tag map
  rm -rf "$P/Saved/Cooked"
  t0=$(date +%s.%N)
  "$UE/Engine/Binaries/Mac/UnrealEditor-Cmd" "$P/OneBodyCook.uproject" -run=cook -targetplatform=Mac -map=$2 -unattended -nopause -nosplash -NullRHI -stdout -NoLogTimes -unversioned > "$R/cook-$1.log" 2>&1
  local rc=$?
  t1=$(date +%s.%N)
  log "cook $1 ($2) rc=$rc wall=$(echo "$t1 - $t0" | bc)"
  grep -E "Cook by the book total time|Cooked packages" "$R/cook-$1.log" | tail -2 | tee -a "$LOG"
  rm -rf "$R/cooked-$1"; cp -R "$P/Saved/Cooked/Mac" "$R/cooked-$1"
  ( cd "$R/cooked-$1" && find . -type f -exec stat -f '%z %N' {} \; ) | sort -rn > "$R/cooked-$1.files.txt"
  log "  total cooked bytes: $(awk '{s+=$1} END {print s}' "$R/cooked-$1.files.txt") in $(wc -l < "$R/cooked-$1.files.txt") files; Game: $(grep -v '/Engine/' "$R/cooked-$1.files.txt" | awk '{s+=$1} END {print s}'); Engine: $(grep '/Engine/' "$R/cooked-$1.files.txt" | awk '{s+=$1} END {print s}')"
  quiet
}
cook_map horror-loose /Game/Variant_Horror/Lvl_Horror
cook_map firstperson-loose /Game/FirstPerson/Lvl_FirstPerson
cook_map body11-loose /Game/Bodies/Body_11
log "sizes"
bash "$R/measure-sizes.sh" > "$R/measure-sizes.out" 2>&1
log "sizes rc=$?"
quiet
log "CHAIN2 DONE"
