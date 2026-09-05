# blender -b --python assemble_hull.py -- <masterfolder> <hull.blend> <zones.json> <choices.json|-> <out_dir> [seed] [budget]
# Assemble onto the coarse hull: resolve each zone's semantic region to hull faces (region attributes), place chosen INSERTs
# along the region's principal axis with surface alignment, connect runs between region centroids, mirror across x, join.
import bpy, bmesh, sys, os, json, math, random, glob
from mathutils import Vector, Matrix
a = sys.argv[sys.argv.index("--")+1:]; master, hullf, zpath, cpath, out = a[0], a[1], a[2], a[3], os.path.abspath(a[4]); seed = int(a[5]) if len(a) > 5 else 7; budget = int(a[6]) if len(a) > 6 else 6000
random.seed(seed); os.makedirs(out, exist_ok=True); Z = json.load(open(zpath)); C = json.load(open(cpath)) if cpath != "-" and os.path.exists(cpath) else {}
bpy.ops.wm.open_mainfile(filepath=os.path.abspath(hullf), load_ui=False); sc = bpy.context.scene; hull = bpy.data.objects["hull"]
SIDES = ["top", "belly", "flank", "nose", "tail"]; LONGS = ["fore", "mid", "aft"]; LATS = ["inner", "outer"]
lib = {}
for kp in sorted(glob.glob(os.path.join(master, "*"))):
    fp, lp = os.path.join(kp, "features.json"), os.path.join(kp, "labels.json")
    if not os.path.exists(fp): continue
    F = json.load(open(fp)); L = json.load(open(lp)) if os.path.exists(lp) else {}
    for n, f in F.items():
        l = L.get(n, {}); lib[n] = {"name": n, "blend": os.path.join(kp, n + ".blend"), "tags": l.get("tags", []), "conf": l.get("confidence", 0), "dims": f["dims"], "below": f.get("below_plane", 0), "planar": f["planar_fraction"], "attach": f.get("attach", ["surface"]), "sockets": f.get("sockets"), "kit": f["kit"]}
def region_faces(name):
    sd, lg, lt = name.split("."); si, li, ti = SIDES.index(sd), LONGS.index(lg), LATS.index(lt)
    S, Lg, Lt = hull.data.attributes["region_side"].data, hull.data.attributes["region_long"].data, hull.data.attributes["region_lat"].data
    fs = [f for f in hull.data.polygons if S[f.index].value == si and Lg[f.index].value == li and Lt[f.index].value == ti]
    if sd == "flank": fs = [f for f in fs if f.center.x > 0]  # +x half only; mirrored later
    return fs
HX, HY, HZ = [(max(v.co[i] for v in hull.data.vertices) - min(v.co[i] for v in hull.data.vertices)) / 2 for i in range(3)]
EXTREME = {"top": (2, 1), "belly": (2, -1), "flank": (0, 1), "nose": (1, 1), "tail": (1, -1)}   # axis, sign: the outer skin of each side is the extreme along that axis
def extreme_filter(fs, side, band=0.35):
    # keep faces within `band` metres of the side's outermost skin; drops overhangs (a downward-facing canopy lip is labelled belly but sits at z=+1.2)
    if not fs: return fs
    ax, sg = EXTREME[side]; ref = max(f.center[ax] * sg for f in fs); keep = [f for f in fs if ref - f.center[ax] * sg <= band]
    return keep or fs
def anchor_faces(region, anchor, k=40):
    # anchor placement: [x, y] fractions of the hull half-extents (-1..1; +x starboard, +y nose) on the region's side; flank anchors are [y, z].
    sd = region.split(".")[0]; si = SIDES.index(sd); S = hull.data.attributes["region_side"].data
    fs = [f for f in hull.data.polygons if S[f.index].value == si]
    if sd == "flank": fs = [f for f in fs if f.center.x > 0]; tgt = Vector((0, anchor[0] * HY, anchor[1] * HZ)); key = lambda f: (f.center.y - tgt.y) ** 2 + (f.center.z - tgt.z) ** 2
    else: tgt = Vector((anchor[0] * HX, anchor[1] * HY, 0)); key = lambda f: (f.center.x - tgt.x) ** 2 + (f.center.y - tgt.y) ** 2
    fs.sort(key=key); return extreme_filter(fs[:k * 3], sd)[:k]
