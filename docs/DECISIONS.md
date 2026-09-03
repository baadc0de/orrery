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
| D19 | [ADR-0019](adr/0019-indexed-waldb-journal.md) | Indexed wal-db journal default |
| D20 | [ADR-0020](adr/0020-journal-retention.md) | Journal retention and the recovery budget |
| D21 | [ADR-0021](adr/0021-ruleset-distribution.md) | `Ruleset` distribution and the harness API freeze |
| D22 | [ADR-0022](adr/0022-grid-id-in-the-storage-key.md) | `GridId` stays a key discriminator (P-7) |
| D23 | [ADR-0023](adr/0023-follower-journal-retention.md) | Follower journal retention and the P2 retention clause |
| D24 | [ADR-0024](adr/0024-island-drain.md) | Island drain is peer-driven; no evacuation |
| D25 | [ADR-0025](adr/0025-expire-fan-out.md) | `Expire` fan-out recipient set and amplification bound |
| D26 | [ADR-0026](adr/0026-sibling-gateways.md) | Sibling gateways: ownership, reachability, live handover |
| D27 | [ADR-0027](adr/0027-attestation-envelope.md) | Attestation envelope and required-K draw |
| D28 | [ADR-0028](adr/0028-witness-set-seeding.md) | Witness-set seeding, announcement envelope, `epoch/` record |
| D29 | [ADR-0029](adr/0029-low-population-path.md) | P5 low-population path: quarantined provisional commit, spot replay, annulment |
| D30 | [ADR-0030](adr/0030-cell-epoch-standing.md) | An intent is judged only under a cell-epoch its issuer stands in |
| D31 | [ADR-0031](adr/0031-id-account-subspace.md) | The `id/` account subspace, its reverse index, and what a miss means |
| D32 | [ADR-0032](adr/0032-enforcement-ramp.md) | Enforcement ramp: shadow semantics per control, flag inventory, promotion evidence, auto-suspend; amended 2026-09-02 by new clause (i), which closes open question 1 — posture writes are authenticated at the *reader* by an operator Ed25519 signature carried in the `ramp/{control}` row, so the FoundationDB cluster file is not authority over fleet enforcement — closes open question 3 in the negative (C2 has no `off` arm; its only demotion is `live → shadow`), and requires a mandatory expiry on any write below a control's clause (c) default; two defects in the accepting spike's own prose corrected in the same amendment — the automation arm is a conjunction rather than a rank comparison, and a refused row falls back to the startup default rather than to `shadow` |
| D33 | [ADR-0033](adr/0033-strike-ledger-standing.md) | Strike ledger, decay, and quarantine → cooldown → ban standing |
| D34 | [ADR-0034](adr/0034-candidate-accounts-announcement.md) | Candidate accounts travel in the witness-epoch announcement; amends D28 |
| D35 | [ADR-0035](adr/0035-lease-key-discriminator.md) | The lease registrar row takes the `le` discriminator inside `l`; the disjointness guard learns sub-spans |
| D36 | [ADR-0036](adr/0036-binding-rate-window.md) | The binding-rate window: `dw` answers D31 clause (g) inside `d`; amends D31 |
| D37 | [ADR-0037](adr/0037-unavailable-witness-epoch.md) | Unavailable witness epochs refuse with a bounded cure; corrects D27/D28 |
| D38 | [ADR-0038](adr/0038-at-rest-schema-versioning.md) | At-rest schema versioning as three work items: formats before machinery, migrations fit D21's freeze additively |
| D41 | [ADR-0041](adr/0041-offline-identity-issuer-custody-and-lifecycle.md) | Offline invites are single-use capabilities, account allocation is singular, issuer keys are portable escrowed secrets |
| D42 | [ADR-0042](adr/0042-canonical-simulation-architecture.md) | Canonical state stays in the engine-neutral executor; composition root and `SimulationHost` seam land now; shared app world rejected; dedicated ECS world trigger-gated; clauses (a) and (b)(2) amended 2026-08-31 — a dedicated `bevy_ecs::World` is reachable from the rules and the seam gained a per-section read surface, with (c) untouched; clause (d) restated 2026-09-02 — a dedicated world enters automatically on a fired trigger or by recorded owner decision, the trigger list standing |
| D43 | [ADR-0043](adr/0043-determinism-envelope-and-gate-replacement.md) | The three-ring determinism envelope, canonical stages S0–S7, and role-discovery replacing the typed gate list; overflow flags inside witnessed state; clause (d)'s neighbour ban narrowed 2026-08-25 to declared audited predicates; clause (e)(1) amended 2026-08-31 to admit `orrery_games` as an ECS host, with (e)(5) unamended but enforced by gate rather than by type; clause (f)(3) amended 2026-08-31 to make overflow-canonicity a required per-ruleset declaration rather than a blanket requirement |
| D44 | [ADR-44](adr/0044-identity-classes-and-allocation.md) | Three closed identity classes; granted-range derivation; no provisional durable identity; id reuse stays legal and durable readers must be lifetime-aware |
| D45 | [ADR-0045](adr/0045-per-component-capability-policy.md) | Five independent capability dimensions with fail-closed zeros and eight invalid combinations; `classify_component` replaced; IV-7's engine-handle rule accepted, its enforcement mechanism open |
| D46 | [ADR-0046](adr/0046-message-class-semantics.md) | Six message classes with internal commands collapsed onto domain events; delivered-first composition ratified as law; emission cap binds and flags rather than failing the tick |
| D47 | [ADR-47](adr/0047-rollback-unit.md) | Only prediction rewinds: canonical is correction-only, durable recovery-only, critical compensation-only; the unit is the per-entity predicted set with all-or-nothing restore |
| D48 | [ADR-0048](adr/0048-canonical-witness-projection.md) | The canonical witness projection WP-1..WP-6; `projection_version` becomes a third orthogonal version axis, widening D38 clause (d)(3) |
| D49 | [ADR-0049](adr/0049-compatibility-manifest.md) | The compatibility manifest: field set by verdict, `RulesetId.digest` gains a real scope, the seven-axis composition law, no rolling-upgrade story |
| D51 | [ADR-0051](adr/0051-v1-terrain-is-not-durable-state.md) | v1 terrain is not durable state: the `TerrainDelta` / `chunk/` promises and the `k` family are withdrawn, recovering one clean prefix byte that this decision does not spend |

