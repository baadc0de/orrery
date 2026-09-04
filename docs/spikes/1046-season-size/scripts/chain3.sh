#!/bin/bash
# Spike #1046 chain 3 (Mac): the cook-by-the-book pass, re-targeted. The Mac-target cook died after 617 s in
# Metal shader compilation ("cannot execute tool 'metal' due to missing Metal Toolchain" — Xcode 26 ships it
# as a separate download this spike does not install on the owner's machine). Windows and Linux targets
# cross-compile their shaders with DXC, which the installed build carries, so the cooked-bytes measurement is
# taken for the Windows target (the client platform the game actually ships on) — loose files, bUseZenStore=False.
export PATH="$HOME/.cargo/bin:/opt/homebrew/bin:$PATH"
S=$HOME/Development/orrery-onebody
R=$S/s1046
P=$S/OneBodyCook
UE="/Users/Shared/Epic Games/UE_5.8"
LOG="$R/chain3.log"
log() { echo "[$(date +%H:%M:%S)] $*" | tee -a "$LOG"; }
quiet() { pkill -f CrashReportClient 2>/dev/null; sleep 1; }
pkill -9 -f UnrealEditor-Cmd; sleep 2; quiet
TP=Windows
ls "$UE/Engine/Binaries/Mac/" | grep -q "libUnrealEditor-WindowsTargetPlatform" || TP=Linux
log "target platform for the cook: $TP (present: $(ls "$UE/Engine/Binaries/Mac/" | grep -E 'libUnrealEditor-(Windows|Linux)TargetPlatform\.' | tr '\n' ' '))"
cook_map() { # tag map
  rm -rf "$P/Saved/Cooked"
  t0=$(date +%s.%N)
  "$UE/Engine/Binaries/Mac/UnrealEditor-Cmd" "$P/OneBodyCook.uproject" -run=cook -targetplatform=$TP -map=$2 -unattended -nopause -nosplash -NullRHI -stdout -NoLogTimes -unversioned > "$R/cook-$1.log" 2>&1
  local rc=$?
  t1=$(date +%s.%N)
  log "cook $1 ($2, $TP) rc=$rc wall=$(echo "$t1 - $t0" | bc)"
  grep -E "Cook by the book total time|Cooked packages|Shaders Compiled|shaders compiled|Fatal" "$R/cook-$1.log" | tail -3 | tee -a "$LOG"
  rm -rf "$R/cooked-$1"; cp -R "$P/Saved/Cooked/$TP" "$R/cooked-$1" 2>/dev/null
  ( cd "$R/cooked-$1" 2>/dev/null && find . -type f -exec stat -f '%z %N' {} \; ) | sort -rn > "$R/cooked-$1.files.txt"
  log "  total cooked bytes: $(awk '{s+=$1} END {print s}' "$R/cooked-$1.files.txt") in $(wc -l < "$R/cooked-$1.files.txt") files; Game: $(grep -v '/Engine/' "$R/cooked-$1.files.txt" | awk '{s+=$1} END {print s}'); Engine: $(grep '/Engine/' "$R/cooked-$1.files.txt" | awk '{s+=$1} END {print s}')"
  quiet
}
cook_map horror-cold /Game/Variant_Horror/Lvl_Horror
cook_map horror-warm /Game/Variant_Horror/Lvl_Horror
cook_map firstperson /Game/FirstPerson/Lvl_FirstPerson
cook_map body11 /Game/Bodies/Body_11
log "sizes"
bash "$R/measure-sizes.sh" > "$R/measure-sizes.out" 2>&1
log "sizes rc=$?"
quiet
log "CHAIN3 DONE"
