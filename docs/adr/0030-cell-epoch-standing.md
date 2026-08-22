# ADR-0030: An intent is judged only under a cell-epoch its issuer stands in

**Status:** Proposed · **Date:** 2026-08-21 · **Decision:** D30

This decision is normative once accepted. See the [ADR index](../DECISIONS.md)
for precedence, scope, and the complete decision set.

**Amended 2026-08-22, while still Proposed.** A re-examination before
acceptance left clauses (a)–(e) unchanged and corrected what the tree had
moved under them: two line citations (`keyspace.rs`'s ledger builders and
`witness_epoch.rs`'s `resolve`/cache spans, below), and two statements whose
truth later records changed. Clause (b)'s "`PROTOCOL_VERSION` stays at 1" was
true when this record landed in code (#197, the day it was written);
[D29](0029-low-population-path.md) has since taken the constant to 2 and
[D34](0034-candidate-accounts-announcement.md) to 3, neither through this
record, so the clause now states the invariant that actually governs it — no
bump from here. Clause (c)'s description of `UnknownEpoch` as "D29's
provisional path" was false when written and is false now: the enforcement
landed by #182 refuses that cause, deliberately diverging from
[D27](0027-attestation-envelope.md) clause (e), and the divergence is recorded
where it is implemented; [D31](0031-id-account-subspace.md)'s closing note
assigns the reconciliation to amending record #208. Everything else in the
record verified against the tree as written, including the implementation.

**Supersedes:** nothing. It **closes** [D27](0027-attestation-envelope.md)'s
open question 2 — "a gateway serving more than one cell cannot resolve *which*
announcement an intent names from the intent alone" — and it does so **without**
the wire change that question proposed, so
[D28](0028-witness-set-seeding.md) clause (b) survives word for word: the cell
is still never a field of the intent. D27 clauses (a)–(g) are unchanged; this
record adds one conjunct to clause (d)'s admission predicate and restates
§4.4's collusion bound in the form that clause makes true again.
[D29](0029-low-population-path.md)'s `PROTOCOL_VERSION` bump is not shared, not
anticipated and not needed: nothing here touches the wire.

Out of scope, owned elsewhere: the draw, the attestation envelope and the
required-K derivation (D27, landed); announcement seeding and the `epoch/` row
(D28, landed); provisional commit (D29, #150); enforcement rollout policy.

## Context

### What the tree does today, after #182

K-of-N enforcement is real code. `check_attestation_quorum`
(`crates/orrery_persistd/src/intent/mod.rs`) resolves the intent's cell-epoch,
derives `E(I)`, checks set membership, checks the count, and requires D27's
drawn subset. The first line of it is the one this record is about:

```rust
let Some(epoch) = epochs.resolve(intent.cell_epoch.0) else {
    return Err(RejectionCause::UnknownEpoch);
};
```

`resolve` takes a handle and nothing else
(`crates/orrery_persistd/src/witness_epoch.rs:334-349`), against a cache keyed
`by_handle: HashMap<u64, Arc<AcceptedEpoch>>` (`:158-161`) that is **one map
for the whole gateway**. Anything any peer couriered is resolvable by any
other peer. `Intent::cell_epoch` is a submitter-chosen `u64`
(`crates/orrery_protocol/src/persist.rs:88-99`, "wire-identical to `Epoch`"),
so **the submitter picks which announced witness set judges its intent**.

#182 documented this at length on the function it could not fix, and D27
recorded it as open question 2 before the code existed. It stopped being
theoretical the moment enforcement became a mode a deployment can turn on.

### What the handle already binds, and what it does not

It is worth being exact, because the issue's framing —"an `Intent` names no
cell"— is only half true and the half that is false is the important one.

D28 clause (b) makes the handle `(incarnation << 48) | counter`, unique across
the coordinator's issuance history, and makes the announcement carry the cell:
"the cell is never a field of the intent, because a peer-supplied cell would be
self-declared — the exact failure the interest grant exists to prevent". So a
handle resolves to **exactly one `(grid, cell, epoch)`, under the
coordinator's signature**. `epoch_matches` refuses a second claim set under one
handle (`witness_epoch.rs:460-466`, called at `:246-252`).

The gateway therefore knows precisely which cell an intent names. What nothing
in the tree establishes is **whether that submitter has any business being
judged there**. The gap is not naming. It is *standing*.

That distinction decides the answer, because it means putting a `cell` field on
the intent buys nothing at all: the handle already names one cell, signed,
and a second name for the same fact would be exactly as unconstrained.

### There is no cell to derive from the ops

The other candidate D27 names is deriving the cell from the intent's subject.
#152 found no cell term on `IntentOp`, and the keyspace confirms it for the two
ops this cluster interprets:

```rust
pub fn ledger_bal_key(account: AccountId, asset: AssetId) -> [u8; 18]   // b"lb" ‖ account ‖ asset
pub fn ledger_item_key(item: ItemUid)                    -> [u8; 10]    // b"li" ‖ item_uid
```

(`crates/orrery_persistd/src/keyspace.rs:1454-1473`.) No grid, no cell, no
shard — flat keys owned by no shard, which is also why `IntentFence` fences on
the whole activated shard set rather than per cell. Every other op is
`Ruleset`-opaque by construction (D11 §2.2). **There is nothing to derive.**
This is established here rather than asserted, because it is the fact that
eliminates the second candidate.

### What a coordinator does say about where a peer is

One thing, and it is already on the gateway. An `InterestGrantV1` is a
coordinator-signed claim binding one peer to one grid and a bounded cell list
(claims at `crates/orrery_protocol/src/coord.rs:172-188`, signed envelope at
`:215-220`), localized against the gateway's own clock (`:128-139`) and
answered by `InterestAuthority::allows(peer, grid, cell, now_ms)`
(`crates/orrery_persistd/src/gateway.rs:605-614`). It is the predicate a live
`Claim` passes, the predicate D25 rule 3's fan-out uses, and — the part that
matters here — the predicate D28 clause (d) step 6 already gates *presenting*
an announcement on (`witness_epoch.rs:228-237`).

So the cache is already write-guarded by exactly the standing this record
wants, and is read-guarded by nothing.

## Decision

### (a) Standing, not naming, binds an intent to a cell-epoch

> **A gateway enforcing D27's quorum resolves an intent's `cell_epoch` handle
> only to a cell-epoch whose announced `(grid, cell)` the issuer holds live
> coordinator-confirmed interest in. An intent naming any other cell-epoch is
> refused with `NoStandingInCell`; no required subset is drawn for it, no
> attestation it carries is counted, and the refusal does not depend on the
> intent's contents, on the epoch's age, or on who couriered the
> announcement.**

The conjunct, written into D27 clause (d)'s admission predicate as the term
between resolution and everything else:

```
A          = the coordinator-signed announcement the handle resolves to
cell(A)    = (A.grid, A.cell)                    signed, never asserted (D28 (b))
Grant(p,t) = the cells of p's live coordinator-signed interest grant       (D7/D25)

admit(I) ⟹ cell(A) ∈ Grant(I.issuer, t)          ← this record
         ∧ … D27 clause (d)'s predicate, unchanged
```

Both terms of the comparison are coordinator signatures. Neither is anything
the submitter said about itself, which is the property that makes this a bind
rather than a declaration.

### (b) The cell stays off the wire; `PROTOCOL_VERSION` is untouched

> **`Intent` gains no field, `CellEpoch` stays a bare `u64` handle, and
> `PROTOCOL_VERSION` gains no bump from this record. D28 clause (b) is not
> amended, weakened or reinterpreted.**

The reason is (a)'s reason read backwards. A `cell` field would be a second
name for a fact the announcement already carries under signature, and the
submitter would choose it exactly as freely as it chooses the handle today —
so the gateway would have to check it against the announcement anyway, and the
check that actually refuses anything would still be the standing check. The
field would add a wire break, a `PROTOCOL_VERSION` bump, a new disagreement
rejection cause, and a change to a client-constructed type that
[D21](0021-ruleset-distribution.md)'s frozen harness API would have to absorb —
for no check that this record does not make without it.

This also keeps D29's bump D29's. Two records bumping one constant in one
release is a merge conflict and a rollout question nobody needs.

### (c) One cause, logged by name, collapsed on the wire

> **`RejectionCause::NoStandingInCell` is evaluated immediately after handle
> resolution and above every other quorum check, and answers on the wire with
> `REASON_ATTESTATION_QUORUM` — the code #182 already spends — never with a
> code of its own.**

Two orderings are load-bearing:

- **Above staleness.** Standing is a fact about the submitter and does not
  depend on the epoch's window. Answering `EpochStale` to a peer with no
  standing would confirm the existence *and the age* of a cell-epoch it has no
  business enumerating.
- **Below resolution.** A handle that resolves to nothing is `UnknownEpoch`
  whether or not the submitter stands anywhere, so the standing predicate is
  never consulted for an unresolvable handle. What that cause meets
  downstream is owned elsewhere: [D27](0027-attestation-envelope.md) clause
  (e)'s text routes it to the provisional path, the landed enforcement
  refuses it — a divergence recorded where it is implemented — and amending
  record #208 owns the reconciliation.

And `NoStandingInCell` is deliberately **not** a provisional case.
`UnknownEpoch`, `EpochStale` and `LowPopulationEpoch` all describe a gateway
that *cannot judge* — D27 clause (e)'s answer to those is a quarantined commit.
This one describes a submitter asking to be judged somewhere it does not stand,
which is a refusal at every population, in every netsplit, and with D29 landed
or not.

The wire collapse is #182's argument unchanged: the reason space below
`REASON_ATTESTATION_QUORUM` is one bit — "your attestations were wrong" as
against "your ops were wrong" — and every further distinction an operator needs
is a `RejectionCause` label in the gateway log, where an attacker cannot read
it.

### (d) The restored bound, and why it is per cell again

D27 §4.4's arithmetic, unchanged, for one cell-epoch whose eligible announced
set has `N` members of which `x` are the submitter's colluders:

```
p(x, N) = C(x, K) / C(N, K)              K = 3 (D16)
p(3, 7) = C(3,3) / C(7,3) = 1/35         the number D27 states
p(3, 5) = C(3,3) / C(5,3) = 1/10         the same attack at the N_floor
```

What the submitter's freedom did to it. Let `Cache(G,t)` be every cell-epoch
couriered to gateway `G` and still inside its window, and `x_c` the colluders
the submitter can get co-signatures from in cell `c`'s announced set:

```
before:   P = max  over (c,e) ∈ Cache(G, t)                       of p(x_c, N_c)
after :   P = max  over (c,e) ∈ Cache(G, t) with c ∈ Grant(issuer, t) of p(x_c, N_c)
```

**The per-attempt ratio was never the quantity that moved**, and saying so
plainly is the point of writing it out: a node occupies one cell, so it can be
a candidate for one cell's pool (D28 clause (c): the pool is "bounded by
clause (e)'s eligibility and by physically being in the cell"), so
`Σ_c x_c ≤ c_total` and `max_c p(x_c, N_c) ≤ p(c_total, N)` either way. An
attacker with three colluders always does best by putting all three in one
cell.

What moved is **which submissions inherit that cell's weakness**. Before, an
attacker captured the single cheapest cell in the world — a low-population one
at the `N_floor`, where three of five announced is 1/10 rather than 1/35 — and
then *every account it controls, submitting from anywhere*, was judged by that
captured set. One capture, world-wide reach, and the ledger's keys are flat, so
the effects were never confined to a cell in the first place. The witnesses
adjudicating a trade were witnesses of a cell no party to it was in, which is
not a probability statement at all: it voids D10's premise that a witness is a
peer who could have observed the thing it signs.

After (a), that cell's weakness is available only to intents submitted by a
peer standing in it. So:

> **The probability that all `K` required slots land on colluders is
> `C(x,K)/C(N,K)` for the cell the submitter is standing in, where `x` counts
> colluders in *that cell's* announced set. A submitter cannot raise the ratio
> by naming a different cell's epoch; raising it means putting colluders into a
> cell it is itself present in, which is exactly the cost D27 §4.4 prices.**

That is D27 §4.4's per-cell form, restored: `x` and `N` are again both
properties of one cell, and the attacker's colluders and its submissions have
to be in the same place.

Three cheap moves stay abolished and one is newly abolished. D27's three:
attestation shopping (the submitter still cannot choose which `K` count),
`intent_id` grinding (the draw key is secret until epoch end), reseed grinding
(D28's). The new one: **witness-set shopping** — choosing the judging set
rather than the co-signers within it.

### (e) What this does not narrow, stated as a number

> **A submitter may still be judged under any cell-epoch of its own grant, and
> a grant covers a neighbourhood rather than a cell:
> [D5](0005-spatial-model.md)'s 27-cell interest set in practice, capped at
> `MAX_INTEREST_GRANT_CELLS = 64` by the envelope
> (`crates/orrery_protocol/src/coord.rs:152-157`).**

So the residual choice is at most 64 cell-epochs and normally 27, against a
cache bounded before this record only by what peers couriered — every populated
cell of the shard the gateway serves. Each of the 27 is a cell the submitter is
physically in, and colluders spread across them lower rather than raise the
best `x_c`, per (d). Narrowing 27 to 1 needs a statement nobody makes today
that this intent belongs to *this* cell; see Open questions.

## Consequences

- **A gateway enforcing the quorum now needs an interest authority.**
  `BaselineIntentValidator::enforcing` takes both, and enforcing with only one
  fails closed (`NoStandingInCell`) rather than treating an absent predicate as
  a satisfied one. A deployment that turns enforcement on without coordinator
  grants reaching its gateway refuses every attested intent — which is the
  correct direction, and one more reason the enforcement switch defaults off.
- **What is lost: an honest peer whose grant lapsed mid-flight is refused.**
  A grant's expiry is enforced on the read path and nowhere else (D25 rule 3),
  so a peer that co-signed at `t` and submitted at `t + ttl` is refused with a
  cause that reads as an attack. It is answered by resubmitting under a fresh
  grant, and the gateway log distinguishes it, but the client sees only
  `REASON_ATTESTATION_QUORUM` — the same collapse (c) accepts for the draw.
  The mitigation is grant TTL against co-sign budget (150 ms, D27 (g)), which
  is three orders of magnitude of headroom; the exposure is real only across a
  netsplit.
- **A peer that leaves a cell can no longer commit intents it had already
  collected co-signatures for in it.** Judged against the announced set of the
  epoch it names (D28 clause (g)) but only while it stands there. That is a
  genuine narrowing of D28 (g)'s grace, and it is deliberate: the grace exists
  so a *turnover* does not invalidate a co-signature, not so a departure
  carries one away.
- **No FoundationDB operation is added and no round trip.** `allows` is a
  lock-guarded map lookup on state the gateway already holds for every
  connected peer, so D16's p99 < 10 ms intent budget is untouched.
- **The cache stays global and that is now safe.** Guarding the read path means
  the cache no longer has to be per-peer, so nothing about D28 clause (f)'s
  single-map read pattern or the sibling-handover path (D26) changes.
- **`BaselineIntentValidator` stops deriving `Debug`.** `InterestAuthority` is
  not `Debug` — printing one would put every peer's interest set in a log line
  — so the impl is written out and reports only whether an authority is
  configured.
- **Nothing is required of the submitter, again.** No new field, no
  pre-flight, no extra RTT. An honest peer already holds the grant this checks;
  it is the same one it presented to get its interest served.

## Alternatives considered

- **Put the cell on the intent** (D27's first candidate; the wire change).
  Rejected on the ground (b) gives: the handle already names one cell under the
  coordinator's signature, so the field restates a signed fact with an
  unsigned one, and the submitter would choose it as freely as it chooses the
  handle. The check that refuses anything is still a standing check, so the
  field buys a wire break, a `PROTOCOL_VERSION` bump, a D21 harness-API change
  on a client-constructed type, and a new disagreement cause, for zero
  additional refusals. It would also require reading D28 clause (b) as amended,
  and it is accepted and correct.
- **Derive the cell from the ops** (D27's second candidate). Rejected on the
  evidence in Context: `ledger_bal_key` and `ledger_item_key` carry no spatial
  term, every other op is `Ruleset`-opaque, and #152 already found item
  transfer has no cell anywhere in its path. There is no derivation to write.
  If a future op family *is* spatial — structure placement is the obvious one —
  it can add a strictly narrower conjunct on top of (a) without contradicting
  it.
- **Link the binding through the `Ruleset`** (D27's third candidate). The only
  candidate that could name one cell rather than a neighbourhood, and it loses
  on two counts. D21 froze the harness API, so a `cell_of(&Intent)` hook is an
  ADR of its own before it is a line of code; and it would put a security
  conjunct of the admission predicate inside game-supplied code, where a
  `Ruleset` bug becomes a witness-set bypass. D27 placed the draw with the
  party whose compromise already ends the game; this would place half the
  predicate with a party whose compromise does not.
- **Make the cache per-peer** — a peer resolves only handles it couriered
  itself. Rejected as both weaker and more expensive: weaker because an
  attacker simply couriers the announcement it wants (announcements are public,
  signed, and any peer holding interest in the cell can hand one over), and
  more expensive because it multiplies the cache by the session count and
  re-mints nothing — the draw key must stay per cell-epoch, so the per-peer
  copies would have to share it anyway.
- **Refuse an announcement whose cell no *current* session stands in**, i.e.
  tighten step 6 on the write path further. Rejected because it does not touch
  the read path at all: the cache is written by peers standing in the cell and
  read by anyone, and this changes the first half twice.
- **Require the announcement to be presented on the submitting connection.**
  Attractively simple, and it is the same mistake as the per-peer cache with an
  extra round trip: presenting is not standing, and any peer that holds
  interest in the cell may present.
- **Leave it, and rely on the co-signature requirement.** The honest statement
  of the status quo: shopping is not a forgery, the colluders must still sign.
  Rejected because the bound in an accepted record would remain false, and a
  security argument whose stated number is not the number is worse than no
  number. It is also the cheapest of all the attacks to run: choosing a
  different `u64` costs nothing.

## Open questions

1. **Narrowing 27 to 1.** The residual in (e) closes only if something names
   the cell an intent belongs to. The candidate that does not need a
   `Ruleset` is a coordinator grant that distinguishes the peer's *home* cell
   from its neighbourhood — `CoordinatorInterestSnapshot` carries
   `covered_cells` and no home term (`coord.rs:113-126`) — which is a
   coordinator-side record, not this one.
2. **Whether standing should be evaluated at co-sign time as well.** A witness
   deciding whether to co-sign has the same question this record answers at
   admission, and answers it today by not asking. It is `orrery_witness`'s
   record to make and it is not on the critical path: the gateway's refusal is
   what admits or does not.
3. **Whether an intent should be judged under the cell-epoch of the *witnesses
   that answered*, rather than the one the submitter named.** It would remove
   the choice entirely — the set is whichever set signed — but it inverts the
   draw's direction (the required subset would have to be derived after the
   attestations are fixed, from them), which is the commit-then-check posture
   D27 rejected. Recorded because it is the shape a reviewer will propose.
4. **A grant that lapses inside the co-sign budget.** Consequences prices this
   as negligible against a 150 ms budget, and no measurement exists. If a
   deployment's grant TTL is ever short enough to matter, the fix is a grace on
   the standing check mirroring D28 clause (g)'s, not a weakening of (a).
