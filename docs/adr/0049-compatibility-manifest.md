# ADR-0049: The compatibility manifest — the field set, its storage, and the seven-axis composition law

**Status:** Accepted · **Date:** 2026-08-25 · **Decision:** D49

This record is normative. See the [ADR index](../DECISIONS.md) for precedence,
scope, and the complete accepted decision set.

Accepted by the owner on 2026-08-25 through the #395 planning tree (proposal
R8, [a11-adrs-and-pr-plan.md](../plans/a11-adrs-and-pr-plan.md) §2), the last
of the eight records that tree produced. One sub-question of clause (b) —
the digest-computation mechanism — is explicitly reserved to the owner
(Open questions, item 1), in exactly the manner [D45] reserved IV-7's
enforcement mechanism while accepting IV-7's rule.

**Supersedes:** nothing, and — deliberately — **amends nothing.** This record
enters [D21] through the additive door that record itself holds open ("Additive
change — new methods, new types, new default-carrying config fields — is not
breaking and needs no record", docs/adr/0021:61-64): a new keyspace family and
new types, no frozen signature moved. It **reaffirms** [D29] clause 5 without
reopening it (clause (h)) and **ratifies** [D21]'s static-composition ruling
without relitigating it (clause (i)). Its relationship to [D38]'s
version-domain law is stated in clause (g) and is *generalization in this
record's own text*, not amendment of that record's. Within the #395 set, R7
([D48]) is the only proposal that amends an accepted record; this record is
deliberately not the second. A8's own framing of the
vehicle is kept verbatim: "one new ADR … amending neither record's accepted
text" ([a8-compatibility-manifests.md](../plans/a8-compatibility-manifests.md)
§10 item 1).

Out of scope, each with its owner: everything [D42]–D48 decide — the canonical
topology and host seam ([D42]); the determinism envelope, stages, and the
schedule digest's **content** ([D43] clause (g): "pinned into the game manifest, whose
format and storage are R8's" — this record takes the storage and only the
storage); capability dimensions and their invalid combinations ([D45]);
message classes and C-2's **semantics** ([D46] clause (e) — its constants' storage
lands here, clause (e)(3), their meaning does not); the rollback unit ([D47]);
identity classes and allocation (D44, in flight at acceptance — cited by
decision id, not by file); the witness projection and `projection_version`'s
**content and bump rule** ([D48] clause (f) — this record stores the axis, clause (c),
and defines none of it). Any manifest field that would *widen* peer admission is a
protocol decision and therefore the owner's, never this record's:
`GatewayMsg::protocol_accepted` is exact equality (`offered == current`,
`crates/orrery_protocol/src/gateway.rs:182-184`, verified and mutation-pinned,
Verification M1). Nothing here schedules work inside the P4 digest before P4
exit: the pipeline digest covers `crates/orrery_witness`, `crates/orrery_core`,
`crates/orrery_games` and `gates/p1-swarm` (`scripts/p4-ledger.sh:409-414`,
verified on this tree), and this record orders no code change at all. Its
substance is [A8](../plans/a8-compatibility-manifests.md) §2–§9, carried into
a record; every citation was re-opened on this tree at acceptance time and
every enforcement claim re-proven by mutation (Verification appendix).

## Context

### 1. The digest everything routes on is a placeholder constant — verified, not feared

`RulesetId { version: u32, digest: [u8; 32] }`
(`crates/orrery_protocol/src/verifiable.rs:59-64`) is pinned into frames,
claims, bundles and strike rows (verifiable.rs:170, :203, :213, :292) and
into cluster-authored corrections ("only the game build named by `ruleset`
knows how to install it", `crates/orrery_protocol/src/authority.rs:14-16`,
field at :32). Adjudication routes a bundle to the build whose id matches and
answers `UnknownRuleset` when none does
(`crates/orrery_persistd/src/adjudication.rs:388-400`).

And the digest half of that id is a constant. Regolith ships `[0x63; 32]`
(`crates/orrery_games/src/regolith/mod.rs:74-77`); Skirmish ships
`[0x5C; 32]` under a comment that says it plainly: "The digest is a
placeholder pattern rather than a real build hash: nothing in the tree
computes one yet, and a fabricated-looking constant is more honest than a
plausible-looking one" (`crates/orrery_games/src/skirmish/mod.rs:93-103`).
Both were opened and read at acceptance. Nothing anywhere in the tree
computes a digest from source: every other `RulesetId` construction is a test
literal. The digest is copied into the verifiable frame and compared, never
derived — so **any compatibility scheme leaning on the digest today is
leaning on a constant**, and every clause below that touches the digest is
written with that fact in front of it, not behind it.

