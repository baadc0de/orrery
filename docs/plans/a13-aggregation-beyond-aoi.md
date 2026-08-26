# A13 — Aggregation beyond the AOI: who should tell you about the world you cannot see

> Speculative research requested by the owner: *"some, maybe most games, will want a
> summary, read-only view of data like entity positions outside of their subscribed
> AOI. This could be stale, low frequency best effort data. Who would be best poised
> to deliver this — is it a backend service, is it gossip through the mesh, or
> something else?"* No such requirement is accepted today. This node says what it
> would cost, what it would buy, and what should not be built. Repository facts
> verified 2026-08-27 at `origin/main`; external claims carry a source or an explicit
> needs-verification flag. Nothing here amends an accepted record — **propose, not
> decide.**

## Verdict up front

**The summary view is a third kind of knowledge, and naming it is most of the
answer.** Simulation authority says what happened (#519); visual authority says how
it is shown; a beyond-AOI summary is neither — it is **hearsay**: non-authoritative,
source-labeled, age-stamped knowledge that the ruleset may never read, the client may
never act on mechanically, and the evidence path never banks. Once those rules are
stated, the delivery question mostly answers itself, because only two parties can
serve hearsay without breaking them.

**The coordinator is the best-poised deliverer, and it is already doing 80% of the
work.** It maintains cell-granular presence for every peer
(`crates/orrery_coordinator/src/registry.rs:106`), aggregates it into islands, and
already hands every island member a manifest naming every other peer's cells
(`crates/orrery_protocol/src/coord.rs:73-78`) — a beyond-AOI summary that ships
today, unremarked. A read-only per-cell population map is one query pair away, costs
under 0.1% of the peer upload budget, and adds no new trust surface because peers
already trust the coordinator for exactly this data.

**Gossip through the mesh should not be built.** Orrery's topology deliberately does
not maintain a connected overlay beyond the island — islands are "connected sets of
populated cells plus the peers in them" (`docs/02-networking.md:126`) — so epidemic
dissemination would first have to *create* the links it runs on, against a ≤1 Mbps
budget whose whole shedding discipline assumes replication is the only sheddable load
(`crates/orrery_net/src/budget.rs:21-26`). Worse, gossip is structurally unable to
enforce reveal-gating for hidden state (ADR-0040 regime 3), and it manufactures an
unattributable rumor channel in a system whose entire apparatus exists to make every
claim attributable.

**The cheapest experiment is two small pieces, neither of which touches the mesh:**
client-side last-sighting memory (fog of war proper, serving #533's fade), and a
coordinator map query returning per-shard-cell peer counts. Build those, put a
strategic layer in the campaign client, and measure the question that actually
decides the architecture: do players want to know where *peers* are, or where
*entities* are? Only the second answer justifies the expensive tier (authority-
published aggregates), and nothing today shows it is needed.

## 1. What the requirement actually is — because "a summary view" is not one

"Entity positions outside the AOI" bundles four different products with different
resolutions, staleness tolerances, and threat models. Separating them is the first
real act of specification:

| product | content | resolution needed | staleness tolerable | consumer | today's status |
|---|---|---|---|---|---|
| **Edge continuity** | the craft that just left scope keeps existing visually | last authoritative transform | seconds (it is *memory*, not data) | the skin's fade (#533) | client already holds it — the replica map expires at `REPLICA_TTL_TICKS = 120` (`clients/regolith/src/campaign.rs:78`) |
| **Strategic map** | population density / activity heat per coarse cell | shard cell (8× interest edge, `crates/orrery_protocol/src/cell.rs:43`) | tens of seconds to minutes | map screen, low zoom | coordinator holds the peer half of it now |
| **Threat halo** | "contacts exist just beyond my edge, roughly there" | 1–2 cells beyond AOI, cell-granular | ~10 s | HUD periphery | nothing |
| **Prospecting / economy** | resource distribution, wreck fields, market activity | shard cell or coarser | minutes to hours | planning screens | derivable from the archive (`crates/orrery_persistd/src/census.rs:1-6` is the read-only precedent) |

Two sharp observations fall out:

1. **Nobody in this list needs positions.** Every product above is served by *counts,
   density, or memory*. Per-entity positions beyond the AOI are the one thing the
   owner's phrasing mentions and the one thing no concrete consumer requires — and
   they are also the only version that is a wallhack (§6.4). The requirement, read
   charitably, is for **aggregates**, and the piece should hold that line: the moment
   a "summary" carries individually resolvable entities it has stopped being a
   summary and become replication at a discount.

2. **Staleness is not one number; it is a per-product contract.** A12 §5.3 already
   landed this lesson the hard way — three uncoordinated staleness constants produced
   F-C (`docs/plans/a12-exchange-systems-shakedown.md:317-346`). A summary tier that
   introduced a fourth ad-hoc constant would repeat the mistake. Each hearsay product
   must declare its age bound, and carry its age on the wire.

### The geometry that makes the question live — and what it does not excuse

The 27-cell AOI spans 3 cells per axis; from the observer's cell center the boundary
is 1.5 edges to a face and `1.5·√3 ≈ 2.6` edges to a corner. At the P1 default
128 m edge (`crates/orrery_spatial/src/config.rs:15-16`,
`crates/orrery_protocol/src/cell.rs:58`) that is **192 m to a face** against a 400 m
campaign weapon — the #520 incoherence, fixed by PR #532's campaign-scoped 512 m
edge (open at the time of writing; not yet in this tree — verified by grep: no
`CAMPAIGN_CELL_EDGE_M` at `origin/main`), which puts the nearest face at 768 m.

**A summary tier must never paper over an interest set that is sized wrong.** The
invariant PR #532 restored — *the interest set contains everything in interaction
range* — is prior to everything in this document. ADR-0040's proposed form is
`affects(e,o,t) ⇒ member(e,o,t)`
(`docs/adr/0040-visibility-and-spatial-query-layering.md`, "The governing
invariant"); D40 is proposed, not accepted (`docs/DECISIONS.md:79`), but even as a
proposal it names the boundary correctly: hearsay begins strictly *beyond* the
correctly-sized membership set. A coarse blip is not a substitute for a replica of
something that can shoot you, and any design in which the summary's existence relaxes
the pressure to size the AOI correctly should be rejected on that ground alone.

## 2. What already exists, and the three things hiding in plain sight

An inventory, because the answer changes depending on what you think is missing:

- **The address space for aggregates already exists.** `CellId` is hierarchical:
  levels 0 (root) to 21 (interest), parent-is-a-prefix, coarsening is a shift
  (`crates/orrery_protocol/src/cell.rs:7-27`), and `ancestor_at`
  (`cell.rs:250`) plus the shard level (`SHARD_LEVEL = INTEREST_LEVEL − 3`,
  one shard = 8×8×8 interest cells, `cell.rs:43`) mean "the coarse cell containing
  this point" is a one-instruction question at every granularity from 128 m to a
  whole grid. An aggregation tree does not have to be designed; it is the ID scheme.

- **The coordinator already computes a global-ish aggregate.** It tracks coarse
  presence — "which cells it occupies" (`registry.rs:106`) — refreshed at the D16
  cadence (six intervals per one-minute grant TTL, `registry.rs:38-41`, i.e. ~10 s),
  can answer `peers_covering(cell)` today (`registry.rs:180`), and exposes
  `interest_snapshot` (`registry.rs:339`). It is deliberately not in the packet path
  and rebuilds from re-announcements after an outage (`docs/02-networking.md:318`) —
  exactly the durability posture a best-effort map wants: when the coordinator is
  down, the map goes stale; the game does not.

- **A beyond-AOI summary already ships, unremarked.** Every island manifest lists
  every member peer with its cells (`coord.rs:73-78`), and `visibility.rs` derives
  each client's AOI from it (`crates/orrery_spatial/src/visibility.rs:8-15`). An
  island member therefore *already* learns the cell-granular whereabouts of every
  peer in the island, however far outside its own 27 cells — at presence cadence,
  128 m resolution. This is both a precedent ("we already do this and nothing broke")
  and a live exposure to audit (§6.4): the manifest is interest-set plumbing, and its
  information content was never assessed as a map.

- **The client already holds the fog-of-war half.** The campaign replica map keeps
  last-received transforms and expires them after 2 s
  (`campaign.rs:78`, `campaign.rs:1172`). #527 proved expiries fire only after the
  host stops replicating. Extending *presentation* memory past the replication TTL —
  a faded "last seen here, n seconds ago" ghost — needs no protocol change at all,
  only the discipline that the ghost claims nothing (#519, #533's own constraint
  list). The replica TTL and the sighting memory must stay separate concepts; #533
  says this explicitly and A12 §5.3 says why.

- **The archive can serve the slow products.** `persistd` checkpoints at 20 s
  jittered (`docs/adr/0016-parameter-reference.md:14`), and `WorldCensus` is an
  existing read-only scan over live `world/` rows grouped by grid
  (`census.rs:15-35`). Witness `StateClaim`s are deliberately *hashes, not
  snapshots* (`crates/orrery_protocol/src/verifiable.rs:196`), so the witness stream
  itself carries no positions to aggregate — the archive path, not the witness path,
  is the only offline source of world geometry.

- **The budget machinery already knows how to carry a low-priority extra.** Lanes
  shed replication first and always pass control and witness traffic
  (`budget.rs:21-26`); the priority accumulator sends low-priority state "eventually"
  (`docs/03-replication.md:161`); and the design already prices per-datagram overhead
  at 60 B (`budget.rs:48`). A summary channel is a rounding error against these
  numbers (§4, worked cost).

What does **not** exist: any aggregate of *entities* (as opposed to peers) anywhere
outside a cell authority's own memory; any protocol record for "a summary"; any
notion of a client subscribing to a cell it is not near. Those are the actual gaps.

## 3. Name the thing: hearsay, and the rules that govern it

#519 established two authorities: **simulation authority** (the ruleset's — what
happened, adjudicated under own-state discipline, D46,
`docs/adr/0046-message-class-semantics.md:138`) and **visual authority** (the skin's
— how it is shown, asserting nothing the ruleset has not said). A summary view is a
third thing, and the failure modes all come from letting it impersonate one of the
other two. Call it **hearsay**: what somebody told you about parts of the world you
cannot verify.

Proposed rules (H1–H5), the conceptual contribution of this node:

- **H1 — Hearsay is never a simulation input.** The ruleset may not read it. Every
  neighbor read the ruleset makes is recorded into the replayed read-set (#354's
  layering argument; `StateView::neighbor` appends to `self.reads`), and a
  best-effort, per-client, unordered feed inside that read-set would make replay
  non-reproducible by construction. If a game wants a *mechanic* that senses at
  distance (a scanner, a probe), that is a ruleset fact computed under simulation
  authority and delivered as events — it is not this feature, and this feature must
  not become a cheap substitute for it.

- **H2 — Hearsay never gates membership or rate.** #354's principle — occlusion
  modulates rate, never membership — has a summary-tier corollary: "the client has a
  blip, so we may skip the replica" is forbidden in both directions. Hearsay is
  additive over a correctly-sized interest set; the replication layer must behave
  identically with the summary tier on or off. This is the back door the brief
  worries about, and H2 is the door closing.

- **H3 — Hearsay is labeled with source and age, end to end.** Every summary record
  carries who computed it and when. Not for cryptographic truth — hearsay is not
  evidence and a coordinator signature would prove provenance, not accuracy — but
  because an unlabeled stale datum is how a skin quietly acquires authority (#519's
  named failure). The skin renders age visibly (a faded blip, a timestamp), the way
  #533's fade renders the AOI edge.

- **H4 — Hearsay is coarser and staler than action.** Quantified in §6.4: resolution
  no finer than the shard cell, age at least `E / v_max` so a datum's positional
  uncertainty exceeds its own resolution by the time it arrives. This is the
  anti-wallhack bound, and it is a *protocol* property, not a client courtesy —
  enforced where the aggregate is computed, because the client is the adversary.

- **H5 — Hearsay respects reveal gates at the source.** State that a ruleset hides
  behind a logged reveal (ADR-0040 regime 3, proposed) must not leak into aggregates
  — not even as a count. Only a computing party that already lawfully knows the
  hidden state *and* the hiding rule can enforce this, which is a structural argument
  about who may deliver (§4): a cluster-side service can; a gossiping peer cannot.

## 4. The candidate deliverers, weighed

| candidate | who computes | who pays bandwidth | failure mode | complexity | verdict |
|---|---|---|---|---|---|
| **A. Client memory** (fog of war) | nobody — it is recall | nobody | ghost outlives reality; must visibly age (H3) | trivial | **build now** (it is #533's substrate) |
| **B. Coordinator map** | coordinator, from presence it already holds | coordinator egress + one control-stream reply per query | map freezes on coordinator outage — consistent with the existing "degraded, not dead" posture (`docs/02-networking.md:318`) | small: one query pair, one snapshot encoder | **build as the experiment** |
| **C. Authority-published aggregates** | each cell authority folds its own cells; roll-up along `CellId` ancestors | authorities upload folds; a service or the coordinator serves them | a lying authority poisons the map (unwitnessed by design — hearsay is not evidence, H3); staleness under churn | moderate-to-large: new records, cadence, roll-up service, per-ruleset H5 filter | **defer until B proves demand for entity data** |
| **D. Mesh gossip** | every peer, epidemically | every peer, on links that mostly do not exist yet | budget contention, rumor pollution, unbounded fan-out, cannot enforce H5 | large, and adversarial-hard | **do not build** |
| **E. Archive-derived** (persistd scan) | cluster, offline | cluster read load; client fetches a page | freshness floor = 20 s checkpoint + scan; historical, not live | small-moderate: census-shaped scan, paged serving | **fine later, for the slow products** |

The reasoning behind the extreme rows:

**Why B wins the near term.** The coordinator is the only party that already has a
beyond-island view, already aggregates, already signs handouts peers trust
(`crates/orrery_coordinator/src/interest.rs:1-13`), and is already outside the packet
path so its load and failure cannot touch simulation. Its data is *peer coverage*,
not entities — the honest limitation, taken up in §7 as the strongest argument
against this recommendation. Worked cost, so nobody mistakes bandwidth for the
constraint: a strategic-map reply covering the 27 shard cells around the client
(a 27.6 km cube at the 128 m edge; 110 km at the campaign's proposed 512 m) at
~10 B per cell (id delta + count) is ~270 B payload, ~330 B on the wire with the
60 B datagram overhead (`budget.rs:48`). Refreshed at 0.1 Hz that is **~33 B/s ≈
264 bps ≈ 0.03%** of the ≤1 Mbps peer budget (`budget.rs:1-6`) — and it rides the
reliable control stream, which the meter never sheds. Bandwidth was never the cost;
the cost is a new protocol surface and the discipline of H1–H5, which is why the
rules come before the transport.

**Why D loses on structure, not on taste.** Three independent disqualifications:

1. *No substrate.* Islands are the connectivity unit; peers hold links within their
   island and to nobody else. Epidemic protocols (HyParView's partial views,
   Plumtree's spanning trees) assume a maintained overlay across the whole
   population; here that overlay would have to be built — new links, new keep-alives,
   new NAT traversal — purely to carry data nobody's simulation needs. The system
   already treats unbounded fan-out as a first-class hazard (D25 exists to bound one
   fan-out path with per-recipient token buckets,
   `docs/adr/0016-parameter-reference.md:27`); volunteering a new epidemic fan-out
   against a 1 Mbps budget inverts that discipline.
2. *No accountability.* Orrery's trust model makes every consequential statement
   attributable and re-executable (frames signed, claims chained,
   `docs/06-verifiable-core.md:392`). Gossiped aggregates are anonymous by the time
   they arrive — an adversary seeds a phantom fleet three hops away and no one can
   say who lied. Hearsay being non-authoritative (H1) limits the *simulation* damage
   to zero, but the *product* damage — a strategic map players learn to distrust —
   defeats the feature's entire purpose. A summary you cannot trust at all is worse
   than no summary; players will simply ask in chat.
3. *No H5.* A peer cannot filter hidden state out of an aggregate it forwards,
   because it does not know which distant entities are hidden from *the requester*.
   Only a party holding both the state and the rule can. Gossip fails closed only by
   carrying nothing, i.e., by not existing.

**Why C is real but premature.** C is the only candidate that ever delivers *entity*
density (NPCs, rocks, wrecks — things the coordinator has never heard of), so if the
requirement matures, C is its final form: each authority folds `(count, class
histogram, maybe a centroid)` per shard cell over the entities it owns, publishes at
0.1 Hz on its control stream, and the coordinator (or a thin service beside it)
merges by `CellId` prefix — the fold is associative, so roll-up to any level is one
pass, the Astrolabe/SDIMS shape (§5). But C imports the trust question B avoids:
authorities self-report, hearsay is unwitnessed, and the map becomes a griefing
surface exactly proportional to how much players rely on it. Building C before a
game demonstrably needs entity aggregates would be building the hard 20% first.

## 5. Industry and academia — who solved this, who restated it

The interest-management literature is mostly about the *inside* of the AOI — who
gets full state, at what rate. Read against this specific question ("a stale coarse
view of the *outside*"), it sorts cleanly:

**Actually solved it:**

- **DIS/HLA aggregate entities.** IEEE 1278.1 defines an Aggregate State PDU: a
  platoon replicates as one coarse entity until an observer needs its members, then
  disaggregates. This is the closest thing to a solved version of the owner's ask —
  a *summary object at a coarser resolution*, first-class on the wire. The
  multi-resolution-modeling literature that grew around it (aggregation/
  disaggregation consistency — Reynolds, Natrajan et al.) also documents the
  failure Orrery must not import: chaos at the boundary when entities *interact
  across* resolution levels. Orrery's H1/H2 dodge that entirely by forbidding the
  aggregate from ever being interactable — the hard problem in MRM only exists
  because military sims let aggregates fight. (Confidence: high on the PDU and the
  problem; specific citations not re-verified from here.)
- **Astrolabe** (van Renesse et al., ACM TOCS 2003) and **SDIMS** (Yalagandula &
  Dahlin, SIGCOMM 2004): hierarchical aggregation of summaries over a tree of
  zones, with staleness as an explicit, tunable property — "eventual, coarse,
  cheap" as a design goal rather than a failure. The durable lesson is the *shape*
  — associative folds up an address hierarchy, which Orrery's parent-is-a-prefix
  `CellId` provides for free — not the transport (both used gossip/DHTs because
  their setting was cooperative infrastructure with no adversarial players; §4's
  objections to gossip are about Orrery's trust model, not about epidemics per se).
- **EVE Online's star map** overlays (ships destroyed, jumps, pilots in space, per
  system, delayed): a shipped, played-for-decades proof that a backend-computed,
  minutes-stale, coarse world summary is a *product players build empires on*, and
  that nobody experiences its staleness as a bug because the UI frames it as a map,
  not as sensor truth. (Confidence: high that these overlays exist and are
  server-computed aggregates; exact cadences not verifiable from here.) Planetside
  2's territory map and similar strategic layers are the same pattern.

**Restated the problem:**

- **Aura/nimbus** (Benford & Fahlén) and the region/quadtree AOI families define
  *whether* you perceive; outside the aura you get nothing. They are the cliff,
  formalized. Orrery already implements the layered version (cell filter, then
  ranked interest — `docs/03-replication.md:114`).
- **Donnybrook** (SIGCOMM 2008) bounds the high-rate set and fills the rest with
  1–4 Hz doppelgängers — but only *inside* the subscribed region; Orrery has this
  landed (`interest.rs:1-13`). Its "guided interest" does not reach beyond the AOI.
- **VAST/VON, Mercury** and the spatial pub/sub overlays scale *subscription
  matching*; the subscriber still receives per-entity events for regions it names.
  Subscribing to everything coarsely is exactly the degenerate case they do not
  optimize.
- **Dead reckoning** (DIS thresholding, Gaffer-lineage extrapolation) is a
  *within-subscription* rate reducer; Orrery's proxy tier is this, landed.
- **SpatialOS query-based interest** deserves a specific mention as the cautionary
  tale: it let a worker subscribe to arbitrary component queries over large areas
  with frequency caps — the owner's feature, almost verbatim — and it was, by
  reputation, a chronic performance liability precisely because it stayed
  *per-entity* underneath: the server still evaluated and streamed individual
  entities however coarse the consumer's intent. (Confidence: medium; based on
  Improbable's own docs and postmortem-adjacent commentary, not re-verified.) The
  lesson matches §1: aggregate at the source or you have not left replication.

The pattern across every success: **the summary is computed near the data by a party
with a wide view, served as a distinct product with honest staleness, and never fed
back into simulation.** The failures tried to widen per-entity subscription and
called it a summary.

## 6. Orrery's own constraints, confronted

### 6.1 Determinism and banking

The verifiable core replays entities from signed inputs and 2 Hz phase-staggered
claims (`docs/06-verifiable-core.md:375`); the adjudication window is bounded and
every neighbor read is part of the recorded evidence. Hearsay is unordered,
per-client, lossy, and stale by design — as an input it would be a nondeterminism
injection port, and as evidence it is worthless because nothing signs its accuracy.
H1 is therefore not a policy preference; it is the only assignment consistent with
D9/D10. Corollary for banking: **no hearsay datum may appear in an
`EvidenceBundle`**, and no dispute may turn on what a summary said. The summary
tier lives entirely in the same plane as rendering: consequence-free
(#354's table: an error there "degrades fidelity, never truth").

### 6.2 Own-state discipline and message classes (D46)

D46's delivered-first composition and own-state discipline mean an entity's step
sees its own state plus delivered, sealed inputs — nothing ambient. A summary
consumed by *presentation* never enters that pipeline, so D46 is untouched. The
tempting violation is a ruleset-side "ambient awareness" component refreshed from
hearsay ("morale drops when outnumbered nearby") — that would smuggle an
unadjudicable input into the step. If a game wants that mechanic, the density figure
must arrive as a **delivered event from a party with simulation authority over it**
(a director entity, a coordinator-fed spawner acting through commands), which is the
D46-shaped version of the same idea. Propose: when the hearsay rules are written
down, this anti-pattern is named in the same record.

### 6.3 #354's principle, honored rather than violated

The back-door risk the brief names is real: a summary tier is the natural excuse for
"we can drop him to the summary, he's far/occluded" — which converts rate into
membership, the exact move #354 forbids and #520 punished. H2 exists to close it.
But there is also a front door: the summary tier is what finally makes the principle
*visible to the player*. Today rate degradation is invisible until it becomes
absence; with a strategic layer, the same craft is high-rate in the center, a 1 Hz
proxy near the edge, a fading ghost past it (#533), and a shard-cell count on the
map — one continuous gradient of fidelity in which membership never silently ended.
That is the fog-of-war fade generalized, and it is the honest product framing for
the whole feature.

### 6.4 Cheating: what the dishonest client learns

Assume the client is hostile and renders nothing it isn't told — the only safe
assumption. Then the summary's information content is the exposure, and it must be
bounded at the source:

- **Resolution bound.** Aggregate at shard level or coarser. At the P1 default that
  is a 1 024 m cell against 400 m weapons; at the campaign's proposed 512 m edge it
  is 4 096 m. A count in a cube ≥2.5× weapon reach does not aim a shot.
- **Staleness bound.** Require `age ≥ E / v_max` at delivery: campaign craft cruise
  at 32 m/s (PR #532's measurement), so a 4 096 m cell wants ≥ ~2 min, a 1 024 m
  cell ≥ 32 s — by the time a blip arrives, its subject could be anywhere in a
  volume larger than the blip. For the peer-count map (B) the natural ~10 s presence
  cadence already exceeds the 512 m interest-cell bound (16 s would be exact; round
  the serving cadence up, it is free).
- **The existing leak is bigger than the proposed one.** The island manifest already
  gives every member every peer's *interest-cell*-granular coverage at presence
  cadence (`coord.rs:73-78`) — finer than anything H4 would permit a map to say.
  Whatever resolution the manifest is judged to safely leak, the map can match; and
  if the manifest's leak is ever judged unsafe (a stealth-heavy ruleset), that is a
  D12/D40-adjacent problem that exists with or without this feature. Flagging it is
  a finding of this node.
- **Hidden state** (regime 3): excluded at the source per H5. This is enforceable in
  B and C (cluster-side or authority-side computation) and unenforceable in D —
  repeated here because it is the decisive anti-gossip argument for any game with
  secrets.

The honest client, meanwhile, learns exactly what the product intends: a map. The
delta between honest and dishonest clients under H4 is rendering choices, not
information — which is the definition of a leak-free presentation feature.

## 7. Opinion

**Recommendation.** Adopt the hearsay tier as a named concept with rules H1–H5 (a
short proposed ADR, or a section in D40's successor if the owner prefers — either
way, *propose*; acceptance is the owner's). Then build the cheapest end-to-end
slice, entirely off the mesh:

1. **Client sighting memory** behind the #533 fade: keep expired replicas as
   explicitly-aged ghosts in the skin, visually distinct, claiming nothing. No
   protocol change; lands with the fade work.
2. **Coordinator map query**: one request/response on the existing control surface —
   "peer counts per shard cell for grid G", served from the presence the registry
   already holds, age-stamped, at most one reply per client per 10 s. ~300 B, ~0.03%
   of budget, zero new trust assumptions.
3. **A strategic layer in the campaign client** that renders (2) at low zoom over
   (1)'s ghosts — and instrument it: does anyone use it? do players ask why NPC
   craft and rocks don't appear on it?

That last measurement is the experiment's real output. **If** players demand entity
density, the follow-on is candidate C (authority-published shard-cell folds,
merged by `CellId` prefix) — designed then, with the trust question ("what does a
lying authority cost us?") answered before a byte ships. **If not**, stop: the
correctly-sized AOI (#532), the proxy gradient, and the fade already deliver a
coherent perceptual edge, and the map stays a peer map.

**What I would not build,** in descending order of conviction: mesh gossip of any
kind (structural, §4); any per-entity beyond-AOI feed, including "just positions,
low rate" (it is replication without its safety rails, and SpatialOS already ran
this experiment at scale); a witness-derived live map (the witness stream carries
hashes precisely so it cannot become one — `verifiable.rs:196` is a feature);
ruleset-readable summaries (H1); and any of this before #532's sizing fix and
#533's fade have shipped and been lived with, because they may dissolve the felt
need entirely.

**The strongest argument against this recommendation** is that the coordinator map
measures the wrong thing and therefore proves nothing. Coordinator presence is
*peer coverage* — it has never heard of NPCs, rocks, wrecks, or anything a
low-population or PvE-heavy game's map is actually about. In the campaign as played
(one player, eight NPC craft), the B map would show approximately *one dot*: the
player. A skeptic can fairly say the experiment is guaranteed to under-deliver, its
"no demand" outcome is preordained, and the real decision (build C or not) will be
made later on no better evidence — so either build C's thin end now (one authority
folding one shard cell is a weekend) or build nothing and wait for a game to ask.
That argument is coherent, and the reason I still put B first is cost asymmetry: B
is nearly free and becomes C's serving path if C ever exists, while C-first commits
protocol surface, an aggregation cadence, and a griefing analysis on behalf of a
requirement no accepted record contains. But if the owner reads the campaign as
already having asked for entity density (a map with NPC contacts on it), skip B's
conclusion-drawing and treat it purely as C's plumbing.

## 8. What could not be verified, and stale-citation notes

- **PR #532 is open, not merged**: `CAMPAIGN_CELL_EDGE_M = 512.0`, the 32 m/s
  cruise, the ~2.5 km orbit and the 400 m reach are quoted from the PR body and
  issue #520/#524 text (the 403 000 mm limit also appears in A12's F-C row,
  `a12-exchange-systems-shakedown.md:47`); none of it is in this tree yet. If #532
  lands amended, §1's geometry numbers need a re-check.
- **Design vs. landed divergence, inherited from D40's own note**: docs/03 §4.1
  specifies multi-factor scoring; landed `interest.rs:10` is "purely
  distance-based". Claims here rely only on the shared property (replication
  scoring is outside the adjudicated step).
- **External systems**: DIS Aggregate State PDU, Astrolabe, SDIMS, HyParView/
  Plumtree, VAST, Mercury, Donnybrook — confident from the literature, not
  re-fetched. EVE star-map overlay mechanics and cadence: recalled, unverified.
  SpatialOS QBI performance reputation: medium confidence, explicitly hearsay by
  this document's own definition — labeled with source and age, as H3 demands.
- **Presence cadence**: derived (60 s TTL = "six presence intervals",
  `registry.rs:38-41` → 10 s); no literal `presence interval` constant exists in
  the D16 table to cite. If a cadence constant lands, H4's arithmetic should cite
  it.
- **Owner-reserved decisions touched by this node**: accepting any hearsay-rules
  record; D40's fate (this node leans on its layering argument while it is
  proposed); and whether the island manifest's information content (§6.4) warrants
  its own review. All three are proposals only.
