#!/bin/bash
# Spike #1046 chain 4 (Mac): the real cook, macOS target, loose files (bUseZenStore=False), now that the
# Metal toolchain is installed. Produces:
#   cooked-body11-cold   first cook on this machine: shader compilation paid here (its own term)
#   cooked-body11-warm   the same map again: steady state
#   cooked-body11-again  a third cook of the same source: is COOKED output byte-deterministic?
#   cooked-same-a/b      spike 2's two editor saves of one seed (out-256a, out-256b) cooked: does the
#                        5,560-byte editor-save nondeterminism survive the cook?
#   cooked-horror, cooked-firstperson   the interiors' Unreal half, cooked
#   cooked-s1, cooked-s2 the two 8-body seasons cooked as one cook each (size fit and patch on cooked bytes)
# Every cook is `UnrealEditor-Cmd -run=cook -targetplatform=Mac -map=... -NullRHI -unversioned`; wall clock
# is measured around the process; the cooker's own totals are grepped from its log beside it.
export PATH="$HOME/.cargo/bin:/opt/homebrew/bin:$PATH"
S=$HOME/Development/orrery-onebody
R=$S/s1046
P=$S/OneBodyCook
UE="/Users/Shared/Epic Games/UE_5.8"
LOG="$R/chain4.log"
log() { echo "[$(date +%H:%M:%S)] $*" | tee -a "$LOG"; }
quiet() { pkill -f CrashReportClient 2>/dev/null; sleep 1; }
snapshot() { # tag: copy Saved/Cooked/Mac and list files with sizes
  rm -rf "$R/cooked-$1"; cp -R "$P/Saved/Cooked/Mac" "$R/cooked-$1"
  ( cd "$R/cooked-$1" && find . -type f -exec stat -f '%z %N' {} \; ) | sort -rn > "$R/cooked-$1.files.txt"
  log "  cooked-$1: $(awk '{s+=$1} END {print s}' "$R/cooked-$1.files.txt") bytes in $(wc -l < "$R/cooked-$1.files.txt") files; Game: $(grep -v '/Engine/' "$R/cooked-$1.files.txt" | grep -v Metadata | awk '{s+=$1} END {print s}'); Engine: $(grep '/Engine/' "$R/cooked-$1.files.txt" | awk '{s+=$1} END {print s}'); Bodies: $(grep '/Bodies/' "$R/cooked-$1.files.txt" | awk '{s+=$1} END {print s}')"
}
cook() { # tag maps(+-separated) [extra]
  rm -rf "$P/Saved/Cooked"
  t0=$(date +%s.%N)
  "$UE/Engine/Binaries/Mac/UnrealEditor-Cmd" "$P/OneBodyCook.uproject" -run=cook -targetplatform=Mac -map=$2 -unattended -nopause -nosplash -NullRHI -stdout -NoLogTimes -unversioned $3 > "$R/cook-$1.log" 2>&1
  local rc=$?
  t1=$(date +%s.%N)
  log "cook $1 ($2, Mac) rc=$rc wall=$(echo "$t1 - $t0" | bc)"
  grep -E "Cook by the book total time|Cooked packages|shaders|Shaders|Fatal" "$R/cook-$1.log" | grep -v -E "LogShaderCompilers: Display: Worker|Warning" | tail -6 | cut -c1-220 | tee -a "$LOG"
  snapshot "$1"
  quiet
}
quiet
log "metal: $(xcrun -sdk macosx metal --version 2>&1 | head -1)"
mkdir -p "$P/Content/Bodies"; rm -f "$P/Content/Bodies"/*
cp "$R/s1/Body_11.umap" "$P/Content/Bodies/Body_11.umap"
cook body11-cold /Game/Bodies/Body_11
cook body11-warm /Game/Bodies/Body_11
cook body11-again /Game/Bodies/Body_11
log "determinism of cooked output, warm vs again: $(diff -rq "$R/cooked-body11-warm" "$R/cooked-body11-again" | grep -v Metadata | wc -l | tr -d ' ') differing files"
diff -rq "$R/cooked-body11-warm" "$R/cooked-body11-again" | tee -a "$LOG"
for f in $(cd "$R/cooked-body11-warm" && find . -type f -path '*Bodies*'); do
  n=$(cmp -l "$R/cooked-body11-warm/$f" "$R/cooked-body11-again/$f" 2>/dev/null | wc -l | tr -d ' '); log "  $f: $n differing bytes"
done
# spike 2's two editor saves of seed 1 body 2
rm -f "$P/Content/Bodies"/*; cp "$S/out-256a/Body_2.umap" "$P/Content/Bodies/Body_2.umap"; cook same-a /Game/Bodies/Body_2
rm -f "$P/Content/Bodies"/*; cp "$S/out-256b/Body_2.umap" "$P/Content/Bodies/Body_2.umap"; cook same-b /Game/Bodies/Body_2
log "editor-save nondeterminism after the cook (256a vs 256b): $(cmp -l "$S/out-256a/Body_2.umap" "$S/out-256b/Body_2.umap" | wc -l | tr -d ' ') bytes differ in the editor saves"
for f in $(cd "$R/cooked-same-a" && find . -type f -path '*Bodies*'); do
  n=$(cmp -l "$R/cooked-same-a/$f" "$R/cooked-same-b/$f" 2>/dev/null | wc -l | tr -d ' '); log "  $f: $n differing bytes after cook ($(stat -f %z "$R/cooked-same-a/$f") / $(stat -f %z "$R/cooked-same-b/$f") bytes)"
done
rm -f "$P/Content/Bodies"/*
cook horror /Game/Variant_Horror/Lvl_Horror
cook firstperson /Game/FirstPerson/Lvl_FirstPerson
# the two seasons, 8 bodies each, one cook per season
for s in s1 s2; do
  rm -f "$P/Content/Bodies"/*; cp "$R/$s"/Body_*.umap "$P/Content/Bodies/"
  cook "$s" "/Game/Bodies/Body_11+/Game/Bodies/Body_12+/Game/Bodies/Body_13+/Game/Bodies/Body_14+/Game/Bodies/Body_15+/Game/Bodies/Body_16+/Game/Bodies/Body_17+/Game/Bodies/Body_18"
done
rm -f "$P/Content/Bodies"/*
log "sizes and patches on cooked output"
for s in s1 s2; do
  ( cd "$R/cooked-$s/OneBodyCook/Content/Bodies" && tar -cf "$R/cooked-$s-bodies.tar" . )
  zstd -19 -q -f "$R/cooked-$s-bodies.tar" -o "$R/cooked-$s-bodies.tar.zst"
  ls -l "$R/cooked-$s-bodies.tar" "$R/cooked-$s-bodies.tar.zst" | tee -a "$LOG"
done
zstd -19 -q -f --patch-from="$R/cooked-s1-bodies.tar" "$R/cooked-s2-bodies.tar" -o "$R/patch-cooked-s1-s2.zstd.patch" --memory=1024MB --long=27
xdelta3 -e -f -9 -S djw -s "$R/cooked-s1-bodies.tar" "$R/cooked-s2-bodies.tar" "$R/patch-cooked-s1-s2.xdelta3"
ls -l "$R"/patch-cooked-s1-s2.* | tee -a "$LOG"
for s in s1 s2; do
  RESP="$R/cooked-$s.pakresp.txt"; : > "$RESP"
  for f in $(cd "$R/cooked-$s" && find . -type f -path '*Bodies*'); do echo "\"$R/cooked-$s/$f\" \"../../../$(echo $f | sed 's|^\./||')\"" >> "$RESP"; done
  "$UE/Engine/Binaries/Mac/UnrealPak" "$R/cooked-$s.pak" -Create="$RESP" -compress -compressionformats=Oodle -platform=Mac > "$R/cooked-$s.pak.log" 2>&1 || tail -3 "$R/cooked-$s.pak.log"
done
ls -l "$R"/cooked-s?.pak | tee -a "$LOG"
"$UE/Engine/Binaries/Mac/UnrealPak" "$R/cooked-s1.pak" -List 2>&1 | grep -E "offset:" | sed 's/LogPakFile: Display: //' | head -6 | tee -a "$LOG"
python3 - "$R" <<'EOF' | tee -a "$LOG"
import os, sys, subprocess, json
R = sys.argv[1]
rows = []
for s in ['s1', 's2']:
    base = f'{R}/cooked-{s}/OneBodyCook/Content/Bodies'
    for b in range(11, 19):
        fs = {}
        for fn in os.listdir(base):
            if fn.startswith(f'Body_{b}.'):
                p = f'{base}/{fn}'
                fs[fn.split('.', 1)[1]] = {'fs': os.path.getsize(p), 'zstd19': len(subprocess.run(['zstd', '-19', '-q', '-c', p], capture_output=True).stdout)}
        rows.append({'season': s, 'body': b, 'files': fs, 'total_fs': sum(v['fs'] for v in fs.values()), 'total_zstd19': sum(v['zstd19'] for v in fs.values())})
        print(rows[-1])
json.dump(rows, open(f'{R}/per-body-cooked.json', 'w'), indent=1)
EOF
quiet
log "CHAIN4 DONE"
