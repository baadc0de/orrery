# A12 — What the hardening shakedown teaches about exchanges, location, and the ECS question

**Status:** analysis node for the #395 tree, written after the fact against live
evidence · **Date:** 2026-08-26 · **Branch:** `docs/ecs-exchanges-lessons` at
`709f206d` · **Builds on:** the #395 tree (A1–A11), the Accepted D42–D49
records, `collision-under-own-state.md`, `lock-target-classes.md` · **Evidence
mined:** PRs #499, #501, #505, #506, #508, #510 and issues #498, #502, #503,
#504, #507 — the 2026-08-25/26 shakedown that took live play from *no shot
ever landing* to working.

This document answers one question: **what did the shakedown teach that should
become a more robust, reusable system across games — for location-based play
and exchanges between entities, shooting and collision above all?** It compares
Regolith as implemented today (ruleset v14, `crates/orrery_games/src/regolith/`)
against the `Ruleset` → `bevy_ecs` direction of the #395 brief and its groomed
outcome. Every code citation below was opened on this branch on 2026-08-26;
line numbers in the regolith crate drifted repeatedly across #492–#511, so
nothing is quoted from memory or from an earlier node without re-reading it.

One framing correction before anything else, because the question as posed can
mislead: **there is no longer a live "port to bevy_ecs" plan to compare
against.** The brief proposed one; the #395 tree critiqued it; and the outcome
is Accepted as D42 (`docs/adr/0042-…md:3`, Status: Accepted): canonical state
stays in the engine-neutral per-entity executor, the composition root and
`SimulationHost` seam land now, the shared Bevy world is rejected outright, and
a dedicated `bevy_ecs::World` is admitted only behind pre-registered triggers
T1–T3. Where this document says "the ECS port", it means the brief's original
hypothesis — canonical game logic as systems and components in a
`bevy_ecs::World` — and it evaluates that hypothesis against the shakedown,
which is the first body of *live-failure* evidence the tree has ever had to
test the #395 decision against. The headline is in §4: the evidence lands
almost entirely on D42's side, and this document says so even though it was
asked to look for reusable systems, not to re-litigate the decision.

---

## 1. The five failures, restated as measurements

The shakedown's value is that every failure carries numbers. They are used
throughout, so they are tabulated once, verbatim from the PR records (each
re-read today; the in-tree artifacts of each fix verified at the cited lines).

