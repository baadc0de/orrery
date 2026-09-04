#!/bin/bash
# Runs TRELLIS.2 on the escort concept, then imports and renders in Blender. Logs to ~/trellis-run.log and ~/blender-render.log.
S=/home/baadc0de/Development/baadc0de/orrery/game/pipeline/spikes/ortho-callouts
cd ~/TRELLIS.2 && ~/trellis2-env/bin/python $S/trellis_run.py --image $S/out/escort-pro-concept.png --out $S/out/trellis --name escort > ~/trellis-run.log 2>&1 || { echo "TRELLIS FAILED"; exit 1; }
env -u WAYLAND_DISPLAY blender -b --python $S/blender_render.py -- $S/out/trellis/escort.glb $S/out/trellis hero,front,side,top > ~/blender-render.log 2>&1 || { echo "BLENDER FAILED"; exit 1; }
echo "RENDER DONE"
