# blender -b --python audition_mount.py -- <masterfolder> <kpack> <insert_name> <reference.png> [project]
# Mount audition for an INSERT whose flat base is not its largest plane (generated parts, stragglers): render the part on a hull
# plate in each of its six bounding-box orientations, ask a vision model which one sits the way the reference sheet shows it
# mounted, then rewrite the INSERT in that orientation (mount plane at z=0, features.json updated, new thumbnail).
import bpy, bmesh, sys, os, json, math, base64, subprocess, urllib.request
from mathutils import Vector, Matrix
a = sys.argv[sys.argv.index("--")+1:]; master, kpack, name, ref = os.path.abspath(a[0]), a[1], a[2], os.path.abspath(a[3]); project = a[4] if len(a) > 4 else os.environ.get("VERTEX_PROJECT")
out = os.path.join(master, kpack); blend = os.path.join(out, name + ".blend"); feat_path = os.path.join(out, "features.json"); feats = json.load(open(feat_path))
bpy.ops.wm.read_homefile(use_empty=True); sc = bpy.context.scene
with bpy.data.libraries.load(blend) as (src, dst): dst.objects = list(src.objects)
o = [x for x in dst.objects if x and x.type == 'MESH'][0]; sc.collection.objects.link(o)
for x in dst.objects:
    if x and x.type == 'EMPTY': bpy.data.objects.remove(x, do_unlink=True)
# six orientations: rotate so that the named local axis points down (-Z) onto the plate
ROT = {"-z": Matrix.Identity(4), "+z": Matrix.Rotation(math.pi, 4, 'X'), "-y": Matrix.Rotation(-math.pi / 2, 4, 'X'), "+y": Matrix.Rotation(math.pi / 2, 4, 'X'), "-x": Matrix.Rotation(math.pi / 2, 4, 'Y'), "+x": Matrix.Rotation(-math.pi / 2, 4, 'Y')}
plate = bpy.data.objects.new("plate", bpy.data.meshes.new("plate")); sc.collection.objects.link(plate); bm = bmesh.new(); bmesh.ops.create_grid(bm, x_segments=1, y_segments=1, size=max(o.dimensions) * 1.6); bm.to_mesh(plate.data); bm.free()
pm = bpy.data.materials.new("plate_m"); pm.diffuse_color = (0.18, 0.19, 0.21, 1); plate.data.materials.append(pm)
sc.render.engine = 'BLENDER_WORKBENCH'; sc.display.shading.light = 'MATCAP'; sc.display.shading.color_type = 'MATERIAL'; sc.render.resolution_x = sc.render.resolution_y = 320; sc.render.image_settings.file_format = 'PNG'
om = bpy.data.materials.new("part_m"); om.diffuse_color = (0.65, 0.66, 0.68, 1); o.data.materials.clear(); o.data.materials.append(om)
cam = bpy.data.objects.new("cam", bpy.data.cameras.new("cam")); sc.collection.objects.link(cam); sc.camera = cam; cam.data.lens = 50
keys = list(ROT); shots = []
base_mw = o.matrix_world.copy()
def place(key):
    o.matrix_world = ROT[key] @ base_mw; bpy.context.view_layer.update()
    zs = [(o.matrix_world @ Vector(c)).z for c in o.bound_box]; o.matrix_world = Matrix.Translation((0, 0, -min(zs))) @ o.matrix_world; bpy.context.view_layer.update()
for key in keys:
    place(key); d = max(o.dimensions) or 1.0; ctr = Vector((0, 0, o.dimensions.z / 2))
    cam.location = ctr + Vector((1.0, -1.3, 0.75)).normalized() * d * 2.6; cam.rotation_euler = (ctr - cam.location).to_track_quat('-Z', 'Y').to_euler()
    p = os.path.join(out, f".audition_{name}_{key}.png"); sc.render.filepath = p; bpy.ops.render.render(write_still=True); shots.append(p)