## Status and supersession

Every record carries its own status and date. Accepted ADRs are normative.
A future decision that changes an accepted ADR must be added as a new ADR and
must name the record it supersedes; the superseded record then changes status
and links to its replacement. Decision numbers provide stable references, not
implicit conflict precedence.

D39 is **proposed, not accepted**: [ADR-0039](adr/0039-cell-state-eviction.md)
holds it. It is deliberately absent from the index above, which is the accepted
set; it is non-normative until the owner accepts it, and the false hot-memory
bound it exists to repair is corrected in the tree independently of it.

D40 is **proposed, not accepted**:
[ADR-0040](adr/0040-visibility-and-spatial-query-layering.md) holds it. It is
deliberately absent from the index above, which is the accepted set; it is
non-normative until the owner decides the visibility/spatial-query layering and
the accepted-record tensions the proposal names.

D50 is **proposed, not accepted**:
[ADR-0050](adr/0050-knowledge-tiers.md) holds it. It is deliberately absent
from the index above, which is the accepted set; it names the knowledge tiers
(simulation authority / visual authority / hearsay) and homes A14's hearsay
rules H1-H6. The owner signalled acceptance in principle on 2026-08-27 while
explicitly reserving the right to change their mind; it is non-normative
until the owner flips its status.

D51 was **accepted on 2026-09-02**:
[ADR-0051](adr/0051-v1-terrain-is-not-durable-state.md) records the owner's
2026-09-01 #830 decision to remove v1 durable terrain. Its consequential
amendments landed in the affected records on 2026-09-02, each recorded
against the record it touches rather than by D51 rewriting them: D11's
terrain allocation is withdrawn (status line and a dated note against the
terrain bullets); D35 §4's recount drops `k` — seventeen registered
families, one clean prefix byte; D32 clause (c)'s arithmetic and allocation
rule carry the same correction. The recovered byte is **not pre-spent** by
any of them (D51 §(c)).

D52 and D53 are **proposed, not accepted**, and are deliberately absent from
the index above, which is the accepted set.
[ADR-0052](adr/0052-client-platform-scope.md) proposes narrowing R9's platform
row to a client set (Windows first, Linux second) and a server set (Linux),
leaving the native-only/no-WASM clause verbatim and leaving the determinism
matrix and the P4 accumulation leg untouched.
[ADR-0053](adr/0053-unreal-client-host-scope.md) proposes scoping D4 — the
Bevy client is one host, an Unreal client is another — and records which
`orrery_sim_host` C ABI seams carry to such a host and which do not.

