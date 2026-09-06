#!/usr/bin/env python3
"""Library gap filler: subassembly sheet -> TRELLIS.2 mesh -> KIT OPS INSERT in a generated kpack, labelled from the zone.
Crops the large 3/4 view from the sheet, runs image-to-3D, stages the glb under ~/assets/kits/generated/<asset>/1/extracted/
(private store staging, never the repo), converts it with kits_to_inserts.py and appends labels.json so the chooser's tag pool
finds it. usage: sheet_to_insert.py --zones Z --sheets DIR --asset escort [--only zone ...] [--master M]"""
import argparse, json, os, shutil, subprocess, sys, time
K = os.path.dirname(os.path.abspath(__file__)); S = os.path.join(K, "..", "spikes", "ortho-callouts")
ap = argparse.ArgumentParser(); ap.add_argument("--zones", required=True); ap.add_argument("--sheets", required=True); ap.add_argument("--asset", required=True)
ap.add_argument("--only", nargs="*", default=None); ap.add_argument("--master", default=os.path.expanduser("~/assets/kitops/Orrery_Masterfolder")); ap.add_argument("--budget", type=int, default=12000)
ap.add_argument("--trellis-python", default=os.path.expanduser("~/trellis2-env/bin/python")); ap.add_argument("--trellis-dir", default=os.path.expanduser("~/TRELLIS.2")); ap.add_argument("--force", action="store_true"); a = ap.parse_args()
Z = json.load(open(a.zones)); kit = os.path.expanduser(f"~/assets/kits/generated/{a.asset}/1"); ex = os.path.join(kit, "extracted"); os.makedirs(ex, exist_ok=True); kpack = f"gen-{a.asset}"
man = os.path.join(kit, "manifest.json")
if not os.path.exists(man): json.dump({"kit_id": f"generated/{a.asset}", "vendor": "generated", "licence": "project-owned (generated from the project's own concept sheets)", "created": time.strftime("%Y-%m-%d"), "sources": {}}, open(man, "w"), indent=1)
M = json.load(open(man)); lab_path = os.path.join(a.master, kpack, "labels.json"); os.makedirs(os.path.dirname(lab_path), exist_ok=True); L = json.load(open(lab_path)) if os.path.exists(lab_path) else {}
for z in Z["zones"]:
    if z.get("kind", "part") == "paint" or (a.only and z["name"] not in a.only): continue
    sheet = os.path.join(a.sheets, z["name"] + ".png"); glb = os.path.join(ex, z["name"] + ".glb")
    if not os.path.exists(sheet): print("no sheet", z["name"]); continue
    if os.path.exists(glb) and not a.force: print("have", glb); continue
    W, H = [int(v) for v in subprocess.check_output(["magick", "identify", "-format", "%w %h", sheet], text=True).split()]
    crop = os.path.join(ex, z["name"] + "-34view.png"); subprocess.run(["magick", sheet, "-crop", f"{int(W * 0.58)}x{int(H * 0.9)}+0+{int(H * 0.05)}", "+repage", crop], check=True)
    t0 = time.time(); r = subprocess.run([a.trellis_python, os.path.join(S, "trellis_run.py"), "--image", crop, "--out", ex, "--name", z["name"], "--decimate", "40000", "--texture", "1024"], cwd=a.trellis_dir, capture_output=True, text=True)
    if r.returncode or not os.path.exists(glb): print("TRELLIS failed", z["name"], r.stderr[-800:]); continue
    print(f"trellis {z['name']} {time.time() - t0:.0f}s"); M["sources"][z["name"]] = {"sheet": os.path.abspath(sheet), "zone": z["name"], "feature": z.get("concept_feature")}
json.dump(M, open(man, "w"), indent=1)
r = subprocess.run(["env", "-u", "WAYLAND_DISPLAY", "blender", "-b", "--python", os.path.join(K, "kits_to_inserts.py"), "--", kit, "*.glb", a.master, kpack, str(a.budget)], capture_output=True, text=True)
print("\n".join(l for l in r.stdout.splitlines() if l.startswith(("INSERT", "wrote", "done")) or "Error" in l)[-1500:])
feats = json.load(open(os.path.join(a.master, kpack, "features.json"))) if os.path.exists(os.path.join(a.master, kpack, "features.json")) else {}
for z in Z["zones"]:
    nm = f"{kpack}_{z['name']}"
    if nm in feats and nm not in L: L[nm] = {"heuristic": "generated", "tags": z["tags"], "confidence": 1.0, "source": "sheet->trellis", "note": z.get("concept_feature", ""), "zone": z["name"], "straggler": False}
json.dump(L, open(lab_path, "w"), indent=1); print("inserts in", os.path.join(a.master, kpack), "labelled", len(L))
