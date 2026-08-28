# A16 - Contact arrows at the screen edge: the first hearsay product

> Design research for #603 (three humans drifted out of each other's AOI in
> three minutes and never met again). The owner has already ruled the
> direction: "hearsay would allow a minimap or at least an arrow at the
> screen edge that indicates where other craft are and that's both design
> choices that I would support." This node picks **one** of those two -
> edge-of-screen direction arrows - and designs the way forward for it under
> the hearsay rules H1-H6 (ADR-0050, Proposed). Repository facts verified in
> this worktree at `2b80cef0` on 2026-08-28; every `path:line` below was read
> before being cited. Nothing here amends an accepted record - **propose,
> not decide.** ADR-0050 is Proposed; section 10 lists what stays with the
> owner.

## Verdict up front

**Edge arrows, not a minimap.** An arrow asserts one predicate - "a crewed
craft was within one cell of this bearing, N seconds ago" - and that
predicate is exactly what the hearsay datum contains. A minimap asserts a
surveyed plan of space: absolute positions in a frame and, worse, the
*relative geometry between contacts*, which no fold under H4 ever computes
or delivers - the form invites the player to read closing vectors and
ambush lines off data that asserts cells. At campaign scale the honest
minimap is either useless (shard resolution: A14's own "four numbers",
`docs/plans/a14-summary-tier-as-performance-mechanism.md:820-824`) or a
radar wearing a map's clothes (cell resolution across a 10x10-cell orbit).
Arrows are the A13 product table's **threat halo** row - "contacts exist
just beyond my edge, roughly there", HUD periphery, ~10 s staleness
(`docs/plans/a13-aggregation-beyond-aoi.md:60`) - and the reunion problem
is that row's problem, not the strategic-map row's. The wire record is
form-agnostic; a minimap can be layered on the same fold later if a game
ever wants the strategic product. Choosing arrows forecloses nothing.

**The deliverer is the campaign host's roster fold.** The host already
holds every craft's committed 512 m cell every tick - bots natively,
humans via the once-per-second meta-lane cell report
(`gates/p1-swarm/src/exterior.rs:27`, `clients/regolith/src/campaign.rs:1046-1051`,
consumed at `gates/p1-swarm/src/swarm.rs:983-990`) - and it is the very
roster the scope decisions in #603's decay table were computed from
(`swarm.rs:996-1009`, the `replica_scope_capture` log at `swarm.rs:1000`).
The fold is a double-buffered snapshot of that roster, served on the
existing Meta downlink lane as one new tagged record, ~110 B per client
per 5 s: ~0.02% of the 1 Mbps budget. No new connection, no new trust
surface, no coordinator, no persistd.

**The anti-wallhack numbers.** Resolution E = 512 m (the committed cell,
`crates/orrery_games/src/regolith/mod.rs:219`); speed cap
v_max = 120 m/s (Interceptor, `crates/orrery_games/src/regolith/archetype.rs:94`).
H4's floor is age >= E / v_max = 512 / 120 = **4.27 s**; fold cadence
F = 5 s, double-buffered so the *previous* fold is served, gives delivered
age in **[5, 10) s** - positional uncertainty on arrival >= 5 x 120 =
600 m > 512 m, so a datum is vaguer than its own cell by the time it is
readable. Independently, E = 512 m exceeds the longest resolvable shot,
MAX_ENGAGEMENT_RANGE = 400 m (`mod.rs:133-139`): even a perfectly fresh
cell fix cannot aim a shot that the ruleset would resolve. Both margins
hold at the cap; at the observed ~32 m/s cruise the felt accuracy is about
one cell, which is what makes the arrow good enough to fly home on.

## 1. The product, named precisely

A13 section 1 separates four products people mean by "a summary view"
(`a13-aggregation-beyond-aoi.md:56-61`). What #603 needs is the **threat
halo generalized to the whole island**: for each other crewed craft, a
direction good enough to fly toward, refreshed often enough that flying
toward it converges. It is not:

- **edge continuity** - #533's fade already ships and handles the last
  stretch before the boundary (`clients/regolith/src/aoi.rs:1-27`);
