#!/bin/bash
# The whole measurement for one cooked body dir: rays -> Unreal trace (complex, simple) -> ruleset trace per rep -> compare -> digest -> sizes
export PATH="$HOME/.cargo/bin:/opt/homebrew/bin:$PATH"
UE="/Users/Shared/Epic Games/UE_5.8"
PROJ="$HOME/Development/orrery-onebody/OneBodyCook"
OUT="${OUT:-$HOME/Development/orrery-onebody/out}"
BODY="${BODY:-1}"
N="${N:-5000}"
RAYSEED="${RAYSEED:-42}"
CT="$HOME/Development/orrery-onebody/rust/target/release/collision-trace"
cd "$OUT" || exit 1
set -o pipefail
echo "== rays"
"$CT" rays --collision "body-$BODY.tri.collision" --out "rays-$BODY.bin" --n "$N" --seed "$RAYSEED" | tee "rays-$BODY.json" | head -3
for complex in 1 0; do
  echo "== unreal trace complex=$complex"
  LOG="trace-$BODY-c$complex.log"
  /usr/bin/time -p "$UE/Engine/Binaries/Mac/UnrealEditor-Cmd" "$PROJ/OneBodyCook.uproject" -run=TraceBody -map="$OUT/Body_$BODY.umap" -rays="$OUT/rays-$BODY.bin" -out="$OUT/hits-$BODY-unreal-c$complex.bin" -complex=$complex -unattended -nopause -nosplash -NullRHI -stdout -FullStdOutLogOutput -NoLogTimes > "$LOG" 2>&1
  echo "exit $?"; grep -E "LogTraceBody|real " "$LOG" | grep -v Callstack | tail -6
done
for rep in tri hf vox; do
  echo "== ruleset trace $rep"
  "$CT" trace --collision "body-$BODY.$rep.collision" --rays "rays-$BODY.bin" --out "hits-$BODY-rust-$rep.bin" > "trace-$BODY-rust-$rep.json"; cat "trace-$BODY-rust-$rep.json"
  for complex in 1 0; do
    "$CT" compare --unreal "hits-$BODY-unreal-c$complex.bin" --rust "hits-$BODY-rust-$rep.bin" --rays "rays-$BODY.bin" --label "$rep vs unreal complex=$complex" --out "compare-$BODY-$rep-c$complex.json" > /dev/null
    python3 - "compare-$BODY-$rep-c$complex.json" <<'EOF'
import json,sys
d=json.load(open(sys.argv[1]))
print("  ", d["label"], "| both_hit", d["both_hit"], "both_miss", d["both_miss"], "U-hit/R-miss", d["unreal_hit_rust_miss"], "R-hit/U-miss", d["rust_hit_unreal_miss"], "| diff-actor", d["both_hit_different_actor"], "| below-surface", d["origin_below_surface"])
print("   rate:", ", ".join(f"τ={r['tau_mm']}: {r['rate']:.4f}" for r in d["rate"]), "| excl. below-surface:", ", ".join(f"{r['rate']:.4f}" for r in d["agreement_excluding_below_surface"]))
print("   |Δd| max", d["max_abs_delta_mm"], "p50", d["p50_abs_delta_mm"], "p99", d["p99_abs_delta_mm"], "| hist", [(b["upto_mm"], b["count"]) for b in d["abs_delta_histogram"]])
print("   worst:", [(w["ray"], w["unreal_mm"], w["rust_mm"], w["cause"]) for w in d["worst"][:4]])
EOF
  done
done
echo "== digest"
"$CT" digest --unreal "Body_$BODY.umap" --collision "body-$BODY.tri.collision" --collision "body-$BODY.hf.collision" --collision "body-$BODY.vox.collision" --out "digest-$BODY.json" | grep -E "digest_blake3|flip_check_passed"
echo "== sizes"
"$CT" sizes --file "Body_$BODY.umap" --file "body-$BODY.tri.collision" --file "body-$BODY.hf.collision" --file "body-$BODY.vox.collision" --out "sizes-$BODY.json"
