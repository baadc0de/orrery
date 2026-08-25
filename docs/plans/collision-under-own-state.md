# Collision under own-state discipline (#441 design)

**Verdict: yes, under conditions.** Two-party physical interaction is expressible
under the own-state discipline, cleanly, with the machinery the tree already
has — provided three conditions are accepted. (1) **The impulse is computed
once, by one party, and carried to the other as an event** — computing it twice
from two independently-held snapshots makes momentum conservation approximate,
not exact, because per-entity authority means the two bodies never share a
tick-start snapshot. (2) **Detection enters the core as an untrusted claim**
(a delivered event or a client-submitted order), verified by an integer-exact
predicate in the receiving entity's own step — never as a live neighbour read.
(3) **Pair-level agreement is a protocol-shape property, not an adjudicated
one**: each entity's replay is self-contained and correct in isolation, and no
adjudicator today checks any relation *between* two entities' windows, so "both
bodies bounced" is guaranteed by construction of the event round-trip, not by a
verdict. Under those conditions the recommended shape (§5) produces **zero new
false-deviation surface**, needs **no protocol bump, no new core machinery, and
no change to `core-gates.sh`**. What it forecloses is stated in §5.3: persistent
resting contact (stacking, pushing, load-bearing piles) is not expressible as
impulsive events, and that is a real finding about the architecture, not about
Regolith.

This is a design document. It changes no ruleset. Every code citation below was
read in this worktree (branch `docs/collision-design`, base `13b1eeef`) on
2026-08-25; §8 logs the mutations run to prove the enforcement claims, and §9
lists what was found stale or unevidenced.

## 1. The constraint, as verified

The ban is real, mechanical, and justified by an unreplayable read — not style.

- `scripts/core-gates.sh:126-139` (clause 5) scans `RULES_CRATES =
  (orrery_games orrery_conformance)` for `\bview\.neighbor\s*\(` and dies with
  `live neighbour read in a Ruleset — cross-entity effects travel as events
  (docs/06 §3)`. Proven live in §8, mutation M1.
- The reason, from the gate's own comment (`scripts/core-gates.sh:127-132`):
  "`StateView::neighbor` records the read, but nothing yet *replays* it: a
  `NeighborFrame` producer does not exist (docs/06 §3, implementation status),
  and `ReplayHarness::load_claimed_snapshot` installs exactly one entity — so at
  replay every neighbour read returns `None` and a rule that branched on one
  adjudicates differently than it executed."
- Both halves check out in code: `crates/orrery_core/src/replay.rs:116-130`
  (`load_claimed_snapshot` inserts exactly `claim.entity` and nothing else);
  `crates/orrery_core/src/executor.rs:123,139` collects
  `TickOutcome::neighbor_reads` from `view.recorded_reads()` and no production
  code consumes it; `crates/orrery_protocol/src/verifiable.rs:116` —
  `RecordSource::NeighborFrame { neighbor: PersistId }` carries the id only,
  not the quantized fields docs/06 §3 says a producer must record.
- Why an unreplayable read is the worst outcome, not merely a gap: ADR-0045
  IV-2 (`docs/adr/0045-per-component-capability-policy.md:237`) — a mechanism
  that makes "every honest re-execution a false deviation … convicts everyone,
  which is worse than watching no one."
- The ban is explicitly temporary in the normative text: docs/06 §3
  implementation-status note — "neighbour reads remain permitted by §3 and by
  D9, and become adjudicable when `NeighborFrame` gains a producer that records
  the fields."

**Own-state discipline as ratified (D46).** Cross-entity effects travel as
events; delivery pays one tick ("every cross-entity effect pays one tick of
latency … it is what isolated single-entity replay is made of",
`docs/adr/0046-message-class-semantics.md:132-135`); immediate effects are
allowed "only where the audience is the actor itself" (D46 clause (c),
`0046:163-165`); input composition is delivered-events-first, ratified as law
(D46 clause (d), pinned at `crates/orrery_games/src/scenario.rs:210-214`).

## 2. What already works: the `Order::Damage` pattern, read closely

Regolith's shot resolution is the existing proof that a *one-directional*
cross-entity physical interaction fits the discipline. The mechanics, verified
in `crates/orrery_games/src/regolith/mod.rs`:

1. **Attacker's step** (`mod.rs:272-321`): on `Order::Fire { target }` with an
   acquired lock, the attacker rolls damage from **its own** `TickRng`, and
   emits `Outcome::DamageDealt { attacker: me, target, amount, attacker_pos:
   origin, attacker_vel: firing_vel, … }`. Note `origin` is the attacker's own
   state — the event payload is a pure function of (own state, inputs, rng), as
   D46 clause (e)(4) requires (`0046:275-277`).
