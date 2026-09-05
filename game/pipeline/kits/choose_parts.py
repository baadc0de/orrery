#!/usr/bin/env python3
"""Visual part choice: for each zone, rank candidate INSERT thumbnails against the concept art (Vertex AI Gemini, gcloud auth).
Writes choices.json: zone -> ordered insert names with the model's reasons. Candidates come from tags + geometry filters.
Each zone is judged against a CROP of the concept (bounding boxes located by one vision call, cached in <out>.boxes.json) plus the
full concept for context; the prompt states the zone's size in metres, its hull region, and any critic hint. Inserts a critic
rejected ("exclude" on the zone) are never candidates again."""
import argparse, base64, glob, json, os, random, subprocess, urllib.request
FLAT = {"hull-panel", "plate", "hatch", "grille", "vent", "strip", "rib"}
def token(): return subprocess.check_output(["gcloud", "auth", "print-access-token"], text=True).strip()
def img(p): return {"inlineData": {"mimeType": "image/png", "data": base64.b64encode(open(p, "rb").read()).decode()}}
def call(project, model, parts, schema=None, max_tokens=4096, temp=0.1):
    gc = {"temperature": temp, "responseMimeType": "application/json", "maxOutputTokens": max_tokens, "thinkingConfig": {"thinkingBudget": 0}}
    if schema: gc["responseSchema"] = schema
    url = f"https://aiplatform.googleapis.com/v1/projects/{project}/locations/global/publishers/google/models/{model}:generateContent"
    req = urllib.request.Request(url, data=json.dumps({"contents": [{"role": "user", "parts": parts}], "generationConfig": gc}).encode(), headers={"Authorization": f"Bearer {token()}", "Content-Type": "application/json"})
    with urllib.request.urlopen(req, timeout=180) as r: resp = json.loads(r.read())
    c0 = resp["candidates"][0]
    if "content" not in c0: raise RuntimeError(f"no content: finish={c0.get('finishReason')} usage={resp.get('usageMetadata')}")
    txt = "".join(p.get("text", "") for p in c0["content"]["parts"]); txt = txt[txt.find("{"): txt.rfind("}") + 1]
    return json.loads(txt), resp
ap = argparse.ArgumentParser(); ap.add_argument("--project", required=True); ap.add_argument("--model", default="gemini-3.5-flash"); ap.add_argument("--master", required=True)
ap.add_argument("--zones", required=True); ap.add_argument("--concept", required=True); ap.add_argument("--out", required=True); ap.add_argument("--candidates", type=int, default=8); ap.add_argument("--seed", type=int, default=7)
ap.add_argument("--no-crops", action="store_true"); a = ap.parse_args()
random.seed(a.seed); Z = json.load(open(a.zones)); lib = []
for kp in sorted(glob.glob(os.path.join(a.master, "*"))):
    fp, lp = os.path.join(kp, "features.json"), os.path.join(kp, "labels.json")
    if not os.path.exists(fp): continue
    F = json.load(open(fp)); L = json.load(open(lp)) if os.path.exists(lp) else {}
    for n, f in F.items():
        l = L.get(n, {}); lib.append({"name": n, "png": os.path.join(kp, n + ".png"), "tags": l.get("tags", []), "note": l.get("note", ""), "zone": l.get("zone"), "conf": l.get("confidence", 0), "planar": f["planar_fraction"], "below": f.get("below_plane", 0), "dims": f["dims"], "attach": f.get("attach", ["surface"])})
# 1. locate every zone's feature in the concept once (Gemini box convention: [ymin, xmin, ymax, xmax] in 0..1000)
crops = {}
boxes_path = os.path.splitext(a.out)[0] + ".boxes.json"
if not a.no_crops:
    feats = {z["name"]: z.get("concept_feature", z["name"]) for z in Z["zones"] if z.get("kind", "part") != "paint"}
    if os.path.exists(boxes_path): boxes = json.load(open(boxes_path))
    else:
        schema = {"type": "object", "properties": {"boxes": {"type": "array", "items": {"type": "object", "properties": {"zone": {"type": "string"}, "box_2d": {"type": "array", "items": {"type": "integer"}}, "visible": {"type": "boolean"}}, "required": ["zone", "box_2d", "visible"]}}}, "required": ["boxes"]}
        prompt = (f"Locate each listed feature of the spacecraft in this concept image. Return one entry per feature with box_2d as [ymin, xmin, ymax, xmax] in 0..1000 normalised coordinates, tight around the feature; "
                  f"visible=false if the feature is not visible in this view (then still give your best-guess box). Features: {json.dumps(feats)}")
        try:
            v, _ = call(a.project, a.model, [{"text": prompt}, img(a.concept)], schema, 4096, 0.0); boxes = {b["zone"]: b for b in v["boxes"]}
        except Exception as e: print("box locate failed:", str(e)[:120]); boxes = {}
        json.dump(boxes, open(boxes_path, "w"), indent=1)
    W, H = [int(x) for x in subprocess.check_output(["magick", "identify", "-format", "%w %h", a.concept], text=True).split()]
    cdir = os.path.join(os.path.dirname(a.out), "crops"); os.makedirs(cdir, exist_ok=True)
    for zn, b in boxes.items():
        y0, x0, y1, x1 = [max(0, min(1000, int(v))) for v in b["box_2d"][:4]]
        pad = 0.25; bw, bh = (x1 - x0) / 1000 * W, (y1 - y0) / 1000 * H
        if bw < 8 or bh < 8: continue
        cx, cy = (x0 + x1) / 2000 * W, (y0 + y1) / 2000 * H; side = max(bw, bh) * (1 + 2 * pad); side = max(side, 160)
        x, y = max(0, cx - side / 2), max(0, cy - side / 2); side = min(side, W - x, H - y)
        out_png = os.path.join(cdir, f"{zn}.png"); subprocess.run(["magick", a.concept, "-crop", f"{int(side)}x{int(side)}+{int(x)}+{int(y)}", "+repage", "-resize", "512x512", out_png], check=True)
        crops[zn] = {"png": out_png, "visible": b.get("visible", True)}
