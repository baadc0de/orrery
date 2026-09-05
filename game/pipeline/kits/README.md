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

## Critique loop, mirroring, density (2026-09-05, night)

`critique.py` + `iterate.sh`: assemble → render → Gemini Pro judges hero/side/top against the concept per zone → up to 8 adjustments (scale, count, move, drop, add) applied to the program → parts re-chosen → repeat. Every critique is appended to `zones.json` as provenance. `assemble_hull.py` gained a bisected mirror for centreline parts, a triangle budget proportional to placed size (250–6000), palette materials by zone role, and an extruded "E-07" text decal per flank.

**What the loop taught, in order:**
1. *Undamped adjustments oscillate.* Round one doubled what round two halved; two drops per pass and a square-root damping on scale factors fixed the swing.
2. *Material bugs read as design bugs to the critic.* A strength-6 emissive on every thruster-tagged part rendered as white-blue discs; the critic dropped good zones because of it. Emissive is now a faint glow on the main-thruster zone only.
3. *Relative sizes cannot converge.* With sizes as fractions of a region slot, "make it 2× bigger" meant a different thing per zone and scores sat at 0.3–0.4 across four rounds. Restating the program in **metres** (the extractor reads the callout's dimensions; the assembler scales to `size_m`; the critic nudges metres) lifted the first critique to **0.75**, and its content changed from "everything is the wrong size" to configuration notes (two nose RCS, two rear skids, bigger wing-tip pods).
4. *Part versus paint.* Stripes, hull numbers, crests and colour panels are paint in the concept; asking geometry to represent them fails every time. The program now classifies `kind` and paint zones go to the texture stage.
5. *What remains is part choice.* The critic's last complaints were a spike chosen as a skid and a hinge block over the canopy: the visual chooser needs the zone's size and the concept crop of that feature, not the whole concept.

Records: `out/concept2kit-v2/zones.iter*.json`, `zones.json` (with critiques), `choices.json`, `assembly.json`. Site: the "Kitbash zones" toggle shows the round-five result.

## Automatism pass (2026-09-05, morning): what the loop needed before it could learn

`iterate.py` replaces `iterate.sh`. Per pass: assemble → shaded renders (Cycles GPU, 48 samples, ~2 s a view) **plus flat zone-id renders** (each zone one named colour, legend in `assembly.json`) → critic → apply → re-choose. Scores: v2 program 0.35–0.4 under the stricter per-zone critic, then 0.7 once placement was fixed; the sheet-based chooser run is pending gcloud re-login.

**Found and fixed, in order:**
1. *The critic was blind.* Renders were lit by a mid-grey world at strength 1 with +0.9 exposure: a white blob on grey. Now the backdrop is a camera-ray-only Light Path branch, lamps do the lighting, exposure 0, and Cycles gives real shadows headless (EEVEE Next needs a display for its shadow maps). Zone-id renders let the critic name the blob it is judging.
2. *Belly parts sat on the top skin.* The surface walk searched every hull face nearest the region centroid; a curved region's centroid lies inside the hull, nearer the opposite skin. Now it searches the region's own faces and an **extremity filter** keeps only faces within 0.35 m of the side's outer skin, because the coarse hull labels downward-facing overhangs (canopy lip, nose block underside at z = +1.2) as belly.
3. *The region atlas cannot place anything laterally.* `outer` holds 48 m² and `inner` under 2 m², so every aft zone resolved to the wingtips and the main engines went on the wings each pass. Zones now take an **anchor** `[x, y]` in fractions of the hull half-extents (flank: `[y, z]`); the critic moves with anchors, using the top and belly renders as the map, and even counts become mirrored pairs (the anchor is the +x member).
4. *Hill-climbing on the critic's score is a random walk.* The same program scored 0.7, 0.6, 0.6; with a 0.05 tolerance every pass "regressed" and its adjustments were discarded, so nothing was ever applied. The driver now accepts every pass and keeps the best snapshot only for the final output.
5. *The chooser could not find what the critic asked for.* Every pass asked for the same four silhouettes (four-port nose block, ski skids, long spine pipes, round nozzles) and got eight random tag matches. The chooser now sees a **concept crop** per zone (boxes from one vision call, cached), the size in metres, the region and the critic's hint; the pool widens by neighbour tags and label notes when a hint exists; up to 72 candidates go through numbered **contact sheets** (ImageMagick montage, explicit font or the numbers render as tofu) for a shortlist of 8, then the thumbnail ranking as before. Rejected inserts are excluded per zone; when exclusions exhaust the tag pool the whole library opens.
6. *The coarse hull already carries the big features.* Engine pods, skids and the nose block are lumps in the TRELLIS hull, so parts duplicate them; the critic is told to drop duplicates or scale a part to cover its lump. Carving lumps out of the hull is the real fix and is open.

Costs of the loop, per pass: one Pro call with 9 images (~25 k input tokens), one Flash call per part zone with sheets + crop + 8 thumbnails (~15 k), one box-locate call cached per run. Records: `out/concept2kit-v3/`.

## Primitives, subassembly sheets, generated INSERTs (2026-09-05, midday)

Best score 0.7 → 0.75 with `iterate.py` accepting every pass; then two structural additions.

- **Procedural primitives the critic can request.** The critic asked for the same four silhouettes every pass (four-port nose block, ski skids, long spine pipes, round nozzles) and the library has none of the first three (no `landing-gear` tag in 728 INSERTs). `assemble_hull.py` now builds `skid`, `nozzle`, `box` and `pipe_run` in Blender when a zone carries `prim`; the critic sets it on a replace or add. Unprompted, it made both skids, the nozzles and the spine run procedural in one run. Lessons from the first render: build primitives on the +z side of the mount plane (the skid went into the hull), keep their axes true (+y along the hull, nozzle axis = mount normal, tail normal forced to −y), sink most of the nozzle into the pod lump with only the inner cone emissive, sample pipe heights by ray cast per pipe (face-centre snapping braided them) and exempt the run from the mirror (it is symmetric by construction).
- **Subassembly sheets (owner steering, G12.16).** `subassembly_sheets.py` asks the concept model for one isolated three-view sheet per part zone at the program's size; the six for the escort read unambiguously (the skid, the four-port block, the engine pod). The chooser now judges against the sheet instead of a crop of the painting.
- **Sheet → TRELLIS → INSERT.** `sheet_to_insert.py` crops the 3/4 view, runs image-to-3D (about 2–4 min a part on this box), stages the glb under `~/assets/kits/generated/<asset>/1/` with a manifest, converts it into the `gen-<asset>` kpack and labels it from the zone (confidence 1.0, so it sorts first in the pool). The generated skid reads as a skid; the nose block is blobby at 12 k tris but has its ports.
- **Mount audition.** The dominant-plane rule mounts a generated skid by the ski's underside. `audition_mount.py` renders the part on a plate in its six bounding-box orientations and lets the vision model pick the one that sits like the sheet, then rewrites the INSERT (mount at z=0, features updated). It picked the bolt plate for the skid and the flat base for the block and nozzle. This applies to stragglers from purchased kits too.

Open: the coarse hull still carries lumps for the features the parts reproduce (the critic drops duplicates, it cannot carve); primitives could take the subassembly sheet as a parameter source (strut count, ski length) instead of fixed ratios; a generated part labelled with a zone's tags leaks into neighbouring zones through neighbour-tag widening (the skid was offered for pylons).

## More avenues (2026-09-05, afternoon)

Six passes with four changes at once, scored on a blended metric (half the critic's number, half its own fraction of "good" part zones): 0.61, 0.63, 0.55, 0.45, 0.61, 0.56. The critic-only numbers were 0.65–0.75, the same band as before: **the loop has plateaued** at what the coarse hull and a noisy critic allow.

- **Carving hull lumps** (`carve`, default for skid and box primitives, critic-settable): ring-median skin height around the mount point; a lump is part-sized, surrounded by a consistent base (interquartile spread under 0.15 m) and never on the nose or tail taper. Two failure modes were found the hard way: the nose taper read as a 1.6 m lump and the nose was chopped; a belly skid at the wing root saw the wing underside on half its ring and sank into a recess. After a carve the skid is rebuilt so the ski hangs where the lump surface was.
- **Blended score.** The critic's number varies by 0.15 on an unchanged program; its per-zone verdicts are steadier but count paint zones as "missing" unless excluded. The driver now stores which metric its best snapshot used, because a 0.75 on the old scale silently blocked every update on the new one.
- **Mirror-aware counts.** A count-2 zone on a flank region placed both copies along the wing (one at the nose) before the mirror doubled them; off-centre zones now place half the count.
- **Generated parts stay with their zone.** The generated skid was offered for the pylons through neighbour-tag widening; a generated INSERT's label carries its zone and only that zone may pick it.
- **Hull hole fill.** Boundary loops of the voxel-remeshed hull are filled before mounting; the dark slits the critic read as vents are mostly gone.
- **Negative: corrective smoothing of the hull** melts it into a blob with holes and breaks the boolean. The hull quality has to come from upstream (a finer remesh or the retopo mesh), not from smoothing.

What would move the score next, in order: a better mount hull (the retopo mesh, or the TRELLIS mesh at size/300 instead of size/170); a critic ensemble (two calls, verdicts intersected) to cut the noise; letting the critic see the subassembly sheets; primitives parameterised from the sheets.

## Inverted flow: constructible concepts from an input palette (2026-09-05, evening)

Owner steering: learn on something simple, the other way round. Give the concept artist a **palette of parts** as input, ask for a simple prop built only from those, then reconstruct it. Scripts in `kits/prop/`: `palette.py` (8 library INSERTs + 4 straight primitives, numbered sheet), `concept.py` (constructible concept with palette-number callouts, then a **build list** in metres), `assemble.py` (free-space placement, canonical part frames), `critique.py` and `iterate.py`.

First prop: a deck railing section. The concept came back constructible and annotated on the first try (posts from the strip frame, handrail from the dual pipes, mid rail from the tube, brackets, grille housings, diagonal braces from the pipe-with-bracket). Scores 0.75, 0.71, 0.8, 0.6 over four passes; the best pass plus two owner corrections reads as the concept.

**What this simple case taught, that the ship hid:**
- *Orientation semantics must be axis-based, not Euler.* The first build list rotated the handrail "about y" to lay it along x, which does nothing. Every part is now re-expressed in a canonical frame (longest extent +y, thinnest +z) and the build list says `along` (world axis of the long extent), `spin_deg` about it and `tilt_deg` in the xz plane. After that, the build list placed 18 parts into a recognisable railing at the first assembly.
- *The critic's edits land when the actions are geometric.* Move by delta, spin, tilt, scale, swap, remove, add: each pass's requested edits were visible in the next render (handrail spun flat, braces tilted, housings moved outside the posts).
- *The critic still misreads chirality.* It flipped the handrail's curl the wrong way and put the brace V at the bottom; the owner caught both in one look. Human critique is recorded in the build list (`human_critique`) as a stage of its own, per G12's provenance rule.
- *Pro-model thinking eats the output budget.* An 8 k output cap truncated the critic's JSON at 800 characters; 24 k with the finish reason logged fixed it.

Next on this track: a ladder and a crate from the same palette, then a palette drawn from one purchased kit only, then the same loop with the subassembly sheets of the escort as "concepts" (which closes the circle back to the ship).
