#!/usr/bin/env python3
"""Prop critique: compare renders of the assembled build list with the constructible concept, item by item, and emit an
adjusted build.json. Actions: move (delta_m), spin (spin_deg), tilt (tilt_deg), scale (size_m), swap (part), remove, add."""
import argparse, base64, copy, json, os, subprocess, time, urllib.request
def token(): return subprocess.check_output(["gcloud", "auth", "print-access-token"], text=True).strip()
def img(p): return {"inlineData": {"mimeType": "image/png", "data": base64.b64encode(open(p, "rb").read()).decode()}}
SCHEMA = {"type": "object", "properties": {"overall": {"type": "string"}, "score": {"type": "number"}, "per_item": {"type": "array", "items": {"type": "object", "properties": {
  "i": {"type": "integer"}, "verdict": {"type": "string", "enum": ["good", "wrong_part", "wrong_size", "wrong_place", "wrong_orientation", "extra"]}}, "required": ["i", "verdict"]}},
  "adjust": {"type": "array", "items": {"type": "object", "properties": {"i": {"type": "integer"}, "action": {"type": "string", "enum": ["move", "spin", "tilt", "scale", "swap", "remove", "add"]},
  "delta_m": {"type": "array", "items": {"type": "number"}}, "spin_deg": {"type": "number"}, "tilt_deg": {"type": "number"}, "size_m": {"type": "number"}, "part": {"type": "integer"},
  "name": {"type": "string"}, "pos_m": {"type": "array", "items": {"type": "number"}}, "along": {"type": "string", "enum": ["x", "y", "z"]}, "why": {"type": "string"}}, "required": ["action", "why"]}}}, "required": ["overall", "score", "per_item", "adjust"]}
ap = argparse.ArgumentParser(); ap.add_argument("--project", default=os.environ.get("VERTEX_PROJECT")); ap.add_argument("--model", default="gemini-3.1-pro-preview"); ap.add_argument("--build", required=True)
ap.add_argument("--assembly", required=True); ap.add_argument("--renders", nargs="+", required=True); ap.add_argument("--out", required=True); a = ap.parse_args()
B = json.load(open(a.build)); A = json.load(open(a.assembly)); P = json.load(open(os.path.join(B["palette"], "palette.json"))); parts_txt = "; ".join(f"{p['id']} = {p['desc']}" for p in P["parts"])
items = [{"i": p["i"], "name": p["name"], "part": p["part"], "pos_m": p["pos_m"], "along": p["along"], "spin_deg": p.get("spin_deg", 0), "tilt_deg": p.get("tilt_deg", 0), "size_m": p["size_m"], "bbox": [p["bbox_min"], p["bbox_max"]]} for p in A["placed"]]
prompt = f"""You are the art director checking a kitbash reconstruction of a CONCEPT that was itself built from a numbered palette: {parts_txt}.
Image CONCEPT is the target (with palette numbers as callouts). Images RENDER-* show the reconstruction (hero 3/4, front, side). Frame: x along the prop (left-right in the front view), y depth, z up, ground z=0.
Build list as placed (i = item index; pos_m = centre; along = world axis of the part's longest dimension; spin_deg = rotation about that axis, 0 = flattest face up; tilt_deg = tilt in the xz plane; bbox = world min/max): {json.dumps(items)}
Judge every item against the concept (per_item verdict), then give at most 8 adjustments: move (delta_m in metres), spin (absolute spin_deg), tilt (absolute tilt_deg), scale (absolute size_m), swap (new palette part), remove (an item not in the concept), add (name, part, pos_m, along, size_m for something the concept has and the build lacks).
Be geometric: use the bboxes to see what touches what. Give a 0..1 score for how well the reconstruction matches the concept and a two-sentence note."""
parts = [{"text": prompt}, {"text": "CONCEPT:"}, img(B["concept"])]
for r in a.renders: parts += [{"text": f"RENDER-{os.path.basename(r)}:"}, img(r)]
body = {"contents": [{"role": "user", "parts": parts}], "generationConfig": {"temperature": 0.2, "responseMimeType": "application/json", "responseSchema": SCHEMA, "maxOutputTokens": 24000}}
url = f"https://aiplatform.googleapis.com/v1/projects/{a.project}/locations/global/publishers/google/models/{a.model}:generateContent"
import re
def ask():
    req = urllib.request.Request(url, data=json.dumps(body).encode(), headers={"Authorization": f"Bearer {token()}", "Content-Type": "application/json"})
    with urllib.request.urlopen(req, timeout=300) as r: resp = json.loads(r.read())
    fr = resp["candidates"][0].get("finishReason"); txt = "".join(p.get("text", "") for p in resp["candidates"][0]["content"]["parts"]); txt = txt[txt.find("{"): txt.rfind("}") + 1]
    if fr != "STOP": print("critic finishReason", fr, resp.get("usageMetadata"))
    try: return json.loads(txt), resp
    except json.JSONDecodeError:
        open(a.out + ".raw.txt", "w").write(txt)
        try: return json.loads(re.sub(r",\s*([}\]])", r"\1", txt)), resp   # trailing commas are the usual defect
        except json.JSONDecodeError: return None, resp
C, resp = ask()
if C is None: C, resp = ask()
if C is None: raise SystemExit("critic returned unparseable JSON twice; raw in " + a.out + ".raw.txt")
B2 = copy.deepcopy(B); its = B2["items"]; removed = set()
for adj in C["adjust"]:
    act = adj["action"]; i = adj.get("i")
    if act == "add" and adj.get("part") and adj.get("pos_m") and adj.get("along"):
        its.append({"name": adj.get("name", "added"), "part": adj["part"], "pos_m": adj["pos_m"][:3], "along": adj["along"], "spin_deg": adj.get("spin_deg", 0), "tilt_deg": adj.get("tilt_deg", 0), "size_m": adj.get("size_m", 0.5)}); continue
    if i is None or not (0 <= i < len(its)): continue
    it = its[i]
    if act == "move" and adj.get("delta_m"): it["pos_m"] = [round(p + d, 3) for p, d in zip(it["pos_m"], adj["delta_m"][:3] + [0, 0, 0])]
    elif act == "spin" and adj.get("spin_deg") is not None: it["spin_deg"] = adj["spin_deg"]
    elif act == "tilt" and adj.get("tilt_deg") is not None: it["tilt_deg"] = adj["tilt_deg"]
    elif act == "scale" and adj.get("size_m"): it["size_m"] = round(max(0.03, adj["size_m"]), 3)
    elif act == "swap" and adj.get("part"): it["part"] = adj["part"]
    elif act == "remove": removed.add(i)
B2["items"] = [it for k, it in enumerate(its) if k not in removed]
B2.setdefault("critiques", []).append({"score": C["score"], "overall": C["overall"], "per_item": C["per_item"], "adjust": C["adjust"], "model": resp.get("modelVersion", a.model), "at": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())})
json.dump(B2, open(a.out, "w"), indent=1)
good = sum(1 for p in C["per_item"] if p["verdict"] == "good"); print(f"score {C['score']} | good {good}/{len(C['per_item'])} | {C['overall']}")
for x in C["adjust"]: print(f"  {x['action']:<6} {x.get('i', '+'):>3} {x.get('delta_m', x.get('spin_deg', x.get('tilt_deg', x.get('size_m', x.get('part', '')))))} | {x['why'][:90]}")