2. **Routing** (`Game::deliver`, `mod.rs:1074-1094`): `DamageDealt` becomes
   `Order::Damage { amount, from, from_pos, from_vel, from_weapon, flight_ticks }`
   delivered to the target's next-tick input vector.
3. **Target's step** (`mod.rs:322-395` for craft, `mod.rs:497-556` for rocks):
   `projectile_resolution` (`mod.rs:745-793`) adjudicates entirely from **own
   state plus the carried evidence**: integer-exact range check in i128 mm
   (`nonnegative_distance_squared`), flight-time laddering, then a hit roll
   from the **target's** rng against `hit_chance_ppm`. Shield/hull mutation is
   an own-state write. The result travels back as `Outcome::ShotResolved` /
   `Outcome::Destroyed` events to the attacker at t+1.

Three properties of this pattern carry the whole design:

- **The validator is the party whose state changes.** The target cannot be
  damaged by geometry it can check and reject.
- **The carried evidence is committed on both sides.** The attacker's emission
  is re-derivable from its signed log; the target's consumption is an
  `InboundEvent` record in its signed log. A fabricated `from_pos` is a
  divergence in the *attacker's* replay (its emitted event is a function of its
  own state).
- **Replay stays single-entity.** Neither step reads the other; both windows
  adjudicate in isolation (`orrery_games::scenario::adjudicate_isolated`,
  `scenario.rs:390`, and the battery test
  `honest_play_adjudicates_entity_by_entity`).

**The honest caveat, stated against the pattern's interest:** the link between
the two logs — that the `Order::Damage` the target logged equals the
`DamageDealt` the attacker's replay emits — is verified by **no production
mechanism today**. D46 clause (e)(4) says so in terms
(`0046:284-290`): routing correctness is "golden-pinned only (M-A6-3)", and the
record "does not claim target-side coverage that the evidence says is not
there." Collision inherits exactly this trust surface, neither better nor
worse. Cross-log event-consistency checking is witness-side future work; until
it exists, a cheating build can emit events with fabricated kinematics and be
caught only when *its own* window is adjudicated.

## 3. Why collision is genuinely harder than damage

- **Mutuality.** Both bodies change velocity. Under the discipline each entity
  may mutate only its own state, so a collision is necessarily *two* own-state
  writes on *two* entities, coordinated only by events.
- **Symmetry / no natural target.** Damage has a beneficiary (attacker) and a
  validator (target), and the validator is the sceptic. In a collision both
  parties are both. Who asserts, who checks?
- **Relative geometry.** Overlap is a predicate on `(pos_A, pos_B, r_A, r_B)`.
  Neither step holds both positions; one side's kinematics must be carried.
- **Conservation.** Elastic resolution needs one impulse `J` applied as
  `v_A' = v_A + J/m_A`, `v_B' = v_B − J/m_B`. If A and B each compute `J` from
  their own view of the pair, the two `J`s differ whenever their views differ —
  and under per-entity authority (D42/D7) with replication lag they *will*
  differ. Momentum is conserved exactly only if a single computed `J` is shared.
- **Detection.** A ship's client observes rocks via replication and can claim.
  An autonomous rock observes nothing: its step sees own state and delivered
  events only (`honest_inputs` drives pilots host-side; rocks get no pilot —
  `mod.rs:1063-1072`). Any design must say who *notices* a collision for a
  body that cannot notice one.

## 4. The approaches, compared

The property that decides this is named up front: **which designs can make two
honest peers disagree** (a false deviation — the IV-2 failure class). Elegance
is secondary.

### 4.A Symmetric claim-and-verify with a single-computation impulse echo

The Damage pattern, made mutual by a two-leg event round trip.

- **t** — the party that can observe (the ship; see detection in §4.A.1)
  emits `Outcome::Contact { target, my_pos, my_vel, my_radius, my_mass }` —
  every field a function of its own state, like `DamageDealt`.
- **t+1** — the counterparty's own step receives it as `Order::Bump { … }` and
  is the **verifier**: integer-exact overlap predicate against own state,
  `|p_own − p_carried|² ≤ (r_own + r_carried)²` in i128 mm² (the
  `nonnegative_distance_squared` shape that already exists), plus an approach
  check (`(v_rel · p_rel) < 0` — bodies moving toward each other) so a
  separating pair cannot be re-collided. If it verifies: compute the impulse
  **once**, here, in integer math —

  ```
  n     = p_own − p_carried                (contact normal, i128 mm)
  v_rel = v_own − v_carried                (i64 mm/s)
  j     = −(1 + e) · (v_rel · n̂) · (m_own · m_carried) / (m_own + m_carried)
  J     = j · n̂                            (i64 mm/s · mass-units, e = restitution in /1024 fixed point)
  ```

  apply `v_own += J / m_own` to own state (saturating, D43(f) posture), and
  emit `Outcome::ContactResolved { other, impulse: J, at_pos: p_own }`.
