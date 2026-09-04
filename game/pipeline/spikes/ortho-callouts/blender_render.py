# Blender headless: import a glTF/GLB (or use a stand-in), frame it, three-point light, render stills.
# usage: blender -b --python blender_render.py -- <in.glb|--standin> <out_dir> [views: hero,front,side,top]
import bpy, math, os, sys
args = sys.argv[sys.argv.index("--")+1:]; src, out = args[0], os.path.abspath(args[1]); views = (args[2] if len(args) > 2 else "hero,front,side,top").split(",")
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
w = bpy.data.worlds.new("w"); sc.world = w; w.use_nodes = True; w.node_tree.nodes['Background'].inputs['Color'].default_value = (0.35, 0.36, 0.38, 1); w.node_tree.nodes['Background'].inputs['Strength'].default_value = 1.0
def light(name, loc, energy, kind='AREA', sz=None):
    l = bpy.data.lights.new(name, kind); l.energy = energy
    if sz: l.size = sz
    o = bpy.data.objects.new(name, l); sc.collection.objects.link(o); o.location = ctr + mathutils.Vector(loc) * size
    d = ctr - o.location; o.rotation_euler = d.to_track_quat('-Z', 'Y').to_euler(); return o
light("key", (1.2, -1.5, 1.6), 120 * size**2, sz=size); light("fill", (-1.8, -0.8, 0.8), 40 * size**2, sz=size*1.5); light("rim", (0.3, 1.8, 1.2), 90 * size**2, sz=size*0.6)
# cameras
def cam(name, dirv, ortho=False):
    c = bpy.data.cameras.new(name); o = bpy.data.objects.new(name, c); sc.collection.objects.link(o)
    o.location = ctr + mathutils.Vector(dirv).normalized() * size * (1.0 if ortho else 2.2)
    o.rotation_euler = (ctr - o.location).to_track_quat('-Z', 'Y').to_euler()
    if ortho: c.type = 'ORTHO'; c.ortho_scale = size * 1.15
    else: c.lens = 50
    return o
cams = {"hero": cam("hero", (1.0, -1.3, 0.7)), "front": cam("front", (0, -1, 0), True), "side": cam("side", (-1, 0, 0), True), "top": cam("top", (0, 0, 1), True)}
engines = [i.identifier for i in bpy.types.RenderSettings.bl_rna.properties['engine'].enum_items]
sc.render.engine = 'BLENDER_EEVEE' if 'BLENDER_EEVEE' in engines else 'BLENDER_EEVEE_NEXT'
sc.render.resolution_x = sc.render.resolution_y = 1024; sc.render.image_settings.file_format = 'PNG'; sc.view_settings.view_transform = 'AgX' if 'AgX' in [i.identifier for i in bpy.types.ColorManagedViewSettings.bl_rna.properties['view_transform'].enum_items] else 'Filmic'
for v in views:
    sc.camera = cams[v]; sc.render.filepath = os.path.join(out, f"render-{v}.png"); bpy.ops.render.render(write_still=True); print("rendered", v)
