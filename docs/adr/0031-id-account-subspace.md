# ADR-0031: The `id/` account subspace, its reverse index, and what a miss means

**Status:** Accepted · **Date:** 2026-08-21 · **Decision:** D31

> Accepted by the repo owner on 2026-08-21, together with the four open
> questions below, which were resolved by owner-delegated decision recorded on
> PR #225. Question 5 remains open and is the standing record's to answer.

This decision is normative once accepted. See the [ADR index](../DECISIONS.md)
for precedence, scope, and the complete decision set.

**Supersedes:** nothing. It **complements** [D27](0027-attestation-envelope.md)
clause (f) — the recorded `E(I)` stays the audit's source and this record says
so in clause (h) rather than letting two mechanisms both claim it. It **closes
the gateway half** of [D28](0028-witness-set-seeding.md) clause (e)'s
*approximated* party-exclusion row and leaves the coordinator half open, with
the residual quantified rather than implied. It is the durable substrate
[D12](0012-backend-services.md) assigns to `orrery_identity` and that
[D10](0010-witnessing.md) items 4 and 5 rest on.

Out of scope, owned elsewhere: the strike ledger, standing computation and the
quarantine/cooldown/ban thresholds (the standing record); enforcement ramp
policy (the ramp record); login, credentials, payment and what an account
costs (D10 says it costs something; pricing it is a product decision); any
change to `SessionTokenClaimsV1`; and any code — this record produces a
decision and an index row, and #209 implements the keyspace against it.

## Context

### What the tree actually holds, verified

`AccountId` is a bare `u64` newtype whose own doc comment names the row that
does not exist (`crates/orrery_protocol/src/persist.rs:153-158`): "an
`AccountId` is the durable identity ledger balances, item ownership, and
profile rows are keyed by (`id/{account_id}` binds the two, D10)". The
documented row is `docs/08-persistence.md:3234` — `id/{account_id}` →
"account record, bound NodeIds, tokens", owner `orrery_identity`, "canonical
identity subspace; Sybil cost anchor (D10)". There is no `orrery_identity`
crate: `crates/` holds fourteen and none is it.

The only NodeId→AccountId table anywhere is per-connection and per-process. A
gateway lifts `(account, standing)` out of the verified session token and hangs
it on the peer session (`crates/orrery_persistd/src/gateway.rs:2921-2934`); a
coordinator does the same into `SessionFacts`
(`crates/orrery_coordinator/src/witness.rs:178-184`). Both are, by
construction, maps of *who is connected to me right now*.

### The two consumers, in their own words

The coordinator, on the pool it draws from
(`crates/orrery_coordinator/src/witness.rs:336-342`):

> per-account exclusion holds only within this coordinator — a NodeId bound to
> the same account but connected elsewhere is not deduped, because nothing
> writes the `id/` rows that would answer it.

The gateway, on the admission-time half
(`crates/orrery_persistd/src/intent/mod.rs:675-691`):

> Two NodeIds bound to one account therefore still let a party attest for
> itself, and **this filter does not claim otherwise**. […] **A check that
> fails open on a miss is worse than an absent one, because it reads as
> coverage.** No FDB key family binds an account to a NodeId either, so an
> authoritative answer is a durable read, which the admission path does not
> take.

That last sentence is the whole design problem in one line, and the sentence
before it is this record's clause (f) already written by the code that could
not implement it.

### What `E(I)` is, exactly, and which direction the lookup runs

`eligible_witnesses(&epoch.snapshot.selected, intent.issuer)`
(`crates/orrery_persistd/src/intent/mod.rs:882-886`,
`crates/orrery_protocol/src/persist.rs:356-378`) is the announced set minus the
issuer's **NodeId**. D10 item 4 asks for the announced set minus every party
"matched on **accounts and every NodeId bound to them**".

The party side is not the missing half. The issuer's account is on the
connection (`IntentContext`, `gateway.rs:6709-6717`), and the ledger ops this
cluster interprets are keyed by `AccountId` outright — `ledger_bal_key(account,
asset)` (`keyspace.rs:1015-1022`), and the receipt's `parties: Vec<AccountId>`
is built straight from the writes (`keyspace.rs:1122`,
`crates/orrery_persistd/src/intent/fdb.rs:1364`). **The party set is already a
set of accounts.**

The missing half is the *candidate* side. `WitnessEpochClaimsV1` carries
`candidates: Vec<NodeId>` and `selected: Vec<NodeId>`
(`crates/orrery_protocol/src/coord.rs:446-448`) and no account anywhere. So the
question a gateway must answer, ≤ 7 times per attested intent, is:

```
owner(n) : NodeId → Option<AccountId>          for n ∈ selected
E(I)     = [ n ∈ selected : owner(n) ∉ P(I) ]  in announced order
```

