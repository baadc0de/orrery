#!/usr/bin/env python3
"""Write manifest.json for a kit directory in the private-store layout (kits/<vendor>/<slug>/<version>/).
Hashes every file under source/ and extracted/, records licence facts passed on the command line, leaves text_sha256 null
until a LICENSE.* file is present. Blender-side part inventory is added by ingest_parts.py (separate, headless bpy)."""
import argparse, hashlib, json, os, time
ap = argparse.ArgumentParser(); ap.add_argument("kit_dir"); ap.add_argument("--kit-id", required=True); ap.add_argument("--vendor", required=True)
ap.add_argument("--version", default="1"); ap.add_argument("--license-name", required=True); ap.add_argument("--redistribute-source", default="false")
ap.add_argument("--derived-public", default="false"); ap.add_argument("--purchased-on", default=None); ap.add_argument("--note", default="")
a = ap.parse_args()
def sha(p):
    h = hashlib.sha256()
    with open(p, "rb") as f:
        for b in iter(lambda: f.read(1 << 20), b""): h.update(b)
    return h.hexdigest()
files = []
for sub in ("source", "extracted"):
    d = os.path.join(a.kit_dir, sub)
    for root, _, fs in os.walk(d):
        for f in sorted(fs):
            p = os.path.join(root, f); files.append({"file": os.path.relpath(p, a.kit_dir), "sha256": sha(p), "bytes": os.path.getsize(p)})
lic = [f for f in os.listdir(a.kit_dir) if f.upper().startswith("LICENSE")]
m = {"kit_id": a.kit_id, "version": a.version, "vendor": a.vendor, "purchased_on": a.purchased_on,
     "license": {"name": a.license_name, "spdx": None, "redistribute_source": a.redistribute_source == "true", "use_in_product": True,
                 "derived_meshes_public": a.derived_public == "true", "text_sha256": sha(os.path.join(a.kit_dir, lic[0])) if lic else None,
                 "todo": None if lic else "licence text not yet saved beside the kit"},
     "files": files, "note": a.note, "parts": None, "ingested_at": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())}
json.dump(m, open(os.path.join(a.kit_dir, "manifest.json"), "w"), indent=2)
print(a.kit_id, len(files), "files,", sum(f["bytes"] for f in files) // 2**20, "MiB, licence text:", "yes" if lic else "MISSING")
