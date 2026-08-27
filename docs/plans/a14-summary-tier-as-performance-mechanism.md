# A14 - Summary tier as a performance mechanism: two shapes, two states, one gradient

> Speculative research requested by the owner (#542), superseding part of A13
> (`docs/plans/a13-aggregation-beyond-aoi.md`, which stays as the record of the
> hearsay rules). A13 asked "who should deliver a beyond-AOI summary" from the
> demand side and concluded the demand was unproven. Owner review reframes it
> across several rounds of input: cell interest is not cell membership; the
> summary is a performance optimization whose alternative is a bigger AOI; the
> interest is in entities, not peers; NPCs mean entities are not static; some
> scans must *activate* (a sniper zoom onto frozen AI is useless); a sparse
> cluster-side **walker** creates macroeconomic movement exactly where no
> active process otherwise would, deliberately skipping active areas; and
> activation is backpressured diegetically (the zoom animation is the
> cooldown). The owner also frames the walker as engine-level - "an idea for
> most projects that would build on a PU" - Orrery supplying when and where to
> walk, the ruleset supplying what macroeconomic movement means. Repository
> facts verified 2026-08-27 against this tree (`origin/main` 7e2c0708 plus the
> merged campaign constants A13 could only quote from PR #532's body).
> External claims carry a source or an explicit needs-verification flag.
> Nothing here amends an accepted record - **propose, not decide.**

## Verdict up front

**A13 answered the wrong question first, and this node reorders it.** A13
treated the summary as a product awaiting demand and proposed measuring demand
with a coordinator peer map. The owner's reframing is that the summary is one
of the lawful escapes from a cost law: AOI cost is volumetric at full
replication fidelity, so any mechanic that reaches past the AOI edge - a
sniper scope, a long-range scan - must either inflate the sphere (worked
below: a 2 km reach as inflated AOI costs ~27x the volume and blows the
entire 1 Mbps budget on the proxy floor alone), shape the membership set (a
cone: roughly constant cost), or read a summary (about 0.1% of budget). The
demand question dissolves: the moment any long-reach mechanic exists, one of
these is being paid for, and the first is unaffordable.

**Every cell is in one of two states, and both read paths already exist.** A
cell is **activated** - someone holds leases in it and it is ticking - or
**at rest** - nobody writes it, and its rows are *exact as of the last
write*, not stale, because the quiesce path flushes on the way down. The
gateway's area load already dispatches on exactly this partition (live cells
from actor memory, cold cells from FDB range scans,
`docs/08-persistence.md` section 9); the cold reader is first-class and
tested (`crates/orrery_persistd/src/keyspace.rs:4`,
`crates/orrery_persistd/tests/checkpoint_restore.rs:747`); and the activated
half's input - the lease location index - is already one contiguous range
scan per shard cell (`crates/orrery_persistd/src/keyspace.rs:338`). The
summary tier is therefore not a new delivery subsystem: it is a **read**,
dispatched across an existing partition, plus two guards (H4 aging on the
activated half, H5 filtering everywhere). An intermediate reading considered
during drafting - a three-tier ladder with the walker as a continuously
simulating middle rung - is rejected below (3.2): the walker is sparse and
read-mostly, so it is a second *activator*, not a third state.

**Reads and activations are different acts, and the node keeps the
distinction while dropping the prohibition.** A scan is a read: it leaves the
world unchanged and costs a range scan. A scope that must show entities
*behaving* is a promotion: it activates the cell, mints authority, and costs
simulation - the owner is right that some activation is needed, or the scope
product is a museum window. What must never happen is activation as a *side
effect* of a read; promotion is a distinct, adjudicated, rate-bounded act
(H6, section 4.4), because "activate to scan" silently reintroduces the
volumetric bill and hands griefers a lever: scan a thousand cold cells,
force a thousand cells of simulation.

**Staleness, cost, and wallhack danger scale together on one axis -
activity - and the mechanism self-regulates.** Nothing persistent moves
without a writer (single-writer INV-1, `docs/04-authority.md:61`), so an
at-rest cell's rows are exact and safe at fine resolution *because nothing in
them can be elsewhere*, while activated cells - the expensive, contested,
wallhack-relevant ones - are exactly the set that must be coarsened and
age-stamped. What varies across at-rest cells is not a staleness rate but an
**age since last write** - bursty, per-cell, and exactly what H3 already
requires be stamped. One finding there: `world/` rows carry no per-row write
tick (live values are tag + component bag,
`crates/orrery_persistd/src/keyspace.rs:108-120`; only tombstones carry a
tick), so the stamp must currently be derived from the shard's checkpoint
watermark `ckpt/{grid}/{shard}` (which stores a time, `docs/08-persistence.md`
section 6) - shard-coarse - or a last-write tick added to the versioned
envelope alongside the visibility class this node proposes anyway (5.3).

**The walker is a client of the activation mechanism, not a new authority
concept - and its writes must go through the ruleset.** It cold-reads widely
(shop inventories across a town), then activates one cell, updates it, and
releases - taking leases like any peer and drained by the same implemented
handover machinery when a player arrives (`docs/08-persistence.md:2086`).
Because it deliberately biases to *skip* activated areas - its reason to
exist is movement where no active process otherwise runs - it barely competes
with players for anything, and total simulation load stays roughly flat
across population: peers simulate where players are (the cluster only
persists outcomes, the already-sized cost of docs/08 section 13), and the
walker's spend is a dial on how fast the empty world moves. The hardest
question has a definite answer from the tree: a walker that rewrites rows
without adjudication would be an unauditable god-process inside a system
whose whole apparatus makes claims attributable (reads are recorded -
`StateView::neighbor` appends to `self.reads`,
`crates/orrery_core/src/ruleset.rs:131-134`), so **the walker must be a
ruleset participant** - and since dt is baked in (`TICK_HZ = 60`,
`crates/orrery_core/src/executor.rs:27`), "coarse tick" must mean either
burst-ticking at true dt or an explicit, ruleset-defined `Elapse` order
(4.3). This constrains the whole design and is the main reason the walker
should be its own proposed ADR, with A14 carrying only what makes the
activation path legible.

**The coordinator should not learn about entities - not even a little.** The
obvious shortcut (extend `Presence` with an entity list) fails three ways:
self-report becomes world-report without a fence; memory and churn bounded by
world population instead of AOI size; and a rebuild-from-reannouncement
service lands on the path of every spawn and handoff. The defensible variant -
counts derived from lease *grants*, issued rather than claimed - lands in
persistd anyway, where the registrar and the index live. Gossip stays dead,
for strengthened reasons (6.3).

## 1. The cost law: AOI cost is volumetric, and the campaign now proves it in-tree

The geometry A13 could only quote from an open PR is now landed and verified:

- Campaign interest-cell edge **512 m** (`crates/orrery_games/src/regolith/mod.rs:113`),
  sized in its own doc comment for the stock weapon's 400 m reach.
- Stock reach **400 m** = `optimal_mm` 300 000 + `falloff_mm` 100 000
  (`crates/orrery_games/src/regolith/weapon.rs:47-48`). Heavy reach **900 m**
  = 700 000 + 200 000 (`weapon.rs:67-68`).
- Craft speed cap **120 m/s** (Interceptor `max_speed_mms: 120_000`,
  `crates/orrery_games/src/regolith/archetype.rs:94`; Cruiser 60 m/s,
  `archetype.rs:102`). A13's H4 arithmetic used a 32 m/s cruise figure; the
  bound must use the cap - the adversary flies flat out.
- Campaign orbit radius **2 500 m** (`mod.rs:89`); island window budget
  8 craft + 24 rocks + 4 pickups + 1 director = 37 (`mod.rs:47-56`).
- AOI is the 3x3x3 cell block (`docs/01-spatial-model.md:147`,
  `AOI_RADIUS_GRID = 1.5`, `crates/orrery_spatial/src/interest.rs:31`);
  framework default edge 128 m (`crates/orrery_protocol/src/cell.rs:58`);
  peer upload budget <= 1 Mbps (`crates/orrery_net/src/budget.rs:1-6`,
  default at `budget.rs:166`), 60 B per-datagram overhead (`budget.rs:48`).

**The volumetric argument, with numbers.** The 27-cell block guarantees
coverage only to radius `edge - m` from the observer (hysteresis margin m =
10% of edge, `docs/01-spatial-model.md:204`): **460.8 m at the campaign
edge**. To guarantee a 2 km reach by inflation, the block half-width must grow
from 1 cell to 4 (4 x 512 = 2 048 m): a 9x9x9 block, **729 cells, 27x the
volume**. Entity count scales with volume at fixed density. docs/03 section 8
(`docs/03-replication.md:246`) models a typical 27-cell AOI at ~100 replicable
entities; 27x is ~2 700. The proxy *floor* alone - 1 Hz x 40 B each
(`docs/03-replication.md:142`) - is then 2 700 x 40 B/s = 108 KB/s =
**864 kbps, 86% of the whole peer budget**, before the high-rate set, the
witness lane's reserved 20%, datagram overhead, or the 2 Hz typical proxy rate
(at which it is 1.73 Mbps, 173% of budget). ADR-0040's wire-floor bound makes
this a *lower* bound for anything in the influence set, not an optimization
target (`docs/adr/0040-visibility-and-spatial-query-layering.md:152-166`).
Senders pay symmetrically. Inflation is dead on arrival.

**The same reach as a cone.** A scope with a 2 degree half-angle and 2 000 m
reach sweeps volume (pi/3) x L^3 x tan^2(theta) = 1.047 x 8e9 x 0.00122 ~
**1.0e7 m^3 - 7.6% of a single 512 m cell** (1.34e8 m^3). Conservative
cell-granular membership along the ray adds ~4 cells (up to ~a dozen with
diagonal traversal), an expected fraction of one entity at AOI density, and at
most a few kbps. Breadth traded for reach at roughly constant cost - the
owner's framing, confirmed by arithmetic.

**The same reach as a summary read.** A 5x5x5 block of campaign shard cells
(shard = 8x8x8 interest cells, `crates/orrery_protocol/src/cell.rs:43`; edge
4 096 m; span ~20 km) at ~10 B per cell (id delta + count) is ~1.25 kB per
refresh; at 0.1 Hz that is **~1 kbps, 0.1% of budget**, on the reliable lane.

**And the argument "most entities are static" is dead, explicitly.** The
campaign population is 8 craft that move, 24 rocks that drift, reflect and
split (`mod.rs:47-56`, reflection at `mod.rs:63`), and pickups that appear
and expire. In the campaign as played the NPC craft *are* the population -
and the walker exists precisely so that even unvisited cells change over
time. Nothing below rests on a static-world assumption; where stillness
matters (section 3), it is *derived* per entity from lease state, never
assumed of a class.

**A live finding: the tree already contains a scope-shaped incoherence.** Rock
kills drop Heavy pickups (`mod.rs:901-905`), Heavy's reach is 900 m
(`weapon.rs:67-68`), and lock-break is keyed to weapon reach
(`mod.rs:1135-1142`) - but the campaign AOI guarantees only 460.8 m. A craft
holding Heavy can hold a live lock on a target the interest set does not
guarantee to contain: #520's incoherence, reproduced one weapon up.
`CAMPAIGN_CELL_EDGE_M`'s comment scopes itself to the *stock* reach
(`mod.rs:105-112`), so this is presumably known-but-unresolved rather than
overlooked; fixing it by another edge doubling costs 8x volume, while a
ruleset-authored shape along the lock line costs a few cells. Heavy is the
first concrete consumer for shaped interest, already in the tree.

## 2. Scope: shaped membership, which the invariant already demands

**Cell interest is not cell membership.** Today they are conflated: an
observer's interest set is exactly the 27 rooms derived from its occupancy
(`docs/03-replication.md:112`), and interest.rs selects fidelity within that
set. Decoupling them means interest = f(occupancy, ruleset-authored shapes) -
a superset of the 27, with the shape choosing *which* cells, not merely *how
many*.

**This is not an optimization; ADR-0040's invariant requires it.** The
proposed governing invariant is `affects(e,o,t) => member(e,o,t)`
(`docs/adr/0040-visibility-and-spatial-query-layering.md:117-140`; D40 is
proposed, not accepted, `docs/DECISIONS.md:79-80`). A 2 km scope-fired weapon
extends `affects` 2 km along the aim; the invariant then *requires*
membership there. The cone is the minimal lawful membership set for the
mechanic; the inflated sphere was never required - it is a 27x overshoot of
what the invariant asks. Shaped interest is how a scope mechanic complies at
all.

**H2 is not violated, and the reading survives checking.** A13's H2
(`a13-aggregation-beyond-aoi.md:182-184`) forbids *hearsay* gating membership
or rate, in either direction. A ruleset-authored shape is authored from the
simulation side - it is the legitimate channel by which membership changes,
the same channel that today authors the 27-cell block itself. D46/#354's
"occlusion modulates rate, never membership" (retained as regime 1's posture,
`0040:538-541`) constrains *heuristics demoting* members; a ruleset shape
*adding* members is regime-2/simulation fact, the direction that fails toward
extra bytes. No rule is bent; the trap would be the reverse move (a summary
blip excusing a missing replica), and H2 continues to close it.

**The design-vs-landed divergence is the hook, but not the mechanism.**
docs/03 section 4.1 specifies multi-factor scoring with an aim term
(`g_aim = max(0, cos theta)`, "game-overridable hook",
`docs/03-replication.md:120-131`); landed selection is "purely distance-based
(nearest-first)" (`crates/orrery_spatial/src/interest.rs:10-11`). A13
section 8 filed this as a stale citation. It is more: the aim term is the
germ of a scope. But scoring only ranks *within* the AOI - a scope needs cell
subscription *beyond* it - so landing section 4.1 is necessary and not
sufficient. Three pieces are missing, none large:

1. **A shape record**: the ruleset declares an interest shape (cone: origin,
   axis, half-angle, reach) as a simulation-side artifact of an equipped
   mechanic. Replication obeys it; it never obeys replication.
2. **Grant coverage**: the coordinator's interest grant covers the shape's
   cells (`report_presence`, `crates/orrery_coordinator/src/registry.rs:216`).
   One interaction needs care: interest coverage gates weak authority claims
   (the client-side plausibility gate honours "the claimant's active interest
   covers the entity's cell", `crates/orrery_authority/src/contact.rs:33-38`).
   A scope must grant *sight, not claim rights* - propose a sight-only flag
   on shape-derived coverage, or a scope becomes a remote-grab enabler.
3. **Shape-aware scoring**: section 4.1's aim term, so the scoped target can
   win a high-rate slot without the 24-cap starving the near field.

A fourth piece appears once activation is in the picture: a scope pointed at
an at-rest cell that must show entities *behaving* is a promotion request,
not a membership change - section 4.2 owns that trigger.

**Scope vs scan: two configurations of one authoring surface, two delivery
mechanisms - and the split is forced, not chosen.** Both are "the ruleset
names a region and a fidelity". But membership must be live (20 Hz,
prediction, hit validation) and is peer-served; aggregates must be *aged*
(H4) and *reveal-filtered* (H5), which only the cluster can do (section 5.3).
Merging the mechanisms would either leak (live aggregates) or lag (aged
membership). One record shape -
`InterestShape { region, fidelity: Member | Aggregate, min_state }` - two
executors. Fidelity `Member` routes through grants and replication under
ADR-0040; fidelity `Aggregate` routes through the summary read under H1-H5;
`min_state` is 4.2's promotion trigger.

## 3. The dichotomy: activated or at rest

### 3.1 The dispatch already exists

The owner's frame: a cell is activated (someone holds it, it is ticking) or
it is not (its rows are exact as of the last write). Verified, this partition
is *already how the gateway serves area loads*
(`docs/08-persistence.md:3310`, section 9): live cells are served from actor
memory ("authoritative, >= checkpoint freshness"), cold cells by FDB range
scans over `world/{grid}/{cell}/...`, pages streaming nearest-first on a
dedicated area stream (section 9.1). The cold half is first-class, not
tooling: the keyspace module names "the cold reader" beside the checkpointer
and seeder (`crates/orrery_persistd/src/keyspace.rs:4`); `read_cold` is on
the checkpoint-store trait
(`crates/orrery_persistd/src/checkpoint/mod.rs:130-141`); a
`ColdFallbackRouter` serves a 27-cell area load from a cold, never-loaded
seeded world (`crates/orrery_seed/tests/fdb_gates.rs:549-558`); and the
restore suite pins granularity and honesty - a cold read of one interest cell
returns exactly that cell's rows, a shard read returns the whole subtree
(`crates/orrery_persistd/tests/checkpoint_restore.rs:747-760`), with no
resurrection of tombstoned entities (`checkpoint_restore.rs:984-999`). Cost
per cold cell: one in-region range scan, FDB reads 0.1-1 ms, < 50 ms to
first page-in (`docs/08` section 9).

So the summary tier's skeleton is: **a read, dispatched across the existing
activated/at-rest partition, projected to an aggregate instead of full
pages.** What is genuinely new is the projection, the guards, and the right
to request cells you are not standing in - smaller than anything A13 costed.

### 3.2 Two states, two activators - and the rejected trichotomy

During drafting, the walker was briefly modeled as a third *state* - a
continuously simulating middle rung between live and cold. That reading is
rejected: the walker is sparse and read-mostly (it cold-reads a town's shops,
activates one cell, writes, releases), so a walker-tended cell is simply an
activated cell while the walker holds it and an at-rest cell otherwise. The
dichotomy is exhaustive; **walker and player are both activators**, differing
in who they are and what triggers them, not in kind. Consequences:

- **"At rest = exact" survives in full.** Quiesce-flush checkpoints
  immediately on the way down (`docs/08` section 8), so an at-rest cell's
  rows are the entities, not a stale copy of them.
- **What varies per at-rest cell is age since last write**, bursty and
  per-cell - a walker-visited shop is hours old, a never-visited wreck field
  is seed-old - and H3 already requires the stamp. The gap: live `world/`
  values carry no per-row tick (`keyspace.rs:108-120`; tombstones do), so
  the stamp is currently derivable only from the shard checkpoint watermark
  (`ckpt/{grid}/{shard}` stores a time, docs/08 section 6). Propose a
  last-write tick in the versioned envelope, beside 5.3's visibility class.
- **No new authority concept.** The walker takes leases like any holder and
  is drained by the implemented handover machinery when a player arrives
  (4.3).

Boundary cases, worked, with the axis they force:

- **Hot but peerless.** There is *no cell-state eviction path* in persistd:
  a quiesced cell checkpoints immediately and the actor "goes on holding it"
  (`docs/08-persistence.md:3306`, section 8; issue #124 Part 2). Many
  at-rest cells are therefore served from actor memory rather than FDB.
  Harmless: the actor's copy of an unwritten entity equals the at-rest row,
  so the dispatch degrades to "whichever copy is nearer".
- **Mid-quiesce / waking during the scan.** Quiesce-flush is an immediate
  checkpoint (section 8) that also demotes pinned terrain sections
  (`docs/08-persistence.md:3340`); activation is fenced by the `actor/` CAS
  for storage placement and by the registrar for simulation authority. A
  read racing either fence serves a well-defined side of it, and both sides
  agree for unwritten entities.
- **Peers present, entities unclaimed.** An activated cell can hold parked
  entities (`holder: None`, `docs/04-authority.md:86`). They do not move -
  no lease, no writer - so they belong to the exact-and-safe class even
  inside an activated cell.
- **Moving entity in a peerless cell.** The converse: a leased NPC craft can
  fly through a cell no peer occupies (interest spans 27 cells; the
  plausibility gate requires only that the *claimant's* interest cover the
  entity's cell). Peer-presence does not bound motion; lease state does.

The last two correct the axis at fine grain: the exhaustive classification is
**who currently writes each entity** - some lease holder (peer or walker), or
nobody. The cell-level dichotomy is the routing approximation of that
entity-level fact, and the coordinator's shard-level presence
(`registry.rs:110`) plus the registrar's lease index are its cheap oracle.
One class is invisible to every read path: island ephemerals (projectiles,
debris) never reach the registrar and are never persisted
(`crates/orrery_authority/src/ephemeral.rs:1-11`); no summary product in A13
section 1's table wants them.

### 3.3 The gradient: staleness, cost, and danger on one axis

The claim to test (owner: staleness and cost scale on the same axis, so the
mechanism self-regulates; flagged as too neat to accept unverified):

- **Motion requires a writer.** INV-1: at most one writer per entity, and
  the gateway enforces the lease fence on every uplink
  (`docs/04-authority.md:61`). No writer, no motion.
- **Exactness is the complement of writing.** An at-rest row is exact - and
  quiesce forces a flush on the way down, so a whole at-rest cell is exact
  *now*, not 20 s stale. Serving it fine-grained reveals a truth that cannot
  run away or shoot back: H4's bound `age >= E / v_max` is satisfied with
  E -> 0 when v_max = 0.
- **Cost and danger concentrate on the activated side, together.** Leased
  entities are the moving, contested, wallhack-relevant ones, and the only
  ones whose aggregate must be recomputed continuously. Cost,
  staleness-need, and danger arrive together, and the coarsen-and-age
  treatment applies to exactly the set that needs it.

Verdict: **the gradient holds, with two honest limits.** (1) It keys on
leases, not peer presence - a scan product must not render "peerless" as
"safe or static". Building the activated half on the lease index (5.1),
where a walker-holder appears like any other, makes the implementation
follow the correct axis automatically. (2) "Exact and safe" is an H4
statement only. An at-rest seeded rock pocket's exact location may be a
gameplay secret (prospecting is the product); whether it is readable is a
reveal-policy question - H5 - and rest does not answer it. The gradient
removes the *staleness* policy knob, not the *secrecy* one.

## 4. Reads, promotions, and the walker

### 4.1 The distinction (no longer a prohibition)

A **read** returns rows or aggregates and leaves the world unchanged: no key
written, no lease minted, no `actor/` CAS, no tick advanced, no
ruleset-observable event. The tree's cold reads are already on this side
(`fdb_gates.rs:549-558` reads a never-activated world). A **promotion**
activates a cell and deliberately costs simulation. Both are legitimate;
what is forbidden is promotion as a *side effect* of a read - the
natural-sounding "activate to scan" - because it reintroduces the volumetric
cost the tier exists to avoid and converts every scan UI into a
denial-of-service lever against the cluster.

The admission bound for reads is missing today, and not only for the summary
tier: the `Subscribe` arm takes client-named `{grid, cells}` and routes
them, bounded only by an inflight-permit semaphore
(`crates/orrery_persistd/src/gateway.rs:6532-6571`); no validation of the
requested set against the client's location or grant is visible at that arm,
and the snapshot reader admits by prefix, so `CellId::ROOT` is a covering
scan of the whole grid (`docs/08-persistence.md` section 9's own warning).
If no outer layer restricts the set - none was found; flagged in section 10 -
a client can already request a whole-grid scan. The summary tier does not
create this exposure; it is the occasion to close it: a per-client token
bucket on cold-scan cells (D25 precedent: bound a fan-out with per-recipient
buckets, `docs/adr/0016-parameter-reference.md:27`) and a grant-derived
allowlist - your 27 cells, your shapes' cells, nothing else.

### 4.2 When a scan must promote, and what the trigger is

The owner's correction stands: a sniper zoom onto entities that would
normally animate and execute AI is useless if they stay put. The ruleset,
not the transport, declares which products need which state:

- **Aggregate products** (map counts, prospecting, threat halo) read the
  cell as-is. An at-rest cell's aggregate is exact with its stamped age; an
  activated cell's is aged per H4. No promotion, ever.
- **Behavioral products** (the zoom that shows a pirate base *doing things*)
  set `min_state: Activated` in their `InterestShape`. If the cell is at
  rest, the scope *requests promotion*, and the promotion is adjudicated and
  rate-bounded (4.4). Where the scoped content can be engaged through the
  scope, ADR-0040's `affects => member` requires live membership anyway.

Promotion is per-entity lease acquisition over the cell's active-class
entities - D7's ordinary `Claim` flow, with the registrar assigning hosts for
ruleset-flagged **active entities** exactly as docs/04 section 7.1 already
specifies for NPC hosting (`docs/04-authority.md:538-549`). Release is
lease expiry/parking plus quiesce-flush. Nothing in the state machine needs
a new kind of transfer.

### 4.3 The walker: a second activator, and a ruleset participant or nothing

The owner's design, sharpened over two rounds: the walker creates
macroeconomic movement and resource distribution **where no active process
would otherwise take place** - in active areas, player + NPC action needs no
thumb on the scale, and the cluster's only cost there is persisting
peer-computed outcomes. Its pattern is sparse and read-mostly: many cold
reads (a town's shop inventories), then activate one cell, update, release.
Assessed against the tree:

- **Its authority is ordinary.** The walker takes registrar leases like any
  holder - the NPC-hosting assignment role of docs/04 section 7.1, executed
  by infrastructure, which ADR-0040 explicitly permits (`0040:358-360`).
  When a player arrives mid-pass, the implemented drain machinery applies:
  `CellRuntime::begin_handover` / `quiesce_handover` / `complete_handover` /
  `abort_handover` and `GatewayServer::drain_shard_for_handover` are built,
  with the decision layer deliberately absent - "a handover is invoked,
  never scheduled" (`docs/08-persistence.md:2086`;
  `crates/orrery_persistd/src/runtime.rs:991`, `gateway.rs:5405`;
  `handoff_deadline_ms` = 300, `gateway.rs:3172`). The walker supplies
  precisely the missing piece: a *scheduling policy*. One genuine protocol
  gap: weak claims require a live coordinator-interest snapshot for the
  entity's cell (docs/04 implementation-status note), which a walker in
  peerless space does not have - it needs an infrastructure claim basis, a
  small but real protocol change. Earlier framing that placed the walker's
  fence at the `actor/` row is corrected: that row fences storage-actor
  placement between persistd nodes (`docs/08:2014`); the walker's fence is
  the registrar.
- **Batch at shard granularity.** One activation per shop is churn; the
  natural pass is per shard - the fence, the checkpoint load, and the lease
  index ranges are all shard-shaped (`keyspace.rs:361`). Measured cost of
  the shard machinery: 128-shard activation took 386 ms of fencing plus
  63 ms of recovery, ~3.5 ms per shard, with startup scaling as one fence
  read plus one checkpoint load per shard (`docs/08-persistence.md:2018`).
  A walker pass over a shard is that, plus its cold reads (0.1-1 ms range
  scans), plus one `Elapse` adjudication per touched cell (below), plus a
  quiesce-flush - milliseconds to tens of milliseconds per shard, dominated
  by whatever the ruleset's elapse computes.
- **Read-then-write is a real hazard, honestly sized as rare.** Between the
  walker's cold survey and its activation, a player may have activated the
  cell and emptied the shop - the naive loop is wrong exactly under
  contention. `read_cold` returns a `SnapshotPage` with the shard epoch but
  no read version (`checkpoint/mod.rs:130-141`), so the survey alone cannot
  be trusted at write time. Invariant: **a walker's writes may depend on
  survey reads only if those reads are revalidated inside the activation's
  own fenced transaction** - re-read (or conflict-range) the decided-upon
  rows in the transaction that claims the leases, and abort/reschedule on
  change. FDB's strict serializability makes this cheap; the skip-active
  bias makes it rare (the walker is not *after* contested cells); the
  arrival edge is the one case, and the drain deadline bounds it.
- **The walker's writes go through the ruleset, or the design is
  indefensible.** A row rewrite that bypasses adjudication is an authority
  outside replay: unwitnessed, unattributable, in a system where even a
  neighbor *read* is recorded into the replayed read-set
  (`crates/orrery_core/src/ruleset.rs:131-134`) and the core gates refuse
  nondeterminism in ruleset code (`scripts/core-gates.sh`, AGENTS.md). And
  "run the ruleset at a coarse tick" is not literally available: dt is a
  fixed constant of the contract (`TICK_HZ = 60`,
  `crates/orrery_core/src/executor.rs:27`; Regolith derives
  `DT = 1/TICK_HZ`, `mod.rs:28`), so a big-dt step would be a different
  ruleset. Two lawful forms remain: **(a) burst-ticking** - simulate real
  ticks quickly during the activation window; deterministic and replayable,
  cost proportional to simulated ticks, sensible for short catch-ups and
  absurd for six hours of shop economics; **(b) an explicit `Elapse
  {duration}` ordered event** - the ruleset defines, deterministically, what
  a duration of unattended time means for this entity class; one event, one
  adjudicable record, testable in the same golden-determinism harness as
  every other rule. (b) is the recommended shape, and it *is* the owner's
  engine/game split: Orrery decides when and where to walk; the ruleset
  defines what elapsing means - the same division docs/08 section 10.1
  already uses, where the Ruleset decides per-section demotion policy and
  the engine enforces it. It also directly serves risk 1 below: elapse
  semantics being the ruleset's own is what makes walker outcomes plausibly
  continuous with live-tick outcomes, as a design obligation with a test
  surface rather than a hope.
- **Eligibility is hysteresis at a longer timescale, and the pattern
  exists.** Terrain demotion already requires quiescence sustained for
  `promote_demote_after` (5 s default) plus absence from every high-rate
  interest set, with a 10 s cooldown against thrash
  (`docs/08-persistence.md:3340-3341`). Walker eligibility is the same
  shape, minutes-to-hours: long enough that lease TTL expiry (10 s), island
  drain, and quiesce-flush have all settled (so the at-rest rows are exact
  before the walker surveys them), and long enough that the boundary is not
  trivially farmable - which is risk 2, and it is a knob, not an accident:
  **leave-so-it-restocks is designed-in**, and the hysteresis window sets
  exactly how exploitable it is. Name it in the walker's record; do not let
  players discover it first.
- **The activation-budget contention this node previously designed for is
  largely moot** - the walker skips active areas by definition, so walker
  and players are not after the same cells. What remains is the arrival
  edge (player enters mid-pass: the drain preempts the walker inside
  300 ms) and the global budget of 4.4, which the walker consumes as the
  deferrable customer: a walker pass yields to any player promotion, always,
  because staleness is its product and patience is free to it.
- **Load flatness, tested.** The claim: total simulation load is roughly
  flat across population. The cost terms: in active areas the cluster pays
  ingest and checkpointing of peer-computed outcomes - linear in players and
  already the sized baseline (`docs/08` section 13: 10 k CCU = 100 k diff
  records/s, 3-4 persistd nodes). The walker's term is *deferrable batch
  work*: spend S buys revisit interval T ~ (at-rest cells worth walking x
  per-cell cost) / S, and nothing breaks when S is small - the empty world
  just moves more slowly. So the honest statement is not that load is flat
  but that **the walker converts "simulate the unattended world" from an
  obligation into a dial**, whose spend is chosen, is independent of player
  count, and declines as players activate more of the world themselves. No
  cost term breaks this: cold reads are microseconds-to-milliseconds,
  activation is ~3.5 ms/shard of fencing plus ruleset time, and every one
  of them is schedulable. The term that grows with success - persisting
  peer outcomes - is the one the cluster is already sized for.

**Scope judgement, as asked: split it.** The walker is its own proposed ADR,
not an A14 section: it adds an infrastructure claim basis (D7), a scheduling
policy over the implemented handover machinery (D26-adjacent), an `Elapse`
order class with per-ruleset semantics (D46-adjacent), a witnessing posture
for cluster-executed adjudication (D9/D10 - who witnesses the walker, or
does it inherit the field-host trust position?), and the
seam-continuity/farmability design obligations above. A14 carries what makes
the activation path legible - the dichotomy, the promotion flow, the
ruleset-participation constraint - and the summary tier of this node is
buildable without the walker entirely: where none runs, at-rest cells simply
age until someone visits. The split is a recommendation only; scoping
records is owner-reserved.

### 4.4 Backpressure: diegetic rate limiting, adjudicated commitment, and a global bound that refuses

The owner: peer-backpressure activations at N/s, and plaster the limit over
in UX - make the zoom animation slower and non-cancellable. The instinct is
right and this node endorses it as the *product form* of H6: **the animation
duration is the cooldown**, so the rate limit is never experienced as a
refusal. Two corrections it must carry:

- **Non-cancellable is a ruleset fact, not UX polish.** Being committed
  while the scope spins up is a tactical vulnerability - gameplay - and if
  the cooldown were only a client-side animation, a modified client cancels
  instantly and re-scans: the client is the adversary. The commitment must
  be adjudicated under simulation authority. The tree already contains the
  precedent shape: Regolith's target lock is a half-second *held*
  commitment with decay, adjudicated in the ruleset
  (`LOCK_ACQUISITION_TICKS` and the break/decay derivation,
  `crates/orrery_games/src/regolith/mod.rs:70-78`). Propose the same shape:
  a `ScopeSpinUp` ordered event opens the commitment (the craft is
  scoped-in: turn-limited, or whatever the game prices it at), the interest
  shape and any promotion request take effect only when the hold completes,
  and breaking the hold is itself an adjudicated transition. Once
  adjudicated it is also symmetric and fair - visible to the target's
  ruleset the way a lock is - which is better design, not just better
  anti-cheat.
- **Per-peer backpressure does not bound the cluster.** N/s x P peers is
  the real offered load, and griefing is a coalition problem: a thousand
  accounts each under their individual limit still melt the cluster. The
  house lesson is measured and in the tree: at ~100% utilisation a standing
  queue never drains - "neither a larger concurrency cap nor a faster route
  removes a standing queue; only destroying work does"
  (`docs/08-persistence.md:103`). So the global bound must **refuse**
  promotions, not buffer them: a cluster-wide admission budget on
  activations/s, over which a promotion request is denied with a retry
  hint - which the diegetic layer renders as the scope taking longer to
  resolve, the same honest trick at a second scale. Reads (4.1) are bounded
  separately and much higher; only promotions cost simulation. The walker
  consumes the same budget as its strictly lowest-priority customer (4.3).

## 5. The summary read, specified per state

### 5.1 Sources

**At-rest fold** - `world/` rows, cell-ordered; one shard's subtree is one
contiguous range (`world_range_start`/`world_range_end`,
`crates/orrery_persistd/src/keyspace.rs:48`/`:68`); `WorldCensus` is the
read-only fold precedent (`crates/orrery_persistd/src/census.rs:28`, observe
at `:43`). Cost is proportional to populated rows, never volume - a sorted
KV materializes only written keys (`docs/01-spatial-model.md:9`) - which is
what kills the volumetric argument at the storage tier. Campaign scale: tens
of rows. Solar-grid scale (`docs/12-world-seeding.md:149-155` gives the
extent; the seeding ladder reaches 10 M entities, `docs/12:785`): a one-time
census-shaped full pass, then incremental maintenance, because at-rest
counts change only through intents, seeding, and activation boundaries.
Serving a cached per-shard count is bytes. Products: prospecting, economy,
wreck fields - at any resolution H5 permits, down to per-entity, exact and
stamped with its per-cell age since last write (H3; via the checkpoint
watermark until a row-level tick exists, 3.2).

**Lease fold** - the activated half's source, and it exists today for actor
restore: `lease-cell/{grid}/{cell}/{entity}` (`keyspace.rs:338`), with
shard-subtree range helpers (`lease_cell_range_start`/`end`,
`keyspace.rs:361`/`:370`) - **a per-shard-cell count of held entities is one
contiguous range scan of an index already being written.** Freshness follows
the lease protocol: TTL 10 s, heartbeat 2.5 s
(`crates/orrery_authority/src/lib.rs:38-40`), location updated on rekey. It
is trustworthy where a reported list is not: leases are *issued* by the
registrar (CAS, in persistd - `docs/04-authority.md:3`), not asserted by
peers, and the gateway enforces the fence on every uplink (INV-1). A peer
cannot fabricate a contact into the index without winning a grant for a real
entity whose cell its interest plausibly covers. The count inherits the
grant path's trust properties; no new surface opens. Verified limits: rows
appear on first arbitration (the seeder writes none; the P2 rig claims its
own, `crates/orrery_seed/src/verify.rs:237-241`), and island ephemerals
never appear (`ephemeral.rs:1-11`) - both limits point at the at-rest fold,
which covers the complement. A walker holding leases appears in this index
like any holder, so walker-activated cells are covered with no extra work.
Products: threat halo, strategic map - craft, NPCs, drifting rocks.

### 5.2 H4, done honestly - the arithmetic the brief asked for

The bound is `age >= E / v_max` so positional uncertainty exceeds resolution
on arrival. At v_max = 120 m/s: E = 512 m (campaign interest cell) needs
>= 4.3 s; E = 4 096 m (campaign shard cell) needs >= 34.1 s. The hoped-for
freebie - checkpoint lag enforcing this - **fails structurally**: the 20 s
jittered cadence (`docs/08-persistence.md:79`,
`docs/adr/0016-parameter-reference.md:14`) is a *maximum* age; the minimum
age of a scanned row is the bulk-uplink lag, 0.25-1 s at the 1-4 Hz uplink
(`docs/03-replication.md:182`), and the lease index is fresher still. Even
the *mean* row age (~10 s) misses the shard-resolution bound by ~3x.
Enforcement therefore lives in the serving fold, and is cheap: fold at
cadence F, double-buffer, serve the *previous* fold - delivered age is in
[F, 2F), with F chosen per resolution (F = 5 s for interest-cell products,
F = 35 s for shard products). One rule, one place, testable. At-rest content
is exempt (v_max = 0): it is served exact, with its stamped age.

### 5.3 H5 at fold time: persistd is game-blind, so the gate must be structural

A persistd fold sees every row, but the actor never decodes game types
(`docs/08-persistence.md:1999`) - it *cannot* apply a ruleset's hiding rule
at fold time, and neither can the registrar for lease rows. Two structural
enforcements, both proposed:

1. **Key-family separation for regime-3 secrets.** ADR-0040 regime 3 puts
   unrevealed state cluster-side (`0040:98-102`); peers cannot hold it, so
   it never arrives via the peer uplink. Give it its own subspace that no
   fold scans. Fail-closed: the fold cannot leak what its range never
   covers. A walker adjudicating secret-bearing content writes into that
   family, which is one more reason the walker is infrastructure (4.3).
2. **An at-rest visibility class in the versioned value envelope** for
   revealed-but-restricted state, beside the schema floor that already
   lives outside the bag precisely so persistd can read it without decoding
   game types (`LIVE_VERSIONED_TAG`,
   `crates/orrery_persistd/src/keyspace.rs:130` region). The uplink stamps
   it under ruleset authority; folds count only classes marked aggregable.
   Default for every existing row: public - matching the fully-public
   regime-1 world of today, so nothing changes until a game introduces
   secrecy, and the record introducing regime 3 must introduce the class.
   The same envelope extension is the natural home for 3.2's last-write
   tick.

The same blindness argument is decisive against every peer-side deliverer:
only a party that can see the class bit and honour the secret-family
separation can enforce H5 at all. H5 does not relax for at-rest cells: a
reveal-gated entity in a quiet cell is still hidden, and rest answers
staleness questions, never secrecy ones.

## 6. Deliverers, re-ranked for an entity-shaped product

| candidate | verdict | change from A13 |
|---|---|---|
| A. Client sighting memory | build now | unchanged |
| B. Coordinator peer map | withdrawn as the experiment | A13 section 7's own self-criticism, confirmed by the owner: it measures peers, the product is entities |
| B'. Coordinator entity list | **do not build** (6.1) | new candidate, rejected |
| C. Authority-published aggregates | **dissolved** (6.2) | the publication already exists; C and E collapse |
| D. Mesh gossip | do not build | rejection *strengthened* (6.3) |
| E. Persistd folds (5.1) | **the deliverer** | promoted from "slow products later" to the tier proper |

### 6.1 The coordinator-with-entities shortcut, weighed seriously

A reader will think of it: `Presence` already maps peers to cells; add
entities. Three verified objections:

- **Self-report becomes world-report.** `report_presence(node, cells)`
  (`registry.rs:216`) is a peer's claim *about itself*, made consequential
  only through grants that are "a lease on being believed" with a 60 s TTL
  (`registry.rs:38-41`, `:59`) and fenced against stale replay by
  `interest_epoch` (`registry.rs:112-115`, bumped at `:235`). The fence
  bounds replay, not lying - but a coverage lie is self-limiting: it widens
  only the liar's own sight/claim plausibility, it is visible to every
  island member in the manifest (`crates/orrery_protocol/src/coord.rs:72-78`),
  and acting on it runs into the registrar and the witnesses. An entity
  list is a claim about *world contents*, consumed by everyone's map, with
  no counterparty positioned to notice and no analogous fence. The only fix
  would be to witness it - rebuilding, at the coordinator, the trust
  machinery the uplink and registrar already are.
- **Shape and scale.** `Presence` is a `HashSet<CellId>` at shard level
  (`registry.rs:108-116`) - a few dozen bytes per peer, in memory,
  rebuildable from re-announcements after an outage
  (`docs/02-networking.md:318`). An entity list is bounded by world
  population and churned by every spawn, despawn and handoff: at the
  docs/08 section 13 sizing, 10 k CCU is 40 k hot entities and **100 k
  diff-records/s cluster-wide** - against a service whose cadence is one
  presence report per ~10 s per peer. Free at campaign scale (37 windows);
  a second replication tier at the scale the architecture exists for. It
  fails exactly where the design must not.
- **Role creep, and a posture that cannot survive it.** The coordinator's
  job is turning coarse presence into islands (`docs/02-networking.md:126`);
  it is deliberately not in the packet path, and its outage is "degraded,
  not dead" *because* peers can rebuild its state by re-announcing
  (`docs/02:318`). Peers cannot re-announce the world's entity contents -
  they do not know them beyond their AOI. An entity-holding coordinator
  loses the rebuild-from-nothing property that justifies its failure story.

The salvageable intuition - counts that are *issued* rather than claimed -
is the lease fold (5.1), in persistd, where the registrar and the index
already are. The coordinator's one contribution is the routing oracle it
already is: which shards can hold peer leases at all.

### 6.2 C dissolves into E

A13's candidate C had authorities compute and publish per-cell folds on a
new record at a new cadence, with a new self-report trust surface. But the
authorities already publish exactly the fold's input: the bulk uplink ships
every live entity's state to the cluster at 1-4 Hz (`docs/03:182`), fenced
by lease ids, feeding actor memory and checkpoints. A second publication
channel would duplicate this one, minus its fencing. The fold is a
*read-side projection* of a publication that already ships - and A13's trust
question for C ("what does a lying authority cost us?") collapses into the
existing uplink trust surface, which witnessing already patrols. Nothing new
to believe.

### 6.3 Gossip: the entity reframing strengthens the rejection

A13's grounds were structural: no overlay beyond the island, a shedding
discipline that assumes replication is the only sheddable load
(`crates/orrery_net/src/budget.rs:21-26`), no attribution, no H5. All hold.
The entity reframing adds three: (1) entity aggregates are bulkier and churn
faster than peer presence, on links that still do not exist; (2) H5 is now
shown to require a party that can read the at-rest class bit and honour the
secret-family separation (5.3) - a relaying peer can do neither; (3) with
NPCs, a gossiped fold of a cell is the *current lease holder's* self-report
of entities it simulates - rumor about rumor, churning at every handoff.
Expectation confirmed: stronger, not weaker.

### 6.4 What survives of A13, what is sharpened, what was wrong

**Survives:** the hearsay concept and name; H1, H3 (and H2, restated below);
the four-product decomposition (A13 section 1's table); the anti-gossip
verdict; client sighting memory as step one; the island-manifest exposure
audit (`coord.rs:72-78` still ships every member's cells); "no per-entity
beyond-AOI feed, ever".

**Sharpened:** H4 (from "coarser and staler" to an enforced serving-side age
floor with the fold as the single enforcement point, and v_max corrected
from 32 to 120 m/s); H5 (from "enforced at the source" to two named
structural mechanisms); "the archive can serve the slow products" (from a
slow-lane consolation to the deliverer proper, exact rather than stale for
at-rest content); staleness-is-a-contract (now derived: floor E/v_max for
activated content, stamped true age for at-rest content, ceiling the
product's tolerance).

**Wrong, and named as wrong:** the B-first experiment ordering - the demand
question dissolves under the cost framing, and the instrument measured peers
where the product is entities (A13 said this itself; the owner confirmed
it); "H4 may come free from checkpoint lag" (a cadence bounds age above, not
below); the implied premise that the summary is a new delivery subsystem (it
is a read over existing paths); any "static furniture is exact, only craft
lag" split (exactness is per-entity writer state, 3.3); and, from this
node's own drafting, the walker-as-a-tier trichotomy (rejected in 3.2 - the
walker is an activator, and "at rest = exact" survives in full).

## 7. H1-H6, kept and amended

Amendments are named as amendments; the rest restates A13 normatively.

- **H1 (unchanged).** Hearsay is never a simulation input; the ruleset may
  not read it. A sensing *mechanic* is simulation authority producing
  events - and with section 2 landed, its interest shape and its spin-up
  commitment (4.4) are simulation-side artifacts too.
- **H2 (amended - boundary clarified).** Hearsay never gates membership or
  rate, in either direction. *Amendment:* ruleset-authored interest shapes
  and adjudicated promotions are the legitimate membership channel and are
  not hearsay; the forbidden move is any inference from summary possession
  to replication behaviour.
- **H3 (unchanged in force, sharpened in mechanics).** Source- and
  age-labeled end to end; the skin renders age. For at-rest content the
  label is the true per-cell age since last write - currently derivable
  only at shard granularity from the checkpoint watermark; a row-level
  last-write tick in the versioned envelope is the proposed fix (3.2, 5.3).
- **H4 (amended - enforcement point fixed, exemption named).** Resolution no
  finer than the product's declared cell; delivered age >= E / v_max with
  v_max the ruleset's declared speed cap. *Amendment:* cadences are
  ceilings, not floors - the floor is enforced by the serving fold via
  double-buffering; at-rest content is exempt (v_max = 0) and served exact
  with its stamped age.
- **H5 (amended - mechanism named).** Reveal-gated state never leaks into
  aggregates, not even as a count - and rest does not relax it.
  *Amendment:* enforced structurally - regime-3 secrets live in a key
  family no fold scans, and restricted revealed state carries an at-rest
  visibility class outside the component bag; folds count aggregable
  classes only. Peer-side aggregation is disqualified because it can do
  neither.
- **New, H6 (proposed - the read/promotion boundary).** *A summary read
  leaves the world unchanged*: no key written, no lease minted, no `actor/`
  CAS, no tick advanced, no ruleset-observable event; per-client read cost
  bounded before dispatch. Promotion is a distinct act: requested
  explicitly, committed under an adjudicated ruleset hold (the diegetic
  spin-up, 4.4), bounded per peer *and* by a cluster admission budget that
  refuses rather than buffers (`docs/08-persistence.md:103`); the walker is
  that budget's lowest-priority, infinitely patient customer.

## 8. What to build, in order, with costs

1. **Client sighting memory** (A13 step 1, unchanged; #533's fade
   substrate). No protocol change.
2. **The lease fold and summary query.** Per-shard counts over
   `lease-cell/` (index and range helpers exist), double-buffered per H4,
   served on the gateway's control surface behind a per-client bucket.
   Small: a fold task, a query pair, a snapshot encoder. *Honest
   under-delivery:* the campaign spans ~2x2 shard cells (5 km orbit
   diameter vs 4 096 m shard edge) - at campaign scale the map is four
   numbers. It proves plumbing and H4/H6 discipline, not product appeal; do
   not read its reception as demand evidence - that is the mistake A13's B
   experiment was built on.
3. **The at-rest fold, and the read-quota fix.** Per-shard counts (later,
   class histograms) over `world/`, incremental after a census-shaped first
   pass, age-stamped from the checkpoint watermark, H5-classed the moment a
   class exists - and close the `Subscribe` validation gap (4.1), which is
   worth doing independently of everything else here.
4. **The interest-shape record and scoped membership** (section 2): shape
   record with sight-only grant coverage, aim-aware scoring landing docs/03
   section 4.1 into `interest.rs`, and the adjudicated spin-up commitment
   (4.4) as the shape's activation gate. Moderate. Its first honest
   consumer is already in the tree: Heavy's 900 m reach vs the 460.8 m
   guarantee (section 1); the alternative on record is another edge
   doubling at 8x volume.
5. **A campaign strategic layer** rendering 2+3 over 1 - the experiment A13
   wanted but could not run, now showing entities: NPC craft and drifting
   rocks from the lease fold, the seeded rock pocket from the at-rest fold
   (H5 permitting).
6. **The walker, as its own proposed ADR** (4.3): an infrastructure claim
   basis, a scheduling policy over the implemented handover machinery, the
   `Elapse` order class and its determinism obligations, a witnessing
   posture for cluster-executed adjudication, the eligibility-hysteresis
   timescale (with its farmability knob named), and the seam-continuity
   obligation. This node's tier does not depend on it: where no walker
   runs, at-rest cells simply age until someone visits.

Not build, unchanged in conviction: mesh gossip; any per-entity beyond-AOI
feed; a coordinator entity list; ruleset-readable summaries; a
witness-derived map (`StateClaim`s are hashes by design,
`crates/orrery_protocol/src/verifiable.rs:200`); activation as a side effect
of any read; and any walker write that does not pass through the ruleset.

## 9. Strongest argument against

**The cost law is real, but nothing shipped pays it yet - so this node
builds capability ahead of a consumer, which is the same sin it convicts A13
of, one level up.** The 2 km scope is hypothetical; the campaign's only
over-reach is Heavy's 900 m, which a single further edge decision could
absorb (1 024 m edge: guarantee 921.6 m - still 21.6 m short of Heavy's
reach, note, and 8x the volume); the strategic map at campaign scale is four
numbers; and the walker proposes standing (if deferrable and dialed)
cluster simulation spend in a design whose economics put simulation on
player machines precisely to avoid that spend - plus two designed-in
gameplay consequences (the fidelity seam and the restock-farming boundary)
that a skeptic will call self-inflicted. A skeptic can fairly say: close the
`Subscribe` quota gap (a real finding, useful today), fix Heavy by whichever
cheap means the owner prefers, and build nothing else until a game asks -
and if a game does ask for living empty worlds, the walker should be
justified by *its* economy design in its own record, not by this node.

The reply is cost asymmetry plus an invariant, not demand: steps 2 and 3
are reads over indexes and rows that already exist, priced in days, and
they are the only path that satisfies H4/H5 at all if any summary is ever
wanted; step 4 is ADR-0040's compliance mechanism for *any*
reach-past-the-AOI mechanic, of which the tree already contains one; and
the walker is explicitly deferred to its own record for exactly the
skeptic's reason. But the concession is genuine: if the owner reads Heavy
as a bug to shrink rather than a mechanic to serve, and wants no scan
product, then only the quota gap survives this node as work - and that
outcome should be reached by deciding, not by drift.

## 10. What could not be verified, and findings

- **The `Subscribe` validation gap is a finding, not a proven
  vulnerability.** No check of client-requested cells against location or
  grant is visible at the gateway arm (`gateway.rs:6532-6571`), and the
  prefix-admitting reader makes `CellId::ROOT` a covering scan (`docs/08`
  section 9). An outer layer restricting the set may exist and was not
  found; this needs a deliberate audit. Reported in the PR body per scope
  discipline; no code changed here.
- **`world/` rows carry no per-row write time** (live values are tag +
  bag, `keyspace.rs:108-120`); H3's age stamp for at-rest content is
  therefore shard-coarse (checkpoint watermark) until an envelope
  extension lands. This is a gap between what H3 requires and what the
  storage can currently say.
- **The Heavy-vs-AOI incoherence** (section 1) is arithmetic over verified
  constants, but whether campaign *play* reaches it (pickup uptake, NPC
  behaviour at range) was not measured, and `CAMPAIGN_CELL_EDGE_M`'s
  comment deliberately scopes itself to stock reach, so it may be a known
  deferral.
- **The walker is costed structurally, not numerically.** The shard-fence
  and drain figures are measured (`docs/08:2018`, `docs/08:2057`); the
  per-cell `Elapse` cost is the ruleset's and unknowable here; the
  eligibility timescale, the admission budget's value, and the witnessing
  posture belong to its proposed ADR. The claim that D7's active-entity
  assignment extends to an infrastructure holder is a reading of
  `docs/04-authority.md:538-549`, not landed behaviour; the missing
  infrastructure claim basis is a real protocol change; and the
  `Elapse`-vs-burst-tick choice was argued, not prototyped.
- **Whether the campaign client exercises the gateway path at all** was not
  established; step 5's demo depends on where persistd actually runs in
  campaign sessions.
- **Presence cadence** remains derived, not literal: 60 s grant TTL = "six
  presence intervals" (`registry.rs:38-41`, `:59`) implies ~10 s; no
  constant exists to cite.
- **Lease-row lifecycle at the margins**: that rows appear only on first
  arbitration is inferred from the seeder writing none and the P2 rig
  claiming its own (`verify.rs:237-241`); an explicit statement of when a
  parked row's location-index entry is created or removed was not found.
- **FDB scan throughput** for the one-time at-rest census at 10 M rows is
  estimated from the write-path figure (2.8 min, `docs/12:841`) and the
  0.1-1 ms read-latency claim (`docs/08` section 9), not measured.
- **External systems** (Astrolabe/SDIMS fold shape, DIS aggregate PDUs,
  EVE's map, SpatialOS QBI): inherited from A13 section 5 with its
  confidence labels, not re-fetched.
- **Owner-reserved decisions touched:** accepting any hearsay/H-rules
  record (including H6); D40's fate (this node leans on its invariant while
  it is Proposed); the at-rest envelope extensions (visibility class and
  last-write tick would amend the D38 scheme); the interest-shape record
  and the spin-up commitment; the walker ADR and its scope; and Heavy's
  resolution. All proposals only.