def region_frame(fs):
    A = sum(f.area for f in fs) or 1e-9; c = sum((f.center * f.area for f in fs), Vector((0, 0, 0))) / A; n = sum((f.normal * f.area for f in fs), Vector((0, 0, 0))).normalized()
    # principal axis in the region plane: prefer y (length) unless the region is wider than long
    pts = [f.center for f in fs]; ext = [max(p[i] for p in pts) - min(p[i] for p in pts) for i in range(3)]
    axis = Vector((0, 1, 0)) if ext[1] >= ext[0] else Vector((1, 0, 0)); t = (axis - n * axis.dot(n)).normalized(); b = n.cross(t)
    return c, n, t, b, ext
def surface_at(c, n, t, along_t, fs, across=None):
    # walk the region: the face of THIS region nearest to c + t*along_t (+ across). Searching all hull faces put belly parts on the top skin,
    # because a curved region's centroid lies inside the hull, nearer the opposite skin.
    tgt = c + t * along_t + (across or Vector((0, 0, 0))); f = min(fs, key=lambda f: (f.center - tgt).length_squared); return f.center, f.normal
def region_faces_skin(name): return extreme_filter(region_faces(name), name.split(".")[0])
def load_insert(e):
    with bpy.data.libraries.load(e["blend"]) as (src, dst): dst.objects = list(src.objects)
    o = [x for x in dst.objects if x and x.type == 'MESH'][0]; sc.collection.objects.link(o)
    for x in dst.objects:
        if x and x.type == 'EMPTY': bpy.data.objects.remove(x, do_unlink=True)
    tr = sum(len(p.vertices) - 2 for p in o.data.polygons)
    if tr > budget:
        m = o.modifiers.new("dec", 'DECIMATE'); m.ratio = budget / tr; bpy.context.view_layer.objects.active = o; bpy.ops.object.modifier_apply(modifier="dec")
    return o
def pick(z, i, used):
    ranked = C.get(z["name"], {}).get("ranked", []); excl = set(z.get("exclude", []))  # critic "replace" verdicts exclude the rejected insert
    for nm in ranked:
        if nm in lib and nm not in used and nm not in excl: return lib[nm]
    c = [x for x in lib.values() if any(t in x["tags"] for t in z["tags"]) and x["name"] not in used and x["name"] not in excl and (z["type"] != "connect" or "sockets" in x["attach"])]
    return random.choice(c) if c else None
