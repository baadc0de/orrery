#!/usr/bin/env python3
"""Prop loop: assemble -> render -> critique -> apply, N passes; keeps every pass's build/renders and the best by score."""
import argparse, json, os, shutil, subprocess, sys, time
K = os.path.dirname(os.path.abspath(__file__)); S = os.path.join(K, "..", "..", "spikes", "ortho-callouts")
ap = argparse.ArgumentParser(); ap.add_argument("out"); ap.add_argument("--passes", type=int, default=3); ap.add_argument("--project", default=os.environ.get("VERTEX_PROJECT")); a = ap.parse_args()
O = os.path.abspath(a.out); views = ["hero", "front", "side"]; best = (-1, None); hist = []
def sh(cmd): return subprocess.run(cmd, capture_output=True, text=True)
for i in range(1, a.passes + 1):
    t0 = time.time(); r = sh(["env", "-u", "WAYLAND_DISPLAY", "blender", "-b", "--python", os.path.join(K, "assemble.py"), "--", f"{O}/build.json", O]); print(f"pass {i}", next((l for l in r.stdout.splitlines() if l.startswith("ASSEMBLED")), r.stderr[-300:]))
    sh(["env", "-u", "WAYLAND_DISPLAY", "blender", "-b", "--python", os.path.join(S, "blender_render.py"), "--", f"{O}/assembly.glb", O, ",".join(views)])
    for f in ["build.json", "assembly.json"] + [f"render-{v}.png" for v in views]: shutil.copy(f"{O}/{f}", f"{O}/pass{i}-{f}")
    r = sh([sys.executable, os.path.join(K, "critique.py"), "--project", a.project, "--build", f"{O}/build.json", "--assembly", f"{O}/assembly.json", "--renders", *[f"{O}/render-{v}.png" for v in views], "--out", f"{O}/build.next.json"])
    print(r.stdout.rstrip() or r.stderr[-800:])
    if r.returncode: break
    score = json.load(open(f"{O}/build.next.json"))["critiques"][-1]["score"]; hist.append(score)
    if score > best[0]: best = (score, i)
    shutil.move(f"{O}/build.next.json", f"{O}/build.json"); print(f"   {time.time() - t0:.0f}s")
if best[1]: shutil.copy(f"{O}/pass{best[1]}-build.json", f"{O}/best-build.json")
print("ITERATE-DONE scores", hist, "best", best)