The reverse direction is the *only* direction any consumer in this epic reads.
That fact decides clause (b) before any storage argument does.

### The prefix-byte space is nearly exhausted, and the guard covers less than it says

`all_key_families_are_range_disjoint`
(`crates/orrery_persistd/src/keyspace.rs:1849-1890`) enumerates **fourteen**
families — `a c e f g i k l n p s u v w` — and asserts their one-byte prefixes
are distinct. Grepping every prefix assignment in the same file finds
**seventeen** distinct first bytes in use: the fourteen plus

| Byte | Family | Landed in |
|---|---|---|
| `m` | `lease-cell/{grid}/{cell}/{entity}` (`keyspace.rs:219-226`) | the lease registrar |
| `o` | `lease-location/{grid}/{entity}` (`keyspace.rs:233-238`) | the lease registrar |
| `r` | `provisional/{account}` (`keyspace.rs:644-651`) | D29 |

None of the three is in the table the disjointness test iterates. The guard is
therefore not the guard it reads as — it proves fourteen of seventeen bytes
distinct and would not notice a new family colliding with `m`, `o` or `r`. The
arithmetic of the remaining space, which is why this matters here:

```
lowercase bytes                       26
taken as a family prefix              17   a c e f g i k l m n o p r s u v w
remaining                              9   b d h j q t x y z
of those, in use as an exclusive
  range end (b h j q t x)              6   fence→b  attest→h  intent→j
                                           seedprog→q  seedmap→t  world→x
cleanly free                           3   d y z
```

