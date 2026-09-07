# blender -b --python assemble_hull.py -- <masterfolder> <hull.blend> <zones.json> <choices.json|-> <out_dir> [seed] [budget]
# Assemble onto the coarse hull: resolve each zone's semantic region to hull faces (region attributes), place chosen INSERTs
# along the region's principal axis with surface alignment, connect runs between region centroids, mirror across x, join.
import bpy, bmesh, sys, os, json, math, random, glob
from mathutils import Vector, Matrix
a = sys.argv[sys.argv.index("--")+1:]; master, hullf, zpath, cpath, out = a[0], a[1], a[2], a[3], os.path.abspath(a[4]); seed = int(a[5]) if len(a) > 5 else 7; budget = int(a[6]) if len(a) > 6 else 6000
random.seed(seed); os.makedirs(out, exist_ok=True); Z = json.load(open(zpath)); C = json.load(open(cpath)) if cpath != "-" and os.path.exists(cpath) else {}
bpy.ops.wm.open_mainfile(filepath=os.path.abspath(hullf), load_ui=False); sc = bpy.context.scene; hull = bpy.data.objects["hull"]
# the voxel-remeshed hull has slits and pinholes that render as dark vents; close them (bounded so a real opening like an intake stays)
_bm = bmesh.new(); _bm.from_mesh(hull.data); _open = [e for e in _bm.edges if e.is_boundary]
if _open: bmesh.ops.holes_fill(_bm, edges=_open, sides=24)
_bm.to_mesh(hull.data); _bm.free(); hull.data.update()
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
def skin_hit(side, q):
    """Exact skin point on `side` at plan position q (x, y for top/belly/nose/tail; y, z for flank): ray cast from outside along the side's axis."""
    inv = hull.matrix_world.inverted(); ax, sg = EXTREME[side]; o_far = Vector(q); o_far[ax] = 30.0 * sg
    dirv = Vector([(-sg if i == ax else 0) for i in range(3)]); hit, loc, nrm, _ = hull.ray_cast(inv @ o_far, (inv.to_3x3() @ dirv).normalized())
    return (hull.matrix_world @ loc, (hull.matrix_world.to_3x3() @ nrm).normalized()) if hit else (None, None)
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
def prim_object(z, e_dims_out):
    """Procedural stand-ins for shapes the library lacks. Built in the INSERT frame: mount plane z=0, length along +y, returns (object, dims)."""
    kind = z["prim"]; L = z.get("size_m", 1.0); me = bpy.data.meshes.new(f"prim_{z['name']}"); bm = bmesh.new()
    if kind == "skid":       # ski: struts stand off the hull (+z is away from the skin), a flat bar at their end, tip curled back toward the hull
        w, th, h = 0.14 * L, 0.05 * L, z.get("drop_m", 0.30 * L)
        bmesh.ops.create_cube(bm, size=1.0); bmesh.ops.scale(bm, vec=(w, L, th), verts=bm.verts); bmesh.ops.translate(bm, vec=(0, 0, h + th / 2), verts=bm.verts)
        tip = [v for v in bm.verts if v.co.y > L * 0.49]; bmesh.ops.translate(bm, vec=(0, 0, -0.12 * L), verts=tip)
        for yy in (-0.3 * L, 0.25 * L):
            r = bmesh.ops.create_cube(bm, size=1.0); bmesh.ops.scale(bm, vec=(0.5 * w, 0.5 * w, h), verts=r["verts"]); bmesh.ops.translate(bm, vec=(0, yy, h / 2), verts=r["verts"])
        dims = (w, L, h + th)
    elif kind == "nozzle":   # engine: outer cylinder with a recessed inner cone, axis along +z = the mount normal (a tail face points aft).
        r = L / 2; ln = z.get("length_m", 1.0 * L); sink = z.get("sink", 0.85)   # most of the length sits inside the pod lump; only the lip and the glowing cone show
        bmesh.ops.create_cone(bm, cap_ends=False, segments=24, radius1=r * 0.92, radius2=r, depth=ln)
        inner = bmesh.ops.create_cone(bm, cap_ends=True, segments=24, radius1=r * 0.35, radius2=r * 0.82, depth=ln * 0.9)
        ring = bmesh.ops.create_cone(bm, cap_ends=False, segments=24, radius1=r * 0.92, radius2=r * 0.82, depth=0.02); bmesh.ops.translate(bm, vec=(0, 0, ln * 0.49), verts=ring["verts"])
        inner_faces = {f for v in inner["verts"] for f in v.link_faces}
        for f in bm.faces: f.material_index = 1 if f in inner_faces else 0
        bmesh.ops.translate(bm, vec=(0, 0, ln / 2 - sink * ln), verts=bm.verts); dims = (2 * r, 2 * r, ln * (1 - sink))
    elif kind == "box":
        w, h = z.get("width_m", 0.6 * L), z.get("height_m", 0.3 * L)
        bmesh.ops.create_cube(bm, size=1.0); bmesh.ops.scale(bm, vec=(w, L, h), verts=bm.verts); bmesh.ops.translate(bm, vec=(0, 0, h / 2), verts=bm.verts)
        bmesh.ops.bevel(bm, geom=bm.verts[:] + bm.edges[:], offset=min(w, h) * 0.08, segments=2, affect='EDGES'); dims = (w, L, h)
    else: raise ValueError(kind)
    bm.to_mesh(me); bm.free(); o = bpy.data.objects.new(me.name, me); sc.collection.objects.link(o); e_dims_out[:] = dims
    if kind == "nozzle": o["two_mat"] = "polymer,emissive"
    return o
