# A2 — Kernel versus game-module ownership (#398)

**Status:** evidence for the #395 planning tree · **Date:** 2026-08-25 ·
**Tree:** `docs/398-a2` at `46c9301a` · **Parents:** [#398](https://github.com/baadc0de/orrery/issues/398) ←
[#395](https://github.com/baadc0de/orrery/issues/395) · **Builds on:**
[A1](a1-ruleset-architecture-map.md) · **Source brief:**
[docs/plans/ruleset-ecs-migration-brief.md](ruleset-ecs-migration-brief.md) §Proposed
conceptual model

This document decides **who owns each responsibility**: the Orrery kernel —
infrastructure that must serve any game — or a game module — Regolith-specific
rules. It is the constraint that stops Orrery infrastructure from quietly
becoming gameplay-specific.

It is **not an architecture recommendation**. Every row below is stated so it
holds whether A3 (#399) keeps `Ruleset`, adopts a canonical `bevy_ecs::World`,
builds an engine-neutral core, or lands a hybrid: "owner" names *who decides
semantics and who may change what*, never *which storage or trait expresses it*.
Where a row's full answer depends on a decision another tree item owns
(rollback unit → A7; per-component policy shape → A5; module manifests → A8),
the row assigns what is decidable now and names the deferred decision instead of
inventing one.

Method, as in A1:

- Every claim cites a file and line opened on this tree today. Where this
  document asserts an enforcement property for a gate or a structural rule,
  the **guarded stage was broken**, the named check that died recorded, the
  change reverted and the pass re-confirmed (§9).
- What **exists today**, what is **designed but unwired**, and what is
  **speculative** never share a sentence.
- A1's load-bearing findings were re-verified against this tree before being
  relied on; deltas found are in §10.

---

## 1. Definitions

**Kernel** — code whose specification can be written without naming any game's
semantics. Operational test: could this code be written, tested, and shipped
against two hypothetical games that share nothing but the wire? The kernel's
correctness story (determinism, identity, authority, atomicity, evidence) must
not reference Regolith, Skirmish, or any future game.

**Game module** — code whose meaning is given by a game's design: damage,
inventory, docking, bloom cadence, respawn timers. Another universe replaces it
 wholesale without the kernel noticing.

A third category exists because the tree already has it, and pretending it is
one of the other two is how boundaries rot:

**Game-authored policy consumed by kernel machinery** — declarations *about*
game content that kernel code interprets: `Ruleset::invariants()` predicates,
`classify_component` classifications, `CoreCodec` encodings,
`IntentValidator` implementations. For these, ownership is always two-sided:
the **game authors** the declaration, the **kernel consumes and enforces** it.
Neither side owns it alone, and the seam between them is where most of §2's
rows land.

### 1.1 What exists today (the three strata)

The current tree already separates along these lines, by crate:

| Stratum | Crates | Evidence |
|---|---|---|
| Wire vocabulary | `orrery_protocol` | Engine-agnostic by gate-enforced rule; every serialized type crosses here ([docs/10-crates.md](../10-crates.md):92, D15 layering 1) |
| Kernel machinery | `orrery_core`, `orrery_net`, `orrery_spatial`, `orrery_authority`, `orrery_predict`, `orrery_persist_client`, `orrery_witness` engine half, `orrery_persistd` coordination | None of them names `Ruleset` except core (definition), witness (engine + adapter), persistd (two registration-edge functions), facade (pass-through) — A1 §3; re-verified on this tree (`rg 'R: Ruleset'` hits exactly those files). Net/spatial/authority/predict/persist-client never name the trait at all (A1 §3.7, re-run) |
| Games | `orrery_games` (Regolith v8, Skirmish v2), `clients/regolith`, harness-side assembly in `gates/p1-swarm` | A1 §2, §3.6 |

Two observations sharpen the picture before the table:

1. **"Kernel" is not a crate; it is a role.** `bevy_ecs`, lightyear and
   replicon appear *inside* kernel crates (spatial, predict, persist-client)
   as implementation substrates, while `orrery_core` is Bevy-free. Both are
   kernel under the definition above: neither names game semantics. The
   question A3 will answer — which substrate hosts canonical state — is
   therefore orthogonal to every row below.
2. **The games/harness boundary is already blurry in exactly one place.**
   `gates/p1-swarm` drives `Executor<Regolith>` directly and assembles signed
   frames harness-side (`bot.rs:680` step loop, `bot.rs:1094-1138`
   claim/frame publication). That is harness code standing in for the field
   host that does not exist yet (A1 §5.5), not a precedent about ownership.

---

## 2. The ownership table

One row per responsibility. **Owner** uses three values: *Kernel* (any game's
kernel may change it; games must not), *Game module* (each game owns it;
kernel must not name it), *Split* (one seam, named, with each side listed).

| # | Responsibility | Owner | Where it lives today (evidence) | Reason |
|---|---|---|---|---|
| 1 | **Time and tick progression** — what tick `T` is, the fixed rate, epoch anchoring | **Split.** Kernel owns tick *semantics*: `Tick(u64)`, universe-global, anchored to a coordinator-issued epoch (`orrery_protocol/src/persist.rs:24-28`), fixed 60 Hz as a constant "never a measurement" (`executor.rs:25-28`). Hosts own *driving*: today three hosts advance ticks themselves — p1-swarm bots (`bot.rs:680`), the regolith client's local session (`clients/regolith/src/lib.rs:71,91`), and lightyear's session clock bridged per-tick (`orrery_predict/src/tick.rs`). Games may *read* the tick (Regolith derives bloom cadence from it, `regolith/mod.rs:34-36`) | Adjudication re-runs tick `T` on a different machine months later; if a game could define its own rate or advance shared time, "the same tick" stops meaning one thing and every witness verdict becomes unproducible. VC-1 exists precisely here | A game module defining its own tick rate, introducing a `dt`, or advancing canonical time. The ambient-input ban (VC-8) already fails rules that read wall-clock (`core-gates.sh:103-105`; A1 mutation M4) |
| 2 | **Identity** — durable vs ephemeral ids, allocation rules | **Split.** Kernel owns identity *classes* and their discipline: `PersistId(u64)` (`persist.rs:46`), `RulesetId` (`verifiable.rs:59`), `UniverseSeed` (`verifiable.rs:71`), cluster-side minting of persistent rows (`intent/mod.rs:184-186`: "Every op costs a minted `PersistId`"), the ephemeral registry mapping bevy `Entity ↔ EphemeralId` (`orrery_authority/src/ephemeral.rs:160`). Games own *derivation inside their step*: materialized identifiers must come from replayable inputs — "`executor` deliberately has no allocator … identifiers are derived by the emitting step from its own replayable inputs; they are never allocated from executor population or creation order" (`ruleset.rs:210-215`, `:272-278`) | Identity must survive replay on a machine that holds only the disputed entity. Allocation-order identity would make an isolated replay disagree about which entity was created — the exact property the adjudicator's single-entity model depends on | An id minted from population/creation order, or an engine entity handle crossing the wire/persistence boundary (D15 layering 1 keeps wire types engine-free) |
| 3 | **Authority** — claims, leases, handoff, divestiture | **Kernel**, entirely. Claims/leases/phases/handoff live in `orrery_authority` (`lib.rs:44-464`); D7/D26/D30 govern semantics. The structural proof of ownership is negative: `step`'s signature carries no lease or authority input (`ruleset.rs:257-262` — state, inputs, rng only), so outcomes cannot depend on who executed them | A rule that branched on lease internals would adjudicate differently than it executed whenever replay ran on a holder without the same lease table — the same failure mode the neighbour-read ban closes (`core-gates.sh:126-139`), one level up | A game module implementing or mutating lease logic; conversely the kernel interpreting what a game's *contact graph means* — hosts supply `ContactObservations`, the kernel plans from them (facade doc `crates/orrery/src/lib.rs:434-438`) |
| 4 | **Transactions** — atomic admission, durable effects, anti-dupe | **Split at a named seam.** Kernel owns the envelope: gateway pre-checks (`IntentValidator`, `intent/mod.rs:156-161`), volume bounds (`MAX_OPS_PER_INTENT = 64`, arg caps `:189-195`, attestation cap `:202`), the FDB transaction as sole authority (`:152-155`), idempotency/minting, and op-id reservation discipline (`:243-254`). Game owns op *semantics*: "Every other op id is `Ruleset`-opaque" (`:208-209`). Exactly two ops are cluster-interpreted — ledger credit (`LEDGER_CREDIT_OP = 0`, `:210`) and item transfer (`LEDGER_ITEM_TRANSFER_OP = 2`, `:257`), the second added because the anti-dupe invariant needed a real read-check-write producer (`:218-227`) | Atomicity is infrastructure: any game's trades must commit-or-not identically. But *what* a trade means cannot live in the cluster — the cluster serves every game and links none necessarily ("registering a build means linking a `Ruleset`", `bin/persistd.rs:1261-1263`) | A module writing durable state outside the transactional envelope (the brief states this too). Flagged tension: the two interpreted ops are game-flavoured semantics living kernel-side — §5 CC-1 works through why they are reservations, not precedent |
| 5 | **Spatial state** — grid, cells, AOI, hysteresis, interest | **Split.** Kernel owns geometry and attention: `Cell(CellId)` components, AOI subscription, interest selection, hysteresis (`orrery_spatial/src/plugin.rs:40`, `hysteresis.rs:32`, `interest.rs:45`); `CellId` encoding is protocol-level (D5). Games own movement integration and what position *means*, encoded on the kernel lattice — quantized positions/velocities (`QPos`/`QVel`, `quantize.rs:33,44`; VC-7 snap each tick) | Every game lives in cells and needs interest management; no two games agree on how a craft steers. The lattice is the meeting point: kernel-defined encoding, game-filled content — the same shape as row 8's codec split | The kernel choosing a bounce/wrap/despawn response for entities reaching an island edge (game behaviour — see §5 CC-2), or a game computing its own cell membership (interest management would then diverge from authority) |
| 6 | **Persistence coordination** — journaling, checkpoints, uplink, write classes | **Split.** Kernel owns coordination: cell actors store opaque component bytes — "Components are stored as postcard bytes so the actor never needs the game's component types" (`orrery_persistd/src/actor.rs:117-121`); journal/checkpoint machinery, retention, recovery (D19/D20/D23); client-side uplink/session/area loader (`orrery_persist_client`). Games own codecs for their state (`CoreCodec` impls) and — designed but unwired — the classification that routes write classes: docs/06 says the classification "drives persistence write classes (§D11) and witness attention (§D10)" (`docs/06-verifiable-core.md:60`) | The cluster never interprets game state (A1 §5.4), so it can persist any game; the game must make its state canonically encodable or nothing downstream can hash, replicate, or restore it | Persistence code naming a component type, archetype order, or reflection metadata; a module bypassing the bag to write its own rows |
| 7 | **Rollback** — prediction resimulation, correction after adjudication | **Kernel** owns mechanism, window and budget; the rollback *unit* is explicitly reserved to A7 (#403) and not assigned here. Today two mechanisms exist: lightyear-side resimulation under a budget guard (`orrery_predict/src/budget.rs:97,191`), and canonical correction by re-execution — confirmed verdicts queue authoritative state back through `AuthorityCorrectionInbox` (`orrery_predict/src/correction.rs:48`; facade queues at `crates/orrery/src/lib.rs:338`). Note this honestly: *rollback of canonical rules state is currently a replay/correction story, not a world rewind* (A1 §9 assumption 7 rider) | Re-execution-based correction works only because steps are pure — the game's purity obligation (row 9) is what makes the kernel's rollback substrate valid. Mechanism and obligation are separable from storage, which is why this row survives every A3 variant | A game module keeping hidden state outside `CoreState` — an effect invisible to the hash is invisible to correction too (`ruleset.rs:280-284` states this trap verbatim) |
| 8 | **Witnessing** — stages, evidence, escalation, strikes | **Split.** Kernel owns the pipeline: ingest → stage 1a invariant checks → stage 1c per-entity re-execution → audit window → self-verifying report, shadow-mode by default (`witness/src/lib.rs:13+`, A1 §5.3), plus strike accounting and adjudication registration (`AdjudicationExecutor::register<R>`, `adjudication.rs:350`). Game supplies the two things the pipeline cannot invent: stage-1 predicates (`invariants()`, consumed at `witness.rs:668` via `evaluate`, `invariants.rs:118`) and the canonical codec whose blake3 encoding *is* the claim commitment (`state_hash`, `ruleset.rs:324-326`) | A witness that had to understand damage would need a new release per game; a witness fed arbitrary predicates needs only the declarations. This is the policy-consumer category working as designed — unlike `classify_component` (§7), `invariants()` has real call sites | Witness code branching on game events semantically; a game declaring invariants the kernel must *interpret* rather than merely evaluate |
| 9 | **Gameplay components and systems** — combat, movement meaning, spawns, AI | **Game module**, entirely. `RegolithState` variants, `Order` inputs, `Outcome` events and all stepping live in `orrery_games` (`regolith/mod.rs:122-166`, `state.rs`, `weapon.rs`, `pilot.rs`); Skirmish likewise; presentation skins consume frames (`clients/regolith`, p1-swarm mirroring `bot.rs:669-734`). Verified negatively: no kernel crate source names any game type — `rg "Regolith|Skirmish|BloomDirector|HullIntegrity"` over the nine kernel crates' `src/` returns zero files (run on this tree) | This is the definition of the category. Everything here is replaceable per-universe; anything the kernel needs from it must cross as one of the declared seams (codec bytes, predicate slices, event vocabulary carried opaquely) | Kernel code special-casing a state variant, an event kind, or a tuning constant. Regolith's `ISLAND_CRAFT_BUDGET = 8` (`regolith/mod.rs:45-55`) is population policy — game-owned — even though the *window budget mechanism* it feeds is spatial-kernel |
| 10 | **Invariants — cross-game vs game-specific** | **Split by scope.** Cross-game invariants — VC-1..VC-8, chain integrity, first-writer-wins materialization, input ordering, transaction bounds — are **kernel**: authored in core/persistd, mechanically enforced where possible (`core-gates.sh` clauses; executor guarantees like quantize-before-hash, `executor.rs:126-127`). Game-specific invariants — speed ceilings, cooldown honouring, lock timing, acceleration bounds — are **game**, authored as `Invariant<CoreState>` predicates (`regolith/invariants.rs`, consumed exactly like row 8's stage 1a) | The container/enforcement-point split: the kernel owns *where* checks run and *what happens* on violation; the game owns *what* is checked. Neither can fake the other's half — a kernel-authored speed limit would be wrong for the next game; a game-authored chain-integrity check would be redundant and unauditable |

### 2.1 Supplementary rows (emerged while assigning the ten)

These were not listed in #398 but the corner cases of §5 cannot land without
them, so they are assigned rather than left implicit:

| Responsibility | Owner | Evidence and reason |
|---|---|---|
| Deterministic RNG partitioning | **Kernel** | `tick_rng(seed, entity, tick)` (`rng.rs:31`), supplied by the executor (`executor.rs:120`). Games draw; they never seed from anywhere else (VC-3). A game-supplied RNG source would break replay portability |
| Input admission and ordering | **Kernel** fixes the total order before execution; **game** interprets meanings | `OrderedInputs` iterates log order and is never sorted (`ruleset.rs:149-157`, VC-2). Volume caps are kernel (`intent/mod.rs:189-202`) |
| Cross-entity effect transport | **Kernel** carries emission-ordered events as next-tick inputs; **game** defines vocabulary and consumption | `StepOutput.events` is a `Vec`, "never a set" (`ruleset.rs:195-201`); live neighbour reads are banned until a `NeighborFrame` producer exists (`core-gates.sh:126-139`) |
| Replication/relevance policy | **Kernel** transports (replicon/AOI); **game** declares relevance per component — declaration channel unwired (see §7) | docs/06 names the intended consumers (`docs/06-verifiable-core.md:210`); none exist yet |
| Version identity & compatibility surface | **Kernel** pins `RulesetId` into frames, claims, bundles, strike rows and persisted records; **game** supplies the value | `verifiable.rs:59` + pinning sites (A1 §5.4 list); D21 freezes persistd's exports; D38(c): additive trait change is free, a *required* method names D21 |

---

## 3. Dependency direction

The arrows, as they exist today and as the table requires:

```text
                    ┌────────────────────────────────────────────┐
                    │            orrery_protocol                 │
                    │   (wire vocabulary; engine-free by rule)   │
                    └───────────────▲────────────────────────────┘
                                    │ every crate depends on it
        ┌───────────────────────────┼───────────────────────────┐
        │ kernel machinery          │                           │
        │  core ◀ witness(engine)   │   games: Regolith,        │
        │  core ◀ persistd(coord.)  │   Skirmish, future        │
        │  core ◀ conformance       │   modules                 │
        │  net spatial authority    │                           │
        │  predict persist_client   │                           │
        └───────▲───────────────────┴──────────▲────────────────┘
                │                              │
        presentation skins              game modules depend on
   (clients/regolith, harnesses)        any kernel crate; kernel
        depend on both                  never depends on a game
```

Normative layering already in force (not invented here): protocol ← everything
and `orrery_core` ← {witness, persistd, field-host-to-be, game} are D15's first
two layering rules ([docs/10-crates.md](../10-crates.md):92-94); lightyear only
inside `orrery_predict`, replicon only inside `orrery_spatial`/`persist_client`
(rules 3-4, `:95`). `orrery_persistd` takes the witness engine with
`default-features = false` (`orrery_persistd/Cargo.toml:36`) — the backend
links no Bevy at all.

### 3.1 The three legal crossings

Everything a game module and the kernel exchange must cross one of these:

1. **Ordinary dependency, game → kernel.** A module imports machinery
   (`orrery_games` depends on `orrery_core` + `orrery_protocol`,
   `orrery_games/Cargo.toml:13-14`).
2. **Policy handover at a registration seam.** The game hands the kernel inert
   declarations — an `invariants()` slice, a `classify_component` fn, `CoreCodec`
   impls, an `IntentValidator` impl, a boxed build factory
   (`adjudication.rs:350-360`). The kernel *invokes* these; it never inspects
   their semantics. This crossing is what makes rows 4, 6, 8 two-sided without
   either side owning the other.
3. **Opaque bytes through wire and store.** Canonical encodings (`CoreCodec`),
   intent op args, component bags. The cluster hashes them, routes them, and
   persists them without understanding them.

### 3.2 What must never point back, and the rule each reversal violates

| Forbidden arrow | Rule it violates | Consequence if drawn | Enforcement today |
|---|---|---|---|
| Kernel crate → game-module crate (dependency or type naming) | D15's crate-set neutrality: infrastructure serves any game | Every other game transitively links this game's rules; "the same build links into peers, field hosts and persistd" (D9, `ruleset.rs:3-6`) becomes "the same build links into *these* games"; adjudication loses its footing because the cluster would ship one universe's content as if it were law | **Structural for `orrery_core ↔ orrery_games`**: the reversal is a cargo cycle and the resolver refuses it (§9 M-A). **Nothing mechanical for any other pair** — e.g. `orrery_spatial` gaining a dependency on `orrery_games` compiles and no check fires. Honest finding: this direction is convention plus review everywhere except that one structurally locked edge |
| Engine/Bevy → verifiable core | D9/D15: core is headless and engine-agnostic so peers, field hosts and the cluster can link it | The cluster cannot link the rules to adjudicate; three runtimes disagree about the same log | Gate-enforced: `core-gates.sh:71-75` fails on any bevy in `cargo tree`, dev-deps included (§9 M-B) |
| Presentation/engine state → canonical state | The brief's own boundary: presentation events have "no authority over canonical state" | Rollback rewinds audio and UI; witnesses hash renderer output; determinism dies quietly | Partially structural: canonical state lives in `Executor` maps, not ECS components, so a Bevy system cannot reach it today. Under an ECS-hosting A3 variant this arrow needs its own enforcement (A4/A7 territory) |
| Persistence/wire formats ← engine types (archetype order, entity bits, reflection names) | Protocol leakage (brief §Primary risks); D38(d)(3) keeps schema versions orthogonal to `RulesetId` | Every stored byte pins a Bevy version; migration across engine upgrades becomes impossible | Structural: formats are defined in `orrery_protocol`, which no engine reaches |

The single sentence version of the rule:

> **Games may depend on all of the kernel; the kernel may depend on games only
> through the three crossings of §3.1 — and a kernel artifact that names a
> game's types has already absorbed gameplay, wherever it sits in the tree.**

### 3.3 Where enforcement is missing (findings, not proposals)

Recorded because the table's authority claims should not overstate what is
mechanically held today:

1. Only one kernel↔game edge (`core`/`games`) is protected by structure. The
   other kernel crates could grow game dependencies silently.
2. `classify_component`'s consumers do not exist, so row 6's write-class
   routing and row 8's attention policy are currently *unowned in practice*
   even though ownership is assignable in principle (§7).
3. The neighbour-read ban is scoped to `RULES_CRATES = (orrery_games
   orrery_conformance)` (`core-gates.sh:42`). If game logic migrated into a
   kernel crate, the gate would stop watching it — the gate watches *crates*,
   and §2's table assigns by *role*. A future gate keyed on role rather than
   crate list is A4/A10 material; noted here so the gap is visible now.

---

## 4. Cross-module corner cases, worked through

Each case names the interaction, walks it through the current tree's mechanics,
and lands it on §2's rows. These are chosen so that every owner value
(Kernel / Game / Split-with-seam) gets exercised at least once.

### CC-1 — Damage × inventory × docking (the brief's example)

**The interaction.** A craft's hull crosses zero; its cargo should spill or
burn; the ship was docked to a station, so someone else arguably owned the
hull; and a ledger row somewhere must move exactly once.

**How today's tree carries each leg:**

- *Damage.* The shooter's `step` emits an event; the target consumes it as an
  input **next tick** (`ruleset.rs:196-201`: "an attacker's step emits
  `DamageApplied(target)`; the target consumes it as an input at the next
  tick"). Death itself is a state transition inside the victim's own step —
  Regolith's wreck countdown is ordinary state (`RESPAWN_TICKS`,
  `regolith/mod.rs:44`). No kernel code interprets "damage"; the kernel
  guarantees only emission order and delivery.
- *Inventory.* Items are not in `CoreState` at all — they are ledger rows.
  Moving one is an intent whose op args are opaque to the cluster unless the
  op is one of the two reserved ids (`intent/mod.rs:208-210,257`). The
  atomicity of "cargo destroyed + ownership row released" is the FDB
  transaction envelope (row 4), not game code.
- *Docking.* Docking changes who may mutate the docked craft's state — an
  authority fact (row 3), carried by claims/leases in `orrery_authority`, not
  by any rule. The rule sees at most a reflected input ("my thrusters are
  slaved") because `step` has no lease input (`ruleset.rs:257-262`).

**Where it lands.** Three owners meet, and the join point is precisely the
policy-consumer seam:

1. The *fact* "craft X was destroyed at tick T by emitter E" is a **game**
   event — vocabulary and meaning are module-owned (row 9).
2. Its transport, ordering, and replayability are **kernel** (row 13 of §2.1,
   events-as-next-tick-inputs).
3. The durable consequence — releasing item rows — must enter through the
   transaction envelope (**kernel**, row 4) carrying **game**-meaningful args;
   the anti-dupe read-check-write stays cluster-interpreted so that no module
   can bypass single-ownership.
4. Whether the docking relationship changes who *may* submit intents for the
   docked entity is **kernel** authority policy (row 3), informed by a
   **game-declared** capability statement (the unwired per-component authority
   policy; A5 territory).

**Why this proves the split matters:** neither extreme works. If one damage
module owned the whole chain end-to-end, it would have to implement FDB
transaction semantics — duplicating the envelope and losing atomicity
guarantees for everyone else. If the kernel interpreted destruction, the next
game's death semantics (no wrecks, instant respawn, no cargo) would need
kernel surgery. The table forces both halves to stay home and gives their
meeting points names.

### CC-2 — Island boundary × spatial grid × movement

**The interaction.** A rock reaches the island edge. Does it bounce (Regolith:
`ISLAND_BOUNDARY_MM = 1_000_000`, integer velocity negation,
`regolith/mod.rs:60-61`), wrap, despawn, or keep going into the next cell?

**Working through.** The *existence* and coordinates of the boundary are
spatial-kernel facts: cell geometry, AOI and interest all derive from the grid
(row 5). But the *response* to reaching it is gameplay: another universe wraps
instead of bouncing, and Regolith's own choice is deliberately naive — reflect
with exact integer negation so the invariant check can verify it
(`regolith/invariants.rs` bounds acceleration/velocity against these same
constants). Note what happens if either side swallows the other's half:

- If the kernel bounced entities, every game inherits Regolith's physics
  flavour, and the kernel would now contain a tuned constant — gameplay
  content — inside gated infrastructure.
- If the game computed its own cell membership from position, interest
  management (`orrery_spatial`) and authority scoping would disagree with the
  simulation about which island an entity is in — two sources of truth for
  the same question.

**Where it lands.** Boundary existence/coordinates: **kernel** (row 5).
Response on contact: **game** (row 9). The quantized coordinate lattice is the
interface — the kernel guarantees positions are comparable and hashable
(VC-7); the game decides what they do. This case also shows why row 9's
"population budgets are game-owned" matters: `ISLAND_ROCK_BUDGET = 24`
(`regolith/mod.rs:48`) is Regolith's density choice, while the window-budget
mechanism it feeds belongs to spatial infrastructure.

### CC-3 — Authority handoff × rollback × an open witness window

**The interaction.** A craft's lease transfers mid-flight (drain or handover).
Meanwhile: a predicting client simulated nine ticks ahead; a witness holds
signed claims up to T−1; the disputed window spans the transfer.

**Working through.** Because outcomes are independent of the executor — same
`(universe_seed, entity, tick)` RNG, no lease input, pure step — the new
authority continues the same log without a seam: re-execution of ticks before
the handoff produces identical hashes whether run by old holder, new holder,
witness, or adjudicator. That is not luck; it is three kernel decisions
interlocking: RNG anchored to absolute tick (row 11), authority kept out of
step inputs (row 3), and claims verified against subject-signed history rather
than reporter-supplied hashes (`replay.rs:325-339`, via A1 §5.2). The
prediction client reconciles against whichever authority currently answers,
through the correction inbox (row 7).

**Where it lands.** Handoff mechanics, claim continuity, correction routing:
**kernel** (rows 3, 7, 8). The game's obligation is only purity — which is
row 9's fine print. Had the table put any part of lease semantics in the game,
re-execution could not be delegated to a third party, and D10's whole
economy — witnesses that hold no trust in the authority they watch
(`witness/src/lib.rs:3-6`) — collapses.

### CC-4 — Materialization collision × population budget × replay isolation

**The interaction.** Two bloom directors materialize pickups in the same tick;
Regolith wants at most four outstanding pickup windows island-wide
(`ISLAND_PICKUP_BUDGET`, `regolith/mod.rs:50`); meanwhile the adjudicator will
replay the emitting entity *alone*, holding none of the world
(`replay.rs:106-130`).

**Working through.** Identifiers come from the event — `(director, bloom_index,
slot)` style derivation from the emitter's own replayable inputs — so an
isolated replay reproduces the same descriptions even though it holds no other
entities (`ruleset.rs:272-278`). Collisions resolve first-writer-wins in
description order at install time (`executor.rs:144-157`) — a **kernel**
arbitration rule, deterministic because description order is emission order.
The budget cannot be enforced by asking "how many pickups exist?" — that
question is unanswerable in an isolated replay and depends on allocation order.
So Regolith encodes it as monotone counters in the director's *own state*
(bloom bookkeeping fields on `BloomDirector`), which travel with the entity
into replay. And the trap is documented where the API lives: "Materialization
descriptions are not part of `state_hash`. A game whose materialization
matters to adjudication must also record an own-state trace … an event-only
effect is invisible to state-hash goldens and adjudication"
(`ruleset.rs:280-284`).

**Where it lands.** Identity derivation discipline + collision arbitration +
hash-invisibility caveat: **kernel** (rows 2, 13). Budget semantics + the
counter encoding: **game** (row 9). This case is the cleanest demonstration
of why identity allocation can never be kernel-*provided at runtime*: the
moment ids depend on world population, isolated replay — the entire
adjudication model — stops working.