graph = []; used = set()
for z in Z["zones"]:
    if z.get("kind") == "paint": graph.append({"zone": z["name"], "kind": "paint", "region": z["region"], "note": "handed to texture stage"}); continue
    fs = anchor_faces(z["region"], z["anchor"]) if z.get("anchor") else region_faces_skin(z["region"])
    if not fs: print("empty region", z["name"], z["region"]); continue
    c, n, t, b, ext = region_frame(fs)
    if z["type"] == "connect":
        fs2 = region_faces(z.get("region_to", z["region"]));
        if not fs2: continue
        c2 = region_frame(fs2)[0]; e = pick(z, 0, used)
        if not e or not e.get("sockets"): print("no socketed part for", z["name"]); continue
        used.add(e["name"]); o = load_insert(e); sa, sb = Vector(e["sockets"][0]), Vector(e["sockets"][1]); ax = (sb - sa); L = ax.length; ax.normalize()
        A, B = c + n * 0.05, c2 + region_frame(fs2)[1] * 0.05; d = B - A; D = d.length; d.normalize(); up = (n + region_frame(fs2)[1]).normalized()
        if abs(up.dot(d)) > 0.95: up = Vector((0, 0, 1))
        lz = Vector((0, 0, 1)); ly = lz.cross(ax).normalized(); R_local = Matrix((ax, ly, lz)).transposed().inverted()
        bz = (up - d * up.dot(d)).normalized(); by = bz.cross(d); R_world = Matrix((d, by, bz)).transposed()
        s_cross = z.get("thickness_m", 0.15) / max(1e-6, min(e["dims"][:2]))  # connect zones: size_m is the run length; thickness_m sets the cable cross-section
        S = Matrix.Diagonal((D / max(1e-6, L), s_cross, s_cross, 1.0))
        o.matrix_world = Matrix.Translation(A) @ R_world.to_4x4() @ S @ R_local.to_4x4() @ Matrix.Translation(-sa); o.name = f"{z['name']}_{e['name']}"
        graph.append({"zone": z["name"], "insert": e["name"], "kit": e["kit"], "from": z["region"], "to": z.get("region_to"), "attach": "sockets"}); continue
    k = max(1, z["count"]); span = (ext[1] if t.y != 0 else ext[0]) * 0.85; cross = (ext[0] if t.y != 0 else ext[1]) or span
    if z.get("anchor"): span = min(span, max(0.5, z.get("size_m", 1.0) * k))
    if z["region"].startswith("flank"): cross = ext[2] or cross
    # even counts on a region that straddles the centreline: place half at +x and let the mirror supply the pair, instead of two copies at x≈0
    straddles = not z["region"].startswith("flank") and min(f.center.x for f in fs) < -0.05 and max(f.center.x for f in fs) > 0.05
    pair = straddles and k % 2 == 0 and not z.get("anchor"); across = Vector((ext[0] * 0.3, 0, 0)) if pair else None
    if z.get("anchor") and k % 2 == 0 and not z["region"].startswith("flank"): pair = True; across = None   # anchored pairs: the anchor is the +x member, the mirror makes the other
    if pair: k //= 2
    for i in range(k):
        e = pick(z, i, used)
        if not e: print("no part for", z["name"]); break
        used.add(e["name"]); o = load_insert(e)
        pos, nn = surface_at(c, n, t, (i - (k - 1) / 2) * (span / k), fs, across); tt = (t - nn * t.dot(nn)).normalized(); bb = nn.cross(tt)
        slot = span / k
        if z.get("size_m"): s_fit = z["size_m"] / max(1e-6, max(e["dims"]))  # absolute size from the program
        else: s_fit = random.uniform(*z["scale"]) * min(slot / max(1e-6, e["dims"][0]), cross / max(1e-6, e["dims"][1]))
        cap = z.get("max_size_m", 4.0) / max(1e-6, max(e["dims"])); s_fit = min(s_fit, cap)
        pos = pos + nn * (e["below"] * s_fit + 0.01)
        yaw = Matrix.Rotation(math.radians(z.get("yaw_deg", 0)), 4, 'Z')  # critic "rotate": spin about the surface normal
        o.matrix_world = Matrix.Translation(pos) @ Matrix((tt, bb, nn)).transposed().to_4x4() @ yaw @ Matrix.Scale(s_fit, 4); o.name = f"{z['name']}_{i}_{e['name']}"
        placed_m = max(e["dims"]) * s_fit; tb = int(max(250, min(budget, 1800 * placed_m ** 1.5)))  # triangle budget grows with placed size
        tr = sum(len(pg.vertices) - 2 for pg in o.data.polygons)
        if tr > tb:
            m = o.modifiers.new("lod", 'DECIMATE'); m.ratio = tb / tr; bpy.context.view_layer.objects.active = o; bpy.ops.object.modifier_apply(modifier="lod")
        graph.append({"zone": z["name"], "insert": e["name"], "kit": e["kit"], "region": z["region"], "pos": [round(v, 3) for v in pos], "anchor": z.get("anchor"), "scale": round(s_fit, 4), "placed_m": round(max(e["dims"]) * s_fit, 2), "attach": "surface"})
