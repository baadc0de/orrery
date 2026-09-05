# #1082 — canonical GUIDs: an unchanged body cooks to zero differing bytes

Implements the owner decision of 2026-09-05 on spike 4's evidence (#1046): canonicalise every GUID,
hash and random seed in a body package so that two independent editor saves of one seed cook to
byte-identical `.umap`/`.uexp`, and an unchanged body therefore patches to **0 bytes under any
strategy**, including UE's file-granular pak patch.

The change is in spike 2's `CookBody` commandlet
(`docs/spikes/1044-one-body-cook/unreal/Source/OneBodyCook/Private/CookBodyCommandlet.cpp`, the
`bCanonicalGuids` block before `SavePackage`). Canonical is now the **default**; `-randomguids`
restores the engine's behaviour, and spike 2's `-deterministicguids` is accepted as a synonym for
the default. Verified the way spike 4 verified: two editor saves, each cooked by the book, diffed
**per file with sha1 and byte offsets** (`scripts/diff-per-file.py`, spike 4's script generalised).

Machine: this Linux box (`fortyninety`), UE 5.8.2-56702186 installed build at `~/UnrealEngine/5.8`,
Linux target, loose cooked files (`bUseZenStore=False`, `bUseIoStore=False` added to the project's
`DefaultGame.ini`, as spike 4 did on the Mac). Every editor run was `UnrealEditor-Cmd … -NullRHI
-unattended` with `WAYLAND_DISPLAY`/`DISPLAY` unset; no window was opened.

## Result

Seed 1, body 2 (spike 2's measured body), cooked for Linux:

| cooked file | size | before (two saves, engine GUIDs) | after (two saves, canonical) |
|---|---|---|---|
| `Body_2.uexp` | 4,082,109 B | sha1 **DIFFER** (db6b68b89af8 / 2c4867e629ef), **275 B in 31 runs** | sha1 **EQUAL** (cdf0740a95b8 / cdf0740a95b8), **0 B, 0 runs** |
| `Body_2.umap` | 27,073 B | sha1 **DIFFER** (b0429c5d931e / f809bf0531ae), **20 B in 1 run** @ 24–43 | sha1 **EQUAL** (a1e5e4422626 / a1e5e4422626), **0 B, 0 runs** |
| `Body_2.ubulk` | — | not emitted on this target (see "Not established") | — |

`results/cooked-same-seed-diff-before.txt` and `results/cooked-same-seed-diff-after.txt` carry the
per-file sha1 and every run with the bytes on both sides. The "before" runs are the same 24 spots
spike 4 measured on the Mac (its 364 B in 24 runs became 275 B in 31 runs here only because Linux's
`FGuid::NewGuid()` shares its first four bytes between two runs seconds apart, so a 16-byte GUID
shows as a 12-byte run, sometimes split). The ruleset halves (`body-2.{tri,hf,vox}.collision`)
were byte-identical before and after, as in spike 2.

Two more sanity checks: a third save with `-seed=2` produces a different package (4,455,448
differing bytes, 18,431 B of length difference) and a different content key
(`results/seed2-distinct.txt`), so canonicalising did not collapse content; and PCG placed the
same 1,972 instances in every seed-1 run.

## What was canonicalised, and where each spot came from

All rewrites happen in `CookBodyCommandlet.cpp` after the authoring actors are destroyed and before
`UPackage::SavePackage`, in object-path order so the seed stream (`FSeedStream`, splitmix64 over
`seed ^ 0x6f6e65626f6479 ^ body<<40`) hands the same value to the same object every run. Every
engine citation below was read before it was repeated.

| spot | count | engine source of the randomness | canonical value |
|---|---|---|---|
| `AActor::ActorGuid`, `ActorInstanceGuid` | 11 actors | `FGuid::NewGuid()` at spawn (spike 2 already seeded these) | seeded |
| `FCompiledSectionBuildInfo::BuildKey` | 1 | `FGuid::NewGuid()` (`WorldPartitionMeshPartitionBuilder.cpp:138`; spike 2 already seeded it) | seeded |
| `FStaticMeshComponentLODInfo::OriginalMapBuildDataId` + derived `MapBuildDataId` | 6 LODs on 8 static mesh components (the section's mesh and PCG's four ISMs carry LOD data) | `FGuid::NewGuid()` the first time a component is saved: `UStaticMeshComponent::PreSave` → `UpdateStaticLightingData` → `CreateMapBuildDataId` (`StaticMeshComponent.cpp:526-547`, `:3664`); `MapBuildDataId = FGuid::Combine(Original, ActorInstanceGuid)` (`:3619-3627`); the cook writes **both** per LOD (`operator<<` for `FStaticMeshComponentLODInfo`, `:3827 ff.`) — these are the six 32-byte runs spike 4 read as "PCG node/pin GUID+hash pairs" | PreSave's creation step is replayed first (`SetLODDataCount`, exported), then `Original` seeded and `MapBuildDataId` recombined with the seeded actor guid |
| `UInstancedStaticMeshComponent::InstancingRandomSeed` | 4 ISMs | `PreSave` replaces it with `FMath::Rand()` whenever it is 0 or equal to the path-derived seed (`InstancedStaticMesh.cpp:5846-5875`) — the four 4-byte `020a0301 xxxxxxxx` runs | seeded, non-zero, ≠ path hash (same CityHash32 as the engine), so PreSave leaves it |
| `UStaticMesh::LightingGuid` | 1 | `SetLightingGuid()` defaults to `FGuid::NewGuid()` (`StaticMesh.h:1486`) | seeded |
| `UBodySetup::BodySetupGuid` on the static mesh | 1 | renewed by `InvalidatePhysicsData()` on every build (`BodySetup.cpp:822`) | **content-derived** (below) |
| `UBodySetup::BodySetupGuid` on the section's `UMeshPartitionCollisionComponent` | 1 | `NewObject` + `FGuid::NewGuid()` (`MeshPartitionCollisionComponent.cpp:229`) | content-derived |
| `ULevel::LevelBuildDataId` | 1 | `FGuid::NewGuid()` in the constructor (`Level.cpp:666`) | seeded |
| package summary saved hash (`.umap` @ 24–43) | 1 | a hash of the saved bytes | follows once everything above is fixed |

**Why the body setups are content-derived, not seed-derived.** The engine says so at
`BodySetup.cpp:819-822`: the GUID is the DDC change indicator for the cooked physics mesh. A GUID
that depends only on the seed would make the DDC hand back stale collision after a code change
that moves a vertex. So the key is
`FGuid::NewDeterministicGuid("orrery/body/<seed>/<body>/<intermediate_hash_mm>/simplified=<0|1>", seed^body<<32)`
combined with a per-body-setup name: two saves of one seed share it, a content change changes it.
`intermediate_hash_mm` is spike 2's hash of the millimetre-quantised intermediate (`cook.json`
`intermediate_hash_mm`).

The canonicalisation pass itself costs 0.1–0.4 ms (`cook.json` `canonical_guids.seconds`); save
time 0.036–0.071 s vs 0.033–0.037 s before (noise at this size); commandlet main 1.06–1.22 s
vs 1.11–1.99 s. Cook-by-the-book wall 6.0 s / 5.9 s after vs 6.0 s / 5.4 s before, zero shaders
compiled in either. **Nothing broke:** the cook, the two-half digest inputs (ruleset halves equal)
and PCG's placement are unchanged.

## The editor-save layer: not zero, and why it cannot be from here

The issue expected the same fix to zero the editor-save layer (spike 2's 5,560 B). It does not,
and the reason is that what spike 2 read as "PCG node/pin GUIDs" in the editor save are not GUIDs
on nodes. Two canonical saves still differ by **3,859 B in 569 runs** (before: 4,161 B / 599 runs
on this box; spike 2's flag alone: 3,996 / 583). `results/editor-save-diff-after.txt`,
attributed with `scripts/residual.py`:

| residue | bytes / runs | source |
|---|---|---|
| FText localisation keys on PCG's `OverridableParamPinTooltip` / pin tooltips (32-hex strings `01A072C4…`) | 3,727 B / 559 runs | every text without the package's namespace is re-keyed with `FGuid::NewGuid()` at save (`TextHistory.cpp:882`, `:892`); editor-only, stripped by the cook (`:865-868`) |
| package `LocalizationId` in the summary and on the text namespaces | 2 × 31 B | the package localisation namespace, a fresh GUID per save |
| `UPackage::PersistentGuid` | 12 B | `Package.cpp:160`; dropped from cooked summaries |
| `DateModified` asset-registry tag | 2 B | `FDateTime::Now()` at save (`World.cpp:10318-10319`) |
| three `FEditorBulkData` ids (`EditorBulkData.cpp:554`) | 3 × 12 B | the virtualised-payload identifiers of the mesh description (@3,990,091, beside `PolygonGroups`) and, by position, two more editor-only bulk payloads in the static-mesh/section export region (@311,987, @312,127) |
| package summary saved hash | 20 B | follows the above |

`DateModified` alone means the editor layer cannot reach zero without an engine change; the FText
keys would need every tooltip pre-keyed deterministically; the bulk-data ids have no setter. None
of this reaches the cooked output, which is what ships, so the decision's payoff — an unchanged
body is 0 bytes under UE's own pak patching — holds, and the "both layers" expectation is
corrected here rather than pursued.

## Reproduction (this box)

```sh
# stage spike 2's project and build the editor module (27 s)
cp -r docs/spikes/1044-one-body-cook/unreal ~/Development/orrery-onebody/OneBodyCook
~/UnrealEngine/5.8/Engine/Build/BatchFiles/Linux/Build.sh OneBodyCookEditor Linux Development \
  -Project=$HOME/Development/orrery-onebody/OneBodyCook/OneBodyCook.uproject -WaitMutex -NoHotReload
# two saves with engine GUIDs (-randomguids), two canonical; per-file editor-save diffs
cd docs/spikes/1082-canonical-guids/scripts && bash baseline.sh      # out-a/b: add -randomguids to ARGS to reproduce "before"
bash after.sh                                                          # out-e/f canonical, then cook-pair.sh cooks both and diffs
```

`cookbody-linux.sh` is spike 2's `cook.sh` for this box; `cook-linux.sh` is spike 4's cook step
for the Linux target. Both kill the `CrashReportClient` spike 4 warned about after every run.
Take the `unreal-editor` lane lease first; the scripts never run two editor instances.

## Not established

* **`.ubulk` on Linux.** The Linux cook in this installed build targets `VULKAN_SM5` by default and
  emits no separate `.ubulk` (Nanite bulk data): the cooked body is `.umap` + `.uexp` only
  (4,082,109 + 27,073 B against the Mac's 4,120,165 + 27,118 + 2,131,620 B). Spike 4 measured the
  Mac `.ubulk` as already sha1-equal between saves, and nothing canonicalised here touches Nanite
  data, but the after-state of a Nanite `.ubulk` was not re-measured on this box. A Vulkan SM6
  cook (`TargetedRHIs=SF_VULKAN_SM6`) would show it and was not run: it needs a shader-compilation
  pass this box has not paid.
* The commandlet still trips spike 4's shutdown ensure (`WorldSubsystem.cpp:118`) and exits 139
  **after** the save; every save above completed and was cooked. Not fixed here (out of scope).
* Only seed 1 body 2 was cooked twice; the seed-2 run checks distinctness, not a second zero.
* Whether a body saved on the Mac and one saved here cook to the same bytes (cross-OS
  determinism) — as in spikes 2 and 4, not measured.
