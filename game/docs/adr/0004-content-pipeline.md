# Game ADR-0004: AI-driven content pipeline

**Status:** Accepted · **Date:** 2026-09-04 · **Decision:** GD4

This decision is normative for the *Mothership* game project. See the [game ADR index](../DECISIONS.md). Requirement rows: G12–G12.8 in [00-requirements.md](../00-requirements.md).

## Decision

1. Meshes are produced by a staged, AI-driven pipeline: design document → concept art → callout sheets → model → detailing, texturing and skinning → animation → Unreal. Every stage emits an artifact with a complete provenance record (inputs, prompt, seed, model and version, tool path, licence, reviewer), under Orrery's asset-provenance guard (doc 15).
2. Generators are **open-weight and local by default**, with **hosted services for hero assets**; the record says which.
3. **Two human gates**: concept art, and final in-engine. All intermediate stages pass automated checks whose numbers come from the callout sheet.
4. **glTF 2.0** between stages; USD for scene assembly later; Interchange on import.
5. **UE5 skeleton** for humanoids; **Control Rig driven by mirror state** for mechs and ships.
6. The Unreal stage runs through the editor's **MCP server**, with pipeline steps registered as custom tools.
7. A **style bible** conditions the concept stage per faction; **licence is checked at generation time** and territory-restricted model licences are excluded.

## Rejected

- Hosted services only: fastest start and best rigging, but reproducibility and terms rest with the vendor for every asset.
- Local only: forgoes current hero-asset quality.
- A human gate at every stage: no unattended runs.
- USD end to end: heavier tooling, fewer generators emit it.
- Generator-native rigs: a different skeleton per asset.
- MetaHuman for avatars: high per-character cost for a game whose clones are one body type.

## Consequences

- A content-addressed artifact store and a provenance-chain extension to doc 15's guard.
- Per-class budgets (triangles per LOD, texel density, texture sizes, collision primitives, bones) live in the callout sheet and are the checks' source of truth.
- A private asset path for hero assets whose terms fail public redistribution; the season-data distribution record must allow it.
- Local generation is a leased, heavy GPU job on the shared box.
