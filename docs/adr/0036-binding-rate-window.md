# ADR-0036: The binding-rate window row `dw`: clause (g) becomes answerable inside `d`, and its adversarial bound is corrected

**Status:** Accepted · **Date:** 2026-08-22 · **Decision:** D36

> **This record amends the accepted [ADR-0031], in two distinct ways that are
> recorded differently.** It applies **one erratum**: the storage arithmetic
> in D31's Context priced the adversarial case against only one of clause
> (g)'s two caps, and the published ≈ 490 GB figure overstates the true bound
> by 3.75× — the record was wrong about its own number. And it makes **one
> amendment**: clause (a)'s `d` family gains a fourth ASCII sub-span, `dw`,
> holding the per-account event window without which clause (g)'s rate cap
> cannot be evaluated at all — something changed. No decision text in D31 is
> rewritten; the erratum travels as a bracketed annotation applied to the
> citation site in this same branch, and the amendment lives here.

This decision is normative once accepted. See the [ADR index](../DECISIONS.md)
for precedence, scope, and the complete decision set.

**Supersedes:** nothing. It **repairs the defect [#255] filed**: a clause of an
accepted record that cannot be implemented against that same record's keyspace.
It applies the sub-discrimination practice [ADR-0031] clause (a) itself
establishes — and that [ADR-0032] clause (c) states as a rule and
[ADR-0035] applies to its first real case — inside the family this cap
belongs to. It resolves none of D31's open questions and touches no other
record's decisions.

Out of scope, owned elsewhere: any edit to Rust source — the implementing
change lands against this record in `orrery_identity` and `keyspace.rs`, whose
lanes are held by others, and clause (d) specifies it rather than making it;
the adoption of D16 parameter rows (clause (f), an observation for the owner);
the strike ledger and standing machinery ([ADR-0033]); the ramp inventory
([ADR-0032]); whether identity itself ever files strikes (D31 resolved
question 4's reversal condition).

## Context

### 1. The defect, verified

D31 clause (g) (`0031-id-account-subspace.md:488-491`) sets a rate cap:

> Binding events are capped at 8 per account per rolling 24 h and 64 per
> rolling 30 d, refused at identity, and the cap is a D16 row.

That is an account-scoped, time-windowed question: *how many binding events
has account A filed recently?* The same record's clause (a) keys the history by
node — `dh ‖ node:[u8;32] ‖ versionstamp:[u8;10]`, 44 B, with the account in
the *value* (`:180`) — so answering it from `dh` means scanning every node's
span and filtering on values: O(all binding history), on the mint path. The
node-keying is not the mistake; clause (b) argues it explicitly because the
audit's question runs node→account (`:219-226`). The mistake was asserting a
cap whose evaluation needs a second index the record never defined.

Nothing else in the landed keyspace answers it either:

- `AccountRow.binding_event_count` (resolved question 2, `:638-643`) is a
  lifetime counter, never decremented — it answers "how many ever", never
  "how many since". Its companion `first_event_ms` says when churn began,
  not how much of it was recent.
- `db ‖ node` holds current bindings only; unbinding deletes the row
  immediately (`docs/09-services-and-ops.md:175`), so events that have already
  been unwound are invisible from it.

[#254] enforced what the keyspace could carry:
`MAX_BOUND_NODES_PER_ACCOUNT = 8` (`keyspace.rs:1062`) is checked inside the
bind transaction (`crates/orrery_identity/src/fdb.rs:276-281`). That is a cap
on *concurrently bound nodes*. It does not bound the append rate: a bind/unbind
cycle appends two `dh` rows, stays at or under eight concurrent forever, and
repeats at whatever pace the credential holder likes. The concurrency cap and
the rate cap constrain different degrees of freedom, and #254 was right not to
invent a key to fake the second one.

The consequence is stated by D31 itself: "**append-only without a write cap is
an unbounded storage amplifier with a free trigger**" (`:282-284`). With the
rate cap inert, that is the state the system is in — the amplifier has no
bound, only a concurrency limit that does not constrain the rate.

### 2. What the deployment posture already rules out

Binding the window in memory at the issuer is not merely under-specified — it
contradicts an existing ops commitment. docs/09 §3's service table names
Identity "**Stateless replicas (≥2)** behind the well-known address" (:13),
and §7 provisions "2 identity replicas" (:76), "replicas in each region"
(:85). Per-process windows multiply the effective caps by replica count under
round-robin and fragment them unpredictably otherwise; a restart or deploy
resets them, so the window loses meaning exactly when load is highest; and the
durability premium is nearly zero, because the check can ride the transaction
that already reads and writes `da`, `db` and `dh`. An enforcement structure
whose strength silently depends on replica count fails the way D31 clause (f)
says failures must not fail.

### 3. The byte budget, recounted

The free list per [ADR-0032] clause (c)'s rule — lowercase bytes minus
`registered_families()` minus every byte an accepted record allocates —
recomputed from the tree:

```
lowercase bytes                              26
taken as registered families                18   a c d e f g i k l m n o p r s u v w
                                                 (registered_families, keyspace.rs:2665-2764)
in use as exclusive range ends               6   b h j q t x
                                                 fence→b attest→h intent→j
                                                 seedprog→q seedmap→t world→x
allocated by accepted records                2   y → strike/, z → jarchive/
                                                 ([ADR-0031] resolved question 4)
cleanly free                                 0
```

This matches [ADR-0035]'s independent recount (Context §4) and closes every
route but one: a new key kind takes an ASCII sub-discriminator inside the
family whose writer, transaction profile and retention it matches, or opens
the two-byte space through a dedicated ADR. One honesty note, since this
record leans on the pattern: [ADR-0032] — where the *rule* is written — is
still Proposed. The practice does not wait on it: accepted [ADR-0031]
clause (a) sub-discriminates `d` itself, the `l` family has done it in landed
code since before either record, and [ADR-0035] applies the rule to `le`.
Three records and the tree all point the same way; this record takes the
unanimous tine.

### 4. Where the window belongs, and why not somewhere cheaper

Host family `d`, on every axis the allocation rule names:

- **Writer.** Identity is the sole writer of every `d` row (clause (d)), and
  the rate check must run in the bind transaction — the same transaction that
  stages `da`, `db` and `dh` — or two concurrent binds for one account both
  pass a stale count. Sharing the family is not merely permitted here, it is
  the requirement: the coupling [ADR-0032]'s rule warns against is a *foreign*
  writer inside a transactionally-coupled family, and this writer is the
  family's own.
- **Transaction profile.** The bind path already read-modify-writes `da`
  against a deliberately non-snapshot read whose conflict range serializes
  concurrent binds on one account (`fdb.rs:159-161`). A fourth row read the
  same way adds no new contention class.
- **Retention.** Both kinds are time-bounded expiries of an append-only log —
  `dh` by sweep at `T_history`, the window by self-pruning at write time
  (clause (c)). Different horizons, one mechanism shape.

Why a sibling row and not fields on `da`: `AccountRow` is sized to be safe to
read whole — eight inline NodeIds, ~282 B, and that property is load-bearing
(`store.rs:80-82`). Every token issuance deserializes the row whole
(`service.rs:273`); bloating it by up to ≈ 500 B of window stamps taxes login
QPS to serve a query minting never asks, and mixes a decaying structure into a
row that never expires. The window row is touched only by bind and unbind.

## Decision

### (a) Allocation: `dw`, the window sub-span of `d`

> **The binding-rate window occupies the sub-span `[b"dw", b"dx")` inside the
> registered `d` family — no new family byte is spent. Byte `d`'s registry row
> is unchanged; the implementing change extends the sub-span assertions
> instead, exactly as [ADR-0032] clause (c) requires of `v`'s `vr`.**

```text
dw ‖ account:u64 BE                              10 B → postcard Vec<u64>,
                                                     ascending event timestamps (ms),
                                                     ≤ 64 entries — see (b)

window_range_start() = vec![b'd', b'w']
window_range_end()   = vec![b'd', b'x']     // one past 'w'; house style
```

Position: `dw` sorts above the entire history span —
`binding_history_range_end()` is `[b"d", b"i"]` (`keyspace.rs:1246`) — and
below the family end `[b'e']` (`:1199`), in unoccupied space. No production
code ranges over the whole family `[b"d", b"e")`; the only such scans are in
`keyspace.rs` tests (`:2303`, `:2527`), which assert disjointness and will
learn the new span. `'w'` is chosen for the mnemonic and to keep hand-written
bounds away from `dh`'s end bound, for the same reason [ADR-0035] clause (a)
refuses `'s'` beside `lr`: boundaries that mean two things are how this
defect class breeds. Letters `c..g` were left as deliberate gaps by the
landed bounds; `i..v` remain free after this record.

When [ADR-0035] clause (c)'s pair-modeling guard lands, `dw` registers as a
`(byte, discriminator)` sub-kind like every other discriminated constructor;
its builder must be written in the scanner-recognizable idiom (byte-0 literal
immediately before byte-1 literal) so the completeness scan sees it. Until
then, the minimal obligation that holds regardless: extend
`id_keys_have_the_widths_and_discriminators_d31_specifies` and
`id_sub_spans_are_ordered_disjoint_and_inside_the_family` with the `dw` row —
width 10, prefix `b"dw"`, pairwise-disjoint against `da`/`db`/`dh`, inside
`[b"d", b"e")`.

### (b) Window semantics: exact rolling windows, both directions, one transaction

> **The value at `dw ‖ account` is the ascending vector of `at_ms` stamps of
> every binding event the account filed within the trailing 30 days. A
> *binding event* is every staged `dh` row — `BindKind::Bind` or
> `BindKind::Unbind` alike, which is clause (c)'s own definition of an event.
> Before staging any writes, the bind/unbind transaction reads the row
> non-snapshot, prunes entries older than now − 30 d, refuses when the
> trailing-24 h count ≥ 8 or the trailing-30 d count ≥ 64, and otherwise
> appends the current stamp and writes the row back. Refusals stage nothing,
> consume nothing, and leave the vector untouched.**

The four properties worth stating plainly:

1. **Exact, not bucketed.** Rolling windows computed from stored stamps are
   exact; fixed buckets admit boundary-straddled bursts. At ≤ 64 × 8 B there
   is no size argument for approximation.
2. **Both directions count.** Clause (c) defines events as bind-or-unbind
   appends and clause (g) caps "binding events", so both refuse at the cap.
   Refusing an unbind leaves the binding in place — the transaction aborts
   wholesale — and delays device removal by at most a window slide; exempting
   unbinds would loosen the worst-case bound twofold while protecting nobody,
   because an unbind requires a live binding and bindings are capped anyway
   (Alternatives). Re-binding a pair that already holds is
   `BindOutcome::AlreadyBound` and appends no `dh` row (`store.rs:33-37`,
   `fdb.rs:264-270`), so it consumes nothing — the free-rebind loop is already
   closed upstream.
3. **Serialized by construction.** Same-account mutations already serialize
   through `da`'s read-modify-write conflict range (`fdb.rs:159-161`); reading
   `dw` non-snapshot joins the same discipline rather than being load-bearing
   for it. Distinct accounts touch disjoint rows and do not contend.
4. **One error, named.** Refusal raises
   `IdentityError::BindingRateLimited { account, window_ms, cap }` naming
   which window tripped — the 24 h window checked first when both would trip —
   alongside the existing refusal taxonomy. Check order inside the
   transaction stays: unknown account → already-bound / bound-elsewhere /
   not-bound → too-many-bound-nodes → **rate-limited** → stage writes.

Clock note, so nobody over-trusts it: window arithmetic runs on the `at_ms`
identity supplies, the same trust level as the `first_event_ms` fold of
resolved question 2. Skew between replicas is bounded by fleet NTP discipline
and measured in milliseconds against hour-long and month-long windows.

Migration: none, deliberately. Clusters that predate enforcement may hold
over-window histories; their first post-enforcement events refuse until the
excess ages out within one 30-day window. That is fail-closed, matches the
house posture for an empty subspace filling under clause (f), and costs
nothing — honest accounts sit orders of magnitude below the cap (D31 prices
realistic churn at 0.05 events/account/day).

### (c) Cost and retention: the structure is bounded by what it enforces

The window log's self-bounding property is the reason it cannot become its own
amplifier: the 30-day cap refuses the 65th in-window event, so the vector
holds **≤ 64 entries at all times**, each 8 B — the enforcement structure
costs O(cap) per account, no more.

```
key        dw ‖ account            10 B
value      postcard Vec<u64>       ≤ 1 + 64×8 = 513 B
row, saturated                     ≈ 530 B with FDB overhead
                                   (D31's convention: 61 raw → "call it 68")
ceiling    10^7 accounts saturated × 530 B          ≈ 5.3 GB
realistic  mean entries = 0.05/day × 30 d = 1.5
           ⇒ ≈ 30 B/account ⇒ 10^7 accounts          ≈ 0.3 GB
```

Pruning happens at write time; no background sweep is required, and identity —
stateless replicas behind FDB — is given none. One residue is stated rather
than hidden: an account that goes permanently quiet keeps the stamps written
by its last event until the next one prunes them, so the ceiling includes
churn-and-vanish accounts. If the owner wants that reclaimed, the pass that
performs `dh`'s `T_history` range delete may clear any `dw` row whose newest
stamp is ≤ now − 30 d — such a row can never answer differently than an
absent row, so the sweep is idempotent and needs no read-modify-write. Who
runs `dh` expiry is unspecified in the accepted record; this record does not
assign it either (Open question 1).

The `dh` retention sweep is untouched: it ranges over
`[binding_history_range_start(), binding_history_range_end())` = `[dh, di)`
and cannot reach `dw` any more than it reaches `da`.

### (d) Enforcement posture: always-on at identity, parity in both stores

> **The rate cap is not a ramp control. It joins `MAX_BOUND_NODES_PER_
> ACCOUNT` — which [#254] landed unconditional — as always-on input
> validation: it guards the storage amplifier, punishes nobody the honest
> cohort ever hits, and has no false-positive cohort to protect, which is the
> whole content of a shadow period ([ADR-0032] clause (h)). The C1–C5
> inventory observes intent-path verdicts; identity mints are not among
> them.**

Parity is mandatory, not stylistic: `MemAccountStore` — the harness store —
enforces the identical window from a fourth map under the same lock, with the
refusal computed before any map is mutated, preserving its documented
no-side-effects-on-refusal property (`mem.rs:7-8`). A harness store that
enforces less than the durable one lies to every gate that uses it.

Test obligations for the implementing change:

- the issue's criterion verbatim — a 9th event inside 24 h is refused;
- a 65th event inside 30 d is refused while each day's 8 stay admitted;
- stamps older than a window stop counting (prune correctness);
- `AlreadyBound` consumes nothing; refused binds and unbinds consume nothing;
  an unbind consumes one slot;
- `fdb`-gated: the durable 9th-event refusal, and the all-or-nothing test
  extended to assert `dw` unchanged across an injected abort
  (`fdb.rs:558-608`).

### (e) The restated adversarial bound — and the erratum

D31's Context priced the adversarial case "capped at clause (g)'s 8/day":
`10^7 × 8 × 90 = 7.2×10^9 rows × 68 B ≈ 490 GB`. That line uses only the
24-hour cap. Clause (g) sets two caps and both hold together: 64 per rolling
30 days bounds any 90-day retention horizon at three disjoint 30-day windows,
so **≤ 192 events per account survive retention** — and 192 is tight, not
merely an upper bound: spreading uniformly at 64/30 d ≈ 2.133/day puts
exactly 64 events in every interior sliding 30-day window and ≈ 2.13 in every
24-hour window, satisfying both caps simultaneously.

```
retained events per account       min(8/day, 64/30d) × 90 d → ≤ 192
rows                              10^7 × 192           = 1.92×10^9
storage                           1.92×10^9 × 68 B     ≈ 131 GB
published figure                  7.2×10^9  × 68 B     ≈ 490 GB   (24 h cap only)
overstatement                                          720/192 = 3.75×
```

The published figure was a valid upper bound — overstating a storage bound is
the safe direction — but it was wrong as a statement of what clause (g)'s caps
buy, and capacity planning deserves the tighter number. This record applies an
erratum annotation to the block in D31's Context; no other text there changes.

With enforcement live, the amplifier sentence survives in corrected form:
**append-only without a write cap is an unbounded storage amplifier; the two
caps bound it at ≈ 131 GB of `dh`, plus ≈ 5.3 GB absolute ceiling of `dw` —
the enforcement structure costing two orders of magnitude less than the thing
it bounds.**

### (f) Parameters: none new, several still owed

Cap values are unchanged — 8 / 24 h and 64 / 30 d are accepted clause-(g)
policy and this record does not reopen them. No tunable is added: the log's
horizon *is* the longer window, and everything else derives. Two observations
belong to the owner rather than here:

- D31 clause (i) proposed five D16 rows, including these caps, and clause (g)
  asserts "the cap is a D16 row"; verified today, none of the five has landed
  in `0016-parameter-reference.md` — the assertion remains outstanding
  regardless of this amendment.
- Whether refusing unbinds at the cap should be loosened to binds-only is a
  product-visible choice this record declines to make silently; Alternatives
  prices it.

## Consequences

- **For the implementer, in order:** `keyspace.rs` grows
  `binding_window_key(account) -> [u8; 10]`, `binding_window_range_start()`,
  `binding_window_range_end()`, and the test extensions of clause (a);
  `orrery_identity` grows the `BindingRateLimited` variant, the shared
  prune/check/append logic, enforcement in both stores, and the tests of
  clause (d). No signature changes: `bind`/`unbind` already take `at_ms`.
- **No hot-path change anywhere else.** Token minting reads `da` only; the
  gateway's three tiers are untouched; the bind transaction adds one point get
  and one point set inside a round trip it already makes.
- **The guard story ends consistent:** byte `d` stays registered once;
  sub-spans go from three to four; when [ADR-0035] clause (c) lands, the
  discriminated-constructor floor rises again.
- **The erratum annotation lands in D31's Context with this record** —
  announced here, applied there, changing no ruling, in the manner
  [ADR-0029]'s annotations set.
- **DECISIONS.md gains the D36 row.**
- **AGENTS.md's decision-table row is owed by whoever holds that lane**, as
  [ADR-0032]'s Consequences already recorded.

## Alternatives considered

- **A second, account-keyed history index** (option 2 of [#255]). Answers the
  question exactly and doubles the append volume — the very append the cap
  exists to bound — and re-opens the consistency burden clause (b) rejected
  ("a second history is a second thing to keep consistent"). The hot path
  needs a *count*, not a history; a bounded summary serves it. Rejected.
- **Bound it in memory at the issuer** (option 3). Rejected on three
  independent grounds: docs/09 §3 mandates ≥ 2 replicas and §7 provisions
  them, so per-process windows are wrong by deployment rather than by
  assumption; restarts reset the window precisely when an attacker would use
  it; and the durable version rides a transaction that already exists, so the
  cheapness argument buys nothing.
- **Retract or restate the cap** (option 4). Unsound, not merely undesirable:
  the concurrency cap provably does not bound the append rate — a bind/unbind
  cycle appends two rows per iteration at unbounded rate while remaining at
  ≤ 8 concurrent forever — so dropping the rate language falsifies the
  amplifier sentence instead of freeing the record from it. The half of this
  option that survives is the arithmetic correction, taken as the erratum.
- **A lifetime cap via the existing `binding_event_count`** — not among
  [#255]'s options, and worth recording because it is the one zero-keyspace
  alternative: the counter already folds in-transaction, so "refuse at N
  lifetime events" ships today. Rejected because it changes the accepted
  invariant's shape, bricks the supported compromise-recovery flow
  (`docs/09:175`: "account NodeId compromise = user re-binds") at N events,
  and prices worse at any generosity level — sustaining the accepted caps'
  allowance for ten years is ≈ 7 800 events/account, ≈ 5.3 TB at 10⁷
  accounts, forty times the corrected bound. A storage parameter that decides
  whether victims can re-bind is a product decision wearing a byte budget.
- **Bucketed counters** (hourly/daily cells instead of stamps). Approximate —
  boundary straddling admits up to ~2× the intended burst within a window —
  larger as a static row, and exactness is available at the same price.
  Rejected.
- **Exempt unbinds from the cap.** Loosens the retained-row bound twofold
  (unbinds ≤ lifetime binds still hold, giving ≤ 384 rows/account per horizon
  ≈ 261 GB), protects nobody — an unbind requires a live binding and bindings
  are capped — and re-reads "binding events" against clause (c)'s plain
  definition. Listed for the owner as a visible future decision, not made
  here.
- **Order the window by versionstamp.** Impossible pre-commit: versionstamps
  do not exist until the transaction commits, and the check must run before
  anything is staged. `at_ms` is the available time base, and the trust level
  is the one resolved question 2 already accepted for `first_event_ms`.

## Open questions

1. **Who runs `dh`'s `T_history` range delete** — and therefore whether the
   optional `dw` sweep of clause (c) rides it — is unspecified in the accepted
   record. Resolved question 2 fixes the mechanism ("a pure range delete with
   no read-modify-write") and leaves the runner open; identity is stateless
   replicas, persistd runs the maintenance sweeps, and neither record assigns
   it. Owner's call before P5 enforcement turns on.
2. **Adopting clause (i)'s five proposed D16 rows**, including the caps this
   record enforces, so that "the cap is a D16 row" becomes true rather than
   intended. Owner's call; mechanical once taken.

[#255]: https://github.com/baadc0de/orrery/issues/255
[#254]: https://github.com/baadc0de/orrery/pull/254
[ADR-0029]: 0029-low-population-path.md
[ADR-0031]: 0031-id-account-subspace.md
[ADR-0032]: 0032-enforcement-ramp.md
[ADR-0033]: 0033-strike-ledger-standing.md
[ADR-0035]: 0035-lease-key-discriminator.md
