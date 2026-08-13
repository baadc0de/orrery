# 12 — World Seeding: the TOML Scenario Runner

`orrery-seed` is the offline world seeder and content-import tool for the persistence cluster: a **TOML-configured scenario runner** that bulk-writes a designed or synthetic world into FoundationDB, records a content manifest so later deploys can diff and patch it, and reports exactly what it produced. It is the P2 deliverable "offline world-seed import tool on the persistd harness" ([11-roadmap.md](11-roadmap.md) §P2) and the mechanization of [08-persistence.md](08-persistence.md) §17.

It exists to answer one question a demo operator actually asks: *"give me a world of size X with density pattern Y, and tell me what it will cost before you write it."* Everything in this document follows from taking that question literally.

Normative source: [DECISIONS.md](DECISIONS.md) §D11 (keyspace, seeding, id minting), with D5 (`CellId`), D9 (`Ruleset`, `universe_seed`), D12 (library-harness posture), D13 (float determinism), D15 (canonical scalars) and D16 (parameters). This document expands [08-persistence.md](08-persistence.md) §17 and is normative for the tool; where it and §17 differ, §17 wins and this document is wrong.

---

## 1. Scope

**In scope.** The scenario file format; the generator bank and its parameterizations; targeting and dry-run estimation; the determinism contract; content identity, the manifest, and diff/patch; the FDB write path; actor-tier safety; verification, reporting and telemetry; the seam to the P2 latency rig and the `kill -9` harness.

**Out of scope, deliberately.** Live content hot-patching of a running world (the seeder is offline by default, §11); `Ruleset` semantics for merge policy (the seeder defines the *hook*, the game defines the *policy*); the latency rig itself (specified separately, this document defines only the seam); witness-attested intent seeding (P5 — seeded content is bulk-class by construction, §11.4).

**What "seeding" means here.** Two jobs that share one tool because they share one write path:

| Job | Consumer | What matters |
|---|---|---|
| **Designed content import** | the game's content pipeline | stable identity, diff/patch across builds, never clobbering player edits |
| **Synthetic world generation** | the P2 demo, the latency rig, capacity modelling | exact counts, controllable density patterns, reproducibility, speed |

They pull in different directions — designed content wants *stability under change*, synthetic load wants *variety under a knob* — and §7's identity scheme is where they are reconciled.

---

## 2. Prerequisites: eight defects the seeder depends on

The seeder writes `world/` and `chunk/` rows and its whole value proposition is that those rows are then **readable by area load and survive a restart**. Tracing that path through the current tree found eight defects that break it. They are listed here because the seeder's acceptance criteria (§14) cannot be met until they are fixed, and because several of them change what the seeder is allowed to write.

| # | Location | Defect | Consequence for the seeder |
|---|---|---|---|
| **P-1** | `orrery_persistd/src/runtime.rs` — `CellRuntime::open`, `CellRuntime::restore` | Journal replay filters records with `if rec.cell != shard { continue; }` — **equality** — while the write path routes with `shard.is_prefix_of(cell)` (`runtime.rs:130`) and clients uplink the entity's *interest* cell (`orrery_persist_client/src/feed.rs:87`). Every real diff is discarded at recovery. | The P2 demo criterion ("`kill -9`, restart, the world resumes") cannot pass, seeded or not. Replace the filter with `shard.is_prefix_of(rec.cell)`. |
| **P-2** | `orrery_persistd/src/checkpoint/fdb.rs:118` | Checkpoints write `world_key(data.shard, entity)` — keyed by the **shard**, not the entity's cell. D11 §6 specifies `world/{cell_id}/{entity_id}` where `cell_id` is the entity's own cell; `CheckpointData::by_cell` carries it and is unused for keying. | Fixes the seeder's key convention: the seeder writes the **entity's interest cell**, per §6. Until the checkpointer agrees, the actor tier will rewrite seeded rows to a different key on first checkpoint. |
| **P-3** | `orrery_persistd/src/checkpoint/fdb.rs` — `world_range_start`/`world_range_end` | Compute `[w‖bits, w‖bits+1)` — the *exact-cell* span. The doc comment claims this is the subtree; `CellId::subtree_range()` (`cell.rs:228`) says the subtree is `[bits−lsb+1, bits+lsb−1]`. | `read_cold` cannot read a subtree and `delete(shard)` does not clear a shard's rows. The seeder's `wipe` and the cold-cell readback both need the real subtree span. |
| **P-4** | `orrery_persistd/src/actor.rs:247` | `read_snapshot` opens with `let _ = cells;` and returns the entire actor bag, ignoring the requested cells. | An area load for one interest cell returns up to 8×8×8 = 512 cells of entities. The <50 ms first-page-in budget (D16) is unmeasurable, and §13's page-size ladder is meaningless, until this filters by `by_cell`. |
| **P-5** | `orrery_persistd/src/cluster.rs:236` | `Cluster::has_actor` returns `true` for every cell, because `runtime_for` → `RendezvousHasher::owner` always yields an owner for a non-empty node set. | The cold-store fallback never fires under a multi-node `Cluster` — only under the single-runtime router. A seeded-but-not-yet-loaded world reads as empty. `has_actor` must test for a live actor, not a placement answer. |
| **P-6** | `orrery_persistd/src/checkpoint/fdb.rs` | `checkpoint` only ever `set`s rows for entities currently in the bag; nothing clears rows for despawned or removed entities. The `world/…` despawn tombstone of D11 §6 is unimplemented. | **Fixed 2026-08-13.** `world/` values are tag-prefixed: `0x00 ‖ bag` live, `0x01 ‖ postcard(Tombstone)` despawn marker. Checkpoints write markers and clear rows past their GC deadline; `read_cold` and `load` skip markers; recovery rebuilds the marker set. The seeder's patch-delete (§9.4) must write the `0x01`-tagged marker (or rely on the actor's `Despawn` path), and `wipe` (§9.5) still just clears ranges. |
| **P-7** | `orrery_persistd/src/checkpoint/fdb.rs` — `world_key` | The 17-byte key is `b'w' ‖ cell(8) ‖ entity(8)`. There is no `GridId` discriminator, though `JournalRecord`, `DiffUplink` and the `grid/` keyspace family all carry one and D11 §6 calls `cell_id` "grid-relative". | **Fixed 2026-08-13.** The key is `b'w' ‖ grid(4) ‖ cell(8) ‖ entity(8)`; `ckpt/`, subtree spans, scans, `load`/`delete`/`read_cold` and `Subscribe` are grid-scoped end to end. v1 of the seeder still writes **grid 0 only**; the `kepler` nested-grid mode is no longer blocked on storage and can use `[[grid]] id ≠ 0` (the `EncodeCtx.grid` field already exists). |
| **P-8** | `orrery_persistd/src/checkpoint/fdb.rs:126` | `checkpoint` postcard-encodes the **whole `CheckpointData`** — entity bag included — into the single `ckpt/{shard}` value, in addition to writing the per-entity `world/` rows. D11 §6 fixes that row as `(node_id, journal lsn, epoch, time)`. | The bag is stored twice, and one FDB value carries the whole shard: at 256 B/entity plus ~34 B of per-entry framing, a shard passes FDB's 100 KB value limit at **344 entities** (→ A.15). §13's ladder shows `soak` exceeding it by 5.4×. The `ckpt/` row must carry the watermark only. |

**P-1 and P-8 are P2-demo blockers independent of the seeder.** P-2, P-3, P-5 and P-6 are blockers for *seeded* worlds specifically. P-4 gates the latency numbers. P-7 gates one showcase scenario. §14 states which acceptance gates depend on which. **Status: all eight were fixed 2026-08-13; §14's gates can be met on the current tree** (P-6/P-7 fix notes are in the rows above, and each fix ships a regression test).

None of these are speculative: each was read out of the tree at the cited location, and P-3's mismatch is masked in `tests/checkpoint_restore.rs:293` because the only cold-read assertion uses `CellId::ROOT`, which *is* the shard.

---

## 3. The four decisions everything else follows from

### D-A. Layers compose as **fields**, never as entities

Every layer computes a non-negative scalar field over cells; layers fold into named accumulators with an operator algebra (§5.3); realization into rows happens **once**, at the end, in `[[emit]]`.

The reason is the dry run. A superposition of Poisson intensities is Poisson with the summed intensity, so `E[N] = Σ_cells λ(cell)` stays closed-form under arbitrary layer stacking. If layers composed at the entity level, the estimator would degrade to Monte-Carlo the moment a second layer appeared, and `plan` — the most-used subcommand — would have to generate the world to describe it.

### D-B. The operator declares the **count**; generators are weight functions

Conditioning a Poisson process on `N(W) = n` gives exactly `n` draws from the normalized intensity (Kingman 1993) — *how many* and *where* are separable, so you can fix the count and let the generator decide only the shape (→ **A.2.1**). `[[emit]] count = 10_000` is therefore honoured *exactly*, by construction, whatever generator produced the field. Generators never decide how many entities exist; they decide where the mass is.

This is what makes the catalog in §6 usable. "Game of Life produces whatever it produces" is a research toy; "Game of Life shapes the density of exactly 10 000 entities" is a test fixture.

### D-C. Identity is the **derivation path**, cell-local

An entity's `ContentKey` is a hash of *how it was derived* — layer name, emit name, cell, per-cell index, archetype — never of its position, its minted `PersistId`, or any global ordinal. All derivation is local to a cell: per-cell counts come from a hierarchical splitter (§7.1, → **A.2.3**), and archetype apportionment happens **within** a cell (§5.5, → **A.2.4**), never as a global largest-remainder pass.

This is the whole patch story. Under a global scheme, changing `count` from 10 000 to 10 001 shifts every downstream draw and rewrites the world, making §9's manifest diff useless. Under cell-local derivation, a count change perturbs only the cells whose split changed.

### D-D. Two seed roots: `scenario_seed` (public) and `universe_seed` (secret)

