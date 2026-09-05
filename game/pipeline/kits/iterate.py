#!/usr/bin/env python3
"""Critique-loop driver: assemble -> render (shaded + zone-id) -> critique -> apply -> re-choose, N passes, hill-climbing.
Keeps the best-scoring program in <out>/best/; a pass that scores below the best by more than --tolerance is reverted to the best
program with a new chooser seed, so a bad critic verdict cannot drag the assembly down. Reports the score trajectory.
usage: iterate.py <out_dir> [--passes N] [--project P] [--concept PNG]"""
import argparse, json, os, shutil, subprocess, sys, time
K = os.path.dirname(os.path.abspath(__file__)); S = os.path.join(K, "..", "spikes", "ortho-callouts"); M = os.path.expanduser("~/assets/kitops/Orrery_Masterfolder")
ap = argparse.ArgumentParser(); ap.add_argument("out"); ap.add_argument("--passes", type=int, default=3); ap.add_argument("--project", default=os.environ.get("VERTEX_PROJECT"), help="Vertex project; env VERTEX_PROJECT"); ap.add_argument("--concept", default=os.path.join(S, "out", "escort-pro-concept.png"))
ap.add_argument("--tolerance", type=float, default=1.0); ap.add_argument("--budget", type=int, default=6000); ap.add_argument("--views", default="hero,side,top,belly"); a = ap.parse_args()
O = os.path.abspath(a.out); best_dir = os.path.join(O, "best"); os.makedirs(best_dir, exist_ok=True); views = a.views.split(",")
def sh(cmd, quiet=True, **kw):
    r = subprocess.run(cmd, capture_output=True, text=True, **kw)
    if r.returncode != 0 and not quiet: print(r.stdout[-2000:], r.stderr[-2000:])
    return r
def blender(script, *args):
    return sh(["env", "-u", "WAYLAND_DISPLAY", "blender", "-b", "--python", script, "--", *map(str, args)])
def assemble(seed):
    r = blender(os.path.join(K, "assemble_hull.py"), M, f"{O}/hull.blend", f"{O}/zones.json", f"{O}/choices.json", O, seed, a.budget)
    line = next((l for l in r.stdout.splitlines() if l.startswith("ASSEMBLED")), r.stderr[-300:]); print("  ", line)
def render():
    blender(os.path.join(S, "blender_render.py"), f"{O}/assembly.glb", O, ",".join(views)); blender(os.path.join(S, "blender_render.py"), f"{O}/assembly_id.glb", O, ",".join(views), "flat")
def choose(seed):
    r = sh([sys.executable, os.path.join(K, "choose_parts.py"), "--project", a.project, "--master", M, "--zones", f"{O}/zones.json", "--concept", a.concept, "--out", f"{O}/choices.json", "--seed", str(seed)])
    for l in r.stdout.splitlines(): print("   ", l)
    if r.returncode: print(r.stderr[-800:])
def critique(i):
    r = sh([sys.executable, os.path.join(K, "critique.py"), "--project", a.project, "--concept", a.concept, "--renders", *[f"{O}/render-{v}.png" for v in views], "--id-renders", *[f"{O}/render-{v}-id.png" for v in views],
            "--zones", f"{O}/zones.json", "--assembly", f"{O}/assembly.json", "--atlas", f"{O}/hull_atlas.json", "--out", f"{O}/zones.next.json"])
    for l in r.stdout.splitlines(): print("   ", l)
    if r.returncode: print(r.stderr[-1200:]); return None
    Z = json.load(open(f"{O}/zones.next.json")); return Z["critiques"][-1]["score"]
def snapshot(tag):
    for f in ["zones.json", "choices.json", "assembly.json"] + [f"render-{v}.png" for v in views]:
        if os.path.exists(f"{O}/{f}"): shutil.copy(f"{O}/{f}", f"{O}/{tag}-{f}")
best = json.load(open(f"{best_dir}/score.json"))["score"] if os.path.exists(f"{best_dir}/score.json") else -1.0
hist = []; seed = 7
if not os.path.exists(f"{O}/choices.json"): choose(seed)
for i in range(1, a.passes + 1):
    t0 = time.time(); print(f"pass {i}"); assemble(seed); render(); score = critique(i)
    if score is None: break
    hist.append(score); snapshot(f"pass{i}")
    if score >= best - 1e-9:
        best = score; json.dump({"score": score, "pass": i, "seed": seed}, open(f"{best_dir}/score.json", "w"))
        for f in ["zones.json", "choices.json", "assembly.json", "assembly.glb", "assembly.blend"] + [f"render-{v}.png" for v in views]: shutil.copy(f"{O}/{f}", f"{best_dir}/{f}")
        print(f"   best so far {score}")
    elif score < best - a.tolerance:   # off by default (tolerance 1.0): the critic's score is noisy by ~0.15 on an unchanged program, so reverting on it discards every adjustment
        print(f"   regression {score} < best {best}: reverting to best program, new seed"); shutil.copy(f"{best_dir}/zones.json", f"{O}/zones.next.json"); seed += 1
    shutil.copy(f"{O}/zones.json", f"{O}/zones.pass{i}.json"); shutil.move(f"{O}/zones.next.json", f"{O}/zones.json")
    Z = json.load(open(f"{O}/zones.json"))
    for z in Z["zones"]: z.pop("rechoose", None)
    json.dump(Z, open(f"{O}/zones.json", "w"), indent=1); choose(seed)   # sizes and hints changed: always re-rank
    print(f"   {time.time() - t0:.0f}s")
print("final assembly from best program"); shutil.copy(f"{best_dir}/zones.json", f"{O}/zones.json"); shutil.copy(f"{best_dir}/choices.json", f"{O}/choices.json"); assemble(json.load(open(f"{best_dir}/score.json"))["seed"]); render()
print("ITERATE-DONE scores", hist, "best", best)
