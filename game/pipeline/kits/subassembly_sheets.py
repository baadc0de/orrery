#!/usr/bin/env python3
"""Concept stage, per subassembly: for every part zone of the program, generate one isolated three-view callout sheet of that
feature alone, in the concept's design language (Vertex image model, gcloud auth, provenance per sheet). The chooser and the
critic then judge parts against a clean reference instead of a crop of the whole painting, and a sheet can feed image-to-3D
when the library has no such part. Writes <out>/sheets-sub/<zone>.png (+ .provenance.json); existing sheets are kept."""
import argparse, hashlib, json, os, sys
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "spikes", "ortho-callouts"))
from ortho_callouts import generate, img_part, sha256
ap = argparse.ArgumentParser(); ap.add_argument("--project", default=os.environ.get("VERTEX_PROJECT")); ap.add_argument("--model", default="gemini-3-pro-image"); ap.add_argument("--location", default="global")
ap.add_argument("--zones", required=True); ap.add_argument("--concept", required=True); ap.add_argument("--crops", default=None, help="dir of per-zone concept crops (choose_parts.py writes <out>/crops)"); ap.add_argument("--out", required=True)
ap.add_argument("--only", nargs="*", default=None); ap.add_argument("--force", action="store_true"); a = ap.parse_args()
Z = json.load(open(a.zones)); concept = open(a.concept, "rb").read(); sdir = os.path.join(a.out, "sheets-sub"); os.makedirs(sdir, exist_ok=True)
STYLE = "utilitarian military spacecraft hardware: panelled, riveted, warm-grey painted steel with dark polymer panels and bare-aluminium fasteners, safety-orange only where the concept has it"
made = 0
for z in Z["zones"]:
    if z.get("kind", "part") == "paint" or (a.only and z["name"] not in a.only): continue
    out_png = os.path.join(sdir, f"{z['name']}.png")
    if os.path.exists(out_png) and not a.force: continue
    size = f"about {z['size_m']} m across" if z.get("size_m") else "at the size it has in the concept"
    n = z.get("count", 1); crop_p = os.path.join(a.crops, f"{z['name']}.png") if a.crops else None; refs = [concept] + ([open(crop_p, "rb").read()] if crop_p and os.path.exists(crop_p) else [])
    prompt = (f"Image 1 is the concept painting of a 9 m spacecraft{'; image 2 is a crop around the feature' if len(refs) > 1 else ''}. Isolate ONE subassembly of it: {z.get('concept_feature', z['name'])} "
              f"({', '.join(z['tags'])}), {size}{', one of ' + str(n) + ' identical units' if n > 1 else ''}. Draw a clean production callout sheet of ONLY that subassembly, with no hull around it: "
              f"three views at identical scale on a flat light-grey background: front-left 3/4 view large on the left, side view and front view smaller on the right, each with a small view label. "
              f"Keep the exact design language of the concept ({STYLE}); show its mounting face where it meets the hull as a flat base. No other text, no dimensions, no people, no background scenery.")
    prov = {"stage": "subassembly-sheet", "zone": z["name"], "feature": z.get("concept_feature"), "size_m": z.get("size_m"), "prompt": prompt, "references": [sha256(r) for r in refs], "concept": os.path.basename(a.concept)}
    generate(a.project, a.location, a.model, [{"text": prompt}] + [img_part(r) for r in refs], out_png, prov, temperature=0.5); made += 1; print("sheet", z["name"])
print("sheets made", made, "->", sdir)
