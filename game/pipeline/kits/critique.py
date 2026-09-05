#!/usr/bin/env python3
"""Critique loop step: compare renders of the assembly with the concept, per zone, and emit an adjusted zones.json.
Gemini Pro on Vertex (gcloud auth). The critic sees the shaded renders AND flat zone-id renders (every zone one named colour,
legend from assembly.json), so its verdicts name the right zone. Adjustments: scale, count, move, rotate, replace (re-choose a
different part, the rejected insert is excluded and the reason becomes a hint for the chooser), drop, add."""
import argparse, base64, copy, json, os, subprocess, time, urllib.request
def token(): return subprocess.check_output(["gcloud", "auth", "print-access-token"], text=True).strip()
def img(p): return {"inlineData": {"mimeType": "image/png", "data": base64.b64encode(open(p, "rb").read()).decode()}}
SCHEMA = {"type": "object", "properties": {"overall": {"type": "string"}, "score": {"type": "number"}, "per_zone": {"type": "array", "items": {"type": "object", "properties": {
  "zone": {"type": "string"}, "present": {"type": "boolean"}, "verdict": {"type": "string", "enum": ["good", "wrong_part", "wrong_size", "wrong_place", "wrong_orientation", "missing"]}}, "required": ["zone", "present", "verdict"]}},
  "adjust": {"type": "array", "items": {"type": "object", "properties": {
  "zone": {"type": "string"}, "action": {"type": "string", "enum": ["keep", "scale", "count", "move", "rotate", "replace", "drop", "add"]}, "scale_factor": {"type": "number"}, "count_delta": {"type": "integer"},
  "yaw_deg": {"type": "integer"}, "region": {"type": "string"}, "anchor": {"type": "array", "items": {"type": "number"}}, "tags": {"type": "array", "items": {"type": "string"}}, "count": {"type": "integer"}, "size_m": {"type": "number"}, "prim": {"type": "string", "enum": ["skid", "nozzle", "box", "pipe_run"]}, "carve": {"type": "boolean"}, "why": {"type": "string"}}, "required": ["zone", "action", "why"]}}}, "required": ["overall", "score", "per_zone", "adjust"]}
