"""Spike #1045: author the two maps from the editor's Python, headless.

    UnrealEditor-Cmd MovingInteriors.uproject -run=pythonscript -script=Scripts/make_maps.py -NullRHI -unattended

- /Game/Maps/MovingInteriors: the runnable map. One AInteriorsScenario actor
  (it builds the station, the ship, the mech and the avatar at BeginPlay from
  the ruleset's own population) and a light.
- /Game/Maps/ShipInterior: the sub-level the `stream` interior mode streams
  in at boarding — 200 cubes in a corridor. It is authored world-fixed,
  which is the point the stream mode measures.
"""
import unreal

ELL = unreal.EditorLevelLibrary
EAL = unreal.EditorAssetLibrary

cube = unreal.load_asset("/Engine/BasicShapes/Cube")


def new_level(path):
    if EAL.does_asset_exist(path):
        EAL.delete_asset(path)
    assert ELL.new_level(path), path


def save():
    assert ELL.save_current_level()


new_level("/Game/Maps/ShipInterior")
for i in range(200):
    x = 100.0 + (i % 40) * 100.0
    y = (-1.0 if (i // 40) % 2 == 0 else 1.0) * (350.0 + (i // 80) * 20.0)
    z = 50.0 + (i % 3) * 40.0
    a = ELL.spawn_actor_from_class(unreal.StaticMeshActor, unreal.Vector(x, y, z))
    a.static_mesh_component.set_static_mesh(cube)
    a.set_actor_scale3d(unreal.Vector(0.4, 0.4, 0.4))
    a.set_actor_label("InteriorPiece%03d" % i)
save()

new_level("/Game/Maps/MovingInteriors")
scenario = ELL.spawn_actor_from_class(unreal.InteriorsScenario, unreal.Vector(0, 0, 0))
scenario.set_actor_label("InteriorsScenario")
light = ELL.spawn_actor_from_class(unreal.DirectionalLight, unreal.Vector(0, 0, 1000))
light.set_actor_rotation(unreal.Rotator(-50, 30, 0), False)
save()
unreal.log("spike 1045: maps written")
