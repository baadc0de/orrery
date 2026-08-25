# A1 — Map of the current `Ruleset` architecture (#397)

**Status:** evidence for the #395 planning tree · **Date:** 2026-08-25 ·
**Tree:** `docs/397-a1` at `1c342b1c` · **Source brief:**
[docs/plans/ruleset-ecs-migration-brief.md](ruleset-ecs-migration-brief.md)

This document establishes ground truth. It makes **no architecture
recommendation** — not a hint, not a lean. Where the evidence invites an
argument, that is recorded as "belongs in A3 (#399)" and stopped.

Verification method, used throughout:

- Every claim cites a file and line opened on this tree today. Where a claim
  rests on behaviour rather than text, the named check that enforces it was
  mutation-tested: the **guarded stage** was broken (not the check line), the
  failing check recorded by name, the change reverted, the pass re-confirmed.
  §8 lists every mutation.
- Claims are separated into what **exists today**, what is **designed but
  unwired**, and what is **speculative**. They never share a sentence.

---

## 1. The `Ruleset` surface as it exists

### 1.1 Location and ownership

The trait lives in exactly one place:

> `crates/orrery_core/src/ruleset.rs:233`: `pub trait Ruleset: Send + Sync + 'static {`

`orrery_core` depends only on `orrery_protocol`
(`crates/orrery_core/Cargo.toml:19`) plus non-engine third-party crates
(`rand_chacha`, `rand_core`, `blake3`, `libm`, `iroh-base`, `bytes`,
`postcard`, `serde`). The crate header states the intent:
"Engine-agnostic and headless … no Bevy, no tokio, no OS services"
(`Cargo.toml:9-12`). Assumption 2 of the brief ("lives in, or is primarily
owned by, the engine-independent core") is **confirmed literally**: it lives in
the core and nowhere else.

The trait's module doc scopes it deliberately narrow
(`ruleset.rs:8-13`):

> "**Scoped to what the executor, the replay harness and stage-1 checking
> need.** The §3 sketch also carries `validate_intent`, `park_tick` and
> `catch_up`. Each belongs to a consumer that does not exist yet … They are
> additive on this trait when their consumers land."

So the code defines a smaller trait than docs/06 §3 sketches. The delta is
listed in §1.4.

### 1.2 Associated types (3)

| Associated type | Bound | Line | Role |
|---|---|---|---|
| `CoreState` | `CoreCodec + Clone + Quantized` | `ruleset.rs:237` | Per-entity verifiable state; the only state `step` may touch |
| `CoreInput` | `CoreCodec + Clone` | `ruleset.rs:241` | Player command or inbound event from another entity's previous tick |
| `CoreEvent` | `CoreCodec` | `ruleset.rs:244` | Deterministic outcome event |

There is no `Intent` associated type in the code (the docs/06 §3 sketch has
one — see §1.4).

### 1.3 Methods (2 required + 3 defaulted)

| Method | Kind | Line | Notes |
|---|---|---|---|
| `id(&self) -> RulesetId` | required | `ruleset.rs:248` | Version identity pinned into frames, claims, bundles (`RulesetId { version: u32, digest: [u8; 32] }`, defined `orrery_protocol/src/verifiable.rs:59`) |
| `step(&self, view, inputs, rng) -> StepOutput<CoreEvent>` | required | `ruleset.rs:257` | One entity, one 60 Hz tick; pure (no I/O, clocks, globals); RNG is `TickRng` derived from `(universe_seed, entity, tick)` |
| `materialize(&self, event, out)` | default (empty) | `ruleset.rs:285` | Projects one emitted event into fully described entities to install; identifiers must be derived from replayable inputs, never allocation order |
| `classify_component(&self, ComponentTypeId) -> CoreClass` | default (`Cosmetic`) | `ruleset.rs:298` | §2 classification. **Zero call sites anywhere in the tree** — see §5.5 |
| `invariants(&self) -> &[Invariant<CoreState>]` | default (empty) | `ruleset.rs:314` | Stateless stage-1 checks; consumed by the witness's stage-1a path |

### 1.4 Supporting vocabulary types (all in `ruleset.rs`)

| Type | Line | Role |
|---|---|---|
| `CoreCodec` (trait) | `:30` | Canonical encoding; `encode` output is what gets hashed |
| `CodecError` | `:51` | Decode failure |
| `CoreClass` | `:63` | `Core` / `Bulk` / `Cosmetic` |
| `ComponentTypeId(u32)` | `:76` | Game-assigned replicated-component identifier |
| `StateView<'a, S>` | `:85` | Own state (mutable) + read-only neighbour snapshot + recorded-read log; `neighbor()` records every successful read (`:131`), `recorded_reads()` returns first-read order (`:144`), `entity()` is supplied by the executor (`:112`) |
| `OrderedInputs<'a, I>` | `:155` | Log-order iteration only; never sorted (VC-2) |
| `StepOutput<E>` | `:195` | `{ events: Vec<E> }` — emission order is part of determinism |
| `EntityMaterialization<S>` | `:217` | `{ entity: PersistId, state: S }` — executor has no allocator; identity is derived from replayable inputs |
| `state_hash<S: CoreCodec>(&S) -> [u8; 32]` | `:324` | blake3 over canonical encoding of the quantized state |

Related machinery in the same crate, all generic over `R: Ruleset` where noted:
`Executor<R>` ([executor.rs:48](../../crates/orrery_core/src/executor.rs)),
`ReplayHarness<R>` / `verify_bundle<R>`
([replay.rs](../../crates/orrery_core/src/replay.rs)), `AuthorityLog`
(`store.rs`), chain folding and frame signing (`log.rs`), `Invariant<S>` /
`evaluate()` (`invariants.rs:91`, `:118`), `Quantized`/`QPos`/`QVel`
(`quantize.rs`), `tick_rng` (`rng.rs`).

### 1.5 Designed but absent from the trait (docs/06 §3 sketch vs code)

docs/06-verifiable-core.md §3 sketches four members that do not exist in code
(`docs/06-verifiable-core.md:151`, `:160`, `:171`):

- associated type `Intent: CoreCodec` (sketch line ~146);
- `validate_intent(...)` — same code at three sites (submitting peer,
  witnesses, gateway);
- `park_tick(...)` — cluster-side background tick for parked entities;
- `catch_up(...)` — bulk offline catch-up.

None has a consumer in the tree; the module doc names this explicitly
(`ruleset.rs:8-13`). **Designed-but-unwired, not missing-by-accident.**

Two further surface facts matter for later comparison work:

- **`classify_component` has no consumer.** It is implemented by `Skirmish`
  (`skirmish/mod.rs:186`), `Regolith` (`regolith/mod.rs:129`), `Reference`
  (`conformance ruleset.rs:242`) — and called by nothing. A tree-wide grep for
  call sites returns only the definition and the three impls. docs/06 §2 says
  the classification "drives persistence write classes (§D11) and witness
  attention (§D10)" — that wiring does not exist yet. Declared channel,
  unwired.
- **`dyn` dispatch is structurally unavailable.** Associated types make
  `Box<dyn Ruleset>` unnameable; `game.rs:171-174` states this in context:
  "`Ruleset` has associated types, so a `Vec<Box<dyn Game>>` cannot exist and
  the usual registry shape is unavailable." The one place the backend needed
  type erasure it used boxed closures over concrete builds instead (§4.3).

---

## 2. Every `Ruleset` implementation

Exhaustive as of this tree. Production implementations first, then test-only.

### 2.1 Production (shipped rules)

| Impl | Location | `RulesetId` | Notes |
|---|---|---|---|
| `Skirmish` | `crates/orrery_games/src/skirmish/mod.rs:177` | `SKIRMISH_RULESET`, version 2, digest `[0x5C; 32]` (`:100`) | Reference game for P1/P4 measurement; carries `Tamper` builds that keep the honest id (`:120-123`); supplies `classify_component` and stage-1 invariants (`skirmish/invariants.rs:76`) |
| `Regolith` | `crates/orrery_games/src/regolith/mod.rs:122` | `REGOLITH_RULESET`, version 8, digest `[0x63; 32]` (`:74`) | The game the P4 shakedown (#329), p1-swarm harness and regolith client run |
| `Reference` | `crates/orrery_conformance/src/ruleset.rs:233` | `REFERENCE_RULESET` (`:42`) | The cross-platform determinism corpus's kernel; kinematic movement over `libm` + integer combat |

A tampered build keeps the honest `RulesetId` on purpose — "a cheater claims to
be running the rules; the claim is what the witness holds it to"
(`game.rs:30-36`).

### 2.2 Test/support impls (each inside its own crate's tests)

| Impl | Location | Used by |
|---|---|---|
| `Kinematic` | `crates/orrery_core/src/executor.rs:240`; re-declared in test files | executor unit tests; `tests/adjudication.rs:100`, `tests/round_trip.rs:93`; `orrery_witness/tests/{streaming,detection,multi_entity,escalation}.rs` |
| `Materializer` | `executor.rs:341` | materialization tests (derived child ids, first-writer-wins) |
| `Introspect` / `SelfPeek` / `Peeker` | `executor.rs:539`, `:579`, `:617` | entity attribution / neighbour isolation / read-recording tests |
| `OtherBuild` | `orrery_core/tests/adjudication.rs:419` | wrong-build → `UnknownRuleset` path |
| `Growing` | `orrery_games/tests/materialization.rs:84` | split/drop materialization counters |
| `Counting`, and macro-generated `$name` impls | `orrery_persistd/tests/report_escalation.rs:107`; `orrery_persistd/src/adjudication.rs:771` | persistd adjudication/escalation tests |

---

## 3. Every generic call site (`R: Ruleset`)

Exhaustive. Four first-party crates are generic over `R: Ruleset`; everything
else consumes them concretely or not at all.

### 3.1 `orrery_core` — defines the abstraction

| Item | Line |
|---|---|
| `pub struct Executor<R: Ruleset>` — impl block `:54`, `step_entity` `:110` | `executor.rs:48` |
| `pub struct ReplayHarness<R: Ruleset>` — impl block `:96` | `replay.rs:84` |
| `pub fn verify_bundle<R: Ruleset>(ruleset, seed, authority, bundle) -> Verdict` | `replay.rs:265` |

### 3.2 `orrery_witness` — engine + optional Bevy adapter

Engine half (`witness.rs`, Bevy-free):

| Item | Line |
|---|---|
| `struct Watched<R: Ruleset>` — one `Executor<R>` per watched entity (`:397`) | `witness.rs:358` |
| `pub struct Witness<R: Ruleset>` — impl block `:461`; constructor takes `ruleset_factory: fn() -> R` (`:523-527`) | `witness.rs:422` |

Adapter half (`plugin.rs`, behind the default-on `bevy` feature,
`Cargo.toml:18-19`):

| Item | Line |
|---|---|
| `pub struct WitnessState<R: Ruleset>(pub Witness<R>)` (Resource) | `plugin.rs:384` |
| `pub struct WitnessPlugin<R: Ruleset>` + `Plugin` impl | `plugin.rs:435`, `:459` |
| `pub fn ingest_peer_traffic<R>` | `plugin.rs:619` |
| `pub fn sweep_repairs<R>` | `plugin.rs:808` |
| `fn route<R>`, `fn escalate<R>` | `plugin.rs:859`, `:923` |

### 3.3 `orrery_games` — implements games; adds the measurement trait

| Item | Line |
|---|---|
| `pub trait Game: Ruleset + Sized` — adds `spawn`, `honest_inputs`, `deliver`, `trajectory`, `tampered` | `game.rs:116` |
| `GameVisitor::visit<G: Game>` / `for_each_game` — static-dispatch visitor because `dyn` is unavailable (`:171-174`) | `game.rs:177`, `:181` |
| `Entry<G>`, `TickRecord<G>`, `Play<G>`, `play<G>`, `adjudicate<G>`, `adjudicate_isolated<G>`, `divergence<G>` | `scenario.rs:119`, `:132`, `:140`, `:172`, `:344`, `:390`, `:457` |

### 3.4 `orrery_persistd` — generic only at the registration edge

| Item | Line |
|---|---|
| `AdjudicationExecutor::register<R: Ruleset>(&mut self, factory: fn() -> R)` — boxes the build into `Worker = Box<dyn Fn(NodeId, &EvidenceBundle) -> AdjudicationOutcome + Send + Sync>` (`:278-279`) | `adjudication.rs:350` |
| `fn adjudicate_bundle<R: Ruleset>(factory, seed, authority, bundle)` — called inside the boxed closure (`:557`, `:559`) | `adjudication.rs:551` |

Everything else in persistd is **not** generic: reports route by
`report.bundle.ruleset` against up to `RETAINED_BUILDS = 3` registered builds
(`:33`, `:393-403`); an unregistered id resolves to
`UnadjudicableReason::UnknownRuleset`, never a strike (`:400-403`).

### 3.5 `orrery` (facade) — pure pass-through

`OrreryClientPlugins<R: Ruleset>` exists **only** to name `WitnessPlugin::<R>`
in the group (`lib.rs:467`, `:515`). None of the other seven members of the
group is generic over `R`. The prelude re-exports `orrery_core::Ruleset`.

### 3.6 Out-of-tree consumers

| Consumer | Use | Lines |
|---|---|---|
| `clients/regolith` (Bevy client) | drives `Executor<Regolith>` directly for local simulation | `src/lib.rs:83`, `combat.rs:876-881`, `campaign.rs:289`, `hud.rs:1427` |
| `gates/p1-swarm` (P1/P4 harness) | bots drive `Executor<Regolith>`; installs `WitnessPlugin::<Regolith>`; adjudicates via `orrery_core::verify_bundle` | `bot.rs:535`, `bot.rs:574-575`; `adjudicate.rs:28`, `:91` |

### 3.7 Crates with no `Ruleset` contact at all

`orrery_protocol` (defines only the `RulesetId` wire struct,
`verifiable.rs:59`), `orrery_net`, `orrery_spatial`, `orrery_authority`,
`orrery_predict`, `orrery_persist_client`, `orrery_coordinator`,
`orrery_identity`, `orrery_seed`. This is the load-bearing fact behind §4: the
entire client plugin stack below the facade never names the trait.

---

## 4. Crate dependency impact

### 4.1 The spine (verified from each crate's `Cargo.toml`)

```text
orrery_protocol   (no bevy, no tokio; blake3/rand_chacha/hmac/sha2 for D28)
  ├── orrery_core (no bevy, no tokio; rand_chacha, libm, blake3, iroh-base)
  │     ├── orrery_games        (bevy-free; + orrery_protocol)
  │     ├── orrery_conformance  (bevy-free; + orrery_protocol)
  │     ├── orrery_witness      (engine bevy-free; optional "bevy" feature adds
  │     │                        bevy_app/bevy_ecs/bevy_time + orrery_net;
  │     │                        default = ["bevy"])
  │     ├── orrery_persistd     (bevy-free; takes orrery_witness with
  │     │                        default-features = false — engine only)
  │     └── orrery (facade)     (full bevy; depends on every client plugin)
  └── every other first-party crate (net/spatial/authority/predict/
        persist_client/coordinator/identity/seed) — protocol only
```

Client-path crates use Bevy but never `orrery_core`:
`orrery_net` (`aeronet_io`, `aeronet_iroh`, `aeronet_tokio_runtime`),
`orrery_spatial` (`bevy_replicon`), `orrery_predict` (`lightyear`),
`orrery_persist_client` (`aeronet_*`, `bevy_replicon` + `uplink`),
`orrery_authority` (`bevy_app`/`bevy_ecs` only).

### 4.2 How far genericity propagates

- **Within the workspace:** `R: Ruleset` appears in exactly four crates —
  `orrery_core` (definition), `orrery_witness` (engine + adapter),
  `orrery_games` (`Game` supertrait + scenario harness), `orrery_persistd`
  (two functions at the registration edge), plus the facade's pass-through.
- **Where it stops:** the backend stops it at one function. `register<R>`
  erases into a boxed closure immediately (`adjudication.rs:350-354`); no
  persistd type beyond that signature is generic. The client stack below the
  facade stops it entirely: net/spatial/authority/predict/persist-client are
  compiled without knowing rules exist.
- **Monomorphization surface today:** the concrete instantiations in tree are
  `Executor<Kinematic>`/test impls, `Witness<Regolith>` (+ plugin),
  `Executor<Regolith>`, `ReplayHarness<Regolith>` via `verify_bundle`,
  `Reference` corpus paths. One game binary links one game; persistd holds up
  to three version-keyed builds behind boxes.

### 4.3 The type-erasure workaround, stated precisely

Because associated types forbid `dyn Ruleset`, persistd stores per-build
workers as `Box<dyn Fn(NodeId, &EvidenceBundle) -> AdjudicationOutcome +
Send + Sync>` capturing a concrete factory (`adjudication.rs:278-279,
350-360`). This is composition-time registration keyed by `RulesetId`, not
runtime dispatch over the trait — D38 clause (c) cites this exact shape as the
pattern additive registration follows (docs/adr/0038 §(c)).

### 4.4 Observed vs speculative on "generic infection"

- **Observed today:** propagation is narrow (four crates). The brief's feared
  failure mode — "generic type parameters infecting unrelated crates" — is
  not present in this tree at its current scale. That is a statement about
  what exists, not a claim about what happens when a real game grows many
  modules; whether the seam would hold under that growth is exactly A3's
  comparison question, and this document takes no position.
- **Speculative (marked as such):** any claim about AAA-scale monomorphization,
  compile-time blowup, or god-trait pressure is unsupported by evidence in
  this repository — there is one small reference game set and no third-party
  game yet (`D21`: "no third-party rules are loaded anywhere in the tree").

---

## 5. Dataflow today: input → state → witness projection → persistence

### 5.1 The tick (authority-side execution)

`Executor::step_entity(entity, tick, inputs)` (`executor.rs:110-142`):

1. Remove the entity's state from the map; the remaining map **is** the
   neighbour snapshot (`:116-117`) — a step cannot observe same-tick mutations
   of others, and cannot see itself (`SelfPeek` test, `:573-612`).
2. Build `StateView`, `OrderedInputs`, and `tick_rng(seed, entity, tick)` —
   VC-3 seeding (`:118-120`).
3. Call `ruleset.step` once (`:122`).
4. Collect `recorded_reads()` for the log (`:123`).
5. Quantize own state, then hash: `state_hash(&own)` after quantization, VC-7
   ("snap before anything hashes or replicates it", `:126-127`).
6. For each emitted event in emission order call `ruleset.materialize`
   (`:130-133`) and install descriptions first-writer-wins in description
   order (`install_materializations`, `:144-157`).

`insert()` also quantizes on install (`:78-81`), so a snapshot loaded from a
bundle starts on the lattice.

Inputs reach the executor as already-ordered slices; ordering is fixed by the
authority before execution (VC-2). Cross-entity effects travel only as events
consumed next tick — there is no live neighbour channel in a step, and until a
`NeighborFrame` producer exists, recorded reads have no replay consumer
(core-gates.sh:126-132; conformance `ruleset.rs:287-298` states the residual).

### 5.2 Replay / adjudication (same code path, different driver)

`ReplayHarness::load_claimed_snapshot` verifies snapshot bytes against the
claim hash *before* loading, then installs **exactly one entity** — the
disputed one; its neighbour map is therefore empty at replay
(`replay.rs:106-130`; witness doc `witness.rs:406-421` explains why this makes
neighbour-reading rulesets unadjudicable). `replay()` verifies frame
signatures and chain continuity while decoding logged inputs per tick, then
steps every tick including empty ones (`:156-254`), discarding materialized
children so the window stays closed (`:248-250`). `verify_bundle` is the pure
entry point: wrong `RulesetId` → `UnknownRuleset`; signature failures →
`EvidenceForged`; computed-vs-committed hash mismatch →
`Verdict::Confirms { at, DiscreteMismatch }` (`:265-376`). Verdicts rest only
on subject-signed claims, never on the reporter-supplied `claimed_hashes`
(`:325-339`).

### 5.3 The witness pipeline (stages 1–3, client side)

Engine (`witness.rs`) and adapter (`plugin.rs`), per `lib.rs:1-30`:

1. **Stage 1a — invariants.** `Witness::observe` runs the game's
   `Ruleset::invariants()` on every received state sample (`witness.rs:663`;
   `evaluate` at `orrery_core/src/invariants.rs:118`). Runs on everything a
   peer receives, watched or not.
2. **Stage 1c — re-execution.** Frames stream to witness-set peers
   (`WitnessMsg::Frame`); each watched entity has **its own executor**
   (`Watched.executor`, `witness.rs:397`); sharing one executor across
   entities would expose neighbours sitting at unrelated ticks and diverge
   from the adjudicator's single-entity replay — stated as a correctness
   requirement at `witness.rs:406-421`. Claims are compared against computed
   hashes in `check_pending_claims` (`:1543-1579`).
3. **Stage 2 — audit window.** A mismatch arms `audit_window`
   (`:1591-1596`): opens at the newest claim tick the witness holds a
   snapshot for (demonstrable agreement), closes at the disputed claim,
   bounded by `window_ticks`.
4. **Stage 3 — report.** `raise` assembles a self-verifying
   `DiscrepancyReport`; in shadow mode (default) it is counted, not filed
   (`WitnessConfig::shadow_mode`, `witness.rs:56`). Gaps are repair requests,
   never accusations; persistent non-answer escalates as `Stalled`
   (`:1309+`, plugin `sweep_repairs`).

Adapter plumbing: `publish_authored` records authored frames/claims into
`AuthoredLog` and broadcasts to witness links; `ingest_peer_traffic` feeds
inbound messages through `route`, which attributes every signal from the
engine rather than the carrier and files reports via `ReportFiled`
(`plugin.rs:517-595`, `:619-780`, `:859-916`).

### 5.4 Persistence and intents (Ruleset-opaque by construction)

- The cluster never interprets game state bytes. `EntityRecord.components` is
  an opaque bag (`persistd/src/actor.rs:121-134`); intent op payloads are
  "`Ruleset`-opaque" with only `LEDGER_CREDIT_OP = 0` cluster-interpreted
  today (`intent/mod.rs:208-245`). docs/08 §2.2 keeps op semantics Rules-side.
- `RulesetId` is the discriminator: pinned into claims (`protocol/authority.rs:32`),
  frames, bundles (`verifiable.rs:170,203,213,292`), strike rows
  (`adjudication.rs:104`), and persisted rows; D38 clause (d)(3) pins schema
  versions as orthogonal to `RulesetId.version`.
- Game rules enter persistd through exactly two seams today:
  1. `intent::IntentValidator` (`intent/mod.rs:156`) — a **separate trait**
     (`validate(&self, &Intent, &IntentContext) -> IntentVerdict`), currently
     served by `PermissiveValidator` (bring-up default, `:176`) or
     `BaselineIntentValidator` (`:812`). There is no bridge from it to
     `Ruleset` yet.
  2. `AdjudicationExecutor::register<R>` (§3.4). An unregistered build makes
     every report against it `Unadjudicable(UnknownRuleset)`; the stock
     binary ships no adjudicator ("registering a build means linking a
     `Ruleset`", `bin/persistd.rs:1261-1263`).
- Corrections flow back: confirmed verdicts carry replayed canonical state for
  D10's correction response (`AdjudicatedState`, `adjudication.rs:282-298`),
  queued client-side through `AuthorityCorrectionInbox` (facade
  `queue_authority_corrections`, facade lib.rs:336).

### 5.5 Field host: not a crate; two stand-ins exist

There is no `orrery_field_host` crate. The executor doc anticipates one
("the field host's parked-cell catch-up", `ruleset.rs:10-12`) but nothing
implements parked-cell catch-up. What exists instead:

- **p1-swarm bots** (`gates/p1-swarm/src/bot.rs`) are the closest thing to a
  field host: a Bevy App per bot driving `Executor<Regolith>` directly
  (`:535`), logging inputs *before* execution and tick hashes after
  (`chain.log_inputs` / `log_tick_hash`, `bot.rs:718-731`), running the real
  `WitnessPlugin::<Regolith>` (`:574-575`), spatial plugin, upload budgets,
  and adjudicating via `verify_bundle` (`adjudicate.rs:91`). This is the path
  P4 measures.
- **The regolith client** drives `Executor<Regolith>` for its local
  simulation (`clients/regolith/src/lib.rs:83` etc.) over the swarm exterior
  bridge.

### 5.6 Unwired legs of this dataflow (exists vs missing)

- `PublishFrame` / `PublishClaim` have **no producer** outside
  `orrery_witness`'s own tests: a tree-wide grep finds writers only in
  `orrery_witness/tests/streaming.rs`. No production system converts live
  `Executor` outcomes into signed frames/claims yet; p1-swarm does that
  assembly harness-side (`chain.rs`).
- `classify_component`: zero consumers (§1.5).
- `NeighborFrame`: no producer; recorded neighbour reads have no replay
  consumer (core-gates.sh:126-132).
- `validate_intent` / `park_tick` / `catch_up`: absent from trait and tree
  (§1.5).

---

## 6. The D21 compatibility boundary as it stands

Read both records; do not trust summaries (including this one).

**D21 ([docs/adr/0021](../adr/0021-ruleset-distribution.md)) freezes
`orrery_persistd`'s public exports**, not the workspace:

> "Frozen means: a breaking change to the surfaces below requires an ADR that
> names this one, not a patch release. Additive change — new methods, new
> types, new default-carrying config fields — is not breaking and needs no
> record." (`:61-64`)

The frozen table (`:66-77`): `CellRuntime`/`RuntimeConfig`/`JournalConfig`;
`intent::IntentValidator`, `intent::IntentExecutor`;
`AdjudicationExecutor::register`, `RETAINED_BUILDS`;
`checkpoint::*`; `cluster::Router`/`Cluster` + gateway exports;
`lease::LeaseStore`, `fence::FenceStore`; `journal::Journal` methods +
config.

Two boundary facts the epic needs stated precisely:

1. **`orrery_core::Ruleset` appears nowhere in D21's frozen table.** D21's own
   Consequences call it "already … the only thing the cluster calls"
   (`:98-104`) — "the seam in spirit". D38 clause (c) addresses this head-on:
   "A required trait method would arguably evade the freeze's letter while
   cutting against it; this record declines that branch entirely by pinning
   composition-time registration as the mechanism"
   (docs/adr/0038-at-rest-schema-versioning.md:161-167). And: "if a future
   design genuinely wants migrators on the trait, that specific change names
   D21 and pays its ADR" (`:167-169`). So under the two records read
   together: **adding a defaulted method to `Ruleset` is additive; adding a
   *required* method breaks every implementation and is treated by D38 as
   naming D21** — which matches #395's constraint line verbatim.
2. **D21 contains one stale internal citation:** it names the intent seam as
   "`Ruleset::validate_intent` behind `intent::IntentValidator`" (`:20-21`).
   Today there is no `Ruleset::validate_intent` anywhere (§1.5);
   `IntentValidator` is an independent trait with baseline/permissive impls.
   The freeze itself is unaffected — what is frozen is `IntentValidator`,
   which exists — but the parenthetical describes a trait member that was
   never implemented.

Also relevant: D21 decides link-time rules distribution for 1.0 (no WASM, no
dynamic loading, `:38-58`); reopening that is the owner's call, and its named
reopeners (`:85-90`) say nothing about ECS migration.

---

## 7. Tests and gates that constrain this area

### 7.1 Static: `scripts/core-gates.sh` (per-commit, CI `gates` lane via check.sh:270)

Gated crates (`core-gates.sh:37`): `orrery_core`, `orrery_games`,
`orrery_conformance` — "the games whose `Ruleset` implementations are the code
those rules are actually about" (`:33-36`). Rules-only crates (`:42`):
`orrery_games`, `orrery_conformance`.

| Clause | What it enforces | Lines | Mutation proof (§8) |
|---|---|---|---|
| Bevy-free | `cargo tree -p <crate>` must contain no bevy | `:71-75` | M1 |
| VC-4 | no std `HashMap`/`HashSet` in gated sources (comment-stripped) | `:95-97` | M2 |
| VC-8 | no ambient inputs (`Instant::now`, `SystemTime::now`, `thread_rng`, `std::env::var`, …) | `:103-105` | M4 |
| VC-6 | no std float transcendentals, path **and** method form (`x.sqrt(`) | `:117-123` | M5 |
| Neighbour ban | `\bview\.neighbor\s*\(` forbidden in rules crates — cross-entity effects travel as events; rationale: no `NeighborFrame` producer exists and the adjudicator installs one entity, so a neighbour branch "adjudicates differently than it executed" | `:126-139` | M3 |

### 7.2 In-process determinism (crate tests)

- `orrery_core`: runs an identical tick twice and compares hashes
  (`executor.rs:489-501`); input-order and tick-in-RNG sensitivity tests
  (`:504-529`). Mutation M6 proves the neighbour-read recording test is live.
- `orrery_conformance --test conformance`: same-process repeat + committed
  golden corpus (`corpus/golden.json`) checked on every platform. Mutation M7
  proves a one-constant change to the reference ruleset fails
  `this_platform_matches_the_committed_golden` across all five corpus cases.
- `orrery_games`: golden chains per game/scenario in `src/golden.rs`; bumping
  a game's `RulesetId` version is mandatory when its rules change
  (`skirmish/mod.rs:96-99` doc).

### 7.3 Cross-platform and nightly

- ci.yml `determinism` matrix: four targets (x86_64 Linux/Windows, aarch64
  Linux/macOS) run `cargo test -p orrery_core -p orrery_protocol -p
  orrery_conformance -p orrery_games` plus a per-platform `emit`
  (`ci.yml:673-735`), then compare digests. Regolith is deliberately excluded
  (#344).
- nightly.yml `determinism-soak`: ten repeats of the extended corpus in one
  process to catch per-*process* nondeterminism (`nightly.yml:1187-1249`).
- P4 pipeline digest: `scripts/p4-ledger.sh:33-35` hashes the git trees of
  `orrery_witness`, `orrery_core`, `orrery_games` and `gates/p1-swarm` into
  every banked hour. Touching any of them resets banked hours — a temporal
  blocker tied to #329, not an architectural rule.

### 7.4 Witness behaviour gates

`orrery_witness/tests/detection.rs` pins the detection pipeline end-to-end
(25 tests): shadow-mode counting, single raise per disputed claim,
speed-cheat caught by re-execution, stall/re-anchor coverage accounting.
Mutation M8 (suppressing the claim-hash comparison in
`check_pending_claims`) kills four of them by name, including
`a_speed_cheat_is_caught_by_re_executing_its_own_log`.
`gates/p1-swarm-gate.sh` asserts the harness's false-positive/disposition
criteria over real runs (nightly).

---

## 8. Mutation log (break stage → named check dies → revert → passes)

All mutations ran against this tree; each revert re-ran the check and got the
recorded passing result.

| # | Guarded stage broken | Named check that died | Observed failure | Reverted result |
|---|---|---|---|---|
| M1 | added `bevy_ecs` to `orrery_core` `[dev-dependencies]` | `scripts/core-gates.sh` clause 1 | `core-gates: orrery_core has Bevy in its dependency graph`, exit 1 | exit 0 |
| M2 | `view.neighbor(view.entity())` injected into `Skirmish::step` (`skirmish/mod.rs:208`) | core-gates clause 5 | named file:line + `live neighbour read in a Ruleset — cross-entity effects travel as events (docs/06 §3)` | exit 0 |
| M3 | `HashMap` constructed in `Skirmish::step` | core-gates clause 2 (VC-4) | `VC-4: std HashMap/HashSet in a gated crate…` | exit 0 |
| M4 | `SystemTime::now()` in `Skirmish::step` | core-gates clause 3 (VC-8) | `VC-8: ambient input in a gated crate…` | exit 0 |
| M5 | `(f64).sqrt()` in `Skirmish::step` | core-gates clause 4 (VC-6, method form) | `VC-6: std float transcendental (method form)…` | exit 0 |
| M6 | removed read recording in `StateView::neighbor` (`ruleset.rs:131-137`) | `cargo test -p orrery_core --lib ruleset::tests` | `reading_a_neighbour_records_it_once` FAILED (`left: []`, `right: [PersistId(5)]`); `3 passed; 1 failed` | `4 passed` |
| M7 | `DRAG_PER_SEC` 0.15 → 0.16 in conformance reference ruleset | `cargo test -p orrery_conformance --test conformance` | `this_platform_matches_the_committed_golden` FAILED — all five cases: chain hashes differ, deltas quantified against bands | `13 passed` |
| M8 | claim-vs-computed comparison suppressed in `Witness::check_pending_claims` (`witness.rs:1565`) | `cargo test -p orrery_witness --test detection` | 4 named failures incl. `a_speed_cheat_is_caught_by_re_executing_its_own_log`, `shadow_mode_detects_everything_and_files_nothing`; `21 passed; 4 failed` | `25 passed` |

Baseline results were recorded before each mutation; no mutation landed "on
both sides" of an equality; all failing suites produced real result lines.

---

## 9. The brief's ten current-state assumptions: verdicts

Verdict scale: **confirmed** (evidence matches the assumption as written),
**corrected** (partially true; the delta is stated), **unverifiable** (no
in-tree evidence can decide it).

| # | Assumption (brief, `ruleset-ecs-migration-brief.md:113-122`) | Verdict | Evidence |
|---|---|---|---|
| 1 | `orrery_protocol` and `orrery_core` are intentionally free of full-engine Bevy dependencies | **Confirmed** — with a precision the brief's own issue note anticipated: "full-engine" is doing work here. Neither crate names any bevy crate at all, not even `bevy_ecs`: protocol deps are glam/iroh-base/serde/postcard/bytes/blake3/rand_chacha/hmac/sha2 (`orrery_protocol/Cargo.toml`); core adds rand_chacha/libm (`orrery_core/Cargo.toml:22-35`). Both state the intent in comments (`protocol:7-8` "Engine-agnostic (D15): no Bevy, no tokio"; `core:9-12`). Enforcement is mechanical: core-gates clause 1 fails on *any* bevy in `cargo tree`, dev-deps included (M1). The gate covers `orrery_games`/`orrery_conformance` too — stronger than the brief claims. | Cargo.tomls; gate M1 |
| 2 | `Ruleset` lives in, or is primarily owned by, the engine-independent core | **Confirmed** | Defined once at `orrery_core/src/ruleset.rs:233`; re-exported, never re-declared |
| 3 | The built client path composes networking, spatial, authority, prediction, witnessing, persistence-client plugins | **Confirmed** | `OrreryClientPlugins<R>` members list, facade `lib.rs:404-524` (net → spatial → authority → island-binding → predict → witness → persist-client → escalation) |
| 4 | Aeronet, Replicon, Lightyear are important implementation dependencies of the Bevy client path | **Confirmed** | aeronet: `orrery_net/Cargo.toml:18-20`, `orrery_persist_client:28-29`; replicon: `orrery_spatial:29`, `persist_client:37`; lightyear: `orrery_predict:23`. All vendored/pinned per D14; `bevy_replicon` is a patched fork exposing uplink change-detection (root `Cargo.toml`) |
| 5 | Prediction is the area most tightly coupled to Lightyear/Bevy semantics | **Corrected**. True that prediction is the lightyear-coupled area — `orrery_predict` calls itself "the only one whose internals name lightyear types … the plan-B seam" (`lib.rs:3-6`). But the coupling is **configuration-layer only**: `orrery_predict` does not depend on `orrery_core`, never names `Ruleset`, and drives no rules execution. Today's actual rules-execution coupling to Bevy sits elsewhere: the witness plugin (`WitnessPlugin<R>`, bevy feature) and the game clients driving `Executor` directly inside Bevy apps (regolith, p1-swarm bots) | `predict/lib.rs:1-15`; §3.7 |
| 6 | Orrery uses stable persistent/network identity separately from ephemeral Bevy entities | **Confirmed** | Durable `PersistId` everywhere in protocol/logs/journal; island-scoped `EphemeralId` minted by `EphemeralRegistry`, which maps bevy `Entity ↔ EphemeralId` (`orrery_authority/src/ephemeral.rs:342-430`); `RulesetId` for builds. The brief's proposed `PersistId` component pattern is already the working shape |
| 7 | Fixed-step simulation, scoped determinism, rollback, authority handoff, persistence, witnessing are required | **Confirmed** | Fixed tick `TICK_HZ = 60` constant, "never a measurement" (`executor.rs:25-28`); scoped determinism = D9 + VC-1..VC-8 + four-platform matrix; rollback/prediction = D8 + orrery_predict; handoff = D7/D26; persistence = D11/D19; witnessing = D10 + shipped pipeline (§5.3). Note for A2+: *rollback of canonical rules state* is currently a replay/correction story (`AuthorityCorrectionInbox`), not an ECS-style world rewind |
| 8 | The recently completed external bridge demonstrates a directional framed boundary but not yet a complete engine-neutral playable-client API | **Confirmed** | Exterior-peer bridge: frame grammar `gates/p1-swarm/src/exterior.rs` (`JoinRequest::VERSION = 2`, `:364`), iroh pump `bridge.rs` (one bidirectional stream, lane byte, `EXTERIOR_ALPN = b"orrery/exterior/2"`, `net.rs` client twin at `clients/regolith/src/net.rs:31`); two-process proof merged as #388. Regolith remains a full Bevy client over those frames — no engine-neutral client API exists |
| 9 | Services and wire types should not need access to rendering or presentation worlds | **Confirmed** (as current fact, not just aspiration) | `orrery_persistd` has zero bevy deps and takes the witness engine `default-features = false` (`persistd/Cargo.toml:36`); `orrery_protocol` has none; coordinator/identity/seed likewise. The wire never carries engine types — `RulesetId`/`PersistId`/`EphemeralId` only |
| 10 | The intended future Unreal integration should consume commands and presentation frames rather than reimplement via Actor replication/Iris | **Unverifiable** as current state; forward-looking design intent. No Unreal, Iris, or C-ABI code exists anywhere in the tree ("Unreal" appears only as a Replication-Graph design citation, docs/03-replication.md:106). Nothing contradicts it either; the repo simply has no evidence to confirm or deny an intent statement about future integration. A2–A11 should treat this as a requirement imported from outside the repository | docs/03-replication.md:106 |

Summary: **8 confirmed, 1 corrected (#5), 1 unverifiable (#10)** — with two
precision riders worth carrying forward: #1 holds because the Bevy-free
property is *gate-enforced*, not merely intended; #5 confirms lightyear
coupling sits in prediction while correcting where rules-execution coupling
to Bevy actually lives today.

---

## 10. Stale citations found while verifying

Citation drift found in records other than this document. Each still-open
question is whether the *claim* survived; all did except where noted.

| Record | Citation | What it says | Current truth |
|---|---|---|---|
| ADR-0038 §2 | `ruleset.rs:211` — "the `Ruleset` trait has no migration hook" | line 211 today is inside `EntityMaterialization`'s doc block | Claim still true (no migration hook exists); line drifted ~100 lines by the addition of `materialize` (`ruleset.rs:285-291`) |
| ADR-0038 §2 | `adjudication.rs:324` for `RETAINED_BUILDS = 3` | — | Constant now at `adjudication.rs:33`; value unchanged |
| ADR-0038 §2 | `ruleset.rs:8-13` module doc scoping | — | Still accurate verbatim |
| ADR-0021 §Context | "`Ruleset::validate_intent` behind `intent::IntentValidator`" (`:20-21`) | implies the validator wraps a trait method | `validate_intent` was never implemented; `IntentValidator` is standalone (`intent/mod.rs:156`). Freeze unaffected; parenthetical stale |
| Brief | `p{N}-*` phase paths | pre-#391 layout | Now under `gates/` (`gates/p1-swarm` etc.); `scripts/p4-campaign-session.sh` exists (added #410, commit 596859e9) — both checked because the task flagged them |

No citation in AGENTS.md relevant to this area proved wrong during this task.

---

## 11. Unsure, and what this document deliberately does not do

Stated as unsure rather than smoothed over:

1. **Whether `Executor`'s neighbour-snapshot semantics would survive an ECS
   storage unchanged.** The snapshot is structural today (the map minus the
   stepping entity). Whether any target architecture reproduces exactly this
   observable behaviour is a comparison question — not assessed here.
2. **Compile-time/monomorphization cost of the current shape.** No benchmark
   exists in-tree for "cost of R-generics"; the brief asks for one (phase 0,
   baseline build times). This map records instantiation sites but no timing
   evidence.
3. **`classify_component`'s intended consumer set.** It has implementations
   and zero call sites; whether the unwired consumer is persistence write
   classes, witness attention, replication, or all three is stated in docs
   (06 §2) but not decidable from code.
4. **Regolith-vs-Skirmish divergence.** Two reference games exist and both
   carry goldens; why P4's shakedown standardized on Regolith (v8) is
   recorded in game docs I did not fully trace. Not load-bearing for this
   map.

Deliberately not done:

- **No recommendation.** Nothing above argues for retaining `Ruleset`,
  adopting ECS, or any hybrid. Where the evidence seemed to point somewhere,
  it is recorded as evidence only; the argument belongs in A3 (#399).
- **No Rust changes were made.** All mutations lived for one command run and
  were reverted with their passing results re-confirmed (§8). The only files
  this branch adds are this document.

