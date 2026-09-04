# Game ADR-0002: Unreal Engine 5.8 client with in-process Orrery and cooked season content

**Status:** Accepted · **Date:** 2026-09-03 · **Decision:** GD2

This decision is normative for the *Mothership* game project. See the [game ADR index](../DECISIONS.md). Requirement text: [00-requirements.md §G10](../00-requirements.md#g10--client-engine-and-content-pipeline).

## Decision

1. The client is **Unreal Engine 5.8**, pinned. PCG, Lumen, **MegaLights** and **Substrate** (all production-ready) and Mesh Terrain, Nanite Foliage and the Procedural Vegetation Editor (all Experimental in 5.8) are used for presentation. Lighting is fully dynamic with no baked path; Substrate is the sole material model, deferred rendering only. Experimental status is accepted because nothing canonical depends on any of them.
2. **Orrery runs in-process**: a Rust static library in an Unreal plugin behind a C ABI, Bevy headless inside the game process. Unreal actors mirror engine-neutral canonical state per Orrery ADR-0042; Unreal replication, CharacterMovementComponent and Chaos are presentation only.
3. **Season content is cooked with dual output**: one commandlet run per season seed produces the Unreal package and the deterministic ruleset collision package, distributed together with a shared digest. PCG runs at cook time only.
4. **Platforms**: Windows first, Linux second, macOS dropped for the client. Server builds stay engine-free on Linux. This amends Orrery R9 and is to be recorded on the Orrery trail.
5. **No runtime terrain deformation**; destruction is scoped to structures and vehicles.

## Rejected

- **Out-of-process sidecar** over `orrery_ipc`: kept as the fallback if in-process latency or crash containment proves unacceptable; not the primary path.
- **Runtime PCG on clients with hash comparison**: the server has no engine, and PCG determinism across GPUs and versions is unproven.
- **Ruleset-generated terrain with Unreal decoration**: forgoes Mesh Terrain authoring and duplicates a terrain generator.
- **Production-ready features only**: loses caves and overhangs (G4.6) and foliage density.
- **Keeping macOS**: no Lumen hardware raytracing there.

## Consequences for Orrery

- A C ABI for the network client (connect) and, above all, the prediction loop, spawn/despawn streaming, interpolation, area-of-interest and the hit-claim path, all of which are Bevy plugins today; `orrery_sim_host` already exports step, snapshot, restore, command and event calls. Plus a `staticlib` crate type and a Windows-capable C-consumer proof. Proposed as Orrery D53 (PR #1022).
- A Windows measurement of the sidecar IPC threshold (#920) before D52/D53 are accepted, so the in-process choice rests on evidence.
- A season cook step in the ruleset-distribution path (ADR-0021) that emits and digests the collision package.
- Amendments to R9 (platforms, Orrery D52) and ADR-0004 (the Bevy client is one host; the Unreal host is another, Orrery D53), both in PR #1022. D52 recommends narrowing R9 now and dropping the macOS asset only when the Bevy client stops being the playtest client, since a macOS volunteer banked verified minutes on 2026-09-02 and the CI gates assert a three-runner matrix.
