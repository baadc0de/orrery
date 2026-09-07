#!/usr/bin/env python3
"""Spike: design brief -> concept (3/4 view) -> orthographic views -> callout sheet.

Vertex AI Gemini image models over REST (auth from `gcloud auth print-access-token`).
No third-party Python deps. Every artifact gets a provenance JSON beside it
(G12.1): prompt, references (sha256), model, location, response id, output sha256.
"""
import argparse, base64, hashlib, json, os, subprocess, sys, time, urllib.request

def token():
    return subprocess.check_output(["gcloud", "auth", "print-access-token"], text=True).strip()

def sha256(b): return hashlib.sha256(b).hexdigest()

def generate(project, location, model, parts, out_png, prov, temperature=0.6, retries=3):
    host = "aiplatform.googleapis.com" if location == "global" else f"{location}-aiplatform.googleapis.com"
    url = f"https://{host}/v1/projects/{project}/locations/{location}/publishers/google/models/{model}:generateContent"
    body = {"contents": [{"role": "user", "parts": parts}],
            "generationConfig": {"responseModalities": ["IMAGE", "TEXT"], "temperature": temperature,
                                 "maxOutputTokens": 32768, "imageConfig": {"aspectRatio": "1:1"}}}
    req = urllib.request.Request(url, data=json.dumps(body).encode(),
                                 headers={"Authorization": f"Bearer {token()}", "Content-Type": "application/json"})
    for attempt in range(retries):
        try:
            with urllib.request.urlopen(req, timeout=300) as r:
                resp = json.loads(r.read())
            break
        except urllib.error.HTTPError as e:
            msg = e.read().decode()[:600]
            if e.code in (429, 503) and attempt + 1 < retries:
                time.sleep(5 * (attempt + 1)); continue
            raise SystemExit(f"{model}@{location} HTTP {e.code}: {msg}")
    cand = resp.get("candidates", [{}])[0]
    if "content" not in cand:
        dump = out_png[:-4] + ".response.json"
        with open(dump, "w") as f: json.dump(resp, f, indent=2)
        raise SystemExit(f"no content: finishReason={cand.get('finishReason')} safety={cand.get('safetyRatings')} "
                         f"promptFeedback={resp.get('promptFeedback')} (dumped to {dump})")
    img = None; text = []
    for part in cand["content"]["parts"]:
        if "inlineData" in part: img = base64.b64decode(part["inlineData"]["data"])
        elif "text" in part: text.append(part["text"])
    if img is None:
        raise SystemExit(f"no image in response: {json.dumps(resp)[:800]}")
    with open(out_png, "wb") as f: f.write(img)
    prov.update({"model": model, "location": location,
                 "response_id": resp.get("responseId"), "model_version": resp.get("modelVersion"),
                 "usage": resp.get("usageMetadata"), "output": os.path.basename(out_png),
                 "output_sha256": sha256(img), "model_text": "".join(text)[:2000],
                 "generated_at": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())})
    with open(out_png[:-4] + ".provenance.json", "w") as f: json.dump(prov, f, indent=2)
    return img

def img_part(b): return {"inlineData": {"mimeType": "image/png", "data": base64.b64encode(b).decode()}}

def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--project", required=True); ap.add_argument("--location", default="global")
    ap.add_argument("--model", default="gemini-2.5-flash-image")
    ap.add_argument("--brief", required=True, help="path to the design brief (markdown)")
    ap.add_argument("--style", nargs="*", default=[], help="style-bible reference images")
    ap.add_argument("--out", default="out"); ap.add_argument("--name", default="asset")
    ap.add_argument("--reuse-concept", action="store_true"); ap.add_argument("--views", default="front,side,back,top")
    a = ap.parse_args()
    os.makedirs(a.out, exist_ok=True)
    brief = open(a.brief).read()
    refs = [open(p, "rb").read() for p in a.style]
    ref_prov = [{"path": p, "sha256": sha256(b)} for p, b in zip(a.style, refs)]
    base = dict(brief_path=a.brief, brief_sha256=sha256(brief.encode()), references=ref_prov)

    # 1. concept, 3/4 hero view
    p1 = ("You are a concept artist for a video game. Produce ONE concept image of the asset described in the brief. "
          "Three-quarter front view, slightly above eye level, neutral mid-grey studio background, soft even lighting, "
          "no text, no watermark, no people for scale unless the brief asks. Follow the brief's palette exactly. "
          "If reference images are attached, match their style, materials and palette.\n\nBRIEF:\n" + brief)
    cpath = f"{a.out}/{a.name}-concept.png"
    if a.reuse_concept and os.path.exists(cpath):
        concept = open(cpath, "rb").read(); print("concept reused")
    else:
        concept = generate(a.project, a.location, a.model, [{"text": p1}] + [img_part(r) for r in refs],
                           cpath, dict(base, stage="concept", prompt=p1))
        print("concept ok")

    # 2. orthographic views, one per call, conditioned on the concept
    views = {"front": "FRONT ELEVATION: the camera sits directly ahead of the nose at the object's mid-height, looking straight aft along the "
                      "object's long axis; the nose points at the viewer; wings appear as thin horizontal edges; the top of the object is NOT visible",
             "side": "LEFT SIDE ELEVATION: the camera sits directly to the object's left at mid-height, looking at the flank; the nose points left",
             "back": "REAR ELEVATION: the camera sits directly behind the object at mid-height, looking forward along the long axis; the thruster "
                     "nozzles face the viewer; wings appear as thin horizontal edges; the top of the object is NOT visible",
             "top": "TOP PLAN: the camera is directly above, looking straight down; the nose points up the page"}
    views = {k: v for k, v in views.items() if k in a.views.split(",")}
    ortho = {}
    for v, desc in views.items():
        p2 = (f"Using the attached concept image as the single source of truth for this exact object, draw the SAME object as a strict "
              f"orthographic view. VIEW: {desc}. Requirements: orthographic projection, zero perspective distortion, flat camera, object centred and "
              f"filling ~80% of the frame, plain flat mid-grey background (#7f7f7f), soft even lighting, no shadows on the ground, "
              f"no text, no labels, no extra objects. Keep every part, proportion, material and colour identical to the concept.")
        ortho[v] = generate(a.project, a.location, a.model, [{"text": p2}, img_part(concept)],
                            f"{a.out}/{a.name}-ortho-{v}.png",
                            dict(base, stage=f"ortho-{v}", prompt=p2, input_sha256=sha256(concept)), temperature=0.2)
        print(f"ortho {v} ok")

    # 3. callout sheet: one composed sheet with labels, dimensions, materials
    p3 = ("Compose a production CALLOUT SHEET for 3D modellers from the attached images (concept, then front, side, back, top orthographic views). "
          "Layout: the four orthographic views on a grid at identical scale, the concept small in a corner. Add clear leader lines and short "
          "labels calling out: overall dimensions from the brief, main material zones (name + short PBR note), moving parts, hardpoints, "
          "emissive elements, decals. Include a palette strip with the brief's hex colours. Clean technical style, white background, "
          "legible sans-serif text. Do not redesign the object.\n\nBRIEF:\n" + brief)
    generate(a.project, a.location, a.model,
             [{"text": p3}, img_part(concept)] + [img_part(ortho[v]) for v in views],
             f"{a.out}/{a.name}-callout.png",
             dict(base, stage="callout", prompt=p3, input_sha256=[sha256(concept)] + [sha256(ortho[v]) for v in views]), temperature=0.3)
    print("callout ok")

if __name__ == "__main__":
    main()
