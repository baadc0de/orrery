# Game ADR-0003: Technical decisions settled by the epic #1042 spikes

**Status:** Accepted · **Date:** 2026-09-04 · **Decision:** GD3

This decision is normative for the *Mothership* game project. See the [game ADR index](../DECISIONS.md). Requirement rows: G2.5, G10.2, G10.3a, G10.4a–c, G10.7, G10.8 in [00-requirements.md](../00-requirements.md). Evidence: Orrery PRs #1069 (latency), #1070 (one-body cook), #1071 (season size), #1072 (moving interiors), all merged 2026-09-04.

## Decisions

1. **Host prong: headless Bevy `App` in-process, thread pool capped** (G10.2). Evidence: the C ABI crossing costs 20–22 µs at p50 in both prongs; the `App` adds ~177 µs per frame, 1 % of a 60 Hz frame; only the `App` prong carries the shipped net and prediction stack. Rejected: the no-`App` prong (34 MB, one thread, but no networking; the net client would be rebuilt outside Bevy before the slice could connect).
2. **Collision representation: lattice triangles from the pre-simplification intermediate, Unreal collision simplification off** (G10.4a). Evidence: exact agreement on 5,000 rays; heightfields lose the 16 m ridge layer at any cell size; voxels are a different fact for rays starting underground.
3. **Playable surface is cooked regions; the rest of a body is runtime visual PCG** (G2.5, G10.4b). The owner chose whole-planet visuals over the spike's cooked-regions-only recommendation. Obligations: the ruleset refuses play outside cooked collision; the boundary is legible in presentation; a ruleset-side deterministic runtime terrain generator is a separate spike if the whole surface is ever to be playable.
4. **Distribution: full download per season, content-digested in-season patches** (G10.4c). Evidence: inter-season patch ratio 0.94; Unreal map bytes are not deterministic (5,560 bytes over 195 runs), so file-granular patching re-ships unchanged bodies. Rejected for now: canonicalising the map save.
5. **CMC as cosmetic smoothing only** (G10.3a). The owner chose to keep CharacterMovementComponent for foot placement and slope alignment, against the spike's recommendation of full mirror placement, with the obligation that its offset is bounded, gated, and never feeds back. Fallback is full mirror placement.
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