- **t+2** — the initiator receives `Order::BumpResolved { impulse, … }` and
  applies `v_own −= J / m_own` after a **bounds check** it can do entirely from
  own state: `|J| ≤ (1 + e_max) · m_reduced_max · |v_max_rel|`, with
  `|v_max_rel|` bounded by the two archetypes' speed caps — an out-of-bounds
  impulse is dropped and the drop recorded (an own-state counter, inside the
  claimed bytes, the D43(f)/D46(e) flag shape).

Momentum is conserved **exactly in the exchanged quantities by construction**:
one `J`, two applications with opposite signs. What is *not* guaranteed is that
the exchange completes — see failure modes, §6.

**False deviations: none.** Every input either party's step consumes is a
logged record (`InboundEvent` / player order); replay is closed; the executor,
harness, gate and protocol are untouched. This is the decisive property.

**Costs.** Two ticks of latency between contact and the initiator's bounce
(33 ms at 60 Hz — under the 250 ms sustain window, invisible next to
prediction); carried kinematics trusted at step time exactly as `Order::Damage`
already is (§2 caveat); the initiator's velocity change is computed by the
counterparty, sanity-bounded rather than verified — a cheating counterparty can
shove the initiator *within the physically-plausible envelope*, which is the
same envelope it could achieve by ramming honestly.

#### 4.A.1 Detection, per pair type

- **Ship–rock:** the ship initiates, always. Its client observes the rock via
  replication; the honest client submits automatically (#390 clause 3's
  reasoning: "A client that never claims stays locked" inverts here to "a
  client that never claims clips through rocks on everyone else's screen while
  its own hull takes the bounce late or never" — but see §6 on the
  non-claiming ship). Alternatively — and this is the recommended variant —
  detection needs no client order at all: the *ship's own step* holds
  `own.pos/vel` and the rocks are delivered to it… they are not; a rule cannot
  see rocks. So detection is client- or host-submitted. Both are untrusted
  hints; the rock verifies.
- **Ship–ship:** both can observe, so both could initiate, and a naïve double
  initiation applies the impulse twice. Deterministic role rule: **the lower
  `PersistId` of the pair initiates; the higher verifies and computes.** A
  `Bump` arriving at the lower-id party from the higher-id party for a pair
  the lower also initiated that tick is ignored by rule (own-state decidable:
  the order names both ids). This is an *initiator* asymmetry only — the
  physics is still computed once by the verifier and echoed.
- **Rock–rock:** neither observes. Out of #441's scope (its three items are
  velocity, rock–ship, ship–ship), and this design deliberately does not cover
  it. If ever wanted: host-submitted claims (the authority's broad phase
  submitting `Bump` orders as a system input source) — a new input class with
  its own trust story; or approach 4.D.
- **Per-pair cooldown:** after a resolved contact, both sides hold a
  `contact_cooldown: u16` (own state) during which further `Bump`s from the
  same counterparty are rejected — prevents overlap-resonance double hits
  while the pair separates. Own-state decidable; no neighbour needed.

### 4.B A collision arbiter

One of the pair resolves both deltas and emits the other's to it. Two
sub-variants, both examined:

- **Lowest-`PersistId` arbiter.** Fails on detection, not on resolution: the
  arbiter still cannot *see* the other body (no neighbour reads), so the pair's
  kinematics must be carried to it by event or claim — at which point the
  design has become 4.A with the roles renamed and one leg wasted. And for
  ship–rock the rock may hold the lower id while being constitutionally unable
  to observe anything, so the role rule must be "the party that can observe"
  anyway — which is 4.A.1 verbatim.
- **The cell's authority / field host as arbiter.** Under D42 per-entity
  authority the two bodies may be held by different peers; a third-party
  arbiter would need *both* entities' states, i.e. it is a cross-entity reader
  — exactly what the replay model cannot adjudicate today (single-entity
  harness, replay.rs:116-130). Making the arbiter's resolution adjudicable
  requires either NeighborFrame closure (then this is 4.D) or trusting the
  arbiter (then collisions are unwitnessed cluster fiat, a tier regression for
  state that D46 classifies as core the moment hull damage rides on impact).

