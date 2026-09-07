# blender -b --python retopo.py -- in.glb out_dir target_faces tex_size
# High-poly -> voxel remesh -> QuadriFlow quads -> symmetrize (axis chosen by measured mirror error) -> UV -> bake colour + normal -> GLB.
import bpy, bmesh, sys, os, math, json, time
from mathutils import Vector
from mathutils.kdtree import KDTree
a = sys.argv[sys.argv.index("--")+1:]; src, out, target, tex = a[0], os.path.abspath(a[1]), int(a[2]), int(a[3]); os.makedirs(out, exist_ok=True)
T0 = time.time(); log = {}
bpy.ops.wm.read_factory_settings(use_empty=True); sc = bpy.context.scene
bpy.ops.import_scene.gltf(filepath=os.path.abspath(src))
hi = [o for o in bpy.data.objects if o.type == 'MESH']
bpy.ops.object.select_all(action='DESELECT')
for o in hi: o.select_set(True)
bpy.context.view_layer.objects.active = hi[0]
if len(hi) > 1: bpy.ops.object.join()
hi = bpy.context.view_layer.objects.active; hi.name = "hi"
bpy.ops.object.transform_apply(location=True, rotation=True, scale=True)
# centre on bbox
bb = [hi.matrix_world @ Vector(c) for c in hi.bound_box]; mn = Vector(map(min, *bb)); mx = Vector(map(max, *bb)); ctr = (mn + mx) / 2
for v in hi.data.vertices: v.co -= ctr
log["hi_tris"] = sum(len(p.vertices) - 2 for p in hi.data.polygons); log["bbox"] = [round(x, 4) for x in (mx - mn)]
# --- symmetry axis: mirror error across x=0 and y=0
kd = KDTree(len(hi.data.vertices)); [kd.insert(v.co, i) for i, v in enumerate(hi.data.vertices)]; kd.balance()
def mirror_err(axis):
    n = 0; s = 0.0
    for i, v in enumerate(hi.data.vertices):
        if i % 7: continue
        m = v.co.copy(); m[axis] = -m[axis]; _, _, d = kd.find(m); s += d; n += 1
    return s / n
errs = {ax: mirror_err(i) for i, ax in enumerate("xy")}; axis = min(errs, key=errs.get); log["mirror_error"] = {k: round(v, 5) for k, v in errs.items()}; log["symmetry_axis"] = axis
# --- low-poly: copy, voxel remesh, quadriflow, symmetrize
lo = hi.copy(); lo.data = hi.data.copy(); lo.name = "lo"; sc.collection.objects.link(lo)
bpy.ops.object.select_all(action='DESELECT'); lo.select_set(True); bpy.context.view_layer.objects.active = lo
size = max(mx - mn)
lo.data.remesh_voxel_size = size / 220; lo.data.remesh_voxel_adaptivity = 0.0; lo.data.use_remesh_fix_poles = True
bpy.ops.object.voxel_remesh(); log["voxel_tris"] = len(lo.data.polygons) * 2
bpy.ops.object.mode_set(mode='EDIT'); bpy.ops.mesh.select_all(action='SELECT'); bpy.ops.mesh.delete_loose(); bpy.ops.mesh.select_all(action='SELECT')
bpy.ops.mesh.remove_doubles(threshold=1e-6); bpy.ops.mesh.normals_make_consistent(inside=False); bpy.ops.mesh.select_all(action='SELECT'); bpy.ops.mesh.fill_holes(sides=0)
bpy.ops.object.mode_set(mode='OBJECT')
bm = bmesh.new(); bm.from_mesh(lo.data); nonman = sum(1 for e in bm.edges if not e.is_manifold); bm.free(); log["nonmanifold_edges_after_voxel"] = nonman
r = bpy.ops.object.quadriflow_remesh(target_faces=target, use_mesh_symmetry=True, use_preserve_sharp=False, use_preserve_boundary=False, seed=7)
log["quadriflow"] = str(r); log["quad_faces_before_sym"] = len(lo.data.polygons)
if 'CANCELLED' in log["quadriflow"]:
    # fallback: collapse-decimate to the target so downstream still runs, flagged in the log
    log["matrix_det"] = round(lo.matrix_world.to_3x3().determinant(), 6)
    m = lo.modifiers.new("dec", 'DECIMATE'); m.ratio = min(1.0, target * 2 / max(1, len(lo.data.polygons) * 2)); m.use_symmetry = True; m.symmetry_axis = axis.upper()
    bpy.ops.object.modifier_apply(modifier="dec")
    bpy.ops.object.mode_set(mode='EDIT'); bpy.ops.mesh.select_all(action='SELECT'); bpy.ops.mesh.tris_convert_to_quads(face_threshold=math.radians(40), shape_threshold=math.radians(40)); bpy.ops.object.mode_set(mode='OBJECT')
    log["fallback"] = "decimate+tris_to_quads"; log["quads_share"] = round(sum(1 for f in lo.data.polygons if len(f.vertices) == 4) / max(1, len(lo.data.polygons)), 3)
