#!/bin/bash
# After the change: two independent editor saves of seed 1 body 2 with canonical GUIDs (the default), the
# per-file diff of the editor saves, then both cooked by the book for Linux and diffed per file.
#   E=out-e F=out-f TAGE=canon-e TAGF=canon-f [ARGS=...] bash after.sh
cd "$HOME/Development/orrery-onebody" || exit 1
E="${E:-out-e}"; F="${F:-out-f}"; TAGE="${TAGE:-canon-e}"; TAGF="${TAGF:-canon-f}"
ARGS="${ARGS:--seed=1 -body=2 -size=256 -spacing=1 -density=0.03}"
for t in "$E" "$F"; do
  OUT="$HOME/Development/orrery-onebody/$t" ARGS="$ARGS" bash cookbody-linux.sh
done
echo "--- editor-save diff $E/$F"
python3 diff-per-file.py "$E" "$F" 'Body_2.umap' > "s1082/editor-diff-$TAGE-$TAGF.txt"; head -1 "s1082/editor-diff-$TAGE-$TAGF.txt"
python3 diff-per-file.py "$E" "$F" 'body-2.*.collision' | grep '^=='
A="$E" B="$F" TAGA="$TAGE" TAGB="$TAGF" bash cook-pair.sh
