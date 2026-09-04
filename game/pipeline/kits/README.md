# Kit library convention (G12.9–G12.12)

Procured kits, greebles, PBR sets and decals live in the **private asset store** (G12.12, G12.13), never in this repository. This directory holds only the convention and the manifest schema; the manifests themselves are committed here because they are provenance records (hashes and licences), not assets.

## Layout in the private store

    kits/<vendor>/<kit-slug>/<version>/
      LICENSE.*            the licence text as downloaded, verbatim
      source/              the archive(s) exactly as purchased (never modified)
      blender/<kit>.blend  the ingested library: one collection per part, real-world scale, origin at the mount point, +Y forward, Z up
      inserts/             KIT OPS INSERTs derived from the parts (when the kit is used through KIT OPS)
      manifest.json        see schema below

## Manifest schema (`manifest.json`)

    {
      "kit_id": "cgtrader/scifi-greeble-pack-v2",        // vendor/slug, stable
      "version": "2.1",
      "vendor": "CGTrader",
      "order_ref": "<order id, private>",
      "purchased_on": "2026-09-05",
      "license": { "name": "CGTrader Royalty Free", "spdx": null,
                   "redistribute_source": false, "use_in_product": true, "derived_meshes_public": false,
                   "text_sha256": "<sha256 of LICENSE.*>" },
      "source_archives": [ { "file": "source/greeble-pack-v2.zip", "sha256": "<...>", "bytes": 0 } ],
      "formats": ["fbx", "blend", "obj"],
      "textures": { "pbr": true, "sets": ["basecolor","normal","roughness","metallic","ao"], "resolution": 4096 },
      "parts": [ { "id": "panel_vent_01", "tris": 1240, "dims_m": [0.6, 0.4, 0.05], "tags": ["panel","vent","hull"] } ],
      "scale_applied": 1.0,             // factor applied on ingest to reach metres
      "ingested_by": "pipeline/kits/ingest.py@<commit>"
    }

`license.derived_meshes_public=false` routes every asset built from this kit to the private store (G12.10). The manifest is a versioned input (G12.8): a licence change is a new version.

## Ingest steps (to be scripted as `ingest.py`, Blender headless)

