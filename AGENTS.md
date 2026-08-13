# AGENTS.md

Guidance for AI coding agents working in this repository.

## What this is

**Orrery** is a planned set of Rust crates for the Bevy game engine (0.19):
peer-to-peer multiplayer (QUIC transport with NAT hole punching via iroh) and a
persistent-universe backend. **Architecture and design phase — no code exists
yet.** This repository currently contains only the architecture decision record
and its expansion documents.

## Reading path (normative order)

The design is documented in `docs/`. The ADR is normative over the README and
every numbered doc:

| Order | Document | Covers |
|---|---|---|
| 1 | [docs/DECISIONS.md](docs/DECISIONS.md) | The ADR: every decision, alternatives, D16 parameter table. **Normative.** |
| 2 | [docs/00-overview.md](docs/00-overview.md) | Goals, constraints, system diagram, subsystem tour, glossary |
| 3 | [docs/01-spatial-model.md](docs/01-spatial-model.md) | Grid, `CellId` encoding, `big_space`, AOI, hysteresis, hotspots |
| 4 | [docs/02-networking.md](docs/02-networking.md) | iroh, relays, islands, topology regimes, channels, bandwidth |
| 5 | [docs/03-replication.md](docs/03-replication.md) | replicon/lightyear stack, interest sets, delta compression, priority |
| 6 | [docs/04-authority.md](docs/04-authority.md) | Weak/strong claims, leases, handoff, orphans, promotion |
| 7 | [docs/05-prediction-rollback.md](docs/05-prediction-rollback.md) | Timelines, prediction sets, reconciliation, interpolation, hit validation |
| 8 | [docs/06-verifiable-core.md](docs/06-verifiable-core.md) | `Ruleset`, determinism scoping, signed input logs, replay harness |
| 9 | [docs/07-witnessing.md](docs/07-witnessing.md) | Threat model, discrepancy protocol, adjudication, strikes |
| 10 | [docs/08-persistence.md](docs/08-persistence.md) | Cell actors, journal, FDB schema, intents, terrain, event archive |
| 11 | [docs/09-services-and-ops.md](docs/09-services-and-ops.md) | Service inventory, deployment, scaling, failure modes, telemetry |
| 12 | [docs/10-crates.md](docs/10-crates.md) | Workspace layout, per-crate API sketches, dependency graph |
| 13 | [docs/11-roadmap.md](docs/11-roadmap.md) | Build phases (P0–P6), milestones, tracked risks |
| 14 | [docs/12-world-seeding.md](docs/12-world-seeding.md) | World seeder: TOML scenario runner, generator bank, content diff/patch (expands 08 §17) |
| 15 | [docs/references.md](docs/references.md) | Annotated bibliography, organized by topic |

Also read [README.md](README.md) — it summarizes the architecture, the status,
and the feature set.

## Ground rules

- **The ADR is normative.** `docs/DECISIONS.md` (D1–D17) governs the README and
  every numbered doc. If something conflicts, the ADR wins.
- **Design phase, no code.** Don't assume implementation exists. Code sketches
  in `docs/10-crates.md` are indicative of shape, not guaranteed to compile.
- **Pinned versions (D14).** All dependency versions reflect the ecosystem as of
  August 2026 and are re-validated when implementation starts. Don't bump them
  casually.
- **Roadmap gates (D17).** Each phase (P0–P6) has a demo criterion that is a
  permanent regression harness and gates entry to the next phase. See
  [docs/11-roadmap.md](docs/11-roadmap.md).

## Device-local memory

Durable, machine-local agent context lives in
[`.agents/memory/`](.agents/memory/README.md) — git-ignored, never committed.
Check its `INDEX.md` for notes on decisions, project state, environment quirks,
and open threads. Add or update entries there (dated, one file per topic) rather
than losing context between sessions. Never store secrets in it.