bpy.ops.object.mode_set(mode='EDIT'); bpy.ops.mesh.select_all(action='SELECT')
bpy.ops.mesh.symmetrize(direction=('NEGATIVE_X' if axis == 'x' else 'NEGATIVE_Y'), threshold=size * 0.002)
bpy.ops.mesh.remove_doubles(threshold=size * 0.0005)
bpy.ops.uv.smart_project(angle_limit=math.radians(66), island_margin=0.003, scale_to_bounds=False)
bpy.ops.object.mode_set(mode='OBJECT'); bpy.ops.object.shade_smooth()
log["quad_faces"] = len(lo.data.polygons); log["lo_tris"] = sum(len(p.vertices) - 2 for p in lo.data.polygons)
# --- bake colour + normal from hi to lo
col = bpy.data.images.new("escort_color", tex, tex); nrm = bpy.data.images.new("escort_normal", tex, tex, float_buffer=False); nrm.colorspace_settings.name = 'Non-Color'
mat = bpy.data.materials.new("escort_lo"); mat.use_nodes = True; nt = mat.node_tree; bsdf = nt.nodes["Principled BSDF"]
tc = nt.nodes.new("ShaderNodeTexImage"); tc.image = col; nt.links.new(tc.outputs["Color"], bsdf.inputs["Base Color"])
tn = nt.nodes.new("ShaderNodeTexImage"); tn.image = nrm; nm = nt.nodes.new("ShaderNodeNormalMap"); nt.links.new(tn.outputs["Color"], nm.inputs["Color"]); nt.links.new(nm.outputs["Normal"], bsdf.inputs["Normal"])
bsdf.inputs["Roughness"].default_value = 0.6; bsdf.inputs["Metallic"].default_value = 0.0
lo.data.materials.clear(); lo.data.materials.append(mat)
sc.render.engine = 'CYCLES'; sc.cycles.device = 'GPU'; sc.cycles.samples = 16; sc.render.bake.use_selected_to_active = True; sc.render.bake.cage_extrusion = size * 0.02; sc.render.bake.max_ray_distance = size * 0.05
prefs = bpy.context.preferences.addons.get('cycles'); 
if prefs:
    prefs.preferences.compute_device_type = 'OPTIX' if any(d.type == 'OPTIX' for d in prefs.preferences.get_devices_for_type('OPTIX')) else 'CUDA'
    for d in prefs.preferences.devices: d.use = True
bpy.ops.object.select_all(action='DESELECT'); hi.select_set(True); lo.select_set(True); bpy.context.view_layer.objects.active = lo
nt.nodes.active = tc; sc.render.bake.use_pass_direct = False; sc.render.bake.use_pass_indirect = False; sc.render.bake.use_pass_color = True
bpy.ops.object.bake(type='DIFFUSE'); col.filepath_raw = os.path.join(out, "escort_color.png"); col.file_format = 'PNG'; col.save()
nt.nodes.active = tn; bpy.ops.object.bake(type='NORMAL', normal_space='TANGENT'); nrm.filepath_raw = os.path.join(out, "escort_normal.png"); nrm.file_format = 'PNG'; nrm.save()
# --- export lo only
hi.hide_render = True; bpy.ops.object.select_all(action='DESELECT'); lo.select_set(True)
bpy.ops.export_scene.gltf(filepath=os.path.join(out, "escort-retopo.glb"), export_format='GLB', use_selection=True, export_image_format='AUTO')
bpy.ops.wm.save_as_mainfile(filepath=os.path.join(out, "escort-retopo.blend"))
log["seconds"] = round(time.time() - T0, 1); json.dump(log, open(os.path.join(out, "retopo.json"), "w"), indent=2); print("RETOPO", json.dumps(log))
