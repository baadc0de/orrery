# Spike — Should `TickBackend::state` return canonical bytes?

**Draft, propose-only. Do not merge.** This prices the handoff from
[#804](https://github.com/baadc0de/orrery/pull/804) against the owner decision
and corrections recorded on
[#793](https://github.com/baadc0de/orrery/issues/793). It changes no Rust API,
no canonical byte, and no ADR.

Branch: `spike/state-returns-bytes`.

---

## 0. Verdict

**Owned canonical bytes remove E0515, but replacing `TickBackend::state` is not
mechanical and is not worth taking as the next rung.**

The immediate-operation version of #804's consumer claim is false too. Nine
external production calls through a `B: TickBackend<R>` divide into three
encodes and six clones, but the trait's provided `section_state` body is a
tenth production consumer and projects typed state. The semantic version is
wider still: the three scenario clone sites retain typed `R::CoreState` in
logs and invariant history. The three differential-witness clone sites pass a
typed value to `InputLogProducer`, whose present API hashes it; they can avoid
decode only by adding a byte/hash entry point. A bytes replacement therefore
adds decode work or forces those APIs to change.

The allocation price has two very different readings:

- On the byte-output paths, it mostly moves an allocation that already exists.
  The committed 24,000-craft sweep already allocates 24,000 buffers carrying
  3,792,000 payload bytes and costs 2.664 ms on the executor / 3.058 ms on the
  ECS. A bytes-returning backend need not add another allocation there.
- On a formerly typed read, it introduces one owned buffer per call and then a
  decode if the consumer still needs the value. One full-craft read is 158
  canonical bytes. A once-per-entity 24k read is therefore 24,000 allocations
  and 3.792 MB of logical payload; at 60 Hz that is 1.44 million allocations/s
  and 227.52 MB/s before allocator and `Vec` capacity overhead.

The actual 24k `step(1)` does **not** call `TickBackend::state`, so the proposed
return type adds zero allocations to that measured tick. The tick already
allocates while hashing: `state_hash` calls `to_canonical` once per stepped
entity. That distinction matters more than a hypothetical per-tick price.

The conviction path is similarly not where the new cost lands. A maximum
180-tick single-entity adjudication does not read `state` each tick; it reads
once after a confirmed replay to produce the correction bytes. That call
already allocates one canonical buffer. The dangerous change is instead one of
trust boundaries: today the replay harness encodes the typed state centrally;
a bytes-returning backend can hand the adjudicator bytes that disagree with the
state hash unless the common whole-state `CoreCodec` remains the only encoder
and the Tier-H guards pin that equality.

**Recommendation:** do not replace `state` for the storage rung. The rung has
no capacity case on the committed all-Craft workload (a `Craft` is already the
sum's largest variant), and bytes do not remove the next round-trip:
`canonical_step` still takes `&mut R::CoreState`. If the owner still wants to
take it, first write a new ADR explicitly superseding D42 (b)(2)(1), then
choose between:

1. a typed owned-or-borrowed return such as `Cow<'_, R::CoreState>`, which
   preserves the real typed consumers and keeps the whole-state codec singular;
2. a bytes-only backend read plus a separate typed introspection capability for
   scenario/differential tooling, with explicit decode cost; or
3. the larger native-rules work that removes the sum from `canonical_step`,
   where the storage decomposition can produce value rather than only move its
   cost.

No option is licensed to encode a section by itself.

---

## 1. Scope and normative constraints

The relevant accepted text is ADR-0042 clause (b)(2), lines 261–288 on this
tree:

- clause 1 says, explicitly, **“`TickBackend::state` keeps returning
  `Option<&R::CoreState>`”**;
- clause 2 says sections are a read projection and every byte committed by
  authority, adjudication and `state_hash` remains the canonical encoding of
  the **whole** `CoreState`;
- clause 4 keeps section access on own state only.

This spike's premise conflicts with clause 1. Repository governance does not
permit silently editing that sentence: adoption owes a **new ADR explicitly
superseding D42 (b)(2)(1)**. If a decomposed backend directly implemented a
second per-section byte encoder instead of rebuilding the whole state and
calling its existing `CoreCodec`, it would also change D42 (a)'s single
canonical-producer obligation and D42 (b)(2)(2); that version owes an explicit
supersession too. This spike recommends avoiding it.

D43 does not need to change merely because an owned whole-state byte buffer
crosses the seam. Its Tier-H obligations still bind: byte equality across
substrates, the ambiguity/projection differential, world-of-one replay, and
adjudication on the substrate that authored the evidence all remain required.

The accepted owner decision permits `bevy_ecs` in `orrery_games`; the last
comment on #793 corrects the state of the tree: the dependency is permitted
but has not been taken. `orrery_core` remains Bevy-free. None of that changes
this finding—the proposed return type lives in `orrery_core` and needs no Bevy
type.

---

## 2. Verified production consumers of the trait method

### 2.1 What was searched

The compile blast radius is the trait method at
`crates/orrery_core/src/executor.rs:674`, not every call to the separate
inherent `Executor::state` at `executor.rs:143`. A workspace search was
cross-checked against every `B: TickBackend<_>` production body. There are two
production implementations, **nine external production calls**, and the
provided `section_state` body: ten consumers in total.

The two implementations are:

| Site | What it returns today |
|---|---|
| `crates/orrery_core/src/executor.rs:755-756` | A borrow from the executor's `BTreeMap`. |
| `crates/orrery_sim_host/src/ecs.rs:814-815` | A borrow from the selected ECS component, whose payload is still the whole sum. |

### 2.2 Every production call through `TickBackend`

| Site | Immediate operation | What the consumer actually needs |
|---|---|---|
| `crates/orrery_core/src/executor.rs:710` | `and_then(S::project)` | A typed `&R::CoreState` so the provided `section_state<S>` can return `&S::State`. Bytes cannot implement this default without decoding into a temporary, which recreates the lifetime problem. |
| `crates/orrery_core/src/replay.rs:170-171` | `CoreCodec::to_canonical` | Owned whole-state bytes for a confirmed replay's correction. This one is mechanically replaced by the returned buffer. |
| `crates/orrery_sim_host/src/lib.rs:318` | `CoreCodec::to_canonical` | Owned whole-state bytes for one C-ABI-friendly lookup. Mechanically replaced. |
| `crates/orrery_sim_host/src/lib.rs:408-410` | `state.to_canonical()`, then append | Whole-state bytes for stable-id-ordered output. Mechanically replaced; the per-state temporary buffer already exists today. |
| `crates/orrery_games/src/diff.rs:1018-1020` | `clone` | The current `InputLogProducer::anchor<S: CoreCodec>` API takes typed state only to hash it; the authority log then stores canonical bytes. A direct replacement decodes before `anchor`; an additional byte/hash producer API avoids that decode. |
| `crates/orrery_games/src/diff.rs:1034-1039` | `clone` | The current `cut_claim<S: CoreCodec>` API likewise hashes typed pre-step state, then the log stores bytes if a claim is cut. The state is fetched before `cut_claim` rejects non-claim ticks, so a bytes return allocates on every logged entity-tick unless this control flow also changes. |
| `crates/orrery_games/src/diff.rs:1069-1072` | `clone` | The same current typed API for the closing claim and snapshot; semantically hash + canonical bytes, not game-field inspection. |
| `crates/orrery_games/src/scenario.rs:468-471` | `clone` | Typed state retained in `TickRecord<G>::Entry`; later D-1/D-3, comparison, and divergence code use game projections over it. |
| `crates/orrery_games/src/scenario.rs:631-634` | `clone` | The same typed tick-record state on sealed replay. |
| `crates/orrery_games/src/scenario.rs:669-683` | `clone` | Typed current/previous states passed to `InvariantSample` and `evaluate`, then retained as the next previous sample. |

There is no direct `state_hash(state)` trait call in production. Three calls
encode, six clone immediately, and the provided method projects. Calling the
clone sites “clone consumers” hides two different costs. The three scenario
sites intrinsically retain typed ownership. The three differential sites use
typed ownership because `InputLogProducer` currently accepts `S: CoreCodec`;
making those byte-native is possible, but is an additional core API and
control-flow refactor rather than replacing a clone with a `Vec<u8>` clone.

The provided `section_state` default is an additional API break. Under a bytes
replacement it must either:

- decode an owned sum and fail to return `&S::State` for the same temporary
  lifetime reason;
- return an owned section, widening that method too; or
- become a required backend implementation. The executor can implement the
  last option from its inherent typed store; a per-section backend can return
  its component borrow. It is no longer the additive, correct-by-default seam
  D42 accepted.

### 2.3 The same-named inherent method is much wider

If “the only non-test consumers of `&CoreState`” was intended to include
`Executor::state`, the claim is simply false. These calls do not have to break
if only the trait changes—the inherent method can remain—but they show why a
workspace-wide bytes migration is a different and much larger proposal.

Every production inherent call, grouped without omitting sites:

| Site(s) | Typed use |
|---|---|
| `crates/orrery_sim/src/lib.rs:178` | Pattern-matches `RegolithState::Craft` and projects transform fields across the C ABI. |
| `clients/regolith/src/aoi.rs:192` | Matches all four variants to project lattice position. |
| `clients/regolith/src/combat.rs:1120, 1127` | Returns borrowed `&Craft` / `&Rock` projections. |
| `clients/regolith/src/main.rs:412` | Tests whether a joined peer is a replicated craft. |
| `clients/regolith/src/lib.rs:1267, 1348, 1352, 1605, 1632, 1765, 1774, 1907, 1986, 1992, 2465` | Reads craft/rock geometry and health, selects lockable targets, maintains render bodies/census, tests presence, and projects transforms. |
| `clients/regolith/src/grab.rs:86, 92` | Borrows own `Craft` and every `Pickup` to compute reach. |
| `clients/regolith/src/hud.rs:173` | Projects craft/rock radius for the HUD. |
| `clients/regolith/src/campaign.rs:592, 805, 1063, 1075, 1084, 1420, 1853` | Clones anchor state, cuts typed claims, performs typed collision selection, derives the committed cell, and encodes replication state. |
| `crates/orrery_witness/src/witness.rs:1279` | Encodes the just-replayed typed state into the bounded recent-snapshot map. |
| `crates/orrery_games/src/scenario.rs:748, 828` | Passes the typed computed state into game-specific divergence classification. These are concrete `Executor` paths, unlike the generic calls in §2.2. |
| `gates/p1-swarm/src/bot.rs:1031, 1186, 1195, 1265, 1352, 1718, 1733, 2224, 2382` | Borrows the authored craft, performs typed collision/AOI selection, traces/hash-checks typed state, moves materialized state, builds replication output, cuts claims, and returns a typed anchor state. |
| `crates/orrery_conformance/src/corpus.rs:402, 404` (consumed at `:473`) | Returns `&Body` and projects every final corpus field. |

This is why the least disruptive shape, if the trait really must move, is to
leave `Executor::state` intact. It also explains why `Cow<R::CoreState>` is a
more honest replacement candidate than bytes for the trait's real generic
consumers.

---

## 3. Allocation and CPU price

### 3.1 What “bytes” can mean

An owned result such as `Option<Vec<u8>>` solves the lifetime problem by
allocating and transferring ownership. A borrowed `Option<&[u8]>` does not:
encoding a component into a local buffer and returning its slice is the same
temporary-reference error as E0515.

A slice works only if the backend already owns canonical bytes. For a backend
whose source of truth is typed components, that means a canonical-byte cache
kept coherent after every insert and step. At 24,000 full crafts the cache
holds at least 3.792 MB of payload plus 576 kB of `Vec` headers on a 64-bit
target, before allocation slack and ECS row overhead. It also creates two
representations whose equality becomes a conviction invariant. Storing only
bytes avoids duplication but makes every step decode and re-encode the sum;
that is not the proposed component storage rung.

`Cow<'_, [u8]>` does not create a useful middle ground: neither current backend
stores canonical bytes, so both allocate unless they take on that cache.

### 3.2 Regolith sizes

The whole-state tag is one byte. From the codec in
`crates/orrery_games/src/regolith/state.rs`:

| State | Canonical bytes |
|---|---:|
| `Craft`, empty trail | 134 |
| `Craft`, full four-point trail | **158** |
| `Rock` | 85 |
| `Pickup` | 43 |
| `BloomDirector` | 57 |

The committed capacity workload is 100% full-trail `Craft`, so 158 is the
number that applies. These are logical lengths; `Vec` allocation capacity is
an implementation detail and can be larger.

A throwaway `size_of` probe on this target independently reproduced #804's
in-memory figures: `RegolithState` 168 bytes, `Craft` 168, `Rock` 96, `Pickup`
56, and `BloomDirector` 72. The probe was removed. Thus the all-Craft capacity
leg has literally no archetype-row payload saving to measure; the saving is
72 bytes for a rock, 112 for a pickup, and 96 for a director before ECS
component/layout overhead.

### 3.3 The 24k capacity rates

Committed §12.7 measurements, with per-entity arithmetic shown rather than a
new benchmark:

| Operation | Executor | ECS | Calls / owned state buffers | Payload |
|---|---:|---:|---:|---:|
| `step(1)` | 15.465 ms | 14.238 ms | **0 `state` calls** | **0 new bytes from this proposal** |
| Full `state_bytes` sweep | 2.664 ms (111 ns/entity) | 3.058 ms (127 ns/entity) | 24,000 | 3,792,000 bytes |
| `collect_output_bytes` | 2.644 ms (110 ns/entity) | 3.218 ms (134 ns/entity) | 24,000 per-state temporaries plus one output buffer | 3,792,000-byte payload; 4,080,000-byte framed output |

Both byte operations already call `to_canonical` and allocate one fresh
per-state buffer. Returning that buffer from `TickBackend` changes ownership,
not the minimum allocation count. An implementation that receives a returned
`Vec` and then calls `to_canonical` again would double the work and is simply
wrong.

The tick row needs separate accounting. `canonical_step` calls `state_hash`,
and `state_hash` calls `to_canonical`, so the all-Craft tick already performs
24,000 canonical-buffer allocations carrying 3.792 MB before this seam is
read. The proposal does not change that. Reading every state after every tick
would add another 24,000 allocations and 3.792 MB/tick—1.44 million and
227.52 MB/s at 60 Hz—but no committed tick path does that. Full output every
tick would already pay it under today's API.

For typed generic readers the new cost is real: one allocation plus one decode
per call. Regolith's current variants decode into inline state (including the
craft trail), so decode adds CPU but no nested heap allocation. `Ruleset` does
not require that property of other games; another `CoreState::decode` may
allocate internally.

### 3.4 Adjudication and witness rates

The maximum evidence window is 180 ticks
(`orrery_protocol/src/verifiable.rs:422`). For one full-craft subject:

- replay performs 180 existing hash encodes: 180 allocations and 28,440 bytes
  of logical input to BLAKE3;
- `ReplayHarness::canonical_state` reads once after a confirmed deviation and
  allocates one 158-byte result today; returning owned bytes keeps that one
  allocation rather than adding one;
- present neighbour frames are decoded to typed `R::CoreState` before
  `insert_observed`, independent of the read return type.

The live witness similarly remains typed. It decodes each present recorded
neighbour (`witness.rs:1198-1202`), installs it for one reader tick
(`:1243-1254`), steps, removes it (`:1273-1276`), and encodes the watched state
into `recent` once per judged tick (`:1279-1282`). The map retains 128 snapshots
per watched entity, which is 20,224 payload bytes for full crafts plus map and
buffer overhead. A trait-only change does not touch this path because it uses
the inherent `Executor::state`; if generalized later, bytes replace the
existing encode allocation.

The outlier is `orrery_games::diff`'s evidence authoring. It asks the generic
backend for state at the anchor, before every logged entity step, and at the
closing claim. At a full 180-tick window that is up to 182 state reads per
entity. A direct bytes replacement introduces up to 182 allocations carrying
28,756 bytes plus 182 decodes there. A new byte/hash producer API can remove
the decodes, but not the per-tick returned buffer while the read remains ahead
of `cut_claim`'s cadence check. Moving that check ahead of the read is another
refactor; claim cadence does not reduce the cost in the code as written.

---

## 4. Does it unblock a component holding `Craft`?

**Yes at the compiler boundary, but only there.** The rejected shape borrows a
temporary sum:

```rust,ignore
fn state(&self, id: PersistId) -> Option<&RegolithState> {
    let held: &Craft = self.craft_component(id)?;
    Some(&RegolithState::Craft(held.clone())) // E0515
}
```

Owned bytes make the temporary die only after its encoding has been copied
into the returned owner:

```rust,ignore
fn state_canonical(&self, id: PersistId) -> Option<Vec<u8>> {
    let held: &Craft = self.craft_component(id)?;
    let whole = RegolithState::Craft(held.clone());
    Some(whole.to_canonical())
}
```

There is no returned borrow into `whole`, so E0515 is gone. The important
constraint is the middle line: reconstruct the **whole** `RegolithState` and
use its existing `CoreCodec`. Encoding `Craft` alone would omit the sum tag and
violate D42 (b)(2)(2); copying its encoder into the backend would create a
second canonical producer.

The remaining operations are ownership-shaped and can also be expressed:
`insert_observed` matches an owned sum into the correct component;
`take_state` removes a component and embeds it into an owned sum. But
`step_entity` still has to remove/clone the component, embed the sum, call
`canonical_step(&mut sum, ...)`, then project the result back into the
component. Bytes do not remove that round-trip. Nor does the current `Section`
trait provide the missing owned `embed`; that API/design remains additional
work.

So bytes are **sufficient to clear the read lifetime error, not sufficient to
make per-section storage cheap or native**.

---

## 5. Witnessing, adjudication, and conviction

### 5.1 Typed state does not disappear

The witness types named in #804's handoff are verified and remain typed for
reasons unrelated to the backend read:

- `Witness::samples` stores `(R::CoreState, Tick)`
  (`crates/orrery_witness/src/witness.rs:438`) because `observe`
  (`:674-695`) passes current and previous typed states to `InvariantSample`.
- `Watch<R::CoreState>` carries a typed anchor (`:456-465`); `watch`
  (`:611-624`) hashes and encodes it, then moves it into an executor.
- `RecordedNeighbor<R::CoreState>` is a typed present-or-absent observation
  (`:362-363`). `replay_entity` decodes canonical frame payloads at
  `:1198-1202` because the rules need typed neighbours during the step.
- `Watched::recent` is already canonical `Vec<u8>` (`:399-402`), populated
  after a replayed tick at `:1279-1282`.
- `try_reanchor` reads a typed stage-1 sample, verifies its `state_hash`, clones
  it into a fresh executor, and returns canonical bytes (`:1624-1635`).

Changing only `TickBackend::state` does not require converting `samples`,
`Watch`, `Observation`, or `RecordedNeighbor` to bytes. Doing so would merely
move their existing `CoreCodec::decode` calls later and make invariant/rule
execution pay for them repeatedly.

### 5.2 The adjudicator is already byte-shaped at its edges

The conviction flow is:

1. `ReplayHarness::load_claimed_snapshot` hashes the supplied bytes, compares
   them with the signed claim, then decodes `R::CoreState`
   (`crates/orrery_core/src/replay.rs:145-155`).
2. Replay decodes each present neighbour frame (`replay.rs:263-280`), installs
   typed state for the declared observation tick, steps one entity, and checks
   the recorded read sequence exactly (`:302-338`).
3. A confirmed verdict asks `ReplayHarness::canonical_state` for corrected
   bytes (`crates/orrery_persistd/src/adjudication.rs:551-582`).
4. Persistd copies those bytes into `AuthorityCorrectionClaimsV1`
   (`crates/orrery_persistd/src/gateway.rs:8399-8409`) and signs the claim.
5. The protocol deliberately treats `authoritative_state` as opaque canonical
   whole-state bytes (`crates/orrery_protocol/src/authority.rs:12-35`). The
   client reconciliation adapter receives those same bytes.

The wire and persistd structures therefore do not break under a bytes return.
They already want `Vec<u8>`. The replay substrate still needs typed states for
steps and neighbour reads.

### 5.3 What can break conviction

Today `ReplayHarness::canonical_state` borrows typed state from the backend and
invokes the same whole-state `CoreCodec` every other role uses. A bytes seam
moves the correction's byte production behind `TickBackend`; a faulty backend
can now return bytes different from the state that produced
`TickOutcome::state_hash`.

That is not a false verdict—the per-tick hashes still come from
`orrery_core::canonical_step`—but it can produce a **bad signed correction
after a correct conviction**. `AuthorityCorrectionV1::sign` authenticates the
bytes supplied to it; it does not prove that they are the replay's final state.

Adoption therefore needs all of these properties to stay explicit:

- the returned value is the existing whole-state `CoreCodec` encoding, never a
  section encoding;
- executor and ECS output bytes remain equal under F-4 and the Tier-H
  projection/adjudication battery;
- a confirmed replay's returned bytes hash to the last replayed
  `TickOutcome::state_hash` before persistd signs a correction;
- strict `CoreCodec::decode` round-trips the returned bytes to the same whole
  state for every section variant;
- no golden or protocol version changes, because the encoding did not change.

The third check is defence in depth that the current typed path gets by
construction. If a bytes-returning backend is admitted, making it explicit is
cheaper than letting a storage adapter become the one place conviction and
correction can disagree.

---

## 6. C ABI consequence

`orrery_sim` is an `rlib + cdylib` (`crates/orrery_sim/Cargo.toml:8-9`) and its
public boundary already carries flat bytes and `#[repr(C)]` records, never a
Rust `&CoreState`.

Its inbound replication path decodes a keyframe/delta into canonical bytes,
then decodes `RegolithState` and calls `Executor::insert_observed`
(`crates/orrery_sim/src/lib.rs:131-170`). Its transform export iterates the
concrete executor, pattern-matches `RegolithState::Craft`, and copies six scalar
fields into `OrrerySimCraftTransform` (`:175-192`).

Therefore:

- a **trait-only** bytes change has no C-ABI or compile impact: `orrery_sim`
  does not use `TickBackend::state`, and the inherent `Executor::state` can
  remain typed;
- no ABI symbol, layout, ownership rule, or replication packet changes;
- the inbound decode remains necessary because canonical stepping and typed
  transform projection still require `RegolithState`;
- if `Executor::state` were also replaced, transform export would need to
  allocate/decode once per entity or move to `section_state<CraftSection>`.
  That would turn a zero-allocation borrowed projection into per-call work and
  is the sharp constraint #804 identified.

A bytes seam is therefore compatible with the C ABI precisely when it stays a
backend trait change rather than becoming a tree-wide ban on typed state
access. It does not make the C ABI itself more byte-oriented; it already is.

---

## 7. Recommendation and owed work

### Do now

Do not migrate the trait in this lane. Keep the accepted whole-state encoding,
the witness/adjudication types, and the C ABI unchanged. Treat #804's E0515 as
a proven limit on the current storage rung, not as evidence that the next API
must be bytes.

### If the owner elects to proceed

Before implementation:

1. Write a new ADR explicitly superseding D42 (b)(2)(1). State again that
   (b)(2)(2)—whole-state canonical encoding—is unchanged.
2. Decide whether scenario/differential tooling decodes bytes, gains a separate
   typed capability, or receives `Cow<R::CoreState>`. Do not call those sites
   mechanical clones.
3. Redesign the provided `section_state` default; it cannot return a borrow from
   decoded temporary bytes.
4. Keep the existing `CoreCodec` as the one encoder by rebuilding the sum before
   encoding. If direct section encoding is desired, name the additional D42 (a)
   and (b)(2)(2) supersession instead of smuggling it in as an optimization.
5. Add the correction-bytes/last-state-hash equality guard and keep the entire
   Tier-H battery and F-4 green.
6. Measure a rock/pickup-heavy workload. The committed capacity leg is all
   `Craft`; narrow craft storage saves nothing against the 168-byte sum row, so
   it cannot demonstrate the storage benefit being bought.
7. Price the still-required `canonical_step` narrow → sum → narrow round-trip.
   A bytes read seam does not answer it.

### Final recommendation

**Reject the replacement as the next rung.** Owned bytes are technically valid
and the direct byte consumers are cheap to migrate, but the typed consumers,
accepted ADR text, singular canonical-encoding trust boundary, unchanged step
round-trip, and null Craft storage saving make this the wrong trade now. If a
future rock/pickup-heavy measurement makes per-section storage valuable,
`Cow<R::CoreState>` is the smaller first proposal to compare against a
bytes-only trait—not an implementation detail to skip past.
