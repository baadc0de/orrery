#!/usr/bin/env python3
"""Concept + callout sheet -> zones.json (the design program) via a vision model on Vertex AI (gcloud auth, global endpoint).
Regions are semantic (side.long.lat from the hull atlas vocabulary); the assembler resolves them on the hull mesh."""
import argparse, base64, hashlib, json, os, subprocess, time, urllib.request
VOCAB = ["hull-panel","plate","strip","rib","frame","block","box","cylinder","pipe","tank","nozzle","thruster","vent","grille","hatch","window","antenna","mast","fin","wing","pylon","bracket","strut","landing-gear","cable","conduit","greeble-cluster","turret","gun","launcher","sensor","light","dish","connector","clamp","tread","misc"]
SCHEMA = {"type": "object", "properties": {"asset": {"type": "string"}, "zones": {"type": "array", "items": {"type": "object", "properties": {
  "name": {"type": "string"}, "type": {"type": "string", "enum": ["surface", "connect"]}, "region": {"type": "string"}, "region_to": {"type": "string"},
  "tags": {"type": "array", "items": {"type": "string"}}, "count": {"type": "integer"}, "along": {"type": "string", "enum": ["x", "y"]},
  "scale": {"type": "array", "items": {"type": "number"}}, "concept_feature": {"type": "string"}, "why": {"type": "string"}},
  "required": ["name", "type", "region", "tags", "count", "scale", "concept_feature"]}}}, "required": ["asset", "zones"]}
def token(): return subprocess.check_output(["gcloud", "auth", "print-access-token"], text=True).strip()
def img(p): return {"inlineData": {"mimeType": "image/png", "data": base64.b64encode(open(p, "rb").read()).decode()}}
ap = argparse.ArgumentParser(); ap.add_argument("--project", required=True); ap.add_argument("--model", default="gemini-3.1-pro-preview"); ap.add_argument("--concept", required=True); ap.add_argument("--callout", required=True)
ap.add_argument("--brief", required=True); ap.add_argument("--atlas", required=True); ap.add_argument("--out", required=True); a = ap.parse_args()
atlas = json.load(open(a.atlas)); regions = list(atlas["regions"].keys())
prompt = f"""You are the design-program author for a kitbash pipeline. Read the design brief, the concept art and the production callout sheet of one spacecraft, and write the assembly program as JSON.
The hull already exists as a coarse mesh. Kit parts are placed onto named REGIONS of that hull. Region names are side.long.lat with side in {atlas['vocabulary']['side']}, long in {atlas['vocabulary']['long']} (fore = nose end), lat in {atlas['vocabulary']['lat']} (inner = near the centreline). Regions that exist on this hull, largest first, with area in m2: {json.dumps(atlas['regions'])}. The hull is {atlas['dims_m']} m (x width, y length, z height); it is mirrored across x, so describe only the +x half and centreline; anything you place on a flank appears on both sides.
Part tags must come from this vocabulary only: {', '.join(VOCAB)}.
Zone types: "surface" places `count` parts on the region, laid along axis `along`, each scaled so its footprint is `scale` (min,max fraction) of the slot; "connect" stretches one elongated part (cable/conduit/pipe/strut) from `region` centroid to `region_to` centroid.
Write one zone per distinct feature you can see in the concept or read in the callout sheet: conduit runs, panel lines, vents, RCS clusters, hardpoints, pylons, skids, sensor masts, canopy frame, emissive housings, decals areas (as plate). Name the concept feature each zone reproduces and say why in one clause. 8 to 16 zones. Prefer fewer, larger parts over clutter; leave the canopy region itself empty.

BRIEF:
{open(a.brief).read()}"""
body = {"contents": [{"role": "user", "parts": [{"text": prompt}, {"text": "CONCEPT ART:"}, img(a.concept), {"text": "CALLOUT SHEET:"}, img(a.callout)]}],
        "generationConfig": {"temperature": 0.3, "responseMimeType": "application/json", "responseSchema": SCHEMA, "maxOutputTokens": 8000}}
url = f"https://aiplatform.googleapis.com/v1/projects/{a.project}/locations/global/publishers/google/models/{a.model}:generateContent"
req = urllib.request.Request(url, data=json.dumps(body).encode(), headers={"Authorization": f"Bearer {token()}", "Content-Type": "application/json"})
with urllib.request.urlopen(req, timeout=300) as r: resp = json.loads(r.read())
txt = "".join(p.get("text", "") for p in resp["candidates"][0]["content"]["parts"]); z = json.loads(txt)
bad = [q["region"] for q in z["zones"] if q["region"] not in regions] + [q.get("region_to") for q in z["zones"] if q["type"] == "connect" and q.get("region_to") not in regions]
z["mirror"] = "x"; z["hull"] = os.path.relpath(os.path.join(os.path.dirname(a.atlas), "hull.glb"), os.path.dirname(a.out)) if False else "hull.glb"
z["provenance"] = {"model": resp.get("modelVersion", a.model), "prompt_sha16": hashlib.sha256(prompt.encode()).hexdigest()[:16], "concept_sha16": hashlib.sha256(open(a.concept, "rb").read()).hexdigest()[:16],
                   "callout_sha16": hashlib.sha256(open(a.callout, "rb").read()).hexdigest()[:16], "usage": resp.get("usageMetadata"), "generated_at": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()), "unknown_regions": bad}
json.dump(z, open(a.out, "w"), indent=1)
print("zones", len(z["zones"]), "unknown regions", bad); [print(f"  {q['name']:<22} {q['type']:<8} {q['region']:<18} {q['tags'][:3]} x{q['count']} <- {q['concept_feature']}") for q in z["zones"]]
