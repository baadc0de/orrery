# AGENTS.md

Guidance for AI coding agents working in this repository.

## What this is

**Orrery** is an in-development set of Rust crates for the Bevy game engine (0.19):
peer-to-peer multiplayer (QUIC transport with NAT hole punching via iroh) and a
persistent-universe backend. The repository contains the accepted architecture,
active P0–P2 implementation, test tools, and incomplete milestone harnesses.

## Reading path (normative order)

The design is documented in `docs/`. Accepted ADRs are normative over the
README and every numbered expansion document.

Start with [docs/DECISIONS.md](docs/DECISIONS.md), the ADR index and governance
entry point. Decisions live independently under [docs/adr/](docs/adr/):

- For architecture-wide work, read all accepted ADRs in numeric order.
- For scoped work, read the index, the ADRs named by the relevant expansion
  document, and any ADR dependencies those records link.
- Never treat the index summary as a substitute for the applicable ADR text.
- A future change to an accepted decision is a new ADR that explicitly
  supersedes the old one; do not silently rewrite architectural history.

| Decision | ADR | Covers |
|---|---|---|
| D1 | [ADR-0001](docs/adr/0001-requirements.md) | Requirements |
| D2 | [ADR-0002](docs/adr/0002-simulation-model.md) | Simulation model |
| D3 | [ADR-0003](docs/adr/0003-transport.md) | iroh transport |
| D4 | [ADR-0004](docs/adr/0004-bevy-netcode-stack.md) | Bevy netcode stack |
| D5 | [ADR-0005](docs/adr/0005-spatial-model.md) | Spatial model and CellId |
| D6 | [ADR-0006](docs/adr/0006-population-adaptive-topology.md) | Island topology |
| D7 | [ADR-0007](docs/adr/0007-authority-and-leases.md) | Authority and leases |
| D8 | [ADR-0008](docs/adr/0008-prediction-rollback-interpolation.md) | Prediction and rollback |
| D9 | [ADR-0009](docs/adr/0009-verifiable-core.md) | Verifiable core |
| D10 | [ADR-0010](docs/adr/0010-witnessing.md) | Witnessing |
| D11 | [ADR-0011](docs/adr/0011-persistence.md) | Persistence |
| D12 | [ADR-0012](docs/adr/0012-backend-services.md) | Backend services |
| D13 | [ADR-0013](docs/adr/0013-physics-and-determinism.md) | Physics posture |
| D14 | [ADR-0014](docs/adr/0014-pinned-versions.md) | Pinned versions |
| D15 | [ADR-0015](docs/adr/0015-crate-set.md) | Crate set |
| D16 | [ADR-0016](docs/adr/0016-parameter-reference.md) | Parameter defaults |
| D17 | [ADR-0017](docs/adr/0017-risks-and-open-questions.md) | Risks and open questions |

After the applicable ADRs, use this expansion reading path:

| Order | Document | Covers |
|---|---|---|
| 1 | [docs/00-overview.md](docs/00-overview.md) | Goals, constraints, system diagram, subsystem tour, glossary |
| 2 | [docs/01-spatial-model.md](docs/01-spatial-model.md) | Grid, `CellId` encoding, `big_space`, AOI, hysteresis, hotspots |
| 3 | [docs/02-networking.md](docs/02-networking.md) | iroh, relays, islands, topology regimes, channels, bandwidth |
| 4 | [docs/03-replication.md](docs/03-replication.md) | replicon/lightyear stack, interest sets, delta compression, priority |
| 5 | [docs/04-authority.md](docs/04-authority.md) | Weak/strong claims, leases, handoff, orphans, promotion |
| 6 | [docs/05-prediction-rollback.md](docs/05-prediction-rollback.md) | Timelines, prediction sets, reconciliation, interpolation, hit validation |
| 7 | [docs/06-verifiable-core.md](docs/06-verifiable-core.md) | `Ruleset`, determinism scoping, signed input logs, replay harness |
| 8 | [docs/07-witnessing.md](docs/07-witnessing.md) | Threat model, discrepancy protocol, adjudication, strikes |
| 9 | [docs/08-persistence.md](docs/08-persistence.md) | Cell actors, journal, FDB schema, intents, terrain, event archive |
| 10 | [docs/09-services-and-ops.md](docs/09-services-and-ops.md) | Service inventory, deployment, scaling, failure modes, telemetry |
| 11 | [docs/10-crates.md](docs/10-crates.md) | Workspace layout, per-crate API sketches, dependency graph |
| 12 | [docs/11-roadmap.md](docs/11-roadmap.md) | Build phases (P0–P6), milestones, tracked risks |
| 13 | [docs/12-world-seeding.md](docs/12-world-seeding.md) | World seeder: TOML scenario runner, generator bank, content diff/patch (expands 08 §17) |
| 14 | [docs/13-chain-replication.md](docs/13-chain-replication.md) | Cross-process journal mirroring, reconnect, and recovery |
| 15 | [docs/references.md](docs/references.md) | Annotated bibliography, organized by topic |

Also read [README.md](README.md) — it summarizes the architecture, the status,
and the feature set.

## Ground rules

- **Accepted ADRs are normative.** The records in `docs/adr/` govern the
  README and numbered docs. If an expansion conflicts with an applicable ADR,
  the ADR wins.
- **Implementation is partial.** Inspect the current tree before assuming a
  designed crate or service exists. Code sketches in `docs/10-crates.md` are
  indicative of shape, not guaranteed to match landed APIs.
- **Pinned versions ([D14](docs/adr/0014-pinned-versions.md)).** All dependency
  versions reflect the ecosystem as of August 2026 and are re-validated when
  implementation starts. Don't bump them casually.
- **Roadmap gates ([D17](docs/adr/0017-risks-and-open-questions.md)).** Each
  phase (P0–P6) has a demo criterion that is a permanent regression harness and
  gates entry to the next phase. See [docs/11-roadmap.md](docs/11-roadmap.md).

## Device-local memory

Durable, machine-local agent context lives in
[`.agents/memory/`](.agents/memory/README.md) — git-ignored, never committed.
Check its `INDEX.md` for notes on decisions, project state, environment quirks,
and open threads. Add or update entries there (dated, one file per topic) rather
than losing context between sessions. Never store secrets in it.

## Device-local agent protocol (if present)

Some machines carry a local protocol for delegating work to other coding agents.
It is **not** part of this repository — like `.agents/memory/`, it is git-ignored
and machine-specific, and most checkouts will not have it.

**If `.agents/protocol.md` exists, read it before delegating any task**; it is
authoritative for how work is handed off on that machine. It typically defines
a driver under `.agents/bin/`, a worktree-per-task layout, a document bus for
briefs and reports, and the rule that a delegated agent's self-reported
verification is re-run by the orchestrator rather than trusted.

If it is absent, there is nothing to do — do the work directly, and do not
invent a protocol or create `.agents/` scaffolding to imitate one.
