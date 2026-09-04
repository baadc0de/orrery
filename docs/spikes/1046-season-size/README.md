# Spike #1046 — season size, patch bytes, full-system cook: measured fits from spike 2's body

Research spike (#1042, spike 4). Output: three numbers with the measured fit under each, and the
v1 answer to G2's "dozens of bodies". It settles G2 **as a v1 quantity** only; D52/D53 stay
Proposed, ADR-0021 still has no content-package clause, and nothing here merges.

Every number is tagged **[measured]** (a file or log on this branch produced it) or
**[extrapolated]** (arithmetic over measured points, with the assumption named). Machine for every
measurement: Apple M1 Max, 10 cores, 64 GB, macOS 26.6.2, UE 5.8.2-56702186 (installed build),
spike 2's `OneBodyCook` project and `collision-trace` binary on `bojans-max`. **Every Unreal-half
byte below is a cooked package for the macOS target** (`-run=cook -targetplatform=Mac
-unversioned`, loose files) — the only client target this installed build carries (its targets
are Mac, iOS, tvOS, visionOS; no Windows or Linux). Raw outputs in `results/`; the scripts that
produced them in `scripts/`; the one new commandlet in `unreal/`.

## The three numbers

| | value | fit / error bar | status |
|---|---|---|---|
| **Gigabytes per season** | **5.16 MB per 256 m tile** zstd −19 (cooked Unreal half 2.74 MB + ruleset `tri` 2.41 MB); 6.27 MB + 20.1 MB raw on disk; interiors add ≤ 0.53 MB each | `size(n) = a + b·n` over n = 1..8 tiles: a ≈ 0, b = 2,749,883 + 2,408,570 B, max residual 0.14 % / 0.95 %; per-tile sd across 16 tiles 0.7 % (cooked) / 5.5 % (tri) | [measured] per tile; season(N) is [extrapolated] and depends on **tiles per body**, which no G-number fixes — §3 |
| **Patch bytes between seasons** | **≈ 95 % of a fresh download** between two seeds (18.5 MB patch vs 19.4 MB full, cooked, N = 8). For an **unchanged body re-cooked**: 384 bytes differ, all GUIDs/hash → **~1.4 kB** under a content-agnostic patcher, **1.32 MB in the pak (4.15 MB loose)** under UE's file-granular patch because they sit in `.uexp`, **0** once the GUIDs are canonicalised | zstd `--patch-from`, xdelta3, bsdiff, UnrealPak per-entry sha1 — §4 | [measured] |
| **Full-system cook seconds** | **16.85 s per body-process** (CookBody: 11.6 s engine start fixed + 3.12 s marginal + ~2 s exit); **6.4 s/body amortised at 8 concurrent processes**, core-bound. Plus the cook-by-the-book step: **15 s fixed per cook + ~0.2–0.4 s per body**, and a **one-off shader-compilation term ≥ 11 min on a cold machine** | `cook(n) = c + d·n`: c = −0.2 s, d = 16.85 s, residual 0.35 s (n = 1..8) | [measured] per tile; cook(N) [extrapolated] |

**The v1 answer (§8):** "dozens of bodies" is a v1 quantity **provided a body's cooked landable
surface is a landing region, not a planet**. At spike 2's representation (1 m lattice, PCG rocks
at 0.03/m², flattened rock triangles) the anchors are reached at **3,877 / 9,693 / 19,386 tiles**
(20 / 50 / 100 GB): 48 bodies clear 20 GB at up to ~80 tiles each (a ~2.3 km square) and 50 GB
at ~200 tiles each (~3.6 km square). A whole planet at this spacing is not a v1 quantity at any
anchor. The season jump is a full download, not a patch. **The cooked pass confirms the
first-pass verdict** (editor-saved bytes gave 4,475 / 11,188 / 22,375 tiles): cooking adds 15 %
to the per-tile transfer and moves no threshold the owner would set.

## 1. What was run