**False deviations: none** in the event-carried variant (same closure argument
as 4.A). **Named asymmetry:** the arbiter's word sets both velocities; the
bounds check is the only defence the non-arbiter has. Verdict: 4.B collapses
into 4.A under this tree's constraints, minus 4.A's symmetry of scepticism.
Rejected as a distinct approach; its one useful residue (deterministic role
assignment by id for ship–ship) is absorbed into 4.A.1.

### 4.C Ambient static geometry only

What VC-8 actually permits, verified: "Sole ambient exception: immutable,
content-hash-pinned static geometry, which is part of the build"
(docs/06 §4 VC-8), and §3: "Immutable, content-hash-pinned static geometry
(shipped level content) may be read ambiently — its pinned hash makes it part
of the build, not an input." Mutable terrain is expressly *not* this tier
(non-core invariants only), and `StateView::geometry()` — the recorded
`GeometryFrame` path for journaled terrain — is in the deferred list
(docs/06 implementation status: "the `GeometryFrame`, `FieldFrame`, … record
sources" have no producer).

The tree already contains the degenerate example: rocks reflect off the island
boundary by integer velocity negation against a compile-time constant
(`ISLAND_BOUNDARY_MM`, `mod.rs:624-632`) — collision against build-pinned
geometry, fully replayable, zero protocol surface.

**As an answer to #441 it fails outright:** item 2 *is* "asteroids carry their
own velocity", and a moving, splitting rock is not immutable build content. An
asteroids-as-static-field design forecloses the milestone's own second item.
**False deviations: none** (ambient-pinned input is part of the build).
Verdict: rejected as the collision mechanism; retained as the correct tier for
any future *static* world geometry (station walls, arena obstacles), which
should never be routed through events or neighbour reads.

### 4.D Closing the `NeighborFrame` gap

Make the ban unnecessary: build the producer, and let a rule read a
neighbour's tick-start quantized state as a recorded, replayable input.

**This is not hypothetical in this tree.** #390's approved design (owner,
2026-08-24) already commits to most of it, and #444 executes it in front of
#329: clause 2 — "`NeighborFrame` carries full quantized state — the encoding
`StateClaim.state_hash` already commits to, so cross-check is one blake3";
mechanical steps 1–4 — executor emits frames off `StateView::recorded_reads()`
(the promise already sits in `crates/orrery_core/src/ruleset.rs:141-142`:
"The executor turns these into `NeighborFrame` records"), replay serves
`view.neighbor()` **from the recorded frames** (staying single-entity — the
harness never installs a second entity; docs/06 §3: "replay never needs the
neighbor's live state"), `verify_bundle` rejects over-read windows
(`MAX_NEIGHBOR_READS = 4`, claim-rate cap k = 15), and `core-gates.sh` §5
narrows from "no neighbour reads" to "none outside the audited predicate
module".

**Honest price list**, beyond what #444 pays anyway:

1. **A protocol version bump with an exact-equality handshake.**
   `crates/orrery_protocol/src/gateway.rs:182`: `protocol_accepted(current,
   offered) = (offered == current)`. Changing `NeighborFrame`'s payload changes
   the postcard wire encoding; every non-upgraded participant is refused.
   #390 flagged this as the blocker that "surprised me". Riding #444's bump is
   free; landing a *second* bump later is not.
2. **Staleness must be pinned or it becomes an accusation.** The frame records
   what the reader's replica held, which lags the neighbour's authority by
   replication delay. If the stage-two cross-check compares the recorded frame
   against the neighbour's claim *at the reader's tick*, honest lag reads as
   fabrication — a false-accusation generator aimed at honest peers. The frame
   must therefore carry **the neighbour tick/claim its payload corresponds
   to**, cross-checked against that claim exactly (one blake3), plus a bound
   `|t_read − t_claimed| ≤ staleness_cap`. #390's clause 4 margin
   (δ ≥ 2·ε_pos) exists for the same reason on the geometric side. This is a
   design obligation on #444, not new work created by collision.
3. **Conservation is still not free.** If each body reads the *other* via its
   own (differently-stale) frame and computes `J` locally, the two `J`s differ
   whenever the replicas differ — momentum error proportional to replication
   lag times relative acceleration. Exact conservation still requires the
   single-computation echo of 4.A. So 4.D buys *verified detection* (the
   predicate runs against cross-checkable state instead of carried assertions),
   not symmetric-and-exact resolution.

**False deviations: none at replay** — the recorded frame is the input, and an
honest authority's replay consumes what it recorded. The residual risk lives in
the stage-two cross-check (price 2); with declared-tick frames it is closed.

**Verdict:** right long-term substrate, already approved and en route via #444
— but it is a *detection upgrade* to 4.A, not a different resolution model, and
collision should not block on it.
