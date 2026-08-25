# A8 — Compatibility and module manifests (#404)

**Status:** decision proposal for the #395 planning tree · **Date:** 2026-08-25 ·
**Tree:** `docs/404-a8` (based on `main` at `a82c062e`) · **Parents:**
[#404](https://github.com/baadc0de/orrery/issues/404) ←
[#395](https://github.com/baadc0de/orrery/issues/395) · **Builds on:**
[A1](a1-ruleset-architecture-map.md), [A2](a2-kernel-game-module-ownership.md),
[A3](a3-simulation-host-comparison.md) (+ its preserved
[second opinion](a3-simulation-host-second-opinion.md)),
[A4](a4-deterministic-execution.md), [A5](a5-identity-and-capabilities.md),
[A6](a6-commands-events-transactions.md),
[A7](a7-persistence-rollback-witnessing.md) · **Source brief:**
[docs/plans/ruleset-ecs-migration-brief.md](ruleset-ecs-migration-brief.md)
§Compatibility and versioning

This node owns what every predecessor deferred to it: the **manifest
construct** (A2 §5.3, A3 §7 handoff), the **schedule digest's storage format**
(A4 §3.10, §8), the **capability registry's home and schema-id governance**
(A5 §4, §8.3), the volume-bound constants' eventual home (A6 §12), and the
**storage of `projection_version`** (A7 WP-6, §7.2). It also owns four
acceptance items: peer compatibility rules, persisted-universe/replay
compatibility rules, a rolling-upgrade verdict, and module removal/replacement
behaviour — plus the static-versus-dynamic decision.

Method, as in the predecessors:

- Every claim cites a file and line opened on this tree today. Where this
  document asserts a property is *enforced*, either the guarded stage was
  broken here — named check died with its real result line, revert re-passed
  (§11) — or a predecessor's mutation is relied on re-based (`git diff
  a82c062e..HEAD -- crates gates clients` is empty; this branch adds only this
  document).
- What **exists**, what is **proposed**, and what belongs to another owner or
  the ADR gate never share a sentence.
- Accepting anything below is the owner's call (#395: propose, do not
  decide); ADR text belongs to A11 (#407).

---

## 1. Ground truth inherited and verified on this tree

| # | Finding | Verification |
|---|---|---|
| I1 | `GatewayMsg::protocol_accepted` is **exact equality** — `offered == current` — and the doc comment records that D29 clause 5 *closed* the former `{V, V−1}` rolling window "once, for all traffic" | `crates/orrery_protocol/src/gateway.rs:164-184`; test-pinned by `the_accepted_version_window_is_closed_to_exactly_this_version` (gateway.rs:1015-1021), whose liveness this document re-proved by mutation M-A8-1 (§11) |
| I2 | The wire protocol constant is `PROTOCOL_VERSION: u16 = 5`; `VersionedHello` is "the only live bootstrap" and every admitted session has had its version checked for exact equality | `crates/orrery_protocol/src/protocol.rs:68`, `:106-118`; gateway.rs:104-119 |
| I3 | `RulesetId { version: u32, digest: [u8; 32] }` is pinned into frames, claims, bundles and strike rows (verifiable.rs:170, :203, :213, :292) and into cluster-authored corrections ("only the game build named by `ruleset` knows how to install it") | `orrery_protocol/src/verifiable.rs:59-64`; `authority.rs:16-34` (field at :32); pinning list matches A1 §5.4 |
| I4 | Adjudication routes a bundle to the build whose `RulesetId` equals the bundle's, answering `UnknownRuleset` when none matches — never a strike | `crates/orrery_persistd/src/adjudication.rs:388-400`; `UnadjudicableReason::UnknownRuleset` at verifiable.rs:567 |
| I5 | **The ruleset digest is a placeholder pattern, not a computed hash.** Regolith ships `[0x63; 32]`; Skirmish ships `[0x5C; 32]` under a comment that states it plainly: "The digest is a placeholder pattern rather than a real build hash: nothing in the tree computes one yet" | regolith/mod.rs:74-77; skirmish/mod.rs:94-104. New finding — no predecessor recorded it (see §12) |
| I6 | At-rest schema machinery is live: per-slot `(ComponentTypeId, SchemaVersion)` framing, bag-level floor derived from the slots, bootstrap rule absent == v0, versions orthogonal to `RulesetId.version` (D38 (d)(3): "neither number is ever derived from the other") | `orrery_protocol/src/atrest.rs:14-27` (module doc), SCHEMA_V0 at :82; `orrery_persistd/src/schema.rs:13-58`; D38 clause (d)(3) at docs/adr/0038:198-205 |
| I7 | Unknown component ⇒ refuse, future version ⇒ refuse, missing step ⇒ refuse — fail-closed at every gap; liveness mutation-proven by A5 X3 and re-proven fresh here by M-A8-2 | `orrery_persistd/src/migration.rs:74-101`; A5 §9 X3; this tree: `missing_registration_refuses_stale_checkpoint` FAILED then 6/0 after revert (§11) |
| I8 | The bulk uplink makes **no schema statement**: `DiffUplink.payload` is "the postcard-encoded component payload" with no `ComponentTypeId` and no `SchemaVersion`; the cell actor consequently resets a diff-overwritten bag's floor to v0 under a comment that names the framed-bag producer as the fix | gateway.rs:371-393 (payload doc :383); actor.rs:1299-1309 (quote: "A diff arrives from a peer that makes no schema statement … When a producer starts framing its bags … the declared floor arrives on the uplink"); A5 G-3, re-verified line-fresh |
| I9 | F-1 stands as A7 recorded it: `DiffUplink.tick` is documented "The universe tick at append (D8)" (:378) but the only production writer stamps a client-local per-entity sequence starting at 0 (`seq.next.entry(entity).or_insert(0); let tick = *seq_num`) | gateway.rs:377-378; feed.rs:81-92. Disposition remains the owner's (A7 §9.5) |
| I10 | `RETAINED_BUILDS = 3` bounds adjudicable builds; registering a fourth retires the oldest; a report for a retired build is `Unadjudicable(UnknownRuleset)`, never a strike. Eviction liveness re-proven here by M-A8-3 (two named tests die) | adjudication.rs:33, :308-310, :350-359, :393-400; tests `only_three_builds_stay_adjudicable` (:851-857) and `a_report_for_a_retired_build_is_undecidable_not_a_strike` (:859-871) |
| I11 | Link-time composition is **accepted law** (D21): "WASM sandboxing is not adopted, and no dynamic `Ruleset` loading path is built"; the frozen surface is persistd's public exports; "Additive change … is not breaking and needs no record" | docs/adr/0021:40-42, :61-64; composition-time registration as the additive mechanism is D38 clause (c)'s own ruling (docs/adr/0038:136-173) |
| I12 | A rolling deploy's continuity story exists *only* on the cluster evidence path: "Rolling deploys keep old builds alive for the adjudication retention horizon (three builds, D12); evidence older than that resolves as `Unadjudicable` — never a strike" | docs/adr/0021:93-97 |
| I13 | A4's schedule digest is fully specified and awaits exactly this node: blake3 over ordered stages, per-stage ordered system names, ordering edges sorted lexicographically, ambiguity-detection setting, executor policy — "pinned into the game manifest (format owned by **A8**, #404)"; wire placement explicitly left to the owner | docs/plans/a4-deterministic-execution.md §3.10, §11 item 3 |
| I14 | A7's `projection_version` (WP-6) proposes a third version axis, "carried in the manifest (format A8's) beside `RulesetId` and the schedule digest", value 1 today, bumped only on WP-2/WP-3 framing change; WP-3 assumes the framed-bag slot shape `(ComponentTypeId, SchemaVersion, payload)` survives this node | docs/plans/a7-persistence-rollback-witnessing.md §5.1 WP-3/WP-6, §7.2, §12 item 4 |
| I15 | Capability declarations key off `(ComponentTypeId, SchemaVersion)` with five dimensions P/R/W/N/A, zeros failing closed; who allocates `ComponentTypeId` values across modules and how the pair enters the compatibility manifest are named as **this node's** questions | docs/plans/a5-identity-and-capabilities.md §4 (N-5), §5.2 (N-7), IV-8; A2 §6 flagged the same governance to A8 |
| I16 | The repository already operates one manifest idiom — the seeder's content manifest — with a toolchain stamp and a rolling blake3 digest, built so "a golden-manifest CI test shifts as a reviewed diff on a toolchain bump". It establishes house style for stamps and digests, but answers a different question (what content was seeded), not compatibility | `crates/orrery_seed/src/manifest.rs:52-97` (ToolchainStamp), :99-113 (ManifestDigest encoding); docs/12-world-seeding.md §9.3 |
| I17 | No meaningful kernel version constant exists: every workspace crate is `0.1.0`; the toolchain channel is pinned at `rustc 1.96.0` in `rust-toolchain.toml`; dependency pins are D14's | crates/*/Cargo.toml `version = "0.1.0"` (core, protocol, games checked); rust-toolchain.toml:2; docs/adr/0014 |

### 1.1 What the acceptance items resolve against

Two of them have short answers because the ground truth is already decisive:

- **Peer compatibility.** The wire already refuses any unupgraded participant,
  by exact equality, at the only bootstrap there is (I1/I2). Any manifest
  field that wanted to *widen* admission would be reopening a window D29
  closed deliberately — a protocol decision reserved to the owner, and this
  document does not make it.
- **Static versus dynamic.** D21 is Accepted and says the words (I11). This
  node ratifies static composition and extends it with the manifest
  consequence (§8); it does not relitigate the record.

The remaining work — the manifest field set itself, where it lives, how the
version axes compose, removal/replacement behaviour, and the replay/persistence
rules — is §§2–9.

## 2. The manifest field set

The brief's candidate list (brief:585-595) is taken field by field. Three
verdicts appear: **keep** (carried as proposed), **reshape** (the concern is
real; the proposed form is not what this tree can honestly carry), and
**reject** (no consumer exists; manufacturing the axis would repeat the
`classify_component` mistake of code with no caller). One field the brief did
not list is added (`manifest_format_version`) because D38 clause (d)(1)'s
lesson — every long-lived format becomes self-describing, bootstrap rule
stated in one place — applies to this one too.

| Field | Verdict | Decision |
|---|---|---|
| `game_id` | **keep** | A stable string naming the universe's rules family. Genuinely additive: `RulesetId` carries only `{version, digest}` (I3), so two games shipping version 8 would be indistinguishable to any tool that sees ids without context. No wire consumer today — carried in the manifest, not proposed for `VersionedHello` |
| `manifest_format_version` | **add** | u32, absent == 0 by the at-rest bootstrap rule (atrest.rs:14-21 applied to this format). Bumped on any change to the manifest's own framing |
| `protocol_version` | **keep** | u16, exact-equality domain (I1/I2). The manifest *records* it; it does not widen it |
| `kernel_version` | **reshape** | Not a number — every crate is `0.1.0` (I17), so a numeric kernel version would be meaningless today. Carried instead as an **advisory toolchain stamp** (rustc channel + target), the seed manifest's existing idiom (I16). The load-bearing half of "which kernel built this" belongs to the digest obligation of §2.1: peers never link a kernel separately from a game build — "the same build links into peers, field hosts and persistd" (ruleset.rs:3-4) — so kernel drift *is* build drift once the digest is real |
| `RulesetId {version, digest}` | **keep struct; add obligation** | The struct is frozen into frames, claims, bundles, corrections and strike rows (I3) and routes adjudication (I4). What changes is §2.1: the digest stops being a placeholder |
| `module_id -> module_version` | **keep, future-facing** | Defined now so the composition root (phase 2 of the brief) lands against a fixed target shape; no module system exists yet (A2/A3). Modules are statically linked registrations; see §8 for why they can never be anything else. Versioning per module mirrors `SchemaVersion`: monotone, never reused or gapped |
| `component_schema_id -> schema_version` (+ capabilities) | **keep and extend** | The table of `(ComponentTypeId, SchemaVersion-current)` pairs, each row carrying the five capability values P/R/W/N/A from A5 N-7. This one table answers both the brief's schema question and A7 §7.2's contents list: witnessed schemas are the rows filtered `W ≥ 1`; excluded data is derivable from the zeros rather than enumerated |
| `command_schema_id -> schema_version` | **reject** | Commands have no independent encoding to version. External commands are `CoreInput` entries sealed into signed logs (A6 G4); internal commands are deliberately collapsed onto events (A6 §2.1); durable consequences ride intent ops whose ids are kernel-reserved or game-opaque (intent/mod.rs:208-210). Their shapes change with the build, i.e. under `RulesetId`. A second axis here would be a version number nothing consumes |
| schedule topology | **keep as A4 wrote it** | Carried verbatim as `schedule_digest`, A4 §3.10's blake3 over ordered stages, system names, edges, ambiguity setting and executor policy. This node adds only its storage and equality domain (§4); it does not move the wire placement A4 already reserved to the owner |
| canonical configuration hash | **reject, with the argument** | There is no runtime configuration channel into canonical execution to hash: VC-8 bans ambient reads including `std::env::var` in gated crates ("the environment reaches a rule only as a logged input", core-gates.sh:100-106). Everything outcome-affecting is code, hence inside the digest obligation of §2.1. Operational parameters (D16's cadences, budgets) are deliberately non-canonical and stay out. If a runtime-config seam is ever wanted it needs its own determinism story and its own ADR |
| determinism profile | **reshape to `profile_id`** | Exactly one profile exists — the D9 envelope with A4's three rings — so a free-form hash would have nothing to distinguish. Carried as an identifier whose single legal value names that envelope, so a future second profile cannot silently claim compatibility with builds it cannot replay |

### 2.1 The digest obligation (new, proposed)

The placeholder digests (I5) are the one place where the compatibility story
is currently **honest but empty**: `RulesetId` is pinned everywhere evidence
flows, yet two different builds could ship identical `{version, digest}` pairs
and route to whichever registered first.

**Proposed (X-1): `RulesetId.digest` becomes blake3 over the
determinism-relevant source closure of the build** — the game crate(s) plus
every first-party kernel crate they transitively depend on, hashed over source
content at the pinned toolchain, with the enumeration of contributing crates
itself part of the hashed input. Scope decided here; mechanism (build script,
CI artifact, or lazy runtime computation) unpriced and owned by the
implementation epic — flagged in §10.

Two boundaries on X-1, stated so it is not oversold:

- **Routing and compatibility, not authenticity.** The tamper model keeps the
  honest id on purpose — a tampered build claims the honest `RulesetId`
  (game.rs:33-37; A1 §5.4) — and witnessing, not hashing, is what convicts.
  X-1 makes the id mean *this build*, not *a trustworthy build*.
- **Nothing verifies it yet.** Today no check compares a claimed digest to a
  computed one; until the mechanism lands, X-1 is an obligation with no
  enforcement, recorded as such rather than implied otherwise.

---

## 3. Where the manifest lives: construct, storage, transport

### 3.1 The composition root's form (decided)

**A plain struct of tables, assembled at link time by the game crate and
validated by one kernel-side function.** Not a trait whose methods return
fragments, not a macro, not inventory registration.

The grounds are all tree-shaped, none aesthetic:

- **Data where data belongs.** The manifest must be readable by persistd
  without linking game code — A5 §6.1's reason 2 ("most rows must be readable
  *without* that", bin/persistd.rs:1261-1263) is what kills a trait-method
  shape: calling into a build to learn a static fact is the exact thing the
  capability registry exists to stop.
- **The registration idiom already accepted by D38(c)** is composition-time
  data: `MigrationRegistry::declare` (migration.rs:53-56) and
  `AdjudicationExecutor::register` (adjudication.rs:350). A struct-of-tables
  manifest composes with them; it does not introduce a second mechanism class.
- **No `dyn`.** `Ruleset`'s associated types already forbid trait-object
  games (`game.rs:171-174`: "a `Vec<Box<dyn Game>>` cannot exist"); tables of
  ids and integers need no dynamic dispatch at all.

Validation at composition time — duplicate `ComponentTypeId`, duplicate module
id, missing declared dependency, dependency cycle, undeclared schedule stage —
fails loudly before any byte exists. The depth of cycle detection (declared
edges only vs transitive closure) is unspecified here; named in §13 as unsure.

### 3.2 Persisted universes record their producing manifest

**Proposed (X-2): a permanent, build-keyed manifest record in the cluster
keyspace, written once when a build registers.** Keyed by `RulesetId`; value
is the manifest's canonical encoding. persistd can write it because
registering a build means linking it (bin/persistd.rs:1261-1263); the record
is tiny and is retained permanently — unlike evidence rows, it does not age
out with adjudication windows, because it is the decoder ring for every older
row. This extends D21's three-retained-builds discipline exactly as A7 §7.2
anticipated: routing stays keyed on `RulesetId`; the manifest supplies what an
id alone cannot answer (schema table for migration-aware tooling,
projection and schedule versions).

Fitting the freeze: a new keyspace family and new types enter through D21's
additive door (I11), the same door D38 clause (c) ruled sufficient. No frozen
signature moves.

### 3.3 Transport: nothing widens the handshake

**No field of this manifest joins `protocol_accepted`, and this document
proposes no wire change.** Exact equality at the bootstrap (I1) is the
narrowest possible admission rule; widening it — or adding a manifest digest
to `VersionedHello` so peers assert deeper equality at session setup — is a
protocol decision, reserved to the owner. What is *proposed* (not decided):
parties that re-execute each other's logs assert manifest equality out of
band, per §4's table. A4 §11 item 3 flagged the same placement question; both
flags land in §10 for the owner to take or refuse together.

---

## 4. Peer compatibility rules

What must match between whom, decided field by field against §2's set. Three
domains have different rules because they answer different questions:

| Relationship | Must match exactly | May differ | Grounds |
|---|---|---|---|
| Any two communicating peers (wire) | `protocol_version` | everything not yet on the wire | Exact equality at the only bootstrap (I1/I2); mutation-pinned M-A8-1. D29 closed the wider window deliberately; this document does not reopen it |
| Evidence producer ↔ adjudicator/witness | `RulesetId` (version **and** digest) | platform within the four-target matrix; operational D16 parameters | Routing is id-keyed and refuses otherwise (I4); retired builds answer `UnknownRuleset`, never a strike (I10). Discrete state is bit-exact across the matrix and continuous state compares under D16 bands (A4 §3.1 ring 2), so *platform* is explicitly not an identity axis |
| Authority ↔ predicting client | the build behind the predicted entities' claims; cluster corrections installable | presentation-tier state freely | Corrections carry `ruleset` because "only the game build named by `ruleset` knows how to install it" (authority.rs:19-21, :32); prediction reconciles against claims whose hash is defined under that build's codec |
| Parties re-exchanging input logs (authority ↔ witness-set peers) | `RulesetId` + `schedule_digest` + `projection_version` (**proposed**) | anything outside WP-1..WP-6's projection surface | A4 specified the digest and left its assertion domain open; the proposal here is the narrowest one that makes re-execution comparable: same topology, same framing, same rules. Wire placement stays the owner's (§3.3) |

Two consequences worth stating plainly:

1. **Module-level compatibility is not negotiated between peers.** Under
   static composition (§8) the module table describes how *one build* was
   assembled; two builds either agree at the granularity above or do not
   interoperate at all. There is no per-module handshake to design, which is
   the main simplification static composition buys this entire section.
2. **Cosmetic divergence is unlimited by design.** Presentation state never
   enters canonical bytes (A5 zeros fail closed; A7 §5.3's exclusion list),
   so two peers may render entirely differently while every claim, journal
   row and verdict they produce agrees bit-for-bit.

---

## 5. Persisted-universe and replay compatibility rules

The at-rest half is mostly built; this section states the rules the manifest
adds on top of it, and what each rule costs if broken.

**R-1 — Rows decode by their own statements, never by the reading build's
assumptions.** Per-slot `(ComponentTypeId, SchemaVersion)` framing, absent ==
v0 bootstrap, floor derived from the bag (I6). Fail-closed at every gap:
undeclared component, future version, missing step (I7; M-A8-2). A build that
cannot decode a row refuses it — it never guesses.

**R-2 — Journal logical records carry their encoding version with the record**
(D38 clause (d)(5)); the physical `RawEnvelope::V1` is the upgrade vehicle,
not the answer. Unchanged by this node; restated because replay compatibility
is one of its consumers.

**R-3 — The manifest record (X-2) is permanent and build-keyed; rows are not.**
A `world/` row records its producing manifest *by reference* (`RulesetId` is
already in every claim and journal-adjacent identity path); stamping manifest
bytes into per-row storage would cost real bytes to answer a question R-1's
slots already answer. The reference resolves through X-2's family, which is
why that family never retires rows.

**R-4 — Replay selects the build by `RulesetId`; retention bounds how far back
that selection reaches.** Adjudication routes to the registered match or says
`UnknownRuleset` (I4/I10). Replays of *history older than retention* do not
fail — they resolve `Unadjudicable`, never a strike (D21 consequences, I12).
This asymmetry is deliberate: version skew is the cluster's gap, and punishing
a reporter for an operator's release cadence would corrupt the strike economy.

**R-5 — Goldens pin rules versions, and version discipline is the migration
story for them.** "Bump `version` whenever the rules change" is already each
game's obligation (skirmish/mod.rs:99-101); a bump regenerates goldens as a
reviewed diff. The manifest changes nothing here — it makes the same
discipline legible to tooling that does not link the game.

**R-6 — `projection_version` gates claim comparability.** Claims commit under
WP-1..WP-6 framing (A7 §5). Two hashes computed under different
`projection_version` values are **not comparable**: comparison refuses rather
than reporting a mass false deviation — IV-2's failure mode, closed by
refusing instead of convicting. Orthogonality means a projection bump forces
no schema migration and vice versa (A7 M-4).

### 5.1 What this section deliberately does not solve

G-3 and F-1 bound what persisted-universe compatibility can claim today: the
bulk uplink states no schema, so diff-overwritten bags reset to v0 floors
until the framed-bag producer lands (I8), and the journal's tick field is an
uplink sequence until the owner disposes of F-1 (I9). Both are recorded as
preconditions in §9.3 rather than assumed away.

---

## 6. Rolling upgrade: the verdict

**There is no general rolling-upgrade story, and the absence is a recorded
decision, not a gap.** The wire's `{V, V−1}` window was closed once, for all
traffic, by D29 clause 5 (I1/I2); exact equality is mutation-pinned (M-A8-1).
Reopening it would mean carrying a second admission branch through every site
a protocol bump touches — the complexity D29 refused because no external
client existed to serve. That refusal stands until the owner revisits it.

What exists instead is three narrow, deliberately bounded continuities:

1. **Cluster evidence continuity during rolling persistd deploys** — the one
   real story. Old builds stay registered for the retention horizon; bundles
   produced before a deploy remain adjudicable after it (I10/I12). This is
   continuity of *judgement*, not interop: at no point do two protocol
   versions share a session.
2. **Client upgrades are synchronized refusals.** An upgraded client against
   an unupgraded gateway fails at `VersionedHello` with a version in the
   refusal — one clean error at handshake, never a mid-session decode failure
   moved to "the first low-population commit in the cell it happens to be
   standing in" (protocol.rs:33-36).
3. **Store-side mixed-version windows fail closed, not lossy.** During a
   rolling deploy, a build reading rows written under schema versions it does
   not declare refuses them (`FutureVersion`, I7) rather than interpreting
   them. The window closes when the deploy completes; no dual-format reading
   exists or is proposed.

**Statement for the record:** if a future operator needs live mixed-version
interop, that is a protocol decision naming D29 clause 5 — proposed through
A11, decided by the owner, not engineered around here.

---

## 7. Module removal and replacement behaviour

### 7.1 Removal with persisted data present

The default is already in the tree and mutation-proven: **rows referencing an
undeclared component refuse to load** (I7; M-A8-2). A5 §7 left one question to
this node: whether an operator override exists. Decisions, proposed:

- **X-3 — the manifest carries an explicit `removed` list**: pairs of
  `(ComponentTypeId, last_schema_version)` for deliberately retired
  components, distinct from "never heard of it". The loader's refusal then
  names the cause precisely ("row holds removed component 7") instead of the
  generic `UnregisteredComponent`. Same fail-closed semantics — this is
  diagnostics, not an escape hatch.
- **X-4 — no silent read-and-drop override; quarantine only as a reviewed,
  opt-in operator tool.** Dropping undeclared slots silently is exactly the
  mutation M-A8-2 killed. A quarantine mode (read the row once, set it aside
  intact for inspection) is defensible for forensics but must never become a
  default path that launders data loss into routine operation. Whether it
  ships at all is the owner's (§10).

### 7.2 Replacement

- **`ComponentTypeId` values are never reused**, the same monotone discipline
  `SchemaVersion` already follows within a type (atrest.rs:22-27). A replaced
  module allocates fresh ids for its replacement components.
- **Evolution keeps the id and bumps the schema**; the migration chain serves
  it (R-1).
- **Cross-id transfer is explicit game code, outside the registry.** Nothing
  today migrates payload across component ids — steps are keyed
  `(component, from_version)` (migration.rs:58-71) — so a module whose data
  must survive into differently-keyed components decodes old bytes itself,
  under its own rules purity obligations (D38 clause (e)), and writes the new
  rows. Named here so nobody expects registry machinery that does not exist;
  whether generic cross-id steps are ever worth building is an owner call if
  a second game ever needs them.

### 7.3 Schema-id governance (the question A5 and A2 handed here)

**Proposed (X-5): `ComponentTypeId` allocation becomes a reviewed, permanent,
per-game registry file** — data in the game repo, allocated monotonically,
never reused, checked at composition time for duplicates (§3.1). Today Regolith
hardcodes `ComponentTypeId(1)` (regolith/mod.rs:79-84) with no registry; X-5
generalizes the discipline the tree already applies to schema versions. It
creates no new mechanism class: a table next to the manifest, validated by the
same composition-time check.

---

## 8. Static versus dynamic composition — decided

**Static. Statically compiled modules, runtime data and configuration only
through declared channels; runtime-loaded code is out of scope.** This
ratifies D21 rather than reopening it: "Link-time composition is the answer
for 1.0 … WASM sandboxing is not adopted, and no dynamic `Ruleset` loading
path is built" (I11), with the brief's own initial recommendation in
agreement (brief:608). The manifest adds one consequence the record did not
have to state:

- **Manifests describe builds, not deployments.** Because composition happens
  at compile time, one build's module table is complete, reviewable in the PR
  that composes it, and covered by X-1's digest. A dynamic scheme would need
  per-deployment manifests whose digest no source hash can cover, a platform
  matrix that has no purchase on dlopen'd blobs (D14's pins bind toolchains,
  not loaded files), and an admission story for capability declarations that
  arrive after the handshake. D21 priced the same costs for WASM and refused
  them; native dynamic loading is the same trade with worse determinism
  properties.

The line this keeps sharp: **code is linked; everything else is data entering
through sealed channels.** Scenario TOML, world-seeding content and D16
operational parameters are all data today and stay legal; none of them may
grow behaviour without becoming linked code through the front door.

D21's own reopen conditions stand unchanged: concrete demand from a title that
must run rules it does not compile, or a hotfix path rolling deploys cannot
meet. Absent those, this is settled — by an accepted record, not by preference.

---

## 9. How the version axes compose

The heart of this node. Seven axes now exist or are proposed; the design rule
is that **each answers exactly one question and moves independently of the
others**. D38 clause (d)(3) pinned the first orthogonality ("neither number is
ever derived from the other"); A7 M-4 added a third axis to that rule; this
document generalizes it to the whole set.

| Axis | Allocated by | Answers | Use domain | Orthogonal to |
|---|---|---|---|---|
| `PROTOCOL_VERSION` (u16) | workspace | can these peers decode each other | handshake exact equality (I1) | every game-side axis |
| `RulesetId.version` (u32) | game | which rules semantics produced evidence | exact-match routing; golden discipline | schema (D38 d(3)); projection (M-4) |
| `RulesetId.digest` (32 B, X-1) | game | *which build* — source closure incl. kernel, schedule, capabilities | build selection wherever ids route | content-derived, never derived from other numbers |
| `(ComponentTypeId, SchemaVersion)` per component | game (X-5) | what shape these payload bytes are | at-rest migration chain | `RulesetId`, both directions (D38 d(3)) |
| `projection_version` (A7 WP-6; value 1 today) | kernel/game agreement | how witness bytes were framed | claim comparability (R-6) | schema and rules version (A7 M-4) |
| `schedule_digest` (A4 §3.10) | composition root | what execution topology ran | re-execution comparability between log-exchanging parties (§4, proposed) | pins topology only — A4's own caveat: not the semantics bevy_ecs attaches to it; that stays with D14 pins + upgrade conformance |
| `profile_id` (§2) | workspace | which determinism envelope this build claims | informational today; guards future second profiles | everything |

### 9.1 The composition law

**A build's full identity is the tuple
`(PROTOCOL_VERSION, RulesetId, manifest_format_version, manifest_digest)`,
and no member of the tuple is ever computed from another member.** The
manifest digest covers the manifest's tables with the digest field itself
excluded — no circularity. Co-movements are recorded, never automatic:
adding a component typically bumps the schema table, may bump
`RulesetId.version`, and may change the schedule digest — but each bump
states its own reason in its own review, and nothing derives one from
another. That is D38(d)(3)'s sentence extended from two axes to seven.

### 9.2 WP-3's slot shape: kept

A7 §12 item 4 made this decision explicitly conditional on A8: **the framed
slot stays `(ComponentTypeId, SchemaVersion, payload)`.** The rule's substance
is "witness framing ≡ persistence framing" (WP-3), the at-rest bag already has
exactly this shape (schema.rs:13-58), and reshaping it would ripple through
WP-3, the canonical projection, and every durable row for zero consumer
benefit. The canonical projection is untouched by this document.

### 9.3 What the manifest refuses to assume (the handed constraints)

- **G-3**: `DiffUplink` carries no schema statement (I8), so the manifest's
  schema table describes composition-time truth while diff-overwritten durable
  floors reset to v0 until the framed-bag producer lands. That producer is a
  precondition package owned by the implementation epic (A5 G-3, A7 §4.3);
  nothing here pretends the uplink already names schemas.
- **F-1**: the journal's tick field is an uplink sequence in practice (I9), so
  no manifest field assumes journal/claim tick alignment. If the owner
  disposes of F-1 by fixing the writer, alignment becomes *possible*; this
  manifest neither requires nor precludes it.
- **A5's capability declarations** need `(ComponentTypeId, SchemaVersion)` —
  IV-8 makes its absence invalid — which is exactly what the schema table
  keys on. One namespace, one governance rule (X-5).

---

## 10. Proposals that need the owner (ADR gate)

All of §2–§9 is proposal; these are the items that cannot land without an
explicit owner decision, carried to A11:

1. **The compatibility-manifest record itself** — new normative surface
   extending D21 (distribution story gains a manifest family) and D38 (a
   seventh-axis orthogonality statement). The natural vehicle: one new ADR,
   drafted by A11, amending neither record's accepted text.
2. **X-1's mechanism** — how digests get computed and verified. Obligation and
   scope decided here; timing and machinery are implementation planning.
3. **Schedule-digest session assertion** — whether log-exchanging parties
   assert `schedule_digest` (+ `projection_version`) at session setup. Wire-
   adjacent; A4 flagged it first; both flags should be resolved together.
4. **Rolling-window reopening** — recommended against (§6); the door is D29
   clause 5's, and only the owner may walk through it.
5. **X-4's quarantine override** — whether a read-and-quarantine operator tool
   ships at v1 or waits for forensics demand.
6. **F-1's disposition** — already the owner's from A7 §9.5; restated because
   §5.1 depends on either outcome.

---

## 11. Mutation log (break stage → named check dies → revert → passes)

Baselines were recorded before each mutation; failing runs produced real
result lines; every revert re-ran its check and passed; no mutation landed on
both sides of an equality.

| # | Guarded stage broken | Named check | Observed | Reverted |
|---|---|---|---|---|
| M-A8-1 | `protocol_accepted` widened to admit `current − 1` (`offered == current \|\| offered + 1 == current`) — the exact "widening" any rolling-window manifest would need | `cargo test -p orrery_protocol --lib` | `gateway::tests::the_accepted_version_window_is_closed_to_exactly_this_version` FAILED at gateway.rs:1017; `125 passed; 1 failed` | `126 passed; 0 failed`; tree clean |
| M-A8-2 | `MigrationRegistry::migrate_bag` made to silently skip undeclared components (`continue` instead of `UnregisteredComponent`) — the removal-time failure X-3/X-4 refuse | `cargo test -p orrery_persistd --lib migration` | `migration::tests::missing_registration_refuses_stale_checkpoint` FAILED; `5 passed; 1 failed` | `6 passed; 0 failed`; tree clean |
| M-A8-3 | `AdjudicationExecutor::register`'s eviction loop deleted — builds accumulate past `RETAINED_BUILDS`, retired builds stay adjudicable forever | `cargo test -p orrery_persistd --lib adjudication` | two named failures: `only_three_builds_stay_adjudicable`, `a_report_for_a_retired_build_is_undecidable_not_a_strike`; `11 passed; 2 failed` | `13 passed; 0 failed`; tree clean |

Predecessor mutations relied on as recorded (re-based: this branch adds only
this document): A1 M1–M8, A2 M-A/M-B/M-A′, A3 F-1/F-2 + P1–P5, A4 M-G1 +
E-D/E/P probes, A5 X1–X5, A6 M-A6-1..4b, A7 X-A..X-E.

---

## 12. Stale citations found while verifying

| Record | Citation / claim | Current truth |
|---|---|---|
| This node's issue text | "`protocol_accepted` … is **exact equality** (`offered == current`) — verify the current line" | Verified true at gateway.rs:182-184, and stronger than the brief knew: the doc comment there records D29 clause 5's deliberate window closure, which turns the rolling-upgrade acceptance item from an open question into a recorded decision (§6) |
| A5 §7 G-3 | "the bulk uplink makes no schema statement" (`feed.rs:82-97` drops `fns_id`; `gateway.rs:382-383`) | Verified line-fresh: payload doc at gateway.rs:383; `feed_uplink` drops `FnsId` implicitly by never reading it; actor apology now spans :1299-1309 with `schema_floor = SCHEMA_V0` at :1309 — one-line drift from A5's `:1300-1308`, claim unchanged |
| A7 §1.1 F-1 | `DiffUplick.tick` documented as universe tick; writer stamps client-local seq | Verified true as recorded (gateway.rs:377-378; feed.rs:81-92). Typo in A7's own text ("DiffUplick") noted in passing; finding unaffected |
| Predecessors' shared citations re-opened where §1 leans on them: `ruleset.rs:3-6` same-build quote; D21 freeze + consequences; D38 clause (d)(3) and (c); seed manifest stamp/digest idioms; skirmish digest-honesty comment | — | All held at this tree's state |
| New finding, not stale anywhere: **the ruleset digest is a placeholder** (I5) | Regolith `[0x63; 32]`, Skirmish `[0x5C; 32]`, Skirmish's comment admits nothing computes one | Recorded here because every predecessor cited `RulesetId {version, digest}` without noting the second field is currently decorative. X-1 exists because of it |

---

## 13. Unsure

Stated as unsure rather than smoothed over:

1. **Cycle-detection depth** in composition-time validation (§3.1): declared
   edges only, or transitive closure? The check's existence is specified; its
   depth is not, because no module system exists to fail against yet.
2. **X-1's digest mechanism is unpriced.** Build script vs CI artifact vs lazy
   runtime hash have different failure modes (stale artifacts are worse than
   honest placeholders); choosing one without costing them would repeat the
   fabricated-looking-digest mistake X-1 replaces.
3. **Whether `game_id` ever needs a wire consumer.** No current message needs
   it; a multi-game cluster or cross-universe tooling might. Cheap to add
   later through the additive door; not added now.
4. **Whether the `removed` list (X-3) should also record *why* and *when*.**
   Useful for forensics; costs manifest churn on every retirement; decided
   minimal here, revisitable without breaking anything.
5. **Module dependency/incompatibility semantics** (brief:174 lists them as
   registrable): the manifest table has slots for them, but what an
   "incompatibility" *does* at composition time — refuse the build, or warn —
   is undefined until a second module exists to disagree with.

Deliberately not done:

- **No implementation**: no registry, no keyspace family, no digest code, no
  trait or struct edits. The only file this branch adds is this document.
- **No ADR text**: §2–§9 are proposals for A11 to carry; acceptance is the
  owner's alone.
- **No decision owned elsewhere**: rollback unit, projection framing, command/
  event semantics, gate design, harness construction — consumed as recorded,
  never re-decided.
