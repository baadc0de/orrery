# blender -b --python hull_from_mesh.py -- <in.glb> <out_dir> [target_tris]
# Coarsen an image-to-3D mesh into a symmetric mount hull and tag every face with a semantic region:
# side in {top, belly, flank, nose, tail} x long in {fore, mid, aft} x lat in {inner, outer}. Nose is +Y after alignment.
import bpy, bmesh, sys, os, json, math
from mathutils import Vector, Matrix
a = sys.argv[sys.argv.index("--")+1:]; src, out = a[0], os.path.abspath(a[1]); target = int(a[2]) if len(a) > 2 else 3000; os.makedirs(out, exist_ok=True)
bpy.ops.wm.read_homefile(use_empty=True); sc = bpy.context.scene
bpy.ops.import_scene.gltf(filepath=os.path.abspath(src))
ms = [o for o in bpy.data.objects if o.type == 'MESH']
for o in ms: o.select_set(True)
bpy.context.view_layer.objects.active = ms[0]
if len(ms) > 1: bpy.ops.object.join()
hull = bpy.context.view_layer.objects.active; hull.name = "hull"; bpy.ops.object.transform_apply(location=True, rotation=True, scale=True)
# align: longest horizontal axis -> Y (nose +Y decided by where the mesh is narrower: the nose end has less cross-section)
bb = [Vector(c) for c in hull.bound_box]; mn = Vector(map(min, *bb)); mx = Vector(map(max, *bb)); ext = mx - mn; ctr = (mn + mx) / 2
for v in hull.data.vertices: v.co -= ctr
# lateral axis = the one with the smaller mirror error (the craft is bilaterally symmetric); length is the other one
from mathutils.kdtree import KDTree
kd = KDTree(len(hull.data.vertices)); [kd.insert(v.co, i) for i, v in enumerate(hull.data.vertices)]; kd.balance()
def merr(axis):
    tot = 0; n = 0
    for i, v in enumerate(hull.data.vertices):
        if i % 11: continue
        m = v.co.copy(); m[axis] = -m[axis]; tot += kd.find(m)[2]; n += 1
    return tot / max(1, n)
ex, ey = merr(0), merr(1); print("mirror error x", round(ex, 5), "y", round(ey, 5))
if ey < ex: hull.data.transform(Matrix.Rotation(math.pi / 2, 4, 'Z')); hull.data.update()  # lateral was y: rotate so lateral is x
ys = [v.co.y for v in hull.data.vertices]; L = max(ys) - min(ys)
def width_at(y0, y1): sel = [v.co.x for v in hull.data.vertices if y0 <= v.co.y <= y1]; return (max(sel) - min(sel)) if sel else 0
if width_at(max(ys) - 0.15 * L, max(ys)) > width_at(min(ys), min(ys) + 0.15 * L):  # wide end is aft: flip so nose is +Y
    hull.data.transform(Matrix.Rotation(math.pi, 4, 'Z')); hull.data.update()
# coarsen: voxel remesh then decimate, then symmetrize across x
hull.data.materials.clear()
size = max(hull.dimensions); hull.data.remesh_voxel_size = size / 170; bpy.ops.object.voxel_remesh()
m = hull.modifiers.new("dec", 'DECIMATE'); m.ratio = min(1.0, target / max(1, len(hull.data.polygons) * 2)); m.use_symmetry = True; m.symmetry_axis = 'X'; bpy.ops.object.modifier_apply(modifier="dec")
bpy.ops.object.mode_set(mode='EDIT'); bpy.ops.mesh.select_all(action='SELECT'); bpy.ops.mesh.symmetrize(direction='NEGATIVE_X', threshold=size * 0.002); bpy.ops.mesh.remove_doubles(threshold=size * 0.0005); bpy.ops.object.mode_set(mode='OBJECT')
try: bpy.ops.object.shade_smooth_by_angle(angle=math.radians(30))
except Exception: bpy.ops.object.shade_flat()
# scale to the brief: 9 m long
ys = [v.co.y for v in hull.data.vertices]; s = 9.0 / (max(ys) - min(ys)); hull.data.transform(Matrix.Scale(s, 4)); hull.data.update()
dims = [max(v.co[i] for v in hull.data.vertices) - min(v.co[i] for v in hull.data.vertices) for i in range(3)]
# region atlas as face attributes
bb = [Vector(c) for c in hull.bound_box]; mn = Vector(map(min, *bb)); mx = Vector(map(max, *bb)); ext = mx - mn
side_attr = hull.data.attributes.new("region_side", 'INT', 'FACE'); long_attr = hull.data.attributes.new("region_long", 'INT', 'FACE'); lat_attr = hull.data.attributes.new("region_lat", 'INT', 'FACE')
SIDES = ["top", "belly", "flank", "nose", "tail"]; LONGS = ["fore", "mid", "aft"]; LATS = ["inner", "outer"]
counts = {}
for f in hull.data.polygons:
    n = f.normal; c = f.center
    if n.z > 0.6: sd = 0
    elif n.z < -0.6: sd = 1
    elif abs(n.y) > 0.7: sd = 3 if n.y > 0 else 4
    else: sd = 2
    t = (c.y - mn.y) / max(1e-6, ext.y); lg = 0 if t > 0.66 else (1 if t > 0.33 else 2)
    lt = 0 if abs(c.x) < 0.35 * ext.x / 2 else 1
    side_attr.data[f.index].value = sd; long_attr.data[f.index].value = lg; lat_attr.data[f.index].value = lt
    key = f"{SIDES[sd]}.{LONGS[lg]}.{LATS[lt]}"; counts[key] = counts.get(key, 0) + f.area
atlas = {"dims_m": [round(x, 3) for x in dims], "tris": sum(len(p.vertices) - 2 for p in hull.data.polygons), "regions": {k: round(v, 3) for k, v in sorted(counts.items(), key=lambda kv: -kv[1])},
         "vocabulary": {"side": SIDES, "long": LONGS, "lat": LATS}, "nose": "+Y", "up": "+Z", "mirror": "x"}
json.dump(atlas, open(os.path.join(out, "hull_atlas.json"), "w"), indent=1)
bpy.ops.wm.save_as_mainfile(filepath=os.path.join(out, "hull.blend"), compress=True)
bpy.ops.export_scene.gltf(filepath=os.path.join(out, "hull.glb"), export_format='GLB', use_selection=False)
print("HULL", atlas["dims_m"], atlas["tris"], "tris; top regions", list(atlas["regions"].items())[:6])
