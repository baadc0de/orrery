#!/bin/bash
# Baseline for #1082 on Linux: two editor saves of seed 1 body 2 without the flag (out-a, out-b) and two
# with spike 2's -deterministicguids as committed (out-c, out-d), then the per-file diff of each pair.
cd "$HOME/Development/orrery-onebody" || exit 1
for t in a b; do
  OUT="$HOME/Development/orrery-onebody/out-$t" ARGS="-seed=1 -body=2 -size=256 -spacing=1 -density=0.03" bash cookbody-linux.sh
done
for t in c d; do
  OUT="$HOME/Development/orrery-onebody/out-$t" ARGS="-seed=1 -body=2 -size=256 -spacing=1 -density=0.03 -deterministicguids" bash cookbody-linux.sh
done
echo "--- editor-save diff a/b (no flag)"
python3 diff-per-file.py out-a out-b 'Body_2.umap' | head -3
echo "--- editor-save diff c/d (-deterministicguids as committed)"
python3 diff-per-file.py out-c out-d 'Body_2.umap' | head -3
echo "--- ruleset halves a/b"
python3 diff-per-file.py out-a out-b 'body-2.*.collision' | grep '^=='
