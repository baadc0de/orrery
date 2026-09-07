# Blender headless: build a proxy from the brief's dimensions (9 m long, 6 m span, 2.5 m tall wedge + wings + twin thrusters + fin)
# and render orthographic depth maps from front/side/back/top cameras. Output: out/proxy/depth-<view>.png (16-bit, near=white).
import bpy, math, os, sys
out = os.path.abspath(sys.argv[sys.argv.index("--")+1]) if "--" in sys.argv else "out/proxy"
os.makedirs(out, exist_ok=True)
bpy.ops.wm.read_factory_settings(use_empty=True)
sc = bpy.context.scene
def box(name, loc, dim):
    bpy.ops.mesh.primitive_cube_add(location=loc); o = bpy.context.object; o.name = name; o.scale = (dim[0]/2, dim[1]/2, dim[2]/2); return o
def cyl(name, loc, r, depth, rot=(0,0,0)):
    bpy.ops.mesh.primitive_cylinder_add(location=loc, radius=r, depth=depth, rotation=rot); o = bpy.context.object; o.name = name; return o
# nose +Y. Hull: 9 m long (Y), 2.2 m wide, 2.0 m tall; tapered nose via a second, smaller box
hull = box("hull", (0, -0.5, 1.2), (2.2, 7.0, 2.0))
nose = box("nose", (0, 3.6, 1.0), (1.6, 2.0, 1.4))
canopy = box("canopy", (0, 1.2, 2.3), (1.0, 1.6, 0.5))
wl = box("wing_l", (-2.0, -1.5, 1.1), (2.0, 3.0, 0.25)); wr = box("wing_r", (2.0, -1.5, 1.1), (2.0, 3.0, 0.25))
pl = cyl("rcs_l", (-3.1, -1.5, 1.1), 0.25, 0.9, (math.pi/2, 0, 0)); pr = cyl("rcs_r", (3.1, -1.5, 1.1), 0.25, 0.9, (math.pi/2, 0, 0))
tl = cyl("thr_l", (-0.7, -4.4, 1.3), 0.55, 1.2, (math.pi/2, 0, 0)); tr = cyl("thr_r", (0.7, -4.4, 1.3), 0.55, 1.2, (math.pi/2, 0, 0))
fin = box("fin", (0, -3.2, 2.9), (0.15, 1.6, 1.4))
sk1 = box("skid_f", (0, 2.0, 0.15), (1.6, 0.8, 0.3)); sk2 = box("skid_r", (0, -2.5, 0.15), (2.0, 0.8, 0.3))
# cameras: orthographic, ortho_scale covers 10 m
def cam(name, loc, rot):
    c = bpy.data.cameras.new(name); c.type = 'ORTHO'; c.ortho_scale = 10.0; c.clip_start = 0.1; c.clip_end = 100
    o = bpy.data.objects.new(name, c); sc.collection.objects.link(o); o.location = loc; o.rotation_euler = rot; return o
cams = {"front": cam("front", (0, 30, 1.25), (math.pi/2, 0, math.pi)),
        "back":  cam("back",  (0, -30, 1.25), (math.pi/2, 0, 0)),
        "side":  cam("side",  (-30, 0, 1.25), (math.pi/2, 0, -math.pi/2)),
        "top":   cam("top",   (0, 0, 30), (0, 0, 0))}
sc.render.engine = 'BLENDER_EEVEE' if 'BLENDER_EEVEE' in [i.identifier for i in bpy.types.RenderSettings.bl_rna.properties['engine'].enum_items] else 'BLENDER_EEVEE_NEXT'
sc.render.resolution_x = sc.render.resolution_y = 1024; sc.render.image_settings.file_format = 'PNG'; sc.render.image_settings.color_depth = '16'; sc.render.image_settings.color_mode = 'BW'
sc.view_settings.view_transform = 'Standard'; sc.render.film_transparent = False
mat = bpy.data.materials.new("depth"); mat.use_nodes = True; nt = mat.node_tree; nt.nodes.clear()
camd = nt.nodes.new('ShaderNodeCameraData'); mr = nt.nodes.new('ShaderNodeMapRange'); em = nt.nodes.new('ShaderNodeEmission'); outn = nt.nodes.new('ShaderNodeOutputMaterial')
mr.inputs['From Min'].default_value = 24.0; mr.inputs['From Max'].default_value = 36.0; mr.inputs['To Min'].default_value = 1.0; mr.inputs['To Max'].default_value = 0.0; mr.clamp = True
nt.links.new(camd.outputs['View Z Depth'], mr.inputs['Value']); nt.links.new(mr.outputs['Result'], em.inputs['Color']); nt.links.new(em.outputs['Emission'], outn.inputs['Surface'])
for o in bpy.data.objects:
    if o.type == 'MESH': o.data.materials.clear(); o.data.materials.append(mat)
w = bpy.data.worlds.new("w"); sc.world = w; w.use_nodes = True; w.node_tree.nodes['Background'].inputs['Color'].default_value = (0, 0, 0, 1)
for v, c in cams.items():
    sc.camera = c; sc.render.filepath = os.path.join(out, f"depth-{v}.png"); bpy.ops.render.render(write_still=True); print("rendered", v)
bpy.ops.wm.save_as_mainfile(filepath=os.path.join(out, "proxy.blend"))
