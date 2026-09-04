# blender -b --python assemble_zones.py -- <masterfolder> <zones.json> <out_dir> [seed] [tri_budget_per_insert]
# Zone-driven kitbash: blockout from zones.json, then for each zone pick INSERTs by tag, lay them out along the zone's
# axis on the named face, scale to the zone's range, mirror across the symmetry plane, join, record the assembly graph.
import bpy, bmesh, sys, os, json, math, random, glob
from mathutils import Vector, Matrix
a = sys.argv[sys.argv.index("--")+1:]; master, zpath, out = a[0], a[1], os.path.abspath(a[2]); seed = int(a[3]) if len(a) > 3 else 7; budget = int(a[4]) if len(a) > 4 else 6000
random.seed(seed); os.makedirs(out, exist_ok=True); Z = json.load(open(zpath))
bpy.ops.wm.read_homefile(use_empty=True); sc = bpy.context.scene
lib = []
for kp in sorted(glob.glob(os.path.join(master, "*"))):
    fp, lp = os.path.join(kp, "features.json"), os.path.join(kp, "labels.json")
    if not os.path.exists(fp): continue
    feats = json.load(open(fp)); labels = json.load(open(lp)) if os.path.exists(lp) else {}
    for n, f in feats.items():
        L = labels.get(n, {}); lib.append({"name": n, "blend": os.path.join(kp, n + ".blend"), "tags": L.get("tags", [L.get("heuristic", "misc")]), "conf": L.get("confidence", 0.3),
                                           "dims": f["dims"], "tris": f["tris"], "kit": f["kit"], "planar": f["planar_fraction"], "below": f.get("below_plane", 0.0), "attach": f.get("attach", ["surface"]), "sockets": f.get("sockets")})
