#!/usr/bin/env python3
"""Visual part choice: for each zone, rank candidate INSERT thumbnails against the concept art (Vertex AI Gemini, gcloud auth).
Writes choices.json: zone -> ordered insert names with the model's reasons. Candidates come from tags + geometry filters."""
import argparse, base64, glob, json, os, random, subprocess, time, urllib.request
FLAT = {"hull-panel", "plate", "hatch", "grille", "vent", "strip", "rib"}
def token(): return subprocess.check_output(["gcloud", "auth", "print-access-token"], text=True).strip()
def img(p): return {"inlineData": {"mimeType": "image/png", "data": base64.b64encode(open(p, "rb").read()).decode()}}
ap = argparse.ArgumentParser(); ap.add_argument("--project", required=True); ap.add_argument("--model", default="gemini-3.1-flash-image"); ap.add_argument("--master", required=True)
ap.add_argument("--zones", required=True); ap.add_argument("--concept", required=True); ap.add_argument("--out", required=True); ap.add_argument("--candidates", type=int, default=8); ap.add_argument("--seed", type=int, default=7); a = ap.parse_args()
random.seed(a.seed); Z = json.load(open(a.zones)); lib = []
for kp in sorted(glob.glob(os.path.join(a.master, "*"))):
    fp, lp = os.path.join(kp, "features.json"), os.path.join(kp, "labels.json")
    if not os.path.exists(fp): continue
    F = json.load(open(fp)); L = json.load(open(lp)) if os.path.exists(lp) else {}
    for n, f in F.items():
        l = L.get(n, {}); lib.append({"name": n, "png": os.path.join(kp, n + ".png"), "tags": l.get("tags", []), "conf": l.get("confidence", 0), "planar": f["planar_fraction"], "below": f.get("below_plane", 0), "dims": f["dims"], "attach": f.get("attach", ["surface"])})
choices = {}; used = set(); tok = 0
for z in Z["zones"]:
    flat = any(t in FLAT for t in z["tags"]); need_sock = z["type"] == "connect"
    c = [x for x in lib if any(t in x["tags"] for t in z["tags"]) and x["conf"] >= 0.6 and x["name"] not in used and (not need_sock or "sockets" in x["attach"]) and (not flat or (x["planar"] >= 0.25 and x["below"] <= 0.15 * max(x["dims"])))]
    if not c: c = [x for x in lib if any(t in x["tags"] for t in z["tags"]) and x["name"] not in used and (not need_sock or "sockets" in x["attach"])]
    if not c: choices[z["name"]] = {"ranked": [], "note": "no candidates"}; print(z["name"], "no candidates"); continue
    random.shuffle(c); cand = c[:a.candidates]
    prompt = (f"You choose kitbash parts to match concept art. The first image is the concept of a spacecraft. Zone '{z['name']}' reproduces this feature: {z.get('concept_feature','')} "
              f"(tags {z['tags']}). The following {len(cand)} images are candidate parts, numbered 1..{len(cand)}, untextured, seen from a 3/4 angle. Rank ALL candidates by how well their shape language, "
              f"proportions and detail density match that feature in the concept and the craft's utilitarian, panelled, riveted style. Reply as JSON: {{\"ranking\": [ints], \"reason\": \"<=20 words\"}}.")
    parts = [{"text": prompt}, {"text": "CONCEPT:"}, img(a.concept)]
    for i, x in enumerate(cand): parts += [{"text": f"CANDIDATE {i+1}:"}, img(x["png"])]
    body = {"contents": [{"role": "user", "parts": parts}], "generationConfig": {"temperature": 0.1, "responseMimeType": "application/json", "maxOutputTokens": 4096, "thinkingConfig": {"thinkingBudget": 0}}}
    url = f"https://aiplatform.googleapis.com/v1/projects/{a.project}/locations/global/publishers/google/models/{a.model}:generateContent"
    try:
        req = urllib.request.Request(url, data=json.dumps(body).encode(), headers={"Authorization": f"Bearer {token()}", "Content-Type": "application/json"})
        with urllib.request.urlopen(req, timeout=180) as r: resp = json.loads(r.read())
        cand0 = resp["candidates"][0]
        if "content" not in cand0: raise RuntimeError(f"no content: finish={cand0.get('finishReason')} usage={resp.get('usageMetadata')}")
        txt = "".join(p.get("text", "") for p in cand0["content"]["parts"]); txt = txt[txt.find("{"): txt.rfind("}") + 1]
        v = json.loads(txt); tok += resp.get("usageMetadata", {}).get("promptTokenCount", 0)
        order = [cand[i-1]["name"] for i in v.get("ranking", []) if 1 <= i <= len(cand)] or [x["name"] for x in cand]
        choices[z["name"]] = {"ranked": order, "reason": v.get("reason", ""), "model": resp.get("modelVersion", a.model)}
    except Exception as e:
        body_err = getattr(e, "read", lambda: b"")(); order = [x["name"] for x in cand]
        choices[z["name"]] = {"ranked": order, "reason": f"fallback: {str(e)[:80]} {body_err[:160].decode(errors='ignore') if isinstance(body_err, bytes) else ''}"}
    for nm in order[:z.get("count", 1)]: used.add(nm)
    print(f"{z['name']:<22} -> {choices[z['name']]['ranked'][0][:40]} | {choices[z['name']].get('reason','')[:70]}")
json.dump(choices, open(a.out, "w"), indent=1); print("tokens", tok)