print("placed", len(graph))
def mat(name, rgb, rough=0.55, metal=0.0, emit=None, alpha=1.0):
    m = bpy.data.materials.get(name) or bpy.data.materials.new(name); m.use_nodes = True; bs = m.node_tree.nodes.get("Principled BSDF")
    bs.inputs["Base Color"].default_value = (*rgb, 1); bs.inputs["Roughness"].default_value = rough; bs.inputs["Metallic"].default_value = metal
    if emit: bs.inputs["Emission Color"].default_value = (*emit, 1); bs.inputs["Emission Strength"].default_value = 0.8
    if alpha < 1: bs.inputs["Alpha"].default_value = alpha; m.blend_method = 'BLEND'
    return m
# brief palette: hull #8a8f94 warm grey painted steel, dark panels #2b2e33, safety orange #d9772b, emissive #7fd4ff, canopy #1c3f5a
M = {"hull": mat("hull_paint", (0.25, 0.27, 0.29), 0.6), "dark": mat("dark_panel", (0.026, 0.028, 0.035), 0.5), "accent": mat("safety_orange", (0.70, 0.19, 0.025), 0.5),
     "polymer": mat("rubber_polymer", (0.02, 0.02, 0.022), 0.85), "aluminium": mat("bare_aluminium", (0.6, 0.6, 0.6), 0.35, 1.0), "emissive": mat("thruster_glow", (0.03, 0.03, 0.035), 0.45, 0.0, (0.22, 0.65, 1.0)),
     "glass": mat("canopy_tint", (0.012, 0.05, 0.1), 0.05, 0.0, None, 0.6)}
ROLE = {"nozzle": "polymer", "thruster": "dark", "cable": "polymer", "conduit": "polymer", "pipe": "polymer", "hull-panel": "dark", "plate": "dark", "hatch": "dark", "vent": "dark", "grille": "dark",
        "landing-gear": "polymer", "strut": "aluminium", "bracket": "aluminium", "pylon": "dark", "gun": "dark", "launcher": "dark", "turret": "dark", "window": "glass"}
zone_role = {z["name"]: z.get("material") for z in Z["zones"]}; zone_tags = {z["name"]: z["tags"] for z in Z["zones"]}
def role_for(o):
    if o.name == "hull": return "hull"
    zn = next((z for z in zone_role if o.name.startswith(z + "_")), None)
    if zn and zone_role[zn] in M: return zone_role[zn]
    if zn and "stripe" in zn: return "accent"
    if zn and "main_thruster" in zn: return "emissive"
    if zn and ("rcs" in zn or "nozzle" in zn): return "polymer"  # RCS blocks are dark mechanical, never a glowing part
    for t in (zone_tags.get(zn) or []):
        if t in ROLE: return ROLE[t]
    return "dark"
# hull-number decal: extruded text on each flank, fore, reading correctly on both sides (no mirror)
def add_decal(text, x_sign):
    fs = region_faces("flank.fore.outer"); c, n, t, b, ext = region_frame(fs); c = Vector((c.x * x_sign, c.y, c.z)); n = Vector((n.x * x_sign, n.y, n.z))
    cu = bpy.data.curves.new("hullno", 'FONT'); cu.body = text; cu.extrude = 0.01; cu.size = 0.45; cu.align_x = 'CENTER'; cu.align_y = 'CENTER'
    o = bpy.data.objects.new(f"decal_{text}_{'R' if x_sign > 0 else 'L'}", cu); sc.collection.objects.link(o)
    fwd = Vector((0, 1, 0)); up = Vector((0, 0, 1)); right = fwd if x_sign > 0 else -fwd   # text runs nose-ward on both sides
    o.matrix_world = Matrix.Translation(c + n * 0.02) @ Matrix((right, up, n)).transposed().to_4x4()
    bpy.context.view_layer.objects.active = o; o.select_set(True); bpy.ops.object.convert(target='MESH'); o.data.materials.clear(); o.data.materials.append(M["aluminium"]); return o
