# ADR-0050: Knowledge tiers - simulation authority, visual authority, and hearsay

**Status:** Proposed - **Date:** 2026-08-27 - **Decision:** D50

This record is non-normative until accepted. See the [ADR index](../DECISIONS.md)
for precedence, scope, and the complete accepted decision set. Acceptance is
reserved to the owner.

**Acceptance provenance.** On 2026-08-27 the owner signalled acceptance in
principle of a tiers record homing A14's hearsay rules: "I'm OK with accepting
a tiers like that and home A14 there. For now. I might change my mind, but it
looks convincing." That is a signal, not a sign-off: an Accepted record is
normative over the README and every numbered expansion document
(`AGENTS.md:16-17`), amending one is owner-reserved, and the owner explicitly
reserved the right to change their mind - so this record lands Proposed, and
flipping its status is the owner's act alone.

**Supersedes:** nothing, and it amends no accepted record while Proposed. It
carries into a record the vocabulary A12 section 5.6 defined from evidence
(`docs/plans/a12-exchange-systems-shakedown.md:402-527`), the hearsay tier A13
section 3 built on that vocabulary
(`docs/plans/a13-aggregation-beyond-aoi.md:158-203`), and the H-rules as A14
section 7 amended them
(`docs/plans/a14-summary-tier-as-performance-mechanism.md:772-810`); A14
supersedes A13 wherever they differ, and clause (d) below adopts the A14 text.
[D46] needs no amendment for any of this: its presentation-class rule
("presentation feeds nothing canonical; diagnostics affect nothing and may be
read by no rule", `0046-message-class-semantics.md:135-140`) is the mechanical
half of the visual-authority boundary, and A12 section 5.6 says so explicitly
(`a12-exchange-systems-shakedown.md:526-527`). This record leans on [D40]'s
regime layering and its `affects => member` invariant while D40 is itself
Proposed (`docs/DECISIONS.md:79-83`); if the owner rejects D40, clauses (d) H5
and (e)(3) need re-derivation and say so inline.

Out of scope, each with its owner: the **walker** - A14 recommends it as its
own proposed ADR because it adds an infrastructure claim basis, a scheduling
policy, an `Elapse` order class, and a witnessing posture
(`a14-summary-tier-as-performance-mechanism.md:528-539`); it is a topology and
authority matter, not a knowledge-tier one, and this record only references it
as proposed follow-on work. The **island-manifest exposure** (#535, every
member sees every member's cells) - the owner's 2026-08-27 ruling on #535
accepts the *concept* of a beyond-AOI feed but not the mechanics of how a peer
comes to hold it, and directs that the manifest question be settled inside the
hearsay frame; clause (g) therefore states the question this record makes
answerable and deliberately does not answer it. Also out of scope: the
interest-shape record, sight-only grants, and the adjudicated spin-up
commitment (A14 sections 2 and 4.4 - proposals awaiting their own decision);
the summary folds' implementation and constants (A14 section 8); and all of
D40's own open tensions.

## Context

### 1. The vocabulary is load-bearing and had no record

"Simulation authority" and "visual authority" are used in seven doc-comments
across `clients/regolith/src/` and throughout A13 and A14 - everywhere by
reference to #519, and until A12's 2026-08-27 amendment
(`a12-exchange-systems-shakedown.md:404-418`, merged at `7118025b`) nowhere
with a definition. A13 defined its hearsay tier *as* "neither authority"
(`a13-aggregation-beyond-aoi.md:163-166`), so the hearsay rules inherit any
drift in the two definitions; A12 section 5.6 recommended a short tiers ADR
for exactly that reason (`a12-exchange-systems-shakedown.md:515-527`), and A12
section 7.7 recorded that accepting it is the owner's alone
(`a12-exchange-systems-shakedown.md:643-650`). This is that record.

### 2. The evidence the definitions were argued from

The distinction is not aesthetic; each boundary was drawn at a caught failure,
argued in full in A12 sections 1 and 5.6:

- **#502/#505 frozen ghost**: the skin kept drawing a replica after
  replication stopped asserting it - the skin quietly acquiring simulation
  authority. Fix: expiry as a defined transition, `REPLICA_TTL_TICKS = 120`
  (`clients/regolith/src/campaign.rs:79`).
- **#517/#518 invisible tracer**: the skin correctly declined to draw a
  flight it had not been told about; the fix was compliant interpolation
  between two stated facts, with the endpoint frozen and no arrival cue
  (A12's worked-case table, `a12-exchange-systems-shakedown.md:480-489`).
- **#522 impact burst**: the skin announced a miss as a hit for one tick by
  drawing an outcome off a provisional cue - the quiet-acquisition failure in
  miniature, caught and fixed (`a12-exchange-systems-shakedown.md:486`).
- **#533 AOI fade**: uncertainty drawn as uncertainty - a fade on distance to
  the interest boundary, describing the client's knowledge thinning and
  asserting nothing ("a faded craft is not damaged, not cloaked, and not
  further away than it is", `clients/regolith/src/aoi.rs:12-16`).

The hearsay tier's rules were drawn against the same adversary from the other
side: a summary view of the world beyond the AOI must not impersonate either
authority, and A13 named its failure modes before any summary exists to fail
(`a13-aggregation-beyond-aoi.md:158-203`); A14 then amended the rules with
enforcement mechanics and one new rule
(`a14-summary-tier-as-performance-mechanism.md:772-810`).

## Decision

### (a) Three tiers, exhaustively, and what each is

Every piece of world-knowledge a process holds or presents is in exactly one
tier:

1. **Simulation authority** - *what happened.* It is the ruleset's, exercised
   under own-state discipline and expressed only as adjudicated artifacts:
   hashed per-entity state and the [D46] event classes above diagnostics
   (`a12-exchange-systems-shakedown.md:420-428`;
   `docs/adr/0046-message-class-semantics.md:106-140`). Presentation never
   infers it, predicts it, or fills gaps in it.
2. **Visual authority** - *how it is shown.* It is the skin's: framing,
   interpolation, smoothing, fades, labels, cues, and the timing of all of
   them - provided the skin asserts nothing the ruleset has not said
   (`a12-exchange-systems-shakedown.md:429-435`). Visual authority is real
   authority: "asserts nothing new" does not mean "adds nothing" - the #518
   tracer and the fade band are skin decisions the ruleset neither makes nor
   could (`a12-exchange-systems-shakedown.md:491-498`).
3. **Hearsay** - *what somebody told you about parts of the world you cannot
   verify.* Non-authoritative, source-labelled, age-stamped knowledge:
   summary views, aggregates, sighting memory - anything beyond what
   replication currently asserts and adjudication has stated
   (`a13-aggregation-beyond-aoi.md:163-166`). Hearsay is not evidence; a
   signature on it would prove provenance, not accuracy
   (`a13-aggregation-beyond-aoi.md:186-191`).

The tiers are a classification of *claims*, not of processes: one client holds
all three at once (its replicas and received events, its rendering decisions,
its map blips), and the rules below govern what may move between them.

### (b) Simulation authority's rule set (restated, not created here)

Simulation authority is already governed by accepted records; this clause
names them so the tier is complete, and adds nothing:

1. Own-state discipline: a step reads its own state and recorded neighbor
   reads only - `StateView::neighbor` appends every read to the replayed
   read-set (`crates/orrery_core/src/ruleset.rs:131-134`), and live neighbor
   reads in rules crates are gate-banned ([D43]; `scripts/core-gates.sh`).
2. Single writer: at most one node writes an entity's replicated state at any
   instant, lease-fenced at the gateway (INV-1, `docs/04-authority.md:61`).
3. Message classes and tier boundaries: no class crosses a tier boundary
   backwards within a tick ([D46] clause (a),
   `0046-message-class-semantics.md:135-140`).
4. Its artifacts are witnessable and replayable ([D9], [D10], [D48]).

### (c) Visual authority's rule, in checkable form (adopted from A12 5.6)

> **The skin may interpolate between facts the ruleset has stated; it may
> never extrapolate into facts the ruleset has not stated.**
> (`a12-exchange-systems-shakedown.md:449-450`)

Mechanically (`a12-exchange-systems-shakedown.md:452-468`): for a drawn
element `E`, let `F(E)` be the ruleset-authored facts it derives from - events
this client received, plus replicated hashed state no older than the
presentation-mirror staleness bound (A12 section 5.3). `E` is within visual
authority iff all three hold:

1. `E = f(F(E), camera, style)` - a pure presentation function of those facts
   plus camera and styling inputs, feeding nothing back (the [D46]
   presentation-class rule).
2. Every world-predicate a player would read off `E` ("this exists", "it is
   here", "it was hit", "it is named X") is entailed by some member of
   `F(E)`. Values *between* two members are entailed; values or predicates
   *beyond* them (an outcome before its verdict, a position after the bound,
   an invented name) are not.
3. If `F(E)` is empty, `E` renders absence or visible uncertainty - no
   placeholder that could be mistaken for a fact.

Two boundary notes carried with the rule
(`a12-exchange-systems-shakedown.md:491-498`): visual authority may be *early*
about a stated fact (a provisional cue a later authoritative event
contradicts is explicitly accepted); it may not be *inventive* about an
unstated one.

### (d) Hearsay's rule set: H1-H6, in their A14 form, verbatim in force

The H-numbers are adopted as this record's clause names - no renumbering - so
every existing reference in A13 and A14 resolves unchanged. The normative text
is A14 section 7's (`a14-summary-tier-as-performance-mechanism.md:772-810`),
which restates A13 where unamended; the mapping:

| Rule | A13 (original) | A14 (as adopted here) | Change |
|---|---|---|---|
| H1 | `a13:170-177` | `a14:776-779` | unchanged |
| H2 | `a13:179-184` | `a14:780-784` | amended - boundary clarified |
| H3 | `a13:186-191` | `a14:785-789` | unchanged in force, sharpened in mechanics |
| H4 | `a13:193-197` | `a14:790-795` | amended - enforcement point fixed, exemption named |
| H5 | `a13:199-203` | `a14:796-802` | amended - mechanism named |
| H6 | (none) | `a14:803-810` | new in A14, adopted here as proposed |

(`a13` = `docs/plans/a13-aggregation-beyond-aoi.md`, `a14` =
`docs/plans/a14-summary-tier-as-performance-mechanism.md`.)

- **H1 - Hearsay is never a simulation input.** The ruleset may not read it.
  A sensing *mechanic* is simulation authority producing events - and its
  interest shape and spin-up commitment, if those land, are simulation-side
  artifacts too.
- **H2 - Hearsay never gates membership or rate, in either direction.**
  "The client has a blip, so we may skip the replica" is forbidden;
  replication must behave identically with any summary tier on or off.
  *Amendment (A14):* ruleset-authored interest shapes and adjudicated
  promotions are the legitimate membership channel and are not hearsay; the
  forbidden move is any inference from summary possession to replication
  behaviour.
- **H3 - Hearsay is labelled with source and age, end to end.** Every summary
  record carries who computed it and when, and the skin renders age visibly.
  For at-rest content the label is the true per-cell age since last write -
  currently derivable only at shard granularity from the checkpoint
  watermark; a row-level last-write tick in the versioned value envelope is
  the proposed fix (A14 sections 3.2 and 5.3).
- **H4 - Hearsay is coarser and staler than action.** Resolution no finer
  than the product's declared cell edge `E`; delivered age at least
  `E / v_max`, with `v_max` the ruleset's declared speed cap, so a datum's
  positional uncertainty exceeds its own resolution by the time it arrives.
  Worked (A14 section 5.2, `a14:619-634`): at the campaign's
  `v_max = 120 m/s` (`crates/orrery_games/src/regolith/archetype.rs:94`),
  `E = 512 m` needs age >= 4.3 s and `E = 4096 m` (shard cell) needs
  >= 34.1 s. *Amendment (A14):* checkpoint and uplink cadences are ceilings
  on age, not floors - the floor is enforced where the aggregate is served,
  by folding at cadence `F` and double-buffering so the *previous* fold is
  served (delivered age in `[F, 2F)`); at-rest content is exempt
  (`v_max = 0` - nothing persistent moves without a writer, INV-1,
  `docs/04-authority.md:61`) and is served exact with its stamped age.
- **H5 - Hearsay respects reveal gates at the source.** State a ruleset hides
  behind a logged reveal ([D40] regime 3,
  `docs/adr/0040-visibility-and-spatial-query-layering.md:100-102` -
  Proposed) must not leak into aggregates, not even as a count - and rest
  does not relax it. *Amendment (A14):* enforced structurally - regime-3
  secrets live in a key family no fold scans, and restricted revealed state
  carries an at-rest visibility class outside the component bag; folds count
  aggregable classes only. Peer-side aggregation is disqualified because it
  can do neither (A14 section 5.3, `a14:636-665`).
- **H6 - A read is not a promotion (proposed; new in A14).** A summary read
  leaves the world unchanged: no key written, no lease minted, no `actor/`
  CAS, no tick advanced, no ruleset-observable event; per-client read cost
  bounded before dispatch. Promotion (activating a cell so its entities
  behave) is a distinct act: requested explicitly, committed under an
  adjudicated ruleset hold, bounded per peer *and* by a cluster admission
  budget that refuses rather than buffers. Activation as a side effect of a
  read is forbidden (A14 sections 4.1 and 4.4).

### (e) The boundaries: what may cross, in which direction, and what may never

1. **Simulation -> visual: facts flow down.** The skin consumes adjudicated
   events and replicated state, and clause (c) bounds what it may assert
   from them. This is the only lawful source of a rendered world-predicate.
2. **Simulation -> hearsay: facts may be summarised.** Aggregates are
   read-side projections of publications that already ship under simulation
   authority's fences (the bulk uplink, the lease index - A14 sections 5.1
   and 6.2); summarisation must apply H3-H5 at the fold.
3. **Hearsay -> simulation: never.** H1. This is load-bearing: every
   neighbor read is part of the replayed read-set
   (`crates/orrery_core/src/ruleset.rs:131-134`), and a best-effort,
   per-client, unordered feed inside that read-set would make replay
   non-reproducible by construction (`a13-aggregation-beyond-aoi.md:170-177`).
4. **Hearsay -> replication behaviour: never.** H2, the second load-bearing
   boundary. Membership and rate are governed by simulation-side facts
   (occupancy, and - if the interest-shape proposal lands -
   ruleset-authored shapes); hearsay is additive over a correctly-sized
   interest set.
5. **Visual -> simulation: never.** [D46]'s presentation class feeds nothing
   canonical (`0046-message-class-semantics.md:135-140`).
6. **Visual -> hearsay: allowed, one way.** Client sighting memory (fog of
   war) is the skin remembering facts it lawfully rendered; it becomes
   hearsay the moment the fact ages past the staleness bound, and must then
   carry H3's visible age. It may not flow back into rendered assertion as
   if current - that is the #505 ghost.
7. **Hearsay -> visual: allowed, labelled.** A skin may render hearsay
   (a map blip) only as hearsay: source-labelled, age-rendered (H3), never
   styled such that a player would read a current-world predicate off it
   (clause (c)(2) applies to the rendering).

### (f) Enforcement, honestly: structural, tested, review-only, or absent

Per rule, what actually holds it today, and what is a gap:

| Rule | Enforced by | Status today |
|---|---|---|
| (b) sim authority | core gates, recorded read-set, lease fence, goldens, witnessing | **structural + tested** (accepted records' machinery) |
| (c) visual authority | review test: skin code drawing an outcome must destructure the ruleset's outcome event verbatim; any value without a stated endpoint must be frozen, aged, or faded - never advanced by a skin-side world model (`a12-exchange-systems-shakedown.md:470-476`). Pinned tests: replica expiry (`campaign.rs:79` and its mutation) and the muzzle-interpolation mutation `a_campaign_muzzle_interpolates_without_claiming_an_arrival` (`a12:484`) | **review + spot tests.** Gap, named by A12: no cross-process assertion holds *rendered* claims against *adjudicated* facts (`a12:500-513`); a single-process test collapses the two authorities into one owner and cannot exercise the line |
| H1 | structural by absence: no hearsay feed exists, and no `Ruleset` API exposes one; the read-set recording and core gates would make an added one visible | **holds vacuously.** The obligation is on whoever lands a summary feed: it must not be reachable from rules crates, and the gates must be taught to refuse it |
| H2 | nothing | **review-only, and currently vacuous** (no tier exists). When one lands, the check is stated by A13: replication behaves identically with the tier on or off - a diffable property, not yet a test |
| H3 | nothing structural for the age stamp | **partially unenforceable today:** live `world/` rows carry no per-row write tick (A14 finding, `a14:893-897`), so at-rest age is shard-coarse via the checkpoint watermark until the proposed envelope extension lands (a [D38]-scheme amendment, owner-reserved) |
| H4 | the double-buffered serving fold (specified, not built) | **unenforced today; enforceable by construction when built**, with one testable property: delivered age in `[F, 2F)`. The hoped-for freebie - checkpoint lag - fails structurally: cadences bound age above, not below (`a14:619-634`) |
| H5 | key-family separation and the at-rest visibility class (both proposed) | **unenforced today, and currently moot:** the world is fully public (regime-1) and regime 3 is itself only Proposed in [D40]; the record introducing regime 3 must introduce the class. Fail-closed once built: a fold cannot leak what its range never covers |
| H6 | read side: nothing at the `Subscribe` arm | **a live gap, found by A14 and not created by any summary tier:** the gateway's `Subscribe` arm takes client-named cells bounded only by an inflight-permit semaphore, and the prefix-admitting reader makes `CellId::ROOT` a covering scan (`a14:382-394`; flagged there as a finding, not a proven vulnerability - an outer restricting layer may exist and was not found). Promotion side: does not exist yet; the admission budget and adjudicated hold are its specified shape |

An honest gap beats an aspirational claim: of the eight rows, only the first
is fully machine-held today. This record makes the gaps checkable by naming
them; it does not close them.

### (g) A question this record makes answerable, and deliberately does not answer

The island manifest ships every member's cells to every member (#535; A13/A14
carry the exposure audit, `a14:748-751`). Under this record's vocabulary that
is a *hearsay feed delivered without H3-H5*: the concept of a beyond-AOI feed
is accepted (owner ruling on #535, 2026-08-27), and the tiers frame is where
"how does a peer lawfully come to hold it" must be settled - labelled (H3),
bounded (H4), reveal-filtered (H5). The mechanics are explicitly not being
pushed yet; #535 stays open as a recorded question, and the next move belongs
to the owner.

### (h) Proposed follow-on work (references, not decisions)

1. **The walker ADR** (A14 section 4.3 and its scope judgement,
   `a14:528-539`): an infrastructure claim basis, a scheduling policy over
   the implemented handover machinery, the `Elapse` order class, a
   witnessing posture, and the farmability knob - a topology and authority
   record, deliberately not folded into this one.
2. **The envelope extensions** (last-write tick and at-rest visibility
   class, A14 section 5.3) - would amend the [D38] scheme; owner-reserved.
3. **The `Subscribe` admission bound** (clause (f), H6 row) - worth doing
   independently of everything else here.
4. **The cross-process rendered-vs-adjudicated assertion** (clause (f),
   visual-authority row) - A12 section 5.6's testing consequence.

## Consequences

**What this forbids.** A ruleset that reads a map, a scanner product faked
from summary data instead of computed as a mechanic (H1); any replication
shortcut justified by the client "already having a blip", and any summary
product that quietly substitutes for a correctly-sized interest set (H2);
unlabelled or ageless summary data, however convenient (H3); wallhack-grade
freshness in any aggregate of activated content (H4); aggregate leaks of
reveal-gated state, including counts, and therefore every peer-side deliverer
of aggregates - gossip stays dead (H5; A14 section 6.3); activation as a side
effect of reading, and unbounded promotion (H6); and every skin element that
asserts a world-predicate the ruleset has not stated, placeholders included
(clause (c)).

**What it costs.** A one-tick-minimum, adjudication-shaped path for any
sensing mechanic that a hearsay shortcut would have faked cheaply (H1);
summary products that are deliberately coarse and stale even where fresher
data sits in memory, with the fold cadence as a standing serving cost (H4);
two storage-schema extensions before H3 and H5 are fully enforceable, and a
persistd that stays game-blind - fold logic may never grow a ruleset decoder
(A14 section 5.3); an admission-control surface (budgets that refuse) rather
than a buffer, with the UX obligation to render refusal diegetically (H6);
and, for the skin, the standing discipline that some visible gaps (the #517
missing tracer before its fix) are the contract working, not bugs - which
costs review attention every time one appears.

**What it buys.** Definitions with a stable home, so seven doc-comments, two
plan nodes, and every future summary product cite a record instead of an
issue thread; a classification any reviewer can apply mechanically (clause
(c)'s three-part test, clause (e)'s crossing table); and a frame in which
#535 and every future beyond-AOI question is answerable without re-arguing
what kind of knowledge is in play.

## What could not be verified

- **The owner's acceptance-in-principle quote** is carried as relayed in the
  drafting brief dated 2026-08-27; no committed artifact (issue or PR
  comment) carrying the verbatim sentence was located in the repository or
  via the GitHub API at drafting time. The #535 owner ruling *was* located
  and is quoted from the issue's comment thread.
- **The `Subscribe` gap** is inherited from A14 with A14's own caveat: an
  outer restricting layer may exist and was not found (`a14:884-891`).
- **A14's citation `crates/orrery_spatial/src/interest.rs:31` for
  `AOI_RADIUS_GRID`** has drifted by one line: the constant is at
  `interest.rs:30` on this tree. Value unchanged; recorded rather than
  silently rewritten.
- **The seven regolith doc-comment sites** using the vocabulary are cited
  from A12 section 5.6 (`a12:413-415`, read on `main` at `5ee8bfd1`); this
  record re-verified `aoi.rs:12-16` and `campaign.rs:79` directly and
  inherits the other five.
- **A12's worked-case table and measured figures** (#505's millimetre
  disagreements, #518's 81-muzzle-events measurement) are evidence of
  record quoted from the PR/issue threads; the raw session logs are not in
  the repository (A12 section 8 says the same).
- Whether any in-flight work besides this record drafts against A12 section
  7.7's deferral was not established.

## Verification appendix - what was read for this record

Every `path:line` above was read in this worktree at `7118025b` on
2026-08-27. Key anchors: A12 section 5.6 in full
(`a12-exchange-systems-shakedown.md:402-527`) and section 7.7 (`:643-650`);
A13 section 3 in full (`a13-aggregation-beyond-aoi.md:158-203`); A14
sections 4.1, 5.2, 5.3, 7, and 10
(`a14-summary-tier-as-performance-mechanism.md:368-394`, `:619-665`,
`:772-810`, `:884-933`); [D46] clause (a) point 2
(`0046-message-class-semantics.md:135-140`); [D40]'s regime table and
invariant (`0040-visibility-and-spatial-query-layering.md:100-102`,
`:117-140`) and its Proposed standing (`docs/DECISIONS.md:79-83`); the
recorded neighbor read (`crates/orrery_core/src/ruleset.rs:131-134`); INV-1
(`docs/04-authority.md:61`); the fade's claims
(`clients/regolith/src/aoi.rs:12-16`); `REPLICA_TTL_TICKS`
(`clients/regolith/src/campaign.rs:79`); the campaign speed cap
(`crates/orrery_games/src/regolith/archetype.rs:94`); and the owner's #535
ruling (issue #535 comment thread, 2026-08-27).

[D38]: 0038-at-rest-schema-versioning.md
[D40]: 0040-visibility-and-spatial-query-layering.md
[D43]: 0043-determinism-envelope-and-gate-replacement.md
[D46]: 0046-message-class-semantics.md
[D48]: 0048-canonical-witness-projection.md
[D9]: 0009-verifiable-core.md
[D10]: 0010-witnessing.md