| # | Failure | The measured fact | In-tree artifact of the fix |
|---|---|---|---|
| F-A | **Two coordinate frames** (#498 → #499) | Client spawned on the 150 m scenario ring (`SPAWN_RADIUS_MM = 150_000.0`, `crates/orrery_games/src/regolith/mod.rs:32`); host bots on the 2.5 km crowd orbit (`CAMPAIGN_ORBIT_RADIUS_M = 2_500.0`, `mod.rs:89`). Targets sat ~**2,234,690 mm** away against ~**400,000 mm** of weapon reach. Symptom: `OutOfArc` at every heading of a 360° sweep, lock breaking only when the player's back was turned | `campaign_spawn_pose` / `campaign_orbit_radius_m` (`mod.rs:1638-1652`) is now the single home; the client calls it (`clients/regolith/src/campaign.rs:391`) and `gates/p1-swarm` delegates to it |
| F-B | **Firing-time fact re-decided during flight** (#498 reopened → #501) | At tick 505 the shot was inside the arc by **56,221 µrad** and still resolved `OutOfArc`, because the arc was re-evaluated on every projectile continuation against the target's *moved* position and the attacker's *frozen* firing-time yaw. Session tally: **0 `Hit`, 0 `Miss`** — the tell that the shot died upstream of the damage roll | The arc is decided once, on the initial delivery: the `flight_ticks.is_none()` guard and its comment (`mod.rs:1001-1013`). Rules change → `REGOLITH_RULESET` v13 → v14 (`mod.rs:93-97`) |
| F-C | **Frozen ghost replica** (#502 → #505) | Client and host agreed on the attacker's position *exactly* and disagreed on the target's by **222,452 mm (X)** and **255,552 mm (Z)**: the client rendered a craft that had left its interest set at its last-known transform. One wrong input produced both symptoms at once — client distance **170,288 mm** (inside), host distance **439,797 mm** against a **403,000 mm** limit (`RangeExceeded`), bearing skewed **769,079 µrad** (`OutOfArc`) | `REPLICA_TTL_TICKS = 120` and `expire_stale_replicas` (`clients/regolith/src/campaign.rs:73-78`, `:1111-1126`), pinned by `campaign_replica_expires_instead_of_freezing_on_screen` (`campaign.rs:1434`) |
| F-D | **Version mismatch minted a doomed session** (#504 → #506, #507 → #510) | Deployed host at ruleset v13, client at v14. Admission returned **200**; the harness died with `Error: witness anchor names a different ruleset`, visible only in the host journal; the client saw a dial fail 15 s later for no stated reason | Admission refuses `403 ruleset_version_mismatch` before minting (service half, #506); the client sends `"ruleset_version": REGOLITH_RULESET.version` at join and surfaces the refusal (`clients/regolith/src/admission.rs:295`, test at `:1217`) |
| F-E | **Offline fallback indistinguishable from live** (#503 → #508) | After the failed dial the client continued into `ActiveSession::Local` with nothing on screen; the player "flew, aimed and fired against a dead offline world" and misdiagnosed their controls | The status pill (`LOCAL SANDBOX — CAMPAIGN DIAL FAILED`, `clients/regolith/src/lib.rs:233-238`) and `session_scope: "local"|"campaign"` on every telemetry envelope |

And the methodological finding, which this document treats as a sixth failure
rather than an anecdote: **two of these fixes (#499, #501) shipped with green,
mutation-checked regressions while live play was completely unchanged** — #502
records it in terms ("the harness is **not** reproducing the live path"). F-C
was found only by instrumenting both sides of a real session and diffing the
table above. §6 gives this its own system, because a test programme that can
be green while the game is broken is an architecture defect, not a QA defect.

---

## 2. What today's exchange model gets right — verified, not assumed

Regolith's shot pipeline is the tree's most complete proof that a cross-entity
exchange fits the own-state discipline (D46, Accepted: cross-entity effects
travel as events with one tick of latency; immediate effects only where "the
audience is the actor itself", `docs/adr/0046-message-class-semantics.md:163-165`).
The shape, re-verified on this branch:

1. **Propose from own state.** On `Order::Fire` with a mature, class-confirmed
   lock, the attacker rolls damage from its own `TickRng` and emits
   `Outcome::DamageDealt` carrying its *own* firing-time facts — `attacker_pos:
   origin`, `attacker_vel: firing_vel`, `attacker_yaw_urad`, archetype, weapon
   (`mod.rs:382-421`). Every field is a function of (own state, inputs, rng),
   so a fabricated payload diverges the *emitter's* replay.
2. **Route by one total function.** `Regolith::deliver` maps the event to
   `Order::Damage` on the target (`mod.rs:1488-1512`) — next-tick delivery,
   the D46 latency price paid once.
3. **Adjudicate where the state changes.** The target runs
   `projectile_resolution` (`mod.rs:985-1049`) against its own position and
   the carried evidence: integer CORDIC bearing (`integer_bearing_urad`,
   `mod.rs:1113-1149` — bit-exact, no platform float), i128 distance
   (`nonnegative_distance_squared`, `mod.rs:1211-1217`), a flight-time ladder,
   then a hit roll from the **target's** rng (`mod.rs:1036-1048`). Shield and
   hull mutation are own-state writes; the verdict returns to the attacker as
   `ShotResolved`/`Destroyed` events at t+1.
4. **Collision now works the same way, upgraded by recorded reads.** What
   `collision-under-own-state.md` recommended as approach 4.A with a 4.D
   detection upgrade has landed as exactly that hybrid: a ship submits an
   untrusted broad-phase candidate (`Order::Collide { other }`), the single
   audited read site pulls the counterparty's *recorded* neighbour frame
   (`view.neighbor`, `crates/orrery_games/src/regolith/visibility.rs:97` —
   the recorded-input closure is complete per `scripts/core-gates.sh:126-139`,
   with staleness bounded by `max_neighbor_staleness_ticks`), and
   `verify_collision` (`visibility.rs:158-227`) does the whole predicate in
   checked integer math: overlap (`distance_sq ≤ (r_a+r_b)²`), approach
   (`v_rel · n < 0`, refusing separating pairs at `:197`), deterministic
   ship–ship roles (lower `PersistId` never resolves, `:169`), and a
   **single-computation** impulse whose two halves are applied with opposite
   signs — own velocity immediately (`mod.rs:196-205`), the counterparty's via
   `Outcome::Collision` → `Order::CollisionResolved`, bounds-checked against
   the receiver's own speed limit before application (`mod.rs:564-571`,
   `velocity_within_limit` at `:1237-1243`). Momentum is conserved in the
   exchanged quantities by construction; the counterparty is shoved only
   within its own archetype envelope.

Three properties of this model deserve to be named as the things worth
generalizing, because the shakedown stress-tested all three and none of them
broke:

- **The validator is the party whose state changes.** In F-C the host refused
  a shot the client's screen said was good — and the host was *right*. The
  adjudication model correctly refused garbage input; the defect was upstream,
  in what the client believed. An exchange model in which the attacker's
  client decided hits would have silently converted the ghost into damage.
- **Carried evidence is replayable evidence.** #505's diagnosis table exists
  *because* the exchange carries explicit kinematics: attacker position, yaw
  and archetype could be compared field-by-field across the two sides and
  agreed exactly, isolating the one disagreeing input. An implicit-state
  exchange (each side reading its own world) has nothing to diff.
- **Integer-exact geometry removed a whole suspect class.** At no point in the
  three-day hunt was "the two sides computed different trigonometry" a live
  suspect; #498 eliminated the arc math on inspection and #499's instrumented
  bearings reproduced exactly. CORDIC bearings and i128 distances bought that.

## 3. Where it got it wrong — and every wrongness is a *seam*, not a rule

Against that, the five failures. The striking fact, checked failure by
failure: **not one of the five was in the exchange rule itself.** All five
were in the assembly around it — the layer that has no owner, no manifest,
and no conformance harness.

- **F-A: "where am I" was an assembly detail.** The ruleset owned spawn poses
  for scenarios (`spawn_pose`, `mod.rs:1619-1629`) but the *campaign* spawn
  existed twice — once in `gates/p1-swarm`'s bot, once in the client — and the
  two disagreed by a factor of ~16 in radius. Nothing checked that two
  processes joining one session agreed on the frame; the wire happily carried
  positions between two incompatible geometries. The fix (one shared function,
  `mod.rs:1631-1645`) is right and insufficient: its own doc comment records
  the residual — the client calls `campaign_spawn_pose(slot, slot + 1)`
  because `CampaignConfig` carries no crowd size (`campaign.rs:391`), so the
  two sides still agree on the ring but not the exact radius/arc, and the
  convergence machinery papers over it. The frame is shared by convention,
  not by contract.
- **F-B: commitment semantics were implicit.** The exchange freezes the
  attacker's geometry at firing time (right), but nothing in the vocabulary
  said *which of the carried facts were already decided*. The continuation
  ladder re-entered the same resolver, and the resolver innocently re-decided
  everything it could compute. The fix froze the arc verdict — but note,
  reported against the fix's completeness: **the range clause still re-runs on
  every continuation tick** (`mod.rs:1016-1023` sits *before* the
  `flight_ticks` match), measuring the target's *current* position against
  the attacker's *firing-time* position. That is the same mixed-time frame
  shape that produced F-B, surviving in the adjacent clause. It may be
  intended ("the projectile ran out of reach chasing a fleeing target"), and
  it is bounded — worst case a `LockBroken(RangeExceeded)` instead of a
  retroactive refusal-before-roll — but no test or comment marks it as a
  decision, and F-C showed 18 `RangeExceeded` at close range riding exactly
  this clause when fed ghost geometry. Flagged in §7 as an owner/ruleset
  question, not fixed here.
- **F-C: replica lifetime existed nowhere.** The client's render map had no
  concept of staleness at all — state stayed drawable forever. Meanwhile the
  *verification* side already had a rigorous staleness concept
  (`MAX_NEIGHBOR_STALENESS_TICKS = TICK_HZ`, `mod.rs:84`, enforced at replay
  per `core-gates.sh:134-138`). The tree contained two notions of "how old may
  a belief about a neighbour be" — one formal and enforced, one absent — and
  the absent one was the one the player aimed with. The fix adds
  `REPLICA_TTL_TICKS = 120` (`campaign.rs:78`), chosen to match the headless
  peer's lifetime *by convention* ("matching the headless peer's replica
  lifetime", #505) — a third constant, in a third crate, coupled to the others
  by a comment.
- **F-D: compatibility was enforced at one door out of two.** The gateway's
  exact-equality handshake exists and is tested
  (`crates/orrery_protocol/src/gateway.rs:182`, `protocol_accepted(current,
  offered) = (offered == current)`), and the witness anchor check that finally
  killed the doomed session is itself a compatibility check doing its job —
  at the wrong layer, after minting, visible to nobody. Admission — a separate
  service, in Python — knew about `client_rev` but not about the rules
  identity. A version check that lives in N places is checked in N−1. D49
  (compatibility manifest, Accepted) is precisely the generalization; F-D is
  its first live confirmation.
- **F-E: session identity was not a fact the system carried.** `ActiveSession`
  knew local from campaign internally; nothing downstream (screen, telemetry)
  was forced to consume that fact, so evidence and player were both deceived.

The pattern across all five: **the ruleset's own discipline (D46, integer
geometry, adjudicate-where-state-changes) held; everything that broke was in
the space between processes, which no discipline governed.** That is the
finding the reusable systems in §5 are built from.

---

## 4. Would the ECS port have prevented, worsened, or not touched these?

Failure by failure, against the brief's actual hypothesis (canonical logic as
systems/components in a `bevy_ecs::World`, brief
`ruleset-ecs-migration-brief.md:46-57`). "Orthogonal" is used deliberately and
often, because it is the true answer more often than not.

| # | ECS-storage verdict | Reasoning |
|---|---|---|
| F-A (frames) | **Orthogonal — but the *non-ECS half* of the brief addresses it** | Two processes each hosting a `bevy_ecs::World` would have had exactly the same two spawn functions; storage substrate never enters the failure. What would have prevented it is the brief's *kernel* owning "spatial cells and canonical coordinates" (brief `:136-138`) and the composition root making one game definition the single author of spawn geometry — both of which are the parts D42 adopted, no ECS required. F-A is the strongest live evidence yet that the brief's composition-root motivation was right and its storage conclusion separable |
| F-B (re-decided commitment) | **Would likely have made it worse** | The bug's shape is "re-adjudicate a committed fact against the current world". In today's model that mistake had to be *carried in* — the resolver only sees own state plus the event payload, so the entire bug fit inside one function and one guard fixed it (`mod.rs:1005`). In an ECS system, querying the target's live component *is the default idiom*; a projectile system that reads `Query<&Position>` fresh each tick is the natural first draft, and the frozen-at-commitment discipline would have to be imposed against the grain of the substrate. The own-state discipline did not prevent F-B, but it confined it, made it diagnosable from the event payload alone, and made the fix one line plus one rules-version bump. This is A3's E-7 finding ("the ECS features a canonical world would buy have zero consumers in canonical rules") wearing live clothes |
| F-C (ghost replica) | **Orthogonal to storage; the brief's *presentation-mirror contract* names it** | The ghost lived in the client's render map, not in canonical state. Brief phase 6 lists "Diagnostics for mapping and stale entities" and AOI-limited extraction as deliverables (`:733-739`) — the planned mirror contract is exactly where replica lifetime belongs, and F-C proves that contract is not optional polish but the difference between a playable and unplayable game. But nothing about `bevy_ecs` storage delivers it: the fix landed as a `HashMap<PersistId, Tick>` and a TTL with zero ECS involvement. Credit the *contract*, not the substrate. A single shared world (brief Variant B, rejected by D42) would have made this failure class *worse* — replica and truth in one world blurs exactly the distinction F-C turned on |
| F-D (version handshake) | **Orthogonal** | Admission is a Python service; the ruleset version is a constant. No storage model on the Rust side touches whether a join checks it. The brief's *manifest* section (`:581-609`) and D49 are the relevant plan, and they are storage-agnostic. Note the brief's manifest is also the only planned system that would have flatly prevented one of the five |
| F-E (silent fallback) | **Orthogonal** | Client session-state presentation. No canonical architecture variant changes it. The nearest architectural fact is A3's G14 (no field host exists; three hosts advance ticks independently) — the *existence* of an ambient local host to fall back into is a host-seam question, and D42's `SimulationHost` seam would at least make "which host am I running against" an explicit value rather than an accident |

Scorecard, stated bluntly: **prevented by canonical-logic-in-ECS: zero of
five. Likely worsened: one (F-B), plus one worsened under the shared-world
variant specifically (F-C). Orthogonal: the rest — with four of five
addressed by the parts of the #395 plan that survived grooming into D42/D49
(composition root, host seam, presentation-mirror contract, compatibility
manifest), all of which are storage-independent.**

Reported against this document's own convenience: this is *not* evidence that
the ECS question is settled forever. The shakedown exercised two crafts, tens
of rocks, and one island — nothing near trigger T1 (per-component policy
pressure on `CoreState`-as-one-enum) or T2 (store dominating tick cost). The
shakedown says nothing about scale. What it does say is that at the scale
where the game first met reality, every real failure was a seam failure, and
D42's ordering — contracts and seams before storage — is the ordering the
evidence retroactively endorses. Had the tree spent the same days porting
`step_craft` into ECS systems, all five failures would still have occurred,
and F-B would have been harder to find.

---

## 5. The reusable systems the evidence actually argues for

Five systems, each with: what it is, the failure that convicts today's shape,
what already exists to build on, and the proof obligation — how to demonstrate
it would have caught its failure (per the epic's own standard: a factored-out
system that catches nothing is refactoring, not robustness). These are
proposals for the owner and for the post-#395 implementation epics; nothing
here amends an Accepted record, and §7 lists what would.

### 5.1 One definition of "where": the session frame as a handshaken artifact

**Failure caught: F-A** (and the live half of F-A's residual).

A game today defines its coordinate frame as scattered constants: the scenario
ring (`mod.rs:32`), the island boundary (`ISLAND_BOUNDARY_MM`, `mod.rs:62`),
the campaign orbit (`mod.rs:89-91`), plus whatever a host process improvises.
The fix for F-A moved the campaign pose into the ruleset crate — the right
direction — but the frame is still not part of any identity the two sides
*compare*. Nothing refuses a session whose participants disagree about spawn
geometry; F-A was refused by the player's patience, three days later.

Proposed shape, kernel-owned (this is the brief's own kernel row, "spatial
cells and canonical coordinates", made concrete):

- A **session frame descriptor** — origin convention, boundary, spawn-pose
  function identity (a named constant or hash over the pose table), crowd
  size — minted once per session by the game definition, delivered in the
  invite, and *echoed back at join*. Mismatch refuses at admission with a
  readable reason, exactly as F-D's fix does for the ruleset version.
- The F-A residual closes as a side effect: `CampaignConfig` carrying crowd
  size stops the client guessing `slot + 1` (`campaign.rs:391`) — #499
  already assigns that repair to #387's session geometry; this system is
  where it should permanently live.
- Cheapest strong variant: fold the frame descriptor's hash into what D49's
  manifest already composes, so frame disagreement *is* manifest disagreement
  and needs no second mechanism. That keeps "one definition of where" as one
  comparison instead of a new protocol surface (OD-24's caution applies).

**Proof:** re-create F-A deliberately — build a client whose spawn table
disagrees (the old 150 m ring is sitting in git history) and assert the join
refuses with the frame reason; mutation = drop the frame field from the
echo and the named test must die. This is exactly the shape #506/#510 proved
for versions, reapplied to geometry.

### 5.2 The exchange protocol as a kernel vocabulary: propose → adjudicate → continuation, with commitment explicit

**Failure caught: F-B** (and it would have prevented the F-B-shaped residual
in the range clause from being ambiguous today).

The Damage pattern is currently an idiom — a shape Regolith happens to follow,
re-derived by every new exchange (`Grab`, `LockRequested`, `Collide` each
reinvent the round trip in `deliver`, `mod.rs:1488-1608`). F-B happened
because the idiom has no place to write down its one subtle rule: **facts
decided at commitment are decided once; continuations carry verdicts, not
re-derivations.** The `flight_ticks.is_none()` guard and its comment
(`mod.rs:1001-1004`) are that rule, hand-enforced at one call site, invisible
to the next game.

Proposed shape — a small, game-agnostic vocabulary in the kernel or a shared
games-support crate, deliberately *not* a framework:

```text
Exchange<Proposal, Verdict, Resolution>:
  propose(t)      — emitter freezes evidence E from own state; E is committed
  adjudicate(t+1) — target decides V = f(own state, E, rng) — exactly once
  continue(t+k)   — carries (E, V, remaining); may consume own state only to
                    advance the ladder, never to re-derive any field of V
  resolve         — terminal event(s) back to the proposer
```

with the commitment rule expressed in types: the continuation payload carries
the *verdict*, not the raw inputs the verdict was derived from — so
re-deciding is not a bug one guards against but a state the type cannot
represent. Regolith's projectile would carry `arc: Decided` rather than
re-deriving from `from_yaw_urad`; whether range is `Decided` or `Live` becomes
a visible, per-field choice the ruleset author must make and a reviewer can
see (§7 Q1 asks the owner which Regolith wants).

This is also where the lock handshake (`lock-target-classes.md` §3c, now
landed: `LockRequested`/`LockConfirmed`/`LockRefused`, `mod.rs:341-435`) and
the collision round trip become instances instead of siblings. Three
exchanges already exist; the fourth game should not write its own `deliver`
router by hand.

**Proof:** port `Order::Damage` onto the vocabulary and show #501's mutation
(delete the arc-freeze) is *inexpressible* — the continuation type has no yaw
to re-check — while #501's named regression
(`campaign_projectile_keeps_the_arc_verdict_it_had_when_fired`) still passes.
A vocabulary under which that mutation still compiles has failed its bar.

### 5.3 Replica lifetime and staleness as one concept, not three constants

**Failure caught: F-C.**

Today the tree holds three independent answers to "how stale may my belief
about another entity be":

1. verification: `MAX_NEIGHBOR_STALENESS_TICKS = TICK_HZ` (`mod.rs:84`),
   enforced mechanically at replay;
2. rendering/aiming: `REPLICA_TTL_TICKS = 120` (`campaign.rs:78`), enforced
   since #505, coupled to the peers' lifetime by a comment;
3. the headless peer's own replica lifetime, which #505 matched by hand.

F-C is what the gap between (1) and (2) cost: the *rules* refused to believe
anything older than one second about a neighbour while the *player* was made
to aim with beliefs of unbounded age. The asymmetry is exactly backwards from
what a player perceives — the screen is the most staleness-sensitive consumer
in the system.

Proposed shape: **staleness bounds are declared once, per ruleset, and every
belief-holder consumes the declaration.** `Ruleset` already exposes
`max_neighbor_staleness_ticks` (additive method, D21-safe, already shipped —
`crates/orrery_core/src/ruleset.rs:263`); the replica contract extends the
same declaration to mirrors: a replicated state is `(state, authority_tick)`,
a mirror may not present state older than the declared bound, and expiry is a
defined transition (despawn + focus release, as #505 implemented) rather than
each client's improvisation. The presentation-mirror contract D42's seam work
already owes (A9/brief phase 6) is the natural home; the point of this
proposal is that the *bound itself* comes from the ruleset declaration, so a
new game gets a coherent staleness story by writing one number. The
contract's other clause -- which side may assert what, independent of how
stale -- is named and defined in section 5.6.

**Proof:** #505's mutation already exists (TTL → `u64::MAX` kills
`campaign_replica_expires_instead_of_freezing_on_screen`); the new obligation
is a *cross-host* test — host and client both derive their lifetime from the
one declaration, mutation = hard-code either side's constant and a named test
diffs the two derived bounds. Today that test cannot be written, because
there is no shared source to diff against; that impossibility is the finding.

### 5.4 Compatibility as a handshake at every door — D49 confirmed, plus a door census

**Failure caught: F-D.**

The mechanism-level lesson of F-D is not "add a version check" — the tree had
two (gateway exact-equality, witness anchor) and still minted a doomed
session, because admission was a third door with no check. The lesson is:
**compatibility is a property of the set of doors, and the set was never
enumerated.** D49's manifest (Accepted) is the right artifact; what the
shakedown adds is the deployment-shaped requirement that every join path —
gateway handshake, admission service, witness anchor, and any future
matchmaker — consume *the same* comparison, and that the refusal reach the
human who can act on it (F-D's mismatch was visible only in a journal the
player never sees; #510's fix routes the reason to the dialog).

Concrete proposal: a **door census** as part of the manifest's acceptance —
a table in the D49 implementation epic listing every process boundary where a
session is created or joined, each citing the code that performs the manifest
comparison and the path the refusal takes to a human. A door added without a
census row is the review tripwire, the same mechanism `core-gates.sh` clause 5
uses for neighbour reads ("no code starts reading neighbours without a human
seeing it", `core-gates.sh:141-147`).

Also recorded, because it sharpens D49's priority: F-D is the only one of the
five failures a *planned* #395 system would have outright prevented. The
manifest is not speculative infrastructure; it has a body count.

**Proof:** #506/#510's mutations stand (wrong version sent → named tests die
on both halves). The census's own proof: stand up the v13-host/v14-client
pair from F-D in the two-process harness of §6 and assert the refusal arrives
at the client dialog within the join round trip — i.e. re-run the actual
incident as a fixture, which is now cheap because both halves are code.

### 5.5 Session identity as carried fact

**Failure caught: F-E.** Smallest of the five, included for completeness: any
client that can fall back to a local world must carry the scope on-screen and
in every telemetry row (`session_scope`, shipped in #508). The reusable form
is a rule, not a mechanism: **evidence channels must be partitioned by session
scope at write time** — a telemetry row that could be mistaken for campaign
evidence is corrupted evidence, and F-E showed the corruption costs more than
the outage. Mutation exists
(`local_fallbacks_cannot_present_or_serialize_as_live_campaigns`). Cross-game
cost: one enum on the envelope. Nothing more is proposed.

### 5.6 Two authorities: simulation authority and visual authority

*(Amendment, 2026-08-27, closing #519. The evidence here post-dates this
node's original shakedown window: it is the #505 ghost, the #517/#518 tracer
pair, the #522 burst gate and the #533/#536 boundary fade, each citation
re-read on `main` at `5ee8bfd1` on 2026-08-27. This section defines the
vocabulary; it proposes nothing beyond a definition and a review test, and
section 7.7 records what remains owner-reserved.)*

The presentation-mirror contract (section 5.3) bounds *how stale* a mirror may
be. Its other clause is *which side may assert what*, and the shakedown's
aftermath produced a vocabulary for it that is now used in seven doc-comments
across `clients/regolith/src/` (`combat.rs:552`, `aoi.rs:12`, `aoi.rs:236`,
`starfield.rs:51`, `lib.rs:265`, `lib.rs:1119`, `lib.rs:1456`), in A13 section
3 (which builds a third tier, hearsay, on top of it) and throughout A14 --
everywhere by reference to #519, nowhere with a definition. This section is
that definition.

- **Simulation authority** -- *what happened.* It is the ruleset's,
  exercised under own-state discipline and expressed only as adjudicated
  artifacts: hashed per-entity state, and the D46 event classes above
  diagnostics (`docs/adr/0046-message-class-semantics.md:106-140`). In
  Regolith terms: the muzzle `DamageDealt`
  (`crates/orrery_games/src/regolith/mod.rs:503`), the target-authored
  in-flight continuations (`mod.rs:556-566`), and the verdicts
  (`Outcome::ShotResolved`, e.g. the `Hit` arm at `mod.rs:593-598`).
  Presentation never infers it, predicts it, or fills gaps in it.
- **Visual authority** -- *how it is shown.* It is the skin's: framing,
  interpolation, smoothing, fades, labels, cues, and the timing of all of
  them -- **provided the skin asserts nothing the ruleset has not said.**
  D46 already states the mechanical half ("presentation feeds nothing
  canonical", `0046-message-class-semantics.md:136-137`); this names the
  perceptual half, which no data-flow rule can check: what the *player* is
  made to believe.

The failure mode the distinction exists to prevent: **a skin that quietly
acquires simulation authority** -- drawing a claim the simulation never made,
so the player believes something the ruleset would deny. F-C is the canonical
conviction: the frozen ghost of #502/#505 was the skin asserting "target 3 is
here, now" long after replication had stopped saying so, and the player aimed
at a position 222,452 mm (X) and 255,552 mm (Z) from where the adjudicating
host held the target -- rendered range 170,288 mm against a measured
439,797 mm and a 403,000 mm limit (section 1 F-C; #505's capture table). The
host was right to refuse; the screen was lying.

#### The rule, in checkable form

> **The skin may interpolate between facts the ruleset has stated; it may
> never extrapolate into facts the ruleset has not stated.**

Made mechanical, so a reader can classify a piece of skin code. For a drawn
element `E`, let `F(E)` be the set of ruleset-authored facts it derives from:
events this client received, plus replicated hashed state no older than the
section 5.3 staleness bound. `E` is within visual authority iff all three
hold:

1. `E = f(F(E), camera, style)` -- a pure presentation function of those
   facts plus camera and styling inputs, feeding nothing back (the D46
   presentation-class rule).
2. Every world-predicate a player would read off `E` -- "this exists", "it
   is here", "it was hit", "it is named X", "you may not see past here" --
   is entailed by some member of `F(E)`. Values *between* two members
   (positions along a stated flight, transforms between refreshes) are
   entailed; values or predicates *beyond* them (an outcome before its
   verdict, a position after the bound, an invented name) are not.
3. If `F(E)` is empty, `E` renders absence or visible uncertainty -- no
   placeholder that could be mistaken for a fact.

The review test that falls out of clause 2 is grep-shaped: **any skin code
that draws an outcome must destructure the ruleset's outcome event verbatim,
and any value drawn without a stated endpoint must be frozen, aged, or
faded -- never advanced by a skin-side model of the world.** A `match` on
`ShotResult` copied out of `Outcome::ShotResolved` is on the right side; a
`predicted_hit` flag computed client-side would be on the wrong side, however
accurate.

#### The worked cases, each verified against the tree

| Case | The fact set `F` | What the skin does | Side of the line |
|---|---|---|---|
| **#505 frozen ghost** (F-C) | replication stopped; `F` aged past any bound | pre-fix: kept drawing the last transform indefinitely | **Violation** -- the conviction. Fix: expiry as a defined transition, `REPLICA_TTL_TICKS = 120` (`clients/regolith/src/campaign.rs:79`, enforced at `campaign.rs:1212-1226`) |
| **#517/#518 invisible tracer** | shooter receives one muzzle `DamageDealt` with `flight_ticks: None`; continuations are target-authored and never arrive (#518 measured 81 muzzle events, zero continuations) | pre-fix: drew nothing -- correctly declined to draw what it had not been told. It *looked* like a bug and was the contract working; the owner's read on the adjacent no-tracer hit: "it exercises the case where visual authority is with the skin" (#519, quoting PR #518) | **Compliant**, and the motivating example |
| **#518 presentation-only flight** | the muzzle event plus the target's last replicated position | interpolates a flight using the ruleset's own timing (`projectile_flight_ticks`, `combat.rs:263-270`; `crates/orrery_games/src/regolith/mod.rs:1329`), endpoint **frozen** (`Track.destination`, `combat.rs:169-173`), arming no arrival cue | **Compliant interpolation** -- motion between two stated facts, asserting no outcome. Mutation pinned: `a_campaign_muzzle_interpolates_without_claiming_an_arrival` |
| **Provisional cue** | final in-flight tick observed (`flight_ticks == 1`) | `ShotFeedback::arm_provisional` (`combat.rs:471-484`) claims only "an adjudication is due" -- `IMPACT...`, never a result | **Compliant** -- the cue's predicate is itself a stated fact (timing), not a predicted outcome |
| **#522 impact burst** | `ShotCue::Resolved { result: ShotResult::Hit }` -- the target's adjudication, transcribed (`combat.rs:428-436`) | `impact_burst` draws only on that cue (`combat.rs:569-576`); before #522 it drew on the provisional arrival too, announcing a miss as a hit for one tick (`combat.rs:552-554`). #536 then sized it from the ruleset's own `radius_mm` (`clients/regolith/src/hud.rs:886-890`, `:159-164`) and anchored it on the *thing* hit, not a millimetre the ruleset never transmitted (`combat.rs:556-567`) | **Violation, caught and fixed** -- the quiet-acquisition failure in miniature |
| **Roster labels** | admission's roster rows; nothing replicated | a craft with no sanitised name gets *no label at all* -- deliberately no "UNKNOWN"/"PLAYER 3" placeholder that could be mistaken for a chosen name (`clients/regolith/src/roster.rs:24-27`, `roster.rs:156-160`) | **Compliant** -- clause 3: absence rendered as absence |
| **#533 AOI fade** | position relative to a boundary derived from the session's own cell edge | fades on *distance to the interest boundary*, describing the client's knowledge thinning -- "a faded craft is not damaged, not cloaked, and not further away than it is" (`clients/regolith/src/aoi.rs:12-16`); explicitly never a fade on staleness, which #505's expiry owns (`aoi.rs:18-26`) | **Compliant** -- uncertainty drawn as uncertainty |
| **Offline sandbox boundary** | no host, no interest set: no boundary fact exists | draws no fade at all, "drawing one would be the skin asserting a limit the run does not have" (`aoi.rs:231-236`, `lib.rs:262-265`) | **Compliant** -- clause 3 again, in the negative |

Two boundary notes that keep the rule honest rather than absolutist. First,
visual authority is real authority: the skin *owns* how things are shown, and
"asserts nothing new" does not mean "adds nothing" -- the #518 tracer, the
fade band, the burst's sizing are all skin decisions the ruleset neither
makes nor could. Second, a provisional cue that a later authoritative event
contradicts is explicitly accepted (`combat.rs:428-436`): visual authority
may be *early* about a stated fact; it may not be *inventive* about an
unstated one.

#### The testing consequence

A test where one client owns both parties collapses the two authorities into
one process: everything the skin might wrongly assert, the same process also
adjudicates, so no disagreement can appear and the distinction is
unexercisable. Live campaign play is the first configuration that separates
them -- client owns the shooter, host owns the target -- which is why #517
reached a player through a green tracer suite, the same root shape as the
#502 and #514 misses. This is section 6's argument restated at the
presentation layer, and it adds a requirement to section 6's two-process
harness: at least one assertion must hold *rendered* claims against
*adjudicated* facts across the process boundary (section 6's
"positions rendered and positions adjudicated agree within the replication
bound" is exactly that assertion).

#### Where this should live

This node keeps the definition here because A12 is where the
presentation-mirror contract is argued from evidence, and A13 section 3 and
A14 section 7 already cite #519 for it. If the contract graduates into an
implementation epic (D42's seam work), the two-authorities clause should
travel with it, and -- because A13's hearsay tier (H1-H5) is defined *as*
"neither authority" and would inherit any drift in these definitions -- a
short ADR naming the tiers (simulation authority / visual authority /
hearsay) is worth considering at that point. **Recommended, not decided:**
accepting ADRs is the owner's alone, and nothing in this section amends an
Accepted record. D46 needs no amendment for this: its presentation-class
rule is the enforcement mechanism this section gives a name to.

---

## 6. The sixth system: the harness must include the seams, because that is where all five bugs lived

The methodological finding, promoted to a system of its own.

The existing programme — goldens, mutation checks, the four-platform matrix,
A10's fixtures — verifies the *ruleset* and increasingly the *executor*. The
shakedown demonstrated, twice in one day, that this entire apparatus can be
green while no shot lands: #499's tests stopped at the first delivery, #501's
went to the roll but started from hand-built geometry, and neither could ever
have seen F-A, F-C, F-D or F-E because **those failures live in inputs the
harness synthesizes** — spawn frames, replication freshness, deployed
versions, session routing. G7 already warned that goldens see only state
hashes; the shakedown generalizes the warning: *every* single-process test
sees only what one process believes, and all five failures were disagreements
between two processes' beliefs. Section 5.6 adds the presentation-layer form
of the same warning: a single-process test collapses simulation authority and
visual authority into one owner, so a skin asserting more than the ruleset
said (F-C) and a skin correctly declining to assert what it was never told
(#517) are both invisible until two processes hold the two authorities.

Two components, both with in-tree seeds:

1. **A two-process session conformance harness.** Real admission (the Python
   service under self-test), a real host, a real client runtime headless, one
   real join, N ticks of real replication, and assertions on the *session
   outcome* — at minimum: a shot fired at a locked, in-range, in-arc target
   yields `Hit` or `Miss` (F-B's tell was `0 Hit ∧ 0 Miss`; that predicate is
   a one-line invariant worth pinning forever), positions rendered and
   positions adjudicated agree within the replication bound of §5.3, and the
   incident fixtures of §5.1/§5.4 refuse at the right door. #499/#501/#505
   each built fragments of this (live campaign fixtures in
   `clients/regolith/tests/`, the real-bot-step regression); the proposal is
   to make the assembled two-process form a standing gate rather than a
   post-incident artifact. This is also the natural home for D42's
   `SimulationHost` seam to prove itself: one host API, driven identically by
   the harness and the shipping client, is what makes "the harness reproduces
   the live path" a structural property instead of a hope.
2. **Dual-side geometry capture as a permanent, cross-game facility.** The
   instrument that actually found F-C — both sides logging the inputs to every
   refusal, diffed field-by-field — was built ad hoc inside #505 ("both sides
   gained opt-in live geometry capture") on top of #501's
   `firing_arc_measurement` refactor (`mod.rs:1084-1111`), which exists
   precisely so the geometry is "measurable in future rather than
   reconstructed by hand". Generalize: every exchange adjudication can emit,
   under an opt-in diagnostic flag, the tuple (carried evidence, own-state
   inputs, verdict, margins) — D46-class diagnostic events, feeding nothing
   canonical — and a small diff tool aligns the proposer's and adjudicator's
   tuples by (entity, tick). The §5.1 table format *is* the tool's output
   format. Cost when off: zero; the events are diagnostics-class by
   construction.

**Proof for the harness itself** — the check that it reproduces the live
path, which is the property #502 proved the old suite lacked: re-introduce
each of the five shipped defects (each is a small, known diff: the old spawn
call, the deleted freeze guard, TTL → ∞, version field dropped, scope
mislabeled) and require the session harness to fail on **every one** with a
named assertion. Five mutations, five kills, or the harness has not earned
the word "conformance". That is the same break-stage/named-check discipline
the tree already applies to gates, aimed at the layer that had none.

---

## 7. Open questions and what this document could not determine

Named rather than smoothed over; owners identified where they exist.

1. **Is the live range re-check during projectile flight intended?**
   (`mod.rs:1016-1023` runs on every continuation, mixing the target's
   current position with the attacker's firing-time position — §3 F-B
   discussion.) If intended, it deserves a comment and a characterisation
   test the way #501 pinned the arc; if not, it is a latent F-B sibling.
   Rules semantics → the owner, via a ruleset issue; changing it is a
   `REGOLITH_RULESET` bump.
2. **`Order::Collide` has no production submitter.** The full adjudication
   pipeline for collision exists and is tested through the executor, but
   `rg Collide` over `clients/`, `gates/` and `pilot.rs` finds no site that
   ever submits the claim — the client maps it to `None`
   (`campaign.rs:324`), the deterministic pilot never emits it, the swarm bot
   never emits it. Live play cannot collide with anything today. This is
   the exact green-but-dead shape the collision design document caught for
   rock velocity (its F1: integration existed, every spawn passed zero), one
   layer up: the rule exists, the *input source* does not. It is also a
   ready-made first assertion for §6's session harness ("fly into a rock;
   `collisions` increments"). Not verified: whether a submitter is already
   planned in an open issue this document did not find.
3. **Whether §5.2's exchange vocabulary can stay additive under D21.** It is
   designed to live beside `Ruleset` (games-support code, no trait change),
   but if it ever wants kernel enforcement it touches the frozen surface.
   D21 reopening is the owner's alone; this document deliberately shaped the
   proposal to not need it, and flags the risk that a "vocabulary" hardens
   into the god-abstraction the brief warned about (`:877-884`). The
   mitigation is the brief's own: adopt it only over the three exchanges that
   already exist, never speculatively.
4. **Where the session-frame descriptor belongs** — inside D49's manifest
   composition (preferred in §5.1, one comparison) or as session-setup state
   beside OD-24's schedule-digest question. Both are protocol-adjacent;
   OD-24 is already reserved, and this rides with it. Owner's call at D49
   implementation time.
5. **Scale evidence for the ECS triggers is still zero.** §4's scorecard is
   honest about direction but rests on a two-craft shakedown; T1/T2 remain
   exactly as untested as A3 left them. Nothing here strengthens *or*
   weakens the trigger conditions, and this document must not be cited as
   anti-ECS evidence at a scale it never observed.
6. **Not verified:** the #505 claim that a post-fix live `Hit` was eventually
   observed. The PR itself says so explicitly ("a live `Hit` has not been
   observed" at merge time), the task brief for this document says live play
   "went to working", and #511's merge (`709f206d`, tick-chain test cleanup)
   implies continued live sessions — but this document found no committed
   artifact (session log, issue comment) recording the first live `Hit`, and
   did not run a live session itself. If none exists, recording one is worth
   an issue: the shakedown's own lesson is that "working" must be a measured
   fact somewhere.
7. **Whether the two-authorities vocabulary (section 5.6) should become an
   ADR.** Section 5.6 recommends a short tiers ADR (simulation authority /
   visual authority / hearsay) *if and when* the presentation-mirror
   contract graduates into D42's implementation epic, because A13's hearsay
   rules are defined against these two and would inherit any drift.
   Accepting ADRs and amending Accepted records is the owner's alone; until
   then the definition lives here and D46 stands unamended as the
   enforcement mechanism.

## 8. Verification log and stale-citation notes

- Every `path:line` in this document was read on branch
  `docs/ecs-exchanges-lessons` at `709f206d` on 2026-08-26. Key anchors
  re-verified rather than inherited: `REGOLITH_RULESET` v14 (`mod.rs:93-97`);
  the arc-once guard (`mod.rs:1001-1013`); the pre-ladder range clause
  (`mod.rs:1016-1023`); the `Order::Damage` adjudication arm
  (`mod.rs:436-522`); `deliver` (`mod.rs:1488-1608`); `campaign_spawn_pose`
  (`mod.rs:1631-1645`); `verify_collision` (`visibility.rs:158-227`) and the
  single read site (`visibility.rs:97`); `REPLICA_TTL_TICKS` and expiry
  (`campaign.rs:73-78`, `:1111-1126`); the client's join `ruleset_version`
  (`admission.rs:295`); the status pills (`lib.rs:233-238`);
  `protocol_accepted` exact equality (`gateway.rs:182`).
- **Drift found against the predecessor documents this one builds on:**
  `collision-under-own-state.md` §7 step 8 requires "any `view.neighbor()`
  use" stay out and the gate "pass unexempted" — superseded by its own §0
  re-derivation and now by the landed code: `view.neighbor` is live inside
  the audited predicate module (`visibility.rs:97`) under D43(d), and
  `core-gates.sh` clause 5 has become a declared-site tripwire that
  deliberately does **not** count occurrences (`core-gates.sh:141-147`),
  which also supersedes `lock-target-classes.md` §7's "site count pinned at
  one" framing. Its cited `visibility.rs:32` for the read site is likewise
  drifted (now `:97`). Both documents' conclusions survive; the enforcement
  mechanism they describe is one generation old.
- The five failure tables in §1 quote the PR/issue records verbatim; the
  measured numbers (2,234,690 mm; 56,221 µrad; 222,452/255,552 mm;
  170,288 vs 439,797 vs 403,000 mm; 769,079 µrad; 2,478 fires → 99
  resolutions → 0 hits) were not re-derived from raw session logs, which are
  not in the repository. They are treated as evidence of record, and §7.6
  notes the one place that chain has a gap.
- No code outside `docs/plans/` was changed. No ADR is amended; §5's
  proposals are proposals, and the decisions in §7 belong to their named
  owners.

### Addendum: section 5.6 verification (2026-08-27, #519 amendment)

- Every `path:line` in section 5.6, the section 5.3 pointer, and section 7.7
  was read on `main` at `5ee8bfd1` on 2026-08-27. Key anchors:
  `ShotCue`/`ShotFeedback` docs and `arm_provisional`
  (`clients/regolith/src/combat.rs:428-436`, `:471-484`); `impact_burst`'s
  gate and its #519-citing doc comment (`combat.rs:546-576`); the
  presentation-only `Track` fields and the muzzle-flight build
  (`combat.rs:169-173`, `:263-281`); the muzzle, continuation and `Hit`
  emission sites (`crates/orrery_games/src/regolith/mod.rs:503`,
  `:556-566`, `:593-598`); roster absence (`roster.rs:24-27`, `:156-160`);
  the fade's claims (`aoi.rs:12-26`, `:231-236`); D46's class rules
  (`0046-message-class-semantics.md:106-140`).
- **Drift in this document's own original citations, recorded rather than
  silently rewritten:** section 5.3 cites `REPLICA_TTL_TICKS = 120` at
  `campaign.rs:78` (read at `709f206d`); on today's tree it is
  `campaign.rs:79`, expiry at `:1212-1226`. The value and the conclusion
  are unchanged.
- The #505 disagreement figures (222,452 / 255,552 mm; 170,288 vs
  439,797 vs 403,000 mm) are quoted from #505's capture table, as section 1
  already does; the #518 figures (81 muzzle events, zero continuations;
  1,371 post-fix tracer samples over 817 ticks) are quoted from #518's
  measurement and live-evidence sections. Neither was re-derived from raw
  session logs, which are not in the repository.
- **Not verified:** the owner's comment on PR #518 is quoted in the form
  issue #519 records ("it exercises the case where visual authority is
  with the skin"); this amendment did not locate any longer or earlier
  phrasing and quotes only that sentence. Also not verified: whether any
  in-flight work already drafts the tiers ADR that section 7.7 defers to
  the owner.