`universe_seed` is security-relevant (D9: the verifiable core's per-entity, per-tick RNG is `blake3::keyed_hash(universe_seed, persist_id ‖ tick)`) and lives in the operator's secret store. World *content* derives from a separate, non-secret `scenario_seed`.

Two roots, because deriving content from `universe_seed` would (a) require secret-store credentials on every laptop that runs a dry run, and (b) turn observable world content into an inversion oracle against the RNG that rolls loot and crits. The scenario seed can be committed to git, printed in every report, and stored in `content/version`.

---

## 4. Where the seeder lives

```
crates/orrery_seed/          # Bevy-free library crate (D15)
  src/lib.rs                 #   scenario model, generators, planner, writer, manifest
  src/bin/orrery-seed.rs     #   reference binary, links OpaqueEncoder
```

**A library crate plus a reference binary**, exactly the `orrery_persistd` posture (D12): games link their own `Ruleset`-derived encoder into their own seeder binary; the shipped binary links a built-in opaque encoder sufficient for the demo, the latency rig and CI.

The alternatives, and why not: *a subcommand of `persistd`* couples a batch tool's CLI to a long-running daemon's and drags the gateway/iroh surface into a job that never opens a socket; *a module inside `orrery_persistd`* is closer but makes every `persistd` build carry the generator bank; *a standalone binary with no library* leaves games unable to plug in their encoder, which is the one thing they must be able to do.

**Dependencies.** `orrery_protocol` (types), `orrery_persistd` (keyspace helpers and `FenceStore`, see below), `toml` + `serde`, `blake3`, `rand_chacha`, `foundationdb` behind the `fdb` feature. It must **not** depend on `orrery_spatial`, which is a Bevy crate — so `INTEREST_LEVEL`/`SHARD_LEVEL` (currently `orrery_spatial/src/cell.rs:14,17`) and the metres↔cells conversion move to `orrery_protocol` as a spec delta (§16).

**Dependency posture — a knowing deviation (P2 decision, 2026-08-13).** The
paragraph above rejects the persistd-subcommand shape partly because it "drags
the gateway/iroh surface into a job that never opens a socket". Depending on
`orrery_persistd` as a library currently has the same effect: `journal/mod.rs`
declares `pub mod fjall` unconditionally and `gateway.rs` uses iroh
unconditionally, so `default-features = false` does not compile and the only
buildable form links an LSM store and iroh into a batch tool. P2 accepts that
— it costs build time, not correctness, and the alternative (feature-gating
`journal::fjall` and `gateway`) touches `lib.rs` and `journal/mod.rs` while
several other P2 workstreams own those files. Revisit at P3, when the crate is
quiet enough to gate cleanly.

**Keyspace helpers.** `world_key`, `ckpt_key`, `world_range_start`/`_end` are private free functions in `orrery_persistd/src/checkpoint/fdb.rs`. They become a public `orrery_persistd::keyspace` module — one definition of the keyspace, used by the checkpointer, the cold reader and the seeder alike. This is what stops P-2/P-3 from recurring as a seeder-vs-checkpointer disagreement.

### 4.1 The component-bag seam

`EntityRecord.components` is opaque postcard bytes; the cell actor never decodes game types and neither does the seeder. But a seeder that cannot fill the bag can only write empty entities. The seam is one trait:

```rust
/// A game's bridge from scenario archetypes to component bags.
/// Implemented in the game's crate; linked into the game's seeder binary.
pub trait SeedEncoder: Send + Sync {
    /// Encode one entity's component bag from its archetype and derived context.
    fn encode(&self, ctx: &EncodeCtx<'_>) -> Result<bytes::Bytes, EncodeError>;

    /// Declared bag size for an archetype, for byte-budget estimation without
    /// encoding. Must be an upper bound; `plan` reports measured vs declared.
    fn declared_size(&self, archetype: &str) -> Option<usize>;

    /// Encode one terrain chunk section. `None` if the game has no terrain.
    fn encode_section(&self, ctx: &SectionCtx<'_>) -> Result<Option<bytes::Bytes>, EncodeError> {
        let _ = ctx;
        Ok(None)
    }
}

pub struct EncodeCtx<'a> {
    pub archetype: &'a str,
    pub fields: &'a ArchetypeFields, // resolved from [archetype.<name>] TOML
    pub cell: CellId,
    pub grid: GridId,
    pub local_pos: [f32; 3],         // metres within the cell
    pub content_key: ContentKey,
    pub persist_id: PersistId,
    pub rng: &'a mut ChaCha8Rng,     // seeded from the slot key (§8)
}
```

**Without a linked encoder** the shipped binary uses `OpaqueEncoder`, which emits a postcard-encoded `(schema_version: u16, size: u32, filler: [u8])` bag of a declared size. That is enough for everything the P2 demo measures — row counts, byte volumes, range-scan behaviour, checkpoint sizes, restart recovery — because none of those care what is *in* the bag. It is not enough for a real game, and the tool says so: `plan` prints `payload class: opaque` and `apply` refuses without `--allow-opaque` when `[payload] class = "ruleset"`.

**The hex escape hatch** (`bytes = "0x…"` in an archetype) exists for fixtures and is capped at 4 KiB. It is not a substitute for an encoder; it is how you write the one hand-authored row a regression test needs.

---

## 5. The scenario file

### 5.1 Conventions

**Levels.** `CellId` levels run 0..=21. Level 21 is the interest cell (128 m edge, D16), level 18 the shard cell (8×8×8 interest cells). The seeder speaks levels; metres are a convenience.

**Metres are per-grid.** `CellId` is dimensionless; only `[[grid]].cell_edge_m` gives it a scale. A grid's level-21 half-extent is `2^20 × cell_edge_m`:

| `cell_edge_m` | level-21 half-extent | Use |
|---|---|---|
| 128 (D16 default) | ±134 218 km | the game grid |
| 1 024 000 | ±7.18 AU | a solar-system grid, out to Jupiter |
| 4 096 000 | ±28.7 AU | out to Neptune |

**The seeder never clamps.** `orrery_spatial::cell::clamp_coord` silently clamps out-of-range coordinates — correct for a running client, wrong for an importer. Out-of-range is an error naming the offending value and the grid's extent.

**`CellRef` — three spellings, one canonical.**

```toml
cell = "0xA92492492493D600"                        # canonical raw bits; what the tool prints
cell = { level = 18, xyz = [3, -2, 5] }            # authoring form
cell = { level = 21, m = [384.0, -256.0, 640.0] }  # metres, grid-local
```

The hex form is `CellId::to_bits()` big-endian, so tool output pastes straight back into a config. Every `CellRef` the tool prints is annotated with all three.

**`Bounds` — five shapes**, defaulting to the grid extent:

```toml
bounds = "all"
bounds = { kind = "subtree", cell = { level = 18, xyz = [0, 0, 0] } }
bounds = { kind = "cells",  level = 21, min = [-64, -8, -64], max = [63, 7, 63] }
bounds = { kind = "box",    center = { level = 21, xyz = [0,0,0] }, extent_cells = [64, 8, 64] }
bounds = { kind = "sphere", center = { level = 21, m = [0,0,0] }, radius_m = 8192.0 }
```

`box`/`sphere` snap **outward** to whole cells and the snap is reported. `extent_cells` is a half-extent, so `[64,8,64]` is 128×16×128 = 262 144 cells.

**Do not read §13.2's "Extent (cells)" column as an `extent_cells` value.** That
column is the *full* extent — `demo`'s 64×8×64 is 32 768 interest cells, which
is exactly its metre extent (8 192 m ÷ 128 m) — while `extent_cells` here is a
**half**-extent. Writing the ladder's figure straight into a scenario doubles
every axis and multiplies the cell count by eight: `demo` would get `soak`'s
262 144 cells and miss its occupancy target by 7×. The demo scenario's correct
value is `[32, 4, 32]`.

**`bounds = "all"` is only legal for operators that do not need a normalization sweep.** At level 21 "all" is 2^63 cells; `mask` with `normalize = "max"` would have to scan it. Combining them is a static error (§10, V6) naming the bounded alternative.

**Suffixed scalars.** `"768KiB"`, `"40GiB"`, `"30s"` — the value carries its unit, never the key.

### 5.2 File anatomy

```toml
schema = 1                    # required first key; a breaking surface change bumps it

[scenario]   name, content_build, description, extends, scale, scale_mode
[seed]       scenario seed, derivation context, RNG choice
[payload]    class gate, bag encoding, schema_version
[[grid]]     id, cell_edge_m, parent            — grid 0 is implicit
[archetype.<name>]                              — the component-payload table (§5.5)
[[layer]]    generator + bounds + fold op       — computes a field (§5.3)
[[emit]]     realization: accumulator → rows    (§5.4)
[target]     global targets and tolerance       (§7)
[limits]     hard guards checked before any write
[load]       transaction shape, ordering, concurrency, resume
[profile.<name>]  named overlays
```

**Evaluation order is file order and nothing else.** No implicit dependency graph, no includes beyond a single non-recursive `extends`, no templating. The config stays statically analysable because the entire dry-run feature depends on it — a Go-template layer over this would destroy the tool's headline capability. Maximum nesting depth is 3, enforced by the derive types.

### 5.3 `[[layer]]` and the composition algebra

Every layer does exactly two things: compute a non-negative field `f(cell)` from `kind` + `[layer.params]`, zero outside `bounds`; then fold `f` into the accumulator named by `into` (default `"main"`) with `op`.

```toml
[[layer]]
name   = "caves"       # unique; the seed-tree tag and a ContentKey component
kind   = "ca"          # generator discriminant (§6)
op     = "union"
into   = "main"
bounds = "all"
level  = 21            # the level this layer's field is defined at
spread = "uniform"     # how a coarse layer pushes mass down: uniform | concentrate | hash
weight = 1.0           # blend coefficient
enabled = true

[layer.params]
mode = "quantile"
fill_target = 0.42
```

| `op` | `A' =` | Notes |
|---|---|---|
| `union` (default) | `A + f` | superposition; mass-additive, estimator-exact |
| `blend` | `(1−w)·A + w·f` | preserves total mass only if weights sum to 1; the estimator warns when they do not |
| `mask` | `A · norm(f)` | `normalize = "clamp"` (default) \| `"max"` \| `"none"` |
| `subtract` | `max(A − f, 0)` | carves voids; raises the empty-cell fraction |
| `max` / `min` | `max(A,f)` / `min(A,f)` | idempotent overlays |
| `replace` | `f` where `f > 0`, else `A` | authored content over generated |
| `conditional` | `A · [where]` | multiply by an indicator over named accumulators |

The `conditional` predicate grammar is deliberately **total** — comparisons on `<accumulator>.<stat>` joined by `and`/`or`/`not`, depth ≤ 3, no arithmetic, no calls, no recursion — so the estimator evaluates it symbolically and the dry run stays exact:

```toml
op = "conditional"
where = "solid > 0.15 and solid < 0.85"
```

**Ordering rules.** Layers evaluate in declaration order; a layer may reference only accumulators defined above it (forward references are a static error, making the file a DAG in reading order); `"main"` exists implicitly at 0. Field values clamp to `[0, field_clamp]` (default 64.0) after each fold, and the plan reports how many cells clamped — a non-zero count is almost always a `blend`-weight bug, not intent.

**Conflicts.** Entity emits never conflict: each mints its own `PersistId` block, so two emits into one cell simply both write rows. Terrain emits *do* conflict, because `chunk/{cell_id}/{n}` is one row per (cell, section); the default `on_conflict = "error"` is detected at **plan** time by intersecting occupied-cell sets and reported with both emit names and the offending `CellRef`. `priority`, `replace` and `merge` are explicit opt-ins. A silent clobber is not reachable.

### 5.4 `[[emit]]` — realization

```toml
[[emit]]
name      = "props"
from      = "main"          # accumulator to realize
kind      = "entity"        # entity | terrain
count     = 10_000          # exact, per D-B
level     = 21              # emit level
placement = "hash"          # hash | stratified | centered
archetypes = { crate = 0.6, barrel = 0.3, statue = 0.1 }
```

**Exact-N is realized by hierarchical binomial splitting down the `CellId` octree** (§7.1), so `count` is honoured exactly with per-cell deviation from the target profile bounded at ±1 entity, and output arrives pre-sorted in Morton order with O(depth) memory — a million-entity load never holds more than one FDB batch in RAM and needs no sort pass.

`placement = "hash"` is the default and derives an entity's position inside its cell from its slot key alone, so position is independent of the cell's population. `stratified` gives better in-cell spacing but couples each entity's position to the cell's count; it is offered, documented as count-coupled, and is not the default (§9.3 explains what that costs a patch).

### 5.5 `[archetype.<name>]` — the payload table

```toml
[archetype.crate]
components   = ["Transform", "Prop", "Health"]
declared_size = "192B"        # for byte estimation without encoding
schema_version = 3            # per-component versions live in the bag (D11 §16)

[archetype.crate.fields]
health = { min = 10, max = 100 }
model  = "crate_wood_01"
```

`fields` is passed through to `SeedEncoder::encode` as `ArchetypeFields`; the seeder does not interpret it. **Archetype selection is per-cell**: within a cell, the weighted multiset is apportioned by largest remainder (the parliamentary-seat method, → **A.2.4**) over *that cell's* count and then permuted by the cell key. Selection is therefore stable when a distant cell's count changes — the D-C property that makes patching work.

---

## 6. The generator bank

Thirteen `kind` values ship in v1. Every one is a pure function of `(K_layer, cell)` restricted to `bounds`, evaluated lazily per cell — the seeder never materializes a dense grid, because a dense level-21 grid is 2^63 cells.

The bank was chosen from a survey of 95 candidate techniques across six families. The selection criteria, in order: (1) does it produce a density pattern nothing else in the bank produces? (2) can it be evaluated per-cell without global state? (3) does it stress a specific persistd subsystem? Section 6.7 records what was rejected and why.

### 6.1 Statistical fields — the workhorses

| `kind` | Field | Primary knobs | Why it is in v1 |
|---|---|---|---|
| `uniform` | constant λ | `intensity` | The null hypothesis and the calibration arm. Structurally cannot produce a hotspot, so any variance measured under it is the rig's own noise. |
| `noise` | fBm / ridged / billow / worley → `exp(σ·f)` | `basis`, `base_wavelength_m`, `octaves`, `lacunarity`, `gain`, `skew_gini`, `warp_amplitude_m` | Spatially correlated skew with a closed-form dial. A sum of decorrelated octaves is approximately Gaussian by the central limit theorem, so `exp(σ·f)` is lognormal — a log-Gaussian Cox process — and a lognormal's inequality measures are closed form: `Gini = erf(σ/2)`, `CV = √(e^{σ²}−1)`. So `skew_gini` inverts exactly (`σ = 2·erfinv(G)`) instead of being a magic amplitude (→ **A.4.2**, **A.4.3**; fBm's octave/lacunarity/gain knobs are the musical-harmonics construction spelled out there). |
| `zipf` | rank-order weight ladder | `s` \| `gini` \| (`hot_fraction`, `hot_share`), `rank_layout` | The benchmark-comparable skew ladder (YCSB lineage): `w_k ∝ k^(−s)`, the law that governs word frequencies and city sizes (→ **A.5**). **The only knob in the bank that varies spatial arrangement while holding marginal skew fixed**: `rank_layout = "morton"` makes hot cells contiguous in the keyspace — one hot FDB range, the [#11510](https://github.com/apple/foundationdb/issues/11510) write-hotspot pattern named in R-7 — while `"scattered"` hashes the rank (YCSB's `ScrambledZipfianGenerator`) so hot cells spread across shards. |
| `cluster` | Thomas / Matérn cluster process | `parents`, `sigma_m`, `kernel`, `size_dist`, `size_zipf_s` | Clustering, voids and hotspots from two knobs — scatter `κ` invisible parents, then scatter offspring around each with Gaussian spread `σ` — with an analytic pair correlation function (→ **A.6.1**). Only `σ/cell_edge` matters: `σ ≤ 0.5·edge` packs a cluster into 1–8 interest cells, `σ ≥ 2·edge` is indistinguishable from uniform. **Equal-size clusters cannot make a hotspot** — the survey measured a hottest cell of 38 entities at N=10 000 — so `size_dist = "zipf"` is what actually trips the split threshold. |

**A hard cap on `noise.octaves`:** `octaves ≤ 1 + floor(log2(base_wavelength_m / (2·cell_edge_m)))`. Sub-cell octaves alias away under per-cell aggregation and *reduce* realized skew, so an operator adding octaves to "make it rougher" gets a flatter world. The cap is enforced, not advised.

### 6.2 Fractal and recursive subdivision — the exact-fit family

`CellId` is a binary octree: `children()` returns exactly 8, level *L* has edge `128·2^(21−L)` m, and a cell's whole subtree is the contiguous u64 range `subtree_range()`. A "keep k of 8 octants" IFS is therefore not *modelled by* Morton prefixes — it **is** a set of Morton prefixes. The generator's output type and the storage key type are the same type.

| `kind` | Field | Primary knobs | Why it is in v1 |
|---|---|---|---|
| `octree_ifs` | keep-k-of-8 Morton subdivision | `octant_mask`, `depth`, `fill_subtree`, `terminal_level` | Exact count `k^depth` — not sampled, not estimated. The fractal dimension `D = log2(popcount(mask))` is dialled by popcount, so `k=4` is a surface (`D=2`, the Sierpinski tetrahedron) and `k=7` is the nearest native analogue of a Menger sponge (→ **A.7.1**). Shard assignment is `ancestor_at(18)`, pure bit-masking. Emission is pre-sorted in Morton order. The split concentration ratio is `8/k` at **every** level forever — the cleanest possible adversary for the hotspot splitter. |
| `octree_branching` | Galton–Watson on the 8-ary tree | `split_prob` (critical at **1/8**), `max_depth`, `level_prob_override` | A Galton–Watson branching process: mean offspring is `8p`, so `p = 1/8` is the knife edge between certain extinction and runaway growth — the surname-extinction problem on an octree (→ **A.7.3**). The only generator that populates many `CellId` levels at once — the input the shard tier was designed for and has never received. |
| `percolation` | site/bond at *p*, Hoshen–Kopelman clusters | `occupancy` (simple-cubic site *p_c* = 0.3116077), `connectivity`, `min_cluster_size` | A calibrated power law — at `p_c` the cluster-size distribution is exactly `n_s ∝ s^(−τ)` with a published Fisher exponent `τ ≈ 2.189` (→ **A.7.4**) — for about ten lines of code, and Morton-**incompressible** output — the necessary control against the prefix-optimised generators. |
| `heightfield` | fBm / diamond-square + thermal or hydraulic erosion | `variant`, `hurst`, `octaves`, `vertical_scale_m`, `erosion` | The only source of terrain surfaces. The Hurst exponent `H` is the one roughness knob; surface box dimension `D = 3 − H` doubles as a dial on Morton run-compression (→ **A.4.5**, with diamond-square pseudocode). |

**Three corrections worth recording**, because they are the kind of thing that gets specified wrong:

1. **The Menger sponge does not fit.** It is ternary — 27 subcubes, keep 20, `D = log20/log3 = 2.7268`. No octree level has an edge one third of another's, so the sponge can only be *voxelized*, forfeiting the exact-prefix property that motivated it. Same for 3D Cantor dust. The native substitutes are mask popcount 7 (`D = 2.8074`, closest to the sponge) and 6 (octahedron flake, `D = 2.5850`). The Sierpinski tetrahedron **does** fit exactly: 4 of 8, `D = 2`.
2. **A prefix set is not a small number of ranges.** Run-compression equals the mean run length of the octant mask: a contiguous mask `{0..k−1}` gives k cells per run, but the tetrix mask `{0,3,5,6}` gives 1.33 and an even-octant mask gives 1.00. Compressible masks are axis-aligned slabs — geometrically degenerate. The resolution is `fill_subtree`/`terminal_level`: **stop the recursion early and populate whole subtrees**, so each retained cell is one large contiguous range regardless of mask, and the range count is `k^depth` by construction. These are the primary size knobs, not the mask.
3. **Sorted bulk writes are FDB's documented write-hotspot anti-pattern** (#11510, already cited in [08-persistence.md](08-persistence.md) §12). Plan in prefix order, but *dispatch* shuffled across pre-split boundaries (§10.3).

### 6.3 Cellular automata — the correlation spectrum

One engine, five modes. `(dim, neighborhood, radius, states, birth_set, survive_set, mode ∈ {life, generations, cyclic, quantile})` covers 2D B/S rulestrings, 3D Bays/Softology rules, cyclic CA and the rank-order cave operator in roughly 300 lines. `B3/S23` reads "born with exactly 3 live neighbours, survives with 2 or 3" (→ **A.8.1**), and the neighbour count is computed by a **separable box-sum** — three axis passes, 6 adds per cell, regardless of the 26-neighbourhood, the same trick as a separable Gaussian blur (→ **A.8.2**). Never write the 26-gather.

```toml
[[layer]]
kind = "ca"
[layer.params]
mode = "quantile"          # life | generations | cyclic | quantile
dim  = 3
rule = "B3/S23"
neighborhood = "moore"
fill_target = 0.42
stop = { stable_within = 16 }
slab = "extrude"           # extrude | spacetime | none
```

**The four modes, plainly.** `life` is the classic two-state B/S rule. `generations` adds refractory states — a cell that dies counts down through `states−1 … 1` before it can be reborn, which is what keeps 3D rules off both the extinction and saturation attractors (→ **A.8.3**). `cyclic` runs a state wheel where a cell advances only when enough neighbours are already one step ahead, self-organising into rotating spiral waves. `quantile` is the rank-order cave operator: instead of a fixed birth/survival test it keeps the top `k = fill_target · n_cells` cells by neighbour count each iteration — "pass the top 450 candidates" rather than "pass everyone above 70" (→ **A.8.4**). Because the threshold moves to hit the quota, `quantile` is the only mode in the bank that lands on an exact fill fraction with no search, no bisection and no rule-dependent tuning, while still producing organic connected structure.

**What this family uniquely gives the seeder** is a *spectrum of spatial correlation* no noise function can produce: at a fixed entity count, per-shard-cell skew dials from Poisson (CV 0.33) to severe (CV 2.92, 75.6% of shard cells empty) purely by iteration count, and blob size dials independently by kernel radius. That is exactly the A/B that persistd testing needs — hold entity count fixed, vary only the distribution, and watch HRW skew, `ckpt/{shard}` value size, cold-cell scan tails and the split path move.

Measured density signatures for the 2D rulestring bank (512² torus, 50% soup, 8×8 block statistics, where an 8×8 block is one level-18 shard cell under an origin aligned to a multiple of 8):

| Rule | fill @100 / @1000 / @5000 | block CV | % empty shard-blocks | max/mean |
|---|---|---|---|---|
| B3/S23 (Life) | .0970 / .0427 / .0290 | 1.345 | 53.6% | 8.08 |
| B36/S23 (HighLife) | .1171 / .0324 / .0203 | 1.682 | 66.7% | 10.76 |
| B368/S245 (Move) | .0790 / .0049 / .0047 | 3.538 | 90.6% | **36.45** |
| B38/S23 (Pedestrian) | .0245 | 1.578 | 61.7% | 15.92 |
| B4678/S35678 (Anneal) | .4929 / .4266 / .2795 | 1.553 | 66.4% | 3.58 |
| B3678/S34678 (Day&Night) | .5120 / .5354 / .5704 | 0.833 | 35.7% | 1.75 |
| B5678/S45678 (Vote) | frozen .4964 by g=10 | 0.544 | 2.9% | 2.01 |
| B2/S (Seeds) | .2107 / .2121 / .2119 | 0.289 | 0.0% | 2.14 |
| B3/S12345 (Maze) | frozen .5512 by g=10 | 0.056 | 0.0% | **1.19** |

One string in TOML spans max/mean from 1.19 to 36.45 at constant grid size. That is the highest pattern-variety-per-line-of-code in the bank, and every number regenerates from the recorded seed.

**Two engineering rules the bank forces:**

- **`stop = { stable_within = 16 }` matters.** Maze, Vote, Coral and Life-without-Death all freeze by g=10 and otherwise burn 4 990 wasted generations. The stop is a 16-slot ring buffer of packed-grid hashes, which catches still lifes and every oscillator up to period 16.
- **Anti-extinction screening is mandatory for `dim = 3`.** Before simulating, solve the mean-field recurrence `ρ' = ρ·P(n∈S) + (1−ρ)·P(n∈B)` with `n ~ Binomial(N, ρ)`, `N = 26` for 3D Moore — pretend cells are independent and ask where the live fraction settles (→ **A.8.5**). A rule whose only interior fixed point is unstable will die out or saturate; the plan aborts with that diagnosis instead of burning the generation budget. Twenty lines, no simulation. `states` defaults to 4 (Generations-style refractory decay), which is what keeps 3D rules off both attractors.

**Non-monotone knobs are declared.** For `mode = "life"`, population versus `generations` is oscillatory, so bisecting on it is invalid; the solvable knob is `soup_density` and `generations` stays a fixed input. Naming a non-monotone knob in `[target] solve.knob` is a config error with that message.

### 6.4 Structured content — the patchable family

These are the generators that serve §17's actual purpose. What separates them from the statistical bank is a **derivation path**: content that can be named by how it was made (`lot:{key}/floor:3/bay:2/window`), which is precisely what makes a `ContentKey` stable across a parameter change.

| `kind` | Field | Primary knobs | Why it is in v1 |
|---|---|---|---|
| `stamp` | hash-priority blue-noise prefab scatter | `min_spacing`, `rounds`, `sampler`, `density_field` | The pragmatic 90% case for real content, per-cell parallel and streamable, and the only sampler with a **provable** patch blast radius — exactly `rounds · min_spacing`, because a candidate's fate depends only on whether a higher-hash candidate sits within the spacing radius (→ **A.12.3**). Bridson's algorithm is offered but is not the default: its global active list destroys locality (→ **A.12.2**). |
| `explicit` | an authored list of cells/positions | `entries` | Set pieces, quest hubs, spawn points. Always exact, never sampled. |
| `import` | a serialized fragment, translated and repeated | `path`, `displacement_cells`, `repeat` | OpenSim OAR's `--merge`/`--displacement` semantics: stamp a known-good fragment. Repeating produces perfectly periodic density — a deliberately adversarial input for HRW. |

Chunked simple-tiled **WFC / model synthesis** (Merrell's modification-in-blocks) — constraint propagation over a tile grid, structurally a Sudoku solver whose rules are tile adjacencies (→ **A.13**) — is the strongest authored-looking generator in the survey and is specified as the first v2 addition, not v1. Block-wise solving is not merely a scaling trick, it is the *patch unit*: a block plus its margin is the reachable set of any change, and at `block = 8 slots × tile_size = 4 m = 32 m` a block maps 1:1 onto a `chunk/{cell}/{n}` section and 64:1 onto a 128 m interest cell. It is deferred because it needs a tile-set authoring format that does not exist yet, and v1 must not block on one.

Overlapping-model WFC is rejected outright: 10–100× the memory in 3D for an exemplar you cannot parameterize, with no derivation path at all.

### 6.5 Movement and dynamics — worlds that move

A seeded world that then *evolves* under scripted motion is what exercises cross-cell handoff, hotspot formation and split, interest-set churn, and the diff-uplink write path. These generators emit an initial state **and** a trajectory program (§12.2).

| `kind` | Field / motion | Primary knobs | Why it is in v1 |
|---|---|---|---|
| `kepler` | closed-form orbital bodies and belts | `bodies`, `belt`, `time_scale` | The project is called Orrery. More usefully: the only trajectory that is a **stateless, seekable, closed-form function of `(entity, tick)`** — six orbital elements, Kepler's equation solved by four Newton iterations, and a rotation (→ **A.9.1**) — which is what makes a trajectory script viable as *parameters* rather than as a multi-gigabyte trace. It is also the only route to exercising nested `GridId` frames ([01-spatial-model.md](01-spatial-model.md) §13) — fully specified, entirely untested, and gated on **P-7**. |
| `profile` | equilibrium-profile sampling + test particles | `family` (Plummer/Hernquist/King/Kuzmin/exponential disc), `scale_radius_m`, `motion` | Closed-form astrophysical density profiles that invert to one-line inverse-CDF samplers (→ **A.10**) — the seeder's density vocabulary, and the only *statistically stationary* load source — what a multi-hour soak and the `kill -9` harness need. |
| `boids` | Reynolds flocking | `flocks`, `agents_per_flock`, `speed_ms`, `radii`, `weights`, `phase_lock` | Three local steering rules — separation, alignment, cohesion (→ **A.11.1**) — and flocking emerges. The single best stress for handoff, hysteresis and interest-set churn, because it is the only technique that makes cell crossings **correlated across entities**. |

The measured numbers settle two design arguments outright:

**Clustering is not optional if you want to find HRW imbalance.** 10 000 entities in a uniform 2 560 m box occupy 64 shards at 4.4× max/mean skew. The same 10 000 in a Plummer sphere (a = 768 m) occupy 687 shards at **53×**; Hernquist gives **138×**; a shard-centred Hernquist cusp gives **2 070×**. Uniform seeding structurally cannot find `RendezvousHasher` imbalance.

**Correlated crossings are not reproducible by independent motion.** A 100-agent flock at 15 m/s with a 60 m diameter re-keys its entire membership in a 4 s burst — 25 `Rekey`/s — every 5.7 s, a 70% duty cycle. At 200 agents / 25 m/s / 90 m the duty exceeds 100%: the flock permanently straddles a boundary. With `phase_lock = 1.0`, 50 flocks burst simultaneously, a 50× spike no Poisson capacity model predicts.

**Plus a `[schedule]` modulator**, which is not a generator: time-varying attractor weights implementing diurnal and migration cycles. It costs almost nothing and it is the only thing in the bank that forces a shard **merge** on demand — every clustered generator gives splits for free, but nothing empties a shard on a timetable.

### 6.6 Choosing a generator

| If you want to stress… | Use | Because |
|---|---|---|
| nothing (calibration baseline) | `uniform` | any variance you see is the rig |
| HRW placement skew | `profile` (Hernquist/Plummer) | 53×–2 070× max/mean vs uniform's 4.4× |
| the hotspot splitter | `octree_ifs` | concentration ratio `8/k` at every level, forever |
| the FDB write-hotspot path (R-7) | `zipf` with `rank_layout = "morton"` | hot cells contiguous ⇒ one hot key range |
| the same skew *without* the hot range | `zipf` with `rank_layout = "scattered"` | isolates keyspace effects from load effects |
| cold-cell scans over sparse neighbourhoods | `ca` (Life, Move) | 54–91% empty shard blocks |
| cold-cell scans over dense neighbourhoods | `ca` (Maze, Seeds, Coral) | 0% empty, near-uniform |
| many `CellId` levels at once | `octree_branching` | the only generator that does |
| Morton-incompressible layout | `percolation` | the control against prefix-optimised generators |
| cross-cell handoff and interest churn | `boids` | correlated crossings, 70–100% duty |
| shard *merge* | any + `[schedule]` | nothing else empties a shard on a timetable |
| nested reference frames | `kepler` | gated on P-7 |
| terrain rows and size limits | `heightfield` | the only `chunk/` writer |
| a realistic patch workload | `ca`, re-run at generation `g+Δ` | large, reproducible, density-preserving diff |

### 6.7 Rejected, and why

Recorded so the question is not reopened without new information.

| Technique | Verdict |
|---|---|
| Lenia / SmoothLife / continuous CA | Beautiful, continuous-state, float-heavy — breaks §8's integer determinism contract for no pattern the discrete bank lacks. |
| Wireworld | Models circuits, not densities. |
| Rule 30 as the seeder's PRNG | blake3 is faster, better distributed, and already required. |
| Chaos game (IFS by random iteration) | Same attractor as `octree_ifs` with sampling noise and no exact count. |
| Menger sponge / 3D Cantor dust | Ternary; does not fit an octree (§6.2). |
| Mandelbulb / quaternion Julia | Distance estimation is float-iterative per cell; expensive and non-deterministic across libm. |
| Kleinian limit sets | Same, plus no size knob an operator would recognise. |
| Overlapping-model WFC | 10–100× memory in 3D, unparameterizable, no derivation path. |
| Strauss / Gibbs point processes | MCMC-simulated: no closed-form estimator, so the dry run dies. |
| Determinantal point processes | Elegant repulsion, O(n³) eigendecomposition, and `stamp` covers the use case. |
| Social force model / continuum crowds | Better crowd realism than `boids`, none of which changes a cell-crossing pattern. |
| Barnes–Hut self-gravitating N-body | The dynamics are the point of an astrophysics code and irrelevant here; `profile` + test particles gives the same density at closed form. |
| FDB 7.4 `bulkdump`/`bulkload` SST ingest | The fastest possible path, and the right answer eventually — but it pins us to 7.4 while D11 pins 7.3.x. Revisit at the 7.4 upgrade. |

---

## 7. Targeting and the dry run

### 7.1 Exact-N by hierarchical binomial splitting

`[[emit]] count = N` is realized by recursively splitting `N` down the `CellId` octree: at each node, distribute the parent's count among its eight children in proportion to their accumulated field mass, using integer arithmetic with a deterministic remainder rule (pseudocode: → **A.2.3**).

This single mechanism delivers four things nothing else gives together:

1. **Exact counts**, with per-cell deviation from the target profile bounded at ±1 entity (**systematic** rather than multinomial allocation — deal the deck round-robin instead of rolling a die `N` times, → **A.2.2** — which measured max `|count − N·w|` of 0.995 versus 6.2 for multinomial at `N` = 10 000 over 32 768 cells).
2. **An O(depth) ≈ 21-draw density oracle**: "how many entities land in this shard?" is answered by a 21-step descent, with no world generation — which is what makes `explain --cell` work on a laptop against a 34-million-entity world.
3. **Embarrassingly parallel generation**: each worker owns a subtree, which is a contiguous `CellId` range, which is a disjoint FDB key range. No conflict ranges, no merge step.
4. **Single-cell regeneration** for resume and repair.

The O(depth) oracle property holds for **closed-form fields only** (`uniform`, `noise`, `zipf`, `cluster`, `octree_*`, `profile`, `kepler`). For iterative generators (`ca`, `percolation`, `heightfield`) and for `mask`/`conditional` folds over them, the field is not evaluable at one cell without evaluating its neighbourhood, so the oracle degrades to "generate the layer's bounded region". The tool states which tier a scenario is in, in the plan header. **This is a real limitation and the spec does not paper over it**: a `ca` layer over a large bounds costs generation time in `plan`, not milliseconds.

### 7.2 What the operator declares

```toml
[target]
count            = 10_000       # exact (D-B)
hot_shard_share  = 0.08         # fraction of entities in the hottest shard
hotspots         = 3
gini             = 0.42         # Gini of the per-cell count distribution
occupied_fraction = 0.30
max_bytes        = "40GiB"      # storage-cost inversion
tolerance        = 0.05         # for the targets that are not exact
solve            = { knob = "sigma_m" }   # which layer param the solver may move
```

| Target | Inversion |
|---|---|
| `count` | exact, by construction (§7.1) |
| `gini`, `skew` | closed form for `noise` (`Gini = erf(σ/2)`, → **A.4.3**) and `zipf`; numeric bisection otherwise, against the **integer** realized distribution, not the continuous weight distribution. Note the **Gini floor** (→ **A.3.4**): at low `λ` the integer rounding alone produces a large Gini, and no scenario can report below it |
| `hot_shard_share`, `hotspots` | constrained water-filling: reserve the declared mass for the named shards, distribute the remainder by the field |
| `occupied_fraction` | support-mask thinning, or `N = k^L` for `octree_ifs` |
| `max_bytes` | cost-model inversion: bytes → rows → entities, via §13.1's per-row model |

**The realized distribution is integer, and the solver must bisect against it.** Bisecting `s` against the continuous Zipf weights and then rounding gives an answer that misses the target — at low λ the rounding *is* the distribution. This is why `tolerance` exists and why the plan reports achieved-vs-target for every target, not just the ones that failed.

**Over-constraint is detected, not silently resolved.** Declaring `count`, `gini` and `occupied_fraction` together over-determines a one-knob solve; the tool reports which pair is satisfiable, the residual on the third, and names the extra knob that would close it.

### 7.3 The three-tier dry run

`plan` is the default verb. **`orrery-seed run` without `--apply` never writes** — the Terraform posture, adopted deliberately.

| Tier | Cost | What it gives |
|---|---|---|
| **analytic** (default for closed-form scenarios) | milliseconds, no cluster | exact `E[N]`, per-shard distribution, byte estimate, transaction count, wall-clock estimate |
| **sampled** (`--sample F`, default 0.01 for iterative scenarios) | seconds | the above, plus measured encoder output size on a sampled subset |
| **probe** (`--probe`) | seconds, needs a cluster | the above, plus FDB `\xff\xff/status/json` preflight: cluster health, free space, current write load, existing rows in the target ranges |

`--sample` defaults to 0 (pure analytic) for closed-form scenarios and to 0.01 only where the analytic tier is unavailable, so the cheapest and most-used command stays cheap. The plan header always names which tier ran.

---

## 8. The determinism contract

Stated precisely, because "deterministic" claimed loosely is worse than not claimed.

**Guaranteed bit-identical across platforms, compilers, thread counts and runs:** every entity's `ContentKey`; every cell's entity count; the archetype assigned to every entity; the set of cells written; the manifest and its digest.

**Not guaranteed bit-identical across platforms:** float-valued *content* inside a bag (positions within a cell, orbital anomalies), because it goes through `libm`. It is guaranteed identical for a fixed toolchain, and the manifest records the toolchain, so a golden-manifest CI test is valid within a pinned build and is expected to shift on a toolchain bump.

**How the first list is achieved:**

1. **No generator may consume a global sequential RNG.** Every draw is addressed by `(layer, cell, index)`. This one rule buys order-independence, parallelism, resumability and single-cell repair; a `StdRng` consumed in iteration order silently breaks all four, because inserting one entity shifts every subsequent draw on the far side of the world.

2. **The seed tree**, with domain tags so a layer name cannot collide with a cell id's byte pattern:

   ```
   K_root  = blake3::derive_key(seed.context, seed_material)   // context "orrery.seeder.v1"
   K_layer = blake3::keyed_hash(K_root,  b"L" ‖ layer_name)
   K_cell  = blake3::keyed_hash(K_layer, b"C" ‖ cell.to_bits().to_be_bytes())
   K_slot  = blake3::keyed_hash(K_cell,  b"E" ‖ index.to_le_bytes())
   rng     = ChaCha8Rng::from_seed(K_slot)
   ```

   Deliberately the same idiom as VC-3's `blake3::keyed_hash(universe_seed, persist_id ‖ tick)`, and deliberately a different root (D-D).

3. **Fields are quantized before they decide anything.** A generator's field is computed in `f64` but **rounded to Q16.16 fixed point** — a 32-bit integer read as `i/65536` (→ **A.14.4**) — before any comparison, threshold, accumulation or split. Every count-determining path is then integer, and the splitter's arithmetic is `u128`. This is the fix for the obvious hole: `integer_only = true` on the splitter alone is worthless when `exp`, `erf` and fBm feed it, because a libm change flips one cell's count, which flips a `ContentKey`, which rewrites the world. The quantization boundary is the contract.

4. **No `HashMap` iteration in any reduction.** Accumulators are keyed `BTreeMap` or sorted vectors. Same-binary, different-thread-count runs must agree.

5. **The seed is an output.** `scenario = "random"` draws from the OS and prints the copy-pasteable form as the *first* line of output, before anything else happens. Every plan, report, failure message and the `content/version` row carries it, plus the digest of the resolved config — a seed alone does not identify a world if the config changed.

**Secret content.** `[[layer]] secret = true` routes that layer's *content* substream (bag field values) through a third root derived from a `content_secret_seed` in the secret store, while structure — counts, occupancy — stays on the public stream so dry-run totals remain exact **without the secret**. Used for pre-placed loot a player must not be able to precompute.

---

## 9. Identity, manifest, diff and patch

### 9.1 `ContentKey`

```
ContentKey = blake3(b"orrery.ck.v1" ‖ scenario_name ‖ emit_name ‖ layer_name
                    ‖ grid ‖ cell.to_bits() ‖ index ‖ archetype)[..16]
```

A function of the **derivation path** only (D-C). Not of position, not of the minted `PersistId`, not of a global ordinal.

### 9.2 `PersistId` allocation

`pid/next` is an FDB atomic-add allocator (D11 §6). The seeder takes **block grants**, default 4096, one block per worker, and records the mapping in an `idmap` subspace:

```
seedmap/{content_key}  →  (PersistId, grid, cell, first_seen_build)
```

The idmap is what makes a re-seed *not* renumber the world: an entity whose `ContentKey` is unchanged keeps its `PersistId` across builds, which is the precondition for `world/` row identity, for `lease/{entity_id}` continuity, and for the whole patch flow. A `ContentKey` absent from the idmap is a new entity and draws from the block.

This composes with peer-side block grants (D11 §4) because both draw from one `pid/next`: designed and dynamic entities share one id space, per §17.

### 9.3 The manifest

One entry per seeded row:

```
(ContentKey, PersistId, grid, cell, value_digest, byte_len, archetype, layer, emit)
```

**`value_digest` covers the component bag only** — never the key, and never the
storage value's one-byte live/tombstone tag (P-6). The tag is storage framing,
not content: a tombstone carries no bag at all, so a retire shows up in the
manifest as a *presence* change rather than a digest change, and the seeder can
compute the digest from `SeedEncoder`'s output before it knows anything about
how the row will be framed. Pinning it on this side is what makes gate A4
("identical manifest digest, zero rows changed") mean the same thing to the
seeder, to `verify --full`, and to a cell actor re-checkpointing an untouched
row. A digest over the key would be a function of the minted `PersistId` and the `CellId`, which makes the "same content, moved cell" case (a `Rekey`) arithmetically undetectable and makes manifests non-reproducible across clusters that allocated ids in a different order. Location lives in its own `(grid, cell)` field, so a move is a location diff with an unchanged digest, which is exactly what the patcher needs to see.

Canonical order is **`(grid, cell, ContentKey)` ascending** — generation order, so the manifest streams out without a sort pass. (Sorting by `ContentKey`, which is uniformly random, would require an external sort: 470 MB at 10 M entities.)

`content/version` records `(content_build, manifest_digest, scenario_seed, config_digest, toolchain, seeded_at)`.

### 9.4 Diff and three-way patch

On a later deploy, the seeder generates the new manifest, loads the recorded one, and diffs by `ContentKey`. For each changed key it performs a three-way merge:

- **base** = the digest recorded in the last seeded manifest
- **ours** = the digest of the row currently in FDB
- **theirs** = the digest of the newly generated row

| base vs ours | base vs theirs | Outcome |
|---|---|---|
| same | same | no-op |
| same | differs | **patch**: write `theirs` |
| differs | same | **keep**: the player modified it and content did not change |
| differs | differs | **conflict**: hand to `Ruleset::merge_policy(ContentKey, base, ours, theirs)`; default is `keep` |
| — | key absent in new manifest | **retire**: tombstone, subject to §2 P-6 |
| key absent in old manifest | — | **create** |

**One honest limitation.** The durable `world/` row is a whole component bag, so `ours` is a bag-level digest. Detecting *which component* a player changed requires decoding the bag, which requires the `Ruleset`. So the default merge granularity is the whole entity: a player who moved a seeded crate one metre blocks a content update to that crate's material. Games that want finer granularity implement `merge_policy` with per-component awareness; the seeder passes all three bags through and does not pretend to understand them. This is stated as a limitation rather than designed around because the alternative — the seeder decoding game components — violates the seam that makes the whole persistence tier engine-agnostic.

**Patch blast radius is a reported quantity.** `stamp` has a provable radius of `rounds · min_spacing`. `octree_ifs`, `explicit` and `import` have zero radius outside the changed subtree. `ca` and `percolation` have unbounded radius — a one-cell change propagates at one cell per generation — and `plan --diff` reports the affected-cell count so the operator learns it before applying, not after.

### 9.5 Wipe and re-seed

`wipe` clears the scenario's `world/`, `chunk/` and `seedmap/` ranges by real subtree spans (blocked on **P-3**), then clears `content/version`. It requires `--yes` plus the `content_build` string typed back, refuses outright when any `actor/{shard}` fence row in range is live, and refuses when `[limits] protect = true` — the production-wipe guard.

---

## 10. Validation

Checked before any write, in this order, with the config span quoted in the error:

| # | Check |
|---|---|
| V1 | `schema` is known; all keys recognised (unknown keys are errors, not warnings — a typo'd generator param must not silently take a default) |
| V2 | every `CellRef` is in range for its grid's extent; no clamping |
| V3 | layer accumulator references are backward-only; the file is a DAG in reading order |
| V4 | every named archetype exists and, when `[payload] class = "ruleset"`, is encodable by the linked encoder |
| V5 | `noise.octaves` within the sub-cell cap (§6.1); non-monotone knobs not named in `solve.knob` (§6.3) |
| V6 | `bounds = "all"` not combined with an operator needing a normalization sweep (§5.1) |
| V7 | terrain emit conflicts, by intersecting occupied-cell sets (§5.3) |
| V8 | targets not over-constrained beyond the declared knobs (§7.2) |
| V9 | projected `ckpt/{shard}` value size, `world/` row size and `chunk/` shard size against FDB limits — **an over-limit projection is an error, not a warning** |
| V10 | `[limits]` guards: `max_entities`, `max_bytes`, `max_wall_clock`, `protect` |
| V11 | cluster preflight (with `--probe`): health, free space, existing rows in target ranges |

Errors quote the offending line and name the fix:

```
error: noise.octaves = 8 exceeds the sub-cell cap of 4
  --> scenarios/p2demo.toml:41:11
   |
41 |   octaves = 8
   |             ^ base_wavelength_m = 2048 with cell_edge_m = 128 admits at most 4
   |
   = note: octaves beyond the cap alias away under per-cell aggregation and reduce
           realised skew. To roughen the field, lower base_wavelength_m instead.
```

---

## 11. The write path and actor-tier safety

### 11.1 Direct FDB batch load

§17 mandates direct FDB batch loads — no gateway, no journal, because there is nothing to replay yet. Rows are written through the shared `orrery_persistd::keyspace` helpers (§4).

| Constraint | Value | Consequence |
|---|---|---|
| FDB transaction size | 10 MB | target **768 KiB** per transaction: well inside the limit, and small enough that a retry is cheap |
| FDB transaction duration | 5 s | a 768 KiB batch commits in single-digit ms; never at risk |
| FDB value size | 100 KB | **a hard error, not a split row** (P2 decision, → [08-persistence.md](08-persistence.md) §6): the reader identifies a `world/` row by its exact 21-byte key length, so a suffixed row would be invisible to `load` and `read_cold`. `plan` rejects an over-limit projection at V9; nothing writes one. Split rows are a P3 item |
| Writes are blind | — | no reads, so no conflict ranges: the loader is `set`-only and transactions never conflict with each other |

Concurrency is latency-governed: start at 8 in-flight transactions, additively increase while commit p99 stays under 20 ms, multiplicatively back off above it. Measured throughput is reported, never assumed.

**Idempotency.** Every write is a byte-identical overwrite of a pure function of `(ContentKey → row)`, so a re-run of an interrupted load is safe. Resume is a per-subtree completion marker in `seedprog/{emit}/{cell}`, not a global cursor — a global cursor is only valid if generation order is total and stable across a config change, which it is not.

### 11.2 Ordering: plan sorted, dispatch shuffled

Generate in Morton order (free from §7.1, gives locality and streaming), but **dispatch batches shuffled across pre-split boundaries**. Purely sequential key writes are FDB's documented write-hotspot pattern (#11510, R-7): they concentrate all write load on one storage team at a time. The shuffle is deterministic — a permutation of batch indices derived from `K_root` — so it does not cost reproducibility.

### 11.3 Pre-split and placement warm-up

The seeder knows the density distribution before writing, which no other component ever does. It uses it twice:

- **`actor/{shard}` rows** are pre-created at the level the projected density warrants, using the existing `FenceStore` (`orrery_persistd::fence`), so the first cluster start does not discover the hotspot at load time.
- **FDB range boundaries** are pre-warmed by writing and clearing sentinel keys at the projected shard boundaries, so the initial load does not fight the data distributor.

`hot_shard_placement = "hrw_adversarial"` calls the build's own `RendezvousHasher::owner()` (`placement.rs:66` — highest-random-weight placement, → **A.14.2**) during planning to pick hot shards that collide on one node — so the world is adversarial to the placement function *in this binary*, not to a model of it.

### 11.4 Actor-tier interaction — the correctness core

The failure mode this section exists to prevent: you seed a world into `world/` rows, a running cell actor holds an empty in-memory bag for that shard, its next checkpoint fires, and it overwrites your seed with nothing.

Three modes:

| Mode | Precondition | Behaviour |
|---|---|---|
| **`offline`** (default) | no `actor/{shard}` row in range has status `Active` | write directly; refuse otherwise, naming the live shards |
| **`quiesce`** | operator confirms | request quiesce-flush for the shards in range via `QuiesceSignal`, wait for the fence rows to clear, then write as `offline` |
| **`online`** | — | route every row through the gateway as ordinary bulk diffs: slow, journal-durable, safe against live actors. The only mode that can seed a live world, and it is not the default because it is 10–100× slower and produces journal volume the demo does not want |

**Visibility.** A seeded world becomes visible through the cold-cell path — `ColdFallbackRouter` + `ColdCellReader::read_cold` — which is gated on **P-3** (subtree spans) and **P-5** (`has_actor` must mean "a live actor holds this", not "HRW would place it here").

**The seeder does not write `ckpt/{shard}` rows.** That row is the recovery watermark `(node_id, lsn, epoch, time)` (D11 §6) and a seeder has no journal position to record. Writing a synthetic watermark would be actively harmful: `CellRuntime::restore` calls `restore_entities` unconditionally when `load()` returns `Some`, so a fabricated checkpoint would install whatever bag it carried over the actor's real state. Absence of the row is correct and already handled — recovery replays from `Lsn(0,0)`.

This is also why **P-8 must be fixed before `soak`**: with the entity bag inside the `ckpt/` value, a shard over ~390 entities cannot checkpoint at all, seeded or not.

---

## 12. Reporting, verification and the rig seam

### 12.1 The seed report

Machine-readable JSON plus a terminal summary. Per layer: achieved-vs-target with deltas. Globally: the entity-per-cell distribution (histogram, p50/p90/p99/max, Gini), level distribution, hot-shard share, rows written, bytes, throughput achieved, wall-clock, and the manifest digest.

```
orrery-seed apply --profile demo

  seed         p2demo-2026-08-13          (config digest 9f3a…c17e)
  plan tier    analytic                    payload class: opaque
  grid 0       cell_edge 128 m             extent 64×8×64 cells (8.2×1.0×8.2 km)

  layer  terrain    heightfield  fbm  H=0.7      → field, 32 768 cells
  layer  hotspots   cluster      parents=3       → field, ×1.00
  emit   props      entity       10 000          → 10 000 rows

  cells occupied    8 618 / 32 768  (26.3%)      target 30.0%  Δ −3.7pp  ✓ within 5%
  entities/cell     p50 0  p90 1  p99 3  max 9   Gini 0.771
  shards            64      hottest 312 (3.1%)   target 8.0%   Δ −4.9pp  ✗ over tolerance
  rows              10 000 world · 0 chunk       2.60 MiB logical
  ckpt projection   43.2 KiB / shard             ✓ under 100 KB
  wrote             10 000 rows in 5 txns        0.17 s  (58 800 rows/s)
  manifest          d41a…8b02                    content/version ← "demo-2026-08-13"

  ⚠ hot_shard_share missed: cluster with size_dist="equal" cannot concentrate.
    Set [layer.params] size_dist = "zipf" (see docs/12 §6.1).
```

The warning is the report earning its keep: a missed target with the named cause and the named fix.

### 12.2 `verify`

Reads the world back and asserts it matches the manifest.

| Check | Coverage | Cost |
|---|---|---|
| row count per shard | exhaustive (range counts) | one scan |
| `value_digest` | sampled, default 1%, `--full` for all | proportional |
| Morton locality — do 27-cell neighbourhood scans read contiguously? | sampled cells | one scan per sample |
| orphans — `world/` rows with no manifest entry | exhaustive | one scan |
| id-space integrity — `pid/next` exceeds every minted id, no duplicates | exhaustive (manifest-local) | in-memory |
| `chunk/` shard sizes ≤ 100 KB | exhaustive | metadata only |

### 12.3 The rig and demo seam

The scenario file carries an optional `[[workload]]` section, so the world and the load that runs on it are one artifact and cannot drift:

```toml
[[workload]]
name       = "p2demo"
entities   = { from_emit = "props", fraction = 0.4 }
motion     = { kind = "boids", flocks = 20, agents_per_flock = 20, speed_ms = 12.0 }
diff_hz    = 4
intent_mix = { trade = 0.02, craft = 0.01 }
duration   = "30m"
```

The rig consumes the manifest for its entity/cell inventory and the `motion` block as a **trajectory program**, not a trace — which is why `kepler` and `profile` matter: a closed-form `(entity, tick) → position` is a few hundred bytes of parameters where a recorded trace of 10 000 entities at 60 Hz for 30 minutes is gigabytes.

**The `kill -9` assertion is a manifest comparison.** Take a manifest snapshot before the kill (`verify --emit-manifest pre.json`), restart, snapshot again, and assert: every `ContentKey` present before is present after, with an identical `value_digest` for every entity whose last acked diff preceded the kill. Bulk loss is bounded by the journal/replication window by design, so the assertion is over *acked* state, and the rig supplies the ack watermark. That is the P2 demo criterion, mechanized.

### 12.4 Telemetry

OTel from P0 onward (D12). Spans `seed.plan`, `seed.generate{layer}`, `seed.encode`, `seed.write.batch`, `seed.verify`; metrics `seed.rows_written`, `seed.bytes_written`, `seed.txn_commit_ms` (histogram), `seed.throughput_rows_per_s`, `seed.target_delta{target}`. Attributes carry `scenario`, `content_build`, `seed`, `profile`.

---

## 13. Scale

### 13.1 The cost model

Stated assumptions, because every number below moves with them:

| Term | Value | Note |
|---|---|---|
| component bag | **256 B** | the sensitivity is linear; §13.2 restates the binding constraints at 128 B and 512 B |
| `world/` key | 21 B | `b'w' ‖ grid(4) ‖ cell(8) ‖ entity(8)` (P-7 landed; the pre-P-7 key was 17 B) |
| FDB per-row overhead | ~40 B | key + value framing, conservative |
| FDB replication | ×3, plus ~1.3 storage amplification | |
| write throughput | 60 000 rows/s | measured on the 3-node dev posture; reported, not assumed |

The occupancy, Gini and `ckpt/{shard}` ceiling columns of §13.2 are derived in **A.3.3**, **A.3.4** and **A.15** respectively; every figure there is reproducible from the stated formula.

### 13.2 The world-size ladder

Five named profiles. `orrery-seed apply --profile demo` is the entire P2 demo runbook line.

| Profile | Entities | Extent (cells) | Extent (m) | Interest cells | Shards | ent/cell λ | Occupied cells | Occ % |
|---|---|---|---|---|---|---|---|---|
| **smoke** | 1 000 | 16×2×16 | 2 048 × 256 × 2 048 | 512 | 4 | 1.953 | 439 | 85.8% |
| **demo** | **10 000** | **64×8×64** | **8 192 × 1 024 × 8 192** | **32 768** | **64** | **0.305** | **8 618** | **26.3%** |
| **soak** | 1 000 000 | 128×16×128 | 16.4 × 2.0 × 16.4 km | 262 144 | 512 | 3.815 | 256 365 | 97.8% |
| **stress** | 10 000 000 | 256×32×256 | 32.8 × 4.1 × 32.8 km | 2 097 152 | 4 096 | 4.768 | 2 079 338 | 99.2% |
| **absurd** | 100 000 000 | 512×64×512 | 65.5 × 8.2 × 65.5 km | 16 777 216 | 32 768 | 5.960 | 16 733 952 | 99.7% |

| Profile | `world/` rows | Logical | Disk (×3 ×1.3) | `ckpt/{shard}` value | vs 100 KB | Txns @768 KiB | Seed @60 K rows/s |
|---|---|---|---|---|---|---|---|
| **smoke** | 1 000 | 267 KiB | 1.19 MiB | 69.1 KiB | ✓ | 1 | 0.02 s |
| **demo** | 10 000 | 2.60 MiB | 11.94 MiB | 43.2 KiB | ✓ | 5 | 0.17 s |
| **soak** | 1 000 000 | 260 MiB | 1.17 GiB | **540 KiB** | ✗ 5.4× | 409 | 17 s |
| **stress** | 10 000 000 | 2.54 GiB | 11.66 GiB | **675 KiB** | ✗ 6.7× | 4 084 | 2.8 min |
| **absurd** | 100 000 000 | 25.4 GiB | 116.6 GiB | **843 KiB** | ✗ 8.4× | 40 837 | 27.8 min |

**The occupied-cell column is a Poisson expectation, not what the seeder
realizes.** It is derived in A.3.3 as `C·(1 − e^(−λ))`, which is the occupancy
you get if entities are *scattered* independently. The seeder does not scatter
them: §7.1 chooses **systematic** allocation precisely because it holds per-cell
deviation to ≤ 1 entity (measured max |count − N·w| of 0.995 against 6.2 for
multinomial). At λ < 1 that deals one entity to each of `N` distinct cells, so
the realized occupancy is `N/C` — for `demo`, 10 000 / 32 768 = **30.5%**, not
the 26.3% the Poisson column predicts, and every occupied cell holds exactly 1.
Both numbers are right about different things; a scenario's `occupied_fraction`
target is checked against the realized figure. The Poisson column remains the
correct guide for *storage* questions, where what matters is the expected
distribution of a scattered world, and for the iterative generators whose
fields are not evaluated by the splitter.

**`demo` satisfies the P2 criterion with two orders of magnitude of headroom**: 10 000 entities across 8 618 occupied cells against "10k entities across 100+ cells".

**The ladder is calibrated so `smoke` and `demo` pass on today's code and `soak` is the first rung that forces the P-8 fix.** That is intentional: the P2 demo can ship, and the very next rung produces the bug report with a number attached. The sensitivity is worth stating exactly, because it is what makes P-8 a prerequisite rather than a tuning exercise: the ceiling is `100 000 / (bag + 34)` entities per shard — 617 at a 128 B bag, 344 at 256 B, 183 at 512 B — while `soak` needs 1 953 and `demo` needs 156. So `soak` fails at every plausible bag size, and `demo` passes at every plausible bag size. But `demo-hotspot`'s hottest shard holds 800 entities, which exceeds the limit for **any bag larger than 91 B**. The binding constraint is the `ckpt/` row, not the bag.

### 13.3 The hotspot arm

`demo` is the uniform control arm — it structurally cannot produce a hotspot, so it isolates rig noise from world-induced variance. `demo-hotspot` holds 10 000 entities and the same extent, with `hot_shard_share = 0.08`, `hotspots = 3`, `hot_shard_placement = "hrw_adversarial"`:

| Shard rank | Entities | `ckpt/{shard}` value | Over 100 KB? |
|---|---|---|---|
| 1 (hot) | 800 | 221 KiB | ✗ |
| 2 | 400 | 111 KiB | ✗ |
| 3 | 200 | 55 KiB | ✓ |
| 4–64 | 141 each | 39 KiB | ✓ |

Shard Gini 0.095, max/mean 5.1×, 2 of 64 shards over the value limit — the P-8 failure reproduced deterministically at demo scale.

### 13.4 Scaling modes

`scale` multiplies absolute quantities; `scale_mode` picks which ratio is held fixed:

| Mode | Holds | What it scales |
|---|---|---|
| `isodensity` (default) | entities/cell | *the cluster* — the capacity model R-7 asks the latency rig to double as |
| `isovolume` | extent | *the hotspot* — walks a fixed world into the `ckpt` and area-page ceilings |
| `isocount` | N | *sparsity* — cold-cell scan cost per row, empty-subtree scans |

Scale-invariance is a **tested property** (§15): at `isodensity`, Gini, occupied fraction, hot-shard share and max/mean must hold within tolerance across `scale ∈ {1, 10, 100}`. A generator whose shape drifts with N has a bug.

### 13.5 Generation is not the bottleneck

At `stress` (10 M entities), generation is ~30 s single-threaded and trivially parallel across subtrees; the FDB write path is 2.8 minutes. The budget target is **generation ≤ 20% of write time at every rung**, which the per-cell-independent design meets by construction. The exceptions are the iterative generators, where `ca` at `soak` extent costs more than the write — and that is reported in the plan, not discovered at apply.

---

## 14. Acceptance

What the seeder must prove for P2, and which prerequisite each gate depends on.

| Gate | Assertion | Blocked on |
|---|---|---|
| **A1** | `apply --profile smoke` writes 1 000 rows; `verify --full` passes | P-2, P-3 |
| **A2** | `apply --profile demo` writes 10 000 rows across ≥ 100 occupied cells in < 5 s; report matches targets within tolerance | P-2, P-3 |
| **A3** | A cold client area load of a 27-cell neighbourhood in a seeded, never-loaded world returns the seeded entities, first page < 50 ms | P-3, P-4, P-5 |
| **A4** | `apply` twice with the same scenario is a no-op: identical manifest digest, zero rows changed | — |
| **A5** | Changing one layer param changes only the cells the plan predicted; `plan --diff` blast radius matches the applied diff | — |
| **A6** | A row modified out-of-band is **kept**, not clobbered, by a subsequent patch, and is reported as a merge outcome | P-6 |
| **A7** | `kill -9` the cluster on a seeded world under rig load, restart, and every pre-kill acked `ContentKey` is present with an identical digest | **P-1**, P-8 |
| **A8** | `demo-hotspot` reproduces the hot-shard skew deterministically — hottest shard ≥ 800 entities, shard Gini ≈ 0.095, max/mean ≈ 5.1× — and the `ckpt/{grid}/{shard}` row stays under 128 B **at that skew**, proving P-8 stays fixed | P-8 (landed) |

**A8 was restated (2026-08-13).** As originally written it asserted that
`demo-hotspot` *reproduces* a `ckpt/{shard}` value overflow, which was true of
the pre-P-8 tree: the whole entity bag lived inside that one value, so 800
entities on the hottest shard blew past FDB's 100 KB limit. The P-8 fix made
`ckpt/` watermark-only, so the overflow is no longer reachable and the gate as
written can never pass. Its value was never the overflow itself but the
deterministic skew that produced it — so A8 now asserts the skew is still
reproducible and that the watermark row stays small under it. That turns a gate
which self-destructed on being satisfied into a permanent regression guard,
which is what §14's gates are for.

A1–A5 are the seeder's own; A6–A8 are the seeder proving the *cluster* works, which is what makes it a demo requirement rather than a tool.

---

## 15. Test strategy

| Test | Kind | Cost |
|---|---|---|
| same seed → identical manifest digest, across thread counts | property | ms |
| achieved count == `count`, exactly, for every generator | property | ms |
| per-cell deviation from target profile ≤ 1 entity | property | ms |
| `union` is commutative and associative; `max`/`min` idempotent | property | ms |
| scale-invariance under `isodensity` across `scale ∈ {1,10,100}` (§13.4) | property | seconds |
| declared density signatures (§6.3 table) reproduce within tolerance | golden | seconds |
| golden manifest for `smoke` and `demo` | regression | ms |
| write/read/verify round-trip against `.fdb-dev` | integration, `fdb` feature | seconds |
| `plan` byte estimate within 5% of applied bytes | integration | seconds |

Golden-manifest tests are valid within a pinned toolchain (§8) and the manifest records it, so a toolchain bump regenerates them as a reviewed diff rather than a mystery failure.

---

## 16. Spec deltas this document requires

| # | Delta | Where |
|---|---|---|
| S1 | `INTEREST_LEVEL`, `SHARD_LEVEL` and the metres↔cells conversion move from `orrery_spatial` (Bevy) to `orrery_protocol` (engine-free) | [10-crates.md](10-crates.md) §1 |
| S2 | `orrery_persistd::keyspace` becomes public: one definition of `world_key`, `ckpt_key` and the subtree spans | [10-crates.md](10-crates.md) §11 |
| S3 | New crate `orrery_seed` in the workspace and the crate table | [10-crates.md](10-crates.md) |
| S4 | `toml` + `blake3` added to the D14 pinned-dependency list (neither is currently a workspace dependency) | [DECISIONS.md](DECISIONS.md) D14 |
| S5 | `seedmap/{content_key}` and `seedprog/{emit}/{cell}` added to the keyspace table; `content/version` value extended to `(build, manifest digest, scenario seed, config digest, toolchain, seeded_at)` | [08-persistence.md](08-persistence.md) §6 |
| S6 | The `world/` key gains a `GridId` discriminator, or per-grid Directory subspaces are specified (P-7) | [08-persistence.md](08-persistence.md) §6 |
| S7 | §17 gains a pointer to this document as its expansion | [08-persistence.md](08-persistence.md) §17 |

---

## 17. Open questions

| Question | Proposed path | Decide by |
|---|---|---|
| **`GridId` in the storage key** (P-7): key discriminator vs. per-grid Directory subspace. A subspace is cleaner and costs a directory lookup per grid; a key field is uniform and costs 4 bytes on every row forever. | Prototype the subspace form against the `kepler` showcase; measure the lookup cost against the < 50 ms first-page-in budget | P2 exit |
| **Patch granularity below the entity** (§9.4). Whole-bag merge blocks a content update whenever a player touched anything on the entity. Per-component merge needs the `Ruleset` to decode. | Ship whole-entity in v1; measure how often the conflict case actually fires in the reference game before adding a decode path that weakens the seam | P5 entry |
| **WFC tile-set authoring format** (§6.4). The generator is specified; the content format it consumes is not. | Author the reference game's tile set first and let the format fall out of it, rather than designing a format with no consumer | P4 |
| **FDB 7.4 `bulkload` SST ingest** (§6.7). Range-parameterized bulk ingest is the fastest possible path and `octree_ifs` output feeds it directly. | Revisit at the 7.3 → 7.4 upgrade decision; the write path is behind a trait so the swap is local | 7.4 upgrade |
| **Live seeding** (§11.4 `online` mode). Currently specified, unbuilt, and 10–100× slower. Is a live content patch ever operationally required, or is a rolling quiesce always acceptable? | Answer from live-ops requirements, not from the tool | P6 |

---

## 18. Worked scenarios

Five complete files, trivial to elaborate. These ship in `crates/orrery_seed/scenarios/`.

### 18.1 `smoke.toml` — the smallest thing that works

```toml
schema = 1

[scenario]
name          = "smoke"
content_build = "smoke-2026-08-13"
description   = "1k entities in a 4-shard box. CI's canary; runs in 20 ms."

[seed]
scenario = "smoke-v1"

[payload]
class = "opaque"          # no Ruleset linked; bags are declared-size filler

[archetype.prop]
declared_size = "256B"

[[layer]]
name   = "flat"
kind   = "uniform"
bounds = { kind = "box", center = { level = 21, xyz = [0, 0, 0] }, extent_cells = [8, 1, 8] }

[[emit]]
name       = "props"
from       = "main"
count      = 1_000
archetypes = { prop = 1.0 }
```

Everything else is defaulted: `op = "union"`, `into = "main"`, `level = 21`, `placement = "hash"`, `intensity = 1.0`.

### 18.2 `p2demo.toml` — the roadmap criterion, exactly

```toml
schema = 1

[scenario]
name          = "p2demo"
content_build = "demo-2026-08-13"
description   = """
The P2 demo criterion: 10k entities across 100+ cells (this delivers ~8.6k
occupied cells across 64 shards). Uniform control arm — structurally cannot
produce a hotspot, so any variance the rig measures is the rig's own.
"""

[seed]
scenario = "p2demo-2026-08-13"
context  = "orrery.seeder.v1"

[payload]
class          = "opaque"
schema_version = 1

[[grid]]
id          = 0
cell_edge_m = 128.0

[archetype.crate]
declared_size = "256B"
[archetype.crate.fields]
model = "crate_wood_01"

[archetype.barrel]
declared_size = "224B"

# One flat field over the demo extent: 64x8x64 half-extent = 128x16x128 cells.
[[layer]]
name   = "world"
kind   = "uniform"
level  = 21
bounds = { kind = "box", center = { level = 21, xyz = [0, 0, 0] }, extent_cells = [32, 4, 32] }

[[emit]]
name       = "props"
from       = "main"
kind       = "entity"
count      = 10_000
level      = 21
placement  = "hash"
archetypes = { crate = 0.7, barrel = 0.3 }

[target]
count             = 10_000
occupied_fraction = 0.30
tolerance         = 0.05

[limits]
max_entities   = 20_000
max_bytes      = "128MiB"
max_wall_clock = "60s"

[load]
mode        = "offline"      # refuse if any actor/{shard} in range is live
txn_bytes   = "768KiB"
concurrency = 8
dispatch    = "shuffled"     # sorted generation, shuffled dispatch (#11510)

[[workload]]
name     = "p2demo"
entities = { from_emit = "props", fraction = 0.4 }
motion   = { kind = "boids", flocks = 20, agents_per_flock = 20, speed_ms = 12.0 }
diff_hz  = 4
duration = "30m"
```

### 18.3 `p2demo-hotspot.toml` — deliberate skew

Same 10 000 entities, same extent. The `extends` keeps the two arms provably comparable: only the density changes.

```toml
schema = 1

[scenario]
name          = "p2demo-hotspot"
content_build = "demo-hot-2026-08-13"
extends       = "p2demo.toml"
description   = "The demo world with 3 hotspots at 8% of mass in the hottest shard."

# Zipf cell-weight ladder folded over the flat field, then clusters on top.
# rank_layout = "morton" puts hot cells in ONE contiguous FDB range — the
# #11510 write-hotspot pattern named in R-7. Flip to "scattered" to hold the
# marginal skew fixed while spreading the hot cells across shards.
[[layer]]
name   = "skew"
kind   = "zipf"
op     = "mask"
into   = "main"
bounds = { kind = "box", center = { level = 21, xyz = [0, 0, 0] }, extent_cells = [64, 8, 64] }
[layer.params]
gini        = 0.42
rank_layout = "morton"

[[layer]]
name = "hotspots"
kind = "cluster"
op   = "union"
into = "main"
[layer.params]
parents     = 3
sigma_m     = 96.0          # <= 0.5 * cell_edge: a cluster packs into 1-8 cells
kernel      = "gaussian"
size_dist   = "zipf"        # equal-size clusters CANNOT make a hotspot (§6.1)
size_zipf_s = 1.0

[target]
count               = 10_000
hot_shard_share     = 0.08
hotspots            = 3
hot_shard_placement = "hrw_adversarial"   # collide hot shards on one node
tolerance           = 0.05
solve               = { knob = "sigma_m" }
```

### 18.4 `caves.toml` — terrain and correlated entities from one field

The CA field drives *both* the terrain sections and where the entities go, so entity density and terrain solidity are correlated rather than independently random — which is what a real world looks like and what an independent-Poisson generator cannot produce.

```toml
schema = 1

[scenario]
name          = "caves"
content_build = "caves-2026-08-13"
description   = "Cave-smoothing CA: exact fill, organic connected volume, terrain + entities from one field."

[seed]
scenario = "caves-v3"

[payload]
class = "ruleset"           # needs a linked SeedEncoder for chunk sections

[[grid]]
id          = 0
cell_edge_m = 128.0

[archetype.ore]
declared_size = "192B"

# --- the field -------------------------------------------------------------
# quantile mode thresholds the neighbour-count field at its k-th largest value
# each iteration, so it hits `fill_target` EXACTLY with no search (§6.3).
[[layer]]
name   = "solid"
kind   = "ca"
into   = "solid"
op     = "union"
level  = 21
bounds = { kind = "box", center = { level = 21, xyz = [0, 0, 0] }, extent_cells = [32, 8, 32] }
[layer.params]
mode        = "quantile"
dim         = 3
neighborhood = "moore"
radius      = 1
fill_target = 0.45
iterations  = 6

# Ore sits in the cave walls: solid enough to be rock, open enough to reach.
# The predicate grammar is total, so the estimator stays exact (§5.3).
[[layer]]
name  = "ore_zone"
kind  = "uniform"
op    = "conditional"
into  = "main"
where = "solid > 0.15 and solid < 0.85"

# --- realization -----------------------------------------------------------
[[emit]]
name       = "ore"
from       = "main"
kind       = "entity"
count      = 25_000
archetypes = { ore = 1.0 }

[[emit]]
name         = "rock"
from         = "solid"
kind         = "terrain"
sections     = 8              # sections per interest cell
elide        = "empty"        # write only sections this scenario authored (§11)
max_shard    = "100KB"
on_conflict  = "error"

[target]
count     = 25_000
tolerance = 0.02
```

**On elision.** `elide = "empty"` writes a `chunk/` row only for sections the scenario actually authored as non-empty. The seeder does **not** evaluate the client's runtime terrain function to decide whether a section equals the procedural default — it cannot, without linking the client's generator, and pretending otherwise was an early draft's mistake. "Absence of `chunk/` keys means regenerate from seed" ([08-persistence.md](08-persistence.md) §10) stays the client's contract; the seeder's contribution is to record the terrain generator's parameters in the manifest so the client can reproduce the base.

### 18.5 `solar.toml` — the showcase (gated on P-7)

```toml
schema = 1

[scenario]
name          = "solar"
content_build = "solar-2026-08-13"
description   = """
A literal orrery: star, planets, moons, stations, and a belt — each body a
nested GridId frame whose velocity lives at the grid root. The only scenario
that exercises nested reference frames (01-spatial-model §13), and the only
one whose trajectories are a closed-form function of (entity, tick).

BLOCKED on P-7: the world/ key carries no GridId today. Runs with
--single-grid (everything flattened into grid 0) until that lands.
"""

[seed]
scenario = "sol-1"

[payload]
class = "opaque"

# System grid: 1024 km cells put the level-21 half-extent at ~7.18 AU.
[[grid]]
id          = 0
cell_edge_m = 1_024_000.0

# Each body is its own CellId space at game scale.
[[grid]]
id          = 1
parent      = 0
cell_edge_m = 128.0

[[grid]]
id          = 2
parent      = 0
cell_edge_m = 128.0

[archetype.station]
declared_size = "512B"
[archetype.asteroid]
declared_size = "128B"

[[layer]]
name   = "system"
kind   = "kepler"
bounds = "all"

[layer.params]
time_scale = 1.0

  [[layer.params.bodies]]
  name = "primary"
  grid = 1
  semi_major_axis_m = 149_600_000_000.0
  eccentricity      = 0.017
  inclination_deg   = 0.0
  period_s          = 31_557_600.0

  [[layer.params.bodies]]
  name   = "moon"
  grid   = 2
  parent = "primary"
  semi_major_axis_m = 384_400_000.0
  eccentricity      = 0.055
  period_s          = 2_360_591.0

  [layer.params.belt]
  inner_m   = 329_000_000_000.0
  outer_m   = 478_000_000_000.0
  count     = 50_000
  # A Kuzmin disc profile gives a realistic radial falloff instead of a
  # uniform annulus — and 53x HRW skew instead of 4.4x (§6.5).
  profile   = "kuzmin"
  scale_radius_m = 400_000_000_000.0

[[emit]]
name       = "belt"
from       = "main"
count      = 50_000
archetypes = { asteroid = 1.0 }

[[emit]]
name       = "stations"
from       = "main"
kind       = "entity"
count      = 24
archetypes = { station = 1.0 }
```

### 18.6 `million.toml` — composition, profiles, and a CI override

```toml
schema = 1

[scenario]
name          = "million"
content_build = "soak-2026-08-13"
description   = "1M-entity soak world: fractal skeleton, noise skew, carved voids."
scale         = 1.0
scale_mode    = "isodensity"

[seed]
scenario = "soak-v2"

[payload]
class = "opaque"

[archetype.debris]
declared_size = "256B"

# 1. Fractal skeleton. terminal_level stops the recursion early and populates
#    whole subtrees, so each retained cell is ONE contiguous FDB range
#    regardless of mask (§6.2 correction 2).
[[layer]]
name   = "skeleton"
kind   = "octree_ifs"
into   = "main"
bounds = "all"
[layer.params]
octant_mask    = 0b1010_0101    # popcount 4 -> D = 2.0 (Sierpinski tetrahedron)
depth          = 7
terminal_level = 19
fill_subtree   = true

# 2. Correlated skew on top. octaves is capped by base_wavelength (§6.1);
#    skew_gini inverts in closed form because the marginal is lognormal.
[[layer]]
name   = "skew"
kind   = "noise"
op     = "mask"
into   = "main"
[layer.params]
basis              = "opensimplex2"
base_wavelength_m  = 4096.0
octaves            = 4
lacunarity         = 2.0
gain               = 0.5
skew_gini          = 0.35

# 3. Carve voids. subtract raises the empty-cell fraction, which is what
#    exercises cold-cell scans over sparse neighbourhoods.
[[layer]]
name   = "voids"
kind   = "percolation"
op     = "subtract"
into   = "main"
[layer.params]
occupancy    = 0.3116077       # simple-cubic site p_c
connectivity = 6

[[emit]]
name       = "debris"
from       = "main"
count      = 1_000_000
archetypes = { debris = 1.0 }

[target]
count     = 1_000_000
gini      = 0.55
tolerance = 0.05
solve     = { knob = "skew_gini" }

[limits]
max_bytes      = "4GiB"
max_wall_clock = "10m"
protect        = true          # refuses `wipe` without an explicit override

# CI runs the same topology 1000x smaller:  --profile ci
[profile.ci]
scenario = { scale = 0.001 }
target   = { count = 1_000 }
limits   = { max_bytes = "16MiB", max_wall_clock = "30s" }
```

**Merge semantics for `extends` and `[profile.*]`:** tables merge key-wise, scalars override, and **arrays of tables replace wholesale**. An overlay cannot append one `[[layer]]` to an inherited stack — it replaces the stack or leaves it alone. That is a deliberate restriction: partial array merging by index is the single most common source of "which config actually ran?" confusion in every format that allows it.

---

## Appendix A — The mathematics, spelled out

Everything the body of this document invokes by name is defined here: the actual formula, a plain-language reading, pseudocode where an algorithm is meant, worked numbers, and — where it helps — the everyday thing it is secretly the same as. Nothing in the spec should require you to already know a paper.

Numbers marked **[verified]** were computed against the stated formula while writing this document; a reader who reproduces them should get the same digits.

### A.1 Notation and the units that matter

| Symbol | Meaning | Typical value here |
|---|---|---|
| `C` | number of candidate interest cells in a scenario's bounds | 32 768 for `demo` |
| `N` | total entities emitted | 10 000 for `demo` |
| `λ` (lambda) | mean entities per cell, `= N / C` | 0.305 for `demo` |
| `S` | number of shard cells (level 18) | 64 for `demo` |
| `w_c` | a cell's normalized weight, `Σ_c w_c = 1` | — |
| `f(cell)` | a layer's field value at a cell, `≥ 0` | — |
| `edge` | interest-cell edge length | 128 m (D16) |

A shard cell is 8×8×8 interest cells, so `S = C / 512` and one shard spans 1 024 m. A level-`L` cell has edge `128 · 2^(21−L)` metres. These three relations are used constantly and are not restated.

### A.2 Counting: how "exactly N entities" is achieved

#### A.2.1 The Poisson process and the conditioning theorem

A **Poisson point process** with intensity `λ(x)` is the formal version of "scatter things at random, but denser in some places than others." Two properties define it: the number of points in any region `W` is Poisson-distributed with mean `∫_W λ(x) dx`, and counts in disjoint regions are independent.

The theorem the whole seeder rests on (Kingman, *Poisson Processes*, 1993, §2.4):

> Conditional on `N(W) = n`, the `n` points of a Poisson process on `W` are independent and identically distributed with probability density `λ(x) / ∫_W λ`.

**In plain terms:** *how many* and *where* are separable. If you already know you want exactly `n` points, you can forget the Poisson distribution entirely and just draw `n` samples from the normalized intensity. The result is statistically indistinguishable from a Poisson process that happened to produce `n` points.

**Why this is the load-bearing idea:** it means a generator never has to decide how many entities exist. Game of Life produces whatever fill fraction its rule produces — useless as a knob. But *normalize* the Life pattern into a weight function and hand it to an allocator that emits exactly 10 000 entities, and you have a fixture. This is decision **D-B**, and it is why every generator in §6 is described as producing a *field*, never a set of points.

**The everyday analogy:** it is the difference between "throw a handful of rice at a map and see where it lands" and "I have exactly 10 000 grains; distribute them in proportion to this map's shading." The second is reproducible and orderable; the first is not.

#### A.2.2 Systematic vs multinomial allocation

Given normalized weights `w_c` and a total `N`, you have to turn fractional expectations `N·w_c` into integers.

**Multinomial** (the naive way): draw `N` independent uniforms and bucket each by the cumulative distribution. Per-cell error is random with standard deviation `√(N w_c (1−w_c))` — order `√N` in the worst case.

**Systematic** (also called stratified resampling, borrowed from particle filters): use a single jittered offset and take evenly spaced samples of the CDF.

```
phi   = uniform_from_key(K_layer)       # one draw, in [0,1)
cdf   = 0
i     = 0
for cell in cells_in_morton_order:
    cdf += w[cell]
    count[cell] = 0
    while i < N and (i + phi) / N <= cdf:
        count[cell] += 1
        i += 1
```

Because `u_i = (i + φ)/N` is monotone increasing, this walks the cells once, in order, and the deviation from `N·w_c` is bounded by **±1 entity for every cell** — not on average, always.

**[verified]** measured max `|count − N·w_c|` over 32 768 cells at `N = 10 000`: **0.995** systematic vs **6.2** multinomial.

Three consequences fall out of the monotonicity, and they are the reason this is the default:

1. Output arrives **pre-sorted by `CellId`**, which is Morton order, which is FDB key order — no sort pass.
2. Memory is **O(1)**: a million-entity load never holds more than one batch.
3. It **splits by index block**: worker `p` takes indices `[pN/P, (p+1)N/P)`, which is a contiguous CDF interval, which is a contiguous `CellId` range, which is a disjoint FDB key range. No coordination, no merge.

**The everyday analogy:** multinomial is rolling a die `N` times; systematic is dealing a deck round-robin. Both give the same expected counts; only the second guarantees nobody is short by more than one card.

#### A.2.3 Hierarchical binomial splitting

Systematic allocation over a flat list of cells needs the list. At level 21 a scenario's bounds can hold billions of cells, so instead the count is split recursively down the octree — which is the same algorithm applied at each node to eight children.

```
fn split(cell, n, depth):                  # n entities to place in `cell`
    if n == 0: return
    if depth == terminal_level:
        emit(cell, n); return
    masses = [field_mass(child) for child in cell.children()]   # 8 values, Q16.16
    total  = sum(masses)
    if total == 0: emit(cell, n); return
    # integer apportionment, largest remainder, tie-broken by child index
    counts = largest_remainder(n, masses, total)
    for child, k in zip(cell.children(), counts):
        split(child, k, depth + 1)
```

All arithmetic is `u128` over Q16.16 fixed-point masses (→ A.14.4), so the result is bit-identical on every platform.

**What this buys, and it is four things at once:**

1. **Exact counts** — `Σ counts = n` at every node, by construction of largest-remainder.
2. **An O(depth) density oracle.** "How many entities are in this shard?" is answered by a 21-step descent — no world generation. This is what makes `explain --cell` work on a laptop against a 34-million-entity world, and what makes the analytic dry run (§7.3) possible at all.
3. **Embarrassingly parallel generation** — a subtree is self-contained.
4. **Single-cell regeneration** for resume and repair.

The oracle property requires `field_mass(child)` to be computable without evaluating the child's neighbours — true for closed-form fields, false for `ca`, `percolation` and `heightfield`. §7.1 states that limitation rather than hiding it.

#### A.2.4 Largest-remainder apportionment

The same method used to allocate parliamentary seats to parties by vote share (the Hare quota method).

```
fn largest_remainder(n, masses, total):
    exact     = [n * m / total for m in masses]        # rational
    counts    = [floor(e) for e in exact]
    remainder = n - sum(counts)
    order     = indices sorted by (frac(exact) desc, index asc)
    for i in order[..remainder]: counts[i] += 1
    return counts
```

The tie-break on index (not on a hash, not on iteration order) is what makes it deterministic. The same routine apportions archetypes within a cell (§5.5).

### A.3 Measuring a density pattern

Four numbers describe "how lumpy is this world", and the spec uses all four.

#### A.3.1 Gini coefficient

The inequality measure from economics, applied to entities-per-cell instead of income-per-person.

```
G = ( Σ_i Σ_j |x_i − x_j| ) / ( 2 · n² · μ )
```

where `x_i` is cell `i`'s entity count, `n` the number of cells, `μ` the mean. Equivalently: twice the area between the Lorenz curve (cumulative share of entities against cumulative share of cells, sorted ascending) and the 45° line of perfect evenness.

- `G = 0` — every cell has identical population.
- `G = 1` — one cell has everything.
- `G ≈ 0.4` — roughly the income inequality of a developed country, and a good "visibly lumpy but not pathological" world.

#### A.3.2 Coefficient of variation

`CV = σ / μ` — standard deviation as a fraction of the mean. Useful because it has a clean value for the reference cases: a Poisson-distributed count has `CV = 1/√λ`, so **`CV = 1/√λ` is the "no structure at all" baseline** and anything above it is real clustering.

#### A.3.3 Occupancy

The expected number of non-empty cells when `N` entities are spread evenly over `C` cells:

```
E[occupied] = C · (1 − (1 − 1/C)^N)  ≈  C · (1 − e^(−N/C))  =  C · (1 − e^(−λ))
```

This is the birthday-problem/coupon-collector calculation. **[verified]** against the §13.2 ladder:

| `C` | `N` | `λ` | exact | occupancy |
|---|---|---|---|---|
| 512 | 1 000 | 1.953 | 440 | 85.8% |
| 32 768 | 10 000 | 0.305 | 8 618 | 26.3% |
| 262 144 | 1 000 000 | 3.815 | 256 365 | 97.8% |
| 2 097 152 | 10 000 000 | 4.768 | 2 079 338 | 99.2% |
| 16 777 216 | 100 000 000 | 5.960 | 16 733 952 | 99.7% |

The exact and Poisson-approximate forms agree to the printed digits at every rung, which is why the spec uses the approximation.

#### A.3.4 The Gini floor — why a "uniform" world is not `G = 0`

This trips people up and matters for reading §13.2's table. Even with a *perfectly flat* intensity, the realized integer counts are lumpy, because at `λ = 0.305` most cells must hold 0 and a few must hold 1. That lumpiness has a Gini, and it is a **floor**: no scenario at that `λ` can report a lower one.

For Poisson counts, `G = E|X − X'| / (2μ)` where `X, X'` are independent draws:

```
G(λ) = ( Σ_{i<j} P(i)·P(j)·(j − i) ) / λ ,      P(k) = e^(−λ) λ^k / k!
```

**[verified]** exact values, against the large-`λ` asymptotic `G ≈ 1/√(πλ)`:

| `λ` | exact `G` | `1/√(πλ)` | Verdict |
|---|---|---|---|
| 0.305 (`demo`) | **0.769** | 1.022 | asymptotic is **invalid** — it exceeds 1 |
| 1.953 (`smoke`) | 0.390 | 0.404 | 3.6% high |
| 3.815 (`soak`) | 0.284 | 0.289 | 1.7% high |
| 4.768 (`stress`) | 0.255 | 0.258 | 1.4% high |
| 5.960 (`absurd`) | 0.229 | 0.231 | 0.9% high |

The asymptotic is fine above `λ ≈ 2` and nonsense below it. The tool uses the exact sum; the asymptotic appears here only so a reader who has seen it elsewhere knows where it stops working.

**The practical reading:** `demo`'s reported Gini of 0.771 against a floor of 0.769 means the world is *structurally uniform* — the measured inequality is entirely the integer-rounding floor, none of it is clustering. That is the point of the uniform control arm.

#### A.3.5 max/mean skew

The hottest shard's entity count divided by the mean shard's. Cruder than Gini but it is the number that predicts an operational failure, because a checkpoint value limit or an actor's memory is a per-shard ceiling, not a distributional property.

### A.4 Noise fields

#### A.4.1 Gradient noise, in one paragraph

Perlin/simplex/OpenSimplex noise assigns a pseudorandom *gradient vector* to each point of an integer lattice, and at a query point interpolates the dot products of those gradients with the offsets to the query. The result is smooth, band-limited (one characteristic feature size), zero-mean, and repeatable from a seed. Value noise does the same with scalars instead of gradients and is cheaper and blockier. Neither is random per point — nearby points get similar values, which is exactly what makes it useful as terrain or density.

#### A.4.2 Fractal Brownian motion and its three knobs

One noise call gives one feature size. Real density has structure at many scales, so you sum several:

```
f(x) = Σ_{o=0}^{O−1}  gain^o · noise( x · lacunarity^o / λ₀ )
```

| Knob | Meaning | Default | Effect |
|---|---|---|---|
| `octaves` `O` | how many summed layers | 4 | more layers = finer detail on top of the same large shapes |
| `base_wavelength_m` `λ₀` | feature size of the first layer | 2048 m | the size of the big blobs |
| `lacunarity` | frequency multiplier per octave | 2.0 | each layer's features are half the previous layer's |
| `gain` (persistence) | amplitude multiplier per octave | 0.5 | each layer contributes half the previous layer's amplitude |

**The everyday analogy:** it is exactly how a musical tone is built from a fundamental plus quieter, higher harmonics. `lacunarity = 2` is octaves in the musical sense; `gain = 0.5` is each harmonic at half the volume.

**The sub-cell cap, derived.** Octave `o` has feature size `λ₀ / lacunarity^o`. The seeder aggregates the field to one value per cell, so any octave whose features are smaller than about two cells averages to a constant and contributes nothing but cost — and because it contributes to the *mean* while not contributing to the *variance between cells*, it actively **reduces** realized skew. Requiring `λ₀ / 2^o ≥ 2·edge` gives

```
O ≤ 1 + floor( log₂( base_wavelength_m / (2 · cell_edge_m) ) )
```

At `λ₀ = 2048 m` and `edge = 128 m`: `2048/256 = 8`, `log₂ 8 = 3`, so `O ≤ 4`. An operator who sets `octaves = 8` to "make it rougher" gets a *flatter* world — which is why V5 rejects it rather than warning (§10).

#### A.4.3 Why `exp(σ·f)` — the log-Gaussian Cox process

The field must be non-negative to be an intensity, and `f` is zero-mean, so it is exponentiated: `λ(x) = exp(σ · f(x))`.

That step has a consequence worth naming. A sum of many decorrelated octaves is approximately Gaussian by the central limit theorem, so `exp(σ·f)` is approximately **lognormal** — which means the graphics idiom (fBm exponentiated) and the statistics idiom (a log-Gaussian Cox process, Møller–Syversveen–Waagepetersen 1998) are the same object. And a lognormal has closed-form inequality measures:

```
Gini = erf(σ / 2)                  CV = √( e^(σ²) − 1 )
```

where `erf(z) = (2/√π) ∫₀^z e^(−t²) dt` is the error function — the integral of a bell curve, available in every standard library.

So `skew_gini` inverts exactly: `σ = 2 · erfinv(G)`. The operator dials an inequality number they can interpret instead of a magic amplitude. **[verified]**:

| `σ` | Gini | CV | Reads as |
|---|---|---|---|
| 0.25 | 0.140 | 0.254 | barely textured |
| 0.50 | 0.276 | 0.533 | gently varied |
| 1.00 | 0.521 | 1.311 | clearly lumpy |
| 1.50 | 0.711 | 2.913 | strong hotspots |
| 2.00 | 0.843 | 7.321 | a few cells hold most of the world |

**Careful:** this is the Gini of the *intensity field*, not of the realized integer counts. The realized Gini is the larger of this and the A.3.4 floor. At `demo`'s `λ = 0.305` the floor is 0.769, so any `skew_gini` below ~0.77 is invisible in the output — the tool reports that rather than silently missing the target.

#### A.4.4 The other bases, briefly

- **Ridged multifractal**: `1 − |noise|`, then squared. Turns the smooth blobs into sharp ridge lines. Musgrave's construction; the standard way to get mountain crests rather than rolling hills.
- **Billow**: `|noise|`. The opposite — puffy, cloud-like lobes.
- **Worley / cellular noise**: scatter feature points, then at each query return `F1` (distance to nearest), `F2` (second nearest), or `F2 − F1`. `F1` gives Voronoi-cell-shaped blobs; `F2 − F1` is near zero on the boundaries *between* feature points, so it draws a network of walls — the standard "cracked mud" or "cell wall" look.
- **Domain warping**: evaluate `f(x + A · g(x))` — displace the query point by a second noise field before sampling the first. Cheap, and it turns the recognisable "Perlin blobbiness" into something organic. `A` is `warp_amplitude_m`.

#### A.4.5 Heightfields, the Hurst exponent, and diamond-square

A **fractal heightfield** is a 2D surface whose roughness is scale-invariant. Its roughness is one parameter, the Hurst exponent `H ∈ (0,1)`, related to fBm by `gain = 2^(−H)`:

- `H → 1`: smooth, rolling.
- `H → 0`: jagged at every scale.

The surface's box-counting dimension is `D = 3 − H`. A perfectly smooth surface is `D = 2` (an ordinary 2D sheet in 3D); a maximally crumpled one approaches `D = 3` (it starts to fill volume). This matters operationally: `D` is also a dial on how many distinct cells the terrain touches, hence on Morton run-compression, hence on how many contiguous FDB ranges a terrain write becomes.

**Diamond-square** builds one directly, and is the cheapest thing that works:

```
seed the four corners of a (2^n + 1) square
for step = size, size/2, ... , 1:
    DIAMOND: each square's centre = mean(4 corners) + random(±scale)
    SQUARE:  each diamond's centre = mean(4 edge-neighbours) + random(±scale)
    scale *= 2^(−H)                       # this is the only place H enters
```

**The everyday analogy:** repeatedly find the midpoint of a line and nudge it up or down, by a smaller amount each time. That is midpoint displacement; diamond-square is its 2D version.

### A.5 Skew ladders

#### A.5.1 Zipf

The empirical law that in ranked data, the `k`-th most popular item gets a share proportional to `k^(−s)`. Word frequencies in English, city populations, website hits, and database key access all follow it — which is why it is the standard skew knob in load generators.

```
w_k = k^(−s) / H(C, s)          H(C, s) = Σ_{k=1}^{C} k^(−s)
```

`H(C,s)` is the generalized harmonic number — just the normalizing sum. Knobs:

- `s = 0` — uniform, no skew.
- `s = 1` — classic Zipf. The top cell gets `1/H(C,1) ≈ 1/ln C` of everything; at `C = 32 768`, that is about 9.6%.
- `s > 1` — increasingly extreme; as `s → ∞` one cell takes everything.

The seeder also accepts `gini` or the pair `(hot_fraction, hot_share)` and solves for `s` numerically, because "8% of entities in the hottest shard" is a requirement an operator actually has, and `s = 1.07` is not.

#### A.5.2 `rank_layout` — the knob that separates two things everyone conflates

Zipf gives each cell a *rank*. Which physical cell gets rank 1 is a separate decision, and it is the only knob in the bank that changes spatial arrangement while holding the marginal skew *exactly* fixed:

- `rank_layout = "morton"` — rank 1 goes to the first cell in Morton order, rank 2 to the second, and so on. Hot cells are therefore **contiguous in the FDB keyspace**: one hot key range. This is precisely [FDB #11510](https://github.com/apple/foundationdb/issues/11510)'s write-hotspot pattern, cited in R-7.
- `rank_layout = "scattered"` — the rank is hashed to a cell (YCSB's `ScrambledZipfianGenerator` does exactly this). Same distribution of counts, hot cells spread across shards and storage teams.

Running both and diffing isolates "the load is skewed" from "the *keyspace* is skewed" — two failure modes with the same summary statistics and completely different fixes.

### A.6 Cluster processes

#### A.6.1 The Thomas process

A two-stage recipe, and the cleanest way to get tunable clustering:

1. Scatter `κ` **parent** points uniformly (parents are never emitted; they are scaffolding).
2. Around each parent, scatter offspring with an isotropic Gaussian displacement of standard deviation `σ`.

That is it. It is a Neyman–Scott process with a Gaussian kernel, and it has closed-form second-order structure. The **pair correlation function** — "how much more likely are you to find a second point at distance `r` from a given point, compared to uniform" — is, in 3D:

```
g(r) = 1 + exp( −r² / (4σ²) ) / ( κ · (4πσ²)^(3/2) )
```

`g(r) = 1` everywhere means no structure. The bump near `r = 0` is the clustering, its width is set by `σ`, and its height by `1/κ` — few parents, tight clusters, strong clustering.

**The only thing that matters is `σ / edge`:**

| `σ / edge` | Result |
|---|---|
| `≤ 0.5` | a cluster packs into 1–8 interest cells — a real hotspot |
| `≈ 1` | a cluster spans a cell neighbourhood — visible but mild |
| `≥ 2` | statistically indistinguishable from uniform |

**The finding that changes how you configure it:** with equal-size clusters, `N = 10 000` gave a hottest cell of only **38 entities** — nowhere near a split threshold. The cluster *count* is what concentrates mass, and equal-size clusters spread it evenly across parents by construction. Setting `size_dist = "zipf"` (cluster sizes themselves power-law distributed) at the identical parent count is what actually produces a hotspot. If you want a crowd, you need one big cluster, not many medium ones — and that is a property of the *size distribution*, not of `σ`.

#### A.6.2 Matérn thinning

Enforces a minimum separation by deleting points that are too close. Type I deletes both members of any too-close pair; Type II gives each point a random "age" and deletes the younger of any too-close pair (which preserves more points). Used when you want clustering *and* a floor on spacing — scattered trees that never overlap.

### A.7 Fractal subdivision

#### A.7.1 Octree IFS: keep `k` of 8

An **iterated function system** builds a fractal by repeatedly applying a fixed set of contractions. On an octree the contractions are just "descend into these octants", so the rule is one 8-bit mask:

```
level 0:  1 cell
level 1:  k cells        (the octants named by the mask)
level d:  k^d cells
```

Entity count is `k^depth` — **exact, not sampled, not estimated**. The fractal (box-counting) dimension is

```
D = log(k) / log(2)
```

because each level halves the edge length and multiplies the count by `k`. **[verified]**:

| `k` = popcount(mask) | `D` | Named shape |
|---|---|---|
| 1 | 0.000 | a single descending line — a point |
| 2 | 1.000 | a curve |
| 3 | 1.585 | — |
| **4** | **2.000** | **Sierpinski tetrahedron** — a surface, exactly |
| 5 | 2.322 | — |
| 6 | 2.585 | octahedron flake |
| 7 | 2.807 | closest native analogue of a Menger sponge |
| 8 | 3.000 | solid — no fractal at all |

**Why this family is uniquely well-fitted:** the generator's output *is* a set of Morton prefixes, and a Morton prefix *is* a `CellId` subtree, which *is* one contiguous FDB key range (`CellId::subtree_range()`). The generator's output type and the storage key type are the same type. Shard assignment is `ancestor_at(18)` — pure bit masking.

#### A.7.2 Why the Menger sponge does not fit

The Menger sponge divides each cube into **27** subcubes (3×3×3) and keeps 20, giving `D = log 20 / log 3 = 2.7268`. Orrery's octree divides into **8** (2×2×2). There is no octree level whose edge is one third of another's — 3 is not a power of 2 — so a sponge can only be *voxelized* onto the grid, which throws away the exact-prefix property that made it attractive in the first place. Same argument kills 3D Cantor dust (8 of 27).

The native substitutes are mask popcount 7 (`D = 2.807`, nearest to the sponge) and 6 (`D = 2.585`). The Sierpinski **tetrahedron** — 4 of 8 — fits exactly, with `D = 2` on the nose.

#### A.7.3 Galton–Watson branching: why `1/8` is the interesting number

A **branching process** asks: each individual has a random number of children; does the family line survive forever? On the octree, each of the 8 children is retained independently with probability `p`, so the mean number of children is

```
m = 8p
```

The classical result (Watson & Galton 1875, on the extinction of surnames) is that extinction is certain if and only if `m ≤ 1`. So:

- `p < 1/8` — **subcritical**: the tree dies out. Sparse, finite, scattered fragments.
- `p = 1/8 = 0.125` — **critical**: survives with probability zero but with infinite expected size — the scale-free regime, where cluster sizes follow a power law and structure appears at every level simultaneously.
- `p > 1/8` — **supercritical**: survives, with box dimension `D = log₂(8p) = 3 + log₂ p`.

This is the only generator in the bank that populates **many `CellId` levels at once**, which is the input the shard tier was designed for and has never actually received.

**The everyday analogy:** it is the surname-extinction problem, and it is also why a nuclear chain reaction either fizzles or runs away with nothing in between — `m = 1` is the knife edge.

#### A.7.4 Percolation: a calibrated power law for ten lines of code

Occupy each site of a lattice independently with probability `p`, then find connected clusters. There is a sharp threshold `p_c` below which only small clusters exist and above which an infinite spanning cluster appears.

- **Simple-cubic site percolation: `p_c = 0.3116077`** (Wang, Zhou, Zhang, Garoni & Deng 2013).
- At exactly `p_c`, the cluster-size distribution is a pure power law: `n_s ∝ s^(−τ)` with the **Fisher exponent `τ ≈ 2.18906`** in 3D.

A power law with a *known, published* exponent is a rare thing to get for free, and it is why this generator is in the bank: it is the calibrated heavy-tail control. It is also **Morton-incompressible** — the occupied set is spatially random, so it does not collapse into a few contiguous ranges — which makes it the necessary control against the prefix-optimised generators of A.7.1, whose FDB behaviour is unrepresentatively good.

**Hoshen–Kopelman** labels the clusters in a single raster pass using union-find:

```
for each site in raster order:
    if not occupied: continue
    prior = labels of already-visited neighbours (−x, −y, −z)
    if prior is empty:      label[site] = new_label()
    else:                   label[site] = min(find(l) for l in prior)
                            union all prior labels together
# one final pass resolves every label to its root
```

Same algorithm as flood-filling a bitmap, done in one pass instead of a traversal per cluster.

### A.8 Cellular automata

#### A.8.1 B/S rulestrings and neighbourhoods

A cell is alive or dead. Each step, count live neighbours `n`:

- a **dead** cell becomes alive iff `n ∈ B` (the **birth** set),
- a **live** cell stays alive iff `n ∈ S` (the **survival** set).

Written `B{births}/S{survivals}`. Conway's Life is `B3/S23`: born with exactly 3 neighbours, survives with 2 or 3.

Neighbourhoods:

- **Moore** — all cells in the surrounding box: 8 in 2D, **26** in 3D.
- **von Neumann** — face-adjacent only: 4 in 2D, 6 in 3D.

In 2D there are `2^9 × 2^9 = 262 144` Life-like rules; the bank in §6.3 measures eighteen of them.

#### A.8.2 The separable box-sum — never write the 26-gather

The naive neighbour count in 3D reads 26 neighbours per cell. Don't. A box sum is **separable**: sum along x, then along y, then along z.

```
a = grid
for axis in (x, y, z):
    a = a + shift(a, +1, axis) + shift(a, −1, axis)    # 2 adds per cell per axis
neighbours = a − grid                                   # remove the centre
```

Six additions per cell, independent of dimension or of the neighbourhood having 26 members. For radius `r`, use prefix sums along each axis and it stays O(1) per cell.

**The everyday analogy:** it is the same trick as a separable Gaussian blur in image editing — a 2D blur is a horizontal blur followed by a vertical one, and costs `2n` instead of `n²`.

#### A.8.3 Generations (multi-state) and cyclic CA

**Generations** adds `C` states: `0` is dead, `1` is alive, and `2..C−1` are *refractory* — they count down to 0 and cannot be reborn on the way. Written `S/B/C/N`. The refractory tail is what stops 3D rules from either dying instantly or saturating, which is why `states` defaults to 4 for `dim = 3`.

**Cyclic CA**: states `0..C−1` in a cycle; a cell in state `s` advances to `s+1 mod C` iff at least `threshold` neighbours are already in state `s+1`. From a random soup this self-organises into rotating spiral waves — a coherent-wavefront density pattern nothing else in the bank produces, for one extra predicate on the same engine.

#### A.8.4 Quantile mode — exact fill with no search

The rank-order operator, and the reason the CA family can hit a target at all:

```
k = round(fill_target · n_cells)
repeat `iterations` times:
    field = neighbour_count(grid)            # separable, A.8.2
    t     = k-th largest value of field      # exact via a 27-bin histogram at r=1
    grid  = (field >= t)                     # ties broken by cell key, deterministically
```

Because the threshold is chosen *from the data* each iteration rather than fixed, the live count is exactly `k` at every step. No search, no bisection, no rule-dependent tuning — and the structure is still organic and connected, because the field being thresholded is still the neighbour count.

**The everyday analogy:** instead of "pass everyone who scored above 70", it is "pass the top 450 candidates" — the cut moves to hit the quota.

#### A.8.5 Mean-field extinction screening

Before simulating a 3D rule, predict whether it will die or saturate, in twenty lines and no simulation. Assume cells are independent with live-fraction `ρ` (the mean-field approximation). Then a cell's neighbour count is Binomial(`N`, `ρ`) with `N = 26` for 3D Moore, and

```
ρ' = ρ · P(n ∈ S)  +  (1 − ρ) · P(n ∈ B)

     where  P(n ∈ X) = Σ_{n ∈ X} C(N, n) · ρⁿ · (1 − ρ)^(N−n)
```

Iterate `ρ' = F(ρ)` from a few starting points. If the only fixed points are `ρ = 0` and `ρ = 1`, the rule has no interior attractor: it will die out or fill the volume, and `plan` aborts with that diagnosis instead of burning the generation budget. A stable interior fixed point is the rule's predicted steady-state fill.

Mean-field is an approximation — it ignores exactly the spatial correlation that makes CA interesting — so it is used only as a *screen* for the two degenerate outcomes, never as a fill predictor.

### A.9 Orbital mechanics — the `kepler` generator

#### A.9.1 The elements and the position pipeline

Six numbers fix an orbit. `a` semi-major axis (metres, the orbit's "size"), `e` eccentricity (0 = circle, `<1` = ellipse), `i` inclination (tilt of the orbital plane), `Ω` longitude of ascending node (rotation of that tilt about the reference axis), `ω` argument of periapsis (rotation of the ellipse within its plane), and `M₀` mean anomaly at epoch (where the body starts along the orbit).

Position at tick `t` is a closed-form pipeline — no integration, no state:

```
1.  M = M₀ + 2π · t / T                                  # mean anomaly: linear in time
2.  solve  M = E − e·sin E   for E                        # Kepler's equation
3.  r = a · (1 − e·cos E)                                 # radius
4.  ν = 2·atan2( √(1+e)·sin(E/2),  √(1−e)·cos(E/2) )      # true anomaly
5.  p = (r·cos ν, r·sin ν, 0)                             # position in the orbital plane
6.  rotate p by ω about z, then by i about x, then by Ω about z
```

Step 2 is the only hard one: Kepler's equation is transcendental — no closed-form inverse exists, which is why it occupied astronomers for four centuries. Newton's method converges fast:

```
E = M                                     # good starting guess for e < 0.8
repeat until |ΔE| < 1e-12:
    ΔE = (E − e·sin E − M) / (1 − e·cos E)
    E -= ΔE
```

Four to five iterations at `e < 0.8`. For near-parabolic orbits (`e > 0.95`) use a better initial guess or Halley's method; the seeder rejects `e ≥ 0.99` with that message.

**Why this generator matters beyond the pun:** step 1 is linear in `t` and steps 2–6 are stateless, so **position is a pure, seekable function of `(entity, tick)`**. That makes a trajectory script a few hundred bytes of orbital elements. The alternative — recording where 10 000 entities were at 60 Hz for 30 minutes — is on the order of gigabytes. It is the only generator in the bank with this property, and it is what makes the `[[workload]]` seam (§12.3) viable.

#### A.9.2 Reference measurements

| Quantity | Value |
|---|---|
| Astronomical unit (AU) | 1.495 978 707 × 10¹¹ m |
| `GM` of the Sun (`μ`) | 1.327 124 400 18 × 10²⁰ m³/s² |
| Earth: `a`, `e`, period | 1.496 × 10¹¹ m, 0.0167, 365.25 d |
| Moon about Earth: `a`, `e`, period | 3.844 × 10⁸ m, 0.0549, 27.32 d |
| Main asteroid belt | 2.2 – 3.2 AU |

**Kepler's third law** ties period to size, so a scenario may specify either:

```
T = 2π · √( a³ / μ )
```

**[verified]** for Earth: `a³ = 3.348×10³³`, `÷ μ = 2.523×10¹³`, `√ = 5.023×10⁶`, `× 2π = 3.156×10⁷ s = 365.3 days`. ✓

#### A.9.3 Why an operator cares: orbital speed becomes journal write rate

Circular orbital speed is `v = √(μ/a)`. A body crossing 128 m interest cells at speed `v` re-keys at `v / 128` cells per second, and each re-key is a `Rekey` journal record. Earth's orbital speed is 29.8 km/s, which in a 128 m grid would be 233 cell crossings per second — which is why a solar-system scenario uses a coarse grid (`cell_edge_m = 1 024 000`) for the system frame and a 128 m grid inside each body's nested frame. That is the same reason [01-spatial-model.md](01-spatial-model.md) §13 puts a carrier's velocity at its grid root and not in its contents.

### A.10 Equilibrium density profiles

Astrophysics has spent a century producing closed-form density profiles for gravitationally bound systems. They are free, well-studied, and exactly what "clustered but not arbitrary" means. Each has one shape and one scale radius `a`.

| Profile | Density `ρ(r)` | Sample `r` from `u ~ U(0,1)` | Shape |
|---|---|---|---|
| **Plummer** | `(3M / 4πa³) · (1 + r²/a²)^(−5/2)` | `r = a / √(u^(−2/3) − 1)` | flat core, steep falloff — a globular cluster |
| **Hernquist** | `M a / (2π r (r+a)³)` | `r = a√u / (1 − √u)` | central cusp (`ρ → ∞` as `r → 0`) — a galaxy bulge |
| **Kuzmin disc** | surface `Σ(R) = aM / 2π(R²+a²)^(3/2)` | `R = a·√(1/(1−u)² − 1)` | a flat disc — a galaxy or an accretion disc |
| **King** | lowered isothermal, tidally truncated | tabulated inverse CDF | a cluster with a hard outer edge |

The sampling column is **inverse transform sampling**: integrate the density to get the enclosed-mass fraction `M(<r)/M`, set it equal to a uniform draw `u`, and solve for `r`. For Hernquist, `M(<r)/M = r²/(r+a)²`, so `√u = r/(r+a)` and the inverse falls out in one line. That is why these profiles are used rather than something prettier: they invert.

**The finding that justifies them.** Uniform seeding *structurally cannot* find HRW placement imbalance. Measured max/mean shard skew at `N = 10 000`:

| Distribution | Occupied shards | max/mean skew |
|---|---|---|
| Uniform in a 2 560 m box | 64 | **4.4×** |
| Plummer, `a = 768 m` | 687 | **53×** |
| Hernquist | — | **138×** |
| Hernquist cusp centred on a shard | — | **2 070×** |

An order of magnitude separates "uniform" from "any real profile", and two more separate it from an adversarially placed cusp.

### A.11 Agent motion

#### A.11.1 Boids

Reynolds' 1987 model. Three steering rules, each a vector, summed with weights and clamped to a maximum speed:

```
separation:  v_sep = − Σ_{j ∈ near}  (p_j − p_i) / |p_j − p_i|²     # push away, strongest when closest
alignment:   v_ali = mean_{j ∈ view}(v_j) − v_i                     # match the neighbourhood's heading
cohesion:    v_coh = mean_{j ∈ view}(p_j) − p_i                     # steer toward the neighbourhood's centre

v_i ← clamp( v_i + w_s·v_sep + w_a·v_ali + w_c·v_coh,  v_max )
p_i ← p_i + v_i · Δt
```

Three local rules, no leader, no global plan — and flocking emerges. That is the point of the model and also why it is in the bank.

**Why it is the best handoff stress in the bank:** every other generator produces *independent* cell crossings, which a Poisson capacity model predicts correctly. A flock crosses a boundary **together**. Measured:

| Configuration | Behaviour |
|---|---|
| 100 agents, 15 m/s, 60 m diameter | entire membership re-keys in a 4 s burst (25 `Rekey`/s) every 5.7 s — a **70% duty cycle** |
| 200 agents, 25 m/s, 90 m diameter | duty exceeds 100% — the flock permanently straddles a boundary |
| 50 flocks with `phase_lock = 1.0` | all burst simultaneously — a **50× spike** no Poisson model predicts |

#### A.11.2 Ornstein–Uhlenbeck — walkers that stay home

A random walk that is pulled back toward a home point. The stochastic differential equation and its discrete form:

```
dX = θ(μ − X) dt + σ dW

X_{t+Δ} = X_t + θ·(μ − X_t)·Δ + σ·√Δ · Z ,     Z ~ Normal(0,1)
```

`θ` is the restoring strength, `μ` the home position, `σ` the noise scale. The stationary variance is `σ²/(2θ)`, so a population settles into a stable cloud of known width instead of diffusing away — which is what makes it usable for a multi-hour soak where a plain Brownian walk would eventually empty the world.

**The everyday analogy:** a mass on a spring being shaken. It rattles, but it does not wander off.

#### A.11.3 Lévy flights

A random walk whose step lengths are heavy-tailed: `P(step > L) ∝ L^(−α)` with `α ∈ (0,2)`. Mostly short hops with occasional very long jumps. The resulting occupancy is clustered — dense patches connected by long transits — which is a good model of both animal foraging and player travel, and produces a different cell-crossing signature from either Brownian motion or boids.

### A.12 Sampling: placing things without clumps

#### A.12.1 Blue noise, and why you want it

Uniform random points clump — that is what randomness looks like, and it is why randomly scattered trees look wrong. **Blue noise** is a point set that is random but with a guaranteed minimum spacing: no clumps, no lattice regularity. The name comes from its power spectrum having no low-frequency energy, the way blue light is the high-frequency end.

#### A.12.2 Bridson's algorithm (offered, not the default)

```
grid ← background grid with cell size r/√d          # so each cell holds ≤ 1 sample
emit an initial sample; push it to an active list
while active list is non-empty:
    pick a random sample `s` from the active list
    for k attempts:
        candidate ← uniform point in the annulus [r, 2r] around s
        if no existing sample within r (check the 3×3(×3) grid neighbourhood):
            emit it; push to active list; break
    if no candidate succeeded: remove s from the active list
```

O(n), and the standard answer. But the active list is **global mutable state**, which destroys the per-cell independence that §7.1's parallelism, resumability and single-cell repair all depend on. So it is available and it is not the default.

#### A.12.3 Hash-priority dart throwing (the default)

Same output quality, no shared state:

```
for each candidate position (derived from the cell key, deterministically):
    priority ← hash(K_cell, candidate_index)
    keep it iff no candidate within `min_spacing` has a HIGHER priority
```

Every candidate's fate depends only on its own neighbourhood, so cells generate independently and in any order. It also gives the property §9.4 needs: the **patch blast radius is provably `rounds · min_spacing`** — a change cannot propagate further than the spacing test can see, multiplied by the number of rounds. Bridson has no such bound.

#### A.12.4 Low-discrepancy sequences

"Discrepancy" measures how far a point set's local density strays from perfectly even. Random points have discrepancy `O(√(log log n)/√n)`; deliberately constructed low-discrepancy sequences reach `O((log n)^d / n)` — measurably more even, and, crucially here, **stateless**: point `i` is a closed-form function of `i`.

- **Halton**: coordinate `d` of point `n` is the radical inverse `φ_b(n)` in base `b_d` (2, 3, 5, …) — write `n` in base `b` and mirror its digits about the decimal point.
- **R2 / plastic sequence**: `x_d(n) = frac(0.5 + α_d · n)` where `α_d = ρ^(−d)` and `ρ` is the root of `x^(d+1) = x + 1`. **[verified]**: `ρ₂ = 1.324717957244746` (2D, the plastic number), `ρ₃ = 1.220744084605760` (3D). One multiply and a fractional part per coordinate.

**[measured]** 4.3× lower L2 star discrepancy than i.i.d. uniform at `n = 1024` in 3D.

Statelessness is the operational point: row `i` is a pure function of `i` and the seed, so the loader is idempotent, resumable from a single integer, and reproducible byte-for-byte across machines — which is what makes §9's manifest diff mechanical rather than heuristic.

### A.13 Constraint-based assembly (WFC), for §6.4

Wave Function Collapse is constraint propagation dressed in quantum-mechanical vocabulary; the physics analogy is decoration, not content.

```
every slot starts with the full set of allowed tiles ("superposition")
while some slot has more than one option:
    pick the slot with the FEWEST remaining options   ("minimum entropy")
    choose one of its tiles at random, weighted        ("collapse")
    propagate: remove from every neighbour any tile whose adjacency rules
               are now unsatisfiable; repeat transitively
    if a slot reaches zero options: backtrack or restart
```

It is a Sudoku solver whose constraints are "which tiles may sit next to which", and the failure mode is the same one Sudoku has: a bad early choice makes a later slot unsatisfiable.

**Merrell's model synthesis** solves that at world scale by working in **blocks with margins** — re-solve one block at a time, keeping its surroundings fixed. That is not merely a scaling trick, it is the *patch unit*: a block plus its margin is exactly the reachable set of any change. At `block = 8 slots × tile_size = 4 m = 32 m`, a block maps 1:1 onto a `chunk/{cell}/{n}` section and 64:1 onto a 128 m interest cell.

### A.14 Machinery

#### A.14.1 Morton (Z-order) encoding

Interleave the bits of the coordinates: `x₂x₁x₀`, `y₂y₁y₀`, `z₂z₁z₀` becomes `x₂y₂z₂ x₁y₁z₁ x₀y₀z₀`. Two consequences carry the whole storage design: sorting by the interleaved integer approximately sorts by spatial locality, and truncating low bits gives the enclosing parent cell — so a cell's entire subtree is one contiguous integer range. Full encoding in [01-spatial-model.md](01-spatial-model.md) §3.

Morton's known weakness is locality discontinuities at power-of-two boundaries, which is why neighbourhood reads enumerate the 27 explicit cell ranges rather than one span.

#### A.14.2 Rendezvous (highest-random-weight) hashing

To decide which node owns a shard: hash `(node_id, cell)` for every node and take the **largest**.

```
owner(cell) = argmax_{node ∈ nodes} h(node, cell)
```

Adding or removing a node only moves the keys whose argmax changed — about `1/n` of them — with no ring, no virtual nodes, and no coordination. It is consistent hashing's simpler cousin, and it is what `hot_shard_placement = "hrw_adversarial"` (§11.3) queries directly, so a scenario is adversarial to the placement function *in this binary* rather than to a model of it.

#### A.14.3 blake3 keyed modes

Two distinct modes, used for two distinct jobs:

- `derive_key(context, material)` — a KDF. The `context` string is a hardcoded, application-unique domain separator; changing it produces an entirely unrelated key space, which is why bumping `[seed] context` versions the whole derivation.
- `keyed_hash(key, data)` — a MAC. Used for each level of the seed tree.

The domain tags `b"L"`, `b"C"`, `b"E"` in §8 exist so a layer *name* can never collide with a cell id's byte pattern — without them, a layer called `"\x00\x01…"` could derive the same key as a cell.

#### A.14.4 Q16.16 fixed point

A 32-bit signed integer read as `value = i / 65536`: 16 bits of integer part, 16 of fraction. Resolution `1/65536 ≈ 1.5 × 10⁻⁵`, range `±32 768`.

It is used for one reason. Floating-point results depend on the libm implementation, the compiler's instruction selection, and FMA contraction; a one-ULP difference in `exp()` on a different machine can flip a threshold comparison, which flips a cell's count, which flips a `ContentKey`, which rewrites the world. Rounding every field value to Q16.16 **before any comparison, threshold, accumulation or split** means every count-determining path is integer arithmetic. Floats are still used to *compute* the field — they just never decide anything directly. That boundary is the determinism contract of §8, and D13 takes the same position for the simulation core.

#### A.14.5 The error function

```
erf(z) = (2/√π) · ∫₀^z e^(−t²) dt
```

The integral of a bell curve from 0 to `z`, scaled so `erf(∞) = 1`. It shows up here only because the Gini coefficient of a lognormal distribution happens to equal `erf(σ/2)` (→ A.4.3); `erfinv` is its inverse, used to solve a declared Gini back to the `σ` that produces it. Both are in `libm` and in every scientific library.

### A.15 The `ckpt/{shard}` ceiling, worked

Cited in §2 (P-8) and §13.2; here is the arithmetic in full.

`FdbCheckpointStore::checkpoint` postcard-encodes the entire `CheckpointData` — the whole entity bag included — into the single `ckpt/{shard}` FDB value. FDB's hard value limit is 100 000 bytes. With a `bag` byte component bag and ~34 B of per-entry framing (`PersistId` + length prefixes + map overhead):

```
max_entities_per_shard = 100 000 / (bag + 34)
```

**[verified]**:

| Bag size | Max entities per shard |
|---|---|
| 128 B | 617 |
| 192 B | 442 |
| **256 B** (the spec's assumption) | **344** |
| 512 B | 183 |

Against the ladder's requirements: `demo` needs 156 per shard (passes at every plausible bag size), `soak` needs 1 953 (fails at every plausible bag size), and `demo-hotspot`'s hottest shard needs 800 — which exceeds the limit for **any bag larger than 91 B**.

So the failure is not a tuning problem and cannot be configured away. D11 §6 already specifies the row as `(node_id, journal lsn, epoch, time)` — the watermark only — with entity state living in the per-entity `world/` rows that the same function already writes. P-8 is a correction to the implementation, not a change to the design.

---

## Cross-references

[08-persistence.md](08-persistence.md) §6 (keyspace), §9 (area load), §17 (world seeding — this document's normative parent) · [01-spatial-model.md](01-spatial-model.md) §3 (`CellId`), §13 (nested grids) · [06-verifiable-core.md](06-verifiable-core.md) (VC-3 RNG derivation, `universe_seed`) · [10-crates.md](10-crates.md) §11 (persistd harness) · [11-roadmap.md](11-roadmap.md) §P2 (the demo criterion), R-7 (FDB hotspots) · [DECISIONS.md](DECISIONS.md) D5, D9, D11, D12, D13, D15, D16.
