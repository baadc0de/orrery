# Spike #1046 — season size, patch bytes, full-system cook: measured fits from spike 2's body

Research spike (#1042, spike 4). Output: three numbers with the measured fit under each, and the
v1 answer to G2's "dozens of bodies". It settles G2 **as a v1 quantity** only; D52/D53 stay
Proposed, ADR-0021 still has no content-package clause, and nothing here merges.

Every number is tagged **[measured]** (a file or log on this branch produced it) or
**[extrapolated]** (arithmetic over measured points, with the assumption named). Machine for every
measurement: Apple M1 Max, 10 cores, 64 GB, macOS 26.6.2, UE 5.8.2-56702186 (installed build),
spike 2's `OneBodyCook` project and `collision-trace` binary on `bojans-max`. Raw outputs in
`results/`; the scripts that produced them in `scripts/`; the one new commandlet in `unreal/`.

## The three numbers

| | value | fit / error bar | status |
|---|---|---|---|
| **Gigabytes per season** | **4.47 MB per 256 m tile** (umap 2.06 + `tri` 2.41, zstd −19); interiors add ≤ 0.53 MB each | `size(n) = a + b·n` over n = 1..8 tiles: a ≈ 0 (−62 kB, 47 kB), b = 2,060,629 + 2,408,570 B, max residual 0.95 %; per-tile sd across 16 tiles 1.1 % (umap) / 5.5 % (tri) | [measured] per tile; season(N) is [extrapolated] and depends on **tiles per body**, which no G-number fixes — see §3 |
| **Patch bytes between seasons** | **≈ 94 % of a fresh download** between two seeds (29.6 MB patch vs 31.5 MB full, N = 8); **≈ 5 kB per unchanged body** with `.umap` nondeterminism under a content-agnostic patcher, **2.2 MB per unchanged body** under UE's file-granular pak patch | zstd `--patch-from`, xdelta3, bsdiff and UnrealPak all reported (§4) | [measured] |
| **Full-system cook seconds** | **16.85 s per body-process** sequential = 11.6 s engine start (fixed per process) + 3.12 s commandlet main (marginal) + ~2 s exit; **6.4 s per body amortised at 8 concurrent processes**, core-bound | `cook(n) = c + d·n`: c = −0.2 s, d = 16.85 s, max residual 0.35 s over n = 1..8 | [measured] per tile; cook(N) [extrapolated] |

**The v1 answer (§8):** "dozens of bodies" is a v1 quantity **provided a body's cooked landable
surface is a landing region, not a planet**. At spike 2's representation (1 m lattice, PCG rocks at
0.03/m², flattened rock triangles) the anchors are reached at **4,475 / 11,188 / 22,375 tiles**
(20 / 50 / 100 GB), i.e. 48 bodies clear 20 GB at up to ~93 tiles each (a ~2.4 km × 2.4 km region)
and clear 50 GB at ~233 tiles each (~3.9 km square). A whole planet at this spacing is not a v1
quantity at any anchor. The season jump is a full download, not a patch.

## 1. What was run

* **Bodies.** Spike 2's `CookBody` commandlet, one process per body, `-NullRHI`, 256 m at 1 m
  spacing, density 0.03/m² — the measured body of #1044. Two season seeds, eight body ids each
  (`s1` = seed 1001, `s2` = seed 2002, bodies 11–18): `scripts/../results/per-body.json`,
  `results/fit-cook-sizes-pass.txt`. A separate timing pass (`scripts/timing-cook.sh`,
  `results/timing-cook.txt`) cooked eight more bodies sequentially, then 2, 4 and 8 concurrently.
