# Blender headless: import a glTF/GLB (or use a stand-in), frame it, three-point light, render stills.
# usage: blender -b --python blender_render.py -- <in.glb|--standin> <out_dir> [views: hero,front,side,top]
import bpy, math, os, sys
args = sys.argv[sys.argv.index("--")+1:]; src, out = args[0], os.path.abspath(args[1]); views = (args[2] if len(args) > 2 else "hero,front,side,top").split(",")
flat = len(args) > 3 and args[3] == "flat"   # flat: unlit id-colour pass (emissive materials, Standard view transform), files render-<view>-id.png
os.makedirs(out, exist_ok=True)
bpy.ops.wm.read_factory_settings(use_empty=True); sc = bpy.context.scene
if src == "--standin":
    bpy.ops.mesh.primitive_monkey_add(size=2); bpy.ops.object.shade_smooth()
else:
    bpy.ops.import_scene.gltf(filepath=os.path.abspath(src))
meshes = [o for o in bpy.data.objects if o.type == 'MESH']
# bounding box of everything
import mathutils
mn = mathutils.Vector((1e9,)*3); mx = mathutils.Vector((-1e9,)*3)
for o in meshes:
    for c in o.bound_box:
        w = o.matrix_world @ mathutils.Vector(c); mn = mathutils.Vector(map(min, mn, w)); mx = mathutils.Vector(map(max, mx, w))
ctr = (mn + mx) / 2; size = max(mx - mn); print("bbox size", tuple(round(v, 3) for v in (mx - mn)), "tris", sum(len(o.data.polygons) for o in meshes))
# world + lights
w = bpy.data.worlds.new("w"); sc.world = w; w.use_nodes = True; nt = w.node_tree; bg = nt.nodes['Background']
# the camera sees a mid-grey backdrop, but the scene is lit only by the lamps (plus a faint dark ambient), so shading has contrast
lp = nt.nodes.new('ShaderNodeLightPath'); mix = nt.nodes.new('ShaderNodeMixShader'); bg2 = nt.nodes.new('ShaderNodeBackground')
bg.inputs['Color'].default_value = (0.0, 0.0, 0.0, 1) if flat else (0.06, 0.065, 0.075, 1); bg.inputs['Strength'].default_value = 0.0 if flat else 1.0
bg2.inputs['Color'].default_value = (0.0, 0.0, 0.0, 1) if flat else (0.52, 0.53, 0.55, 1); bg2.inputs['Strength'].default_value = 1.0
nt.links.new(lp.outputs['Is Camera Ray'], mix.inputs['Fac']); nt.links.new(bg.outputs[0], mix.inputs[1]); nt.links.new(bg2.outputs[0], mix.inputs[2]); nt.links.new(mix.outputs[0], nt.nodes['World Output'].inputs['Surface'])
def light(name, loc, energy, kind='AREA', sz=None):
    l = bpy.data.lights.new(name, kind); l.energy = energy
    if sz: l.size = sz
    o = bpy.data.objects.new(name, l); sc.collection.objects.link(o); o.location = ctr + mathutils.Vector(loc) * size
    d = ctr - o.location; o.rotation_euler = d.to_track_quat('-Z', 'Y').to_euler(); return o
if not flat: light("key", (1.2, -1.5, 1.6), 26 * size**2, sz=size); light("fill", (-1.8, -0.8, 0.8), 6 * size**2, sz=size*1.5); light("rim", (0.3, 1.8, 1.2), 16 * size**2, sz=size*0.6)  # ~1.6 kW key at 9.5 m: mid-grey hull reads as mid-grey
# cameras
def cam(name, dirv, ortho=False):
    c = bpy.data.cameras.new(name); o = bpy.data.objects.new(name, c); sc.collection.objects.link(o)
    o.location = ctr + mathutils.Vector(dirv).normalized() * size * (1.0 if ortho else 1.35)
    o.rotation_euler = (ctr - o.location).to_track_quat('-Z', 'Y').to_euler()
    if ortho: c.type = 'ORTHO'; c.ortho_scale = size * 1.15
    else: c.lens = 50
    return o
cams = {"hero": cam("hero", (1.0, -1.3, 0.7)), "front": cam("front", (0, -1, 0), True), "side": cam("side", (-1, 0, 0), True), "top": cam("top", (0, 0, 1), True), "belly": cam("belly", (0, 0, -1), True), "rear": cam("rear", (0.9, 1.3, 0.5))}
engines = [i.identifier for i in bpy.types.RenderSettings.bl_rna.properties['engine'].enum_items]
engine = os.environ.get("RENDER_ENGINE", "CYCLES")   # Cycles GPU with few samples gives real shadows headless; EEVEE Next needs a display for its shadow maps
if engine == "CYCLES" and not flat:
    sc.render.engine = 'CYCLES'; sc.cycles.samples = int(os.environ.get("RENDER_SAMPLES", "48")); sc.cycles.use_denoising = True; sc.cycles.device = 'GPU'
    prefs = bpy.context.preferences.addons.get('cycles')
    if prefs:
        cp = prefs.preferences
        for backend in ('OPTIX', 'CUDA'):
            try:
                cp.compute_device_type = backend; cp.get_devices()
                for d in cp.devices: d.use = d.type != 'CPU'
                if any(d.use for d in cp.devices): break
            except Exception: continue
else: sc.render.engine = 'BLENDER_EEVEE' if 'BLENDER_EEVEE' in engines else 'BLENDER_EEVEE_NEXT'
if hasattr(sc.eevee, "use_shadows"): sc.eevee.use_shadows = True
if hasattr(sc.eevee, "use_raytracing"): sc.eevee.use_raytracing = True
sc.view_settings.exposure = 0.0; sc.render.resolution_x = sc.render.resolution_y = 1024; sc.render.image_settings.file_format = 'PNG'; sc.view_settings.view_transform = 'Standard' if flat else 'AgX' if 'AgX' in [i.identifier for i in bpy.types.ColorManagedViewSettings.bl_rna.properties['view_transform'].enum_items] else 'Filmic'
for v in views:
    sc.camera = cams[v]; sc.render.filepath = os.path.join(out, f"render-{v}{'-id' if flat else ''}.png"); bpy.ops.render.render(write_still=True); print("rendered", v)
