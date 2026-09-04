#!/usr/bin/env python3
"""Image -> 3D with TRELLIS.2-4B (MIT). Input: a concept/ortho PNG. Output: <name>.glb + provenance JSON.
Run with ~/trellis2-env/bin/python from ~/TRELLIS.2 (needs its package on sys.path)."""
import os, sys, json, time, hashlib, argparse
os.environ['OPENCV_IO_ENABLE_OPENEXR'] = '1'; os.environ["PYTORCH_CUDA_ALLOC_CONF"] = "expandable_segments:True"
os.environ.setdefault("ATTN_BACKEND", "flash_attn")
ap = argparse.ArgumentParser(); ap.add_argument("--image", required=True); ap.add_argument("--out", required=True); ap.add_argument("--name", default="asset")
ap.add_argument("--weights", default=os.path.expanduser("~/models/TRELLIS.2-4B")); ap.add_argument("--seed", type=int, default=7)
ap.add_argument("--decimate", type=int, default=200000); ap.add_argument("--texture", type=int, default=2048); ap.add_argument("--no-rembg", action="store_true")
a = ap.parse_args(); os.makedirs(a.out, exist_ok=True)
sys.path.insert(0, os.path.expanduser("~/TRELLIS.2"))
import torch; from PIL import Image
from trellis2.pipelines import Trellis2ImageTo3DPipeline
import o_voxel
raw = open(a.image, "rb").read(); img = Image.open(a.image).convert("RGBA")
if not a.no_rembg:
    from rembg import remove; img = remove(img)
    img.save(os.path.join(a.out, f"{a.name}-input-rgba.png"))
t0 = time.time()
pipe = Trellis2ImageTo3DPipeline.from_pretrained(a.weights); pipe.cuda()
t1 = time.time(); torch.manual_seed(a.seed)
mesh = pipe.run(img, seed=a.seed)[0] if "seed" in pipe.run.__code__.co_varnames else pipe.run(img)[0]
t2 = time.time()
mesh.simplify(16777216)
glb = o_voxel.postprocess.to_glb(vertices=mesh.vertices, faces=mesh.faces, attr_volume=mesh.attrs, coords=mesh.coords, attr_layout=mesh.layout,
    voxel_size=mesh.voxel_size, aabb=[[-0.5, -0.5, -0.5], [0.5, 0.5, 0.5]], decimation_target=a.decimate, texture_size=a.texture,
    remesh=True, remesh_band=1, remesh_project=0, verbose=False)
path = os.path.join(a.out, f"{a.name}.glb"); glb.export(path, extension_webp=False); t3 = time.time()
b = open(path, "rb").read()
prov = {"stage": "model", "track": "local", "model": "microsoft/TRELLIS.2-4B", "license": "mit", "seed": a.seed, "input": os.path.basename(a.image),
        "input_sha256": hashlib.sha256(raw).hexdigest(), "rembg": not a.no_rembg, "decimation_target": a.decimate, "texture_size": a.texture,
        "output": os.path.basename(path), "output_sha256": hashlib.sha256(b).hexdigest(), "output_bytes": len(b),
        "seconds": {"load": round(t1-t0,1), "generate": round(t2-t1,1), "export": round(t3-t2,1)}, "generated_at": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())}
json.dump(prov, open(path[:-4] + ".provenance.json", "w"), indent=2); print(json.dumps(prov["seconds"]), path)