* **Interiors.** A new `MeasureLevel` commandlet (`unreal/MeasureLevelCommandlet.cpp`, added to
  spike 2's editor module on the Mac) loads a level, walks every colliding static-mesh component,
  and writes spike 2's `tri` package for it. Run on two hand-authored levels from Epic's UE 5.8
  FirstPerson template: `Variant_Horror/Lvl_Horror` (corridors and rooms from LevelPrototyping
  cubes, doors, 87 actors) and `FirstPerson/Lvl_FirstPerson` (the open arena, 68 actors).
  `results/level-1-horror.measure.json`, `results/level-2-firstperson.measure.json`.
* **Sizes, containers, patches.** `scripts/measure-sizes.sh` → `results/measure-sizes.out`,
  `results/same-seed-pak-and-rss.txt`.
* **One transfer.** `scripts/transfer-origin.sh` (Mac, `python -m http.server`) and
  `scripts/transfer-client.sh` (this Linux box) → `results/transfer-verify.json`.
* **Fits.** `scripts/fit.py` → `results/fit.txt`.

### Confounders found and handled

* **Spike 2's commandlet trips an ensure at shutdown** (`WorldSubsystem.cpp:118`, `!bInitialized`)
  and leaves a `CrashReportClient` spinning at ~45 % CPU per cook. Fifteen of them from spike 2's
  runs were still alive when this spike started; the first eight cooks of the size pass climbed
  from 23.5 s to 29.2 s wall for that reason (`results/fit-cook-sizes-pass.txt`) and dropped to
  16.6 s the moment they were killed. The timing pass kills the reporter after every cook. **Only
  the timing pass is quoted for cook seconds**; the size pass is quoted for bytes only.
  `mediaanalysisd` at 216 % CPU was also running on the Mac and was killed.
* **Cooked-bytes for the Unreal half could not be produced.** `-run=cook -targetplatform=Mac` ran
  617 s and died in Metal shader compilation: `cannot execute tool 'metal' due to missing Metal
  Toolchain` (Xcode 26 ships it as a separate download; the coordinator's install attempt failed
  on the machine's Xcode plugin loading and is with the owner). The Mac installed build carries no
  Windows or Linux target platform (`Available = { IOS, ..., Mac, ... VisionOS }`), so no
  cross-cook was possible either. **Every Unreal-half byte below is the editor-saved `.umap`
  (`UPackage::SavePackage`), the same thing spike 2 reported, not a cooked package.** A cooked
  package drops editor-only data and re-serialises bulk data; the Nanite/collision bulk data that
  dominates the 6 MB will still be there, but the number is unmeasured, not estimated. What it
  would take: the Metal toolchain on the Mac, or a Linux-target cook on the Linux box's UE install
  once spike #1045 releases it. A Linux-target cook would change the umap column by whatever the
  cook strips or adds; it would not change the verdict, because the anchors sit at thousands of
  tiles and the `tri` half (which is exactly what ships) is already half the bytes.
* **UE 5.8 defaults `bUseZenStore=True`** (`Engine/Config/BaseGame.ini:95`): a successful cook
  would have written to zenserver, not `Saved/Cooked`. `bUseZenStore=False` was set for the
  retries; moot, given the above.

## 2. size(n): the fit [measured]

Per-tile file sizes (`results/per-body.json`, 16 tiles over two seeds). fs = bytes on disk;
zstd19 = `zstd -19` of the raw bytes.

| file | mean fs | sd | mean zstd19 | sd | min–max zstd19 |
|---|---|---|---|---|---|
| `Body_N.umap` (editor-saved: Nanite static mesh, Chaos trimesh, 4 ISM components) | 6,030,905 | 11,596 | 2,056,752 | 22,813 | 1,989,933 – 2,097,442 |
| `body-N.tri.collision` (131,072 terrain + ~592 k flattened rock tris) | 20,147,572 | 373,351 | 2,386,888 | 131,062 | 2,215,446 – 2,622,387 |
| `body-N.hf.collision` (500 mm heightfield + prisms) | ~1,487,000 | — | ~598,000 | 22,000 | 553,793 – 633,197 |

PCG placed 1,976 ± 4 instances per tile (1,967–1,982), so the per-tile variance is small: the
seed changes *where* the rocks are, not how many.

Least squares `size(n) = a + b·n` over the cumulative bytes of `s1` bodies 11..18 in id order
(`results/fit.txt`):

| series | a (bytes) | b (bytes / tile) | max residual | residual / size(8) |
|---|---|---|---|---|
| umap fs | −13,663 | 6,030,376 | 17,696 | 0.04 % |
| umap zstd19 | −61,915 | 2,060,629 | 52,784 | 0.32 % |
| tri fs | 566,485 | 20,105,301 | 565,284 | 0.35 % |
| tri zstd19 | 46,922 | 2,408,570 | 183,145 | 0.95 % |
| hf zstd19 | −24,380 | 610,825 | 32,337 | 0.67 % |

**a ≈ 0.** Body packages share nothing with each other; the only shared assets are the four
engine BasicShapes, which live in the engine, not the season. The season-constant share of a
*client install* (engine content, shaders, game binaries) is not in these numbers and was not
measured — it is the same for every season and is not what G2's question prices.

Bundles at N = 8 (`results/measure-sizes.out`): `s1-season.tar` (8 umaps + 8 tri) 209,582,592 B
raw → **31,701,709 B zstd19** (3.96 MB/tile) → **35,915,845 B as an Oodle UnrealPak** (per-entry:
umap 2,205,447, tri 2,315,038). zstd of the Oodle pak: 29,550,970 — the pak is already
compressed; a wire layer over a pak buys ~18 % more, and zstd over an uncompressed pak
(31,917,305) is within 1 % of zstd over the tar. Container and wire are therefore one number
here (±13 %) as long as one of them compresses.

The flattened-`tri` caveat from spike 2 holds: 95 % of the tri package is the 1,976 rock copies.
Spike 2 estimated the instanced form at ~0.5 MB zstd per tile; that is **[extrapolated]** — not
built — and would put the per-tile transfer at ~2.6 MB instead of 4.47 MB.

## 3. season(N): bodies [extrapolated], and what a "body" is

Spike 2 measured **one 256 m × 256 m tile**. G2 says "dozens of bodies (star, planets, moons)";
no G-number, no ADR and no doc in the tree fixes how much cooked surface a body carries
(`01-spatial-model.md` sizes cells and shards, not planets; `game/docs/00-requirements.md` G2/G4
name bodies, not areas). So season(N) has a second free variable, **K = tiles per body**, and
the honest table is N × K. Per tile: **4.47 MB** zstd19 (umap + flattened tri), 26.1 MB raw on
disk.

| N bodies \ K tiles per body | 1 (256 m) | 16 (1 km²) | 64 (2 km × 2 km) | 256 (4 km × 4 km) | 1,024 (8 km × 8 km) |
|---|---|---|---|---|---|
| 12 | 54 MB | 0.86 GB | 3.4 GB | 13.7 GB | 55 GB |
| 24 | 107 MB | 1.72 GB | 6.9 GB | **27.5 GB** | 110 GB |
| 36 | 161 MB | 2.58 GB | 10.3 GB | 41.2 GB | 165 GB |
| 48 | 215 MB | 3.43 GB | 13.7 GB | 55.0 GB | 220 GB |

Anchors in tiles (flattened tri; the instanced estimate would roughly double them):

| anchor | tiles | largest N at K = 64 | at K = 256 | at K = 1,024 |
|---|---|---|---|---|
| 20 GB | 4,475 | 69 | 17 | 4 |
| 50 GB | 11,188 | 174 | 43 | 10 |
| 100 GB | 22,375 | 349 | 87 | 21 |

Bytes scale with cooked area, and area is quadratic in the body's edge: the number that decides
the season size is K, not N. A 1 m lattice over a real planet (10⁷–10⁸ km²) is 10⁸–10⁹ tiles and
clears no anchor by six orders of magnitude; that is not a finding about representation, it is
why G4's landable surface has to be regions.

## 4. Patch bytes between seasons [measured]

Two seeds, same eight body ids, N = 8 (`results/measure-sizes.out`). The full download is
`s2-season.tar.zst` = 31,456,578 B; the Oodle pak is 36,024,210 B.

| tool, settings | input | patch bytes | patch / full |
|---|---|---|---|
| `zstd -19 --patch-from --long=27` | raw season tars | 29,574,055 | **0.94** |
| `xdelta3 -9 -S djw` | raw season tars | 46,772,448 | 1.49 (its own compressor is weaker than zstd −19; a patch larger than the full download) |
| `zstd -19 --patch-from` | Oodle paks | 29,130,164 | 0.81 of the pak, 0.93 of the zstd full |
| `xdelta3 -9 -S djw` | Oodle paks | 29,547,475 | 0.82 of the pak |
| `bsdiff` | umap tars only | 15,623,645 | 1.16 of the umap zstd full (13.5 MB) |
| UnrealPak | `s1.pak` vs `s2.pak` | every entry's sha1 differs (`-List`) — a file-granular patch is the whole pak | 1.00 |

**Patch ≥ 0.94 × full between seeds.** Two seeds share the four rock meshes and nothing else; a
patcher finds the ~6 % that is format framing and rock-copy coincidence. The falsifier in #1046
fires: a season jump under G2.1 is a fresh download of the season package, and "patch bytes
between seasons" is not a useful quantity for the seed-to-seed case.

**The unchanged-body case** — the one the `.umap` nondeterminism actually prices. Spike 2's two
cooks of one seed (`out-256a`, `out-256b`): `tri` byte-identical (confirmed again here), `.umap`
differs in 5,560 bytes (package save hash, PCG node/pin GUIDs; spike 2 README).

| tool | patch bytes per unchanged body |
|---|---|
| `zstd -19 --patch-from` on the two umaps | 5,088 |
| `xdelta3 -9` | 4,969 |
| `bsdiff` | 5,565 |
| UnrealPak entry: `Body_2.umap` Oodle 2,206,332 → 2,206,335 B, sha1 differs; `body-2.tri.collision` sha1 equal | **2,206,335** (the whole entry) |
| with the bytes canonicalised (spike 2's ~200 spots, or a content digest over the Unreal half) | 0 |

So for a season of N bodies where a body is re-cooked but unchanged: content-agnostic patching
costs **~5 kB × N** (noise), UE's pak/IoStore patch costs **~2.2 MB × N = the entire Unreal half
again**, and canonicalising costs nothing at all. What it takes to achieve the 0: either the
`-deterministicguids` path finished (spike 2 got 5,560 → 5,314 by seeding actor GUIDs; the save
hash and PCG graph GUIDs remain) or a patch/digest layer that is content-agnostic rather than
UE's. Either is a distribution-record question, not this spike's.

## 5. cook(n) [measured], and the parallelism ceiling

`results/timing-cook.txt`, quiet machine, crash reporter killed between cooks, `-NullRHI`, warm
DDC for the engine and shaders (each body's own Nanite build is always a miss: the body is new).

* **Fixed cost per process: 11.58 s** — `UnrealEditor-Cmd -run=TraceBody` with no arguments,
  which starts the engine and exits.
* **Marginal cost per body: 3.12 ± 0.10 s** commandlet main (`body-N.cook.json`
  `timing.commandlet_main_s`, n = 8: 2.97–3.27 s), of which ~1.5 s is the async Nanite build,
  0.27 s PCG, 0.1 s save (spike 2's breakdown).
* **Wall per body-process: 16.78 s** (16.30–17.47); fit `cook(n) = −0.2 + 16.85 n`, max residual
  0.35 s. The ~2 s over start + main is shutdown (the ensure, the crash-report handoff).

| concurrency | total wall for k bodies | amortised per body | commandlet main under load |
|---|---|---|---|
| 1 | 16.8 s | 16.8 s | 3.1 s |
| 2 | 18.7 s | 9.4 s | 3.6 s |
| 4 | 26.8 s | 6.7 s | 5.5–6.5 s |
| 8 | 50.9 s | **6.4 s** | 9.6–14.4 s |

**Core-bound, not memory-bound.** Peak RSS of one cook process is 2.44 GB
(`results/same-seed-pak-and-rss.txt`, `/usr/bin/time -l`), so eight of them use ~20 GB of 64 GB;
what saturates is the 10 cores — commandlet main quadruples under par8 because each process's
Nanite build and PCG are themselves multithreaded. The ceiling on this machine is **~6.4 s per
tile ≈ 560 tiles per hour**, and it will not improve by adding processes. It parallelises across
machines trivially: every body is an independent process with no shared state (a ≈ 0 above), so
M machines give M × 560 tiles/hour.

cook(N) for N tiles [extrapolated from the two measured regimes]:

| tiles | sequential processes (16.85 s) | par8 on this Mac (6.4 s) | one process, bodies in sequence: 11.6 + 3.12 n (not measured as a multi-body process) |
|---|---|---|---|
| 12 | 3.4 min | 1.3 min | 49 s |
| 48 | 13.5 min | 5.1 min | 2.7 min |
| 48 × 64 = 3,072 | 14.4 h | 5.5 h | 2.7 h |
| 48 × 256 = 12,288 | 57.5 h | 21.8 h | 10.7 h |

G2.1e's maintenance window holds up to roughly K = 64 at N = 48 on one M1 Max, and the cook is
embarrassingly parallel beyond that. The "days on one machine" falsifier only fires at the K
values the size anchors already reject.

**Cold versus warm.** Every number above is warm-engine (DDC has the engine shaders; each body's
content is new). A cold `-run=cook` on this Mac spent 617 s before failing in shader compilation;
spike 2 saw 5 minutes of shader compilation on its first run without `-NullRHI`. The cold cost is
a one-off per machine per engine version, not per season.

## 6. Interiors: what was measured, what was assumed, and how much rests on it

The issue's G10 consequence puts caves/buildings (G4.6), the mothership interior (G4/G11.1) and
ship interiors (G6) in the ruleset package. Spike 2 measured none. This spike measured the
**ruleset half** of two hand-authored levels and could not measure the Unreal half (§1).

| level | actors | colliding SM components | LOD0 tris | `tri` package raw | zstd19 | not counted |
|---|---|---|---|---|---|---|
| `Lvl_Horror` (corridors, rooms, doors; 60 × 40 m footprint) | 87 | 97 | 22,948 | 682,360 | **47,024** | 17 other colliding prims: `BrushComponent` (BSP), `BoxComponent`, mesh-less SMCs |
| `Lvl_FirstPerson` (open arena, 40 × 40 m) | 68 | 54 | 5,724 | 197,408 | 17,316 | — |

The Unreal half, as an **upper bound from source assets** (editor `.uasset`/`.umap`, not cooked):
Horror level + its external actors + the LevelPrototyping meshes, materials, textures and door
assets it references = 3,052,544 B raw, **483,871 B zstd19** (`results/interior-sizes.txt`).
So one grey-box interior block is **≤ 0.53 MB zstd19 both halves** — an eighth of a terrain
tile.

**Assumption used for I(N):** mothership interior = 10 Horror-sized blocks (a station of ten
60 × 40 m decks), three ship classes (G2.2) at one block each, and one cave/building block per
body: `I(N) = (13 + N) × 0.53 MB`. At N = 48 that is **32 MB zstd19, ≤ 230 MB raw** — under 1 %
of the 20 GB anchor and under 3 % of the 12-tile season in §3. Even at 100× that content (a
1,000-deck mothership, ten buildings per body) interiors stay under 20 GB. **The "interiors
dominate" falsifier does not fire at grey-box density**, and the verdict in §3 does not rest on
the interior number at all: it rests on K.

What that assumption does *not* cover, and is the largest unknown in this spike: **art-quality
interiors are textures, not triangles.** The prototyping assets carry one 12 kB grid texture; a
shipped interior carries megabytes of material textures per room, which the Unreal half would
have to ship and the ruleset half would not. That number cannot come from anything in the tree or
the templates; it is per-art-direction and needs a real asset, cooked, which needs the Metal
toolchain or a Linux-target cook.

## 7. The one measured transfer [measured]

Origin: the Mac (`192.168.0.155`, `python -m http.server`). Client: this Linux box
(`192.168.0.120`, on `wlan0` — WiFi — while spike #1045's editor was running on it). Path: home
LAN, tailscale-reported direct, 6 ms RTT. `results/transfer-verify.json`.

| object | bytes | seconds (curl `time_total`) | throughput |
|---|---|---|---|
| `s1-season.tar.zst` (8 tiles, both halves) | 31,701,709 | 1.105 | 28.7 MB/s ≈ 229 Mbit/s |
| `s1.pak` (Oodle) | 35,915,845 | 1.959 | 18.3 MB/s |

After extraction on the client, spike 2's two-half blake3 digest (`collision-trace digest`,
built on Linux from the spike-2 crate, unmodified) matched the origin's `digests-s1.json` for
**8 of 8 bodies**. Scaling the measured rate: 20 GB at this WiFi link is ~12 min; at a
100 Mbit/s consumer line it is ~27 min, 50 GB ~67 min, 100 GB ~2.2 h. Those three are
[extrapolated] from one LAN sample; no CDN was involved.

## 8. The row the owner asked for

| anchor | tiles | bodies at 2 km × 2 km regions (K = 64) | at 4 km × 4 km (K = 256) | verdict |
|---|---|---|---|---|
| 20 GB | 4,475 | 69 | 17 | dozens fit if regions ≤ ~2.4 km square |
| 50 GB | 11,188 | 174 | 43 | dozens fit up to ~3.9 km square |
| 100 GB | 22,375 | 349 | 87 | dozens fit up to ~5.5 km square |

**"Dozens of bodies" is a v1 quantity, and the first seasons can ship dozens — as regions of a
few kilometres per body, never as planets.** Recommendation: fix K, not N. A landing region of
2 km × 2 km per body (K = 64) puts 48 bodies at 13.7 GB flattened (≈ 7–8 GB with instanced rocks),
cooks in 5.5 h on one M1 Max or 1.4 h on four, and is transferred as a fresh ~14 GB download
each season (patching between seeds buys 6 %). Every one of those numbers moves with K², so the
owner's number to set is the per-body cooked surface; nothing in the tree sets it today.

## Not established

* Cooked Unreal-half bytes (Metal toolchain / no cross-target on the Mac; §1). Editor-saved bytes
  are reported throughout, labelled.
* The instanced `tri` package (spike 2's ~0.5 MB estimate) — not built.
* UnrealPak's `-Diff` summary line did not print on this build; the per-entry sha1 listing is
  the evidence for file-granular patching. UE's release-versioned patch flow (`BuildCookRun
  -generatepatch`) was not run; it is file-granular by the same rule.
* Art-quality interior size (§6). BSP brushes in the Horror level were not exported.
* Cold-DDC cook time for a body: the engine-warm number is the operative one for a season cook;
  the cold one-off was measured only as the 617 s failed run.
* Windows/Linux cook determinism — as in spike 2, macOS only.
