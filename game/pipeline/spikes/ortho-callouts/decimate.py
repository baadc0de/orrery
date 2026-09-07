# blender -b --python decimate.py -- in.glb out.glb ratio texsize
import bpy, sys, os
a = sys.argv[sys.argv.index("--")+1:]; src, dst, ratio, tex = a[0], a[1], float(a[2]), int(a[3])
bpy.ops.wm.read_factory_settings(use_empty=True)
bpy.ops.import_scene.gltf(filepath=os.path.abspath(src))
for o in [o for o in bpy.data.objects if o.type == 'MESH']:
    bpy.context.view_layer.objects.active = o; m = o.modifiers.new("dec", 'DECIMATE'); m.ratio = ratio; bpy.ops.object.modifier_apply(modifier="dec")
for img in bpy.data.images:
    if img.size[0] > tex: img.scale(tex, tex)
tris = sum(len(o.data.polygons) for o in bpy.data.objects if o.type == 'MESH'); print("tris", tris)
bpy.ops.export_scene.gltf(filepath=os.path.abspath(dst), export_format='GLB', export_image_format='JPEG', export_jpeg_quality=80)