### 2. The wire is exact-equality, and the window was closed on purpose

`protocol_accepted` is `offered == current`, and its doc comment records why:
[D29] clause 5 closed the former `{V, V−1}` rolling-upgrade window "once, for
all traffic" (gateway.rs:164-184; docs/adr/0029:369-378). The constant is
`PROTOCOL_VERSION: u16 = 5` (`crates/orrery_protocol/src/protocol.rs:68`),
`VersionedHello` is the only live bootstrap (protocol.rs:30-38), and the
closure is pinned by a named test
(`the_accepted_version_window_is_closed_to_exactly_this_version`,
gateway.rs:1010-1022) whose liveness this record re-proved (Verification M1).
This turns one of the questions handed to this record — the rolling-upgrade
story — from an open design problem into a recorded decision to reaffirm
(clause (h)).

### 3. The machinery a manifest composes with already ships, and it fails closed

Per-slot `(ComponentTypeId, SchemaVersion)` framing with an absent-means-v0
bootstrap rule is live ([D38] clause (d); `crates/orrery_protocol/src/atrest.rs:12-27`,
`SCHEMA_V0` at :82; `crates/orrery_persistd/src/schema.rs:13-30`). The
migration path refuses an undeclared component, a future version, and a
missing step (`crates/orrery_persistd/src/migration.rs:74-101`; liveness
re-proven, Verification M2). `RETAINED_BUILDS = 3` bounds adjudicable builds
and eviction is real (adjudication.rs:33, :357-359; Verification M3). The
repository even operates a manifest idiom already — the seeder's content
manifest, with a toolchain stamp and a rolling blake3 digest
(`crates/orrery_seed/src/manifest.rs:52-60`, :99-113; docs/12 §9.3) — which
establishes house style for stamps and digests while answering a different
question (what content was seeded, not what build is compatible). And there
is no kernel version to record: every workspace crate is `0.1.0`
(crates/*/Cargo.toml, checked for core, protocol, games), and the toolchain is
pinned at channel `1.96.0` (`rust-toolchain.toml:2`).

### 4. Two live findings this record inherits and refuses to assume away

A8 refused to write compatibility rules that quietly presume these are fixed,
and this record inherits the refusal:

- **G-3 (A5).** The bulk uplink makes no schema statement: `DiffUplink`
  carries a bare postcard payload with no `ComponentTypeId` and no
  `SchemaVersion` (gateway.rs:371-393, payload doc at :382-383), and the cell
  actor consequently resets a diff-overwritten bag's floor to `SCHEMA_V0`
  under a comment naming the framed-bag producer as the fix
  (`crates/orrery_persistd/src/actor.rs:1299-1309`). Verified line-fresh.
- **F-1 (A7).** `DiffUplink.tick` is documented "The universe tick at append
  (D8)" (gateway.rs:377-378), but the only production writer stamps a
  client-local per-entity sequence starting at 0
  (`crates/orrery_persist_client/src/feed.rs:81-95`: `seq.next.entry(entity)
  .or_insert(0)`, then `Tick::new(tick)` from that counter). Today's bulk
  journal cannot be tick-aligned with claim windows. Verified line-fresh.

Both remain open dispositions for the owner (A7 §9.5; A8 §9.3). Clause (f)
states what they subtract from the rules below.

## Decision

### (a) The manifest field set — every field carries its verdict, and the rejections carry their reasons

Each game build assembles one **compatibility manifest**: a plain struct of
tables built at link time by the game crate and validated by one kernel-side
function (duplicate `ComponentTypeId`, duplicate module id, missing declared
dependency, dependency cycle, undeclared schedule stage — loud failure before
any byte exists). Not a trait whose methods return fragments: the manifest
must be readable without linking game code (registering a build means linking
a `Ruleset`, `crates/orrery_persistd/src/bin/persistd.rs:1258-1263`, and most
consumers must not have to), and composition-time data is the registration
idiom [D38] clause (c) already ruled additive.

The brief's candidate fields, verdict by verdict. Declining an axis is a
decision with teeth — an unconsumed version number is the `classify_component`
mistake ([D45] Context §1) in manifest form — so the rejections are the
normative center of this clause:

| Field | Verdict | Ruling |
|---|---|---|
| `game_id` | **keep** | Stable string naming the rules family. `RulesetId` is only `{version, digest}`, so two games at version 8 are indistinguishable to id-only tooling. Manifest-only; no wire consumer proposed |
| `manifest_format_version` | **add** | u32, absent == 0 by the at-rest bootstrap rule (atrest.rs:14-21 applied to this format), bumped on any change to the manifest's own framing. [D38] clause (d)(1)'s lesson applied to the format that carries the others |
| `protocol_version` | **keep** | u16, exact-equality domain (Context §2). The manifest *records* it; nothing here widens it |
| `kernel_version` | **reshape → advisory toolchain stamp** | A numeric kernel version would be meaningless: every crate is `0.1.0` (Context §3), and peers never link a kernel separately from a build — "the same build links into peers, field hosts and `persistd`" (`crates/orrery_core/src/ruleset.rs:3-4`) — so kernel identity collapses into the build digest once clause (b) is real. What survives is the seed manifest's existing idiom (rustc channel + target, manifest.rs:52-60): advisory, never an admission axis |
| `RulesetId {version, digest}` | **keep struct; add clause (b)'s obligation** | The struct is frozen into every evidence path (Context §1). What changes is what the digest *means* |
| `module_id → module_version` | **keep, future-facing** | Defined now so the composition root lands against a fixed shape; no module system exists yet, and under clause (i) modules are statically linked registrations. Versions monotone, never reused or gapped, mirroring `SchemaVersion` discipline |
| `component_schema_id → schema_version` + capabilities | **keep and extend** | The table of `(ComponentTypeId, SchemaVersion)` pairs, each row carrying [D45]'s five dimensions P/R/W/N/A. One table answers the schema question and the witnessed-contents question: witnessed schemas are the rows with `W ≥ 1`; exclusions are derivable from the zeros, not enumerated |
| `command_schema_id → schema_version` | **reject** | **Commands have no independent encoding to version — the axis would have no consumer.** External commands are logged inputs sealed into signed logs; internal commands are mechanically collapsed onto domain events ([D46] clause (a)); durable consequences ride intent ops whose ids are kernel-reserved or game-opaque (`crates/orrery_persistd/src/intent/mod.rs:204-210`). Every command shape changes with the build — that is, under `RulesetId` — and a second number here would be a version nothing reads |
| schedule topology | **keep as [D43] clause (g) wrote it** | Carried verbatim as `schedule_digest` — blake3 over ordered stages, per-stage ordered system names, sorted edges, ambiguity setting, executor policy. **Content is [D43]'s; this record adds only storage (clause (c)) and equality domain (clause (f))**; wire placement stays reserved (Open questions, item 2) |
| canonical-configuration hash | **reject, with the architecture's own argument** | There is nothing to hash that is not already code. VC-8 bans ambient reads in gated crates — "the environment reaches a rule only as a logged input" (`scripts/core-gates.sh:100-105`) — so outcome-affecting configuration is *code by construction*, inside clause (b)'s digest. [D16] operational parameters (cadences, budgets) are deliberately non-canonical and must stay out: hashing them would assert an outcome dependency the architecture denies. A runtime-config seam, if ever wanted, needs its own determinism story and its own ADR |
| determinism profile | **reshape → `profile_id`** | Exactly one profile exists — the [D9] envelope with [D43]'s three rings — so a free-form hash would have nothing to distinguish. An identifier whose single legal value names that envelope, so a future second profile cannot silently claim compatibility with builds it cannot replay |

### (b) X-1 — the digest's scope is decided; its mechanism deliberately is not

**`RulesetId.digest` becomes blake3 over the determinism-relevant source
closure of the build**: the game crate(s) plus every first-party kernel crate
they transitively depend on, hashed over source content at the pinned
toolchain, with the enumeration of contributing crates itself part of the
hashed input. That is the whole of what is accepted. **The computation
mechanism — build script, CI artifact, or lazy runtime hash — is not chosen**
(Open questions, item 1), in the same posture [D45] took for IV-7: rule
accepted, mechanism reserved, and the record must not be read as claiming a
mechanism exists.

Three boundaries, so X-1 is not oversold:

1. **Today the digest is a constant** (Context §1), and until the mechanism
   lands it stays one. X-1 is an obligation with no enforcement; no check
   compares a claimed digest to a computed one, and this record says so
   rather than implying otherwise.
2. **Routing and compatibility, not authenticity.** The tamper model keeps
   the honest id on purpose — "a tampered build keeps the honest `RulesetId`
   — which is the whole point. A cheater claims to be running the rules; the
   claim is what the witness holds it to"
   (`crates/orrery_games/src/game.rs:30-36`). X-1 makes the id mean *this
   build*, not *a trustworthy build*; witnessing convicts, hashing never
   does.
3. **Nothing outside the closure enters.** [D16] parameters, presentation
   assets, and operator configuration are outside by the same argument that
   rejected the config hash in clause (a).

### (c) X-2 — a permanent, build-keyed manifest record in the cluster keyspace, and the three storage obligations inherited from siblings

A manifest record is written **once, when a build registers**, keyed by
`RulesetId`, value the manifest's canonical encoding. persistd can write it
because registering a build means linking it (bin/persistd.rs:1258-1263). The
record is permanent: unlike evidence rows it never ages out with the
`RETAINED_BUILDS = 3` horizon (adjudication.rs:33), because it is the decoder
ring for every older row. Routing stays id-keyed exactly as today; the record
answers what an id alone cannot (the schema table for tooling that does not
link the game, and the axes below). A new keyspace family and new types enter
through [D21]'s additive door — the same door [D38] clause (c) ruled sufficient — and
no frozen signature moves.

Three fields of this record are **storage-only inheritances**, semantics
owned elsewhere and not redefined here:

1. **`schedule_digest`** — content, computation, and CI assertion are
   [D43] clause (g)'s; this record stores the value in the manifest and gives it an
   equality domain in clause (f).
2. **C-2's constants** — `MAX_EVENTS_PER_STEP` and companions are [D46] clause (e)'s
   law, owner-tunable per a11 OD-28; [D46] clause (e)(5) assigns their storage "to
   R8's manifest/registry work", and they live in the clause (e) registry
   file beside the schema table. Changing one is a rules change and rides
   the same versioning discipline; the number's meaning stays [D46]'s.
3. **`projection_version`** — the axis is [D48] clause (f)'s: value 1 today, bumped
   only on WP-2/WP-3 framing change, never for a payload-schema or rules
   change, and "carried nowhere in the tree today" — [D48] clause (f)'s own words,
   which also assign this slot: "stored in the manifest beside `RulesetId`
   and the schedule digest — the manifest construct, storage and governance
   are R8's". This record supplies exactly that: the manifest slot and the
   comparability rule (clause (f) R-3), no semantics.

### (d) X-3 / X-4 — removal is diagnosed precisely and never silently absorbed

The default already ships and is mutation-proven: rows referencing an
undeclared component refuse to load (migration.rs:80-85; Verification M2).
On top of it:

- **X-3 — the manifest carries an explicit `removed` list**: pairs of
  `(ComponentTypeId, last_schema_version)` for deliberately retired
  components, distinct from "never heard of it", so the loader's refusal
  names the cause ("row holds removed component 7") instead of the generic
  `UnregisteredComponent`. Same fail-closed semantics — diagnostics, not an
  escape hatch.
- **X-4 — no silent read-and-drop, ever.** Dropping undeclared slots
  silently is exactly the mutation M2 kills; no override that launders data
  loss into routine operation exists or may be added. A quarantine mode
  (read once, set aside intact for forensics) is admissible only as a
  reviewed, opt-in operator tool, and whether it ships at all is the
  owner's (Open questions, item 3).

Replacement discipline: `ComponentTypeId` values are **never reused**;
evolution keeps the id and bumps the schema through the migration chain;
cross-id transfer is explicit game code under rules-purity obligations
([D38] clause (e)) — migration steps are keyed `(component, from_version)`
(migration.rs:58-71) and no registry machinery for cross-id transfer exists
or is promised.

### (e) X-5 — a reviewed, permanent, per-game `ComponentTypeId` registry file

`ComponentTypeId` allocation becomes **data in the game repo**: a reviewed
registry file per game, allocated monotonically, never reused, checked for
duplicates by the same composition-time validation as clause (a). Today
Regolith and Skirmish each hardcode `ComponentTypeId(1)` with no registry
(regolith/mod.rs:80-84; skirmish/mod.rs:105-110) — legal only while each game
is its own universe; the registry generalizes the discipline the tree already
applies to schema versions. [D45]'s capability declarations key on
`(ComponentTypeId, SchemaVersion)` and IV-8 makes an absent pair invalid, so
the capability table and this registry share one namespace and one governance
rule. C-2's constants ride in this file (clause (c)(2)).

### (f) Peer and persisted-universe compatibility rules

What must match, between whom — each relationship answers a different
question, so each gets its own rule rather than one blunt equality:

| Relationship | Must match exactly | May differ | Grounds |
|---|---|---|---|
| Any two communicating peers | `protocol_version` | everything not on the wire | Exact equality at the only bootstrap (Context §2, M1). This record does not reopen D29's window |
| Evidence producer ↔ adjudicator/witness | `RulesetId`, version **and** digest | platform within the four-target matrix; [D16] operational parameters | Routing is id-keyed and refuses otherwise (adjudication.rs:388-400); discrete state is bit-exact across the matrix and continuous state compares under bands ([D43] clause (a)), so platform is deliberately not an identity axis |
| Authority ↔ predicting client | the build behind the predicted entities' claims; corrections installable | presentation state freely | Corrections carry `ruleset` because only that build can install them (authority.rs:14-16, :32) |
| Parties re-executing each other's logs | `RulesetId` + `schedule_digest` + `projection_version` | anything outside the projection surface | The narrowest set that makes re-execution comparable: same rules, same topology, same framing. Where the assertion happens on the wire is reserved (Open questions, item 2) |

And for persisted rows and replay:

- **R-1 — rows decode by their own statements**, never by the reading
  build's assumptions: per-slot framing, absent == v0, fail-closed at every
  gap (Context §3; M2). A build that cannot decode a row refuses it; it
  never guesses.
- **R-2 — the manifest is referenced, not stamped.** A row's producing
  manifest resolves through clause (c)'s permanent family via the
  `RulesetId` already present on every evidence path; per-row manifest bytes
  would cost real storage to answer what R-1's slots already answer.
- **R-3 — `projection_version` gates claim comparability.** Hashes computed
  under different values are not comparable, and comparison **refuses**
  rather than reporting a mass false deviation. Refusal, not conviction, is
  the failure mode — the same asymmetry the retention path already
  exhibits, where history older than three builds resolves `Unadjudicable`,
  never a strike (adjudication.rs:388-400, :540-543; [D21] Consequences,
  docs/adr/0021:93-97).
- **R-4 — what these rules do not cover yet, by Context §4:** the bulk
  uplink states no schema (G-3), so the manifest's schema table describes
  composition-time truth while diff-overwritten durable floors reset to v0
  until the framed-bag producer lands; and the journal's tick field is an
  uplink sequence (F-1), so no rule above assumes journal/claim tick
  alignment. Both dispositions stay the owner's; nothing here pretends
  either is closed.

### (g) The seven-axis composition law

Seven version axes now exist or are accepted, and the design law is:
**each axis answers exactly one question, and no axis is ever derived from
another.**

| Axis | Allocated by | Answers |
|---|---|---|
| `PROTOCOL_VERSION` (u16) | workspace | can these peers decode each other |
| `RulesetId.version` (u32) | game | which rules semantics produced this evidence |
| `RulesetId.digest` (32 B, clause (b)) | content-derived | which exact build |
| `(ComponentTypeId, SchemaVersion)` per component | game, via clause (e) | what shape these payload bytes are |
| `projection_version` ([D48] clause (f); value 1 today) | per [D48] clause (f)'s bump rule | how witness bytes were framed |
| `schedule_digest` ([D43] clause (g)) | composition root | what execution topology ran |
| `profile_id` (clause (a)) | workspace | which determinism envelope this build claims |

A build's full identity is the tuple
`(PROTOCOL_VERSION, RulesetId, manifest_format_version, manifest_digest)`,
where `manifest_digest` covers the manifest's tables with the digest field
itself excluded — no circularity. Co-movements are **recorded, never
automatic**: adding a component typically bumps the schema table, may bump
`RulesetId.version`, and may change the schedule digest — but each bump
states its own reason in its own review, and none is computed from another.

Relationship to [D38], as that record stands on this tree: clause (d)(3)
originally pinned the orthogonality for two axes, and [D48] clause (g) — the one
amendment in the #395 set — has already widened it to three: on this branch
(d)(3) now closes "The bag's version fields, the build's digest, and the
projection version answer different questions and none of the three may ever
be derived from another" (docs/adr/0038:198-215, closing sentence at
:212-215; the two-axis restatement survives at atrest.rs:23-26 as "neither
number is ever derived from the other"). **This record does not edit D38's
text.** It generalizes [D48]'s widened form of the same sentence from three
axes to seven in its own normative text, above — the same relationship to
(d)(3) that [D48] had, minus the amendment, because a law stated over a
superset needs no edit to the subset's record.

### (h) Rolling upgrade — there is no general story, and the absence is reaffirmed, not left open

[D29] clause 5 closed the `{V, V−1}` window once, for all traffic
(docs/adr/0029:369-378), exact equality is mutation-pinned (M1), and **this
record reaffirms that closure without reopening it**. No manifest field
creates a mixed-version admission path. What exists instead is three narrow,
bounded continuities, each verified on this tree:

1. **Cluster evidence continuity during rolling persistd deploys** — the one
   real story. Old builds stay registered for the three-build retention
   horizon; evidence for a retired build resolves `Unadjudicable
   (UnknownRuleset)`, never a strike (adjudication.rs:33, :357-359,
   :388-400; [D21] Consequences, docs/adr/0021:93-97; eviction liveness M3).
   Continuity of *judgement*, not interop: two protocol versions never share
   a session.
2. **Client upgrades are synchronized refusals.** A version-skewed bootstrap
   fails once, cleanly, at `VersionedHello`: `HelloRefused` carries the
   gateway's own `protocol` "so the client can report the skew rather than
   only the failure" (gateway.rs:280-289, reason code at :341-349) — never a
   mid-session decode failure.
3. **Mixed-version store windows fail closed, not lossy.** A build reading
   rows written under schema versions it does not declare refuses them
   (`FutureVersion`, migration.rs:86-92; M2) rather than interpreting them;
   the window closes when the deploy completes, and no dual-format read path
   exists or is proposed.

If a future operator needs live mixed-version interop, that is a protocol
decision naming [D29] clause 5 — the owner's door, not this record's.

### (i) Static composition, ratified — manifests describe builds, not deployments

[D21] is accepted law: "Link-time composition is the answer for 1.0 … WASM
sandboxing is not adopted, and no dynamic `Ruleset` loading path is built"
(docs/adr/0021:40-42). This record ratifies it and adds the one consequence
the manifest makes visible: **a manifest describes a build, never a
deployment.** Because composition happens at compile time, one build's module
table is complete, reviewable in the PR that composes it, and covered by
clause (b)'s digest. A dynamic scheme would need per-deployment manifests no
source hash can cover, a platform matrix with no purchase on loaded blobs
([D14] pins toolchains, not files), and an admission story for capability
declarations arriving *after* the handshake — none of which has a digest
story, a matrix purchase, or a post-handshake admission story, which is the
manifest-shaped restatement of why [D21] refused the same costs. The line
kept sharp: code is linked; everything else is data entering through sealed
channels, and no data channel may grow behaviour without becoming linked code
through the front door. [D21]'s reopen conditions stand unchanged.

## Consequences

- **What this record actually adds is smaller than its title**, in the house
  manner of [D42]'s consequences. Clause (f)'s wire row, R-1, and much of
  clause (h) ratify what already ships and fails closed today (Context §§2–3);
  clause (i) ratifies an accepted record; the rejections in clause (a) build
  nothing at all. The genuinely new surface is: the manifest struct and its
  validation, clause (c)'s keyspace family, clause (e)'s registry file, X-3's
  `removed` list — and clause (b)'s obligation, which is new *meaning* for an
  existing field.
- **Until X-1's mechanism lands, the digest axis of clause (g) is written in
  invisible ink.** Two builds could still ship identical `{version, digest}`
  pairs and route to whichever registered first. This record makes that a
  named, bounded debt rather than an unnoticed assumption; no clause above
  behaves *worse* than today while the debt stands, and clause (f)'s digest
  rows become fully meaningful only when it is paid.
- **Three sibling records gain a home without gaining a landlord**: [D43]'s
  schedule digest, [D46]'s C-2 constants, and [D48]'s `projection_version` get
  storage here with semantics untouched there. A reader asking "what does
  this field mean" is always sent to the owning record.
- **The strike economy is protected by refusal asymmetry** (clause (f) R-3,
  clause (h)(1)): version skew resolves as refusal or `Unadjudicable`, never
  as deviation or strike, so an operator's release cadence can never convict
  a reporter.
- **G-3 and F-1 stay visible.** Both are restated as open bounds (clause (f)
  R-4) rather than absorbed; any future claim that "the manifest covers the
  bulk journal" is false until the framed-bag producer lands and F-1 is
  disposed.

## Alternatives considered

- **A `command_schema_id` axis.** Rejected in clause (a): every command shape
  rides the build. The strongest form of the argument is that the axis has
  *no possible consumer* — nothing decodes a command except the build that
  defines it, under an id already carried on every evidence path.
- **A canonical-config hash.** Rejected in clause (a): VC-8 makes
  outcome-affecting config code by construction, and [D16] parameters are
  deliberately non-canonical. Hashing config would assert a dependency the
  architecture denies — and would drift toward making operational tuning an
  identity axis, which clause (f) explicitly refuses ("[D16] operational
  parameters" in the may-differ column).
- **A numeric `kernel_version`.** Reshaped, not kept: with every crate at
  `0.1.0` and no separately-linked kernel, the number would be either
  invented or constant — both worse than an advisory stamp plus a real
  digest.
- **Stamping manifest bytes into rows.** Rejected by clause (f) R-2:
  reference through the permanent family costs one key; per-row bytes cost
  storage forever to answer a question the slots already answer.
- **Reopening the `{V, V−1}` window via manifest negotiation.** Refused by
  clause (h). Mutation M1 is the demonstration: the exact widening any such
  scheme needs is the mutation the named test kills.
- **A trait-shaped manifest** (methods returning fragments). Rejected in
  clause (a)'s construct ruling: the manifest must be readable without
  calling into the build, and `Ruleset`'s associated types already forbid
  trait-object registries ("a `Vec<Box<dyn Game>>` cannot exist",
  `crates/orrery_games/src/game.rs:170-173`).
- **Dynamic composition with per-deployment manifests.** Rejected in
  clause (i), on [D21]'s grounds plus the three manifest-specific gaps: no
  digest story, no matrix purchase, no post-handshake admission story.

## Open questions reserved to the owner

1. **X-1's mechanism.** Build script, CI artifact, or lazy runtime hash —
   priced differently: a build script computes at the honest point but
   couples every build to the hasher; a CI artifact risks staleness (a stale
   artifact is *worse* than today's honest placeholder — it is a
   plausible-looking lie, the exact thing skirmish/mod.rs:93-99 refuses); a
   lazy runtime hash needs source present at runtime. The scope in
   clause (b) is fixed; the choice is not made here, and until it is, the
   digest remains the placeholder Context §1 describes.
2. **Schedule-digest (and `projection_version`) session assertion** — whether
   log-exchanging parties assert them at session setup. Wire-adjacent;
   [D43]'s Open questions item 2 flagged the same door; both flags should be
   taken or refused together.
3. **X-4's quarantine tool** — ships at v1, or waits for forensics demand.
4. **The rolling-window door** — [D29] clause 5's, reaffirmed shut by
   clause (h); only the owner may walk through it.
5. **F-1's disposition** — already the owner's from A7 §9.5; restated because
   clause (f) R-4 depends on either outcome.
6. **Cycle-detection depth** in composition-time validation (declared edges
   only vs transitive closure) — unspecifiable until a module system exists
   to fail against.
7. **Whether X-3's `removed` list also records why and when** — decided
   minimal here; revisitable additively.

## Verification appendix — what was re-run at acceptance

All on this tree (branch `docs/adr-0049-r8`), 2026-08-25. Citations opened
and read at acceptance: `regolith/mod.rs:74-77` and `:80-84`;
`skirmish/mod.rs:93-103` and `:105-110`; `verifiable.rs:59-64`, `:170`,
`:203`, `:213`, `:292`; `authority.rs:14-16`, `:32`; `gateway.rs:164-184`,
`:280-289`, `:341-349`, `:371-393`, `:1010-1022`; `protocol.rs:30-38`, `:68`;
`feed.rs:81-95`; `actor.rs:1295-1309`; `migration.rs:74-101`;
`adjudication.rs:33`, `:350-360`, `:388-400`, `:540-543`, `:852`, `:860`;
`atrest.rs:12-27`, `:82`; `schema.rs:13-30`; `intent/mod.rs:184-210`;
`ruleset.rs:1-6`; `game.rs:30-36`, `:170-173`; `bin/persistd.rs:1258-1263`;
`manifest.rs:52-60`, `:99-113`; `core-gates.sh:100-105`;
`p4-ledger.sh:409-414`; `rust-toolchain.toml:2`; docs/adr/0021:40-42,
:61-64, :93-97; docs/adr/0029:369-378, :644; docs/adr/0038:136-173,
:198-205. Drift found against A8's text: the tamper-model quote A8 cited at
"game.rs:33-37" sits at `crates/orrery_games/src/game.rs:30-36` on this
tree, and A8's D38 (d)(3) quotation "neither number is ever derived from the
other" is the atrest.rs:26 restatement, not D38's own sentence (":204-205"
reads "must never be derived from each other") — both claims verified true
in substance, cited here at their real lines. A "nothing computes a digest"
sweep found every non-game `RulesetId` construction to be a test literal.

Mutations run at acceptance (break the stage → named check dies → revert →
passes; baselines recorded first; every failing run produced a real result
line; no mutation landed on both sides of an equality):

| # | Guarded stage broken | Named check | Observed | Reverted |
|---|---|---|---|---|
| M1 | `protocol_accepted` widened to `offered == current \|\| offered + 1 == current` (gateway.rs:183) — the exact widening any rolling-window manifest would need | `cargo test -p orrery_protocol --lib` | `gateway::tests::the_accepted_version_window_is_closed_to_exactly_this_version` FAILED; `125 passed; 1 failed` | `126 passed; 0 failed` |
| M2 | `migrate_bag`'s `UnregisteredComponent` refusal replaced with `continue` — the silent read-and-drop X-4 forbids | `cargo test -p orrery_persistd --lib migration` | `migration::tests::missing_registration_refuses_stale_checkpoint` FAILED; `5 passed; 1 failed` (355 filtered) | `6 passed; 0 failed` |
| M3 | `AdjudicationExecutor::register`'s eviction loop deleted (adjudication.rs:357-359) — builds accumulate past `RETAINED_BUILDS` | `cargo test -p orrery_persistd --lib adjudication` | `only_three_builds_stay_adjudicable` and `a_report_for_a_retired_build_is_undecidable_not_a_strike` FAILED; `11 passed; 2 failed` | `13 passed; 0 failed` |

All three sources restored byte-identical (`git status` clean); this record
is the branch's only change. At the first acceptance pass neither D44 nor
D48 had a file under `docs/adr/`; the rebase onto `origin/main` immediately
before the final commit brought in `0048-canonical-witness-projection.md`
(and with it D38's amended three-axis (d)(3)), and this record was updated
to link [D48] and cite the amended D38 text. D44 still had no file at the
final commit and remains cited as a plain decision id.

[D9]: 0009-verifiable-core.md
[D14]: 0014-pinned-versions.md
[D16]: 0016-parameter-reference.md
[D21]: 0021-ruleset-distribution.md
[D29]: 0029-low-population-path.md
[D38]: 0038-at-rest-schema-versioning.md
[D42]: 0042-canonical-simulation-architecture.md
[D43]: 0043-determinism-envelope-and-gate-replacement.md
[D45]: 0045-per-component-capability-policy.md
[D46]: 0046-message-class-semantics.md
[D47]: 0047-rollback-unit.md
[D48]: 0048-canonical-witness-projection.md
[A8]: ../plans/a8-compatibility-manifests.md
