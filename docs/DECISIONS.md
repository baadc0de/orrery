# Orrery — Architecture Decision Records

**Status:** Accepted architecture · **Initial decision date:** 2026-08-11 · **Naming:** the `orrery` prefix is provisional and mechanically replaceable.

Orrery's architectural decisions are maintained as independent records under
[`docs/adr/`](adr/). The accepted ADRs collectively govern the README and the
numbered expansion documents. Where an expansion document conflicts with an
applicable accepted ADR, the ADR wins.

This file is the index and governance entry point; it does not duplicate the
decision text. Read all ADRs in numeric order for architecture-wide work. For a
scoped change, read the ADRs named by the relevant expansion document and follow
their linked dependencies.

## Decision index

| Decision | Record | Scope |
|---|---|---|
| D1 | [ADR-0001](adr/0001-requirements.md) | Settled product and system requirements |
| D2 | [ADR-0002](adr/0002-simulation-model.md) | Per-entity authority state replication with prediction |
| D3 | [ADR-0003](adr/0003-transport.md) | iroh QUIC transport, hole punching, and relays |
| D4 | [ADR-0004](adr/0004-bevy-netcode-stack.md) | aeronet, bevy_replicon, and lightyear stack |
| D5 | [ADR-0005](adr/0005-spatial-model.md) | Hierarchical grid and canonical CellId |
| D6 | [ADR-0006](adr/0006-population-adaptive-topology.md) | Population-adaptive island topology |
| D7 | [ADR-0007](adr/0007-authority-and-leases.md) | Authority claims, leases, and handoff |
| D8 | [ADR-0008](adr/0008-prediction-rollback-interpolation.md) | Prediction, rollback, and interpolation |
| D9 | [ADR-0009](adr/0009-verifiable-core.md) | Scoped deterministic verifiable core |
| D10 | [ADR-0010](adr/0010-witnessing.md) | Witnessing, adjudication, and attested writes |
| D11 | [ADR-0011](adr/0011-persistence.md) | Cell actors, journal, and FoundationDB persistence |
| D12 | [ADR-0012](adr/0012-backend-services.md) | Operated backend service inventory |
| D13 | [ADR-0013](adr/0013-physics-and-determinism.md) | Physics and determinism posture |
| D14 | [ADR-0014](adr/0014-pinned-versions.md) | Pinned dependency versions |
| D15 | [ADR-0015](adr/0015-crate-set.md) | Crate set and dependency spine |
| D16 | [ADR-0016](adr/0016-parameter-reference.md) | Default parameter reference |
| D17 | [ADR-0017](adr/0017-risks-and-open-questions.md) | Known risks and open questions |

## Status and supersession

Every record carries its own status and date. Accepted ADRs are normative.
A future decision that changes an accepted ADR must be added as a new ADR and
must name the record it supersedes; the superseded record then changes status
and links to its replacement. Decision numbers provide stable references, not
implicit conflict precedence.

D18 is reserved only as a proposal in
[ADR-0017](adr/0017-risks-and-open-questions.md); it is not accepted and has no
ADR file.

## Document map

| Document | Primary ADRs | Covers |
|---|---|---|
| [00-overview.md](00-overview.md) | D1–D17 | Goals, constraints, system diagram, subsystem tour, glossary |
| [01-spatial-model.md](01-spatial-model.md) | D5 | Grid, CellId, big_space, AOI, hysteresis, hotspots, nested grids |
| [02-networking.md](02-networking.md) | D3, D6 | iroh, relays, islands, topology regimes, channels, budgets |
| [03-replication.md](03-replication.md) | D4, D8 | Replicon/lightyear stack, interest sets, delta compression, priority |
| [04-authority.md](04-authority.md) | D7 | Claims, leases, handoff, orphans, promotion interplay |
| [05-prediction-rollback.md](05-prediction-rollback.md) | D8 | Timelines, prediction sets, reconciliation, interpolation, hits |
| [06-verifiable-core.md](06-verifiable-core.md) | D9 | Ruleset, determinism scoping, logs, replay harness |
| [07-witnessing.md](07-witnessing.md) | D10 | Threat model, discrepancy protocol, adjudication, strikes |
| [08-persistence.md](08-persistence.md) | D11 | Cell actors, journal, FDB schema, intents, terrain, event archive |
| [09-services-and-ops.md](09-services-and-ops.md) | D12 | Service inventory, deployment, scaling, failure modes, telemetry |
| [10-crates.md](10-crates.md) | D15 | Workspace, per-crate API sketches, dependency graph |
| [11-roadmap.md](11-roadmap.md) | D17 | Build phases, milestones, risks |
| [12-world-seeding.md](12-world-seeding.md) | D11 | World seeding, deterministic generation, content diff and patch |
| [13-chain-replication.md](13-chain-replication.md) | D11, D12 | Cross-process journal mirroring and recovery |
| [14-capacity.md](14-capacity.md) | D11, D12, D16 | Measured single-box capacity envelope: the knee, what binds first, entities and players |
| [references.md](references.md) | D1–D17 | Annotated bibliography |