sheet = os.path.join(out, f".audition_{name}.png"); font = os.environ.get("SHEET_FONT", "/usr/share/fonts/noto/NotoSans-Regular.ttf")
subprocess.run(["magick", "montage"] + sum([["-label", str(i + 1), p] for i, p in enumerate(shots)], []) + ["-tile", "3x2", "-geometry", "320x320+6+6", "-background", "#202020", "-fill", "white", "-pointsize", "30", "-font", font, sheet], check=True)
def img(p): return {"inlineData": {"mimeType": "image/png", "data": base64.b64encode(open(p, "rb").read()).decode()}}
prompt = ("Image 1 is a reference sheet of a spacecraft subassembly, drawn with its mounting face toward the hull. Image 2 shows the same part as a 3D scan placed on a dark hull plate in six orientations, numbered 1..6. "
          "Which number has the part sitting on the plate the way the reference shows it mounted (its base against the plate, the working feature pointing away from the plate)? Reply as JSON: {\"pick\": int, \"reason\": \"<=15 words\"}.")
tok = subprocess.check_output(["gcloud", "auth", "print-access-token"], text=True).strip()
body = {"contents": [{"role": "user", "parts": [{"text": prompt}, {"text": "IMAGE 1:"}, img(ref), {"text": "IMAGE 2:"}, img(sheet)]}], "generationConfig": {"temperature": 0.0, "responseMimeType": "application/json", "maxOutputTokens": 512, "thinkingConfig": {"thinkingBudget": 0}}}
req = urllib.request.Request(f"https://aiplatform.googleapis.com/v1/projects/{project}/locations/global/publishers/google/models/gemini-3.5-flash:generateContent", data=json.dumps(body).encode(), headers={"Authorization": f"Bearer {tok}", "Content-Type": "application/json"})
with urllib.request.urlopen(req, timeout=120) as r: resp = json.loads(r.read())
txt = "".join(p.get("text", "") for p in resp["candidates"][0]["content"]["parts"]); v = json.loads(txt[txt.find("{"): txt.rfind("}") + 1]); pick = keys[max(1, min(6, int(v.get("pick", 1)))) - 1]
print("AUDITION", name, "pick", pick, "|", v.get("reason", ""))
# rewrite the INSERT in the chosen orientation: mount plane at z=0, xy centred
place(pick); bpy.context.view_layer.objects.active = o; o.select_set(True); bpy.ops.object.transform_apply(location=True, rotation=True, scale=True)
xs = [v.co.x for v in o.data.vertices]; ys = [v.co.y for v in o.data.vertices]; cx, cy = (min(xs) + max(xs)) / 2, (min(ys) + max(ys)) / 2
for v in o.data.vertices: v.co.x -= cx; v.co.y -= cy
dims = [round(x, 4) for x in o.dimensions]; f = feats[o.name]; f.update({"dims": dims, "aspect": round(max(dims[:2]) / max(1e-6, min(dims[:2])), 2), "height_ratio": round(dims[2] / max(1e-6, max(dims[:2])), 3), "below_plane": 0.0, "mount": "audition:" + pick})
json.dump(feats, open(feat_path, "w"), indent=1)
bpy.data.objects.remove(plate, do_unlink=True); bpy.data.objects.remove(cam, do_unlink=True)
sc.display.shading.color_type = 'SINGLE'; sc.display.shading.single_color = (0.6, 0.62, 0.65); sc.render.resolution_x = sc.render.resolution_y = 256; sc.render.film_transparent = True
cam = bpy.data.objects.new("thumbcam", bpy.data.cameras.new("thumbcam")); sc.collection.objects.link(cam); sc.camera = cam; d = max(o.dimensions) or 1.0; ctr = Vector((0, 0, o.dimensions.z / 2))
cam.location = ctr + Vector((1.0, -1.3, 0.9)).normalized() * d * 2.4; cam.rotation_euler = (ctr - cam.location).to_track_quat('-Z', 'Y').to_euler(); cam.data.lens = 50
o.data.materials.clear(); sc.render.filepath = os.path.join(out, o.name + ".png"); bpy.ops.render.render(write_still=True)
bpy.data.objects.remove(cam, do_unlink=True); bpy.data.libraries.write(blend, {o}, fake_user=True, compress=True)
for p in shots + [sheet]: os.remove(p)
print("REWROTE", blend, dims)
