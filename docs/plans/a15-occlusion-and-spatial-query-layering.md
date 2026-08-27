# A15 - Occlusion and the network stack: what a spatial query may read, feed, and never gate

> Research node for #354, landing in-tree the research that so far lives only
> in the issue thread (the issue body and the owner's two follow-up comments)
> while three merged records already cite it: A13's H1/H2 lean on "#354's
> layering argument" and "#354's principle" by GitHub reference
> (`docs/plans/a13-aggregation-beyond-aoi.md:171`, `:179`), A14 builds its H6
> read/promotion boundary on the same layering
> (`docs/plans/a14-summary-tier-as-performance-mechanism.md:207`), and
> ADR-0050 carries H2 into a Proposed record
> (`docs/adr/0050-knowledge-tiers.md:194`). The decision-shaped residue of the
> issue thread is itself already Proposed as ADR-0040
> (`docs/adr/0040-visibility-and-spatial-query-layering.md`, merged
> `e00e47c6`, absent from the accepted index by design,
> `docs/DECISIONS.md:79-83`); this node is the evidence record underneath it,
> re-verified against a tree that has moved substantially since the issue's
> 2026-08-23 verification pass. Repository facts verified 2026-08-27 at HEAD
> `5f7e2194`; line numbers drift, re-verify before building on them. External
> claims carry a source or an explicit needs-verification flag. Nothing here
> amends any record - **propose, not decide.**

## Verdict up front

**"Occlusion modulates rate, never membership" survives verification - as the
rule for heuristic visibility, which is the only kind the issue title was
about.** A per-peer, unaudited visibility computation (a raycast against a
replica set, a PVS lookup, any presentation-side occlusion test) may lower an
entity's send rate and may never remove it from a peer's replication
membership. The mechanism that makes this a correctness rule rather than a
taste is verified in section 2: membership decides which recorded facts exist
for the adjudicated step to name, and the replay path refuses what it was
never given.

**As a universal, the principle is already withdrawn - by the tree itself,
twice, and the withdrawal was correct both times.** ADR-0040 retains
rate-not-membership as regime 1's posture and rejects it as the universal rule
(`docs/adr/0040-visibility-and-spatial-query-layering.md:538-541`), replacing
it with the `affects => member` invariant (`0040:134-135`). And since the
issue was researched, Regolith landed exactly the mechanic the issue treated
as deferred: replayable line-of-sight lock decay (#444/#457, merged
`1a14551c`; `crates/orrery_games/src/regolith/visibility.rs`). Visibility is
now a gameplay input in the reference game - and it was landed the safe way,
which is this node's best evidence (section 3): the expensive spatial query
runs outside the core as an unaudited broad phase, enters the step as a
client-authored *claim* naming its candidates, and is verified by an O(1)
integer predicate through the gate-declared audited read site, with the read
count and staleness bounded by named constants.

**The deeper finding is unchanged from the issue and now has a compile-time
witness: the layer boundary is aligned with the audit boundary, and the tree
has begun enforcing the alignment by construction.** Replication-layer queries
(`crates/orrery_spatial`) are outside the adjudicated step - heuristic,
per-peer, approximate, failing toward wasted bytes. Ruleset-layer queries are
adjudicated - integer-exact, recorded, bounded, failing toward refused
windows. Occlusion is cheap on one side of that line and ruinous on the
other. #549 (merged `5f7e2194`) added the third leg: the campaign cell edge is
now *derived from* the weapon envelope so that AOI membership covers
everything the ruleset can resolve a shot against
(`crates/orrery_games/src/regolith/mod.rs:105-139`, `:170-180`) - the
`affects => member` invariant enforced by sizing arithmetic before any
visibility heuristic gets a vote.

**What to build: nothing, yet.** The rate slot for occlusion exists on paper
(docs/03 section 4.1 scoring, section 5.2 priority) and not in code
(`crates/orrery_spatial/src/interest.rs:10` - "Selection is purely
distance-based"); the worked arithmetic in section 4 shows the bandwidth win
is real but modest (tens of kb/s against a 1 Mbps budget in the only regime
where it matters), and membership-drop would buy only the proxy floor itself
(~32 kb/s in the worked case) while taking on the whole correctness hazard.
Measure first; the trigger series are in section 10.

## 1. Three layers answer spatial questions, and only one is adjudicated

| Layer | Mechanism | Adjudicated? | Failure direction |
|---|---|---|---|
| Storage/partition | `CellId` hierarchy; "everything near me" is contiguous key ranges (ADR-0005; octrees/k-d trees are "per-cell in-memory query structures only", `docs/adr/0005-spatial-model.md:19`) | No | extra or missed load/routing work |
| Replication | 27-cell AOI (`docs/adr/0005-spatial-model.md:9`) -> manifest-derived per-client visibility (`crates/orrery_spatial/src/visibility.rs:54-64`, `:108-116`) -> distance ranking into a 24-entity high-rate set with a 15% eviction margin (`crates/orrery_spatial/src/interest.rs:102`) -> 1-4 Hz proxies with a 5 s demotion ramp (`interest.rs:172-181`) | No | bandwidth or presentation fidelity |
| Ruleset | Integer point/segment predicates against declared candidates: grab range (`crates/orrery_games/src/regolith/mod.rs:1134`), fire range and flight time (`mod.rs:1274-1285`), tracking/lead against signature radius (`mod.rs:1417-1467`), and the audited visibility/collision predicates (`crates/orrery_games/src/regolith/visibility.rs`) | **Yes** - every neighbour read is recorded and replayed | refused frames, discrepancy reports, D17-risk-3 false positives |

D16 parameters, all verified in `crates/orrery_spatial/src/config.rs:14-33`:
cell edge 128 m, hysteresis 0.10, high-rate cap 24, proxy 1.0..=4.0 Hz.

Two corrections to the issue-thread record, both already noted by ADR-0040 and
re-confirmed here:

- The issue argued "scoring is already multi-factor" from docs/03 section 4.1.
  That is a design-document claim, not a landed one: `interest.rs:10` says
  "Selection is purely distance-based (nearest-first)" and the file contains
  no interaction, aim, or relevance-class term. Section 5 takes this up.
- The issue's ruleset inventory ("four call sites, `mod.rs:240,330,399,516`")
  is stale twice over: the lines have drifted by ~900, and the inventory
  itself grew a fifth and sixth member - the audited visibility and collision
  predicates - which change the analysis qualitatively (section 3).

The bandwidth frame everything below is argued against
(`docs/02-networking.md:154-174`): Donnybrook's ~12n kb/s receive footprint;
full mesh comfortable at 8 peers (~8% of the <=1 Mbps budget), the 9-32
interest mesh viable only with interest management (~37%, headroom still owed
to input logs, claim hashes, intents, jitter). The regime where an occlusion
term could matter is 9-32, where the budget is already a third spent.

## 2. Why membership must not depend on a per-client visibility computation

The issue's core argument is reproduced here against the landed mechanism,
read line by line, because three records now lean on it.

**The recorded read-set is the audit payload.** `StateView::neighbor` takes
`&mut self` "precisely because reading has a side effect on the log. A view
that let neighbours be read without recording would produce windows that
cannot be replayed" (`crates/orrery_core/src/ruleset.rs:126-137`); the
recorded reads become `NeighborFrame` records in first-read order
(`ruleset.rs:139-146`). Replay does not consult a live world: the harness
serves exactly the recorded frames, refuses a frame older than the ruleset's
staleness bound, and caps how many frames a tick may pull in
(`crates/orrery_core/src/replay.rs:236`, `:252`; Regolith declares
`MAX_NEIGHBOR_READS = 4` and `MAX_NEIGHBOR_STALENESS_TICKS = 60`,
`crates/orrery_games/src/regolith/mod.rs:81-84`). Own-state discipline
completes the closure: neighbours are snapshotted before the step, so "a step
cannot observe another entity's mutation from the same tick"
(`crates/orrery_core/src/executor.rs:140-152`), and every cross-entity effect
travels as a D46 event composed delivered-first
(`docs/adr/0046-message-class-semantics.md:173`). The static gate confines
neighbour reads to a declared allowlist of audited predicates
(`scripts/core-gates.sh:191-207`).

So the replayed window is a pure function:

```text
replay(window) = f(snapshot_t0, ordered_inputs, recorded_neighbor_frames)
```

Every term is attributable, signed, and identical for every replayer. Now put
a per-client visibility computation into the path that decides what a peer
*receives*, and trace the two ways it breaks:

1. **Upstream of the claim.** The audited predicates consume replicas by name:
   `ClaimCover { locker, rock }` names two entities the claiming client found
   in its replica set (`crates/orrery_games/src/regolith/order.rs:217-223`),
   and the collision broad phase iterates the neighbours the harness handed it
   (`crates/orrery_games/src/regolith/visibility.rs:67-103`). Membership
   decides which entities a client can name at all. Rate degradation only
   ages the data - and the staleness bound is engineered for that: claims
   arrive at 2 Hz and one whole missed claim is tolerated (`mod.rs:83-84`),
   so a 1 Hz proxy still clears the 60-tick window. Membership-drop removes
   the term entirely: the entity is unnameable, the mechanic that depends on
   naming it silently stops working for exactly the client the occlusion
   "optimized".
2. **Downstream, at validation.** A delivered effect (a projectile
   continuation, a collision resolution) applies whether or not the receiver
   holds the counterparty's replica - delivery is logged, not derived from the
   replica. A receiver starved of state it needed renders damage from nowhere
   and, in shadow mode, files exactly the reconciliation-error class the trust
   apparatus must keep at zero for honest players: "false-positive strikes on
   honest players are the failure mode that kills witness-based trust"
   (`docs/06-verifiable-core.md:262`), with the whole protection stack of
   bands, sustain windows and shadow launch built to prevent it
   (`docs/07-witnessing.md:215`). #240's exit criterion - zero discrepancy
   reports over 500 honest player-hours - is the named gate an
   occlusion-membership scheme would trip.

Note what the argument does *not* say: it does not say the visibility
computation itself enters `self.reads` - replication scoring runs outside
`step()` and writes nothing any replay verifies. The determinism cost of
occlusion-as-rate is zero. The hazard is entirely in letting an unaudited,
per-client, asymmetric computation decide *existence* of state that audited
machinery downstream assumes.

And membership has one more consumer the issue flagged and the tree confirms:
the witness lane. Log frames go to the <=7-link cell-epoch witness set on a
budget-derived cadence (`docs/03-replication.md:184-209`, 6 Hz at mesh
defaults), are "not sheddable" by the budget layer (same section), and are
bounded independently of island size (`docs/02-networking.md:174`). No
presentation-visibility factor may shed them; a witness starved by a rock
drifting between two ships would be manufacturing chain gaps. ADR-0040 clause
(b) states both exemptions - pins and the witness lane - as hard invariants;
this node re-verifies the machinery they protect and endorses the clause.

## 3. The tree crossed the line the issue drew - and crossing it proved the layering

The issue treated #353 (line-of-sight lock break) as deferred, and its safety
argument leaned on #352's no-LoS combat: "hidden does not mean harmless"
because an occluded ship retained lock and kept shooting. Since then the
mechanic landed. What it looks like is the strongest available evidence for
the layering this node proposes, so it is worth reading closely.

**The shape: claim outside, verify inside.**

- The *target's client* - unaudited, outside the step - runs the expensive
  broad phase over its own replicas: find a rock on the segment between a
  locker and yourself. It submits `ClaimCover { locker, rock }` as an
  ordinary order (`order.rs:217-223`), rate-capped at four claims per second
  (`COVER_CLAIM_INTERVAL_TICKS`, `mod.rs:79-80`).
- The *adjudicated step* verifies the claim through the single declared read
  site: two neighbour reads (locker, rock), one integer segment-sphere
  predicate (`segment_intersects_sphere`,
  `crates/orrery_core/src/geometry.rs:11`), with the sphere shrunk by
  `OCCLUSION_MARGIN_MM = 20` - two centimetres, twice VC-7's one-centimetre
  position epsilon, so the predicate cannot flap inside the tolerance band
  (`mod.rs:85-86`). The module doc states the division explicitly: "The
  expensive broad phase stays outside the core"
  (`crates/orrery_games/src/regolith/visibility.rs:106-112`).
- The verified transition is emitted as `Outcome::LockVisibility { occluded }`
  and delivered to the locker, whose held lock decays by
  `LOCK_DECAY_PER_TICK` per occluded tick and re-fills instantly on
  visibility (`mod.rs:785-796`); a full break takes `LOCK_BREAK_TICKS = 30`
  ticks - half a second (`mod.rs:69-74`).

This is the issue's own design rule 2 ("prefer query-as-input over
query-in-step"), implemented: the candidate set is assembled outside the
adjudicated code, the step makes point tests against declared candidates, the
read-set is bounded by construction (`MAX_NEIGHBOR_READS = 4`). The #353 cost
objection - reading every intervening rock per lock per tick into the audit
payload - was resolved not by avoiding LoS but by inverting the query. The
incentive analysis closes cleanly too: the claiming target *benefits* from a
true claim (its attacker's lock decays), a false claim is refuted at replay by
the same predicate over the same recorded frames, and an omitted claim only
costs the omitter. Nobody needs to trust the broad phase, because the broad
phase decides nothing.

**Why LoS-as-mechanic still does not legitimize occlusion membership-drop.**
Three verified reasons, in increasing order of strength:

1. **The decay window.** An occluded attacker keeps a functioning lock for 30
   ticks of decay, and in-flight projectiles resolve on target-authored
   continuation orders that deliberately keep range live (`mod.rs:1269-1281`).
   "Hidden" and "harmless" are still separated by at least half a second plus
   flight time, and D40's reveal-latency bound (`0040:152-166`) prices exactly
   that gap.
2. **The AOI is sized for effect, not for sight.** #549 derives the campaign
   cell edge from the weapon envelope: the AOI "has to cover the weapon
   envelope plus the largest radius anything adjudicable can carry"
   (`mod.rs:105-111`), budgeting `edge - 2m` against two adversarially
   composed hysteresis lags and rounding the edge up so `0.8 * edge >=
   engagement range` (`mod.rs:140-180`). Membership is already, by landed
   arithmetic, a function of what can *affect* you - `affects => member`
   enforced at compile time. An occlusion heuristic subtracting from that set
   would be undoing a bound another part of the tree just paid to establish.
3. **Occlusion consumes the replicas it would delete.** The cover mechanic
   needs the target to hold fresh replicas of both the occluded *locker* and
   the intervening *rock* to name and time its claim. Membership-drop of
   occluded entities disables the occlusion mechanic for its main
   beneficiary. Rate-clamping at the proxy floor does not: the staleness
   window was sized to tolerate the floor (section 2).

The general lesson, stated once: **when visibility becomes a gameplay input,
it must move *into* the adjudicated layer as a claim-shaped, integer-exact,
bounded predicate - it does not reach *down* from the replication layer as a
heuristic that suddenly acquired authority.** The replication-side PVS the
issue sketched and the ruleset-side predicate that landed are different
objects that happen to share a word; ADR-0040 clause (c) ends with the same
sentence ("none may be cited as proof of a gameplay LoS result") and section 8
of the issue predicted the confusion. Keep the two vocabularies apart in every
future record.

## 4. Where occlusion may legitimately act: rate, and what it is worth

The slot is specified and empty. docs/03 section 4.1 defines receiver-driven
per-peer scoring at 1 Hz (`docs/03-replication.md:120-138`):

```text
score(e) = W_rel(class(e)) * ( a*g_dist + b*g_interact + c*g_aim )
```

with pinned members (current interaction partners, strong-ownership
relations) always in-set (`:136`), and section 5.2 composes send priority as a
product of independent factors (`:159-176`). A visibility term is one more
multiplicand in each:

```text
score'(e)    = score(e)    * g_vis(e),  g_vis in [v_floor, 1.0]
priority'(E) = priority(E) * g_vis(E)
```

clamped so an occluded entity scores as if at AOI edge - proxy floor - never
below, never out; pins bypass scoring entirely; the witness lane is not
scored at all. For static geometry, `g_vis` is a cell-pair PVS lookup keyed by
the `CellId` grid the AOI already uses: conservative ("from any point of cell
A, is any point of cell B potentially visible past static geometry"),
computed offline per grid, O(1) per candidate per rescore. Table cost for a
populated region of C interest cells is a C^2 bitset - at C = 512 cells,
512^2 / 8 = 32 KiB. Conservatism errs safe: over-visible costs bytes, never
correctness. Destructible fields (Regolith's rocks split, #323) make "static"
only epoch-static; recompute lazily per dirtied cell pair or exclude
destructible occluders initially.

**The honest arithmetic**, in docs/03 section 8's own units
(`docs/03-replication.md:254-283`; 25 B high-rate delta at 20 Hz, 40 B proxy,
interest-mesh case of 24 high-rate + ~100 proxies):

```text
one high-rate entity:  20 Hz * 25 B = 500 B/s = 4.0 kb/s
one proxy at 1 Hz:      1 Hz * 40 B =  40 B/s = 0.32 kb/s
demote one occluded high-rate member to floor: saves ~3.7 kb/s

occluded fraction f = 0.30 (untested; measure it):
  7 of 24 high-rate demoted:            ~26 kb/s
  half-rating 30 of 100 proxies:        ~ 5 kb/s
  total                                 ~31 kb/s
    = ~3% of the 1 Mbps budget, ~10-12% of the ~260-300 kb/s
      interest-mesh receive total
```

And the ceiling on what membership-drop could add *beyond* rate-clamping is
the proxy floor itself: 100 proxies * 0.32 kb/s = 32 kb/s if every one were
occluded and dropped. Tens of kb/s is real money in the 9-32 regime and it is
not transformative - which is why this node, like the issue, says measure the
occluded fraction before building anything (section 10). If
`interest.occluded_fraction` in representative Regolith fields is under
10-15%, the mechanism cannot repay its complexity regardless of budget
pressure.

**Dynamic occluders: recommend against, unchanged from the issue.** The
valuable sub-case is already solved by a structural mechanism: a ship
interior is a nested grid whose contents an outside observer never subscribes
to - "the frame boundary *is* an interest boundary"
(`docs/01-spatial-model.md:267`), summary flag on the root proxy only
(`:306`). That is occlusion at 100% effectiveness, membership-grade, and safe
precisely because the frame boundary is also an interaction boundary
(cross-frame effects are logged events, `docs/01-spatial-model.md:283`). The
unsolved sub-case - a hull shadowing open-space entities behind it - is
per-tick moving-occluder geometry per (observer, subject) pair, the case
Valorant found expensive enough to need aggressive engineering *with* a
trusted server (source in section 6), buying little in a mostly-empty volume.
The asymmetry generalizes: **occlusion is worth having exactly where it is
also an interaction boundary or precomputable; everywhere else it is
bandwidth arithmetic that must win on measurements.**

## 5. The design-vs-landed divergence, and what multi-factor scoring would take

`docs/03-replication.md:120-138` specifies four factors and pins;
`crates/orrery_spatial/src/interest.rs:10` implements one factor and no pins.
A13 filed the divergence as a caveat
(`a13-aggregation-beyond-aoi.md:461-464`); A14 treats it as the hook for
shaped interest, with aim-aware scoring "landing docs/03 section 4.1 into
`interest.rs`" as part of its proposed interest-shape record and Heavy's
900 m reach as the first honest consumer
(`a14-summary-tier-as-performance-mechanism.md`, "The interest-shape record
and scoped membership" item). This node's assessment of what closing it takes:

- **The mechanism is small.** `rank_by_distance` -> rank-by-score is a
  comparator change; `select_high_rate`'s margin/stickiness logic
  (`interest.rs:100-166`) and the demotion ramp transfer unchanged. The 15%
  eviction margin note at `interest.rs:119-121` (squared-distance comparison
  makes the margin squared) stops applying to a non-metric score and must be
  re-derived - a real but hour-scale subtlety.
- **The inputs are the work.** `orrery_spatial` sees positions; `W_rel`
  (archetype class), `g_interact` (who shot/traded with whom, when), `g_aim`
  (facing), and pins (strong ownership, current interaction partners) are
  game facts. Closing the divergence means a scoring input the game writes -
  a component or hook trait - which is a crate-boundary design question, and
  it is exactly the question A14's interest-shape record already owns. Do not
  design it twice.
- **Occlusion belongs in it as one multiplicand, not as a parallel path.**
  `g_vis` is the same shape as `g_aim`: a receiver-side, unaudited fidelity
  preference. If the interest-shape record lands, occlusion is a follow-on
  factor with one archetype flag (`static_occluder`) as its entire
  per-entity surface; if it does not land, occlusion should not be built on a
  bespoke side channel either.

The divergence also has a doc-hygiene consequence this node inherits from
ADR-0040 (`0040:49-52`): any record citing section 4.1's scoring must say
whether it means the design or the landed selector. Everything in this node
that depends only on the shared property - replication scoring is outside the
adjudicated step - says so.

## 6. Prior art, sorted by who the visibility oracle is

Condensed from the issue's survey; every entry re-attributed here, flags
inline. The organizing distinctions: rendering occlusion vs network
relevancy; membership culling vs rate reduction; and who computes visibility.

**Trusted server decides what you see (membership culling):**

- **Quake/Source PVS.** BSP-compile-time potentially-visible sets filter
  network transmission per client per tick, with always-transmit and
  dependency escapes (Valve Developer Community, "Networking Entities",
  https://developer.valvesoftware.com/wiki/Networking_Entities). Clusters are
  coarse and conservative; enemies behind thin walls share your cluster and
  still replicate - why wallhacks work in CS despite PVS culling. CS:GO's
  `sv_occlude_players` algorithm was never documented - *needs verification*.
- **Valorant fog of war** (Riot, "Demolishing Wallhacks with VALORANT's Fog
  of War",
  https://www.riotgames.com/en/news/demolishing-wallhacks-valorants-fog-war).
  Evolution with Riot's own numbers: single raycast -> 10 raycasts vs
  bounding box -> target bounds expanded by velocity times a look-ahead
  greater than expected ping -> final server-side voxel cell-to-cell PVS.
  Naive raycasting consumed ~50% of server frame time; the PVS version runs
  under 2%. Membership culling with delayed despawn and effect-replay
  catch-up. Every part assumes the trusted-server oracle. Vendor blog;
  numbers not independently reproduced.
- **CornerCulling** (github.com/87andrewh/CornerCullingSourceEngine):
  third-party per-pair raycast culling for CS with latency-aware optimistic
  lookahead, 1-2% frame time at 10v10 128-tick per its README. Same trust
  model.

**Relevance without occlusion, rate-shaped - Orrery's own lineage:**

- **Tribes** (Frohnmayer & Gift, "The Tribes Engine Networking Model",
  gamedevs.org): per-client ghost scope (membership, pluggable predicate;
  occlusion not documented as a factor) plus per-ghost priority filling each
  packet most-important-first. The ancestor of both mechanisms in one system.
- **Donnybrook** (Bharambe et al., SIGCOMM 2008,
  https://dl.acm.org/doi/10.1145/1402946.1403002): attention-based interest
  sets - proximity, aim, interaction recency; no occlusion term anywhere.
  Top-5 at high rate, everyone else at ~1 Hz doppelganger guidance;
  membership never dropped. Interest sets are receiver-self-declared with no
  adversarial consideration (cheating deferred by the authors). 68%/s
  interest-set churn; 900-player simulated scale. Orrery's section 4.1/4.2
  design is this pattern, so "occlusion as one more attention factor" has
  exact prior-art alignment.
- **EVE Online**: replication scope is grid membership - merged/split
  absolute volumes, everything on-grid visible regardless of distance, no
  occlusion (EVE University "Grid"; CCP "Grid Sizes & You" dev blog on the
  Dec 2015 enlargement). Cautionary tale: absolute bucket boundaries were
  player-manipulable ("grid-fu", acknowledged by CCP when enlarging grids) -
  the spatial index itself became an adversarial surface. Directly relevant
  wherever bucket geometry has gameplay consequences, which after #549 it
  does here (the campaign edge is engagement-derived; its manipulation
  surface deserves a look when campaign PvP matters).
- **Unreal**: stock relevancy is distance/ownership (`NetCullDistanceSquared`,
  `IsNetRelevantFor`), dormancy (rate to zero without membership loss), and
  Replication Graph's flat 2D grid spatialization chosen over trees for
  Fortnite BR (Epic tech blog, "Replication Graph Overview"). No stock path
  does geometric occlusion for networking. Unity NGO and Fish-Net likewise
  ship distance/scene/owner conditions and no occlusion condition. Across
  mainstream engines, network occlusion is bespoke.
- **Academic interest management**: aura/nimbus (Benford & Fahlen, ECSCW
  1993) is mutual extent-based awareness, honesty-assuming, no occlusion; the
  ACM Computing Surveys interest-management survey
  (https://dl.acm.org/doi/10.1145/2535417, already cited at
  `docs/03-replication.md:114`) confirms visibility-based IM is a minor
  branch of a distance/region-dominated field.

**The structurally interesting corner for P2P:**

- **Frontier Sets** (Steed & Angus, IEEE VR 2005,
  https://ieeexplore.ieee.org/document/1492750/): cell-to-cell visibility
  designed for P2P - for each cell pair, two cell sets such that while both
  peers remain inside theirs, they provably cannot see each other and stop
  exchanging updates. Symmetric and independently computable by both ends -
  the only published visibility scheme with the right shape for a
  cross-verifying architecture. Unsolved caveat: each peer trusts the other's
  claimed cell. In Orrery positions are witnessed, logged state, so the claim
  is retrospectively auditable - the property ADR-0040 clause (d) builds on.
- **Chambers et al.** (NOSSDAV 2005, "Mitigating information exposure...",
  https://www.thefengs.com/wuchang/cstrike/nossdav05_mitigating.pdf): moves
  outside the opponent's declared viewable area sent as hash commitments,
  opened and re-simulated post-game. Philosophically closest to Orrery
  (commit now, adjudicate later) but it audits fog-of-war radius, not
  occlusion, and its own numbers show audit machinery dominating bandwidth.
- **Adversarial-P2P verdict** (Baughman & Levine, INFOCOM 2001; Webb et al.,
  RACS, NOSSDAV 2007): information exposure in P2P is handled by
  zero-knowledge tricks that do not scale, post-hoc audit, or a reintroduced
  referee - and no published system combines occlusion-based membership
  culling with adversarial verification of the visibility decision itself.
  That gap is why membership culling here would mean building novel audit
  machinery for the send-or-not decision on top of the D9 apparatus, while
  rate reduction needs none: a low rate is never a claim, and "a subscription
  is a request, not a contract" is already the stated posture
  (`docs/03-replication.md:138`).

Who made the mistake this node warns against: nobody shipped it, which is the
point. Server-authoritative games cull membership safely because the server
is the truth; every P2P system in the literature either kept membership and
shaped rate (Donnybrook, Tribes ghost priority under load) or paid for a
referee. The mistake available to Orrery is importing the first pattern
without the oracle it silently assumes.

## 7. The layering rules, stated as this node proposes them

For any spatial query Q (raycast, PVS lookup, range scan, visibility test):

- **What Q may read** depends on where it runs. Replication-side: anything
  the peer lawfully holds - replicas, manifests, float geometry - because its
  output is unaudited. Ruleset-side: only recorded terms - own state, ordered
  inputs, and neighbours through the declared audited read sites, integer
  math end to end, bounded by `max_neighbor_reads`.
- **What Q may feed.** Replication-side Q feeds rate and priority (score
  multiplicands, section 4) and presentation. Ruleset-side Q feeds adjudicated
  facts and events. A client-side Q may additionally feed *claims* - orders
  naming candidates for the ruleset to verify - which is the landed cover
  mechanic's shape and the recommended pattern for every future "what is near
  me" mechanic that tolerates it (the authority-assembled variant, where the
  candidate list arrives as a logged input, remains available for mechanics
  that need completeness and accept trusting the assembler per D40's terms).
- **What Q must never gate**: replication membership (regime 1 - heuristics
  fail toward more-visible, never toward removal, `0040:381-394`); the
  section 4.1 pins; the witness lane; and any gameplay outcome, unless Q
  itself moves inside the adjudicated layer and pays the full recording cost
  (regime 2/3, D40 clauses (d)/(e)).
- **The ruleset gains a spatial index only on a trigger that has not fired.**
  Every landed ruleset query is a point or segment test against declared
  candidates; the first mechanic whose *outcome* depends on an unbounded
  "everything within r" (AoE damage, sensor sweep, many-body gravity) forces
  one. When it fires: the index is the cell grid (ADR-0005 already demotes
  trees to per-cell structures), integer, ruleset-owned, with a stated
  declared-reads bound - and it is not the replication PVS, which is
  float-space, conservative and unaudited. Same word, different object.

The one-line invariant remains D40's, quoted rather than restated: "Any
entity must be replicated to anyone it can currently affect; hiding is
permitted exactly where affecting first requires a logged reveal"
(`0040:134-135`).

## 8. Strongest argument against

**The title's principle was falsified as a universal within two weeks of
being stated, by the repository's own reference game - so why land a node
whose headline rule is regime-scoped?** The issue was verified 2026-08-23;
`1a14551c` landed replayable LoS lock decay shortly after. Under the FPS
inversion the owner's first comment records, membership-drop stops being
unsafe and becomes the only branch that actually withholds state from an ESP
client - "occlusion modulates rate, never membership" is then not a safety
theorem but a description of one regime, and elevating it to a slogan invites
the next reader to apply it where it is false, or to dismiss it where it is
true. The sharpest form of the objection: the durable content here is
`affects => member`, which ADR-0040 already carries; a research node
re-arguing the superseded slogan adds surface area, citation-drift liability
(this node contains ~60 line-number citations that will rot), and a second
place for the layering story to diverge from the first.

The response, which the reader should weigh rather than accept: (1) the
regime-scoped rule is the one three merged records actually cite - A13's H2
and ADR-0050's H2 inherit "#354's principle" for *heuristic* feeds, where it
is true unconditionally, and today they inherit it from a GitHub issue that
is outside the normative reading path (`AGENTS.md:14-27` names ADRs and
`docs/`; issues are neither); an in-tree evidence record with re-verified
citations is what those records were missing. (2) The falsification is
itself the finding: section 3 documents *how* the tree crossed the line
safely, which no other record does - ADR-0040 predates the landing and argues
from the design; this node argues from `visibility.rs` as merged. (3) The
bandwidth counter-argument stands against building, not against recording:
section 4's arithmetic is exactly the evidence a future implementer needs to
decide *not* to implement `g_vis`, and it did not exist in-tree either.

There is a second-strongest argument and it deserves its line: **rate
modulation leaks through the side channel it creates.** An entity whose send
rate visibly drops when occluded tells a modified client "geometry between
you and it" - `g_vis` is itself an oracle, coarser than the state it
protects but nonzero. The issue conceded a 1 Hz stream is "still a
functioning wallhack"; the concession must extend to the rate *transition*
being informative. This is another reason not to sell occlusion-as-rate as
anti-cheat, ever: its honest justification is bandwidth, and section 4 shows
the bandwidth case is measurable and modest.

## 9. What could not be verified, and corrections to the issue-thread record

Could not be verified from primary sources:

- **Riot's fog-of-war numbers** (~50% frame time naive, <2% final, net
  performance improvement): vendor blog only.
- **CS:GO `sv_occlude_players`**: existence documented in community sources,
  algorithm never published.
- **Star Citizen replication internals and Dual Universe single-shard
  claims**: press-release grade; no relevance algorithm disclosed beyond
  range and container membership.
- **Donnybrook's 68%/s churn and 900-player result**: from the paper, not
  re-run.
- **EVE grid-fu mechanics**: player manual and CCP acknowledgement recalled
  from the issue's research round; not re-fetched this pass.
- **Frontier Sets' Quake II validation numbers**: from the paper, not re-run.

Corrections to the issue thread, found by this verification pass:

- **"#353 deferred" is stale**: LoS lock decay is landed and adjudicated
  (#444/#457, `1a14551c`; section 3). The issue's premise "an occluded ship
  can still shoot you" survives only as the 30-tick decay window plus flight
  time - still enough to keep membership-drop unsafe, no longer enough to
  carry the argument alone, which is why section 3 rests it on the AOI
  sizing bound and the claim-input dependency instead.
- **"Four call sites" and their line numbers**: drifted and outgrown
  (section 1).
- **"Scoring is already multi-factor"**: design-only; ADR-0040 corrected
  this first (`0040:49-52`), re-confirmed at `interest.rs:10`.
- **The issue's "20 Hz" witness-stream description**: the landed cadence is
  budget-derived, 6 Hz at mesh defaults (`docs/03-replication.md:184-186`);
  ADR-0040 already flags the D9/docs/02 reconciliation as owner work.
- **One live nit found in passing**: `MAX_NEIGHBOR_READS` is 4 while its doc
  comment says "at most three distinct recorded frames"
  (`crates/orrery_games/src/regolith/mod.rs:81-82`), and the audited
  predicates read at most three (locker, rock, collision counterparty). One
  unit of unexplained slack or a stale comment - either way the two lines
  disagree; flagged, not fixed, per this node's scope.
- **Stale citation in an Accepted record**: ADR-0046 cites
  `crates/orrery_core/src/executor.rs:103-106` for the own-state snapshot
  rule (`0046-message-class-semantics.md:55-57`); the rule now lives at
  `executor.rs:140-152` (`:103` is `insert_observed`). Content intact, line
  drifted - a routine re-anchor whenever ADR-0046 is next touched, noted
  here so it is not rediscovered.

Claims in this node relying on Proposed records: everything citing ADR-0040
or ADR-0050 leans on records the owner has not accepted; if D40 is rejected,
sections 3, 7 and 8 need re-derivation against whatever replaces the
`affects => member` invariant, and say so here rather than pretending
independence.

## 10. What to measure, and the records a decision would touch

Measurement series, before any `g_vis` implementation (denominators and
triggers restated from the issue, series names kept):

- [ ] `replication.link.budget_utilization` - per-link bytes sent over
  per-link budget, p50/p99, at 8/16/32-peer islands, >=3 sessions x >=30 min.
  Adoption trigger: p99 > 0.7 in the 9-32 regime.
- [ ] `interest.occluded_fraction` - of in-AOI entities, the fraction in
  cells a prototype PVS marks not-potentially-visible; denominator = in-AOI
  entity-seconds. Under 10-15% in representative fields: stop, the mechanism
  cannot repay its complexity.
- [ ] `interest.pin_override_rate` - how often an entity is pinned while
  occluded; denominator = occluded entity-seconds. The empirical check on
  how often "hidden but harmful" happens - now directly observable, since
  cover claims and locks are both adjudicated facts.
- [ ] `proxy.starvation` - entities below floor rate, before/after any
  `g_vis` prototype, proving the clamp holds (docs/03 section 9.3's failure
  class).
- [ ] The #240-shape check if a prototype lands: discrepancy reports per
  honest player-hour, `g_vis` on vs off, same impairment profile, >=50 h per
  arm. One new report attributable to occlusion is a stop-ship.

Records a decision would touch - all owner-reserved:

- **ADR-0040** is the decision vehicle; this node recommends deciding *it*
  (accept, amend, or reject) rather than opening a parallel record, and
  supplies the re-verified evidence for that reading. If amended while
  Proposed, section 3 of this node is the landed-mechanism evidence its
  Context section currently argues from design documents.
- **ADR-0050 H2** inherits nothing new: this node confirms H2's inherited
  premise from the landed mechanism, strengthening, not changing, its basis.
- **A14's interest-shape record** is where multi-factor scoring (and any
  future `g_vis`) belongs; this node adds the requirement that its scoring
  hook be designed with one occluder flag as the entire per-entity occlusion
  surface.
- **D16** would gain `g_vis` clamp and PVS granularity rows only if
  implementation is ever triggered; **docs/03 sections 4.1/5.2** would gain
  the term at the same moment; **D5/D6 stay untouched** - cells remain the
  membership and topology unit, and occlusion at the `ClientAoi` layer
  remains rejected because cell membership feeds island formation and merge
  triggers (D6), exactly the coupling the layering forbids.