ap = argparse.ArgumentParser(); ap.add_argument("--project", required=True); ap.add_argument("--model", default="gemini-3.1-pro-preview"); ap.add_argument("--concept", required=True)
ap.add_argument("--renders", nargs="+", required=True); ap.add_argument("--id-renders", nargs="*", default=[]); ap.add_argument("--zones", required=True); ap.add_argument("--assembly", required=True); ap.add_argument("--atlas", required=True); ap.add_argument("--out", required=True); a = ap.parse_args()
Z = json.load(open(a.zones)); A = json.load(open(a.assembly)); atlas = json.load(open(a.atlas)); legend = A.get("id_colors", {})
summary = [{"zone": z["name"], "type": z["type"], "kind": z.get("kind", "part"), "region": z["region"], "tags": z["tags"], "count": z["count"], "size_m": z.get("size_m"), "anchor": z.get("anchor"), "prim": z.get("prim"), "feature": z.get("concept_feature", ""), "id_colour": legend.get(z["name"])} for z in Z["zones"]]
placed = {}
for p in A["placements"]: placed.setdefault(p["zone"], {"insert": p.get("insert"), "placed_m": p.get("placed_m"), "pos_m": p.get("pos")})
vocab = sorted({t for z in Z["zones"] for t in z["tags"]} | {"thruster", "nozzle", "cable", "pipe", "conduit", "hull-panel", "plate", "hatch", "vent", "grille", "landing-gear", "strut", "pylon", "gun", "box", "block", "cylinder", "greeble-cluster", "antenna", "bracket", "window", "fin"})
prompt = f"""You are the art director reviewing an automated kitbash against its concept art. Image CONCEPT is the target. Images RENDER-* are shaded renders of the current assembly (hero 3/4 from front-left, side, top, belly). Images ID-* are the SAME views with every zone's part painted one flat colour on a dark-grey hull; use the legend to identify which blob belongs to which zone before judging it.
Materials in the shaded renders follow the brief palette: painted warm-grey hull, dark panels, safety-orange accents, blue emissive thrusters, aluminium fasteners; the hull number is a text decal on the flanks.
Design program (zones), one per feature, with the id colour of each: {json.dumps(summary)}
Placements that happened (zone -> insert, placed size in metres): {json.dumps(placed)}
POSITIONS: the hull is 9 m long (y, +y = nose) and about 9.5 m wide (x, +x = starboard). Placements list pos_m in those metres. In RENDER-top the nose is at the top of the image and starboard on the right; in RENDER-belly the nose is at the top and starboard on the LEFT. A zone may carry an "anchor": [x, y] as fractions of the hull half-extents (-1..1), e.g. the starboard main engine at [0.15, -0.9], a nose block at [0, 0.95], a wingtip pod at [0.95, -0.4]; flank anchors are [y, z]. Anchors override the coarse region names, which only distinguish the side of the hull reliably. "count" is the total across both sides; even counts become mirrored pairs, so a twin engine is count 2 with the starboard anchor.
The coarse hull already contains lumps for the biggest features (engine pods, skids, nose block); a part duplicating a lump that already reads correctly should be dropped, a part that should REPLACE the look of a lump should be scaled to cover it.
Zones with kind "paint" are intentionally absent from the geometry (they will be painted later); do not ask for them as parts. The craft is 9 m long: judge sizes in metres.
Hull regions available: {list(atlas["regions"].keys())}. Tag vocabulary for "add": {vocab}.
For every part zone give a per_zone verdict: good, wrong_part (the shape reads as a different thing than the concept feature), wrong_size, wrong_place, wrong_orientation, or missing.
Then give adjustments, at most 8, only where the render clearly disagrees with the concept:
- "replace" when the part's shape is wrong: say in why what silhouette the chooser should look for instead (this text is handed to the part chooser).
- "scale" with scale_factor 0.5..2.0 (or an absolute size_m) when the shape is right but the size is off.
- "rotate" with yaw_deg (90, 180, 270) when the part is right but spun on the hull.
- "move" with an "anchor" [x, y] (preferred; use the top/belly renders as the map) and/or a new region; "count" with count_delta; "drop" only when the feature should not exist at all; "add" for a missing feature (region, tags, count, size_m, and ALWAYS an anchor).
- "carve": true on a replace/scale/move when the hull's own lump under this zone should be cut away so the part replaces it instead of sitting on top of it.
PRIMITIVES: the library has no landing skids and no long straight piping. When the same silhouette has been requested before and the library keeps failing, request a procedural stand-in instead: set "prim" on a replace or add: "skid" (ski bar with struts, size_m = length), "nozzle" (engine cylinder with recessed cone, size_m = diameter), "box" (bevelled box, size_m = length), "pipe_run" (count parallel pipes following the skin, size_m = run length, on connect zones or an anchored zone). A zone with a prim keeps it until you replace it without one.
Prefer replace over drop. Give an overall 0..1 fidelity score (1 = a modeller would accept it as a blockout of the concept) and a two-sentence overall note."""
parts = [{"text": prompt}, {"text": "CONCEPT:"}, img(a.concept)]
for r in a.renders: parts += [{"text": f"RENDER-{os.path.basename(r)}:"}, img(r)]
for r in a.id_renders: parts += [{"text": f"ID-{os.path.basename(r)} (legend: {json.dumps(legend)}):"}, img(r)]
body = {"contents": [{"role": "user", "parts": parts}], "generationConfig": {"temperature": 0.2, "responseMimeType": "application/json", "responseSchema": SCHEMA, "maxOutputTokens": 24000}}
url = f"https://aiplatform.googleapis.com/v1/projects/{a.project}/locations/global/publishers/google/models/{a.model}:generateContent"
req = urllib.request.Request(url, data=json.dumps(body).encode(), headers={"Authorization": f"Bearer {token()}", "Content-Type": "application/json"})
with urllib.request.urlopen(req, timeout=300) as r: resp = json.loads(r.read())
txt = "".join(p.get("text", "") for p in resp["candidates"][0]["content"]["parts"]); C = json.loads(txt[txt.find("{"): txt.rfind("}") + 1])
# apply
Z2 = copy.deepcopy(Z); byname = {z["name"]: z for z in Z2["zones"]}
drops = 0
for adj in C["adjust"]:
    z = byname.get(adj["zone"]); act = adj["action"]
    if z and adj.get("carve") is not None: z["carve"] = bool(adj["carve"])
    if act == "scale" and z:
        if adj.get("size_m") and z.get("size_m"): f = (adj["size_m"] / z["size_m"]) ** 0.5
        else: f = max(0.5, min(2.0, adj.get("scale_factor", 1.0))) ** 0.5  # damped: half the requested change in log space, so passes converge instead of oscillating
        if z.get("size_m"): z["size_m"] = round(max(0.1, min(4.0, z["size_m"] * f)), 3)
        else: z["scale"] = [round(max(0.15, min(1.5, v * f)), 3) for v in z["scale"]]
        if f > 1.0 and z.get("scale", [0, 0])[1] >= 1.2: z["max_size_m"] = round(min(3.5, z.get("max_size_m", 2.5) * f), 2)
        if f < 1.0 and z.get("max_size_m", 2.5) > 2.5: z["max_size_m"] = round(max(2.5, z["max_size_m"] * f), 2)
    elif act == "count" and z: z["count"] = max(0, z["count"] + adj.get("count_delta", 0))
    elif act == "move" and z:
        if adj.get("region") in atlas["regions"]: z["region"] = adj["region"]
        if adj.get("anchor") and len(adj["anchor"]) >= 2: z["anchor"] = [round(max(-1.0, min(1.0, float(v))), 3) for v in adj["anchor"][:2]]
    elif act == "rotate" and z: z["yaw_deg"] = (z.get("yaw_deg", 0) + int(adj.get("yaw_deg", 90))) % 360
    elif act == "replace" and z:
        cur = placed.get(z["name"], {}).get("insert")
        if cur: z.setdefault("exclude", []); z["exclude"] = sorted(set(z["exclude"]) | {cur})
        z["hint"] = adj["why"]; z["rechoose"] = True
        if adj.get("prim"): z["prim"] = adj["prim"]
        else: z.pop("prim", None)
        if adj.get("size_m"): z["size_m"] = round(float(adj["size_m"]), 3)
    elif act == "drop" and z and drops < 2: Z2["zones"] = [q for q in Z2["zones"] if q["name"] != z["name"]]; drops += 1
    elif act == "add" and adj.get("region") in atlas["regions"] and adj.get("tags") and adj["zone"] not in byname:
        Z2["zones"].append({"name": adj["zone"], "type": "surface", "kind": "part", "region": adj["region"], "tags": adj["tags"], "count": max(1, adj.get("count") or 1), "along": "y", "scale": [0.5, 0.7], "size_m": adj.get("size_m", 1.0), "anchor": adj.get("anchor"), "prim": adj.get("prim"), "concept_feature": adj.get("why", ""), "hint": adj.get("why", "")})
Z2.setdefault("critiques", []).append({"score": C["score"], "overall": C["overall"], "per_zone": C.get("per_zone", []), "adjust": C["adjust"], "model": resp.get("modelVersion", a.model), "renders": [os.path.basename(r) for r in a.renders + a.id_renders], "at": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())})
json.dump(Z2, open(a.out, "w"), indent=1)
print(f"score {C['score']} | {C['overall']}")
print("  verdicts:", ", ".join(f"{p['zone']}={p['verdict']}" for p in C.get("per_zone", [])))
for x in C["adjust"]: print(f"  {x['action']:<7} {x['zone']:<22} {x.get('scale_factor', x.get('size_m', x.get('count_delta', x.get('yaw_deg', x.get('anchor', x.get('region', ''))))))} | {x['why'][:90]}")
