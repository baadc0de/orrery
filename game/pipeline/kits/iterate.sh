#!/bin/bash
# iterate.sh <out_dir> <iterations> : assemble -> render -> critique -> apply, N times. Uses out_dir/{hull.blend,zones.json,choices.json}.
set -e; O=$1; N=${2:-2}; K=/home/baadc0de/Development/baadc0de/orrery/game/pipeline/kits; S=/home/baadc0de/Development/baadc0de/orrery/game/pipeline/spikes/ortho-callouts; M=$HOME/assets/kitops/Orrery_Masterfolder
for i in $(seq 1 $N); do
  env -u WAYLAND_DISPLAY blender -b --python $K/assemble_hull.py -- $M $O/hull.blend $O/zones.json $O/choices.json $O 7 6000 2>&1 | grep -E '^ASSEMBLED'
  env -u WAYLAND_DISPLAY blender -b --python $S/blender_render.py -- $O/assembly.glb $O hero,side,top 2>&1 | grep -c rendered >/dev/null
  cp $O/render-hero.png $O/iter${i}-hero.png; cp $O/render-top.png $O/iter${i}-top.png
  python3 $K/critique.py --project "$VERTEX_PROJECT" --concept $S/out/escort-pro-concept.png --renders $O/render-hero.png $O/render-side.png $O/render-top.png --zones $O/zones.json --assembly $O/assembly.json --atlas $O/hull_atlas.json --out $O/zones.next.json
  cp $O/zones.json $O/zones.iter${i}.json; mv $O/zones.next.json $O/zones.json
  python3 $K/choose_parts.py --project "$VERTEX_PROJECT" --model gemini-3.5-flash --master $M --zones $O/zones.json --concept $S/out/escort-pro-concept.png --out $O/choices.json >/dev/null 2>&1 || true
done
env -u WAYLAND_DISPLAY blender -b --python $K/assemble_hull.py -- $M $O/hull.blend $O/zones.json $O/choices.json $O 7 6000 2>&1 | grep -E '^ASSEMBLED'
env -u WAYLAND_DISPLAY blender -b --python $S/blender_render.py -- $O/assembly.glb $O hero,side,top 2>&1 | grep -c rendered >/dev/null; echo ITERATE-DONE
