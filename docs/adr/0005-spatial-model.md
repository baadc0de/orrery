# ADR-0005: Spatial model: one 64-bit cell ID does triple duty

**Status:** Accepted · **Date:** 2026-08-11 · **Decision:** D5

This decision is normative. See the [ADR index](../DECISIONS.md) for precedence, scope, and the complete decision set.

The universe is partitioned by a **hierarchical uniform integer grid**, canonically aligned with `big_space`'s `GridCell` (integer cell coords + local `f32` transform; solves float precision at huge coordinates). A single sortable **`CellId(u64)`** serves as:

1. **Replication interest group** — peers subscribe to their cell + the 3×3×3 neighborhood (27 cells), mapped to replicon visibility/rooms (Unreal Replication Graph / Fortnite precedent);
2. **Storage shard key prefix** — `[cell_id][entity_id]` in a range-sharded keyspace, so "load everything near me" is a handful of contiguous range scans;
3. **Authority/handoff unit** — leases, island membership, field-host promotion, and hotspot splitting all operate on cells.

**`CellId` encoding (S2-style, parent = prefix):** offset-binary (unsigned-shifted) cell coords at the finest level (21 bits/axis → ±2²⁰ cells/axis), Morton-interleaved into 63 bits, truncated to `3·level` bits, followed by a single `1` sentinel bit then zeros. Sorted order = spatial locality; a parent cell's entire subtree is one key range. Morton for the runtime/network ID (cheapest); optional Hilbert mapping (via `lindel`) at the storage layer only if scan locality measurably matters. Games needing more range use nested `big_space` grids or a `u128` feature.

**Parameters (defaults):** interest-level cell edge ≈ AOI radius (default **128 m**); shard level = interest level −3 (one shard cell = 8×8×8 interest cells); handoff hysteresis margin = **10% of cell edge** (an entity keeps its cell/authority while inside the overlap zone — SpatialOS anti-thrash lesson).

**Nested grids (moving reference frames).** A carrier whose contents move together and interact mostly with each other (a crewed ship, an inhabited planet) is a *nested* `big_space` grid with its own `CellId` space (`GridId` carried alongside); its velocity lives at the grid root, never in its contents — so a 500 m/s cruiser crosses cells as *one* entity, and crew walk at 5 m/s in ship space under ordinary witness validation. Frame crossings are teleports when frames are stationary relative to each other (docking) and continuous **frame migrations** otherwise (EVA), logged as `FrameChange` records so replay stays closed across the basis change. Interaction requires frame coincidence; cross-frame observation sees the carrier root as one entity. Elaborated in `01-spatial-model.md` §13.

**Rejected:** S2/H3 proper (spherical geodesy is wrong for abstract 3D space; we copy S2's bit layout only); adaptive octrees as the *partition* unit (unstable group IDs, handoff storms; octrees/k-d trees — `kiddo` — are per-cell in-memory query structures only); Voronoi/VAST overlays (academically elegant, never shipped, no storage story); `bevy_spatial` (stalled at Bevy 0.16). **Risk:** `big_space` 0.12 targets Bevy 0.18 — budget a small upstream port to 0.19.

