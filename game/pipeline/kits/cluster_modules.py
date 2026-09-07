# blender -b <modules-exploded.blend> --python cluster_modules.py -- <out_dir> [gap]
# Groups exploded islands into modules by bounding-box proximity (islands whose boxes, grown by `gap`, touch a chain).
# Joins each cluster, centres it, writes modules.json and modules.blend. Big clusters are the weapons/pods to keep.
import bpy, sys, os, json
from mathutils import Vector
a = sys.argv[sys.argv.index("--")+1:]; out = os.path.abspath(a[0]); gap = float(a[1]) if len(a) > 1 else 0.02
objs = [o for o in bpy.data.objects if o.type == 'MESH']
boxes = []
for o in objs:
    bb = [o.matrix_world @ Vector(c) for c in o.bound_box]; boxes.append((Vector(map(min, *bb)) - Vector((gap,)*3), Vector(map(max, *bb)) + Vector((gap,)*3)))
def touch(i, j):
    (a0, a1), (b0, b1) = boxes[i], boxes[j]; return all(a0[k] <= b1[k] and b0[k] <= a1[k] for k in range(3))
parent = list(range(len(objs)))
def find(x):
    while parent[x] != x: parent[x] = parent[parent[x]]; x = parent[x]
    return x
order = sorted(range(len(objs)), key=lambda i: boxes[i][0].x)
for ii, i in enumerate(order):
    for j in order[ii+1:]:
        if boxes[j][0].x > boxes[i][1].x: break
        if touch(i, j): parent[find(i)] = find(j)
groups = {}
for i in range(len(objs)): groups.setdefault(find(i), []).append(objs[i])
rows = []
for gi, (root, members) in enumerate(sorted(groups.items(), key=lambda kv: -sum(len(o.data.polygons) for o in kv[1]))):
    for o in bpy.data.objects: o.select_set(False)
    for o in members: o.select_set(True)
    bpy.context.view_layer.objects.active = members[0]
    if len(members) > 1: bpy.ops.object.join()
    m = bpy.context.view_layer.objects.active; m.name = f"module_{gi:03d}"
    bpy.ops.object.origin_set(type='ORIGIN_GEOMETRY', center='BOUNDS')
    rows.append({"id": m.name, "islands": len(members), "tris": sum(len(p.vertices) - 2 for p in m.data.polygons), "dims": [round(x, 3) for x in m.dimensions], "location": [round(x, 3) for x in m.location]})
json.dump(rows, open(os.path.join(out, "modules.json"), "w"), indent=1)
print("MODULES", len(rows), "from", len(objs), "islands"); [print("  ", r["id"], r["islands"], "islands", r["tris"], "tris", r["dims"]) for r in rows[:15]]
bpy.ops.wm.save_as_mainfile(filepath=os.path.join(out, "modules.blend"), compress=True); print("SAVED")