Three clean bytes, against five families `docs/08-persistence.md` §6 documents
and nothing implements: `id/`, `strike/{account_id}/{versionstamp}`,
`jarchive/{node_id}/{segment_seq}`, `section_pin/{section_key}`, and
`coord/leader`. Taking one byte per family runs out. This record therefore
spends **one** byte and puts three key kinds inside it (clause (a)), which is
the pattern the ledger already established — `lb`/`li`/`lr` under one `l`
(`keyspace.rs:1006-1009`: "All three families share the `b'l'` prefix,
discriminated by the second byte so range scans of one kind never see
another").

One more thing found while counting, recorded because a later reader will
otherwise assume the guard covered it. `lease_key(grid, entity)` is
`b'l' ‖ grid:u32 BE ‖ entity:u64 BE` (`keyspace.rs:210-216`) — the same first
byte as the ledger family, but byte 1 is the grid id's most significant byte
rather than an ASCII discriminator. The two are disjoint today only because
grid ids are small: a `grid.0 ≥ 0x6200_0000` puts a lease row inside the
`ledger/bal/` sub-span, `0x6900_0000` inside `ledger/item/`, `0x7200_0000`
inside `ledger/receipt/`. No full-family scan of either exists in the tree, so
nothing reads across the overlap today and this is latent rather than live. It
is not this record's to fix — it is filed as a finding, and it is the second
reason clause (a) writes its sub-discriminators as ASCII bytes at a fixed
offset rather than letting an id's high byte land there.

### Why the coordinator is not a reader

`crates/orrery_coordinator/Cargo.toml` declares no `foundationdb` dependency
at all; its `fdb-state` feature is a stub with the comment "Not implemented in
P1; the in-memory state is reconstructible from presence". A coordinator
**cannot** read `id/` today, and clause (d) does not ask it to — because it
does not need to. Every candidate in `eligible_pool`
(`witness.rs:344-384`) comes out of `self.sessions`, i.e. has a live token-
verified session with that coordinator, so its account is already known
without any lookup. The reverse index is a gateway concern.

## Decision

### (a) One family byte, `d`, with three ASCII-discriminated sub-spans

> **The `id/` subspace occupies the single one-byte family prefix `b'd'`. Its
> span is `[b"d", b"e")`. Within it, byte 1 is an ASCII discriminator at a
> fixed offset: `b"da"` account records, `b"db"` the reverse binding index,
> `b"dh"` the append-only binding history. Byte `d` is added to
> `all_key_families_are_range_disjoint`, and so — as a condition of that
> change, not as a suggestion — are `m`, `o` and `r`.**

```
da ‖ account:u64 BE                              10 B  → AccountRow
                                                   (incl. binding_event_count:u32
                                                    and first_event_ms:u64 — see
                                                    Resolved question 2)
db ‖ node:[u8;32]                                34 B  → (AccountId, bound_at_ms)
dh ‖ node:[u8;32] ‖ versionstamp:[u8;10]         44 B  → (AccountId, BindKind, at_ms)

id_range_start() = vec![b'd']
id_range_end()   = vec![b'e']        // one past 'd'; also the epoch/ family start
```

`'d'` is the first free byte of `id/`'s own name — `'i'` is the intent
idempotency row and was never available — and it is one of the three bytes not
already spoken for as a range end. The end bound `b'e'` doubling as another
family's start is house precedent, not an accident: `provisional_range_end()`
is `vec![b's']` and `seedmap`'s span starts there, with the reason written out
at `keyspace.rs:659`. An exclusive bound of `[b'e']` cannot include any key
`e‖…`, because `[0x65] < [0x65, …]`.

Sub-span ordering `a < b < h` makes the three scans disjoint by construction,
exactly as `lb < li < lr` does. Keeping them under one byte rather than
spending three is forced by the budget arithmetic in Context, and it is also
correct on the merits: all three are written by one service, in one
transaction, with one retention policy.

### (b) The reverse direction is a materialized index inside the family, not a scan and not a second family

> **`db ‖ node` is written by identity in the same FoundationDB transaction as
> the `da ‖ account` row it derives from, so the two are never observed
> disagreeing. No consumer ever answers `owner(n)` by scanning `id/`.**

The three candidate shapes and why this one:

| Shape | Cost of `owner(n)` | Verdict |
|---|---|---|
| Forward rows only, scan `da` | O(accounts) range read per lookup | Not a lookup. At 10⁷ accounts it is a full-subspace scan on the admission path |
| Second family, e.g. `y ‖ node` | O(1) point read | Works, and spends one of three remaining clean bytes for nothing (a) does not give |
| Index inside the family (**this**) | O(1) point read | Same cost, one byte, one transaction, one retention rule |

Atomicity is the load-bearing half, not the byte. A reverse index maintained
in a second transaction has a window in which `db` names an account that `da`
no longer binds — and under clause (f) a *wrong* answer is worse than a miss,
because a miss excludes and a wrong answer admits.

The history sub-span is keyed **by node, not by account**, for the same
reason (b) exists: the audit's question is `owner_t(n)` for the ≤ 7 announced
NodeIds, so a node-keyed history answers it with ≤ 7 bounded reverse range
reads, each contiguous. The per-account question — "which devices has this
account ever held" — is a support and abuse-investigation query, not the
audit's, and is served offline from the same rows. That asymmetry is stated
rather than split into two histories, because a second history is a second
thing to keep consistent for a query no hot path makes.

### (c) The binding history is append-only, and this is what it costs

> **Every bind and unbind appends one versionstamped `dh` row and is never
> updated in place. `db` and `da` are current-state rows and are mutated; `dh`
> is the log they are a fold of.**

D27 clause (f) (`0027-attestation-envelope.md:417-421`) names exactly this as
the upgrade path: "an append-only account↔NodeId binding history in
`orrery_identity` so that `E(I)` becomes reconstructible from first
principles". This record takes it, and states the two things D27 could not.

**First, it is sufficient for `E(I)` specifically.** Worth checking rather than
assuming, because `E(I)` depends on more than bindings:

```
E(I)  = selected(A)  \  { n : owner_t(n) ∈ P(I) }

selected(A)   coordinator-signed, in the announcement            → recorded (D28 (b))
P(I)          the receipt's parties: Vec<AccountId>              → recorded (keyspace.rs:1122)
owner_t(n)    node → account, as of the derivation instant t     → this record's dh rows
```

The presence, cooldown and standing filters of D28 clause (e) shape
`candidates`, and `candidates` is inside the coordinator's signature. So the
three inputs are all published or derivable, and D27's claim holds.

**Second, sufficiency of the inputs is not sufficiency of the reconstruction**,
and this is the part a reviewer will otherwise over-trust. The gateway derives
`E(I)` from its *cache* (clause (e)), which lags the durable history by at most
the staleness bound `T_stale`. An auditor replaying `dh` at the commit instant
can therefore compute a different `E(I)` than the gateway lawfully computed,
whenever a binding changed within `T_stale` of the commit. So:

> **An append-only history makes `E(I)` reconstructible up to the cache
> staleness bound, not exactly. Closing the residual needs the gateway to
> record which binding view it resolved against, and the natural carrier is a
> `binding_epoch` vector alongside D27 clause (f)'s `AttestRow.eligible`.**

That is a recommendation to the record that owns `AttestRow`, not a change this
one makes. Clause (h) says what stands in the meantime.

**Storage.** One row is 44 bytes of key plus a value of `(AccountId, BindKind,
at_ms)` ≈ 17 bytes postcard, call it 68 B with overhead. Growth is *events*,
not sessions — a bind is a credentialed user action, not a login:

```
realistic  10^7 accounts × 0.05 events/account/day × 90 d
           = 4.5×10^7 rows × 68 B ≈ 3.1 GB
adversarial, uncapped: unbounded — bind/unbind is a free loop
adversarial, capped at clause (g)'s 8/day:
           10^7 × 8 × 90 = 7.2×10^9 rows × 68 B ≈ 490 GB
```

> *Erratum (2026-08-22, [ADR-0036](0036-binding-rate-window.md)):* the
> adversarial line above prices only the 24 h cap. Clause (g) sets two caps
> and both hold together: 64 per rolling 30 d bounds any 90-day retention
> horizon at three disjoint 30-day windows — ≤ 192 events per account,
> achievable by spreading uniformly at 64/30 d, which also satisfies the
> 24 h cap. The bound is 10⁷ × 192 × 68 B ≈ **131 GB**, not ≈ 490 GB; the
> published figure was a valid upper bound and overstated by 3.75×. Nothing
> else in this record changes.

The realistic figure is nothing next to `world/`. The adversarial figure is
why clause (g) carries a rate cap at all: **append-only without a write cap is
an unbounded storage amplifier with a free trigger**, which is the same shape
D25 priced for `Expire` fan-out. The current-state rows are small and bounded:
`da` at ≈ 282 B/account (8 NodeIds inline) is 2.8 GB at 10⁷ accounts, `db` at
50 B/binding is 1.0 GB at two devices each.

**What choosing the other way would have cost**, stated because the issue asks
for it explicitly: D27 clause (f)'s audit bound
(`0027-attestation-envelope.md:405-415`) — the audit proves "given the
eligibility list you recorded, did you draw correctly", never "was that list
honest" — would become permanent rather than provisional. A gateway that lied
about `E(I)` would pass forever, with no artifact that could ever contradict
it.

### (d) Identity writes; the gateway reads; the coordinator does not read

> **`orrery_identity` is the sole writer of every `d` row. The gateway is the
> only durable reader, and it reads FoundationDB directly rather than calling
> identity on the intent path. The coordinator reads nothing: every candidate
> it seeds from has a live token-verified session, so its account is already
> in hand.**

Single-writer is not a style preference here. `db` must be written with `da`
in one transaction (b), and a second writer would have to be trusted to
maintain an index whose staleness is a security property under (f).

The gateway reading FDB directly, rather than through identity, follows from
what is already true of it: it is the sole writer of durable truth (D11), it
already holds an FDB handle, and D12's identity service is not built. Routing
the read through identity would add a service dependency to the intent path
for a value the gateway can read in one point read, and would make an identity
outage a gateway outage — which `docs/09-services-and-ops.md:175`'s grace rule
deliberately forbids ("an identity outage locks out new logins, never
in-flight play").

**Sibling gateways (D26).** A reader never needs to reach another gateway's
connected peers, and this is the reason the tier order in (e) is written the
way it is: tier 1 is a *convenience* over the durable answer, not a separate
source of truth. Two siblings serving one shard resolve `owner(n)` from the
same `db` rows and agree. Where they can disagree is on cache freshness, in
exactly the way D28's Consequences already record for `first_seen_ms` under
D26 — and it is not a shopping opportunity for the same reason D28 gives: D26
rule 2 sends a peer's writes to the gateway that owns the shard, so there is no
second gateway to submit to.

### (e) Three tiers, and the admission path takes no FoundationDB read

> **`owner(n)` resolves in three tiers: (1) a verified session token on a live
> connection; (2) a bounded in-process binding cache; (3) a `db` point read.
> The admission path consults tiers 1 and 2 only. A tier-2 miss enqueues an
> asynchronous tier-3 fill and returns `None` to the caller in the same
> instant — it never blocks.**

```
owner(n):
  if let Some(session) = live_sessions.get(n)   -> Some(session.account)   // tier 1, signed
  else if let Some(e) = cache.get(n), fresh(e)  -> Some(e.account)         // tier 2
  else { fill_queue.push(n); None }                                        // tier 3, off-path
```

Tier 1 never misses for a connected peer, because a peer cannot hold a session
without presenting a token that binds `(account, node)` under identity's
signature (`SessionTokenClaimsV1`,
`crates/orrery_protocol/src/identity.rs:73-87`). The residual miss set is
therefore exactly: *NodeIds in an announced set that are not connected to this
gateway and not in its cache* — which is a much smaller set than "every NodeId"
and is the honest scope of clause (f).

**Cost on the admission path**, against D16's `Intent commit p99 < 10 ms`
(`0016-parameter-reference.md:16`):

```
lookups per attested intent = 1 issuer (tier 1) + |selected| ≤ 7 (D16: witness set target N = 7)
                            = 8 hash probes
cost                        ≈ 8 × 60 ns ≈ 0.5 µs
fraction of budget          = 0.5e-6 s / 10e-3 s = 5×10⁻⁵ = 0.005 %
FDB operations added        = 0
```

**Cache shape and invalidation.** A bounded map, capped at
`MAX_BINDING_CACHE_ENTRIES = 65_536` — the precedent is
`MAX_REPORT_LIMITER_ACCOUNTS = 4_096`
(`crates/orrery_persistd/src/gateway.rs:445`, and its own comment: "cycling
accounts turns rate limiting into unbounded memory"). At ≈ 96 B per entry that
is ≈ 6.3 MB. An entry is `(account, binding_epoch, fetched_at_ms)` and is
valid while both hold:

```
now_ms − fetched_at_ms < T_stale                       T_stale = 30 s (proposed, (i))
binding_epoch(account) unchanged since the fill        pushed by identity
```

`T_stale` is set equal to D27's witness epoch length deliberately: a stale
binding then cannot survive the epoch it could distort, and the reseed at the
next epoch boundary is already a full re-derivation. Invalidation is a push
from identity carrying `(account, binding_epoch)`; losing the push channel
degrades the cache to TTL-only, which is safe precisely because of (f) — a
degraded cache misses more, and a miss excludes.

### (f) A miss fails **closed**, and the closure is a demotion, not a refusal

> **A reader that cannot establish which account a NodeId is bound to treats
> that NodeId as if it were a party: it is removed from `E(I)` and is never
> seated in a witness set. A miss excludes; it never admits.**

This is the load-bearing sentence of the record, so here is the argument
rather than the assertion.

**Who controls whether a lookup misses.** The attacker. Binding a second NodeId
to an account is a credentialed operation the attacker performs on its own
account; keeping that NodeId out of a gateway's tier-1 map is achieved by not
connecting it there; keeping it out of tier 2 is achieved by waiting out
`T_stale`, or by cycling enough NodeIds to evict it from a 65 536-entry cap.
Every one of those is free. **A predicate whose "unknown" branch the attacker
selects must not have "admit" on that branch.**

**What fail-open costs, as a number.** Take a cell with 9 honest candidate
accounts and one attacker account. The attacker binds `m = 5` NodeIds to its
single account. `N = 7` are selected from the candidate pool; D27's per-intent
draw then requires a specific `K = 3` (D16) of `E(I)` to have attested, and the
attacker wins only if all three drawn slots are its own.

```
account dedup working (D28 (e), "one witness slot per account"):
    pool = 10 accounts, attacker holds ≤ 1 of the 7 selected
    P(capture) = 0                                     — it can never hold K = 3

fail-open on a miss (the attacker's 5 NodeIds each resolve to ⊥, so none is deduped):
    pool = 9 + 5 = 14 nodes, 7 selected uniformly
    P(x = j attacker nodes selected) = C(5,j)·C(9,7−j) / C(14,7),   C(14,7) = 3432
        j=3: 10·126 = 1260      j=4: 5·84 = 420      j=5: 1·36 = 36
        P(x ≥ 3) = 1716/3432 = 1/2 exactly
    then D27's draw:  p(x,K) = C(x,K)/C(N,K),  C(7,3) = 35
        P(capture) = Σ_j P(x=j)·C(j,3)/35
                   = (1260·1 + 420·4 + 36·10) / (3432·35)
                   = 3300 / 120120 = 5/182 ≈ 2.75 %
```

Now the part that matters more than the percentage. **Fail-closed does not make
that 2.75 % smaller — it makes it expensive.** An attacker willing to buy five
*accounts* reaches the same 2.75 % with exclusion working perfectly. What
fail-open changes is the price:

```
cost(2.75 % capture) with exclusion enforced   = 5 accounts, each with D10's acquisition cost
cost(2.75 % capture) failing open              = 1 account + 4 ed25519 keypairs ≈ free
```

D10 item 5 — "identities are accounts (D12) binding NodeIds, with acquisition
cost (Sybil resistance)" — *is* that price. Failing open does not weaken the
Sybil anchor; it removes it, and leaves D28 clause (e)'s per-account dedup as
a filter that reads like enforcement and enforces nothing. Which is the
gateway's own stated objection at `intent/mod.rs:684-685`, applied to itself.

**What fail-closed costs, and why it is affordable.** Nobody is refused at
seeding time: every candidate the coordinator considers has a live session with
it, so tier 1 answers and the miss set there is empty (Context, "Why the
coordinator is not a reader"). At the gateway the miss set is non-empty, and
the cost is that `|E(I)|` shrinks. The consequence of shrinking is already
specified and is *not* a refusal:

```
|E(I)| < WITNESS_SET_FLOOR_N (= 5, coord.rs:395)
   ⟹ RejectionCause::LowPopulationEpoch          (intent/mod.rs:887-889)
   ⟹ D29's quarantined provisional commit + spot replay
```

So an honest intent whose announced set the gateway cannot fully resolve is
committed provisionally and finalized by replay, not lost. **Fail-closed here
means "demote to the path that already exists for a gateway that cannot judge",
which is the direction D27 clause (e) already chose for `UnknownEpoch` and
`EpochStale`.** That is what makes closing affordable, and it is why this
record depends on D29 rather than merely citing it.

**One cause, logged by name, collapsed on the wire**, following D30 clause (c).
An attestation from an announced NodeId whose binding did not resolve is not
`WitnessOutsideAnnouncedSet` — that label would send an operator hunting a
forgery. It gets `RejectionCause::UnresolvedWitnessBinding` in the gateway log
and answers `REASON_ATTESTATION_QUORUM` on the wire like every other
sub-distinction in that space.

**The interlock with (g) is worth naming.** Current-bindings-only exclusion
plus fail-closed is strictly stronger than historical-bindings exclusion plus
fail-open. An attacker that unbinds a NodeId just before submitting, to shed
its account, converts that NodeId's lookup into a miss — and a miss excludes.
It cannot buy anything by unbinding.

### (g) Retention: `db` is current, `dh` is history, and exclusion matches current

> **Unbinding deletes the `db` row immediately (`docs/09:175`: "unbinding is
> immediate") and appends a `dh` row. Party exclusion and witness dedup match
> **current** bindings at the instant of derivation, never historical ones.
> The history is retained for `T_history`; `da` and `db` do not expire.**

Why current and not historical, since both are available once (c) lands:

- Historical matching excludes a device that has legitimately changed hands or
  been re-bound after a compromise — `docs/09:175`'s "account NodeId
  compromise = user re-binds" makes that a *supported* flow, and permanently
  tainting the key would punish the victim.
- Historical matching buys nothing against the shed-a-NodeId attack, because
  (f) already refuses an unresolvable NodeId.
- Historical matching would make exclusion non-monotonic in a way no reader
  can cache: the answer would depend on the query instant *and* on an unbounded
  past, so `T_stale` would stop bounding anything.

Rate cap, without which (c)'s append-only log is a free storage amplifier:

> **Binding events are capped at 8 per account per rolling 24 h and 64 per
> rolling 30 d, refused at identity, and the cap is a D16 row.**

`T_history` is an **owner decision**; both options are priced in (c) and in
Open questions 2.

### (h) D27 clause (f) is complemented, not superseded

> **`AttestRow` (`crates/orrery_persistd/src/keyspace.rs:920-940`) remains the
> audit's source for `E(I)`. Nothing in this record authorizes an auditor to
> reconstruct `E(I)` from `dh` instead of reading the recorded vector.**

The issue asks for this in writing, because two mechanisms both claiming to be
the audit's source is worse than either alone. The division:

| Question | Answered by |
|---|---|
| "Given the eligibility list you recorded, did you draw the required subset correctly?" | `AttestRow.eligible` + the announcement + the revealed draw key (D27 (f), unchanged) |
| "Was that eligibility list consistent with the bindings that existed at the time?" | `dh`, as of the commit instant, up to `T_stale` (this record, (c)) |
| "Which account was this NodeId bound to on day X?" | `dh` (this record) |

The second row is new capability, and it is the row D27 clause (f) said did not
exist. It is a *cross-check* on the recorded vector, not a replacement for it:
a mismatch inside `T_stale` is not evidence of anything, and a mismatch outside
`T_stale` is. Recording the `binding_epoch` view alongside `AttestRow.eligible`
would collapse `T_stale` to zero and turn the cross-check into a proof; that is
Open question 1.

### (i) Parameters proposed for D16

| Parameter | Default | Source |
|---|---|---|
| Binding cache staleness bound `T_stale` | 30 s | this record (e), tied to D27's witness epoch length |
| Binding cache entries (per gateway) | 65 536 | this record (e); precedent `MAX_REPORT_LIMITER_ACCOUNTS = 4096` |
| Bound NodeIds per account | 8 | this record (g) |
| Binding events per account | 8 / 24 h, 64 / 30 d | this record (g) |
| Binding history retention `T_history` | **owner decision** — 90 d recommended | this record (g), Open question 2 |

## Consequences

- **The gateway half of D28 clause (e)'s party-exclusion row closes; the
  coordinator half does not.** A NodeId bound to a party's account but not
  connected to this gateway is now excluded from `E(I)`. What remains
  *approximated* is D28's other named miss — "collusion across *different*
  paid accounts" — which no account↔NodeId map can ever answer, because those
  are genuinely different identities that genuinely paid. D28's row should be
  read as split rather than closed, and the sibling standing record is where
  the cross-account half goes if it ever goes anywhere.
- **Cross-coordinator dedup is barely touched, and the reason is worth
  stating.** D28 clause (e) grades one-slot-per-account as "enforced, within
  one coordinator", missing "a Sybil whose NodeIds are split across coordinator
  incarnations or regions". A reverse index does not help: a coordinator dedups
  the pool it can see, and a NodeId connected elsewhere is not in that pool to
  take a slot. The real residual is *across incarnations* — a failover whose
  successor rebuilds sessions — and it is bounded by the reseed cooldown
  already in D28 clause (g).
- **Capability deferred: nothing here builds `orrery_identity`.** This record
  decides bytes, directions and semantics. The writer is still absent, and
  until it exists every `d` row is empty, every tier-2 lookup misses, and (f)
  means the gateway excludes every announced NodeId it is not directly
  connected to. **That is a real behaviour change on an empty subspace**, and
  it is why the enforcement switch defaulting off matters more after this
  record than before it: turning enforcement on with no identity service, under
  (f), routes attested intents to D29's provisional path wholesale.
- **Capability deferred: `E(I)` reconstruction is exact only up to `T_stale`.**
  See (c) and Open question 1.
- **A keyspace test grows by four rows, not one.** Clause (a) requires `m`,
  `o` and `r` to join `d` in `all_key_families_are_range_disjoint`. A record
  that added only its own byte would leave the guard claiming fourteen while
  the tree holds eighteen.
- **The one-byte family space is down to two clean bytes** (`y`, `z`) for
  `strike/`, `jarchive/`, `section_pin/` and `coord/leader`. The next family
  record has to either adopt sub-discrimination as this one does or open the
  question of a two-byte family space. Saying it here is cheaper than
  discovering it at the fourth of those four.
- **`lease_key` overlaps the ledger family's byte without its discriminator
  discipline** (Context). Latent — no scan crosses it — but it should be an
  issue rather than a paragraph in someone's memory.
- **Memory on the gateway grows by ≈ 6.3 MB** and by nothing on the
  coordinator, which reads nothing.
- **No new FoundationDB operation on the intent path**, and no new round trip:
  the tier-3 fill is off-path by construction (e).
- **An identity outage does not stop play.** Tier 1 keeps answering for every
  connected peer, tier 2 ages out at `T_stale`, and the degradation is toward
  exclusion, which (f) makes safe. Consistent with `docs/09:175`'s grace rule.

## Alternatives considered

- **Fail open on a miss** — today's behaviour, and the honest statement of the
  status quo. Rejected on (f)'s arithmetic: it does not lower the capture
  probability, it lowers the *price* of capture from five accounts to one
  account plus four free keypairs, which is D10 item 5's Sybil anchor removed.
  The gateway's own code already argues against it in the comment this record
  quotes.
- **Fail closed, but as a refusal rather than a demotion.** Simpler, and wrong
  in the same direction D27 clause (e) already ruled on: "in all three cases
  the failure mode is provisional commit, never refusal and never silent full
  admission". Refusing would also make an empty `id/` subspace — the state on
  the day this lands — an outage for every attested intent rather than a
  detour through D29.
- **Put the account beside each NodeId in the announcement.** Genuinely the
  cleanest answer, and it would make the gateway's miss set *empty*:
  `WitnessEpochClaimsV1` gains `candidate_accounts: Vec<AccountId>` parallel to
  `candidates`, signed by the coordinator that already knows every candidate's
  account from its own tokens. Cost is `8 B × ≤ 32` = ≤ 256 B per announcement,
  a `PROTOCOL_VERSION` bump, and an amendment to accepted D28. Not rejected —
  **deferred**, and it is Open question 3, because it is a wire change and this
  record does not need one to be correct. It would also not remove the need for
  `id/`: the durable history (c) and the audit cross-check (h) are not
  announcement concerns.
- **A second family byte for the reverse index.** Same lookup cost, one more
  byte out of three, and it separates rows that must be written in one
  transaction. Rejected in (b).
- **Reverse resolution by scanning `id/`.** Rejected in (b): O(accounts) per
  lookup is not a lookup, and it would put a full-subspace range read on a path
  D16 gives 10 ms.
- **Route the gateway's read through the identity service.** Rejected in (d):
  it adds a service dependency to the intent path and makes an identity outage
  a play outage, which `docs/09:175` explicitly forbids.
- **Current bindings only, no history** — the shape D27 clause (f) says it
  would then have to live with permanently. Rejected in (c): the audit bound
  would become permanent, and the incremental cost of the log at realistic
  rates is ≈ 3 GB.
- **Historical bindings for exclusion.** Rejected in (g) on three counts, the
  decisive one being that it punishes a re-binding compromise victim while
  buying nothing (f) does not already provide.
- **Key the history by account rather than by node.** Rejected in (b): the
  audit's question runs node→account, and an account-keyed history answers it
  only by scanning.

## Resolved questions

Questions 1–4 were resolved by owner-delegated decision on 2026-08-21 and are
recorded here as part of the accepted record. The full reasoning, including what
was rejected and the condition that would reverse each, is on PR #225.

1. **Recording the binding view with `AttestRow` — no.** A `Vec<u32>` of
   `binding_epoch` parallel to `AttestRow.eligible` is **not** added, and D27
   clause (f)'s artifact is not amended for this. Two reasons, the second
   decisive: resolved question 3 subsumes it — an announcement that carries
   accounts makes the eligibility view coordinator-signed and epoch-frozen,
   which collapses the `T_stale` residual at the source rather than recording it
   after the fact. And on its own it would buy little: `AttestRow` is GC'd with
   the intent's own row at `INTENT_ROW_RETENTION_MS`, **one hour**
   (`intent/fdb.rs:86`, applied at `:927`), so a durable `binding_epoch` vector
   would outlive nothing. **Reverses if** question 3 is rejected or deferred
   indefinitely.

2. **`T_history` = 90 days, hard delete at expiry, with a write-time fold.**
   `da` additionally carries `binding_event_count: u32` and
   `first_event_ms: u64`, maintained in the same transaction that appends a `dh`
   row. Expiry is then a pure range delete with no read-modify-write, and the
   lifetime-churn signal — the thing a dispute actually asks for — survives
   expiry at ≈ 12 B/account, ≈ 120 MB at 10⁷ accounts.

   The horizon is set by the **strike and appeal window**, not by the audit
   cross-check. Clause (h)'s cross-check cannot justify it: `AttestRow` is gone
   in an hour (see question 1), so no attestation-side artifact survives even a
   day. D16's 14-day half-life does justify it — at 90 d a strike retains
   ≈ 1.2 % of its original weight (2^(−90/14) ≈ 0.0118), so the history outlives
   every dispute that could cite it. Cost ≈ 3.1 GB realistic.

   *Rejected:* retaining forever, which makes the adversarial term unbounded in
   time as well as rate; the intent audit window, which makes (h) useless
   precisely when it would be interesting; and per-node summary rows, which
   never expire and grow at the adversarial rate rather than the account rate.
   **Reverses on** a compliance requirement with a longer statutory horizon, or
   evidence of adjudication queries reaching the 90-day boundary.

3. **Accounts go in the D28 announcement — yes, as its own amending ADR, gated
   on "before enforcement turns on".** `candidate_accounts` parallel to
   `candidates` in `WitnessEpochClaimsV1`, at ≤ 256 B/announcement.

   This overrides the recommendation in Alternatives, which said to wait for the
   next time D28's wire opens for another reason. That framing assumed a
   rolling-upgrade window worth preserving. There is none:
   `PROTOCOL_VERSION` is already **2** (`protocol.rs:35`) and the window D29
   closed is closed, so the price of a bump is at its historic minimum and does
   not fall further by waiting. It zeroes this record's miss set, which is what
   makes enforcement viable while the `id/` subspace is still filling, and it
   adds no trust — the coordinator already chooses the set.

   **Reverses if** demotion telemetry shows a miss rate below 10⁻⁴ (the miss set
   is then not worth a wire change), or if external clients appear before the
   bump lands, restoring a compatibility cost that does not exist today.
   Confidence is high on direction and **medium on timing** — the gate is the
   part to revisit.

4. **`strike/` takes its own family byte `y`, with sub-discriminator `ya` — it
   does not share `d`.** Sharing would put a second writer inside `d`, and
   clause (d)'s single-writer property is load-bearing for clause (b)'s
   atomicity: `db` must be written with `da` in one transaction, and (f)'s
   fail-closed posture makes index staleness a security property. Breaking that
   to save a byte, in the record that establishes it, is not a trade worth
   making.

   The byte budget still closes: `z` goes to `jarchive/`, and `coord/leader`
   presupposes coordinator-side FoundationDB state that clause (d) says does not
   exist by design. **Reverses if** the standing record makes identity itself
   the strike writer — in which case `d` is correct after all. That is free to
   change before any `strike/` row exists, so the standing record should decide
   its writer before it decides its byte.

## Open questions

5. **Account age past probation** — D28 clause (e) grades it *skipped* because
   it is not a token field, and `da` would now carry a `created_ms` that could
   answer it durably. Whether the gateway should read it, or whether identity
   should put an `account_age_bucket` in the token as D28 suggests, is the
   standing record's call; the token change is explicitly out of scope here.

## Consequential edits this record now requires

Accepting resolved question 3 puts a load-bearing sentence in clause (f) on a
countdown: it cites "the direction D27 clause (e) already chose for
`UnknownEpoch` and `EpochStale`", and #208's amending record reverses that
direction. The demotion in (f) survives on its own legs — it rides
`LowPopulationEpoch` with an announcement in hand — but the citation must be
re-pointed when #208 lands, not left to rot.
