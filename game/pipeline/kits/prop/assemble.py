# blender -b --python assemble.py -- <build.json> <out_dir>
# Free-space kitbash of a build list: each item is a palette part (library INSERT or primitive) scaled so its longest dimension is
# size_m, rotated by rot_deg (XYZ Euler), centred at pos_m. Ground plane at z=0 for the render. Writes assembly.glb/.blend and
# assembly.json (what was placed, with each item's world bbox for the critic).
import bpy, bmesh, sys, os, json, math
from mathutils import Vector, Matrix, Euler
a = sys.argv[sys.argv.index("--")+1:]; B = json.load(open(a[0])); out = os.path.abspath(a[1]); os.makedirs(out, exist_ok=True)
P = json.load(open(os.path.join(B["palette"], "palette.json"))); byid = {p["id"]: p for p in P["parts"]}
bpy.ops.wm.read_homefile(use_empty=True); sc = bpy.context.scene
def prim_mesh(kind):
    bm = bmesh.new()
    if kind == "tube": bmesh.ops.create_cone(bm, cap_ends=True, segments=16, radius1=0.025, radius2=0.025, depth=1.0); bmesh.ops.rotate(bm, cent=(0, 0, 0), matrix=Matrix.Rotation(math.pi / 2, 3, 'X'), verts=bm.verts)
    elif kind == "bar": bmesh.ops.create_cube(bm, size=1.0); bmesh.ops.scale(bm, vec=(0.05, 1.0, 0.05), verts=bm.verts)
    elif kind == "plate": bmesh.ops.create_cube(bm, size=1.0); bmesh.ops.scale(bm, vec=(1.0, 0.5, 0.02), verts=bm.verts)
    else: bmesh.ops.create_cube(bm, size=1.0); bmesh.ops.scale(bm, vec=(0.5, 0.3, 0.3), verts=bm.verts)
    me = bpy.data.meshes.new(kind); bm.to_mesh(me); bm.free(); return me
AX = {"x": Vector((1, 0, 0)), "y": Vector((0, 1, 0)), "z": Vector((0, 0, 1))}
def canonical(o):
    """Re-express the mesh so its longest extent is +y, its middle extent +x and its thinnest +z: the build list's 'along' then means what it says for every part."""
    d = list(o.dimensions); order = sorted(range(3), key=lambda k: -d[k])   # [long, mid, thin]
    R = Matrix.Identity(3)
    src = [Vector([1 if k == order[1] else 0 for k in range(3)]), Vector([1 if k == order[0] else 0 for k in range(3)]), Vector([1 if k == order[2] else 0 for k in range(3)])]   # part axes for x, y, z
    if src[0].cross(src[1]).dot(src[2]) < 0: src[2] = -src[2]   # keep it right-handed
    R = Matrix((src[0], src[1], src[2]))   # rows: new x, y, z expressed in old coordinates
    for v in o.data.vertices: v.co = R @ v.co
    bpy.context.view_layer.update()
def orient(along, spin_deg, tilt_deg=0):
    """Rotation taking the canonical frame (long +y, thin +z) to: long axis along `along`, spin about it, then tilt about world y (a diagonal brace in the xz plane)."""
    if along == "y": base = Matrix.Identity(4)
    elif along == "x": base = Matrix.Rotation(-math.pi / 2, 4, 'Z')
    else: base = Matrix.Rotation(math.pi / 2, 4, 'X')   # +y -> +z, thin axis (+z) -> -y: the flat face looks toward the viewer
    return Matrix.Rotation(-math.radians(tilt_deg or 0), 4, 'Y') @ Matrix.Rotation(math.radians(spin_deg or 0), 4, AX[along]) @ base