print("library", len(lib), "inserts,", sum(1 for x in lib if x["conf"] >= 0.6), "with confident labels")
FLAT = {"hull-panel", "plate", "hatch", "grille", "vent", "strip", "rib"}
def pick(tags, used):
    flat = any(t in FLAT for t in tags)
    c = [x for x in lib if any(t in x["tags"] for t in tags) and x["conf"] >= 0.6 and x["name"] not in used and (not flat or (x["planar"] >= 0.25 and x["below"] <= 0.15 * max(x["dims"])))]
    if not c: c = [x for x in lib if any(t in x["tags"] for t in tags) and x["name"] not in used]
    if not c: return None
    c.sort(key=lambda x: -x["conf"]); return random.choice(c[:max(3, len(c)//3)])
def load_insert(e):
    with bpy.data.libraries.load(e["blend"]) as (src, dst): dst.objects = list(src.objects)
    o = [x for x in dst.objects if x and x.type == 'MESH'][0]; sc.collection.objects.link(o)
    for x in dst.objects:
        if x and x.type == 'EMPTY': bpy.data.objects.remove(x, do_unlink=True)
    t = sum(len(p.vertices) - 2 for p in o.data.polygons)
    if t > budget:
        m = o.modifiers.new("dec", 'DECIMATE'); m.ratio = budget / t; bpy.context.view_layer.objects.active = o; bpy.ops.object.modifier_apply(modifier="dec")
    return o
def box(name, dim, loc):
    bpy.ops.mesh.primitive_cube_add(location=loc); o = bpy.context.object; o.name = name; o.scale = (dim[0]/2, dim[1]/2, dim[2]/2); bpy.ops.object.transform_apply(scale=True); return o
def cyl(name, r, depth, loc):
    bpy.ops.mesh.primitive_cylinder_add(location=loc, radius=r, depth=depth, rotation=(math.pi/2, 0, 0), vertices=24); o = bpy.context.object; o.name = name; return o
blk = {}
for name, spec in Z["blockout"].items():
    blk[name] = cyl(name, *spec["cyl"]) if isinstance(spec, dict) else box(name, spec[:3], spec[3])
    if not isinstance(spec, dict):
        m = blk[name].modifiers.new("bev", 'BEVEL'); m.width = 0.05; m.segments = 2; bpy.context.view_layer.objects.active = blk[name]; bpy.ops.object.modifier_apply(modifier="bev")
FACE = {"top": Vector((0, 0, 1)), "bottom": Vector((0, 0, -1)), "side": Vector((1, 0, 0)), "front": Vector((0, 1, 0)), "back": Vector((0, -1, 0))}
AX = {"x": Vector((1, 0, 0)), "y": Vector((0, 1, 0)), "z": Vector((0, 0, 1))}
graph = []; used = set()
def pick_sockets(tags, used):
    c = [x for x in lib if "sockets" in x.get("attach", []) and any(t in x["tags"] for t in tags) and x["name"] not in used]
    return random.choice(c) if c else None
for z in Z["zones"]:
    if z.get("type") == "connect":
        # socket-to-socket: place socket_a at `from`, aim socket_b at `to`, stretch along the cable axis to span the distance
        e = pick_sockets(z["tags"], used)
        if not e: print("no socketed insert for zone", z["name"]); continue
        used.add(e["name"]); o = load_insert(e)
        A, B = Vector(z["from"]), Vector(z["to"]); sa, sb = Vector(e["sockets"][0]), Vector(e["sockets"][1])
        ax = sb - sa; L = ax.length; ax.normalize(); tgt_dir = (B - A); D = tgt_dir.length; tgt_dir.normalize()
        up = Vector(z.get("up", [0, 0, 1])); s_cross = random.uniform(*z["scale"]); s_along = D / max(1e-6, L)
        # frame: local cable axis -> tgt_dir, local plane normal (+Z) -> up
        lz = Vector((0, 0, 1)); ly = lz.cross(ax).normalized(); R_local = Matrix((ax, ly, lz)).transposed().inverted()  # orthonormal local basis: cable axis, side, mount normal
        if abs(up.dot(tgt_dir)) > 0.95: up = Vector((0, 0, 1)) if abs(tgt_dir.z) < 0.9 else Vector((1, 0, 0))
        bx = tgt_dir; bz = (up - bx * up.dot(bx)).normalized(); by = bz.cross(bx); R_world = Matrix((bx, by, bz)).transposed()
        S = Matrix.Diagonal((s_along, s_cross, s_cross, 1.0))
        M = Matrix.Translation(A) @ R_world.to_4x4() @ S @ R_local.to_4x4() @ Matrix.Translation(-sa)
        o.matrix_world = M; o.name = f"{z['name']}_{e['name']}"
        graph.append({"zone": z["name"], "insert": e["name"], "kit": e["kit"], "tags": e["tags"], "from": z["from"], "to": z["to"], "scale_along": round(s_along, 3), "scale_cross": round(s_cross, 3), "attach": "sockets"})
        continue
    tgt = blk[z["target"]]; n = FACE[z["face"]]; along = AX[z["along"]]
    bb = [tgt.matrix_world @ Vector(c) for c in tgt.bound_box]; mn = Vector(map(min, *bb)); mx = Vector(map(max, *bb)); ctr = (mn + mx) / 2; ext = mx - mn
    # face centre on the blockout surface; for "side" use the +x face (mirrored later)
    fc = ctr + Vector(n[i] * ext[i] / 2 for i in range(3))
    span = along.dot(ext) * 0.8; k = z["count"]
    t = along; b = n.cross(t)
    for i in range(k):
        e = pick(z["tags"], used)
        if not e: print("no insert for zone", z["name"], z["tags"]); break
        used.add(e["name"]); o = load_insert(e)
        pos = fc + t * ((i - (k - 1) / 2) * (span / max(1, k))) + (Vector((z.get("offset", [0, 0])[0], z.get("offset", [0, 0])[1], 0)) if "offset" in z else Vector((0, 0, 0)))
        # footprint fits the zone's cross extent times the scale range
        cross = abs(b.dot(ext)) if abs(b.dot(ext)) > 1e-6 else span; slot = span / max(1, k)
        # fit the insert's footprint (x along the zone axis, y across) into the slot on both axes, then apply the zone's scale range
        s_fit = random.uniform(*z["scale"]) * min(slot / max(1e-6, e["dims"][0]), cross / max(1e-6, e["dims"][1]))
        # orient: insert +Z along face normal, insert +X along the zone axis
        R = Matrix((t, b, n)).transposed().to_4x4()
        pos = pos + n * (e["below"] * s_fit)  # lift the part so its mount plane, not its deepest point, touches the face
        o.matrix_world = Matrix.Translation(pos) @ R @ Matrix.Scale(s_fit, 4); o.name = f"{z['name']}_{i}_{e['name']}"
        graph.append({"zone": z["name"], "insert": e["name"], "kit": e["kit"], "tags": e["tags"], "pos": [round(v, 3) for v in pos], "normal": list(n), "scale": round(s_fit, 4)})
print("placed", len(graph))
# role materials so the render reads: blockout hull grey, kit parts by source, cables safety orange (brief palette)
def mat(name, rgb):
    m = bpy.data.materials.get(name) or bpy.data.materials.new(name); m.use_nodes = True
    b = m.node_tree.nodes.get("Principled BSDF"); b.inputs["Base Color"].default_value = (*rgb, 1); b.inputs["Roughness"].default_value = 0.55; return m
M = {"hull": mat("hull_grey", (0.54, 0.56, 0.58)), "cables": mat("safety_orange", (0.85, 0.47, 0.17)), "ship-a": mat("dark_panel", (0.17, 0.18, 0.20)), "ship-b": mat("warm_panel", (0.40, 0.41, 0.42)), "mech": mat("canopy_blue", (0.11, 0.25, 0.35))}
for o in [o for o in bpy.data.objects if o.type == 'MESH']:
    key = "hull" if o.name in blk else next((k for k in ("cables", "ship-a", "ship-b", "mech") if f"_{k}_" in o.name), "hull")
    o.data.materials.clear(); o.data.materials.append(M[key])
    for pgon in o.data.polygons: pgon.material_index = 0
print("materials", {o.name[:24]: o.data.materials[0].name for o in bpy.data.objects if o.type == 'MESH'})
mir = Z.get("mirror", "x"); parts = [o for o in bpy.data.objects if o.type == 'MESH']
for o in parts:
    bbx = [(o.matrix_world @ Vector(c)).x for c in o.bound_box]
    if min(bbx) > -0.02 or (o.name in blk and not o.name.startswith(("wing", "thruster", "rcs")) and False):
        m = o.modifiers.new("mir", 'MIRROR'); m.use_axis = (mir == "x", mir == "y", False); m.mirror_object = blk["hull"]
for o in parts:
    bpy.context.view_layer.objects.active = o
    for m in list(o.modifiers): bpy.ops.object.modifier_apply(modifier=m.name)
for o in parts: o.select_set(True)
bpy.context.view_layer.objects.active = blk["hull"]; bpy.ops.object.join(); hull = bpy.context.object; hull.name = Z["asset"].replace(" ", "_")
tris = sum(len(p.vertices) - 2 for p in hull.data.polygons)
json.dump({"seed": seed, "zones": zpath, "placements": graph, "tris": tris}, open(os.path.join(out, "assembly.json"), "w"), indent=1)
bpy.ops.wm.save_as_mainfile(filepath=os.path.join(out, "assembly.blend"), compress=True)
bpy.ops.export_scene.gltf(filepath=os.path.join(out, "assembly.glb"), export_format='GLB')
print("ASSEMBLED", len(graph), "placements", tris, "tris")