# 2. rank candidates per zone: pool by tags (+ neighbour tags and note keywords when a critic hint exists) -> contact-sheet shortlist -> thumbnail ranking
NEIGH = {"thruster": ["nozzle", "cylinder", "tank"], "nozzle": ["thruster", "cylinder"], "landing-gear": ["strut", "bracket", "fin", "frame"], "strut": ["bracket", "frame", "landing-gear"],
         "cable": ["pipe", "conduit"], "pipe": ["conduit", "cable"], "conduit": ["pipe", "cable"], "gun": ["box", "launcher", "turret", "block"], "pylon": ["bracket", "strut", "fin", "frame"],
         "block": ["box", "greeble-cluster"], "box": ["block", "greeble-cluster"], "greeble-cluster": ["block", "vent"], "plate": ["hull-panel", "strip"], "hull-panel": ["plate", "strip"], "fin": ["wing", "plate"]}
STOP = set("the a an and or of to with for in on at is are be not need needs look looks like current currently part parts this that it its as from than more much very too should must sits sit into replace replaced them they".split())
FONT = os.environ.get("SHEET_FONT", "/usr/share/fonts/noto/NotoSans-Regular.ttf")
def shortlist(z, pool, crop, hint, want):
    sheets = []; sdir = os.path.join(os.path.dirname(a.out), "sheets"); os.makedirs(sdir, exist_ok=True)
    for si in range(0, len(pool), 24):
        chunk = pool[si:si + 24]; sp = os.path.join(sdir, f"{z['name']}-{si // 24}.png")
        cmd = ["magick", "montage"] + sum([["-label", str(si + j + 1), x["png"]] for j, x in enumerate(chunk)], []) + ["-tile", "6x4", "-geometry", "224x224+4+4", "-background", "#202020", "-fill", "white", "-pointsize", "28", "-font", FONT, sp]
        subprocess.run(cmd, check=True, capture_output=True); sheets.append(sp)
    prompt = (f"Contact sheets of untextured kitbash parts, each numbered under its thumbnail (numbers run across sheets, 1..{len(pool)}). Image {'SHEET-REF (the feature drawn alone in three views)' if crop and crop.get('sheet') else 'CROP'} shows the concept feature to reproduce: "
              f"{z.get('concept_feature','')} for zone '{z['name']}'.{hint} Pick the {want} parts whose silhouette best matches that feature. Reply as JSON: {{\"picks\": [ints], \"reason\": \"<=20 words\"}}.")
    parts = [{"text": prompt}] + ([{"text": "SHEET-REF:" if crop.get("sheet") else "CROP:"}, img(crop["png"])] if crop else [{"text": "CROP unavailable; use the full concept:"}, img(a.concept)])
    for i, sp in enumerate(sheets): parts += [{"text": f"SHEET {i+1}:"}, img(sp)]
    v, resp = call(a.project, a.model, parts); picks = [pool[i - 1] for i in v.get("picks", []) if 1 <= i <= len(pool)]
    return picks, v.get("reason", ""), resp.get("usageMetadata", {}).get("promptTokenCount", 0)
