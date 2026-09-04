# onebody_collision_trace — spike #1044, the ruleset half

Research spike, never merged (#1042 rule 6). Paired with the Unreal half in
`docs/spikes/1044-one-body-cook/unreal/` (two commandlets: `CookBody`,
`TraceBody`). The reproduction commands, the numbers and the recommendation
are in `docs/spikes/1044-one-body-cook/README.md`.

One binary, five subcommands:

| command | what it does |
|---|---|
| `collision-trace rays --collision body.tri.collision --out rays.bin --n 5000 --seed 42` | seeded rays (`rand_chacha`): origins uniform over the body's XY bounds at `surface − 2 m .. surface + 200 m`, directions uniform on the sphere, 500 m long; the seed is written into the file |
| `collision-trace trace --collision body.<tri\|hf\|vox>.collision --rays rays.bin --out hits.bin` | the deterministic ray test over one representation, dispatched on the package magic |
| `collision-trace compare --unreal hits-unreal.bin --rust hits.bin --rays rays.bin --out report.json` | `rate(τ)` at τ ∈ {10, 50, 250, 1000} mm, hit/miss disagreements, max/p50/p99 and the histogram of |Δd|, the by-actor breakdown, and the worst rays with a cause |
| `collision-trace digest --unreal Body.umap --collision a --collision b ...` | blake3 over both halves read back from disk, and the flip-one-byte check |
| `collision-trace sizes --file ...` | raw bytes and `zstd -19` bytes (via the `zstd` CLI) |

## Determinism envelope

Every hit decision and every distance is integer arithmetic (i64 positions in
millimetres, i128 products, exact rationals compared by continued-fraction
division — `geom.rs`, `Rat::cmp` — because cross-multiplying two triangle
times overflows i128). No float participates in a hit or a distance, so D43's
"bit-identical on Windows and Linux" holds by construction. Floats appear in
exactly two places, neither adjudicated: the ray *generator* (`rays.rs`) and
the reported unit normal (`normal_1e6`, from the exact integer cross product).

## Representations (file formats in `format.rs` and each module)

* `tri` — the intermediate's triangles snapped to the mm lattice, terrain then
  each scatter instance flattened to world space; BVH built by the reader
  (median split, index tiebreak: deterministic).
* `hf` — height grid at a stated cell (heights sampled from the intermediate
  by the cook), fixed-diagonal triangulation, exact 2D DDA; instances as
  26-DOP prisms (13 integer slab directions).
* `vox` — occupancy at a stated edge as per-column run lengths (terrain solid
  below the surface, instances as rasterised shells), exact column walk.

`cargo test` holds the `hf` DDA and the `vox` column walk against brute force
over every cell / every occupied voxel on seeded random rays.
