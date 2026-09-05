#!/usr/bin/env python3
"""Constructible concept: the concept artist gets the palette sheet as INPUT and must design a simple prop from those parts only.
Then a vision model reads palette + concept and writes the BUILD LIST (which numbered part, where, how big) in metres.
usage: concept.py --palette DIR --prop "deck safety railing section" --out DIR [--size "2 m long, 1.1 m high"]"""
import argparse, base64, json, os, subprocess, sys, urllib.request
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "..", "spikes", "ortho-callouts"))
from ortho_callouts import generate, img_part, sha256
ap = argparse.ArgumentParser(); ap.add_argument("--project", default=os.environ.get("VERTEX_PROJECT")); ap.add_argument("--palette", required=True); ap.add_argument("--prop", required=True); ap.add_argument("--size", default="about 2 m long and 1.1 m high")
ap.add_argument("--out", required=True); ap.add_argument("--image-model", default="gemini-3-pro-image"); ap.add_argument("--model", default="gemini-3.5-flash"); ap.add_argument("--seed", type=int, default=0); a = ap.parse_args()
P = json.load(open(os.path.join(a.palette, "palette.json"))); os.makedirs(a.out, exist_ok=True); sheet = open(P["sheet"], "rb").read()
parts_txt = "; ".join(f"{p['id']} = {p['desc']}" for p in P["parts"])
concept_png = os.path.join(a.out, "concept.png"); ap2 = None
if not os.path.exists(concept_png):
    prompt = (f"You are a concept artist for a utilitarian, panelled, riveted spacecraft. The attached image is a PALETTE of numbered kitbash parts: {parts_txt}. "
              f"Design a {a.prop}, {a.size}, built ONLY from these parts (any part may be repeated, scaled and rotated; nothing else may be invented). "
              f"Draw it as a clean front-left 3/4 concept on a flat light-grey background, untextured grey with the concept's dark polymer and bare-metal accents, "
              f"and add small numbered callouts (leader line + the palette number) on every element so a modeller can see which palette part it is. No other text.")
    generate(a.project, "global", a.image_model, [{"text": prompt}, img_part(sheet)], concept_png, {"stage": "constructible-concept", "prop": a.prop, "size": a.size, "prompt": prompt, "palette_sha256": sha256(sheet)}, temperature=0.6)
    print("concept", concept_png)
# build list
def token(): return subprocess.check_output(["gcloud", "auth", "print-access-token"], text=True).strip()
def img(p): return {"inlineData": {"mimeType": "image/png", "data": base64.b64encode(open(p, "rb").read()).decode()}}
SCHEMA = {"type": "object", "properties": {"frame_note": {"type": "string"}, "items": {"type": "array", "items": {"type": "object", "properties": {
  "name": {"type": "string"}, "part": {"type": "integer"}, "pos_m": {"type": "array", "items": {"type": "number"}}, "along": {"type": "string", "enum": ["x", "y", "z"]},
  "spin_deg": {"type": "number"}, "tilt_deg": {"type": "number"}, "size_m": {"type": "number"}, "why": {"type": "string"}}, "required": ["name", "part", "pos_m", "along", "size_m"]}}}, "required": ["items"]}
prompt = (f"Image 1 is a numbered palette of kitbash parts: {parts_txt}. Image 2 is a concept of a {a.prop} ({a.size}) built only from those parts, with numbered callouts. "
          f"Write the BUILD LIST that reproduces the concept: one item per placed part instance (a repeated part is several items). Frame: x runs along the prop's length (left to right in the concept), "
          f"y is depth (away from the viewer), z is up, the ground is z=0, the prop is centred on x=0. pos_m is the part's CENTRE in metres. Orientation is given by 'along': the world axis the part's LONGEST dimension points along (x = horizontal along the prop, "
          f"z = vertical post, y = into the depth), and 'spin_deg': rotation about that long axis in degrees (0 = the part's flattest face points up for x/y, or toward -y for z). 'tilt_deg' (optional) tilts an x-along part in the xz plane for diagonals (positive lifts its +x end). size_m is the length of its longest dimension after scaling. Be geometric and complete: every rail, post, bracket and plate, "
          f"with positions that actually touch each other.")
body = {"contents": [{"role": "user", "parts": [{"text": prompt}, {"text": "IMAGE 1:"}, img(P["sheet"]), {"text": "IMAGE 2:"}, img(concept_png)]}], "generationConfig": {"temperature": 0.1, "responseMimeType": "application/json", "responseSchema": SCHEMA, "maxOutputTokens": 8000}}
url = f"https://aiplatform.googleapis.com/v1/projects/{a.project}/locations/global/publishers/google/models/{a.model}:generateContent"
req = urllib.request.Request(url, data=json.dumps(body).encode(), headers={"Authorization": f"Bearer {token()}", "Content-Type": "application/json"})
with urllib.request.urlopen(req, timeout=300) as r: resp = json.loads(r.read())
txt = "".join(p.get("text", "") for p in resp["candidates"][0]["content"]["parts"]); B = json.loads(txt[txt.find("{"): txt.rfind("}") + 1])
B["prop"] = a.prop; B["size"] = a.size; B["palette"] = os.path.abspath(a.palette); B["concept"] = concept_png; B["model"] = resp.get("modelVersion", a.model)
json.dump(B, open(os.path.join(a.out, "build.json"), "w"), indent=1)
print("build list:", len(B["items"]), "items"); [print(f"  {it['name']:<24} part {it['part']:>2} at {it['pos_m']} along {it['along']} spin {it.get('spin_deg', 0)} size {it['size_m']}") for it in B["items"]]
