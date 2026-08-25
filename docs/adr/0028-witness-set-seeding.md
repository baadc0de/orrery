# ADR-0028: Witness-set seeding, the announcement envelope, and the `epoch/` record

**Status:** Accepted · **Date:** 2026-08-20 · **Decision:** D28

This decision is normative once accepted. See the [ADR index](../DECISIONS.md)
for precedence, scope, and the complete decision set.

**Supersedes:** nothing. It **implements** [D10](0010-witnessing.md) item 4's
"seeded by the coordinator per cell-epoch … never self-chosen" clause, which
has no mechanism anywhere in the tree, and **narrows two sentences** of
[docs/07 §4.1](../07-witnessing.md) and one of
[docs/09 §6](../09-services-and-ops.md) that describe the coordinator writing
the epoch record *through* the gateway — under clause (a) it does not, because
[D12](0012-backend-services.md)'s inventory has no coordinator→gateway edge and
[D24](0024-island-drain.md) clause (a) reaffirmed that it gains none. Neither
expansion file is edited here; the divergences are listed in Consequences.
It adds five parameters to [D16](0016-parameter-reference.md)'s table and one
key family to the [docs/08 §6](../08-persistence.md) keyspace.

**Sibling records.** [#142] owns the attestation envelope, witness role
separation, and the required-K derivation, including how the deriving party
obtains a secret seed. This record produces and commits the epoch secret; it
does not decide who may hold it. [#144] owns the sub-N fallback. [#147] owns
gateway enforcement of K-of-N. Where the two sets of decisions are genuinely
entangled it is said so in Open questions rather than guessed at.

## Context

Six facts about the landed tree, each read before it was written down.

**The only witness set in the workspace is self-chosen, and its own doc comment
says that is a bug waiting for this record.** `WitnessSet`
(`crates/orrery_witness/src/plugin.rs:305-307`) is a Bevy resource holding
`members: Vec<NodeId>`, left empty in practice; `witness_links`
(`:599-607`) falls back to the island roster sorted by `NodeId` and truncated
to `MAX_WITNESS_LINKS` (7, `:150`). The comment above the struct
(`:293-303`) states the danger and the deadline:

> Not this peer. D10 requires the witness set to be seeded per cell-epoch by
> the coordinator and **never self-chosen** … That fallback is deterministic
> and bandwidth-correct, and it is **self-chosen — which is only tolerable
> because P4 files nothing**. … the moment reports carry consequences, this
> must come from the coordinator.

**The coordinator says the same thing about itself.**
`crates/orrery_coordinator/src/lib.rs:1-13`: "witness-seed epochs and island
generation counters are durably journaled to FDB behind the `fdb-state`
feature (not yet implemented in P1)."

**There is no witness message on any wire.** `CoordMsg`
(`crates/orrery_protocol/src/coord.rs:355-402`) has exactly `Hello`,
`Welcome`, `Presence`, `IslandAssignment`, `InterestGrant`, `Drain`.
`GatewayMsg` (`crates/orrery_protocol/src/gateway.rs:33-76`) has `Hello`,
`Lease`, `InterestGrant`, `Diff`, `Subscribe`, `SubmitIntent`,
`VersionedHello`. The intent path carries `cell_epoch` and does not look at
it: "`cell_epoch` is carried, not checked: nothing here knows which witness
set it names" (`crates/orrery_persistd/src/intent/mod.rs:263-265`).

**There is no `epoch/` key builder.** `crates/orrery_persistd/src/keyspace.rs`
defines one-byte discriminators `w` world (`:31`), `c` ckpt (`:151`), `a`
actor (`:168`), `l` lease (`:212`) and ledger (`:588`, `:601`, `:618`,
second-byte discriminated), `m` lease-cell (`:221`), `o` lease-location
(`:235`), `s` seedmap (`:285`), `p` seedprog (`:317`), `k` chunk (`:396`),
`v` content/version (`:459`), `i` intent (`:481`), `u` player (`:525`), `n`
pid (`:565`). `e` and `f` are free.

**The courier model is the repository's one answer for a coordinator-asserted
fact a gateway must trust**, and it is stated twice in the tree.
`crates/orrery_coordinator/src/interest.rs:8-13`:

> Delivery is deliberately not the coordinator's problem. It signs a grant,
> hands it to the peer, and the peer presents it to whichever gateway it is
> talking to … That is why there is no coordinator→gateway connection anywhere
> in this crate: adding gateways does not add coordinator fan-out, and a
> gateway needs only the coordinator's *public* key to check the claim.

and `crates/orrery_persistd/src/gateway.rs:715-719`, on
`CoordinatorHandoutAuthority`: "It holds only the coordinator's **public**
keys: it verifies handouts, it never mints them. … the gateway needs no
connection to the coordinator — the peer is the courier, exactly as it is for
its identity token."

**The coordinator already learns account↔NodeId and throws it away.**
`CoordMsg::Hello` carries an identity session token; the server checks that the
claimed node equals the transport identity and verifies the token — and then
discards the claims: `Ok(_claims) => {}`
(`crates/orrery_coordinator/src/server.rs:521-535`). Those claims are
`SessionTokenClaimsV1 { version, account, node, issued_at_ms, ttl_ms, standing,
issuer_key_id }` (`crates/orrery_protocol/src/identity.rs:73-88`) with
`standing ∈ {Good, Quarantined}` (`:63-69`). So two of `docs/07 §4.1`'s four
eligibility filters are already inside a signature the coordinator already
verifies, and are unavailable only because a binding is dropped on the floor.
`orrery_identity` itself does not exist — `crates/` holds fourteen crates and
none of them is it — so the `id/` and `strike/` rows
(`docs/08-persistence.md:3234-3235`) have no writer.

### The two facts that constrain the wire

`CellEpoch` is a `u64` newtype, "chosen peer-side", explicitly not the shard
`Epoch`, and the two "shared a type once, and the intent fence silently
compared them" (`crates/orrery_protocol/src/persist.rs:79-99`). The intent
fence still records the separation (`crates/orrery_persistd/src/intent/fdb.rs:
248-250`). Meanwhile `Intent` is `{ intent_id, issuer, cell_epoch, ops,
attestations, signature }` (`persist.rs:284-297`) — **no cell field** — and
`GatewayMsg::SubmitIntent { intent }` (`gateway.rs:72-76`) adds none. So the
gateway holding an intent knows an epoch number and nothing about *which
cell's* epoch it is.

And it cannot be packed. `CellId` is a `NonZeroU64` whose `to_bits` is the
whole word (`crates/orrery_protocol/src/cell.rs:158`, `:172`), 63 Morton bits
at level 21 plus the level marker. There are no spare bits for an epoch
counter, and truncating a cell id to make room is [D22](0022-grid-id-in-the-storage-key.md)'s
C-8 bug class re-introduced deliberately.

## Decision

### (a) The coordinator seeds; the peer couriers; the gateway verifies and writes

> **A witness set is chosen only by the coordinator, published only as a
> coordinator-signed `WitnessEpochV1` envelope, delivered only by the peers it
> names or covers, and made durable only by the gateway that accepted it — no
> coordinator→gateway edge exists, and D12's service inventory gains no edge
> from this record.**

That is the sentence a reviewer should point at when asked whether D24 clause
(a) survives P5. It does, in the same shape and for the same three reasons.
The announcement follows `InterestGrantV1` step for step:

| Stage | Interest grant (exists) | Witness epoch (this record) |
|---|---|---|
| mint | `InterestIssuer::sign` (`interest.rs:50-52`) | `WitnessEpochIssuer::sign` |
| coordinator→peer | `CoordMsg::InterestGrant { grant }` (`coord.rs:385-394`) | `CoordMsg::WitnessEpoch { announcement }` |
| peer→gateway | `GatewayMsg::InterestGrant { grant }` (`gateway.rs:47-57`) | `GatewayMsg::WitnessEpoch { announcement }` |
| verify | `verify_interest_grant` (`coord.rs:302-331`) | `verify_witness_epoch` |
| keys held | `CoordinatorHandoutAuthority::new(keys)`, rotation overlap (`gateway.rs:726-737`) | the same key set, the same `IssuerKeyId` selection |

**Why the record does not take the edge `docs/09 §6` implies.**
`docs/09-services-and-ops.md:132` reads "The coordinator writes each cell-epoch
seed through the gateway to FDB … at issuance", which describes a coordinator
holding a gateway session. Under D24 the reasons against it are unchanged and
one is new:

- **It is an edge D12's five-service inventory does not have**, and the
  argument that added it here would add it everywhere — `docs/09:15` already
  lists "no new … witness epochs" as the coordinator's blast radius, and an
  epoch that cannot be *recorded* while a gateway is unreachable is a second
  failure mode for the same fact.
- **It scales the wrong way.** One coordinator would hold a session to every
  gateway in every region, and write one row per cell per 30 s whether or not
  any peer in that cell ever submits an intent. The courier writes exactly the
  rows that are load-bearing, at exactly the moment they become load-bearing.
- **It buys nothing the courier does not.** The gateway must verify the
  coordinator's signature either way, because the row is only as trustworthy
  as the envelope inside it (clause (d)). A direct write would let the gateway
  skip verification — which is the property the record must not have, since
  then a compromised gateway could seed its own sets and D10's "never
  self-chosen" would be enforced against peers and not against the cluster.

**What the courier costs, stated plainly.** A cell whose peers never present
an announcement never gets a durable row for that epoch. That is correct
rather than a gap: the row exists to make attestations checkable after the
fact, and an epoch under which nothing was attested has nothing to check. The
reveal chain in clause (c) is what stops a coordinator from exploiting the
gap by simply never revealing.

### (b) `CellEpoch` becomes a handle; the cell arrives signed, never asserted

> **`Intent::cell_epoch` is an opaque `u64` *epoch handle*, unique across the
> coordinator's issuance history and resolved by the gateway against a
> presented announcement; the cell is never a field of the intent, because a
> peer-supplied cell would be self-declared — the exact failure the interest
> grant exists to prevent.**

The handle is not the per-cell counter. Both exist, and the announcement
carries both:

```
epoch   : u32   per (grid, cell), monotone      — docs/07:156's EpochId.epoch
handle  : u64   = (incarnation << 48) | counter — what Intent::cell_epoch names
```

`incarnation` is the coordinator leader-lease generation from `coord/leader`
(`docs/08-persistence.md:3237`), so a failover cannot mint a colliding handle
without also winning the lease; `counter` is monotone within one incarnation.
The sizing is not tight: a universe with 100 000 simultaneously populated cells
reseeding at the 10 s floor issues 10⁴ handles/s, and `2⁴⁸ / 10⁴ ≈ 8.9 × 10⁸ s`
— about 28 years inside one incarnation.

Three properties follow, and they are the reason for the split:

1. **One `u64` field on the existing wire.** `Intent`, `SubmitIntent`, and
   `Intent::signing_preimage` (`persist.rs:316-329`, which mixes
   `cell_epoch.0.to_le_bytes()`) are unchanged. Postcard keys fields
   positionally, so adding a cell to `Intent` would break every deployed
   decoder to carry a value the gateway must not trust anyway.
2. **No ambiguity.** A per-cell counter is not unique — epoch 4 exists for
   every cell at once — so a handle that *was* the per-cell counter could not
   resolve to one announcement. The incarnation-scoped counter can.
3. **`EpochId { cell, epoch }` survives verbatim.** `docs/07:156`'s type is
   the durable key and the human-facing identity; the handle is the wire
   pointer to it. Nothing is conflated, and the `intent/fdb.rs:248-250`
   history — two epoch namespaces once sharing a type — is not repeated: this
   record adds a third quantity with its own name rather than overloading
   either of the two.

### (c) Selection: a secret-keyed shuffle over an announced pool, with a chained reveal

> **The coordinator draws the set by a Fisher–Yates shuffle of the announced
> candidate pool under a per-epoch secret key, publishes a blake3 commitment
> to that key in the announcement, and reveals the key inside the *next*
> announcement for the same cell — so the reveal is carried by the same
> courier as the announcement and cannot be silently withheld.**

The derivation, in full. Let `K_master` be a coordinator-provisioned secret
(`docs/09:178`'s key-hygiene class), `g` the grid, `c` the cell id bits, `e`
the per-cell epoch counter, `P` the eligible candidate pool.

```
k_e     = HKDF-SHA256(K_master, "orrery/witness-epoch-key/v1" ‖ g ‖ c ‖ e)   [32 B]
commit  = blake3("orrery/witness-epoch-commit/v1" ‖ g ‖ c ‖ e ‖ k_e)         [32 B]
seed    = HMAC-SHA256(k_e, "orrery/witness-epoch-seed/v1" ‖ g ‖ c ‖ e)       [32 B]

candidates = sort_by_bytes(P)                       // total order, no ties
rng        = ChaCha20(seed)                          // rand_chacha, D14
selected   = fisher_yates(candidates, rng)[0 .. min(N_target, |candidates|)]
```

with `N_target = 7`, `N_floor = 5`; a pool below `N_floor` after party
exclusion is [#144]'s path, not this record's.

**Why `k_e` is derived from a master key rather than drawn fresh.** A fresh
random `k_e` lives only in the leader's memory, so a failover between issuance
and reveal loses it permanently and every epoch it covered becomes
unauditable — the one window an attacker would choose. Deriving it makes the
warm standby (`docs/09:150`) able to reveal an epoch it did not issue, from
the provisioned secret alone. HKDF is one-way, so revealing `k_e` says nothing
about `K_master` or about `k_{e+1}`; the draws remain independent across
epochs, which is what clause (f)'s anti-grind argument rests on.

#### Unpredictability and verifiability, reconciled

They pull in opposite directions only if one asks a single mechanism to do
both. This record uses **two verification tiers over the same envelope**, and
the quantity kept secret is not an input to the tier that must run live:

```
in-epoch  (authenticity)  w ∈ A.selected  ∧  Ed25519_verify(coord_pk, A)
                          — no secret required, offline-checkable, enforcing

post-epoch (fairness)     blake3(… ‖ k_e) = A.commit
                       ∧  A.selected = fisher_yates(A.candidates, ChaCha20(seed(k_e)))
                          — requires k_e, available one epoch later, auditing
```

The in-epoch tier answers "was this witness in the announced set", which is
what a gateway enforcing K-of-N (#147) and a party checking an attestation
need. It needs the coordinator's *public* key and the envelope, and nothing
live. The post-epoch tier answers "was the announced set actually drawn rather
than hand-picked", which is what makes the coordinator itself accountable, and
it is the only question the secret blocks.

**Publishing `selected` inside the epoch therefore costs nothing**, because
the unpredictability that matters is not "who is witnessing right now" — the
witnesses must know that themselves, and D9 already streams input logs to them
(`docs/adr/0009-verifiable-core.md:29`). What must stay unpredictable is:

- **the next epoch's draw**, so an adversary cannot pre-position colluders.
  Guaranteed by `k_{e+1}` being independent of `k_e` under HKDF: an attacker
  holding every revealed key to date has no advantage on the next one over
  guessing.
- **the per-intent required-K subset**, so attestation shopping is impossible
  (`docs/07:180`). That derivation is [#142]'s, and it is where the two records
  are entangled — see Open questions.

An adversary's *only* lever is therefore the pool, and the pool is bounded by
clause (e)'s eligibility and by physically being in the cell.

**The chained reveal.** `A_{e+1}.prev_seed_key` carries `k_e`, and a verifier
rejects `A_{e+1}` if `blake3(… ‖ A_{e+1}.prev_seed_key) ≠ A_e.commit` for the
`A_e` it holds. This is the clause that makes the reveal non-optional: the
coordinator cannot issue a usable epoch `e+1` for a cell without opening `e`,
so withholding a reveal costs the coordinator the cell rather than costing the
auditor the proof. The worst-case reveal latency is one epoch length plus one
reseed floor (30 s + 10 s), which is the delay a VRF would have removed — see
Alternatives.

### (d) The announcement envelope

> **`WitnessEpochV1` is a postcard envelope of Ed25519-signed claims whose
> preimage is `DOMAIN ‖ postcard(claims)`, binding one grid, one cell, one
> epoch counter and one handle; a recipient verifies it against the
> coordinator public keys it is configured with, and needs no connection to
> the issuer.**

```rust
pub const WITNESS_EPOCH_V1_DOMAIN:  &[u8] = b"orrery/witness-epoch/v1";
pub const WITNESS_EPOCH_V1_VERSION: u8    = 1;
pub const MAX_WITNESS_EPOCH_BYTES:  usize = 2048;
pub const MAX_EPOCH_CANDIDATES:     usize = 32;   // D6's interest-mesh ceiling

struct WitnessEpochClaimsV1 {
    version:          u8,
    grid:             GridId,
    cell:             CellId,
    epoch:            u32,             // monotone per (grid, cell)
    handle:           u64,             // (incarnation << 48) | counter — clause (b)
    epoch_ms:         u64,             // a DURATION, never a deadline
    accept_grace_ms:  u64,             // a DURATION, never a deadline
    candidates:       Vec<NodeId>,     // ascending byte order, ≤ MAX_EPOCH_CANDIDATES
    selected:         Vec<NodeId>,     // draw order, ≤ N_target, ⊆ candidates
    seed_commitment:  [u8; 32],
    prev_seed_key:    Option<[u8; 32]>,// opens epoch-1's commitment; None only at epoch 0
    issuer_key_id:    IssuerKeyId,
}
struct WitnessEpochV1 { claims: WitnessEpochClaimsV1, signature: Signature }
```

The preimage is the one at `coord.rs:333-339`, with its own domain tag:
`WITNESS_EPOCH_V1_DOMAIN ‖ postcard(claims)`. One canonical function, used by
signer and verifier alike, for the reason that file gives.

**Durations, never deadlines.** `epoch_ms` and `accept_grace_ms` follow
`InterestGrantClaimsV1::ttl_ms` verbatim, and for the reason spelled out at
`coord.rs:161-170`: "The coordinator and the gateway are separate processes
with unrelated monotonic origins, so a coordinator-stamped instant is not a
quantity a gateway can compare against its own clock." The verifier stamps its
own `first_seen_ms` on acceptance. `docs/07:156`'s "epoch boundaries are
tick-aligned" is a peer-side statement about *when the coordinator rolls the
counter*; it is not a quantity the gateway can evaluate, and clause (f) does
not use it.

**Verification, in order** — the shape of `verify_interest_grant`
(`coord.rs:302-331`), with an error enum shaped after
`InterestGrantVerificationError` (`coord.rs:257-276`):

```
1. len ≤ MAX_WITNESS_EPOCH_BYTES, postcard-decodes with no remainder,
   claims.version == 1                                        → Malformed
2. some configured IssuerKey has claims.issuer_key_id         → UnknownIssuer
3. Ed25519 verify over DOMAIN ‖ postcard(claims)              → BadSignature
4. 0 < |candidates| ≤ MAX_EPOCH_CANDIDATES, strictly ascending,
   selected ⊆ candidates, |selected| ≤ N_target, no repeats   → BadPool
5. epoch_ms and accept_grace_ms in (0, MAX] each              → OverTtl
6. presenter's interest covers (grid, cell) right now         → NotCovered
7. no announcement on file for (grid, cell) with a higher
   epoch **and** none on file for this handle with different
   claims                                                     → Superseded
8. prev_seed_key opens the stored commitment for epoch-1,
   when one is held                                           → BadReveal
```

Step 6 replaces the grant's `WrongPeer` check (`coord.rs:319-321`). An
announcement names a cell, not a peer, so "the grant must name the presenter"
has no analogue — but an unrestricted presenter would let any authenticated
peer stuff a gateway's cache with epochs for cells it has nothing to do with.
The predicate is the one already on the gateway:
`InterestAuthority::allows(peer, grid, cell, now_ms)`
(`gateway.rs:604-613`) — the same test a live `Claim` and a successor
nomination pass, which is D25 rule 3's seam reused rather than a second
eligibility notion.

Step 7 is `InterestGrantClaimsV1::epoch`'s monotonicity rule
(`coord.rs:177-179`, `:272-273`) with one difference that matters: a *lower*
epoch is not discarded, it is merely refused as a **replacement**. Intents
still in flight under epoch `e` must resolve after `e+1` is announced (clause
(f)), so the cache is keyed by handle and holds a bounded window, not a single
current value.

### (e) Eligibility: what P5 can actually check

> **P5 enforces exactly the filters that are inside a signature the
> coordinator already verifies or an observation it already makes, and every
> filter it cannot evaluate is recorded here as skipped rather than
> implemented as a no-op that reads like enforcement.**

The immediate, zero-new-service change is to stop discarding the session token
claims at `server.rs:530` (`Ok(_claims) => {}`) and retain
`(account, standing)` alongside the peer session. Everything in the "enforced"
column below follows from that one binding.

| `docs/07:157-158` filter | P5 status | What it rests on / what it misses |
|---|---|---|
| account in good standing (no active quarantine) | **enforced** | `SessionTokenClaimsV1.standing == Good` (`identity.rs:63-69`, `:84-85`), signed by identity, already verified at `server.rs:529` |
| strike score under the witness-eligibility threshold | **approximated** | `standing` is the only signed reputation bit on the wire, so the coarse `Quarantined` flag stands in for the continuous score. Misses every account whose score is nonzero but below identity's own quarantine threshold |
| account age past probation (7 days) | **enforced** *(was skipped; see erratum below)* | `SessionTokenClaimsV1.on_probation`, signed by identity, evaluated at mint from `AccountRow::created_ms` against D33 clause (d)'s configured window. As fresh as the token and no fresher: an account that crosses its window mid-session stays excluded until its next refresh |
| present in the island ≥ 10 s | **enforced** | The coordinator times its own peer sessions (`server.rs:536-550` records a session at `Hello`); no new observation |
| one witness slot per account | **enforced, within one coordinator** | Dedup on the retained `claims.account`. Misses a Sybil whose NodeIds are split across coordinator incarnations or regions — there is no cross-coordinator account view |
| party exclusion on accounts **and every NodeId bound to them** | **approximated** | Exclusion covers the party's own NodeId and any other NodeId with a live session on this coordinator carrying the same signed `account`. Misses NodeIds bound to the account that are not currently connected, and misses collusion across *different* paid accounts entirely — which is what `id/{account_id}` (`docs/08:3234`) would answer and nothing writes it |

**So `docs/07:194-196`'s collusion analysis does not hold in full under P5, and
this record declines to let that be assumed.** Its clause (b) — "each colluding
account costs real acquisition and probation time" — has the acquisition half
(the token is signed by identity, so accounts are real) and not the probation
half. Its per-account dedup holds only against the coordinator that seeded the
epoch. The multiplicative argument survives with the *placement* and *exposure*
terms intact and the *identity* term reduced to "paid for, possibly minutes
old".

> *Erratum (2026-08-23, issue #214):* the probation half is now enforced, so the
> paragraph above is superseded for that row only. D31 gave `da` a `created_ms`,
> D33 clause (d) made the window deployment configuration with a 7-day default,
> and identity now stamps the verdict — not the age — into a new signed
> `on_probation` claim, which the coordinator's `eligible_pool` filters on. The
> *identity* term of `docs/07:196` reads as written again: a colluding account
> costs acquisition **and** probation time. Two of that paragraph's three
> reservations still stand, unamended: the strike-score row remains
> *approximated* (D33 clause (f) declines to widen the token for it), and
> per-account dedup and party exclusion remain scoped to one coordinator.
>
> The field is a boolean verdict rather than `account_age_bucket` as this clause
> suggested. The window is a deployment dial; sending an age would put a second
> copy of that dial in every coordinator, where it could disagree with
> identity's. The cost of sending the verdict instead is that the token answers
> exactly one question about account age and a future filter wanting a different
> granularity needs another claims version.

### (f) The durable `epoch/` record, and the read path that does not pay for it

> **The gateway writes `epoch/{grid}/{cell}/{epoch}` — holding the announcement
> envelope verbatim — inside the same FoundationDB transaction as the first
> intent that resolves against it, and no intent ever reads it on the hot
> path.**

Two families, both new, both prefix-disjoint from the fourteen in
`keyspace.rs`:

```
epoch/{grid}/{cell}/{epoch}     'e' ‖ grid:4 ‖ cell:8 ‖ epoch:4     = 17 bytes
epoch-handle/{handle}           'f' ‖ handle:8                      =  9 bytes  → the key above
```

all big-endian, so a cell's epochs sort in order and a grid's subtree is one
contiguous range — the `world_key`/`ckpt_key` convention (`keyspace.rs:29-36`,
`:149-155`). The `GridId` discriminator is [D22](0022-grid-id-in-the-storage-key.md)'s
rule: `docs/08:3236` writes the family as `epoch/{cell_id}` with no grid, which
is C-8 in the one family that escaped the sweep — two grids' identically
numbered cells would share a row. **This record fixes that in passing and says
so, because a witness set silently shared between nested grids is a witness set
chosen by neither cell's population.**

The second family is the handle index, and it exists for the reason
`lease_cell_key` and `lease_location_key` both exist (`keyspace.rs:217-238`):
two read patterns over one fact. An auditor and the adjudication executor scan
by cell; the intent path resolves by handle. Neither should scan for the other.

Value:

```rust
struct EpochRow {
    announcement:   Vec<u8>,        // the verbatim signed WitnessEpochV1 envelope
    first_seen_ms:  u64,            // the accepting gateway's local stamp
    revealed_key:   Option<[u8; 32]>,  // filled when A_{e+1} arrives (clause (c))
    gc_deadline_ms: u64,            // carried, not re-derived — IntentRow's shape
}
```

**The envelope is stored verbatim, not decomposed.** That is the whole
security value of the row: a reader recomputes the coordinator signature from
the bytes and needs to trust neither the gateway that wrote it nor FDB. A
decomposed row would be the gateway's *assertion* about an announcement, which
is exactly the trust inversion clause (a) refuses. `gc_deadline_ms` copies
`IntentRow` (`keyspace.rs:503-510`) — "the deadline is carried on the row, not
re-derived, so the sweep is a pure deadline comparison" — and is swept by the
same checkpoint pass.

**Why this adds no FDB round trip to the intent p99** (< 10 ms,
[D16](0016-parameter-reference.md)):

1. Steady state is a **memory hit**. A verified announcement lives in the
   gateway's per-cell epoch cache — the same structure and lifetime as
   `CoordinatorHandoutAuthority`'s `snapshots: RwLock<HashMap<..>>`
   (`gateway.rs:721-724`), pruned on the same 1 s sweep that already calls
   `prune_expired` (`:751-755`). Resolution is one map lookup on `handle`.
2. The **write** is one extra key in a transaction the intent already runs.
   `require_intent_fence` (`intent/fdb.rs:251-255`) already reads the
   ownership rows inside that transaction, and the intent already writes its
   idempotency row and its effects. One 17-byte key and one 9-byte index key,
   once per `(cell, epoch)` — i.e. at most once per 10 s per cell, amortized
   over every intent in that epoch.
3. The **read** happens only off the hot path: the adjudication executor
   replaying a disputed window, an auditor recomputing a shuffle after the
   reveal, and a gateway that restarted mid-epoch with no cache. The third
   case is also served by the peer simply re-presenting the announcement,
   which every peer in the cell holds.

### (g) Epoch turnover: an intent is judged against the epoch it names

> **Validity is judged against the *announced* set of the epoch the intent's
> handle names, never against current presence; an announcement remains usable
> for `epoch_ms + accept_grace_ms` after the accepting gateway first saw it,
> and an intent arriving after that is rejected stale rather than re-judged
> under a newer set.**

```
resolve(h)         = the accepted announcement A with A.handle = h
usable(A, t)      ⟺ A.first_seen_ms ≤ t < A.first_seen_ms + A.epoch_ms + A.accept_grace_ms
admissible(I, t)  ⟺ ∃A = resolve(I.cell_epoch)
                     ∧ usable(A, t)
                     ∧ witnesses(I) ⊆ A.selected
                     ∧ required-K holds                    [#142 / #147]
```

Three consequences, each answering one of the questions turnover raises:

- **In-flight attestations survive the boundary.** Nothing about the arrival of
  `A_{e+1}` invalidates `A_e`: `usable` is a function of `A_e`'s own window.
  This is `docs/07:232`'s rule — "validity is judged against the epoch's
  *announced* set, not current presence" — made checkable, and it is what makes
  Donnybrook-rate churn (68 %/s membership turnover) survivable at all. A
  witness that left the cell one second after signing is still a valid signer.
- **A late arrival is judged, then refused, in that order.** Past the grace,
  the answer is a distinct `EpochStale` rejection and not a signature failure,
  because the two are operationally different: the first says re-collect under
  the current epoch (or take [#144]'s provisional path), the second says
  somebody forged something. Conflating them would put honest netsplit
  survivors in the same bucket as attackers.

  > *Erratum (2026-08-22,
  > [ADR-0037](0037-unavailable-witness-epoch.md)):* the parenthetical
  > provisional alternative contradicts D29 clause 2, accepted in the same
  > commit as this record. D37 proposes refusal and re-collection under the
  > current epoch as the only stale cure. This annotation is not normative
  > unless D37 is accepted.
- **The grace *is* the netsplit posture.** `docs/07:233` promises "a grace
  window (one epoch length) of stale-epoch attestations" on reconnect;
  `accept_grace_ms = 30 s = epoch_ms` is that promise, expressed as a duration
  on the envelope rather than as a rule someone has to remember.

**Reseed triggers, and which process decides.** The coordinator decides, alone,
on observations it already makes:

```
reseed(g, c) at time t  ⟺  t − t_last(g, c) ≥ RESEED_MIN (10 s, D16)
                        ∧ ( t − t_last ≥ EPOCH_LEN (30 s)          // elapsed
                          ∨ |P_now △ P_last| > |P_last| / 2        // >50% churn
                          ∨ |P_now| < N_floor )                    // pool collapse
```

`docs/07:156` says churn reseeds fire on **gateway-observed** organic
disconnects. **This record moves that observation to the coordinator's own
session loss, and the divergence is deliberate.** Routing a disconnect from the
gateway to the coordinator needs a gateway→coordinator edge — the same edge
D24 declined, pointing the other way — to carry a signal the coordinator
already has, since a peer that has left the island has dropped its coordinator
session too. More importantly, "gateway-observed" was never the property that
mattered: a gateway-observed disconnect is just as manufacturable by an
attacker as a coordinator-observed one. What makes churn reseeds
un-grindable is the rate limit and the cooldown, not the identity of the
observer.

**The anti-grind argument, written out.** A colluder wants a favorable draw and
its only lever is to leave and return, changing `P` and forcing a redraw. Three
independent things make that a losing move:

1. **The redraw is not a redraw of a known function.** `k_{e+1}` is
   independent of `k_e` (clause (c)), so an attacker who has watched a hundred
   epochs has learned nothing about the next.
2. **The cooldown removes the bouncer from the pool.** An account whose
   session loss contributed to a reseed is excluded from `P` for
   `RESEED_COOLDOWN = 60 s`. With the reseed floor at `RESEED_MIN = 10 s`, one
   bounce forfeits `⌈60/10⌉ = 6` draws to buy 1. The cooldown is set at exactly
   `6 × RESEED_MIN` for that reason, and any value `> RESEED_MIN` is
   strictly-losing — 60 s is the round number that also exceeds the 30 s epoch
   length, so a bouncer misses a whole natural epoch as well.
3. **The draw is hypergeometric and the attacker does not move it.** With pool
   size `M`, `c` colluders and `N` drawn, the number of colluding slots is

   ```
   P(X = j) = C(c, j) · C(M − c, N − j) / C(M, N)
   ```

   which depends on `c/M` — physical co-residence in the cell — and on nothing
   the attacker can do in the 10 s between draws. At `M = 20`, `c = 3`,
   `N = 7`: `P(X ≥ 3) = C(17,4)/C(20,7) = 2380/77520 ≈ 0.031`, and per-intent
   success still needs the hidden
   required-K subset to land inside those three ([#142]).

**The self-chosen fallback is retired as an attestation source, and only as
that.** `witness_links` (`plugin.rs:599-607`) keeps its roster fallback for
D9 log-streaming fan-out, because a peer with no announcement must still stream
its input log somewhere and `MAX_WITNESS_LINKS` is a bandwidth bound. What it
must never again be is the set an attestation is checked against: once
enforcement is on, `WitnessSet::members` is written only from
`A.selected`, and an empty `WitnessSet` means "no attested intent may be
submitted for this cell", not "pick seven neighbours".

## Consequences

- **`docs/07 §4.1` diverges from this record in three places and is not edited
  here.** (i) `:159`'s `seed = HMAC-SHA256(k_epoch, cell_id ‖ epoch)` gains a
  grid term and a domain tag, and `k_epoch` becomes HKDF-derived from a master
  secret rather than free-standing. (ii) `:161`'s
  `WitnessSetAnnouncement { epoch_id, tick_range, seed_key_commitment,
  candidates, selected, coordinator_sig }` becomes clause (d)'s envelope:
  `tick_range` is replaced by `epoch_ms`/`accept_grace_ms` durations, `handle`
  and `prev_seed_key` are added, and "committed through the persistence
  gateway" becomes "committed *by* the persistence gateway on courier
  presentation". (iii) `:156`'s gateway-observed churn trigger becomes
  coordinator-observed, per clause (g). Under the index's precedence rule an
  accepted ADR wins, but the file should be reconciled; that edit is owed and
  is not taken in this lane.
- **`docs/09 §6 item 1` (`:132`) overstates the coordinator's reach.** "The
  coordinator writes each cell-epoch seed through the gateway to FDB … at
  issuance" is false under clause (a) in both halves: not the coordinator, and
  not at issuance. Its *conclusion* is unaffected and is in fact strengthened —
  "persistd validates intent attestations against FDB, never against
  coordinator memory, so attestations from before a failover remain verifiable"
  is exactly what clause (f)'s verbatim-envelope row provides.
- **`docs/08 §6`'s `epoch/{cell_id}` row (`:3236`) is respecified** — grid
  discriminator added (D22), epoch counter added to the key, handle index
  added, value shape pinned. The one-byte discriminator table at `:3247-3248`
  gains `e` and `f`.
- **Three wire additions are owed**, none of them a break: `CoordMsg::
  WitnessEpoch`, `GatewayMsg::WitnessEpoch`, and a `GatewayReply` rejection
  reason for `EpochStale`/`EpochUnknown`. All three are appended variants, and
  postcard's positional variant keying makes appending safe for deployed
  decoders in the way that editing `Intent` would not be.
- **One field stops being discarded.** `server.rs:530`'s `Ok(_claims) => {}`
  becomes a retained `(account, standing)`. That is the smallest change in
  this record and it is what four of the six eligibility rows in clause (e)
  rest on.
- **~~Capability deferred: probation age is not enforced.~~ Closed
  2026-08-23 (issue #214).** `docs/07:217`'s "fresh accounts carry probation
  (no witness eligibility for 7 days)" needed exactly the one new signed token
  field this bullet named, and it now has it: `SessionTokenClaimsV1` carries
  `on_probation` at claims version 2. A freshly purchased account can play
  immediately and cannot witness, so the Sybil cost is the purchase price plus
  the window. What the field does not buy is finer age granularity — it answers
  "past probation?" and nothing else — and the answer is only as fresh as the
  token's one-hour TTL cap.
- **Capability deferred: cross-account and offline-NodeId exclusion.** Party
  exclusion is only as good as the account↔NodeId map, which lives in `id/`
  and has no writer. A colluder attesting from a second NodeId of the same
  account that is *not* connected to this coordinator is not excluded.

  > *Erratum (2026-08-25, #210/#234):* "has no writer" is stale. The writer
  > exists: `FdbAccountStore::bind`
  > (`crates/orrery_identity/src/fdb.rs:353`, landed by #210) writes the
  > `id/da` account row, the `id/db` reverse binding, the versionstamped
  > `id/dh` history event and the `id/dw` rate window in one FoundationDB
  > transaction, over the keys #234 landed in `orrery_persistd::keyspace`.
  > What the map still lacks is a live **caller**: the only non-test call
  > path is `redeem_invite` (`crates/orrery_identity/src/invite.rs:497`), a
  > library function no deployed binary or endpoint invokes — the
  > `orrery-invite` binary mints offline against a local ledger and never
  > binds. In a running cluster the subspace is therefore still empty, and
  > the rest of this bullet stands unchanged: a colluder attesting from a
  > second NodeId of the same account not connected to this coordinator is
  > still not excluded.
- **Capability lost relative to `docs/07:156`: tick alignment is not
  verifiable at the gateway.** Epoch boundaries may still be tick-aligned
  coordinator-side, but the gateway judges the window on its own monotonic
  clock, because it cannot compare a peer-supplied tick against anything. If
  [#142] lands a tick binding inside the attestation, the window predicate can
  be tightened to a tick range and this record's `usable` becomes the outer
  bound rather than the only test.
- **Sibling gateways make the window process-local.** Under
  [D26](0026-sibling-gateways.md) two gateways may stamp different
  `first_seen_ms` for the same epoch, so their grace windows differ by the
  courier delay. This is not a shopping opportunity: D26 rule 2 sends a peer's
  writes to the gateway that owns the shard, so there is no second gateway to
  submit to, and the `intent/{intent_id}` idempotency row
  (`keyspace.rs:479-484`) makes a duplicate submission a replay of a recorded
  outcome rather than a second commit.
- **The reveal chain makes epoch 0 special.** The first announcement for a cell
  carries `prev_seed_key: None` and is unfalsifiable in the fairness tier until
  epoch 1 lands. A cell that sees exactly one epoch, ever, is never audited for
  fairness. This is accepted: such a cell had at most 30 s of attested traffic,
  and every one of its attestations is still authentic under the in-epoch tier.
- **Nothing here builds `orrery_identity`,** and this record's eligibility
  table is the honest statement of what its absence costs — which is the
  statement [#105] asked for, so the collusion argument is not quietly assumed.

## Alternatives considered

- **A VRF instead of commit-then-reveal.** Genuinely better on the axis this
  record struggles with: a VRF proof in the announcement makes the draw
  verifiable *immediately*, with no secret and no reveal, so the fairness tier
  and the authenticity tier collapse into one. Rejected for now on three
  grounds. It adds a cryptographic primitive to a stack that currently uses
  Ed25519, blake3, HMAC and ChaCha20 and nothing else; `docs/07:160` already
  names it as future work rather than the plan; and the cost it removes — the
  reveal delay — is capped at one epoch plus one reseed floor (40 s) by the
  chained reveal in clause (c), which is short next to a 14-day strike
  half-life. It is the alternative to revisit first, and revisiting it changes
  only clause (c) and one envelope field.
- **A coordinator→gateway edge, so the coordinator writes the row itself.**
  What `docs/09:132` describes. Rejected under clause (a) and, independently,
  under D24 clause (a), which already declined this edge for drain and whose
  three reasons all apply here unchanged. The additional reason specific to
  this record is the worst one: a direct write would let the gateway store a
  row it did not verify, so a compromised gateway could seed its own witness
  sets — D10's "never self-chosen" would then bind peers and not the cluster.
- **Packing `(cell, epoch)` into the existing `CellEpoch(u64)`.** The reading
  the issue asks about, and it is arithmetically impossible: `CellId` is a full
  `NonZeroU64` (`cell.rs:158`, `:172`) with no spare bits at level 21. Any
  packing truncates a cell id, which is D22's C-8 class deliberately
  reintroduced, and it would make two distinct cells share an epoch identity —
  precisely the failure the grid discriminator exists to prevent.
- **Adding a `cell: CellId` field to `Intent`.** The other reading. Rejected
  twice over: it is a positional-encoding break of the one message every
  deployed client sends on the critical path, and the value would be
  *self-declared* — a peer naming the cell whose witness set it wants to be
  judged against is the interest-grant failure mode (`interest.rs:1-13`:
  "self-declared interest would be self-granted authority") reappearing one
  field over. The signed announcement carries the cell instead, which is
  strictly stronger and costs no wire break.
- **Keying the durable row by handle alone (`epoch/{handle}`).** One family
  instead of two, and it drops the audit read pattern: an auditor asking "every
  witness set this cell ever had" would scan the whole family. The pair of
  families is the `lease/` + `lease-cell/` + `lease-location/` shape already in
  the keyspace (`keyspace.rs:208-238`), which exists for exactly this reason.
- **Keeping one row per cell (`epoch/{cell_id}`, overwritten each epoch), as
  `docs/08:3236` literally reads.** Rejected: an overwritten row destroys the
  history the reveal is *for*. An attestation adjudicated an hour later would
  find the row describing some later epoch, and the fairness tier would have
  nothing to check. The retention question the history creates is answered by
  `gc_deadline_ms`, not by throwing the row away on the next epoch.
- **Announcing only the commitment and the set size, withholding
  `candidates`.** Tempting, since the pool leaks who is in the cell. Rejected
  because it destroys the fairness tier: without the pool the coordinator can
  claim any `selected` was drawn from a pool it invents after the fact, and the
  reveal proves nothing. The privacy it would buy is also illusory — every
  candidate is by definition a peer in the same interest set, which D9 already
  streams input logs to (`0009-verifiable-core.md:29`).
- **Letting the peer pick its witnesses from the announced pool, with the
  coordinator only publishing the pool.** The minimal-mechanism option, and it
  is the status quo with a signature on it: a cheater picking K friends out of
  an announced 20 is the `plugin.rs:293-303` failure with extra steps.
- **Drawing a fresh random `k_e` per epoch instead of deriving it.** Simpler,
  and it loses every reveal that spans a coordinator failover — the exact
  window an attacker would target, since `docs/09:150` puts a failover gap at
  under 30 s, i.e. one epoch. HKDF from a provisioned master costs one
  derivation and makes the standby able to reveal what the leader issued.
- **Letting the gateway observe disconnects and report them to the
  coordinator.** `docs/07:156` as literally written. Rejected under clause (g):
  it is a new service edge to carry a signal the coordinator already has, and
  the property it was protecting (un-grindability) comes from the rate limit
  and cooldown, not from which process saw the socket close.

## Open questions

- **Who holds the epoch secret at enforcement time, and how does it get
  there.** This is the entanglement with [#142] and the biggest open item in
  the record. #142 derives the required-K subset from the epoch seed, and
  `docs/07:180` puts that derivation *at the gateway* — but the seed must stay
  secret from the submitter, and the submitter is the courier. So the
  announcement in clause (d) deliberately carries neither `k_e` nor `seed`, and
  this record provides no channel that gets them to a gateway. Three exits
  exist and #142 owns the choice: (i) the required subset is derived by
  something that already holds `K_master`, which means a coordinator→gateway
  edge and a D12 inventory change to argue for on its own terms; (ii) the
  required subset is derived from a *second* commitment published per intent
  rather than per epoch; (iii) required-K is checked only retroactively, after
  the reveal, and the live gateway enforces plain K-of-N over `selected`. This
  record is compatible with all three and is not guessing between them.
- **`gc_deadline_ms` for the epoch row has no derived default.** It must at
  least cover the longest interval in which an attestation can still be
  adjudicated, and nothing in the accepted set bounds that: `docs/07:237` pins
  evidence to retained `Ruleset` builds (3, D16) with no time bound at all.
  A 7-day default is proposed in D16's table on the strength of the 14-day
  strike half-life and nothing else, and it is the weakest number in this
  record.
- **Where the pool comes from when the cell spans islands.** `docs/07:157`
  says the candidate pool is "members of the entity's interest set", and the
  coordinator's presence view is per-peer coverage
  (`CoordMsg::Presence { cells }`, `coord.rs:376-379`), which is the right
  input. What is undefined is whether two islands whose peers both cover cell
  `c` produce one pool or two. One pool is the correct reading of "per
  cell-epoch" and needs the coordinator to union across islands; nothing in
  `IslandRegistry` does that today.
- **Whether `N_target = 7` should track population.** The value is fixed here
  at `MAX_WITNESS_LINKS` (`plugin.rs:150`) because that is the bandwidth bound
  D9's log fan-out already lives inside. In a cell at D6's 32-peer ceiling a
  larger set would dilute a fixed colluder count — but it does not, and the
  arithmetic runs the other way: at `M = 32, c = 3`, `P(X ≥ 3)` **rises** from
  0.0020 at `N = 5` to 0.0071 at `N = 7`, because drawing more slots can only
  make it likelier that all three colluders are drawn. A larger `N` helps only
  through the *required-K* subset ([#142]), where the colluders must land on
  specific slots, and hurts through fan-out. The two effects are owned by two
  records and their product has never been tuned against a measurement; P5's
  gauntlet is where that number should come from.
- **Whether the coordinator's own key rotation should be visible in the epoch
  row.** `issuer_key_id` is inside the stored envelope, so a rotation is
  auditable, but a verifier reading a row a month later needs the *retired*
  public key to check it. Key retention policy is `docs/09:178`'s and is not
  set here.

[#105]: https://github.com/baadc0de/orrery/issues/105
[#142]: https://github.com/baadc0de/orrery/issues/142
[#144]: https://github.com/baadc0de/orrery/issues/144
[#147]: https://github.com/baadc0de/orrery/issues/147
