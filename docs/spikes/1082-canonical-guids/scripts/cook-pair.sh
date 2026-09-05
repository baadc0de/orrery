#!/bin/bash
# Cook two editor saves by the book and diff the cooked body files per file (sha1 + byte offsets).
#   A=out-a B=out-b TAGA=same-a TAGB=same-b bash cook-pair.sh
cd "$HOME/Development/orrery-onebody" || exit 1
A="${A:?A}"; B="${B:?B}"; TAGA="${TAGA:?TAGA}"; TAGB="${TAGB:?TAGB}"
R="${R:-$HOME/Development/orrery-onebody/s1082}"
export R
TAG="$TAGA" MAP="$HOME/Development/orrery-onebody/$A/Body_2.umap" bash cook-linux.sh
TAG="$TAGB" MAP="$HOME/Development/orrery-onebody/$B/Body_2.umap" bash cook-linux.sh
echo "--- cooked diff $TAGA vs $TAGB"
python3 diff-per-file.py "$R/cooked-$TAGA/OneBodyCook/Content/Bodies" "$R/cooked-$TAGB/OneBodyCook/Content/Bodies" 'Body_2.*' | tee "$R/cooked-diff-$TAGA-$TAGB.txt"
echo "diff exit: ${PIPESTATUS[0]}"
