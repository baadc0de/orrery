#!/usr/bin/env python3
"""Auto-label KIT OPS INSERTs. Pass 1: geometry heuristics from features.json. Pass 2: a vision model over the thumbnails
(Vertex AI Gemini over REST, gcloud auth) with a fixed vocabulary; stragglers (low confidence / disagreement) are listed
for manual labelling. Writes labels.json beside features.json. Provenance: model, version, prompt hash per label."""
import argparse, base64, hashlib, json, os, subprocess, sys, time, urllib.request
VOCAB = ["hull-panel", "plate", "strip", "rib", "frame", "block", "box", "cylinder", "pipe", "tank", "nozzle", "thruster", "vent", "grille", "hatch",
         "window", "antenna", "mast", "fin", "wing", "pylon", "bracket", "strut", "landing-gear", "cable", "conduit", "greeble-cluster", "turret", "gun",
         "launcher", "sensor", "light", "dish", "connector", "clamp", "tread", "misc"]
def heur(f):
    d = f["dims"]; h = f["height_ratio"]; a = f["aspect"]; p = f["planar_fraction"]; ns = f["normal_spread"]
    if h < 0.12 and p > 0.35: return "plate" if a < 2.5 else "strip"
    if h < 0.2 and a > 4: return "strip"
    if a > 6 and h > 0.5: return "pipe"
    if abs(ns[0] - ns[1]) < 0.08 and ns[2] < 0.25 and h > 0.6: return "cylinder"
    if p < 0.05 and f["tris"] > 3000: return "greeble-cluster"
    if 0.3 < h < 1.6 and a < 1.6 and p > 0.2: return "block"
    return "misc"
def token(): return subprocess.check_output(["gcloud", "auth", "print-access-token"], text=True).strip()
def vision(project, model, png, hint):
    url = f"https://aiplatform.googleapis.com/v1/projects/{project}/locations/global/publishers/google/models/{model}:generateContent"
    prompt = ("You label kitbash parts for a sci-fi hard-surface spaceship pipeline. The image is a single untextured mesh part on a transparent "
              "background, viewed from a 3/4 angle above. Choose the best 1-3 tags from this vocabulary only: " + ", ".join(VOCAB) +
              f". A geometry heuristic suggests: {hint}. Reply as JSON: {{\"tags\": [..], \"confidence\": 0..1, \"note\": \"<=12 words\"}}.")
    body = {"contents": [{"role": "user", "parts": [{"text": prompt}, {"inlineData": {"mimeType": "image/png", "data": base64.b64encode(open(png, "rb").read()).decode()}}]}],
            "generationConfig": {"temperature": 0.1, "responseMimeType": "application/json", "maxOutputTokens": 200}}
    req = urllib.request.Request(url, data=json.dumps(body).encode(), headers={"Authorization": f"Bearer {token()}", "Content-Type": "application/json"})
    with urllib.request.urlopen(req, timeout=120) as r: resp = json.loads(r.read())
    txt = "".join(p.get("text", "") for p in resp["candidates"][0]["content"]["parts"])
    return json.loads(txt), resp.get("modelVersion"), hashlib.sha256(prompt.encode()).hexdigest()[:16], resp.get("usageMetadata", {})
ap = argparse.ArgumentParser(); ap.add_argument("kpack_dir"); ap.add_argument("--project"); ap.add_argument("--model", default="gemini-3.1-flash-lite"); ap.add_argument("--no-vision", action="store_true"); ap.add_argument("--limit", type=int, default=0)
a = ap.parse_args()
feats = json.load(open(os.path.join(a.kpack_dir, "features.json"))); lp = os.path.join(a.kpack_dir, "labels.json")
labels = json.load(open(lp)) if os.path.exists(lp) else {}
names = [n for n in feats if n not in labels]; names = names[:a.limit] if a.limit else names
tok_in = tok_out = 0
for n in names:
    f = feats[n]; hl = heur(f); rec = {"heuristic": hl, "tags": [hl], "confidence": 0.3, "source": "heuristic"}
    if not a.no_vision and a.project:
        png = os.path.join(a.kpack_dir, n + ".png")
        try:
            v, ver, ph, use = vision(a.project, a.model, png, hl); rec.update({"tags": v.get("tags", [hl])[:3], "confidence": float(v.get("confidence", 0)), "note": v.get("note", ""),
                                                                 "source": "vision", "model": ver or a.model, "prompt_sha16": ph}); tok_in += use.get("promptTokenCount", 0); tok_out += use.get("candidatesTokenCount", 0)
        except Exception as e: rec["error"] = str(e)[:200]
    rec["straggler"] = rec["confidence"] < 0.6 or (rec["source"] == "vision" and hl not in ("misc", "greeble-cluster") and hl not in rec["tags"])
    labels[n] = rec
    json.dump(labels, open(lp, "w"), indent=1)
print(f"labelled {len(names)}; stragglers {sum(1 for r in labels.values() if r.get('straggler'))}/{len(labels)}; tokens in/out {tok_in}/{tok_out}")
