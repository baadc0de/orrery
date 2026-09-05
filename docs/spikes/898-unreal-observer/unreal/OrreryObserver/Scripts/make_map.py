"""Spike #898 step 3: author the observer map from the editor's Python, headless.

    UnrealEditor-Cmd OrreryObserver.uproject -run=pythonscript -script=Scripts/make_map.py -NullRHI -unattended

/Game/Maps/OrreryObserver: a flat plane, a light, and one AObserverScenario
actor. The capsules are not authored — the scenario spawns one per stable id
the sidecars present, which is the whole point: the observer learns the
population from the frames and never from the map.
"""
import unreal

ELL = unreal.EditorLevelLibrary
EAL = unreal.EditorAssetLibrary

plane = unreal.load_asset("/Engine/BasicShapes/Plane")

path = "/Game/Maps/OrreryObserver"
if EAL.does_asset_exist(path):
    EAL.delete_asset(path)
assert ELL.new_level(path), path

floor = ELL.spawn_actor_from_class(unreal.StaticMeshActor, unreal.Vector(0, 0, 0))
floor.static_mesh_component.set_static_mesh(plane)
floor.set_actor_scale3d(unreal.Vector(200.0, 200.0, 1.0))
floor.set_actor_label("Ground")

scenario = ELL.spawn_actor_from_class(unreal.ObserverScenario, unreal.Vector(0, 0, 0))
scenario.set_actor_label("ObserverScenario")

light = ELL.spawn_actor_from_class(unreal.DirectionalLight, unreal.Vector(0, 0, 1000))
light.set_actor_rotation(unreal.Rotator(-50, 30, 0), False)

assert ELL.save_current_level()
unreal.log("spike 898: map written")
