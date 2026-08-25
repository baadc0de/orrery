# What a lock on a rock means (#442 design)

**Verdict: a lock on a rock is a combat lock, because rocks are already combat
targets.** The fork #442 poses — targeting affordance versus combat target —
dissolves against the current tree: rocks carry hull, adjudicate incoming
damage in their own step, split, drop weapon pickups, and pay resolver-owned
score points to their killer **today**. Three of the issue's founding premises
("a body that may have no signature radius, no velocity of its own until #441
lands", "an asteroid that cannot be damaged") are stale against the code (§1).
What is genuinely missing is not a meaning for the lock — it is (a) an
**adjudicated target-class predicate**, because today a lock will happily name
a pickup, a bloom director, or an id that exists nowhere, and fire into the
void silently; (b) **class-differentiated feedback**, so a mining lock and a
combat lock read differently on the HUD; and (c) a deliberate answer to
**occlusion decay for rock locks**, which the LoS machinery currently forecloses
by construction rather than by decision. This document decides all three, keeps
the tracking maths exactly as it stands (it is already well-defined for both a
static and a moving rock, §4), and names the calls that are the owner's.

This is a design document. It changes no ruleset. Every code citation below
was read from `origin/main` at `191a8aa6` on 2026-08-25. The working checkout
this file lands in is **behind** that commit and #443 is in flight in this
same checkout, so line numbers cite the fetched snapshot, not the working
tree; §10 logs everything found stale or unevidenced, including in #442's own
text.

## 1. What the tree actually holds (verified), versus what #442 assumes

The brief and the issue describe an asteroid that "cannot be damaged", has "no
signature radius", and "no velocity of its own". None of the three survives
contact with `origin/main`:

- **Rocks are damageable and worth points.** `step_rock`
  (`crates/orrery_games/src/regolith/mod.rs:567-728`) adjudicates
  `Order::Damage` against the rock's own state: a `Hit` decrements `rock.hull`
  (mod.rs:622), a dead Large/Medium rock splits into two children
  (mod.rs:651-666), a dead Small rock rolls a weapon-pickup drop
  (mod.rs:667-682), and the killer is paid `Outcome::RockDestroyed { points }`
  (mod.rs:691-695) which `deliver` routes back as `Order::RockCredit`
  (mod.rs:1281-1283) into the killer's hashed `score_rock_points`
  (mod.rs:472-474). Mining rocks for score is a live economy, pinned by
  `rock_credit_is_log_delivered_with_resolver_owned_points`
  (`crates/orrery_games/tests/regolith.rs:401`).
- **Rocks have a signature radius, and it is already used as one.**
  `RockTier::limits()` (`crates/orrery_games/src/regolith/state.rs:92-114`)
  publishes `radius_mm`: 40 000 / 20 000 / 8 000 mm for Large/Medium/Small.
  `step_rock` passes exactly that value into `projectile_resolution` as
  `target_radius_mm` (mod.rs:593), where it feeds both the range reach
  (mod.rs:866-869) and the tracking denominator of `hit_chance_ppm`
  (mod.rs:990-991). There is no missing quantity to invent.
- **Rocks integrate their own velocity every tick, with boundary reflection**
  (mod.rs:714-726), pinned by
  `rock_position_integrates_velocity_on_all_axes_each_tick`
  (`crates/orrery_games/tests/regolith.rs:27`, landed as #449). What #441
  actually changes is not the integrator but the *inputs*: today every rock
  spawn path yields zero velocity — bloom rocks spawn with `QVel::default()`
  (`bloom_spec`, mod.rs:1158), and split children inherit the parent's
  velocity rotated and scaled by 1.4 (mod.rs:1094-1146), which maps zero to
  zero. So rocks are static in practice, by data rather than by rule.
- **"No authority holder in the same sense" is unevidenced.** A rock is an
  entity window like a craft: it is stepped by the same
  `Ruleset::step` dispatch (mod.rs:175-178), budgeted per island
  (`ISLAND_ROCK_BUDGET`, mod.rs:49), and its state is hash-encoded
  (`state.rs:362-386`). Nothing in the lock or damage path distinguishes who
  "holds" it, and this document found no mechanism that would need to.

Two more facts shape everything below:

- **A lock can already name a rock — or anything.** The `Order::Fire` arm
  (mod.rs:313-366) accrues `lock_progress` against an opaque `PersistId` with
  no class check of any kind; switching to a different id restarts acquisition
  from scratch (mod.rs:328-335, pinned by
  `fire_on_a_different_target_switches_the_lock_and_restarts_acquisition`,
  tests/regolith.rs:1129). The deterministic pilot's Mining and
  BloomConvergence scenarios lock and fire at *synthetic* rock-lineage ids
  (`pilot.rs:92-98,121-127`) that mostly name no live entity — the shot's
  `DamageDealt` is delivered to nobody and vanishes without a `ShotResolved`,
  a refusal, or any event at all. Locking the void is today's silent default,
  not a hypothetical.
- **The locker cannot look at its target.** Own-state discipline (D46; the
  `view.neighbor(` gate in `scripts/core-gates.sh`) means the lock predicate
  cannot be "read the target's class at acquisition" — the only audited
  neighbour-read site is the visibility claim verifier
  (`crates/orrery_games/src/regolith/visibility.rs:32`), and PR #461's merge
  record pins the site count at one. Any adjudicated class check must
  therefore be **target-side, event-carried** — the same shape as damage,
  grabs, and lock breaks. This single constraint drives §3.

## 2. Decision 1 — what a rock lock is for

**Decided: a rock lock is the fire-control token for the mining loop — the
same lock, aimed at a different economy.** After #443, no lock means no fire
(a fire input with no mature lock is refused, per #443's acceptance). Rocks
are damageable, scoring, splittable targets (§1). Therefore rocks **must** be
lockable, or #443 silently deletes the mining economy that
`score_rock_points`, `RockCredit`, and the pickup-drop loop already implement
— locking rocks is not a feature request, it is the survival condition of an
existing mechanic across #443's separation.

Rejected alternatives, and why:

- **"Targeting affordance" (a lock that marks but cannot be consumed).**
  Rejected because its premise — an asteroid that cannot be damaged — is
  false in the tree (§1). Building a second, weaker lock class for a target
  that fully participates in combat adjudication would add a state machine
  with no behaviour behind it.
- **"A HUD hover is enough."** Rejected on #442's own acceptance ("clicking a
  body locks it") and on mechanism: the hit-chance band (item 10 of #439,
  `clients/regolith/src/hud.rs:256`) and the tracking maths are properties of
  a *held lock*, and the half-second acquisition cost (`
  LOCK_ACQUISITION_TICKS`, mod.rs:70) plus the pay-again switch rule is the
  price signal that makes choosing between a ship and a rock a decision. A
  hover has no cost, so it can carry no decision.

What the player gains by locking a rock: the right to fire at it (post-#443),
the hit-percentage forecast against it, and the points/pickup payoff when it
dies. The lock's *meaning* is uniform across classes; the *feedback* is not
(§5). The switch rule is deliberately uniform too: ship→rock and rock→ship
both restart acquisition from scratch — cover-swapping between a bomber and
its escort must cost the same half second the tree already charges for any
switch (mod.rs:328-335).

## 3. Decision 2 — the lock predicate, and where it can legally live

**Decided: lockable(target) ⇔ the target's own state is `Craft` or `Rock` and
its hull is above zero — adjudicated by the *target*, carried back as an
event, and mirrored (not decided) by a client-side click filter.**

The signature-radius half of the predicate needs no invention: the radius is
the class's published limit — `Archetype::limits().radius_mm` (3 000 / 6 000
mm, `archetype.rs:96,103` in the snapshot's numbering) for craft,
`RockTier::limits().radius_mm` (40 000 / 20 000 / 8 000 mm) for rocks — and
`projectile_resolution` already consumes exactly these (§1). The predicate
does not read the radius at all; the radius only matters at shot resolution,
where the target supplies its own (mod.rs:380, mod.rs:593). **Rejected:** a
per-entity stored signature field (nothing would ever vary it; it would be a
second copy of the tier/chassis limit, and the tree's own comment forbids
deriving limits "from a damage event" rather than the hashed tier,
state.rs:65-66).

The class half is the real design problem, because of the constraint in §1:
the locker may not read the target's class, so the check must round-trip.
Three shapes were weighed:

- **(a) Client-side filter only.** The click handler simply refuses to select
  pickups/directors. Rejected as the *sole* mechanism: #442's acceptance
  demands a ruleset-level check whose vacuous mutation kills a named test,
  and a hostile or buggy input source (the deterministic pilot already does
  this, §1) can name any id it likes. UX keeps the filter; adjudication
  cannot live there.
- **(b) Refusal at fire time.** Unlockable entities answer a delivered shot
  with a refusal event (extend `step_pickup`/`step_director` to answer
  `Order::Damage` with `LockBroken`). Cheapest change, but under #443 a lock
  is *held* without firing — a player could sit "LOCKED" on a pickup
  indefinitely and learn the truth only when space is pressed. Late feedback
  is precisely the illegibility #442 warns about. Kept only as the fallback
  (§8).
- **(c) Acquisition handshake — recommended.** Lock maturity requires the
  target's own confirmation:

  1. When acquisition *starts* (fresh lock or switch — the `None` and
     `Some(_) != target` arms, mod.rs:315-319/331-335), the locker emits one
     `Outcome::LockRequested { locker, target }`. Once per acquisition, not
     per tick: amplification stays bounded at one event per switch, the same
     shape as `GrabAttempted`.
  2. `deliver` routes it to the target as an order. The target adjudicates in
     its own step from its own state: `Craft` with `hull > 0` or `Rock` with
     `hull > 0` answers `Outcome::LockConfirmed { locker, class }` where
     `class ∈ {Ship, Rock}`; `Pickup` and `BloomDirector` answer
     `Outcome::LockRefused { locker }`. A dead craft (respawn countdown)
     refuses too.
  3. The locker records the reply in hashed state: a new
     `lock_class: Option<LockClass>` beside `lock_target`
     (state.rs:52-58). `LockRefused` clears the lock outright and surfaces a
     banner (§5). A nonexistent id never replies, so `lock_class` stays
     `None`.
  4. **Maturity gate:** `lock_progress` accrues exactly as today, but the
     `Locked` phase — and, post-#443, the fire action — requires
     `lock_progress == LOCK_ACQUISITION_TICKS && lock_class.is_some()`.

  Plain arithmetic: the round trip costs two ticks of delivered-event latency
  (one per hop, D46), and acquisition takes `LOCK_ACQUISITION_TICKS = 30`
  ticks (mod.rs:70). 2 < 30, so against any live, lockable target the
  handshake adds **zero** perceptible latency — confirmation is banked ~28
  ticks before the lock could mature anyway. Against the void, the lock now
  visibly never matures instead of silently wasting ammunition, which
  retroactively fixes the pilot-shaped fires-into-nothing behaviour described
  in §1. That is a bot-outcome change, hence goldens regenerate — but #442's
  acceptance already requires the version bump and regeneration, so the cost
  is already spent.

  Pseudocode for the target-side arm (the guarded stage a mutation must
  break):

  ```
  Order::LockRequested { locker } => events.push(match own_state {
      Craft(c) if c.hull > 0 => LockConfirmed { locker, class: Ship },
      Rock(r)  if r.hull > 0 => LockConfirmed { locker, class: Rock },
      _                      => LockRefused  { locker },
  })
  ```

  Ordering with #443: D46's delivered-first composition and the
  order-sensitivity pin (`input_order_changes_the_outcome`) mean a
  `LockConfirmed` arriving in the same tick as a lock *switch* must be
  defined: the confirmation names a target; it is applied only if
  `lock_target` still equals that target after the tick's input fold —
  otherwise dropped. One line, one test.

**Interaction with #443 (in flight, must not be contradicted):** #443 splits
"lock intent" from "fire intent". This design binds the handshake to the
*lock* half — whatever order shape #443 gives the sustained lock intent, the
`LockRequested` emission belongs in its fresh-acquisition/switch arm, and the
maturity gate in step 4 is exactly the predicate #443's "fire with no lock is
refused" check should read. The two designs compose instead of colliding: #443
defines *when* the lock is consulted; this defines *what counts as a lock*.

**Version discipline:** this is a rules change inside the P4 digest; it takes
the `REGOLITH_RULESET.version` *after* #443's — the #457/#461 collision
(both independently claimed version 9; the merge deliberately became 10)
is the precedent: whichever of #443/this lands second takes the next number
against the merged base, never a number chosen in isolation.

## 4. Decision 3 — tracking maths for a target with no velocity of its own

**Decided: one formula, unchanged, for both classes and for both sides of the
#441 boundary. No rock-specific tracking rule.** The maths already in the
tree needs nothing from this design, and the proof is worth writing down
because the issue assumes otherwise.

`hit_chance_ppm` (mod.rs:963-1013) is built entirely on **relative** motion:

```
r  = target_pos − attacker_pos                      (mod.rs:971-973)
v  = target_vel − attacker_vel                      (mod.rs:974-976)
ω  = |r × v| / |r|²   µrad/s                        (mod.rs:980-989)
tracking_ratio = ω · REF_SIG / (tracking · target_radius)   (mod.rs:990-995)
chance = S³ / (S² + tracking_ratio² + range_ratio²) , S = 10⁶ ppm
```

A rock with `vel = 0` is not a degenerate input — it just makes
`v = −attacker_vel`, so the angular rate against a static rock is *entirely
the shooter's own transversal*, exactly as the brief suspected. That is a
statement about the data, not a hole in the rule: the formula is the standard
angular-velocity identity ω = v⊥/r and is continuous in `target_vel`. The day
#441 gives bloom rocks real velocities, the same expression simply starts
receiving nonzero `v_target`; nothing switches, so there is no silent
behaviour change *of the rule* at the boundary. The numbers change — hit
chances against drifting rocks shift, and that golden regeneration belongs to
**#441's own version bump**, not to this one. The one thing this design must
*not* do is special-case ω for rocks (e.g. zeroing the shooter's
contribution "because the rock isn't dodging"): that would create exactly the
discontinuity the issue fears, twice — once now and once when #441 lands.

Worked example, real numbers from the tree (Stock weapon: `tracking = 180 000
µrad/s`, `optimal = 300 000 mm`, weapon.rs; `REF_SIG = 3 000 mm`, mod.rs:87).
An interceptor strafing at its speed cap, 120 000 mm/s (archetype limits),
perpendicular at 100 m (100 000 mm), inside optimal so `range_ratio = 0`:

```
ω = (100 000 · 120 000) / 100 000² · 10⁶ = 1 200 000 µrad/s
vs a Large rock (radius 40 000 mm):
  ratio = 1 200 000·3 000·10⁶ / (180 000·40 000) = 0.50 S
  chance = 1 / (1 + 0.25) ≈ 80 %
vs an interceptor (radius 3 000 mm), same geometry:
  ratio = 1 200 000·3 000·10⁶ / (180 000·3 000) = 6.67 S
  chance = 1 / (1 + 44.4) ≈ 2.2 %
```

The tier radius makes a Large rock 13.3× "larger" in tracking terms than an
interceptor at identical geometry — a barn versus a bird, from the same gun.
That is the EVE-shaped signature mechanic working as intended (mining while
manoeuvring stays viable; dogfighting demands matched transversal), and it
falls out of numbers that are already hashed state. Whether 80 % versus 2.2 %
is the *right* spread is a balance knob (`radius_mm` per tier), and that knob
is the owner's (§8).

## 5. Decision 4 — feedback: one refusal family, two lock skins

**Decided: refusals and breaks stay one visual family — a named, all-caps
banner through the existing `LockBreak`/`ShotFeedback` path — and the lock
reticle/panel forks on `LockClass`, not on a new mechanism.**

The tree is already growing a refusal vocabulary; this design completes it as
a legible set:

| Situation | Source | Player-visible line |
|---|---|---|
| Fire pressed, no mature lock | #443 (in flight) | #443's refusal (its call on copy) |
| Shot emitted outside the hull's arc | #461, `ShotResult::OutOfArc` | `SHOT REFUSED · OUT OF ARC` |
| Lock requested on pickup/director/dead body | **this design**, `LockRefused` | `LOCK REFUSED · NOT A TARGET` |
| Lock on an id that answers nothing | **this design**, maturity gate | ring never closes; caption holds `ACQUIRING — NO RETURN` |
| Held lock occluded / range / target died | #457, `LockBroken{reason}` | existing break banner |

Distinctions that matter: *refused* means the ruleset answered no and the
lock is gone (instant, banner); *no return* means nothing answered (the ring
visibly stalls at the maturity gate rather than lying `LOCKED`); *broken*
means a mature lock ended. Three words, three states, no overlap with each
other or with #443's no-lock refusal.

The class fork rides the machinery the client already has. `LockView` copies
lock fields out of the craft (`clients/regolith/src/combat.rs:56-75`) — it
additionally copies the new `lock_class`, so the skin never guesses class
from a world lookup that could race a switch. The HUD target panel already
draws chassis, hull, shield, and the hit band (`hud.rs:244-258`). Decided
presentation, chosen so the two locks cannot be confused at a glance:

- **Ship lock (`LockClass::Ship`):** the current combat treatment unchanged —
  accent-red reticle brackets, caption `LOCKED`, panel shows chassis name,
  hull and shield bars, hit band.
- **Rock lock (`LockClass::Rock`):** amber reticle, caption `MINING`, panel
  shows tier name, hull only (rocks have no shield, state.rs Rock fields),
  and the tier's point value from `RockTier::limits().points` — the number
  the player is actually buying. The hit band renders identically for both
  classes; it costs nothing because the band's inputs (radius, range,
  relative velocity) are class-uniform (§4).

Rejected: a separate "affordance" reticle with no combat readouts (falls with
§2), and any scheme that derives the skin's class from client-side world
state instead of the confirmed `lock_class` (the skin must not decide
anything the ruleset didn't — the client's own module header says so,
combat.rs:4).

## 6. Decision 5 — firing at a locked rock

**Decided: a fire action with a mature rock lock fires, and the rock takes
the damage. Full stop — no refusal, no harmless hit.** This is the sharpest
case in the issue and the easiest in the tree: it is current behaviour.
`step_rock` runs the *same* `projectile_resolution` as a craft — arc gate
included (mod.rs:590-601 feeding mod.rs:856-862) — so a rock shot already
respects `SHOT REFUSED · OUT OF ARC` exactly as a ship shot does, then rolls
the same tracking/range chance, then applies hull damage, splits, drops, and
pays points (§1). Consistency with the arcs refusal is not a goal to design
toward; it is already structural, because there is one resolution pipeline.

Rejected alternatives:

- **Refuse the fire ("rocks aren't combat targets").** Deletes the live
  mining economy across #443 (§2) and forks the fire path by class — the
  exact illegibility this milestone is trying to remove.
- **Hit harmlessly (visual impact, no damage).** Worst of the three: spends
  cooldown and ammunition-shaped time for nothing, teaches the player the
  gun is broken, and requires a class branch in `projectile_resolution` that
  §4 just argued against. A lock that can be consumed for zero effect is the
  "affordance" design smuggled back in.

Under #443's separation the sequence is: click rock → acquisition (30 ticks,
handshake §3) → `MINING` lock held, no damage while held → space → one
adjudicated volley → repeat per cooldown. Same verbs as a ship, different
skin — which is precisely #442's "visibly distinguishable" acceptance
without a second mechanism.

## 7. Decision 6 — does a rock lock decay behind cover?

**Position: yes in principle — same decay, same clock, occluder must be a
different rock — but shipping it in this milestone is severable, and the
severance is an owner scope call (§8).**

What exists: LoS decay (#444/#457) is claim-and-verify. The *target* of a
lock receives an untrusted `ClaimCover { locker, rock }` and verifies it in
its own step with an integer segment-sphere test against exactly two recorded
neighbour reads (`visibility.rs:13-51`); a verified `occluded` transition
drains the locker's `lock_progress` at `LOCK_DECAY_PER_TICK` until the lock
breaks (mod.rs:280-287, 482-492), pinned by
`occluded_lock_decays_over_time_and_visibility_restores_it`
(tests/regolith.rs:1276).

What forecloses rock locks from decaying today is a single guard:
`verify_claim` bails unless the claim's target is a `Craft`
(`visibility.rs:17-19`). So the asymmetry — ship locks decay behind rocks,
rock locks never decay behind anything — is an artefact of the verifier's
target class, not a decision anyone recorded. #442 asks the question; the
answer on the merits:

- **Symmetry argument (adopt):** the lock is one mechanism (§2-§6); a
  duck-behind-cover escape that works on a bomber but not on the rock next to
  it is unteachable. The self-occlusion worry — "the rock is its own cover" —
  is already handled by construction: the verifier refuses `rock == target`
  (`visibility.rs:29`), so only a *different* rock between locker and locked
  rock can occlude, and the segment runs to the target's centre with the
  occluder's radius shrunk by the two-centimetre margin
  (`OCCLUSION_MARGIN_MM`, visibility.rs:42-46), so a rock never shadows
  itself by its own bulk.
- **Cost argument (why it is severable):** widening the verifier to
  `Craft | Rock` targets keeps the neighbour-read *site* count at one — the
  same function, same two reads — but it changes the audited predicate that
  PR #461's merge record singles out ("exactly one neighbour-read site …
  admitting another is an owner amendment"). Whether widening the accepted
  target class of the *existing* audited site needs the same owner sign-off
  as adding a site is not this document's call to make; it is flagged, not
  assumed. Mechanically the rock target also needs a claim rate limit
  (craft carry `cover_claim_cooldown`, state.rs:60; rocks have no such field
  — one `u16` and a codec/length change to the 79-byte rock encoding,
  state.rs:388) — small, but a state-shape change to every rock in every
  golden. And the payoff is modest **today**: rocks are static (§1), so
  rock-lock occlusion happens only through the shooter's own motion until
  #441 moves the field.

**Recommendation:** adopt the widened verifier *with or after #441*, when
moving rocks make the case common; if #442 ships first without it, the
asymmetry must be stated as deliberate in the change that ships — one line in
the PR and a `rock_locks_do_not_yet_decay_behind_cover` characterisation
test, so the gap is pinned as a decision instead of rediscovered as a bug.

## 8. Owner decisions, named as such

Decided above on technical grounds; the following are handed up, not taken:

1. **Handshake (§3c) versus fire-time refusal (§3b).** The handshake is
   recommended and costs no perceived latency, but it is the larger diff two
   days before the milestone date (new order/outcome pair, one hashed field,
   codec bump). §3b plus the client filter meets the letter of #442's
   acceptance with a weaker feedback story. Scope call.
2. **Rock-lock occlusion decay now, with #441, or never** (§7) — including
   whether widening the audited visibility predicate's target class counts as
   the D43-clause-(d)-adjacent owner amendment PR #461's record describes.
3. **Balance of the signature spread** (§4): tier radii as they stand make a
   Large rock ~13× easier to track than an interceptor; per-tier `radius_mm`
   is the knob if the mining/dogfight difficulty gap should narrow.
4. **Refusal copy and palette** (§5): `NOT A TARGET` / `MINING` / amber are
   proposals; the refusal *family structure* (refused ≠ no-return ≠ broken)
   is the design.

Nothing here amends an Accepted ADR. If item 2 is judged to touch D43's
audited-predicate clause, that lands as an explicit owner amendment, proposed
separately — not smuggled in with the ruleset bump.

## 9. Named tests and the mutation story

Per #442's acceptance ("a mutation making the target-class check vacuous
kills a **named** test"), the guarded stage is the target-side match in §3's
pseudocode — not the client filter, not the maturity comparison. Named
tests an implementer writes, each through the real executor (the
`fire_through_executor` pattern, tests/regolith.rs:948):

- `a_rock_confirms_a_lock_and_the_reticle_class_is_rock` — happy path;
  `lock_class == Some(Rock)` at maturity.
- `a_pickup_refuses_a_lock_and_the_locker_clears_it` — the mutation target.
  Mutating the match's pickup arm to confirm (the vacuous-check mutation)
  must kill this test by observing a mature lock where a cleared one is
  asserted.
- `a_lock_on_a_missing_id_never_matures` — the maturity gate; kills the
  mutation that drops `lock_class.is_some()` from the gate.
- `switching_between_classes_restarts_acquisition_and_reconfirms` — extends
  tests/regolith.rs:1129 across the class boundary; also pins the
  same-tick confirm-versus-switch ordering rule from §3.
- If §7 ships: `a_rock_lock_decays_behind_another_rock_and_not_behind_itself`
  — kills a mutation that re-narrows `verify_claim`'s target class.

House rule restated because it bites here: `0 passed; N filtered out` is not
a pass, and a mutation that fails to compile emits no result line — every
mutation above must be confirmed *run*, and a survivor is a reportable
finding.

## 10. Stale and unevidenced, logged

- **#442's text vs the tree:** "an asteroid that cannot be damaged" — false
  (mod.rs:616-626 and the whole of §1); "no signature radius" — false
  (state.rs:79-114, consumed at mod.rs:593); "no velocity of its own until
  #441 lands" — misleading: the integrator and reflection exist and are
  pinned (mod.rs:714-726, tests/regolith.rs:27); only the spawn *data* is
  zero (mod.rs:1158). "No authority holder in the same sense" — no mechanism
  found that this would affect; treated as unevidenced, not designed around.
- **The tasking brief for this document** repeated the same three claims and
  additionally placed rock velocity integration "at `mod.rs` (bloom rocks
  spawn with `QVel::default()`)" — the spawn-velocity half is right, the
  implied absent-integration half is not.
- **Working-checkout skew:** the checkout this file lands in is behind
  `origin/main` (its `archetype.rs` predates #461 and has no `FiringArc`; its
  ruleset version predates 10). All citations here are against `origin/main`
  at `191a8aa6`; an implementer must rebase before trusting any line number,
  including these.
- **#443 unlanded:** every statement about fire-as-action is written against
  #443's issue text and acceptance, not against merged code. If #443 lands in
  a different shape (e.g. lock intent stays inside `Order::Fire`), §3's
  binding point ("the fresh-acquisition/switch arm") moves with it; the
  handshake and maturity gate are unaffected.
- **Unverified balance claim:** the 80 %/2.2 % worked example (§4) is hand
  arithmetic from tree constants, not a golden — an implementer should
  confirm it against `hit_chance_ppm` in a unit test before quoting it.
