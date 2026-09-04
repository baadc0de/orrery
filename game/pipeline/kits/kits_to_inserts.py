# blender -b --python kits_to_inserts.py -- <kit_dir> <glob> <out_masterfolder> <kpack_name> [budget_tris] [limit_files]
# Converts every mesh part of a kit into a KIT OPS INSERT: decimate to budget, find the dominant plane (largest coplanar
# area cluster), rotate it to face -Z, put the origin on it at z=0, tag kitops props, save one .blend + .png thumbnail
# per part, and write features.json for auto-labelling.
import bpy, bmesh, sys, os, glob, json, math, time
from mathutils import Vector, Matrix
a = sys.argv[sys.argv.index("--")+1:]
kit, pat, master, kpack = a[0], a[1], os.path.abspath(a[2]), a[3]
budget = int(a[4]) if len(a) > 4 else 12000; limit = int(a[5]) if len(a) > 5 else 0
out = os.path.join(master, kpack); os.makedirs(out, exist_ok=True)
kit_id = json.load(open(os.path.join(kit, "manifest.json")))["kit_id"]
feat_path = os.path.join(out, "features.json"); feats = json.load(open(feat_path)) if os.path.exists(feat_path) else {}
files = sorted(glob.glob(os.path.join(kit, pat if pat.startswith("source/") else os.path.join("extracted", pat)), recursive=True))
if limit: files = files[:limit]

def dominant_plane(obj):
    bm = bmesh.new(); bm.from_mesh(obj.data); bm.faces.ensure_lookup_table()
    total = sum(f.calc_area() for f in bm.faces) or 1e-9
    bins = {}
    for f in bm.faces:
        n = f.normal
        if n.length < 0.5: continue
        key = (round(n.x * 6), round(n.y * 6), round(n.z * 6))
        b = bins.setdefault(key, [0.0, Vector((0, 0, 0)), Vector((0, 0, 0))]); ar = f.calc_area(); b[0] += ar; b[1] += n * ar; b[2] += f.calc_center_median() * ar
    if not bins: bm.free(); return Vector((0, 0, -1)), Vector((0, 0, 0)), 0.0, 0.0
    key = max(bins, key=lambda k: bins[k][0]); area, nsum, csum = bins[key]
    n = nsum.normalized(); c = csum / area
    # refine: faces within 8 degrees of n and within 1% of size of the plane
    size = max(obj.dimensions) or 1.0
    sel = [f for f in bm.faces if f.normal.length > 0.5 and f.normal.dot(n) > math.cos(math.radians(8)) and abs((f.calc_center_median() - c).dot(n)) < 0.01 * size]
    planar = sum(f.calc_area() for f in sel) / total
    if sel:
        n = sum((f.normal * f.calc_area() for f in sel), Vector((0, 0, 0))).normalized(); c = sum((f.calc_center_median() * f.calc_area() for f in sel), Vector((0, 0, 0))) / sum(f.calc_area() for f in sel)
    # normal spread as a cylindricity/curvature hint
    nx = sum(abs(f.normal.x) * f.calc_area() for f in bm.faces) / total; ny = sum(abs(f.normal.y) * f.calc_area() for f in bm.faces) / total; nz = sum(abs(f.normal.z) * f.calc_area() for f in bm.faces) / total
    bm.free(); return n, c, planar, [round(nx, 3), round(ny, 3), round(nz, 3)]

