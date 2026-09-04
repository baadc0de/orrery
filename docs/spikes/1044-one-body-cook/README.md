# Spike #1044 — one-body cook: numbers, artefacts, reproduction

Research spike (#1042, spike 2). Output is a commandlet that runs, agreement
numbers, cook time and package size per representation. It settles nothing:
G10.2/G10.4 stay open, D52/D53 stay Proposed. Nothing here merges.

Machine for every number below: Apple M1 Max (10 cores), macOS 26.6.2, Unreal
Engine 5.8.2-56702186 (installed build), Rust 1.96.0 `aarch64-apple-darwin`.
Spike branch `spike/1044-one-body-cook`. Raw JSON for every table is in
`results/` (`out/` = 64 m smoke body, `out-256a` = the measured body,
`out-256s` = Unreal collision simplification on, `out-256f` = 250 mm
heightfield/voxel grids).

## What runs

* `unreal/` — a UE 5.8 editor module with two commandlets.
  * `CookBody -seed=<u64> -body=<id> -out=<dir> [-size=256 -spacing=1 -density=0.03 -hfcell=500 -voxedge=500 -nonanite -simplifycollision -deterministicguids]`
    builds one Mesh Terrain body (a 256 m base plane at 1 m spacing plus two
    seed-driven fBM noise modifiers) through the Mesh Partition editor
    pipeline, runs PCG scatter at cook time, saves `Body_<id>.umap` and writes
    `body-<id>.{tri,hf,vox}.collision` plus `body-<id>.cook.json`.
  * `TraceBody -map=… -rays=… -out=… -complex=0|1` loads the saved map in a
    fresh process and traces the ray file with `UWorld::LineTraceSingleByChannel`
    (ECC_WorldStatic), recording per ray which component class answered.
* `crates/onebody_collision_trace` — `collision-trace rays|trace|compare|digest|sizes`
  (see its README): the D43-conformant integer ray test per representation,
  the seeded ray generator, the comparator, the two-half digest.
* `cook.sh`, `pipeline.sh`, `build.sh` — the drivers used for every number here.

### The cook path hooked

Mesh Terrain is the `MeshPartition` plugin. Its builder
(`Engine/Plugins/Experimental/MeshPartition/Source/MeshPartitionEditor/Private/WorldPartitionMeshPartitionBuilder.cpp`)
turns the modifier stack into one `MeshPartition::FMeshData`
(`ETaskState::ModifiersProcessed`), spawns compiled sections
(`PrepareCompiledSections`), wraps the mesh in a `FTransformerUnit`
(`MakeTransformerUnit`, line 897 of that file) and launches the definition's
transformer pipeline (`UMeshPartitionEditorComponent::LaunchTransformers`,
`MeshPartitionEditorComponent.cpp:609-658`), which runs each `FTransformer`
as a task with the previous one as prerequisite. The commandlet drives those
same entry points directly (no World Partition), with the pipeline
`[FStaticMeshTransformer (Nanite), FCollisionTransformer, FOrreryExportTransformer]`.

**The one intermediate exists and both halves derive from it.** All three
transformers receive the identical `FTransformerContext`, whose
`TransformerUnits[0].MeshData` is one `TSharedPtr<const FMeshData>`
(`MeshPartitionTransformer.h:26-48`). `FStaticMeshTransformer` builds the
Nanite static mesh from it (`EditorUtils::BuildSourceModel`), `FCollisionTransformer`
builds Unreal's Chaos trimesh from it (`MeshPartitionCollisionTransformer.cpp:287`
→ `Collision::ConvertMeshToCollisionData`), and our transformer captures the
same pointer (`cook.json: export_saw_same_meshdata_pointer_as_static_mesh_and_collision = true`).
Unreal's collision vertices are that mesh's doubles cast to `float`
(`MeshPartitionCollisionGeneration.cpp:294`): measured max deviation
0.0012 mm over 66 049 vertices, identical triangle list, 13 vertices round to a
different millimetre. With `bSimplifyCollision` (off by default) the collision
transformer QEM-simplifies (`ErrorTolerance` 10 cm) and the triangle list is
no longer the intermediate's — that is the G10 "collision coarser than render"
case, measured below as `out-256s`.

