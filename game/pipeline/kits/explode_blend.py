# blender -b <file.blend> --python explode_blend.py -- <out_dir> [min_tris]
# Splits every mesh into connected islands with bmesh (fast), drops scene dressing and tiny islands, centres each piece,
# saves an exploded library .blend and pieces.json. Big islands are the reusable modules (cannons, launchers, pods).
import bpy, bmesh, sys, os, json
from mathutils import Vector
a = sys.argv[sys.argv.index("--")+1:]; out = os.path.abspath(a[0]); min_tris = int(a[1]) if len(a) > 1 else 200; os.makedirs(out, exist_ok=True)
sc = bpy.context.scene
for o in list(bpy.data.objects):
    if o.type != 'MESH' or o.name == 'Plane' or o.dimensions.z < 0.01 and max(o.dimensions) > 20: bpy.data.objects.remove(o, do_unlink=True)
lib = bpy.data.collections.new("modules"); sc.collection.children.link(lib); rows = []
for src in [o for o in bpy.data.objects if o.type == 'MESH']:
    bm = bmesh.new(); bm.from_mesh(src.data); bm.verts.ensure_lookup_table(); bm.faces.ensure_lookup_table()
    seen = set(); islands = []
    for f in bm.faces:
        if f.index in seen: continue
        stack = [f]; isl = []
        while stack:
            g = stack.pop()
            if g.index in seen: continue
            seen.add(g.index); isl.append(g)
            for e in g.edges:
                for h in e.link_faces:
                    if h.index not in seen: stack.append(h)
        islands.append(isl)
    mats = src.data.materials
    for i, isl in enumerate(sorted(islands, key=lambda x: -len(x))):
        tris = sum(len(g.verts) - 2 for g in isl)
        if tris < min_tris: continue
        nb = bmesh.new(); vmap = {}
        for g in isl:
            vs = []
            for v in g.verts:
                if v.index not in vmap: vmap[v.index] = nb.verts.new(v.co)
                vs.append(vmap[v.index])
            try: nf = nb.faces.new(vs); nf.material_index = g.material_index
            except ValueError: pass
        me = bpy.data.meshes.new(f"{src.name}_{i:03d}"); nb.to_mesh(me); nb.free()
        for m in mats: me.materials.append(m)
        o = bpy.data.objects.new(me.name, me); o.matrix_world = src.matrix_world; lib.objects.link(o)
        bb = [o.matrix_world @ Vector(c) for c in o.bound_box]; mn = Vector(map(min, *bb)); mx = Vector(map(max, *bb)); ctr = (mn + mx) / 2
        me.transform(o.matrix_world); o.matrix_world.identity(); me.transform(bpy.types.Object.bl_rna and __import__('mathutils').Matrix.Translation(-ctr)); o.location = ctr
        rows.append({"id": o.name, "tris": tris, "dims": [round(x, 3) for x in (mx - mn)], "materials": [m.name for m in mats if m][:3]})
    bm.free(); bpy.data.objects.remove(src, do_unlink=True)
rows.sort(key=lambda r: -r["tris"]); json.dump(rows, open(os.path.join(out, "pieces.json"), "w"), indent=1)
print("PIECES", len(rows), "tris", sum(r["tris"] for r in rows)); [print("  ", r["id"], r["tris"], r["dims"]) for r in rows[:20]]
bpy.ops.wm.save_as_mainfile(filepath=os.path.join(out, "modules-exploded.blend"), compress=True); print("SAVED")
