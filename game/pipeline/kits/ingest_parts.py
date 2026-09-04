# blender -b --python ingest_parts.py -- <kit_dir> <glob-relative-to-extracted>
# Imports each matching file headless, records every mesh part (name, tris, dims, materials) into manifest.json["parts"].
import bpy, sys, os, json, glob
a = sys.argv[sys.argv.index("--")+1:]; kit, pat = a[0], a[1]
mp = os.path.join(kit, "manifest.json"); m = json.load(open(mp)); parts = []
for f in sorted(glob.glob(os.path.join(kit, "extracted", pat), recursive=True)):
    bpy.ops.wm.read_factory_settings(use_empty=True)
    ext = f.rsplit(".", 1)[-1].lower()
    if ext == "obj": bpy.ops.wm.obj_import(filepath=f)
    elif ext == "fbx": bpy.ops.import_scene.fbx(filepath=f)
    elif ext == "blend":
        with bpy.data.libraries.load(f) as (src, dst): dst.objects = src.objects
        for o in dst.objects:
            if o: bpy.context.scene.collection.objects.link(o)
    else: continue
    for o in [o for o in bpy.data.objects if o.type == 'MESH']:
        parts.append({"id": o.name, "file": os.path.relpath(f, kit), "tris": sum(len(p.vertices)-2 for p in o.data.polygons),
                      "dims": [round(x, 3) for x in o.dimensions], "materials": [mm.name for mm in o.data.materials if mm]})
    print("PARTS", os.path.basename(f), len([o for o in bpy.data.objects if o.type == 'MESH']), flush=True)
m["parts"] = parts; m["parts_total_tris"] = sum(p["tris"] for p in parts); json.dump(m, open(mp, "w"), indent=2)
print("DONE", len(parts), "parts", m["parts_total_tris"], "tris")