PCG's scatter samples the section's *collision* (`WorldRayHitQuery`,
`bTraceComplex`), so the scatter itself depends on which collision Unreal
cooked: the simplified run placed the same 1972 instances at slightly
different heights (1 645 505 vs 1 645 899 shell voxels).

## The measured body

seed 1, body 2: 256 m × 256 m base at 1 m spacing → intermediate 66 049
verts / 131 072 tris; fBM standard layer (intensity 13.6 m, wavelength 71.7 m,
5 octaves) plus ridge layer (3.9 m, 15.9 m, 3 octaves); z range −47 m…+137 m
on the smoke body; PCG surface sampler at 0.03 pts/m² with random yaw and
uniform scale 0.6–3.0 over engine BasicShapes (Cube ×4, Cylinder ×2, Cone ×2,
Sphere ×1 weights) → 1972 instances, 591 692 instance triangles. ISM
descriptors: `BlockAll`, `QueryAndPhysics`, Static; static-mesh trace flag
`CTF_UseDefault` (simple collision = box / convex hulls, complex = triangles).

Rays: N = 5000, `rand_chacha` seed 42, origins uniform over the body's XY
bounds at surface − 2 m … + 200 m, directions uniform on the sphere, 500 m
long. 1000 of the 5000 rays hit anything on either side; the other 4000 miss
on both and agree trivially, so both tables are given.

## Agreement — Unreal line trace (bTraceComplex = 1) vs the ruleset

`agree(ray) = both miss ∨ (both hit ∧ |d_U − d_R| ≤ τ)`; `rate` over all 5000
rays, `cond` over the 1000 rays where at least one side hit.

| representation | rate(10) | rate(50) | rate(250) | rate(1000) | cond(10) | cond(50) | cond(250) | cond(1000) | hit/miss disagreements | different actor | p50 / p99 / max |Δd| mm |
|---|---|---|---|---|---|---|---|---|---|---|---|
| **tri** (lattice triangles) | 1.0000 | 1.0000 | 1.0000 | 1.0000 | 1.000 | 1.000 | 1.000 | 1.000 | 0 | 0 | 0 / 1 / 3 |
| tri, Unreal collision QEM-simplified (`out-256s`) | 0.9788 | 0.9964 | 1.0000 | 1.0000 | 0.894 | 0.982 | 1.000 | 1.000 | 0 | 0 | 0 / 69 / 168 |
| tri vs Unreal *simple* collision (bTraceComplex = 0) | 0.9990 | 1.0000 | 1.0000 | 1.0000 | 0.995 | 1.000 | 1.000 | 1.000 | 0 | 0 | 0 / 4 / 44 |
| **hf** 500 mm + 26-DOP prisms | 0.8764 | 0.9342 | 0.9760 | 0.9934 | 0.383 | 0.672 | 0.880 | 0.967 | 7 | 55 | 22 / 2416 / 11462 |
| hf 250 mm + 26-DOP prisms (`out-256f`) | 0.9322 | 0.9604 | 0.9780 | 0.9942 | 0.662 | 0.802 | 0.890 | 0.971 | 4 | 54 | 4 / 2416 / 11462 |
| **vox** 500 mm | 0.8004 | 0.8232 | 0.9066 | 0.9736 | 0.024 | 0.136 | 0.544 | 0.871 | 26 | 122 | 219 / 5719 / 59355 |
| vox 250 mm (`out-256f`) | 0.8074 | 0.8534 | 0.9580 | 0.9850 | 0.055 | 0.281 | 0.794 | 0.926 | 20 | 92 | 96 / 4765 / 59355 |