- **the strategic map** - shard-cell density for a map screen; the campaign
  spans ~1-2 shard cells and the honest render is a handful of numbers
  (`a14-summary-tier-as-performance-mechanism.md:820-824`);
- **prospecting/economy** - minutes-to-hours archive products.

One consequence follows immediately: the product needs **per-craft cell
data, not counts**. A13 section 1's "nobody in this list needs positions"
holds for its four rows; reunion is a fifth row and it genuinely needs to
distinguish *which* craft is *where* at cell granularity - otherwise the
arrow cannot say "your friend", only "somebody". That is more than a count
and much less than a position: the design below delivers exactly a
(seat, cell, age) triple and nothing finer, and section 4 shows the triple
stays on the safe side of H4 by two independent margins.

## 2. The form: why arrows beat the minimap

Both forms would render the same fold. The question is what each *asserts*
over it, because ADR-0050 clause (c)(2) applies to the rendering of hearsay
too (clause (e)(7), `docs/adr/0050-knowledge-tiers.md:264-267`): every
world-predicate a player would read off the element must be entailed by the
datum.

**An arrow's full assertion set** is: "craft S was in a cell whose center
bears this way from you, A seconds ago" plus the age label. Every part is
entailed by one (seat, cell, age) triple plus the client's own stated
position. The arrow relates one contact to *you*; it is structurally unable
to assert contact-to-contact geometry, because each arrow is drawn from a
single triple.

**A minimap's assertion set is larger than its data.** A plan view with
blips asserts (i) absolute positions in a surveyed frame, (ii) the pairwise
geometry of every blip pair - separations, closing lines, who is between
whom - and (iii) by its visual language, that the space itself is known.
(i) is only cell-true; (ii) is read off by the player with error up to two
cell diagonals plus two aging drifts (~2.4 km worst case at the numbers
above) with nothing on screen to say so; (iii) is false - nothing surveys
the space. A minimap honest about all three needs blurred blips, error
rings and a disclaimer; the arrow needs an age label. Under H4 the arrow is
honest *by construction*, the minimap honest only *by restraint* - and the
house record on skins is that restraint erodes at exactly these seams
(#502/#505, #522; ADR-0050's context section,
`docs/adr/0050-knowledge-tiers.md:69-86`).

Secondary arguments, each real but none decisive alone:

- **Cost and skin surface.** An arrow layer is one HUD module in the
  `aoi.rs` mould. A minimap is a second visual frame: projection, zoom,
  markers, self-marker, orientation convention - all new surface for the
  clause (c) review test to patrol.
- **Screen economy.** Regolith is flown full-screen with a world-space HUD
  (`clients/regolith/src/hud.rs:1-6`); arrows live at the screen edge the
  player already scans; a minimap competes with the reticle for attention.
- **The plane question.** Campaign craft happen to fly at y = 0 and the
  fade code deliberately refuses to encode that assumption
  (`clients/regolith/src/aoi.rs:103-107`). A 2D minimap must pick a
  projection plane and silently discard elevation; arrows project a 3D
  bearing into screen space with no plane assumption.

**Test the choice against its inversion:** if the campaign someday wants
"where is the fight densest" or "which sector is mined out", that is the
strategic-map or prospecting row, the minimap becomes the right form, and
the fold below serves it unchanged. The forms are products; the tier is the
design. This node designs the tier's campaign instance and picks the
product #603 needs.

## 3. Where the data comes from

### 3.1 The candidates, weighed for this campaign

The campaign's shape: one host process (the p1-swarm harness) simulating
`peers = 5` bots in-process and routing all traffic for up to `humans = 3`
exterior clients; full house exactly the 8-peer shard unit
(`docs/plans/multi-human-campaign-sessions.md:106-127`). Against that
shape:

| candidate | verdict | why |
|---|---|---|
| coordinator map (A13's B) | not available, and withdrawn anyway | campaign sessions run no coordinator; A14 withdrew B as the experiment and rejected coordinator entity knowledge outright (`a14-summary-tier-as-performance-mechanism.md:678-717`) |
| persistd lease/at-rest folds (A14's E) | right engine answer, wrong campaign layer | the architecture's deliverer, but whether campaign sessions exercise the gateway at all was never established (`a14:913-914`), and standing up cluster folds for an 8-craft box is machinery ahead of its consumer |
| admission service | wrong direction of flow | it holds the seat map and labels (`clients/regolith/src/roster.rs:63-86`) but has never heard a position; teaching it positions would create a second, unfenced position channel |
| client sighting memory | not this product | it remembers what *this* client saw; the failure mode is precisely that nothing was seen for minutes. Still worth building (A13 step 1) - it is complementary, not competing |
| **the campaign host's roster fold** | **build** | the host already holds every craft's committed cell each tick, refreshed for scope gating itself (`swarm.rs:975-1009`); it is the campaign-scale instance of the tier's own rule that only a party which lawfully sees everything can enforce H4/H5 at the source |

The host is not a new trust assumption: every downlink byte the client acts
on already comes from it, and the fold's input is the same roster its scope
gate reads. When campaigns move onto the coordinator/persistd stack, the
record below is exactly a lease-fold row (entity, cell, age -
`a14-summary-tier-as-performance-mechanism.md:599-617`); the client module
does not change, only the party computing the fold does. The campaign
shortcut therefore does not fork the architecture; it instantiates it one
layer down.

### 3.2 The record and its cost

One new Meta-lane downlink message, joining the existing tag grammar
(`gates/p1-swarm/src/exterior.rs:177-179`: ack = 0xa1, cell reports eight
bytes, announce empty, goodbye 0xff). Proposed shape, little-endian:

```text
HearsayContacts (Meta lane, host -> exterior, tag 0xa2)
  [tag u8 = 0xa2]
  [source u8 = 0x01 (HOST_ROSTER_FOLD)]      ; H3: who computed it
  [fold_tick u64]                            ; when the snapshot was taken
  [count u8]
  count x {
    [slot u8]                                ; seat, resolves via the roster
    [cell u64]                               ; CellId bits, 512 m level
    [fact_age_ticks u16]                     ; fold_tick minus the tick the
  }                                          ;   cell fact was last current
```

Per entry 11 B; 8 craft = 88 B + 11 B header = 99 B per record. Sent to
each exterior once per fold period (F = 5 s): **~20 B/s per client,
~160 bps, 0.016% of the 1 Mbps peer budget** (`crates/orrery_net/src/budget.rs:1-6`).
The client's Meta downlink dispatch already ignores unknown tags
(`clients/regolith/src/campaign.rs:1064-1066` decodes the ack member and
falls through otherwise), so an old client against a new host degrades to
today's behaviour - nothing to version.

`fact_age_ticks` exists because the host's knowledge is not uniformly
fresh: a bot's cell is current at the fold tick, but a human's cell is a
meta report up to ~1 s old plus impairment jitter. H3 requires the age of
the *fact*, not the age of the envelope; the client renders
`now - fold_tick + fact_age_ticks`.

Whether the record lists all 8 craft or only crewed seats is a product
knob (section 5.3 argues crewed-only for the default render, and the
*record* should carry only what the product renders - what is never sent
cannot leak; a later product that wants bot blips extends `count`).

## 4. The H-rules, clause by clause

ADR-0050 is Proposed; building this is building against Proposed clauses,
and section 10 flags that squarely. Compliance as designed:

- **H1 (never a simulation input).** The fold's output goes to exterior
  clients only; no bot, no ruleset, no order ever reads it. Client-side,
  the module mirrors `aoi.rs`'s stated discipline: "nothing here is
  readable by intent submission, range, arc, lock or collision code"
  (`clients/regolith/src/aoi.rs:14-16`) - the hearsay module exports one
  render-only view and nothing else. The gates cannot see client skin
  code, so this is review-held, same as clause (c) today
  (`docs/adr/0050-knowledge-tiers.md:277`).
- **H2 (never gates membership or rate).** The fold *reads* the same
  roster the scope gate reads and *writes* nothing. The testable property
  ADR-0050 names ("replication behaves identically with the tier on or
  off", clause (f) H2 row) becomes concrete here for the first time: run
  the deterministic campaign harness twice, fold on and fold off, and diff
  the `replica_scope_capture` log (`swarm.rs:1000`) - byte-identical or
  the build is wrong. This is the first place H2 stops being vacuous, and
  the test is cheap because the harness is already deterministic.
- **H3 (source- and age-labelled end to end).** The record carries a
  source byte and two-part age (3.2); the skin renders age as ASCII text
  next to the arrow ("7s"). An arrow with no age label is a clause (e)(7)
  violation, and the render test should pin that the label exists whenever
  the arrow does.
- **H4 (coarser and staler than action), quantified.** Two independent
  bounds, both enforced host-side because the client is the adversary:
  - *Resolution.* E = 512 m, the committed interest cell - no finer datum
    exists in the fold's input, so over-resolution is structurally
    impossible. E exceeds the longest resolvable shot: MAX_ENGAGEMENT_RANGE
    = 400 m (`crates/orrery_games/src/regolith/mod.rs:133-139`, the #545
    table cut), so a cell fix of *any* age cannot aim a shot the ruleset
    would resolve; closing to where the replica takes over is the only way
    to act on it, and there the real replication rules govern.
  - *Age.* Floor = E / v_max = 512 / 120 = 4.27 s (v_max = 120 000 mm/s,
    `archetype.rs:94`; the bound uses the cap, not the ~32 m/s cruise,
    because the adversary flies flat out - A14's correction of A13,
    `a14:754-756`). Enforcement is A14 section 5.2's mechanism verbatim:
    fold at F = 5 s, double-buffer, serve the previous fold - delivered
    age in [5, 10) s, worst-case drift 600-1200 m, always exceeding E.
    Pinned test: no delivered entry's total age below 4.27 s (256 ticks).
  - *The dishonest client's take.* A modified client rendering the raw
    record learns seat -> cell at 5-10 s age: it can navigate toward a
    contact (the product, working) and cannot aim, pre-fire, or track
    through cover with it (uncertainty > weapon envelope at all times).
    The honest and dishonest clients differ in rendering choices, not
    information - A13's definition of a leak-free presentation feature
    (`a13-aggregation-beyond-aoi.md:400-402`).
- **H5 (reveal gates at the source).** Regolith is fully regime-1 public;
  H5 is vacuous today. The fold must still be written with the filter
  point named (one function between snapshot and encode), so the record
  that ever introduces hidden state has a place to stand. Rocks and
  pickups are excluded from the record not for secrecy but because the
  product does not render them - what is not sent cannot leak.
- **H6 (a read is not a promotion).** Vacuous in the campaign: every craft
  is always simulated, nothing is at rest, nothing can be promoted. The
  fold is a pure read of host memory; noted for completeness.

## 5. What it shows, and what it refuses to show

### 5.1 Shown

For each **other crewed seat** (roster `kind == "human"`,
`clients/regolith/src/roster.rs:79-81`) whose craft has **no live replica**
on this client:

- one arrow at the screen edge, along the screen-space projection of the
  world-space direction from own craft to the reported cell's center
  (`crates/orrery_protocol/src/cell.rs:99-103`: center = min corner +
  half-edge on each axis);
- an age label in whole seconds, ASCII only ("7s") - Bevy's built-in face
  is ASCII-only and the roster module already lives with that (#526,
  `roster.rs:36-39`);
- the roster label when one exists; `None` means no text, exactly the
  roster discipline - no "PLAYER 3", no "UNKNOWN"
  (`roster.rs:24-28`, `roster.rs:222`).

The arrow re-aims when a new record arrives and as the *own* craft moves
(recomputing bearing to a fixed reported cell from one's own stated
position is a pure function of stated facts - clause (c)(1)). Between
records the reported cell is **frozen**: the arrow never advances a
contact along an inferred velocity, because velocity was never delivered.

### 5.2 Refused

- **Range.** No distance readout in v1. The datum entails distance only to
  +/- one cell diagonal plus aging drift; a number invites precision the
  triple does not carry, and reunion needs none - the arrow re-points
  every 5 s and converges (section 6). If playtests demand it, a coarse
  band ("far/near") derived from cell distance is entailed and can be
  added without a protocol change - noted as a knob, not designed here.
- **Contact velocity or heading.** Never delivered, never inferred.
- **Extrapolated motion.** The arrow does not drift, lead, or animate the
  contact between records.
- **A stale-data placeholder.** If no record names a seat for
  3F (15 s), that seat's arrow is removed - absence rendered as absence,
  clause (c)(3). No "last known" ghost arrow: persistent last-sighting
  memory is A13's step-1 product (client sighting memory), a separate
  piece with its own aging rules, deliberately not smuggled in here.
- **Arrows for craft with a live replica.** When the craft is replicated
  the real body is on screen or in the fade band, and it is strictly
  better knowledge; a simultaneous arrow would be a second, staler
  assertion about the same subject. Suppression keys on replica liveness
  (the same map `expire_stale_replicas` maintains,
  `clients/regolith/src/campaign.rs:79`, `campaign.rs:1355-1360`), not on
  distance.

### 5.3 Crewed seats only, as the default

Seven arrows on an 8-craft box is clutter, and bot discovery is encounter
content - the campaign's interest churn is its test surface
(`mod.rs:203-212`). The failure #603 records is humans losing *humans*.
Default: arrows for crewed seats only, which also caps the render at 2.
Whether bots may appear (a scanner product, a difficulty option) is the
owner's product call; the record shape does not change, only `count`.

### 5.4 Where the arrow meets the #533 fade

The fade is a function of *distance to the interest boundary*, deliberately
never of staleness - #527 pinned two tests in opposition to keep replica
expiry and fading separate concerns (`aoi.rs:18-26`). The arrow keys on
*replica absence*. The seam, in order, for a contact flying away at cruise:

```text
full opacity            depth > band (120 m inside the face)
fade band               last 120 m before the face   (aoi.rs fade_band_m)
hard drop               membership ends; body despawns with its state
[0..2 s]                no replica, no arrow yet     (REPLICA_TTL expiry
                                                      already ran or the
                                                      state cut off clean)
[<= F later]            arrow appears, age 5-10 s
```

The gap between drop and arrow is real, bounded (one fold period plus
expiry), and honest: for those seconds the client knows nothing current and
shows nothing. Closing it with client sighting memory (a ghost aging in
place) is A13 step 1 and composes cleanly later; the arrow must not fake
it. Neither module touches the other's input: fade reads positions of live
replicas, arrows read the hearsay record - no shared constant, no new
coupling for the #499/#502 two-definitions failure to grow in.

## 6. What it would have done last night

The decay table (#603): in-scope answers per minute 521 / 284 / 37 / 8 /
0 / 46 / 0 while total decisions held ~1060/min. Replaying that session
with arrows:

- **15:07-15:08** - players mostly in mutual scope; zero to two arrows,
  each suppressed whenever its craft is replicated. Screen unchanged for
  engaged play.
- **15:09** (37 in scope) - separations pass ~1-2 km. Each player now
  holds up to two arrows, age 5-10 s. Bearing error at 1.5 km: half a
  cell diagonal (443 m) plus cruise drift over 10 s (320 m) is <= 763 m
  of positional error, atan(763/1500) ~ 27 deg worst case - coarse, but a
  27 deg heading error still closes at cos(27) ~ 0.89 of full closure
  speed, and the error shrinks as the next record lands.
- **15:10-15:11** (8, then 0) - the session's point of no return. With
  arrows: separation ~5.7 km, bearing error atan(763/5700) ~ 7.6 deg,
  closure cost cos(7.6) ~ 0.99. Two players flying each other's arrows
  close at ~2 x 32 = 64 m/s: **5.7 km in ~90 s**. One player chasing a
  player who keeps flying away closes at ~0 m/s - but that player is
  *choosing* not to be found, which is gameplay, not the failure mode;
  and drag (5%/s, `mod.rs:30`, applied at `mod.rs:864-867`) means a
  player who stops thrusting stops, so an unattended craft cannot recede.
- **15:12** (46, the chance crossing) - under arrows this is not luck but
  the expected outcome of two guided approaches, and it ends in scope
  regained rather than passed through: inside 409.6 m pairwise
  (`mod.rs:239-244`, edge - 2m, #553) replication takes over and the
  arrows suppress themselves.

Two further properties worth as much as the reunion:

- **The diagnosis becomes possible in-game.** #603 notes a player who sees
  nobody cannot tell "replication bug" from "5 km away" (#594 was exactly
  such a bug). With arrows: bug = seat seated but no arrow and no replica;
  distance = arrow present. The instrument and the feature are the same
  40 bytes.
- **The interest machinery stays untouched and measurable.** H2's diff
  test guarantees the 1060 decisions/min pipeline is byte-identical with
  arrows on; the campaign remains the interest-churn shakedown it exists
  to be.

These are model numbers, not measurements; the pursuit arithmetic assumes
cruise speed and cooperative players. Per the house lesson that three
green regressions once passed while play stayed broken, the acceptance
test is a live three-human session, instrumented with the same
`replica_scope_capture` log plus per-client arrow age samples.

## 7. Decomposition

Ordered; each piece lands separately and is testable alone. Sizes assume
one person familiar with the tree.

1. **Wire record** (`gates/p1-swarm/src/exterior.rs`): `HearsayContacts`
   encode/decode joining the Meta tag grammar, with round-trip and
   unknown-tag-tolerance tests. Mechanical. ~0.5 day. No dependencies.
2. **Host fold** (`gates/p1-swarm/src/swarm.rs`): per-exterior fact-tick
   bookkeeping for meta cell reports (store the tick at
   `swarm.rs:983-990`); a double-buffered roster snapshot at F = 5 s;
   emit one record per exterior from the *previous* buffer. Tests: (a)
   delivered age floor - no entry younger than 256 ticks; (b) **the H2
   diff** - two deterministic runs, fold on/off, `replica_scope_capture`
   byte-identical; (c) crewed-only contents. The diff test and the
   fact-tick bookkeeping are the subtle parts; the fold itself is ~50
   lines. ~1-2 days. Depends on 1.
3. **Client hearsay state** (`clients/regolith/src/hearsay.rs`): decode at
   the existing Meta dispatch (`campaign.rs:1064-1066`), hold the latest
   record, expire seats not named for 3F, expose one render-only view.
   Module doc-comment carries the aoi.rs-style discipline statement
   (nothing readable by intent, lock, range or collision code) and the
   H-rule citations. Tests: absence after 3F; no placeholder for an
   unnamed seat. ~1 day. Depends on 1.
4. **Arrow rendering** (skin): screen-edge indicator per eligible seat -
   3D bearing to cell center (`cell.rs:99-103`), projected and clamped to
   the screen rectangle; ASCII age text; roster label via the existing
   `ShipRoster::label` (None = no text, `roster.rs:222`); suppression
   while a live replica exists. This is the judgement-heavy piece
   (legibility, clutter, the exact edge treatment) and should go to
   whoever owns skin decisions, in one batch with its visual tuning.
   Tests: label present iff arrow present (H3); arrow absent while
   replica live; arrow absent when state expires. ~1-2 days. Depends on
   2 and 3.
5. **Live acceptance**: one instrumented three-human session against the
   #603 decay metric - in-scope decisions per minute must not collapse to
   sustained zero while players are trying to meet; capture arrow ages.
   Testers are the scarce resource: everything above must be green in the
   deterministic harness before this session is spent. Depends on 1-4.

Explicitly out of scope, each with its own record if wanted: client
sighting memory (A13 step 1); any spawn-geometry or soft-boundary change
(#603 options 2-3 - orthogonal, and a boundary is a ruleset change with a
version bump); the persistd folds (A14 section 8); minimap rendering; bot
arrows / scanner products.

## 8. Strongest argument against

**This makes "lost" impossible, permanently, and that is a game-design
decision wearing a bug fix's clothes.** The campaign's whole interest
design fights to keep contacts crossing cell boundaries - the owner cut
Heavy's reach rather than widen the AOI precisely to preserve that churn
(#545; `mod.rs:203-212`) - and a standing two-arrow contact list means no
human can ever disappear from another human's HUD for more than 15 s.
Every hide, every flank through the dark, every "where did he go" moment
is deleted for crewed craft, forever, in exchange for fixing a failure
that cheaper levers also fix: a tighter spawn ring buys the same first
half hour with zero protocol surface, and a soft boundary bounds
separation at the flight-model level. Worse, this node builds the
project's *first* hearsay product against an ADR that is still Proposed -
if the owner rejects or reshapes ADR-0050, the campaign has shipped
behaviour citing rules that never became normative.

The reply, for the record: the owner has already ruled the direction
("that's both design choices that I would support"), which answers the
should-we but not the forever - so the design keeps the exit cheap: the
arrow set is one record whose `count` the host chooses, and "arrows only
when no crewed contact has been replicated for T minutes", "arrows as a
consumable", or "no arrows in a future stealth mode" are all host-side
policy over the same wire shape, decidable per campaign later. The
flank-and-hide objection is also weaker than it looks at these numbers: a
5-10 s stale cell fix bounds hiding at the strategic scale, not the
tactical - inside one cell (512 m, wider than any weapon's reach) the
arrow says nothing at all, and that is where every fight happens. The
Proposed-ADR objection is real and is exactly why section 10 puts the
acceptance question first: the honest orderings are "accept ADR-0050,
then build" or "build explicitly against Proposed and say so in the PR" -
the owner picks. What the reply does not defeat: if the owner would
rather spend the fix on spawn geometry and keep hearsay unbuilt until the
engine needs it, that is coherent, cheaper, and this node's work keys the
decision rather than forcing it.

## 9. Findings, and what could not be verified

**Findings against the commissioning brief and #603:**

- **"Momentum flight with no drag" is contradicted by the tree.** Craft
  velocity is multiplied by `1 - DRAG_PER_SEC * DT` every tick
  (`mod.rs:864-867`; `DRAG_PER_SEC_PER_MILLE = 50`, `mod.rs:30`, present
  since Regolith v1, #333). At 5%/s a coasting craft's speed halves in
  ~14 s; the 5.7 km separation was flown under sustained thrust, and a
  craft nobody is thrusting does not recede. This weakens no part of the
  case for arrows (the players *were* thrusting) and slightly strengthens
  the reunion math (an unattended friend is stationary). #603's text
  carries the same claim and should be corrected.
- **"409.6 m guaranteed AOI radius" is the pairwise figure.** The one-body
  guarantee is edge - m = 460.8 m; 409.6 m is edge - 2m, the
  two-hysteretic-bodies membership guarantee from #553
  (`mod.rs:220-244`). For two players finding each other, pairwise is the
  right number, so the brief is right in substance; both are named here so
  neither gets cited for the other's job.

**Not verified / not verifiable from here:**

- The decay table and the ~1060 decisions/min figure are quoted from #603;
  the raw session logs are not in the repository.
- The ~32 m/s cruise figure is inherited from #532/#603; the current v16
  thrust/drag equilibrium was not measured for this node. The H4 bound
  does not depend on it (it uses the 120 m/s cap); only section 6's
  reunion-time estimates do.
- Whether Meta-lane downlink frames pass through the impaired router or
  bypass it was not traced; if impaired, record loss stretches arrow age
  toward the 3F expiry, which degrades safely (older, then absent) but
  should be confirmed when piece 2 lands.
- Section 6's reunion arithmetic is a model. The live session (piece 5) is
  the measurement, per the standing lesson that green harnesses have
  coexisted with broken play.
- Whether any in-flight work already touches the Meta tag space or the
  fold cadence was not established beyond this worktree.

## 10. Owner-reserved decisions this node touches

1. **ADR-0050's status.** This would be the first hearsay product built
   against its Proposed clauses. Recommended: accept ADR-0050 (possibly
   amended) before or alongside piece 2, or explicitly accept building
   against Proposed. If accepted, no amendment is required by this design -
   H1-H6 are satisfied as written; the fold-cadence constants (F = 5 s,
   expiry 3F) belong in the implementing code, not the record.
2. **The form choice itself.** This node argues arrows; the owner offered
   two forms and the choice is theirs to confirm.
3. **Crewed-only default, and every knob named in sections 5.3 and 8**
   (bot arrows, range bands, per-campaign arrow policy).
4. **Scope of the campaign shortcut**: blessing the host roster fold as
   the campaign-scale deliverer, with the persistd folds remaining the
   engine answer (A14 section 8) when campaigns move onto the cluster.
