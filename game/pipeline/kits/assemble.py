# blender -b --python assemble.py -- <masterfolder> <out_dir> [seed]
# Kitbash spike, bottom-up: blockout from the escort brief -> pick hull faces -> place KIT OPS INSERTs from the Orrery
# masterfolder by label -> align to face, scale to fit, mirror across x -> join -> record the assembly graph -> render.
import bpy, bmesh, sys, os, json, math, random, glob
from mathutils import Vector, Matrix
a = sys.argv[sys.argv.index("--")+1:]; master, out = a[0], os.path.abspath(a[1]); seed = int(a[2]) if len(a) > 2 else 7
random.seed(seed); os.makedirs(out, exist_ok=True)
bpy.ops.wm.read_homefile(use_empty=True); sc = bpy.context.scene
# --- library: every INSERT with its labels
lib = []
for kp in sorted(glob.glob(os.path.join(master, "*"))):
    lp, fp = os.path.join(kp, "labels.json"), os.path.join(kp, "features.json")
    if not os.path.exists(fp): continue
    feats = json.load(open(fp)); labels = json.load(open(lp)) if os.path.exists(lp) else {}
    for n, f in feats.items():
        lib.append({"name": n, "blend": os.path.join(kp, n + ".blend"), "tags": labels.get(n, {}).get("tags", [labels.get(n, {}).get("heuristic", "misc")]), "dims": f["dims"], "tris": f["tris"], "kit": f["kit"]})
print("library", len(lib), "inserts")
def pick(tags_wanted, max_aspect=None):
    c = [x for x in lib if any(t in x["tags"] for t in tags_wanted)]
    if max_aspect: c = [x for x in c if max(x["dims"][:2]) / max(1e-6, min(x["dims"][:2])) <= max_aspect]
    return random.choice(c) if c else None
def load_insert(entry):
    with bpy.data.libraries.load(entry["blend"]) as (src, dst): dst.objects = [n for n in src.objects]
    objs = [o for o in dst.objects if o and o.type == 'MESH']
    for o in objs: sc.collection.objects.link(o)
    return objs[0]
# --- blockout from the brief: 9 m long (+Y nose), 6 m span, 2.5 m tall
def box(name, loc, dim):
    bpy.ops.mesh.primitive_cube_add(location=loc); o = bpy.context.object; o.name = name; o.scale = (dim[0]/2, dim[1]/2, dim[2]/2); bpy.ops.object.transform_apply(scale=True); return o
hull = box("hull", (0, -0.5, 1.2), (2.2, 7.0, 2.0)); nose = box("nose", (0, 3.6, 1.0), (1.6, 2.0, 1.4))
wing = box("wing_r", (2.0, -1.5, 1.1), (2.0, 3.0, 0.25)); fin = box("fin", (0, -3.2, 2.9), (0.15, 1.6, 1.4))
for o in (hull, nose, wing, fin):
    m = o.modifiers.new("bev", 'BEVEL'); m.width = 0.06; m.segments = 2; bpy.context.view_layer.objects.active = o; bpy.ops.object.modifier_apply(modifier="bev")
# --- candidate faces: large, outward, on the +x half (we mirror later) or on the centreline top/bottom
graph = []
def place_on_faces(target, want, count, scale_fit=0.8, min_area=0.15, side="x+"):
    bm = bmesh.new(); bm.from_mesh(target.data); bm.faces.ensure_lookup_table()
    faces = [f for f in bm.faces if f.calc_area() >= min_area]
    if side == "x+": faces = [f for f in faces if (target.matrix_world @ f.calc_center_median()).x > 0.05 or abs(f.normal.x) < 0.5]
    random.shuffle(faces); placed = 0
    for f in faces:
        if placed >= count: break
        e = pick(want); 
        if not e: break
        o = load_insert(e)
        # local frame on the face: normal -> insert +Z (insert mount plane faces -Z, sits on the face), tangent along the longest edge
        n = (target.matrix_world.to_3x3() @ f.normal).normalized(); c = target.matrix_world @ f.calc_center_median()
        ed = max(f.edges, key=lambda x: x.calc_length()); t = (target.matrix_world.to_3x3() @ (ed.verts[1].co - ed.verts[0].co)).normalized()
        t = (t - n * t.dot(n)).normalized(); b = n.cross(t)
        R = Matrix((t, b, n)).transposed().to_4x4()
        # scale so the insert's footprint fits the face's shorter extent
        fw = min(ed.calc_length(), math.sqrt(f.calc_area())) * scale_fit; s = fw / max(1e-6, max(e["dims"][:2]))
        o.matrix_world = Matrix.Translation(c) @ R @ Matrix.Scale(s, 4)
        o.name = f"ins_{e['name']}"; placed += 1
        graph.append({"insert": e["name"], "kit": e["kit"], "tags": e["tags"], "target": target.name, "face_center": [round(v, 3) for v in c], "normal": [round(v, 3) for v in n], "scale": round(s, 4)})
    bm.free(); return placed
n1 = place_on_faces(hull, ["conduit", "pipe", "cable"], 6, 0.7)
n2 = place_on_faces(hull, ["grille", "vent", "strip", "plate", "hull-panel"], 6, 0.6)
n3 = place_on_faces(wing, ["strip", "rib", "conduit"], 3, 0.6)
n4 = place_on_faces(nose, ["grille", "vent", "greeble-cluster", "block"], 3, 0.5)
print("placed", n1, n2, n3, n4)
# --- mirror everything across x (the measured symmetry plane), then join into one hull object
parts = [o for o in bpy.data.objects if o.type == 'MESH' and o.name != "thumbcam"]
for o in parts:
    if o.name in ("hull", "nose", "fin"): continue
    m = o.modifiers.new("mir", 'MIRROR'); m.use_axis = (True, False, False); m.mirror_object = hull
for o in parts: o.select_set(True)
bpy.context.view_layer.objects.active = hull
for o in parts:
    bpy.context.view_layer.objects.active = o
    for m in list(o.modifiers): bpy.ops.object.modifier_apply(modifier=m.name)
bpy.context.view_layer.objects.active = hull; bpy.ops.object.join(); hull.name = "escort_kitbash"
tris = sum(len(p.vertices) - 2 for p in hull.data.polygons)
json.dump({"seed": seed, "blockout": "escort brief 9x6x2.5 m", "placements": graph, "tris": tris}, open(os.path.join(out, "assembly.json"), "w"), indent=1)
bpy.ops.wm.save_as_mainfile(filepath=os.path.join(out, "escort_kitbash.blend"), compress=True)
bpy.ops.export_scene.gltf(filepath=os.path.join(out, "escort_kitbash.glb"), export_format='GLB')
print("ASSEMBLED", len(graph), "placements", tris, "tris")
