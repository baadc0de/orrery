#!/usr/bin/env python3
"""Critique loop step: compare renders of the assembly with the concept, per zone, and emit an adjusted zones.json.
Gemini Pro on Vertex (gcloud auth). Adjustments allowed: scale factor, count delta, region move, drop, add (from vocabulary)."""
import argparse, base64, copy, hashlib, json, os, subprocess, time, urllib.request
def token(): return subprocess.check_output(["gcloud", "auth", "print-access-token"], text=True).strip()
def img(p): return {"inlineData": {"mimeType": "image/png", "data": base64.b64encode(open(p, "rb").read()).decode()}}
SCHEMA = {"type": "object", "properties": {"overall": {"type": "string"}, "score": {"type": "number"}, "adjust": {"type": "array", "items": {"type": "object", "properties": {
  "zone": {"type": "string"}, "action": {"type": "string", "enum": ["keep", "scale", "count", "move", "drop", "add"]}, "scale_factor": {"type": "number"}, "count_delta": {"type": "integer"},
  "region": {"type": "string"}, "tags": {"type": "array", "items": {"type": "string"}}, "count": {"type": "integer"}, "why": {"type": "string"}}, "required": ["zone", "action", "why"]}}}, "required": ["overall", "score", "adjust"]}
ap = argparse.ArgumentParser(); ap.add_argument("--project", required=True); ap.add_argument("--model", default="gemini-3.1-pro-preview"); ap.add_argument("--concept", required=True)
ap.add_argument("--renders", nargs="+", required=True); ap.add_argument("--zones", required=True); ap.add_argument("--assembly", required=True); ap.add_argument("--atlas", required=True); ap.add_argument("--out", required=True); a = ap.parse_args()
Z = json.load(open(a.zones)); A = json.load(open(a.assembly)); atlas = json.load(open(a.atlas))
summary = [{"zone": z["name"], "type": z["type"], "region": z["region"], "tags": z["tags"], "count": z["count"], "scale": z["scale"], "feature": z.get("concept_feature", "")} for z in Z["zones"]]
placed = {p["zone"]: p for p in A["placements"]}
prompt = f"""You are the art director reviewing an automated kitbash against its concept art. The first image is the CONCEPT. The following images are RENDERS of the current assembly (hero 3/4, side, top; untextured, grey hull with placed parts in darker grey, cables orange, mech parts blue).
Current design program (zones), one per feature: {json.dumps(summary)}
Placements that actually happened (zone -> insert, scale): {json.dumps({k: {"insert": v["insert"], "scale": v.get("scale")} for k, v in placed.items()})}
Hull regions available: {list(atlas["regions"].keys())}
Judge each zone: is the feature present, at the right place, right size, right density compared to the concept? Then give adjustments: "scale" with scale_factor (0.5..2.0), "count" with count_delta, "move" with a new region, "drop", or "add" a missing feature (region, tags from the existing tag vocabulary, count). Be decisive and sparse: at most 8 adjustments, only where the render clearly disagrees with the concept. Give an overall 0..1 fidelity score and a two-sentence overall note."""
parts = [{"text": prompt}, {"text": "CONCEPT:"}, img(a.concept)]
for r in a.renders: parts += [{"text": f"RENDER {os.path.basename(r)}:"}, img(r)]
body = {"contents": [{"role": "user", "parts": parts}], "generationConfig": {"temperature": 0.2, "responseMimeType": "application/json", "responseSchema": SCHEMA, "maxOutputTokens": 6000}}
url = f"https://aiplatform.googleapis.com/v1/projects/{a.project}/locations/global/publishers/google/models/{a.model}:generateContent"
req = urllib.request.Request(url, data=json.dumps(body).encode(), headers={"Authorization": f"Bearer {token()}", "Content-Type": "application/json"})
with urllib.request.urlopen(req, timeout=300) as r: resp = json.loads(r.read())
txt = "".join(p.get("text", "") for p in resp["candidates"][0]["content"]["parts"]); C = json.loads(txt[txt.find("{"): txt.rfind("}") + 1])
# apply
Z2 = copy.deepcopy(Z); byname = {z["name"]: z for z in Z2["zones"]}
for adj in C["adjust"]:
    z = byname.get(adj["zone"]); act = adj["action"]
    if act == "scale" and z: f = max(0.5, min(2.0, adj.get("scale_factor", 1.0))); z["scale"] = [round(min(1.5, v * f), 3) for v in z["scale"]]
    elif act == "count" and z: z["count"] = max(0, z["count"] + adj.get("count_delta", 0))
    elif act == "move" and z and adj.get("region") in atlas["regions"]: z["region"] = adj["region"]
    elif act == "drop" and z: Z2["zones"] = [q for q in Z2["zones"] if q["name"] != z["name"]]
    elif act == "add" and adj.get("region") in atlas["regions"] and adj.get("tags"):
        Z2["zones"].append({"name": adj["zone"], "type": "surface", "region": adj["region"], "tags": adj["tags"], "count": adj.get("count", 1), "along": "y", "scale": [0.5, 0.7], "concept_feature": adj.get("why", "")})
Z2.setdefault("critiques", []).append({"score": C["score"], "overall": C["overall"], "adjust": C["adjust"], "model": resp.get("modelVersion", a.model), "renders": [os.path.basename(r) for r in a.renders], "at": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())})
json.dump(Z2, open(a.out, "w"), indent=1)
print(f"score {C['score']} | {C['overall']}"); [print(f"  {x['action']:<6} {x['zone']:<22} {x.get('scale_factor', x.get('count_delta', x.get('region', '')))} | {x['why'][:80]}") for x in C["adjust"]]