* **Bodies.** Spike 2's `CookBody` commandlet, one process per body, `-NullRHI`, 256 m at 1 m
  spacing, density 0.03/m² — the measured body of #1044. Two season seeds, eight body ids each
  (`s1` = seed 1001, `s2` = seed 2002, bodies 11–18): `results/per-body.json`,
  `results/fit-cook-sizes-pass.txt`. A separate timing pass (`scripts/timing-cook.sh`,
  `results/timing-cook.txt`) cooked eight more bodies sequentially, then 2, 4 and 8 concurrently.
* **The real cook.** `scripts/chain4-cooked.sh` → `results/chain4-cooked.txt`,
  `results/per-body-cooked.json`: every body map copied under `/Game/Bodies` and cooked by the
  book for the Mac target — one body cold, warm, and again (determinism of cooked output); spike
  2's two editor saves of one seed (`out-256a`/`out-256b`) cooked separately; both interiors; each
  8-body season as one cook, then zstd/xdelta3/UnrealPak over the cooked bytes.
* **Interiors.** A new `MeasureLevel` commandlet (`unreal/MeasureLevelCommandlet.cpp`, added to
  spike 2's editor module on the Mac) loads a level, walks every colliding static-mesh component,
  and writes spike 2's `tri` package for it. Run on two hand-authored levels from Epic's UE 5.8
  FirstPerson template: `Variant_Horror/Lvl_Horror` (corridors and rooms from LevelPrototyping
  cubes, doors, 87 actors) and `FirstPerson/Lvl_FirstPerson` (the open arena, 68 actors).
  `results/level-1-horror.measure.json`, `results/level-2-firstperson.measure.json`.
* **Editor-saved sizes, containers, patches** (the first pass, kept as the comparison row):
  `scripts/measure-sizes.sh` → `results/measure-sizes.out`, `results/same-seed-pak-and-rss.txt`.
* **One transfer.** `scripts/transfer-origin.sh` (Mac, `python -m http.server`) and
  `scripts/transfer-client.sh` (this Linux box) → `results/transfer-verify.json`.
* **Fits.** `scripts/fit.py` → `results/fit.txt` (editor-saved series; the cooked fit is in §2).

### Confounders found and handled

* **Spike 2's commandlet trips an ensure at shutdown** (`WorldSubsystem.cpp:118`, `!bInitialized`)
  and leaves a `CrashReportClient` spinning at ~45 % CPU per cook. Fifteen from spike 2's runs
  were still alive when this spike started; the first eight cooks of the size pass climbed from
  23.5 s to 29.2 s wall for that reason and dropped to 16.6 s the moment they were killed. The
  timing pass kills the reporter after every cook, and **only the timing pass is quoted for cook
  seconds**. **Anyone reproducing either spike's timings must kill these between cooks** (all were
  killed at the end of this spike; zero alive). `mediaanalysisd` at 216 % CPU was also running on
  the Mac and was killed.
* **The Mac-target cook needs the Metal toolchain**, which Xcode 26 ships as a separate download
  and which was not installed when this spike started: the first cook ran 617 s and died in Metal
  bytecode compilation (`cannot execute tool 'metal' due to missing Metal Toolchain`). The owner
  installed it (Command Line Tools had to be bumped first) and the cooked pass below ran after.
  The editor-saved numbers from the first pass are kept only where they show what cooking changes.
* **UE 5.8 defaults `bUseZenStore=True`** (`Engine/Config/BaseGame.ini:95`): the cook writes to
  zenserver, not `Saved/Cooked`. `bUseZenStore=False` / `bUseIoStore=False` were set in the
  project's `DefaultGame.ini` so cooked packages land as loose files that can be sized and diffed.
* The interior cooks returned rc = 1 because the template's Blueprints derive from a C++ module
  the OneBodyCook project does not have (`BP_HorrorPlayerController`, `UI_Horror` ...); the level
  geometry, actors, meshes and materials cooked, and those are what is sized.

## 2. size(n): the fit [measured]

**Cooked** per body, Mac target (`results/per-body-cooked.json`, 16 bodies over two seeds). fs =
bytes on disk; zstd19 = `zstd -19` of the raw bytes.

