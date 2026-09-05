# blender -b --python palette.py -- <masterfolder> <out_dir>
# Build the input palette for a constructible concept: a dozen numbered parts (library INSERTs + straight primitives), each with a
# thumbnail and metres dims, a numbered contact sheet, and palette.json. The concept artist may only use these.
import bpy, bmesh, sys, os, json, glob, subprocess, math
from mathutils import Vector
a = sys.argv[sys.argv.index("--")+1:]; master, out = os.path.abspath(a[0]), os.path.abspath(a[1]); os.makedirs(out, exist_ok=True)
LIB = [("mech_module_010", "pipe with end bracket"), ("cables_Greeble_Cables_Pack1.016", "pipe bundle with brackets"), ("cables_Greeble_Cables_Pack1.005", "strip frame with reinforced ends"),
       ("ship-a_Detail_34", "flat plate with reinforced ends"), ("ship-a_Detail_95", "bracket with mounting points"), ("ship-a_Detail_96", "housing with grille"),
       ("cables_Greeble_Cables_Pack1.036", "dual parallel pipes with curved ends"), ("mech_module_046", "U-shaped bracket")]
PRIM = [("tube", "straight tube, 1 m long, 0.05 m diameter"), ("bar", "square bar, 1 m long, 0.05 m section"), ("plate", "flat plate 1 x 0.5 m, 0.02 m thick"), ("box", "box 0.5 x 0.3 x 0.3 m")]
feats = {}
for kp in glob.glob(os.path.join(master, "*")):
    fp = os.path.join(kp, "features.json")
    if os.path.exists(fp):
        for n, f in json.load(open(fp)).items(): feats[n] = dict(f, blend=os.path.join(kp, n + ".blend"), png=os.path.join(kp, n + ".png"))
def prim_mesh(kind):
    bm = bmesh.new()
    if kind == "tube": bmesh.ops.create_cone(bm, cap_ends=True, segments=16, radius1=0.025, radius2=0.025, depth=1.0); bmesh.ops.rotate(bm, cent=(0, 0, 0), matrix=__import__("mathutils").Matrix.Rotation(math.pi / 2, 3, 'X'), verts=bm.verts); dims = (0.05, 1.0, 0.05)
    elif kind == "bar": bmesh.ops.create_cube(bm, size=1.0); bmesh.ops.scale(bm, vec=(0.05, 1.0, 0.05), verts=bm.verts); dims = (0.05, 1.0, 0.05)
    elif kind == "plate": bmesh.ops.create_cube(bm, size=1.0); bmesh.ops.scale(bm, vec=(1.0, 0.5, 0.02), verts=bm.verts); dims = (1.0, 0.5, 0.02)
    else: bmesh.ops.create_cube(bm, size=1.0); bmesh.ops.scale(bm, vec=(0.5, 0.3, 0.3), verts=bm.verts); dims = (0.5, 0.3, 0.3)
    me = bpy.data.meshes.new(kind); bm.to_mesh(me); bm.free(); return me, dims
def thumb(o, path):
    sc = bpy.context.scene; sc.render.engine = 'BLENDER_WORKBENCH'; sc.display.shading.light = 'MATCAP'; sc.display.shading.color_type = 'SINGLE'; sc.display.shading.single_color = (0.6, 0.62, 0.65)
    sc.render.resolution_x = sc.render.resolution_y = 256; sc.render.image_settings.file_format = 'PNG'; sc.render.film_transparent = True
    cam = bpy.data.objects.get("thumbcam") or bpy.data.objects.new("thumbcam", bpy.data.cameras.new("thumbcam"))
    if cam.name not in sc.collection.objects: sc.collection.objects.link(cam)
    sc.camera = cam; d = max(o.dimensions) or 1.0; ctr = Vector(o.location)
    cam.location = ctr + Vector((1.0, -1.3, 0.9)).normalized() * d * 2.4; cam.rotation_euler = (ctr - cam.location).to_track_quat('-Z', 'Y').to_euler(); cam.data.lens = 50
    sc.render.filepath = path; bpy.ops.render.render(write_still=True)
bpy.ops.wm.read_homefile(use_empty=True)
pal = []; i = 0
for n, desc in LIB:
    f = feats[n]; i += 1; png = os.path.join(out, f"{i:02d}.png"); subprocess.run(["magick", f["png"], "-background", "#2a2a2a", "-flatten", png], check=True)
    d = f["dims"]; s = 1.0 / max(d)   # library parts are shown normalised; the concept states sizes in metres and the assembler scales
    pal.append({"id": i, "name": n, "kind": "insert", "desc": desc, "dims_native": d, "png": png, "blend": f["blend"], "below": f.get("below_plane", 0)})
for k, desc in PRIM:
    i += 1; me, dims = prim_mesh(k); o = bpy.data.objects.new(k, me); bpy.context.scene.collection.objects.link(o); png = os.path.join(out, f"{i:02d}.png"); thumb(o, png)
    subprocess.run(["magick", png, "-background", "#2a2a2a", "-flatten", png], check=True); bpy.data.objects.remove(o, do_unlink=True)
    pal.append({"id": i, "name": k, "kind": "prim", "desc": desc, "dims_native": list(dims), "png": png})
font = os.environ.get("SHEET_FONT", "/usr/share/fonts/noto/NotoSans-Regular.ttf"); sheet = os.path.join(out, "palette-sheet.png")
subprocess.run(["magick", "montage"] + sum([["-label", f"{p['id']}  {p['desc'][:28]}", p["png"]] for p in pal], []) + ["-tile", "4x3", "-geometry", "256x256+8+8", "-background", "#202020", "-fill", "white", "-pointsize", "16", "-font", font, sheet], check=True)
json.dump({"parts": pal, "sheet": sheet}, open(os.path.join(out, "palette.json"), "w"), indent=1); print("PALETTE", len(pal), sheet)