|Δd| histogram for **tri** (1000 both-hit rays): 764 at 0 mm, 231 at 1 mm, 4 at
2 mm, 1 at 3 mm. The 3 mm ray (2551) is at 217.6 m range: it is the float32
cast of the collision vertices plus Chaos's own float arithmetic, not the
representation.

What a disagreement means physically, by class (from `compare-*.json` `worst`):

* **tri**: nothing beyond millimetre rounding. On the 64 m smoke body the only
  disagreements (3 of 5000) were rays whose origin lay *inside* a rock:
  Unreal's complex trace passes through the rock it starts in (backface) and
  hits the terrain behind, or misses; the ruleset's two-sided test reports the
  rock's inner face. Unreal's simple trace reports those as
  `bStartPenetrating` at 0 mm.
* **hf**: the 55 "different actor" rays are the 26-DOP prisms being fatter than
  the rocks (a prism is hit where Unreal hits terrain or a neighbouring rock);
  the ≥ 1 m terrain disagreements are grazing rays over ridges the height grid
  under-samples (p99 2.4 m at 500 mm and still 2.4 m at 250 mm — the ridge
  layer's 15.9 m wavelength is what the grid loses, not the cell size).
* **vox**: p50 is a fifth of the edge (219 mm at 500, 96 mm at 250); 46–50 rays
  start under the terrain and a solid representation answers 0 mm where a
  surface answers the exit distance; shell voxels around rocks answer a rock
  where Unreal answers terrain (92–122 rays).

Unreal's ISM instances did not collide at all until the trace commandlet
waited for static-mesh compilation (`UInstancedStaticMeshComponent::ShouldCreatePhysicsState`,
`InstancedStaticMesh.cpp:4735`, refuses while the mesh is compiling): the first
run reported 0 ISM hits and 38 "different actor" rays on the smoke body. That
is the confounder the issue names, observed and fixed, not a property of PCG.

## Cost per body (256 m body, `out-256a`)

Commandlet main: 3.31 s (modifiers → FMeshData 0.03 s; transformers 0.46 s +
1.45 s async static-mesh/Nanite build; PCG 0.27 s; save 0.10 s). Process wall
time with `-NullRHI`: 16–18 s, of which ~12 s is engine start. Without
`-NullRHI` the first run compiled shaders for 5 minutes.

| package | write s | bytes | zstd −19 bytes | ruleset load+build s | ruleset trace 5000 rays s |
|---|---|---|---|---|---|
| `Body_2.umap` (Nanite static mesh, Chaos trimesh, 4 ISM components) | 0.10 | 6 024 567 | 2 052 360 | — | 0.006 (Unreal, after 0.24 s load) |
| `tri.collision` (131 072 terrain + 591 692 flattened instance tris) | 0.035 | 19 821 904 | 2 282 090 | 0.30 | 0.028 |
| `hf.collision` 500 mm + 1972 prisms | 0.37 | 1 483 568 | 595 406 | 0.001 | 0.084 |
| `hf.collision` 250 mm | 1.36 | 4 699 308 | 1 747 117 | 0.003 | 0.34 |
| `vox.collision` 500 mm (column RLE) | 0.51 | 2 736 210 | 129 090 | 0.005 | 0.007 |
| `vox.collision` 250 mm | 1.76 | 11 186 368 | 514 881 | 0.028 | 0.018 |

The tri package is flattened per instance on purpose (all-integer file, no
transform arithmetic on the ruleset side); 95 % of its bytes are the 1972 rock
copies. Instancing the four meshes would put it near the terrain's 3.7 MB raw
/ ~0.5 MB zstd — that is an obvious follow-up, not measured here.

## Digest and determinism

`collision-trace digest` reads both halves back from disk in a separate process,
blake3 over `(len, bytes)` of each in order; flipping one byte in the middle of
any half changes the digest (`digest-2.json`, `flip_check_passed: true`).

Two cooks of seed 1 on this machine (`out-256a`, `out-256b`): the three
collision packages are byte-identical (blake3 equal), the **`.umap` is not**:
5560 bytes differ in 195 runs, mostly 32-byte hashes/GUID strings — the
package summary hash at offset 519 and PCG node/pin GUIDs inside the saved
graph. Seeding the actor GUIDs (`-deterministicguids`, `out-256c/d`) removes 246
bytes and leaves 5314. So: the *content* the cook produces is a function of
the seed (the same 1972 instances, the same triangles, the same collision
packages), but Unreal's package bytes carry authoring-time GUIDs and a save
hash that are not. G10.4's digest either canonicalises those (a list of ~200
spots per body) or covers the Unreal half by a content digest rather than
file bytes.