1. Unzip `source/` to a scratch dir; hash every file.
2. Import each mesh (FBX/OBJ/blend), apply transforms, convert to metres, set origin to the mount face, orient +Y forward.
3. One collection per part, named `<kit-slug>/<part-id>`; keep the vendor's materials, repath textures to `textures/`.
4. Write `manifest.json` with per-part triangle counts, dimensions and tags (tags from the vendor's names plus a manual pass).
5. Optional: generate KIT OPS INSERTs (one `.blend` per part with the INSERT convention) into `inserts/`.

## What to buy first (escort E-07 and the mothership faction)

- **Hard-surface greeble and panel kit** with PBR textures and Blender-native files: hull plating, vents, conduits, hatches, fasteners.
- **Thruster and engine kit**: nozzles, gimbals, RCS blocks.
- **Cockpit and canopy kit**: frames, glass, interior fittings (the avatar is first-person; interiors are ruleset collision, G10).
- **Landing gear and skids**, **antennae and sensor masts**, **weapon hardpoints and pylons**.
- **PBR material sets**: painted steel (matte, chipped), rubberised polymer, bare aluminium, glossy glass tint (the escort brief's four materials).
- **Decal sheet**: stencils, hull numbers, hazard stripes, crest placeholders.

Prefer kits that ship `.blend` or FBX with separate PBR maps and real-world scale; avoid single-mesh "hero" models, which defeat kitbashing.

## Ingest as run on 2026-09-04

`ingest.py` writes the manifest (hashes, licence facts, todo flag while the licence text is missing); `ingest_parts.py` (Blender headless) adds the per-part inventory. Staged privately under `~/assets/` on the workstation until the bucket exists (G12.13):

| kit | what it is | scale and topology | licence |
|---|---|---|---|
| `cgtrader/spaceship-kitbash-350-a` (model 4691024) | 27 group OBJs, ~6 `Detail_NNN` parts each, 3ds Max export | parts ~25 units across, 100k–300k tris each: subdivided high-poly, meant for baking, needs per-part decimation to a budget | CGTrader Custom, text to be saved |
| `cgtrader/spaceship-kitbash-350-b` (model 4778522) | 24 group OBJs; no file identical to set A, so a second kit, not a duplicate | as above | CGTrader Custom, text to be saved |
| `cgtrader/greeble-cables-pack1` | 42 cable parts on a display grid, one material | metre scale, 98k tris total | Royalty Free, text to be saved |
| `cgtrader/combat-mech-2` (archive `blend.zip`) | one hero mech, 8 meshes, 670k tris, with HDRI and ground plane | reference, not kit parts | unknown, owner to confirm |
| KIT OPS masterfolders (Arch, Bonus, KO-FreeMats, Mega300Tech-v5, SciFi) | 470 INSERT `.blend`s with thumbnails in 20 KPACKs: cutters, tech objects, controls, decals, screens, grids, 63 materials | KIT OPS INSERT convention | KIT OPS / Chipp Walters (Gumroad) |

## Kitbash spike (2026-09-04, evening): bottom-up assembly by script

Scripts, all Blender headless: `kits_to_inserts.py` (part → INSERT: dominant *flat-lying* plane, decimate, kitops props, thumbnail, features, end sockets for aspect ≥ 3.5), `label_inserts.py` (heuristic + Gemini flash-lite vision over thumbnails, stragglers flagged), `explode_blend.py` + `cluster_modules.py` (hero model → islands → proximity modules), `assemble.py` (random faces; the negative result) and `assemble_zones.py` + `zones.json` (the design program).

**Library built:** cables 42 (16 socketed), ship-a 295, ship-b 316, mech 75 modules → **728 INSERTs** in `~/assets/kitops/Orrery_Masterfolder/`, registered in KIT OPS. Labelling cost: about 130 k input tokens per 100 parts on flash-lite, a few cents per kit.

**Findings**
- *Random placement on faces is noise; zones are structure.* The first pass (18 tag-matched INSERTs on random hull faces) produced clutter. With named zones (spine, flank panels, flank vents, nose RCS, wing ribs, wing pylon, fin crest, belly bay) and `connect` runs, every element lands where the callout sheet says. The zone file is the bridge from the callout sheet to geometry and is what the concept stage should emit next.
- *Sockets fix cables.* A cable's largest flat area is its end cap, so surface mounting stood them upright. Choosing the plane the part lies flat on (area share × footprint/height) fixed mounting; end sockets plus `connect` zones make conduit runs span two anchors with the cross-section scaled separately from the length.
- *Curved cables (U-shapes) have aspect < 3.5 and get no sockets yet*; socket detection should use the two ends of the medial curve, not the bbox extremes.
- *The spaceship kits are dense.* Median part 75–94 k tris, decimated to 12 k for INSERTs, 6 k on placement; a real LOD pass per part (or baking to a low-poly proxy) is the next pipeline stage.
- *The mech "kit" yields little.* 621 islands → 75 proximity modules, mostly pipes, brackets and nozzles; one launcher housing. Hero models are not kits.
- *QuadriFlow (from the earlier spike) is not needed here*: kit parts arrive with their own topology; the budget problem is decimation, not retopology.

## Concept → kitbash (2026-09-05): the loop closes

Five scripts, run in order, all headless:

1. `hull_from_mesh.py` — coarsen the image-to-3D mesh into the **mount hull**: lateral axis chosen by mirror error (the escort is nearly square in plan, so extents lie), nose by cross-section, voxel remesh at size/170, decimate to ~14k tris with symmetry, symmetrize, scale to the brief's 9 m, strip textures. Every face gets a **region** `side.long.lat` (top/belly/flank/nose/tail × fore/mid/aft × inner/outer) stored as face attributes; `hull_atlas.json` lists regions by area.
2. `program_from_concept.py` — Gemini Pro reads brief + concept + callout sheet and the atlas, emits **zones.json** against a schema: 14 zones for the escort (spine conduit run, twin thrusters, wing-tip and nose RCS, gun bay, pylons, skids fore/aft, dark wing panels, accent stripe, hull number, crest, canopy hinge, wing-root vents), each naming the concept feature it reproduces. All regions resolved on the hull first time.
3. `choose_parts.py` — per zone, Gemini Flash ranks 8 candidate INSERT thumbnails against the concept (thinking off, 4k output budget; an image-generation model returns no text, and 300 tokens truncates). ~134k input tokens for 14 zones, with a one-line reason each.
4. `assemble_hull.py` — resolve regions to faces, place along the region's principal axis with surface alignment, lift by mount depth, cap any single part at 2.5 m, connect runs between region centroids, role materials, mirror, join, `assembly.json` graph.
5. `blender_render.py` — stills.

**Result:** the first assembly that reads as the concept's craft: silhouette from the hull, thrusters, pods, skids, spine detail in the right places. **Open:** a critique loop (render vs concept, adjust counts/scales), parts that straddle the centreline are not mirrored, part orientation within a zone is only axis-aligned, exposure in the still renderer, and materials/decals from the KIT OPS packs are not applied yet.