def thumbnail(obj, path):
    sc = bpy.context.scene; sc.render.engine = 'BLENDER_WORKBENCH'; sc.display.shading.light = 'MATCAP'; sc.display.shading.color_type = 'SINGLE'; sc.display.shading.single_color = (0.6, 0.62, 0.65)
    sc.render.resolution_x = sc.render.resolution_y = 256; sc.render.image_settings.file_format = 'PNG'; sc.render.film_transparent = True
    cam = bpy.data.objects.get("thumbcam") or bpy.data.objects.new("thumbcam", bpy.data.cameras.new("thumbcam"))
    if cam.name not in sc.collection.objects: sc.collection.objects.link(cam)
    sc.camera = cam; d = max(obj.dimensions) or 1.0; ctr = Vector(obj.location) + Vector((0, 0, obj.dimensions.z / 2))
    cam.location = ctr + Vector((1.0, -1.3, 0.9)).normalized() * d * 2.4; cam.rotation_euler = (ctr - cam.location).to_track_quat('-Z', 'Y').to_euler(); cam.data.lens = 50
    sc.render.filepath = path; bpy.ops.render.render(write_still=True)

t0 = time.time(); done = 0
for f in files:
    bpy.ops.wm.read_homefile(use_empty=True)
    ext = f.rsplit(".", 1)[-1].lower()
    if ext == "obj": bpy.ops.wm.obj_import(filepath=f)
    elif ext == "fbx": bpy.ops.import_scene.fbx(filepath=f)
    elif ext == "blend":
        with bpy.data.libraries.load(f) as (src, dst): dst.objects = src.objects
        for o in dst.objects:
            if o: bpy.context.scene.collection.objects.link(o)
    parts = [o for o in bpy.data.objects if o.type == 'MESH']
    names = [o.name for o in parts]
    for nm in names:
        o = bpy.data.objects[nm]
        for x in bpy.data.objects: x.select_set(False)
        o.select_set(True); bpy.context.view_layer.objects.active = o; bpy.ops.object.transform_apply(location=True, rotation=True, scale=True)
        src_tris = sum(len(p.vertices) - 2 for p in o.data.polygons)
        if src_tris > budget:
            m = o.modifiers.new("dec", 'DECIMATE'); m.ratio = budget / src_tris; bpy.ops.object.modifier_apply(modifier="dec")
        n, c, planar, nspread = dominant_plane(o)
        R = n.rotation_difference(Vector((0, 0, -1))).to_matrix().to_4x4()
        o.data.transform(Matrix.Translation(-c)); o.data.transform(R); o.data.update()
        zmin = min(v.co.z for v in o.data.vertices); zmax = max(v.co.z for v in o.data.vertices)
        if zmax < -zmin:
            o.data.transform(Matrix.Rotation(math.pi, 4, 'X')); o.data.update()
        o.location = (0, 0, 0); o.rotation_euler = (0, 0, 0)
        pid = nm.replace(" ", "_"); o.name = f"{kpack}_{pid}"; o.data.name = o.name
        k = o.kitops; k.main = True; k.type = 'SOLID'; k.id = f"{kit_id}/{pid}"; k.label = pid; k.author = "Orrery pipeline"
        try: bpy.ops.object.shade_smooth_by_angle(angle=math.radians(35))
        except Exception: bpy.ops.object.shade_smooth()
        dims = [round(x, 4) for x in o.dimensions]; zs = [v.co.z for v in o.data.vertices]
        feats[o.name] = {"kit": kit_id, "part": pid, "file": os.path.relpath(f, kit), "src_tris": src_tris, "tris": sum(len(p.vertices) - 2 for p in o.data.polygons),
                         "dims": dims, "aspect": round(max(dims[:2]) / max(1e-6, min(dims[:2])), 2), "height_ratio": round(dims[2] / max(1e-6, max(dims[:2])), 3),
                         "planar_fraction": round(planar, 3), "normal_spread": nspread, "below_plane": round(-min(zs), 4)}
        for x in bpy.data.objects:
            if x.type == 'MESH': x.hide_render = (x.name != o.name)
        thumbnail(o, os.path.join(out, o.name + ".png"))
        bpy.data.libraries.write(os.path.join(out, o.name + ".blend"), {o}, fake_user=True, compress=True)
        done += 1
    json.dump(feats, open(feat_path, "w"), indent=1); print("FILE", os.path.basename(f), "inserts so far", done, round(time.time() - t0), "s", flush=True)
print("CONVERTED", done, "inserts to", out)
