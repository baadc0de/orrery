# Game ADR-0002: Unreal Engine 5.8 client with in-process Orrery and cooked season content

**Status:** Accepted · **Date:** 2026-09-03 · **Decision:** GD2

This decision is normative for the *Mothership* game project. See the [game ADR index](../DECISIONS.md). Requirement text: [00-requirements.md §G10](../00-requirements.md#g10--client-engine-and-content-pipeline).

## Decision

1. The client is **Unreal Engine 5.8**, pinned. PCG, Lumen, **MegaLights** and **Substrate** (all production-ready) and Mesh Terrain, Nanite Foliage and the Procedural Vegetation Editor (all Experimental in 5.8) are used for presentation. Lighting is fully dynamic with no baked path; Substrate is the sole material model, deferred rendering only. Experimental status is accepted because nothing canonical depends on any of them.
2. **Orrery runs in-process**: a Rust static library in an Unreal plugin behind a C ABI, Bevy headless inside the game process. Unreal actors mirror engine-neutral canonical state per Orrery ADR-0042; Unreal replication, CharacterMovementComponent and Chaos are presentation only.
3. **Season content is cooked with dual output**: one commandlet run per season seed produces the Unreal package and the deterministic ruleset collision package, distributed together with a shared digest. PCG runs at cook time only.
4. **Platforms**: Windows, Linux and macOS are all client targets from the start (the 2026-09-03 macOS drop was overturned by the owner on 2026-09-04: a macOS build and test machine is available and much of the early tester pool is on macOS). Server builds stay engine-free on Linux. R9 stands; the platform amendment on the Orrery trail is withdrawn or reduced to the client-versus-server split.
5. **No runtime terrain deformation**; destruction is scoped to structures and vehicles.

## Rejected

- **Out-of-process sidecar** over `orrery_ipc`: kept as the fallback if in-process latency or crash containment proves unacceptable; not the primary path. Measured on Windows at N=24 (#1076): 136.6 µs p50 added, p99.9 of 1.03 ms with `timeBeginPeriod` raised and 14.8 ms without; the boundary, not the simulation, is the cost. If ever deployed, raising the timer resolution is an obligation.
- **Runtime PCG on clients with hash comparison**: the server has no engine, and PCG determinism across GPUs and versions is unproven.
- **Ruleset-generated terrain with Unreal decoration**: forgoes Mesh Terrain authoring and duplicates a terrain generator.
- **Production-ready features only**: loses caves and overhangs (G4.6) and foliage density.
- **Dropping macOS**: proposed 2026-09-03 on rendering grounds, overturned 2026-09-04 on tester-pool and build-machine grounds. The rendering cost moves to a macOS configuration to be verified on Metal.

## Consequences for Orrery

- A C ABI for the network client (connect) and, above all, the prediction loop, spawn/despawn streaming, interpolation, area-of-interest and the hit-claim path, all of which are Bevy plugins today; `orrery_sim_host` already exports step, snapshot, restore, command and event calls. Plus a `staticlib` crate type and a Windows-capable C-consumer proof. Proposed as Orrery D53 (PR #1022).
- A Windows measurement of the sidecar IPC threshold (#920) before D52/D53 are accepted, so the in-process choice rests on evidence.
- A season cook step and a season-data distribution record (ADR-0021 covers link-time `Ruleset` distribution only) that emits and digests the collision package. The Rust-side geometry hit test is new: today `orrery/src/hit.rs` adjudicates entity-versus-entity poses and `GeometryFrame` is deferred.
- An amendment to ADR-0004 (the Bevy client is one host; the Unreal host is another, Orrery D53) in PR #1022. D52 (client platform scope) is overturned as to macOS; whatever survives of it is the client-versus-server platform split only.
