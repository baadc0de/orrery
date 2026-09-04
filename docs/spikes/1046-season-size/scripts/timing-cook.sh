#!/bin/bash
# Spike #1046 — the cook(n) timing pass, on a quiet machine.
# Spike 2's CookBody trips an ensure at shutdown (WorldSubsystem.cpp:118 "!bInitialized") and leaves a
# CrashReportClient spinning at ~45% CPU per cook; eight of them cost a 10-core M1 Max a third of its
# throughput (walls crept 23 -> 29 s in the size pass). This pass kills the reporter after every cook
# so each wall is measured on the same machine state. One process per body (cook.sh, -NullRHI).
S=$HOME/Development/orrery-onebody
R=$S/s1046
mkdir -p "$R"
export OUT ARGS
LOG="$R/timing-cook.log"
log() { echo "[$(date +%H:%M:%S)] $*" | tee -a "$LOG"; }
quiet() { pkill -f CrashReportClient 2>/dev/null; sleep 1; }
one() { # seed body outdir
  OUT="$R/$3"; ARGS="-seed=$1 -body=$2 -size=256 -spacing=1 -density=0.03"
  local t0=$(date +%s.%N)
  bash "$S/cook.sh" > "$R/$3-body$2.cook.out" 2>&1
  local t1=$(date +%s.%N)
  log "seed=$1 body=$2 out=$3 wall=$(echo "$t1 - $t0" | bc) $(grep -E '^real ' "$R/$3-body$2.cook.out" | tail -1) main=$(python3 -c "import json,sys; print(json.load(open('$OUT/body-$2.cook.json'))['timing']['commandlet_main_s'])")"
}
quiet
log "start timing pass; load: $(uptime | sed 's/.*load/load/')"
# engine start alone: the commandlet with no body (bad args -> exits after engine init)
t0=$(date +%s.%N)
"/Users/Shared/Epic Games/UE_5.8/Engine/Binaries/Mac/UnrealEditor-Cmd" "$S/OneBodyCook/OneBodyCook.uproject" -run=TraceBody -unattended -nopause -nosplash -NullRHI -stdout -NoLogTimes > "$R/enginestart.out" 2>&1
t1=$(date +%s.%N)
log "engine start + exit (TraceBody with no args) wall=$(echo "$t1 - $t0" | bc)"
quiet
for b in 41 42 43 44 45 46 47 48; do one 1001 $b t1; quiet; done
log "sequential 8 done"
t0=$(date +%s.%N); for b in 51 52; do one 1001 $b tp2 & done; wait; t1=$(date +%s.%N)
log "par2 wall total=$(echo "$t1 - $t0" | bc)"; quiet
t0=$(date +%s.%N); for b in 61 62 63 64; do one 1001 $b tp4 & done; wait; t1=$(date +%s.%N)
log "par4 wall total=$(echo "$t1 - $t0" | bc)"; quiet
t0=$(date +%s.%N); for b in 71 72 73 74 75 76 77 78; do one 1001 $b tp8 & done; wait; t1=$(date +%s.%N)
log "par8 wall total=$(echo "$t1 - $t0" | bc)"; quiet
log "TIMING DONE"