Both are **the first Orrery records to cite the game project's `G` numbers**,
which is the convention they establish: an Orrery change made to satisfy a
`G`-numbered *Mothership* requirement is an Orrery ADR citing that number, not
a game record reaching into this trail. Both therefore cite
[#1021](https://github.com/baadc0de/orrery/pull/1021), which is **open, not
merged**; their `game/docs/...` links are dead until it lands, and **neither
can be accepted before it does**. Neither amends any accepted record while
Proposed: D52's amendment of D1's R9 row and of `00-overview.md` §2 takes
effect only on acceptance, and D53 amends no record's normative text at all.

**Keyspace and at-rest allocation, 2026-09-03 (#947).**
`content/version` gained a seventh field, `universe_seed_fingerprint`, and with
it two allocations that are recorded here rather than by a new ADR, because
neither changes a decision — each spends an allowance an accepted record
already grants.

- **D31's family arithmetic is untouched.** No prefix byte was allocated and no
  sub-span was carved: `v` is the same family holding the same single key
  (`[b'v']`), and the registered-family count is unchanged. This extends a
  *value*, not the keyspace. The clean byte
  [D51](adr/0051-v1-terrain-is-not-durable-state.md) recovered stays unspent.
- **D38 clause (d)(1) allocates encoding version 1 for the `content/version`
  envelope.** The mechanism is the clause's **trailer**, not a first-byte
  schema tag: this row has an untagged predecessor, so a leading tag would be
  read by an old binary as the length varint of `content_build` and shift every
  field after it — the exact failure the trailer exists to prevent. The shape
  is `postcard(ContentVersion) ‖ 0x01`, and an **absent trailer is v0** (the
  bootstrap rule `orrery_protocol::atrest` states). Encoding versions for this
  envelope are allocated here; the next one is 2.

The consequential carve-out lives with the record it constrains rather than
here: [08-persistence.md](08-persistence.md) §6.1 states that a
domain-separated one-way fingerprint of the universe seed may appear in the
keyspace and the seed itself may not, which is what makes the field legal at
all.

D18 is reserved only as a proposal in
[ADR-0017](adr/0017-risks-and-open-questions.md); it is not accepted and has no
ADR file. D19 deliberately follows that reserved gap.

## Document map

| Document | Primary ADRs | Covers |
|---|---|---|
| [00-overview.md](00-overview.md) | D1–D17, D19 | Goals, constraints, system diagram, subsystem tour, glossary |
| [01-spatial-model.md](01-spatial-model.md) | D5 | Grid, CellId, big_space, AOI, hysteresis, hotspots, nested grids |
| [02-networking.md](02-networking.md) | D3, D6, D24 | iroh, relays, islands, topology regimes, channels, budgets |
| [03-replication.md](03-replication.md) | D4, D8 | Replicon/lightyear stack, interest sets, delta compression, priority |
| [04-authority.md](04-authority.md) | D7, D25 | Claims, leases, handoff, orphans, promotion interplay |
| [05-prediction-rollback.md](05-prediction-rollback.md) | D8 | Timelines, prediction sets, reconciliation, interpolation, hits |
| [06-verifiable-core.md](06-verifiable-core.md) | D9 | Ruleset, determinism scoping, logs, replay harness |
| [07-witnessing.md](07-witnessing.md) | D10 | Threat model, discrepancy protocol, adjudication, strikes |
| [08-persistence.md](08-persistence.md) | D11, D19, D20, D22, D23, D26 | Cell actors, journal, FDB schema, intents, terrain, event archive |
| [09-services-and-ops.md](09-services-and-ops.md) | D12, D21, D26 | Service inventory, deployment, scaling, failure modes, telemetry |
| [10-crates.md](10-crates.md) | D15, D19, D21 | Workspace, per-crate API sketches, dependency graph |
| [11-roadmap.md](11-roadmap.md) | D17, D19, D20–D23 | Build phases, milestones, risks |
| [12-world-seeding.md](12-world-seeding.md) | D11, D22 | World seeding, deterministic generation, content diff and patch |
| [13-chain-replication.md](13-chain-replication.md) | D11, D12, D20, D23 | Cross-process journal mirroring and recovery |
| [14-capacity.md](14-capacity.md) | D11, D12, D16, D19 | Measured single-box capacity envelope: the knee, what binds first, entities and players |
| [15-asset-provenance.md](15-asset-provenance.md) | — | Asset licensing bar, the provenance manifest, and the guard that keeps the record true |
| [references.md](references.md) | D1–D17, D19 | Annotated bibliography |