def pipe_run(z, fs_from, fs_to):
    """Parallel pipes following the hull skin between two regions (or along an anchored span): n tubes, radius thickness_m/2, spacing 2.4 r."""
    n = max(2, z.get("count", 3)); r = z.get("thickness_m", 0.12) / 2; sd = z["region"].split(".")[0]
    S = hull.data.attributes["region_side"].data; skin = extreme_filter([f for f in hull.data.polygons if S[f.index].value == SIDES.index(sd)], sd, 0.6)
    A = region_frame(fs_from)[0]; B = region_frame(fs_to)[0] if fs_to is not None else A + Vector((0, -z.get("size_m", 4.0), 0))
    if z.get("anchor"): A = Vector((z["anchor"][0] * HX, z["anchor"][1] * HY, A.z)); B = A + Vector((0, -z.get("size_m", 4.0), 0))
    elif z.get("size_m"): B = Vector((A.x, A.y - z["size_m"], A.z))   # unanchored: a straight fore-to-aft run of size_m from the region centroid
    if sd in ("top", "belly") and abs(A.x) < 0.6: A.x = B.x = 0.0   # spine runs sit on the centreline
    objs = []
    # each pipe: its own straight line in plan at a constant x offset, surface height by ray cast (no face-centre zigzag, no crossing)
    inv = hull.matrix_world.inverted(); ax, sg = EXTREME[sd]; d = (B - A).normalized()
    def skin_point(q):
        o_far = q.copy(); o_far[ax] = 30.0 * sg; hit, loc, nrm, _ = hull.ray_cast(inv @ o_far, (inv.to_3x3() @ Vector([(-sg if i == ax else 0) for i in range(3)])).normalized())
        return (hull.matrix_world @ loc, (hull.matrix_world.to_3x3() @ nrm).normalized()) if hit else (q, Vector([(sg if i == ax else 0) for i in range(3)]))
    for k in range(n):
        off = (k - (n - 1) / 2) * 3.4 * r; cu = bpy.data.curves.new(f"pipe_{z['name']}_{k}", 'CURVE'); cu.dimensions = '3D'; cu.bevel_depth = r; cu.bevel_resolution = 4; cu.fill_mode = 'FULL'
        sp = cu.splines.new('POLY'); pts = []; side = d.cross(Vector([(sg if i == ax else 0) for i in range(3)])).normalized()
        for i in range(25):
            c0, n0 = skin_point(A.lerp(B, i / 24) + side * off); pts.append(c0 + n0 * (r + 0.01))
        pts = [pts[0]] + [(pts[i - 1] + pts[i] * 2 + pts[i + 1]) / 4 for i in range(1, 24)] + [pts[-1]]
        sp.points.add(len(pts) - 1)
        for pnt, q in zip(sp.points, pts): pnt.co = (q.x, q.y, q.z, 1.0)
        cu.use_fill_caps = True
        o = bpy.data.objects.new(cu.name, cu); sc.collection.objects.link(o); bpy.context.view_layer.objects.active = o; o.select_set(True); bpy.ops.object.convert(target='MESH'); o.select_set(False)
        o.name = f"{z['name']}_{k}_pipe"; o["no_mirror"] = True; objs.append(o)   # the run is symmetric by construction
    return objs, A, B
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
    if z["type"] == "connect" and z.get("prim") == "pipe_run" or (z["type"] == "connect" and z.get("prim") is None and z.get("pipes_procedural")):
        fs2 = region_faces(z["region_to"]) if z.get("region_to") else None
        objs, A, B = pipe_run(z, fs, fs2)
        graph.append({"zone": z["name"], "insert": "prim:pipe_run", "kit": "procedural", "from": z["region"], "to": z.get("region_to"), "pos": [round(v, 3) for v in (A + B) / 2], "placed_m": round((B - A).length, 2), "attach": "surface"}); continue
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
    if z["region"].startswith("flank") or (z.get("anchor") and abs(z["anchor"][0]) > 0.05 and not z["region"].startswith("flank")):
        if k % 2 == 0 and not pair: pair = True; across = None   # off-centre zones are mirrored: place half the count, the mirror supplies the rest
    if pair: k //= 2
    for i in range(k):
        if z.get("prim"):
            dims = [1, 1, 1]; o = prim_object(z, dims); e = {"name": f"prim:{z['prim']}", "dims": dims, "below": 0.0, "kit": "procedural"}
        else:
            e = pick(z, i, used)
            if not e: print("no part for", z["name"]); break
            used.add(e["name"]); o = load_insert(e)
        pos, nn = surface_at(c, n, t, (i - (k - 1) / 2) * (span / k), fs, across)
        if z.get("anchor") and k == 1:   # anchored single (or +x member of a pair): hit the skin exactly at the anchor, not at a face-centroid average
            sd0 = z["region"].split(".")[0]; q = Vector((0, z["anchor"][0] * HY, z["anchor"][1] * HZ)) if sd0 == "flank" else Vector((z["anchor"][0] * HX, z["anchor"][1] * HY, 0))
            if sd0 == "flank": q.x = HX
            p2, n2 = skin_hit(sd0, q)
            if p2 is not None: pos, nn = p2, n2
        if z.get("prim"):   # primitives are authored along +y with +z off the skin: keep them axis-true instead of following the coarse patch frame
            sd = z["region"].split(".")[0]
            if z["prim"] == "nozzle" and sd in ("tail", "nose"): nn = Vector((0, -1 if sd == "tail" else 1, 0))
            ty = Vector((0, 1, 0)); t = ty if abs(ty.dot(nn)) < 0.9 else Vector((0, 0, 1))
        tt = (t - nn * t.dot(nn)).normalized(); bb = nn.cross(tt)
        slot = span / k
        carve = z.get("carve", z.get("prim") in ("skid", "box"))
        if carve:
            # is the mount point on a lump? median skin height on a ring around it, against the mount point
            sd0 = z["region"].split(".")[0]; fx = e["dims"][0] * (z.get("size_m", 1.0) / max(1e-6, max(e["dims"])) if not z.get("prim") else 1.0); fy = e["dims"][1] * (z.get("size_m", 1.0) / max(1e-6, max(e["dims"])) if not z.get("prim") else 1.0)
            R = 1.2 * max(fx, fy, 0.3); offs = []
            for j in range(12):
                ang = 2 * math.pi * j / 12; q = pos + tt * (R * math.cos(ang)) + bb * (R * math.sin(ang)); hp, _ = skin_hit(sd0, q)
                if hp is not None: offs.append((hp - pos).dot(nn))
            offs.sort(); base = offs[len(offs) // 2] if offs else 0.0; spread = (offs[(3 * len(offs)) // 4] - offs[len(offs) // 4]) if len(offs) >= 4 else 9.0
            # a lump is part-sized and surrounded by a consistent base (a skid at the wing root sees the belly on one side and the wing underside on the other: not a lump)
            lump_ok = len(offs) >= 9 and -0.08 > base >= -(0.5 * max(fx, fy) + 0.15) and spread < 0.15 and sd0 not in ("nose", "tail")
            if lump_ok:   # ring sits lower than the mount point: a lump. Drop to base level and cut the lump above it.
                pos = pos + nn * base
                cut = bpy.data.objects.new(f"cut_{z['name']}_{i}", bpy.data.meshes.new("cut")); sc.collection.objects.link(cut); cbm = bmesh.new(); bmesh.ops.create_cube(cbm, size=1.0); cbm.to_mesh(cut.data); cbm.free()
                cut.matrix_world = Matrix.Translation(pos + nn * 1.5) @ Matrix((tt, bb, nn)).transposed().to_4x4() @ Matrix.Diagonal((fx * 1.15, fy * 1.15, 3.0, 1.0))
                cut2 = cut.copy(); cut2.data = cut.data.copy(); sc.collection.objects.link(cut2); cut2.matrix_world = Matrix.Diagonal((-1, 1, 1, 1)) @ cut.matrix_world   # both sides; the part itself is mirrored later
                for c2 in (cut, cut2):
                    md = hull.modifiers.new("carve", 'BOOLEAN'); md.operation = 'DIFFERENCE'; md.object = c2; md.solver = 'EXACT'
                    bpy.context.view_layer.objects.active = hull; bpy.ops.object.modifier_apply(modifier=md.name)
                for c2 in (cut, cut2): bpy.data.objects.remove(c2, do_unlink=True)
                graph.append({"zone": z["name"], "carved": True, "depth_m": round(-base, 3)})
                if z.get("prim") == "skid":   # the lump was the concept's skid: the ski hangs where the lump's surface was, not in the recess
                    bpy.data.objects.remove(o, do_unlink=True); z["drop_m"] = round(-base + 0.05, 3); dims = [1, 1, 1]; o = prim_object(z, dims); e = {"name": "prim:skid", "dims": dims, "below": 0.0, "kit": "procedural"}
        if z.get("prim"): s_fit = 1.0
        elif z.get("size_m"): s_fit = z["size_m"] / max(1e-6, max(e["dims"]))  # absolute size from the program
        else: s_fit = random.uniform(*z["scale"]) * min(slot / max(1e-6, e["dims"][0]), cross / max(1e-6, e["dims"][1]))
        cap = z.get("max_size_m", 4.0) / max(1e-6, max(e["dims"])); s_fit = min(s_fit, cap)
        pos = pos + nn * (e["below"] * s_fit + 0.01)
        yaw = Matrix.Rotation(math.radians(z.get("yaw_deg", 0)), 4, 'Z')  # critic "rotate": spin about the surface normal
        o.matrix_world = Matrix.Translation(pos) @ Matrix((tt, bb, nn)).transposed().to_4x4() @ yaw @ Matrix.Scale(s_fit, 4); o.name = f"{z['name']}_{i}_{e['name']}"
        placed_m = max(e["dims"]) * s_fit; tb = int(max(250, min(budget, 1800 * placed_m ** 1.5)))  # triangle budget grows with placed size
        tr = sum(len(pg.vertices) - 2 for pg in o.data.polygons)
        if tr > tb and not z.get("prim"):
            m = o.modifiers.new("lod", 'DECIMATE'); m.ratio = tb / tr; bpy.context.view_layer.objects.active = o; bpy.ops.object.modifier_apply(modifier="lod")
        graph.append({"zone": z["name"], "insert": e["name"], "kit": e["kit"], "region": z["region"], "pos": [round(v, 3) for v in pos], "anchor": z.get("anchor"), "scale": round(s_fit, 4), "placed_m": round(max(e["dims"]) * s_fit, 2), "attach": "surface"})
print("placed", len(graph))
def mat(name, rgb, rough=0.55, metal=0.0, emit=None, alpha=1.0):
    m = bpy.data.materials.get(name) or bpy.data.materials.new(name); m.use_nodes = True; bs = m.node_tree.nodes.get("Principled BSDF")
    bs.inputs["Base Color"].default_value = (*rgb, 1); bs.inputs["Roughness"].default_value = rough; bs.inputs["Metallic"].default_value = metal
    if emit: bs.inputs["Emission Color"].default_value = (*emit, 1); bs.inputs["Emission Strength"].default_value = 0.25
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
    if o.get("two_mat"):   # primitives with an outer and an inner material keep their face indices
        o.data.materials.clear()
        for rn in o["two_mat"].split(","): o.data.materials.append(M[rn])
    else:
        o.data.materials.clear(); o.data.materials.append(M[role_for(o)])
        for pg in o.data.polygons: pg.material_index = 0
    if o.name != "hull" and not o.get("no_mirror"):
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
