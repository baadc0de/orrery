# Game ADR-0003: Technical decisions settled by the epic #1042 spikes

**Status:** Accepted · **Date:** 2026-09-04 · **Decision:** GD3

This decision is normative for the *Mothership* game project. See the [game ADR index](../DECISIONS.md). Requirement rows: G2.5, G10.2, G10.3a, G10.4a–c, G10.7, G10.8 in [00-requirements.md](../00-requirements.md). Evidence: Orrery PRs #1069 (latency), #1070 (one-body cook), #1071 (season size), #1072 (moving interiors), all merged 2026-09-04.

## Decisions

1. **Host prong: headless Bevy `App` in-process, thread pool capped, rollback driver connected** (G10.2). This is a third configuration that neither spike measured (#1043: `App`, 65 threads, handles unconnected; #1052: driver, no `App`, no transport); its numbers are owed by the slice. Evidence: the C ABI crossing costs 20–22 µs at p50 in both prongs; the `App` adds ~177 µs per frame, 1 % of a 60 Hz frame; only the `App` prong carries the shipped net and prediction stack. Rejected: the no-`App` prong (34 MB, one thread, but no networking; the net client would be rebuilt outside Bevy before the slice could connect).
2. **Collision representation: lattice triangles from the pre-simplification intermediate, Unreal collision simplification off** (G10.4a). Evidence: exact agreement on 5,000 rays; heightfields lose the 16 m ridge layer at any cell size; voxels are a different fact for rays starting underground.
3. **Playable surface is cooked regions; the rest of a body is runtime visual PCG, organised as one World Partition world per body** (G2.5, G10.4b, G10.4b'). **Overrules the spike recommendation.** #1071 priced regions, not planets, and measured no runtime-PCG path; the risk moved from download bytes to unmeasured client generation cost. Cooked cells carry terrain plus exported collision; uncooked cells are PCG runtime generation seeded per cell and streamed as presentation. The owner chose whole-planet visuals over the spike's cooked-regions-only recommendation. Obligations: the ruleset refuses play outside cooked collision; the boundary is legible in presentation; a ruleset-side deterministic runtime terrain generator is a separate spike if the whole surface is ever to be playable.
4. **Distribution: full download per season, content-digested in-season patches** (G10.4c). Evidence: inter-season patch ratio 0.94; Unreal map bytes are not deterministic (5,560 bytes over 195 runs), so file-granular patching re-ships unchanged bodies. Rejected for now: canonicalising the map save.
5. **CMC as cosmetic smoothing only** (G10.3a). **Overrules the spike recommendation.** #1072 measured CMC asserting on 35,845 of 36,000 ticks with based movement off (34,977 of them a constant 24 mm floor offset). The gate is numeric: p99 ≤ 50 mm, max ≤ 250 mm over the transitions scene, and 0 ticks of CMC output reaching intent. Fallback is full mirror placement.
6. **Interiors are attached actor hierarchies with a double-precision mirror; streamed sub-levels are excluded** (G10.7). **PSO precache is a cook obligation** (G10.8). **Replayed frame-change events are de-duplicated by (entity, tick)** in presentation.

## Not settled

- The Windows #920 measurement, still owed for Orrery D53 acceptance.
- Room-to-room transitions inside the mothership and surface landing were not reached by #1072.
- Cooked (as opposed to editor-saved) Unreal package sizes; #1071 judges they do not move the verdict.
- Regions per body, the season-size tuning variable.

## Consequences for Orrery

- D53 reads through decision 1; the 272-line rollback driver from #1052/#1072 becomes the prediction loop behind the ABI.
- The season-data distribution record takes decisions 2 and 4 as inputs.
- The ruleset's kinematic character movement (ADR-0013) must handle interiors and cooked regions as collision; a `FrameChange` record source is on the slice's path once the rollback result is relied on.