decals = [add_decal(Z.get("hull_number", "E-07"), 1), add_decal(Z.get("hull_number", "E-07"), -1)]
parts = [o for o in bpy.data.objects if o.type == 'MESH']
for o in parts:
    if o.name.startswith("decal_"): continue
    o.data.materials.clear(); o.data.materials.append(M[role_for(o)])
    for pg in o.data.polygons: pg.material_index = 0
    if o.name != "hull":
        xs = [(o.matrix_world @ Vector(cn)).x for cn in o.bound_box]
        m = o.modifiers.new("mir", 'MIRROR'); m.use_axis = (True, False, False); m.mirror_object = hull; m.use_mirror_merge = True; m.merge_threshold = 0.002
        if min(xs) < -0.05 and max(xs) > 0.05: m.use_bisect_axis = (True, False, False); m.use_bisect_flip_axis = (min(xs) + max(xs) < 0, False, False)  # straddles: keep one half, mirror it
for o in parts:
    bpy.context.view_layer.objects.active = o
    for m in list(o.modifiers): bpy.ops.object.modifier_apply(modifier=m.name)
# zone-id pass: every zone a flat named colour, hull dark grey, so the critic can tell which blob is which zone
ID_COLORS = [("red", (1, 0, 0)), ("green", (0, 1, 0)), ("blue", (0.1, 0.3, 1)), ("yellow", (1, 1, 0)), ("magenta", (1, 0, 1)), ("cyan", (0, 1, 1)), ("orange", (1, 0.45, 0)), ("purple", (0.5, 0, 1)),
             ("lime", (0.6, 1, 0.2)), ("pink", (1, 0.55, 0.75)), ("teal", (0, 0.55, 0.5)), ("brown", (0.5, 0.25, 0.05)), ("white", (1, 1, 1)), ("olive", (0.5, 0.5, 0)), ("navy", (0, 0, 0.5)), ("gold", (0.9, 0.7, 0.1))]
def idmat(name, rgb):
    m = bpy.data.materials.new("id_" + name); m.use_nodes = True; bs = m.node_tree.nodes.get("Principled BSDF"); bs.inputs["Base Color"].default_value = (0, 0, 0, 1)
    bs.inputs["Emission Color"].default_value = (*rgb, 1); bs.inputs["Emission Strength"].default_value = 1.0; return m
legend = {}; saved = {}
for o in parts:
    saved[o.name] = list(o.data.materials)
    zn = next((z["name"] for z in Z["zones"] if o.name.startswith(z["name"] + "_")), None)
    if zn and zn not in legend: legend[zn] = ID_COLORS[len(legend) % len(ID_COLORS)][0]
    col = dict(ID_COLORS)[legend[zn]] if zn else (0.12, 0.12, 0.12)
    o.data.materials.clear(); o.data.materials.append(idmat(o.name, col))
bpy.ops.export_scene.gltf(filepath=os.path.join(out, "assembly_id.glb"), export_format='GLB')
for o in parts:
    o.data.materials.clear()
    for m in saved[o.name]: o.data.materials.append(m)
for o in parts: o.select_set(True)
bpy.context.view_layer.objects.active = hull; bpy.ops.object.join(); hull = bpy.context.object; hull.name = Z.get("asset", "asset").replace(" ", "_")
tris = sum(len(p.vertices) - 2 for p in hull.data.polygons)
json.dump({"seed": seed, "zones": zpath, "choices": cpath, "placements": graph, "tris": tris, "id_colors": legend}, open(os.path.join(out, "assembly.json"), "w"), indent=1)
bpy.ops.wm.save_as_mainfile(filepath=os.path.join(out, "assembly.blend"), compress=True); bpy.ops.export_scene.gltf(filepath=os.path.join(out, "assembly.glb"), export_format='GLB')
print("ASSEMBLED", len(graph), "placements", tris, "tris")
