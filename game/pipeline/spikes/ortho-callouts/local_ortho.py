#!/usr/bin/env python3
"""Local open-weight track (G12.2a): FLUX.2 klein 4B (Apache-2.0) on ComfyUI, reference-conditioned
orthographic views from a concept image. Deterministic from (seed, weights, graph). Provenance per output."""
import argparse, hashlib, json, os, sys, time, urllib.request, uuid, shutil

VIEWS = {
 "front": "FRONT ELEVATION: camera directly ahead of the nose at mid-height looking aft along the long axis; nose points at the viewer; wings are thin horizontal edges; the top surface is NOT visible",
 "side":  "LEFT SIDE ELEVATION: camera directly to the left at mid-height; nose points left",
 "back":  "REAR ELEVATION: camera directly behind at mid-height looking forward; thruster nozzles face the viewer; wings are thin horizontal edges; the top surface is NOT visible",
 "top":   "TOP PLAN: camera directly above looking straight down; nose points up the page",
}

def sha(b): return hashlib.sha256(b).hexdigest()

def graph(ref_name, prompt, seed, steps, cfg, w, h, prefix, ctrl_name=None):
    g = {
     "1": {"class_type": "UNETLoader", "inputs": {"unet_name": "flux-2-klein-4b.safetensors", "weight_dtype": "default"}},
     "2": {"class_type": "CLIPLoader", "inputs": {"clip_name": "qwen_3_4b.safetensors", "type": "flux2", "device": "default"}},
     "3": {"class_type": "VAELoader", "inputs": {"vae_name": "flux2-vae.safetensors"}},
     "4": {"class_type": "LoadImage", "inputs": {"image": ref_name}},
     "5": {"class_type": "ImageScaleToTotalPixels", "inputs": {"image": ["4", 0], "upscale_method": "lanczos", "megapixels": 1.0, "resolution_steps": 16}},
     "6": {"class_type": "VAEEncode", "inputs": {"pixels": ["5", 0], "vae": ["3", 0]}},
     "7": {"class_type": "CLIPTextEncode", "inputs": {"clip": ["2", 0], "text": prompt}},
     "8": {"class_type": "ReferenceLatent", "inputs": {"conditioning": ["7", 0], "latent": ["6", 0]}},
     "9": {"class_type": "CLIPTextEncode", "inputs": {"clip": ["2", 0], "text": ""}},
     "10": {"class_type": "EmptyFlux2LatentImage", "inputs": {"width": w, "height": h, "batch_size": 1}},
     "11": {"class_type": "KSampler", "inputs": {"model": ["1", 0], "positive": ["8", 0], "negative": ["9", 0], "latent_image": ["10", 0],
                                                 "seed": seed, "steps": steps, "cfg": cfg, "sampler_name": "euler", "scheduler": "simple", "denoise": 1.0}},
     "12": {"class_type": "VAEDecode", "inputs": {"samples": ["11", 0], "vae": ["3", 0]}},
     "13": {"class_type": "SaveImage", "inputs": {"images": ["12", 0], "filename_prefix": prefix}},
    }
    if ctrl_name:
        g["20"] = {"class_type": "LoadImage", "inputs": {"image": ctrl_name}}
        g["21"] = {"class_type": "ImageScaleToTotalPixels", "inputs": {"image": ["20", 0], "upscale_method": "lanczos", "megapixels": 1.0, "resolution_steps": 16}}
        g["22"] = {"class_type": "VAEEncode", "inputs": {"pixels": ["21", 0], "vae": ["3", 0]}}
        g["23"] = {"class_type": "ReferenceLatent", "inputs": {"conditioning": ["8", 0], "latent": ["22", 0]}}
        g["11"]["inputs"]["positive"] = ["23", 0]
    return g