def load(p):
    if p["kind"] == "prim": o = bpy.data.objects.new(p["name"], prim_mesh(p["name"])); sc.collection.objects.link(o); return o
    with bpy.data.libraries.load(p["blend"]) as (src, dst): dst.objects = list(src.objects)
    o = [x for x in dst.objects if x and x.type == 'MESH'][0]; sc.collection.objects.link(o)
    for x in dst.objects:
        if x and x.type == 'EMPTY': bpy.data.objects.remove(x, do_unlink=True)
    bb = [Vector(c) for c in o.bound_box]; ctr = sum(bb, Vector()) / 8
    for v in o.data.vertices: v.co -= ctr   # the build list positions centres
    tr = sum(len(pg.vertices) - 2 for pg in o.data.polygons)
    if tr > 4000:
        m = o.modifiers.new("dec", 'DECIMATE'); m.ratio = 4000 / tr; bpy.context.view_layer.objects.active = o; bpy.ops.object.modifier_apply(modifier="dec")
    return o
mat = bpy.data.materials.new("prop"); mat.use_nodes = True; bs = mat.node_tree.nodes.get("Principled BSDF"); bs.inputs["Base Color"].default_value = (0.25, 0.27, 0.29, 1); bs.inputs["Roughness"].default_value = 0.55
dark = bpy.data.materials.new("dark"); dark.use_nodes = True; bs2 = dark.node_tree.nodes.get("Principled BSDF"); bs2.inputs["Base Color"].default_value = (0.03, 0.03, 0.035, 1)
placed = []; objs = []
sizes = {int(r["part"]): float(r["size_m"]) for r in B.get("part_sizes", [])}   # one size per part id for the whole build
for i, it in enumerate(B["items"]):
    p = byid.get(int(it["part"]))
    if not p: print("unknown part", it); continue
    size = sizes.get(p["id"], it.get("size_m")) or 0.5; n = max(1, int(it.get("count", 1)))
    rot = orient(it["along"], it.get("spin_deg", 0), it.get("tilt_deg", 0)) if "along" in it else Euler([math.radians(v) for v in it.get("rot_deg", [0, 0, 0])[:3]], 'XYZ').to_matrix().to_4x4()
    run_axis = (rot @ Vector((0, 1, 0, 0))).to_3d().normalized()   # the run direction is the part's long axis after orientation (tilt included)
    for k in range(n):
        o = load(p); canonical(o); d = max(o.dimensions) or 1.0; s = size / d
        ctr = Vector(it["pos_m"][:3]) + run_axis * ((k - (n - 1) / 2) * size)   # instances end to end, the run centred on pos_m
        o.matrix_world = Matrix.Translation(ctr) @ rot @ Matrix.Scale(s, 4)
        o.name = f"{i:02d}_{k}_{it['name']}"; o.data.materials.clear(); o.data.materials.append(dark if p["kind"] == "insert" and "bracket" in p["desc"] else mat); objs.append(o)
        bpy.context.view_layer.update(); ws = [o.matrix_world @ Vector(c) for c in o.bound_box]
        placed.append({"i": i, "k": k, "name": it["name"], "part": p["id"], "part_name": p["name"], "pos_m": [round(v, 3) for v in ctr], "run_pos_m": it["pos_m"], "count": n, "along": it.get("along"), "spin_deg": it.get("spin_deg", 0), "tilt_deg": it.get("tilt_deg", 0), "size_m": size,
                       "bbox_min": [round(min(w[kk] for w in ws), 3) for kk in range(3)], "bbox_max": [round(max(w[kk] for w in ws), 3) for kk in range(3)]})
for o in objs: o.select_set(True)
if objs:
    bpy.context.view_layer.objects.active = objs[0]; bpy.ops.object.join(); prop = bpy.context.object; prop.name = B.get("prop", "prop").replace(" ", "_")
    tris = sum(len(pg.vertices) - 2 for pg in prop.data.polygons)
else: tris = 0
json.dump({"prop": B.get("prop"), "placed": placed, "tris": tris}, open(os.path.join(out, "assembly.json"), "w"), indent=1)
bpy.ops.wm.save_as_mainfile(filepath=os.path.join(out, "assembly.blend"), compress=True); bpy.ops.export_scene.gltf(filepath=os.path.join(out, "assembly.glb"), export_format='GLB')
print("ASSEMBLED", len(placed), "items", tris, "tris")
