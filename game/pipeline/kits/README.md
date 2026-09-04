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