def run(server, g):
    cid = str(uuid.uuid4())
    req = urllib.request.Request(f"{server}/prompt", data=json.dumps({"prompt": g, "client_id": cid}).encode(), headers={"Content-Type": "application/json"})
    with urllib.request.urlopen(req) as r: pid = json.loads(r.read())["prompt_id"]
    t0 = time.time()
    while True:
        with urllib.request.urlopen(f"{server}/history/{pid}") as r: h = json.loads(r.read())
        if pid in h:
            st = h[pid].get("status", {})
            if st.get("status_str") == "error": raise SystemExit(json.dumps(h[pid]["status"], indent=1)[:2000])
            outs = h[pid]["outputs"]
            imgs = [o for v in outs.values() for o in v.get("images", [])]
            if imgs: return imgs[0], time.time() - t0
        if time.time() - t0 > 900: raise SystemExit("timeout")
        time.sleep(1.0)

def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--server", default="http://127.0.0.1:8188"); ap.add_argument("--comfy", default=os.path.expanduser("~/ComfyUI"))
    ap.add_argument("--concept", required=True); ap.add_argument("--views", default="front,side,back,top")
    ap.add_argument("--seed", type=int, default=7); ap.add_argument("--steps", type=int, default=4); ap.add_argument("--cfg", type=float, default=1.0)
    ap.add_argument("--size", type=int, default=1024); ap.add_argument("--control-dir", default=None, help="dir with depth-<view>.png proxy renders"); ap.add_argument("--out", default="out"); ap.add_argument("--name", default="escort-local")
    a = ap.parse_args()
    os.makedirs(a.out, exist_ok=True)
    concept = open(a.concept, "rb").read()
    ref_name = os.path.basename(a.concept); shutil.copy(a.concept, os.path.join(a.comfy, "input", ref_name))
    for v in a.views.split(","):
        prompt = (f"Using the reference image as the single source of truth for this exact object, draw the SAME object as a strict orthographic view. "
                  f"VIEW: {VIEWS[v]}. Orthographic projection, zero perspective, flat camera, object centred filling ~80% of the frame, plain flat mid-grey "
                  f"background, soft even lighting, no ground shadow, no text, no labels, no extra objects. Keep every part, proportion, material and colour identical.")
        prefix = f"{a.name}-ortho-{v}"
        ctrl = None
        if a.control_dir:
            cpath = os.path.join(a.control_dir, f"depth-{v}.png"); ctrl = f"{a.name}-depth-{v}.png"; shutil.copy(cpath, os.path.join(a.comfy, "input", ctrl))
            prompt = (f"Two reference images. Image 1 is the concept art of the object: copy its design, panels, decals, materials and colours exactly. "
                      f"Image 2 is a depth map (white = near, black = far, grey background) rendered from a strict orthographic camera in the exact view required; "
                      f"match its silhouette, proportions and camera angle EXACTLY and paint the object from image 1 into that silhouette. VIEW: {VIEWS[v]}. "
                      f"Flat mid-grey background, soft even lighting, no shadow, no text, no extra objects.")
        g = graph(ref_name, prompt, a.seed, a.steps, a.cfg, a.size, a.size, prefix, ctrl)
        info, secs = run(a.server, g)
        src = os.path.join(a.comfy, "output", info["subfolder"], info["filename"]) if info.get("subfolder") else os.path.join(a.comfy, "output", info["filename"])
        img = open(src, "rb").read(); dst = f"{a.out}/{prefix}.png"; open(dst, "wb").write(img)
        prov = {"stage": f"ortho-{v}", "track": "local", "model": "black-forest-labs/FLUX.2-klein-4B via Comfy-Org repack", "license": "apache-2.0",
                "runner": "ComfyUI", "graph": g, "seed": a.seed, "steps": a.steps, "cfg": a.cfg, "input_sha256": sha(concept), "control": ctrl, "output": os.path.basename(dst),
                "output_sha256": sha(img), "seconds": round(secs, 2), "generated_at": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())}
        json.dump(prov, open(dst[:-4] + ".provenance.json", "w"), indent=2)
        print(f"{v}: {secs:.1f}s -> {dst}")

if __name__ == "__main__": main()