choices = {}; used = set(); tok = 0
for z in Z["zones"]:
    if z.get("kind", "part") == "paint": choices[z["name"]] = {"ranked": [], "note": "paint zone, no part"}; continue
    if z.get("prim"): choices[z["name"]] = {"ranked": [], "note": f"procedural {z['prim']}"}; continue
    flat = any(t in FLAT for t in z["tags"]); need_sock = z["type"] == "connect"; excl = set(z.get("exclude", []))
    tags = set(z["tags"]); wide = tags | {n for t in tags for n in NEIGH.get(t, [])}
    words = {w.strip(".,;:()\"'").lower() for w in z.get("hint", "").split()} - STOP if z.get("hint") else set(); words = {w for w in words if len(w) > 3}
    ok = lambda x: x["name"] not in used and x["name"] not in excl and (not need_sock or "sockets" in x["attach"]) and (not x.get("zone") or x["zone"] == z["name"])
    base = [x for x in lib if ok(x) and tags & set(x["tags"])]
    if z.get("hint") or len(base) < 24: base += [x for x in lib if ok(x) and x not in base and (wide & set(x["tags"]) or any(w in x["note"].lower() for w in words))]
    if not base: base = [x for x in lib if ok(x)]   # exclusions exhausted the tag pool: open the whole library
    c = [x for x in base if x["conf"] >= 0.6 and (not flat or (x["planar"] >= 0.25 and x["below"] <= 0.15 * max(x["dims"])))] or base
    if not c: choices[z["name"]] = {"ranked": [], "note": "no candidates"}; print(z["name"], "no candidates"); continue
    size = f"about {z['size_m']} m across" if z.get("size_m") else "size per the concept"; hint = f" The art director noted last round: \"{z['hint']}\"." if z.get("hint") else ""
    crop = crops.get(z["name"]); sub = os.path.join(os.path.dirname(a.out), "sheets-sub", z["name"] + ".png")
    if os.path.exists(sub): crop = {"png": sub, "visible": True, "sheet": True}   # an isolated subassembly sheet beats a crop of the painting
    random.shuffle(c); c.sort(key=lambda x: -x["conf"]); pool = c[:72]; sl_reason = ""
    if len(pool) > a.candidates:
        try: cand, sl_reason, t = shortlist(z, pool, crop, hint, a.candidates); tok += t
        except Exception as e: print("shortlist failed", z["name"], str(e)[:100]); cand = pool[:a.candidates]
        if len(cand) < min(a.candidates, len(pool)): cand += [x for x in pool if x not in cand][:a.candidates - len(cand)]
    else: cand = pool
    prompt = (f"You choose kitbash parts to match concept art. Image 1 is the full concept of a 9 m spacecraft. Image 2 is a CROP of the concept around the feature to reproduce. "
              f"{'Image 2 is a SUBASSEMBLY SHEET: the feature drawn alone in three views, in the concept style. ' if crop and crop.get('sheet') else ''}Zone '{z['name']}' at hull region '{z['region']}' reproduces this feature: {z.get('concept_feature','')} (tags {z['tags']}), placed {size}.{hint} "
              f"The following {len(cand)} images are candidate parts, numbered 1..{len(cand)}, untextured, seen from a 3/4 angle; each will be scaled to {size} and its flat base sits on the hull. "
              f"Rank ALL candidates by how well their silhouette, proportions and detail density match the feature IN THE CROP at that size, in the craft's utilitarian, panelled, riveted style. "
              f"Penalise parts that would read as a different feature at that size (e.g. a whole ship module for a small block). Reply as JSON: {{\"ranking\": [ints], \"reason\": \"<=20 words\"}}.")
    parts = [{"text": prompt}, {"text": "IMAGE 1, CONCEPT:"}, img(a.concept)]
    parts += [{"text": ("IMAGE 2, SUBASSEMBLY SHEET:" if crop.get("sheet") else "IMAGE 2, CROP:") + ("" if crop["visible"] else " (feature not clearly visible; use the concept's style)")}, img(crop["png"])] if crop else [{"text": "IMAGE 2, CROP: not available; use the full concept."}]
    for i, x in enumerate(cand): parts += [{"text": f"CANDIDATE {i+1}:"}, img(x["png"])]
    try:
        v, resp = call(a.project, a.model, parts); tok += resp.get("usageMetadata", {}).get("promptTokenCount", 0)
        order = [cand[i-1]["name"] for i in v.get("ranking", []) if 1 <= i <= len(cand)] or [x["name"] for x in cand]
        choices[z["name"]] = {"ranked": order, "reason": v.get("reason", ""), "shortlist_reason": sl_reason, "pool": len(pool), "model": resp.get("modelVersion", a.model), "crop": bool(crop), "ref": "sheet" if crop and crop.get("sheet") else "crop" if crop else None}
    except Exception as e:
        body_err = getattr(e, "read", lambda: b"")(); order = [x["name"] for x in cand]
        choices[z["name"]] = {"ranked": order, "reason": f"fallback: {str(e)[:80]} {body_err[:160].decode(errors='ignore') if isinstance(body_err, bytes) else ''}"}
    for nm in order[:z.get("count", 1)]: used.add(nm)
    print(f"{z['name']:<22} -> {choices[z['name']]['ranked'][0][:40]} | {choices[z['name']].get('reason','')[:70]}")
json.dump(choices, open(a.out, "w"), indent=1); print("tokens", tok)