| cooked file | mean fs | sd | mean zstd19 | sd |
|---|---|---|---|---|
| `Body_N.umap` (summary, name/import/export tables) | 27,119 | 0 | 5,397 | 5 |
| `Body_N.uexp` (exports: PCG component, ISM instances, section actor, Chaos trimesh) | 4,121,814 | 1,963 | 1,451,275 | 15,771 |
| `Body_N.ubulk` (Nanite bulk data) | 2,122,548 | 15,222 | 1,287,624 | 9,764 |
| **cooked Unreal half** | **6,271,481** | 15,259 | **2,744,296** | 19,198 |
| editor-saved `.umap` (first pass, for comparison) | 6,030,905 | 11,596 | 2,056,752 | 22,813 |
| `body-N.tri.collision` (ruleset half; 131,072 terrain + ~592 k flattened rock tris) | 20,147,572 | 373,351 | 2,386,888 | 131,062 |
| `body-N.hf.collision` (500 mm heightfield + prisms) | ~1,487,000 | — | ~598,000 | 22,000 |

Cooking adds 4 % raw and **33 % after zstd** to the Unreal half (the cooked Nanite/bulk data
compresses worse than the editor's serialisation); it is byte-deterministic for the same source
(§4). PCG placed 1,976 ± 4 instances per tile, so the per-tile variance is small.

Least squares `size(n) = a + b·n` over the cumulative bytes of `s1` bodies 11..18 in id order:

| series | a (bytes) | b (bytes / tile) | max residual | residual / size(8) |
|---|---|---|---|---|
| **cooked Unreal half zstd19** | −16,447 | **2,749,883** | 31,770 | 0.14 % |
| tri zstd19 | 46,922 | **2,408,570** | 183,145 | 0.95 % |
| hf zstd19 | −24,380 | 610,825 | 32,337 | 0.67 % |
| editor-saved umap zstd19 (first pass) | −61,915 | 2,060,629 | 52,784 | 0.32 % |

**a ≈ 0.** Body packages share nothing with each other; the four engine BasicShapes live in the
engine. The season-constant share of a *client install* is visible in the cooked output and is
not a season cost: a one-body cook emits 256 MB of shader archives (Global SM6 155.7 MB, SM5
57.0 MB, project 29.9 + 13.4 MB) and 169 MB of engine content, identical for every body and
every season.

Bundles at N = 8, cooked (`results/chain4-cooked.txt`): the 24 cooked body files
50,225,152 B raw → **19,675,401 B zstd19** (2.46 MB/tile) → **21,099,404 B as an Oodle
UnrealPak** (per body: ubulk 1,311,926 + uexp 1,315,159 + umap 5,322 = 2.63 MB). Container and
wire are one number here (±7 %) as long as one of them compresses. With the tri half:
**5.16 MB zstd per tile, 4.95 MB as pak entries.**

The flattened-`tri` caveat from spike 2 holds: 95 % of the tri package is the 1,976 rock copies.
Spike 2 estimated the instanced form at ~0.5 MB zstd per tile; that is **[extrapolated]** — not
built — and would put the per-tile transfer at ~3.3 MB instead of 5.16 MB.

## 3. season(N): bodies [extrapolated], and what a "body" is

Spike 2 measured **one 256 m × 256 m tile**. G2 says "dozens of bodies (star, planets, moons)";
no G-number, no ADR and no doc in the tree fixes how much cooked surface a body carries
(`01-spatial-model.md` sizes cells and shards, not planets; `game/docs/00-requirements.md` G2/G4
name bodies, not areas). So season(N) has a second free variable, **K = tiles per body**, and
the honest table is N × K. Per tile: **5.16 MB** zstd19 (cooked Unreal half + flattened tri),
26.4 MB raw on disk.

| N bodies \ K tiles per body | 1 (256 m) | 16 (1 km²) | 64 (2 km × 2 km) | 256 (4 km × 4 km) | 1,024 (8 km × 8 km) |
|---|---|---|---|---|---|
| 12 | 62 MB | 0.99 GB | 4.0 GB | 15.9 GB | 63 GB |
| 24 | 124 MB | 1.98 GB | 7.9 GB | **31.7 GB** | 127 GB |
| 36 | 186 MB | 2.97 GB | 11.9 GB | 47.5 GB | 190 GB |
| 48 | 248 MB | 3.96 GB | **15.9 GB** | 63.4 GB | 254 GB |

Anchors in tiles (cooked, flattened tri; the instanced estimate would raise them ~1.6×):

| anchor | tiles | largest N at K = 64 | at K = 256 | at K = 1,024 |
|---|---|---|---|---|
| 20 GB | 3,877 | 60 | 15 | 3 |
| 50 GB | 9,693 | 151 | 37 | 9 |
| 100 GB | 19,386 | 302 | 75 | 18 |

Bytes scale with cooked area, and area is quadratic in the body's edge: the number that decides
the season size is K, not N. A 1 m lattice over a real planet (10⁷–10⁸ km²) is 10⁸–10⁹ tiles and
clears no anchor by six orders of magnitude; that is not a finding about representation, it is
why G4's landable surface has to be regions.

## 4. Patch bytes between seasons [measured]

### Between two seeds — a season jump

Cooked Unreal halves of the two 8-body seasons (`results/chain4-cooked.txt`); the full download
of `s2` is 19,390,041 B zstd19.

| tool, settings | patch bytes | patch / full |
|---|---|---|
| `zstd -19 --patch-from --long=27` on the cooked tars | 18,476,731 | **0.95** |
| `xdelta3 -9 -S djw` on the cooked tars | 22,689,119 | 1.17 (larger than the full download) |
| UnrealPak `cooked-s1.pak` vs `cooked-s2.pak` | every entry's sha1 differs — a file-granular patch is the whole pak (21.0 MB) | 1.00 |
| first pass, editor-saved tars + tri (N = 8): zstd patch 29,574,055 vs 31,456,578 full | | 0.94 |

**Patch ≥ 0.95 × full between seeds.** Two seeds share the four rock meshes and nothing else.
The falsifier in #1046 fires: a season jump under G2.1 is a fresh download of the season package,
and "patch bytes between seasons" is not a useful quantity for the seed-to-seed case.

### An unchanged body re-cooked — where the nondeterminism lives, and what it costs

Three measurements on one body (seed 1, body 2 — spike 2's measured body):

1. **Two cooks of the same source are byte-identical.** `cooked-body11-warm` vs
   `cooked-body11-again`: `Body_11.umap`, `.uexp`, `.ubulk` all 0 differing bytes; only the
   shader archives and `CookMetadata.ucookmeta` differ. Cooked output is deterministic for a
   fixed editor save.
2. **Two editor saves of one seed differ by 5,560 bytes (spike 2); after cooking each, by 384.**
   Cooking absorbs 93 % of the editor-save nondeterminism (the package summary text, the
   editor-only PCG graph serialisation), but not the GUIDs. **The first pass's 2,206,335 B-per-
   unchanged-body pak figure was measured on editor-saved packages — the wrong artifact for a
   shipping cost — and is superseded by the numbers below.**
3. **Which file the 384 bytes are in** (`results/cooked-same-seed-diff.txt`, per-file sha1 and
   offsets):

| cooked file | size | sha1 | differing bytes | runs | what sits there |
|---|---|---|---|---|---|
| `Body_2.ubulk` | 2,131,620 | **equal** | 0 | 0 | Nanite bulk data — stable |
| `Body_2.uexp` | 4,120,165 | differ | **364** | 24 | 16- and 32-byte fields (and the 4-byte field preceding each 32-byte one) beside the names `Scatter` (@52, @116: the PCG component), `PersistentLevel.MeshPartition_0` and `CompiledSection_Default` (@2,890,052–2,890,753: actor GUIDs and the section BuildKey), and five more `020a0301 xxxxxxxx` / `00000501 <32 B>` pairs at 2,946,747…4,120,038 (PCG node/pin GUIDs and their hashes). Nothing in vertex, index or instance data. |
| `Body_2.umap` | 27,118 | differ | **20** | 1 | one 20-byte run at offset 24–43, immediately before the package name at @56: the package summary's saved hash |

These are **the same GUIDs and the same save hash spike 2 traced in the editor save**, now in
binary. Canonicalising them (spike 2's `-deterministicguids` finished — the section BuildKey and
PCG node/pin GUIDs seeded the way actor GUIDs already are — after which the save hash follows
from the content) fixes both layers at once.

Per unchanged body that is re-cooked and shipped as is:

| mechanism | patch bytes per unchanged body |
|---|---|
| `zstd -19 --patch-from`, cooked files | 1,336 (uexp) + 46 (umap) = **1,382** |
| `xdelta3 -9`, cooked files | 774 + 238 = 1,012 |
| UE file-granular pak patch: `.uexp` + `.umap` entries ship whole, `.ubulk` does not | **1,320,481 B in the Oodle pak** (1,315,159 + 5,322); 4,148,933 B as loose cooked files — 50 % of the body's pak bytes, 66 % of its loose bytes |
| with the GUIDs canonicalised | **0** |
| first pass, editor-saved `.umap` pak entry (wrong artifact — superseded) | ~~2,206,335~~ |

So the cooked shipping cost of an unchanged body under UE's own patching is 1.32 MB, not 2.2 MB
and not 6.27 MB — but it is still half the body, because every one of the 24 runs is in the
`.uexp`. A content-agnostic patcher makes it ~1.4 kB; canonicalising makes it nothing. Either is
a distribution-record decision, not this spike's.

## 5. cook(n) [measured], and the parallelism ceiling

`results/timing-cook.txt`, quiet machine, crash reporter killed between cooks, `-NullRHI`, warm
DDC for the engine and shaders (each body's own Nanite build is always a miss: the body is new).

**Term 1 — CookBody (the season's content build), per body:**

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

**Term 2 — cook by the book (the Unreal packaging step), per cook, Mac target
(`results/chain4-cooked.txt`):**

| cook | wall | cooker's own total | of which "in tick" | shaders compiled |
|---|---|---|---|---|
| first cook on the machine, toolchain absent (failed) | **617 s** | — | — | thousands (populated the DDC before dying at Metal bytecode) |
| first successful cook after the toolchain install ("cold", but the DDC was already warm from the 617 s run) | **47.5 s** | 35.5 s | 25.8 s | 10 |
| same body again (steady state) | 15.1 s | 5.2 s | 2.06 s | 0 |
| same body a third time | 15.2 s | 5.3 s | 2.07 s | 0 |
| 8 bodies in one cook (`s1`) | 17.1 s | 6.6 s | 3.24 s | 0 |
| 8 bodies in one cook (`s2`) | 16.7 s | 6.1 s | 2.86 s | 0 |

So the packaging step is **~15 s fixed per cook process + ~0.2–0.4 s per body** (1 → 8 bodies
moved "in tick" from 2.06 s to 3.24 s; two points only, [extrapolated] beyond them), negligible
next to term 1. **The shader term is its own line, not amortised:** a genuinely cold machine pays
**more than 617 + 47 s ≈ 11 min once** per machine per engine version (the 47.5 s "cold" number
sat on a DDC the failed run had already filled; spike 2 saw 5 min of shader compilation on its
first non-`-NullRHI` run). It is paid once, not per season and not per body.

cook(N) for N tiles [extrapolated from the measured regimes], both terms, steady state:

| tiles | sequential CookBody processes (16.85 s) + cook step | par8 on this Mac (6.4 s) + cook step | one process, bodies in sequence: 11.6 + 3.12 n (not measured as a multi-body process) |
|---|---|---|---|
| 12 | 3.7 min | 1.6 min | 1.1 min |
| 48 | 14.0 min | 5.6 min | 3.2 min |
| 48 × 64 = 3,072 | 14.6 h | 5.7 h | 2.9 h |
| 48 × 256 = 12,288 | 58.5 h | 22.9 h | 11.7 h |

G2.1e's maintenance window holds up to roughly K = 64 at N = 48 on one M1 Max, and the cook is
embarrassingly parallel beyond that. The "days on one machine" falsifier only fires at the K
values the size anchors already reject.

## 6. Interiors: what was measured, what was assumed, and how much rests on it

The issue's G10 consequence puts caves/buildings (G4.6), the mothership interior (G4/G11.1) and
ship interiors (G6) in the ruleset package. Spike 2 measured none. This spike measured both
halves of two hand-authored levels.

| level | actors | colliding SM components | LOD0 tris | `tri` raw | `tri` zstd19 | cooked Unreal half (level + external actors + the LevelPrototyping meshes/materials/textures it pulls in) | not counted |
|---|---|---|---|---|---|---|---|
| `Lvl_Horror` (corridors, rooms, doors; 60 × 40 m footprint) | 87 | 97 | 22,948 | 682,360 | **47,024** | **701,742 B raw** (58 files: level + actors 70,334 in 2 files, prototyping assets 505,931 in 36, the rest materials/colorway) | 17 other colliding prims: `BrushComponent` (BSP), `BoxComponent`, mesh-less SMCs |
| `Lvl_FirstPerson` (open arena, 40 × 40 m) | 68 | 54 | 5,724 | 197,408 | 17,316 | 459,501 B raw (29 files) | — |

So one grey-box interior block is **1.4 MB raw both halves, ≤ 0.53 MB zstd19** (the zstd bound
is from the source assets, 483,871 B; the cooked files were not separately compressed) — a
quarter of a terrain tile raw, a tenth compressed.

**Assumption used for I(N):** mothership interior = 10 Horror-sized blocks (a station of ten
60 × 40 m decks), three ship classes (G2.2) at one block each, and one cave/building block per
body: `I(N) = (13 + N) × 0.53 MB`. At N = 48 that is **32 MB zstd19, ≤ 85 MB raw** — under 1 % of
the 20 GB anchor and under 3 % of the 12-tile season in §3. Even at 100× that content interiors
stay under 20 GB. **The "interiors dominate" falsifier does not fire at grey-box density**, and
the verdict in §3 does not rest on the interior number at all: it rests on K.

What that assumption does *not* cover, and is the largest unknown in this spike: **art-quality
interiors are textures, not triangles.** The prototyping assets carry one 12 kB grid texture; a
shipped interior carries megabytes of material textures per room, which the Unreal half ships
and the ruleset half does not. That number cannot come from anything in the tree or the
templates; it needs a real asset, and it can now be cooked on this Mac.

## 7. The one measured transfer [measured]

Origin: the Mac (`192.168.0.155`, `python -m http.server`). Client: this Linux box
(`192.168.0.120`, on `wlan0` — WiFi — while spike #1045's editor was running on it). Path: home
LAN, tailscale-reported direct, 6 ms RTT. `results/transfer-verify.json`. The bundle transferred
was the first pass's (editor-saved umaps + tri); the cooked bundle is the same size to within 3 %
(19.7 MB + 18.2 MB) and would take the same time.

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
| 20 GB | 3,877 | 60 | 15 | dozens fit if regions ≤ ~2.3 km square |
| 50 GB | 9,693 | 151 | 37 | dozens fit up to ~3.6 km square |
| 100 GB | 19,386 | 302 | 75 | dozens fit up to ~5.1 km square |

**"Dozens of bodies" is a v1 quantity, and the first seasons can ship dozens — as regions of a
few kilometres per body, never as planets.** Recommendation: fix K, not N. A landing region of
2 km × 2 km per body (K = 64) puts 48 bodies at 15.9 GB cooked (≈ 10 GB with instanced rocks),
cooks in 5.7 h on one M1 Max or ~1.5 h on four, and is transferred as a fresh ~16 GB download
each season (patching between seeds buys 5 %). Every one of those numbers moves with K², so the
owner's number to set is the per-body cooked surface; nothing in the tree sets it today. The
cooked pass moved the first-pass figures by +15 % (bytes) and +3 % (cook time) and changed
nothing in this row that an owner would decide differently on.

## Not established

* The instanced `tri` package (spike 2's ~0.5 MB estimate) — not built.
* UnrealPak's `-Diff` summary line did not print on this build; the per-entry sha1 listing is
  the evidence for file-granular patching. UE's release-versioned patch flow (`BuildCookRun
  -generatepatch`) was not run; it is file-granular by the same rule.
* Art-quality interior size (§6). BSP brushes in the Horror level were not exported.
* A genuinely cold-machine cook time: bounded below by 617 + 47 s, not measured as one run.
* Windows/Linux cook determinism — as in spike 2, macOS only; and this installed build cannot
  cook those targets at all.
* The cook-by-the-book per-body marginal is two points (n = 1, n = 8); it is small either way.