**Not established:** two OSes. This spike ran on macOS only (the Linux box
was reserved, and has no Unreal); Windows/Linux cook determinism, and D43's
Windows-vs-Linux bit-identity of the ruleset side, are asserted by construction
(integer-only) and not measured.

## Recommendation

**Lattice triangles (`tri`), derived from the intermediate before any
Unreal-only simplification, with Unreal's collision transformer left
unsimplified.** It is the only row that reaches a high rate at τ ≤ 250 mm
without caveats: 1000/1000 hitting rays within 3 mm, zero hit/miss
disagreements, zero "different actor" rays, and its p99 is 1 mm — an avatar
capsule is 300–500 mm wide, so this representation is two orders of magnitude
inside the smallest tolerance the owner could pick. Its costs are the worst
of the three (0.3 s to build a BVH per body on load, 2.3 MB zstd per body
flattened, ~0.5 MB if instanced) but they are seconds and megabytes, not the
hours or gigabytes that would change spike 4's verdict. The heightfield is
the right *shape* for terrain (0.6 MB zstd, 0.08 s traces) but fails on the
ridge layer at both cell sizes (p99 2.4 m) and its prisms answer for rocks
that are not there; voxels at any affordable edge are a different fact from
Unreal's surface (p50 ≥ 96 mm, solid-vs-surface semantics for rays that start
underground). If the owner later turns on Unreal's collision simplification,
the ruleset package must be taken from Unreal's `FTriMeshCollisionData` after
simplification rather than the pre-simplification intermediate, or hit
registration inherits a 69 mm p99 / 168 mm max gap (`out-256s`) it never sees.

## Reproduction (MacBook `bojans-max`)

```sh
# Unreal half: build the editor module (UE 5.8.2 installed build, Xcode 26.2)
"/Users/Shared/Epic Games/UE_5.8/Engine/Build/BatchFiles/Mac/Build.sh" OneBodyCookEditor Mac Development \
  -Project="$HOME/Development/orrery-onebody/OneBodyCook/OneBodyCook.uproject" -WaitMutex -NoHotReload

# Ruleset half (rustup 1.96.0 via rust-toolchain.toml; the crate is staged in a wrapper workspace on the Mac)
cargo build --release -p onebody_collision_trace

# One body, both halves (cook.sh wraps UnrealEditor-Cmd -run=CookBody ... -NullRHI)
OUT=$HOME/Development/orrery-onebody/out-256a ARGS="-seed=1 -body=2 -size=256 -spacing=1 -density=0.03" bash cook.sh
# rays -> TraceBody (complex 1 and 0) -> ruleset trace per rep -> compare -> digest -> sizes
OUT=$HOME/Development/orrery-onebody/out-256a BODY=2 N=5000 RAYSEED=42 bash pipeline.sh
```

The `Engine/Source/Runtime/Engine/Internal` include path in
`OneBodyCook.Build.cs` is a spike workaround: `MeshPartitionCompiledSection.h`
(a Public header of the Experimental plugin) includes an Engine-internal
header. `FStaticMeshTransformer`/`FCollisionTransformer` are instantiated by
reflection because their headers pull a Private plugin header along and their
vtables are not exported from the plugin on Mac.
