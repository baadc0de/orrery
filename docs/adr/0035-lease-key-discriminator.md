# ADR-0035: The lease registrar row takes the `le` discriminator inside `l`, and the disjointness guard learns sub-spans

**Status:** Accepted · **Date:** 2026-08-22 · **Decision:** D35

This decision is normative once accepted. See the [ADR index](../DECISIONS.md)
for precedence, scope, and the complete decision set.

**Supersedes:** nothing. It **resolves the record half of [#226]** — the
on-disk key format change and the guard's blind spot — which that issue filed
after [ADR-0031]'s Context surfaced the finding; the implementing change lands
against this record, not instead of it. It **applies [ADR-0032] clause (c)'s
allocation rule** to its first real case, and **extends the completeness
mechanism [#234] built**
(`every_family_prefix_written_in_this_module_is_registered`) from prefix
bytes to `(byte, discriminator)` pairs.

Out of scope, owned elsewhere: any edit to
`crates/orrery_persistd/src/keyspace.rs` or its callers (the #226
implementation); the two-byte family-space question [ADR-0032] already prices
and defers; `ramp/`'s `vr` sub-span ([ADR-0032] clause (c), still Proposed);
`m` and `o`'s key shapes, which have no defect (Context §3).

## Context

### 1. The defect, verified at `c190639a`

`lease_key(grid, entity)` — the durable registrar row of docs/08 §6,
`lease/{entity_id}` (`docs/08-persistence.md:3229`) — is:

```text
keyspace.rs:210-216
[0u8; 13]:  key[0] = b'l';   key[1..5] = grid.0 BE;   key[5..13] = entity.0 BE
            family byte      byte 1 = the grid id's
                             most significant byte
```

Every ledger key puts an ASCII discriminator at byte 1
(`keyspace.rs:1421-1424`: "discriminated by the second byte so range scans of
one kind never see another"):

| Builder | Bytes | Byte 1 |
|---|---|---|
| `ledger_bal_key` (`keyspace.rs:1431-1438`) | 18 | `b'b'` (0x62) |
| `ledger_item_key` (`keyspace.rs:1444-1450`) | 10 | `b'i'` (0x69) |
| `ledger_receipt_key` (`keyspace.rs:1461-1467`) | 12 | `b'r'` (0x72) |

A lease row's byte 1 is the **most significant byte of `grid.0`**, so a lease
row sorts inside a ledger sub-span exactly when that byte collides:

| `grid.0` range | Width | Lease row lands in |
|---|---|---|
| `[0x6200_0000, 0x6300_0000)` | 2²⁴ = 16 777 216 grids | `ledger/bal/` span `[lb, lc)` |
| `[0x6900_0000, 0x6A00_0000)` | 16 777 216 | `ledger/item/` span `[li, lj)` |
| `[0x7200_0000, 0x7300_0000)` | 16 777 216 | `ledger/receipt/` span `[lr, ls)` |

Exposed fraction of the grid space: `3 × 2²⁴ / 2³² = 3/256 ≈ 1.17 %`. The
first colliding grid id is `0x6200_0000` = 1 644 474 368.

### 2. What moved under the issue, and what it got wrong

The issue was written against the tree at `23061449`, before [#234] landed.
Verified against current HEAD:

- **Its second defect is fixed.** The registry (`registered_families`,
  `keyspace.rs:2665-2764`) now holds **eighteen** families — `a c d e f g i k
  l m n o p r s u v w`, including the `m`, `o` and `d` rows the issue found
  missing — and [#234] added the two-source completeness test
  `every_family_prefix_written_in_this_module_is_registered`
  (`keyspace.rs:2901-2935`), which fails when a constructor writes a prefix
  byte the table does not name. The "fourteen of seventeen" arithmetic is
  history.
- **But its central premise about scans is false as stated.** "No full-family
  range scan of either exists today" is true only of `persistd` itself. Two
  harnesses range-scan a ledger sub-span **today**: `read_receipts` in
  `gates/p5-dupe-gauntlet/src/main.rs:903-908` and `read_receipt_intent_ids` in
  `gates/p3-siblings/src/race.rs:706-718` both scan `[lr, ls)` — begin =
  `ledger_receipt_key()`, exclusive end `[l, s)` (`end[1] = b's'` at
  `main.rs:905`, `race.rs:708`) — and postcard-decode every
  row in range. A lease row with `grid.0 ∈ [0x7200_0000, 0x7300_0000)` on such
  a cluster would abort the nightly P5 gate with a decode error pointing the
  operator at corruption that is not corruption. The consumer class the issue
  says does not exist, exists.
- **What actually keeps this latent is grid reachability, not scan absence.**
  Every grid id written anywhere in the tree has byte 1 = 0x00:
  `GridId::ROOT = GridId(0)` (`crates/orrery_protocol/src/grid.rs:17`) is the
  production path's and `gates/p3-siblings`' grid (`peer.rs:329`), and the P5
  gauntlet's `GRID = GridId(151)` (`gates/p5-dupe-gauntlet/src/main.rs:44`) is
  equally far below `0x62`. **Rows at risk today: zero** — not because nothing
  reads across the boundary, but because no code path writes a colliding row:
  every `GridId` in the tree is a compile-time constant (`ROOT` = 0, the P5
  gauntlet's 151, single-digit test ids), and no dynamic grid-id allocator
  exists yet.
- Its line numbers drifted (`all_key_families_are_range_disjoint` was :1849;
  the machinery now lives at :2665/:2780/:2901), and its quote of the guard's
  comment describes the pre-[#234] text. Its key-shape claims are exact.

So the corrected statement of urgency: the defect becomes live at the moment
any cluster holds a lease row for a grid id ≥ `0x6200_0000` — which no current
allocator mints, but nothing prevents — and #224's economy-wide conservation
sweep (open, gated by [ADR-0032] clause (g) before C3 promotes) is the
consumer that makes full-family `lb`/`li` scans routine rather than
harness-local. Fixing the format before that sweep exists remains cheaper than
debugging it afterwards; the issue's own framing survives, on corrected
premises.

### 3. What the guard cannot see, precisely

[#234]'s completeness clause says it out loud (`keyspace.rs:2894-2899`): the
source scan yields a set of *prefix bytes*, so "a second family that reuses an
already-registered byte without an ASCII sub-discriminator is invisible to
it", and `lease_key_overlaps_the_ledger_family` (`keyspace.rs:2949-2968`)
carries this one instance by name. That recording test asserts the overlap
exists — it is a tripwire against an undiscussed "fix", not coverage of the
class. A future constructor writing `key[0] = b'd'; key[1] = <literal>`
tomorrow passes every test in the module. The class needs the guard to model
pairs, and clause (c) costs that.

One scope fact worth recording so nobody over-fixes: `lease_cell_key` (`m`,
`keyspace.rs:219-226`) and `lease_location_key` (`o`, `:233-238`) put the same
raw grid high byte at their byte 1 and are **not** defects. A collision needs
a *second key kind sharing the family byte*; `m` and `o` are single-kind
families whose entire spans they own, so there is nothing inside to collide
with. The disease is cohabitation without discrimination, and `l` is its only
host.

### 4. The byte budget closes, recounted

> **Amended 2026-09-02 (owner-authorised), on the acceptance of [ADR-0051].**
> D51 withdraws the `k` family (`chunk/`, the v1 terrain allocation that no
> writer ever populated), so the recount below drops from eighteen registered
> families to seventeen and its last line goes from zero to **one clean prefix
> byte**. The numbers in the block are corrected in place; the record of what
> they were is this note. (One thing the block does not re-sort: `y` and `z`
> have since landed as registered families — `strike/` and `jarchive/` — and
> the block still counts them once, on the accepted-allocation line, so the
> sum is the same whichever line they sit on; `registered_families()` in the
> tree therefore lists nineteen prefixes today, seventeen of which are the
> line above.) The recovered byte is **not pre-spent** — D51 §(c)
> leaves it to the normal allocation decision [ADR-0032] clause (c)'s rule
> requires — and nothing in this record's decision moves: the lease registrar
> row is landed as `le` inside `l`, and it was [ADR-0032]'s rule directing
> sub-discrimination, not only the arithmetic, that put it there. What this
> amendment changes is the *second* sentence of the closing paragraph: the
> own-prefix-byte option is no longer "closed by arithmetic"; it is closed by
> this record's accepted decision, and reopening it would be spending the
> recovered byte, which is a dedicated allocation decision and not a lease
> question.

[ADR-0032] clause (c)'s allocation rule — free list = lowercase bytes minus
`registered_families()` minus every byte an accepted record allocates or
earmarks — recomputed from the tree and the accepted record set alone:

```text
lowercase bytes                              26
taken as registered families                17   a c d e f g i l m n o p r s u v w
                                                 (keyspace.rs; k withdrawn by [ADR-0051])
in use as exclusive range ends               6   b h j q t x
                                                 fence→b (:183)  attest→h (:910)
                                                 intent→j (:495) seedprog→q (:333)
                                                 seedmap→t (:299) world→x (:82)
allocated by accepted records                2   y → strike/, z → jarchive/
                                                 ([ADR-0031] resolved question 4)
cleanly free                                 1   not pre-spent ([ADR-0051] §(c))
```

This does not depend on any proposed record: [ADR-0031] is Accepted and its
resolved question 4 earmarks both remaining bytes. **No new prefix byte
exists for a lease family.** The option the issue lists second — move
`lease_key` to its own prefix byte — is closed by arithmetic, and
sub-discriminating inside the matching family is what [ADR-0032]'s rule
directs anyway.

## Decision

### (a) The registrar row takes ASCII discriminator `e` at byte 1, inside `l`

> **`lease_key(grid, entity)` returns
> `[b'l', b'e', grid: u32 BE, entity: u64 BE]` — 14 bytes. The registrar row
> becomes the fourth ASCII-discriminated kind of the registered `l` family,
> sub-span `[b"le", b"lf") ⊂ [b'l', b'm')`. The value encoding is unchanged:
> the row stays a postcard `Lease`; only the key mutates.**

```text
lb ‖ account:u64 BE ‖ asset:u64 BE              18 B   balances   (unchanged)
le ‖ grid:u32 BE ‖ entity:u64 BE                14 B   registrar row (this record)
li ‖ item_uid:u64 BE                            10 B   items      (unchanged)
lr ‖ versionstamp:[u8;10]                       12 B   receipts   (unchanged)

order within the family:  lb < le < li < lr        ('e' = 0x65 < 'i' < 'r')
```

Why inside `l` rather than anywhere else. The budget (Context §4) leaves no
byte, and [ADR-0032]'s rule asks a sub-discriminated kind to join the family
it matches. The registrar row's writer, transaction coupling and retention
profile match its two companion indexes (`m`, `o`) — `put` writes row, cell
index and location index in one transaction
(`crates/orrery_persistd/src/lease/fdb.rs:124-126`), as do `migrate`
(`:196-225`) and `remove` (`:241-243`) — but those are three scattered bytes,
not one hostable family, and consolidating them is priced and rejected below.
Joining `l` costs no byte, keeps every existing bound intact — the receipt
scans `[lr, ls)` of Context §2 exclude `[b"le", …)` by the first byte after
`l` — and gives the pair-modeling guard of clause (c) a uniform object to
prove.

Why `'e'`: it is ASCII per the discipline [ADR-0031] draws from this very
finding ("never an id's high byte"), unused among `{b, i, r}`, and mnemonic.
Deliberately **not** `'s'`, although `'s'` would also sort cleanly: both
receipt scanners write their exclusive end as `[b'l', b's']`
(`gates/p5-dupe-gauntlet/src/main.rs:905`, `gates/p3-siblings/src/race.rs:708`), and a
lease row at `ls` would make that shipped idiom read as both "one past
receipts" and "start of leases". Boundaries that mean two things are how this
defect class breeds.

With the fix, `lease/{grid}/{entity}` joins the registry as a normal row — the
deliberately-absent comment at `keyspace.rs:2758-2763` ("it is not a family in
this table's sense") is deleted, and `l`'s entry names four sub-kinds. The
recording test inverts into the acceptance test the issue demands:

> **`lease_key_is_discriminated_inside_the_ledger_family` constructs
> `lease_key(GridId::new(0x6200_0000), …)` and asserts byte 1 is `b'e'` and
> the key does not sort inside `[lb, lc)`, `[li, lj)` or `[lr, ls)`; and that
> `lease_key(GridId::new(u32::MAX), …)` still sorts inside `[b'l', b'm')`.
> It must fail before the fix and pass after.**

### (b) Migration: no dual-read, no sweep — the no-deployed-data argument, enforced loudly

> **Old-shape rows (`b'l' ‖ grid BE ‖ entity BE`, 13 bytes) are not migrated.
> There is no dual-read, no rewrite sweep, and no runtime version gate. The
> implementing change ships instead: (1) the inverted acceptance test of (a);
> (2) a one-shot audit mode in the `fdb`-gated tier that scans `[b'l', b'm')`
> and reports every key whose byte 1 is not a registered `l` sub-discriminator
> — expected count zero; (3) this record's argument, checked into the runbook
> beside the audit mode, that any cluster holding old-shape rows is a
> development cluster and may be cleared or discarded.**

The argument, priced:

- **Deployed universes: zero.** The project is pre-launch; durable authority
  requires `--fdb-cluster-file` (`persistd.rs:799-803`) and the volatile store
  writes no FoundationDB rows at all. Old-shape rows can exist only in the
  shared development database and long-lived manual dev stores — disposable by
  policy, and re-grantable by design.
- **What an ignored old row costs, worst case, as a number.** An old-shape row
  becomes unreachable garbage: every read of the registrar goes through the
  new 14-byte key, misses, and treats the entity as unclaimed. The prior
  holder is not silently rivalled — the gateway validates presented
  `(holder, lease_id, seq, expiry)` against the registrar row
  (`docs/08-persistence.md:236`) and fences what does not validate — so the
  holder loses authority and re-claims. Leases renew every 2.5 s against a
  10 s TTL (`docs/08-persistence.md:50`): the churn window is **≤ 10 seconds
  per affected entity, cost is one claim round trip, and zero value rows are
  touched** — the fix changes no `lb`/`li`/`lr` byte, so balances, items and
  receipts are untouched by construction.
- **What breaks if this is done wrong.** If the fix ships without (a)'s test,
  nothing fails until a colliding grid exists — the exact latent-until-live
  shape the issue warns about. If a sweep were built instead and got its
  filter wrong, it would be a read-modify-write loop over the money ledger
  hunting 13-byte keys, the most expensive possible way to rescue rows this
  record has just argued are worth ≤ one re-claim. If the audit mode is
  skipped and some unknown long-lived cluster does hold old rows, the cost is
  unexplained garbage bytes plus operator confusion — never corruption, never
  double-spend, because fencing closes the only door.
- **Cost of the chosen path:** the audit mode is one bounded range read over
  `[l, m)` in the existing `fdb` test tier (per C-8, run only where wiping is
  licensed), ~40 lines; the startup hot path gains **nothing**, which is why
  no permanent in-process probe is specified — persistd booting against a
  production-sized economy must not read the whole `l` family to check for
  rows this argument says cannot exist outside dev clusters.

### (c) The guard models `(byte, discriminator)` pairs, not bytes

> **The family registry grows from one row per byte to one row per byte with
> either a whole-span marker or an ordered sub-kind table — `(discriminator,
> name)` pairs, each with a sample drawn from its own constructor. Within a
> family, declared sub-spans are pairwise disjoint and each sample begins with
> its declared pair; between families, the existing whole-prefix proof is
> unchanged. The source-completeness scan extends to recognize `key[1] =
> b'…'` literal writes in the non-test half, pairs each with the nearest
> preceding `key[0] = b'…'` literal, and asserts set-equality in both
> directions against the registry's tables, floored at the number of
> discriminated constructors known to exist — six today (`da db dh lb li lr`),
> seven once (a) lands — so scanner drift fails loudly rather than vacuously.**

This is the class closure the issue's third acceptance bullet asks for: after
it, a constructor that shares a registered family byte cannot exist without
declaring its sub-span, and two declared sub-spans cannot overlap, by test.
The mechanism is deliberately an extension of what [#234] built rather than a
second apparatus: same include-the-source trick, same two-source discipline,
same non-test-half split (`#[cfg(test)]` begins at `keyspace.rs:1547`, so the
doc comment at `:2847` that mentions the form is already excluded).

Honest limits, stated so the clause is not over-trusted: the pairing heuristic
("nearest preceding byte-0 literal") works because every discriminated
constructor assigns byte 0 immediately before byte 1 — true of all six sites
today (`:1094, :1110, :1136, :1434, :1447, :1464`) — and the floor assertion
turns any drift of that idiom into a failure rather than a silent pass. A
constructor that computes byte 1 without a literal (e.g. `copy_from_slice`)
remains invisible, exactly as today; if such a site ever lands it must go
through a named helper the scanner recognizes, and the mutation-check
obligation below is what keeps that decision honest. Per the AGENTS.md lesson,
each new clause is mutation-checked: break a guarded stage (relabel a
discriminator, delete a registry row), confirm the test fails, restore —
breaking the stage and its check together proves nothing.

When [ADR-0032] is accepted, `v` joins the same model with its single `vr`
sub-kind and the ramp rows register themselves; nothing in this clause is
specific to `l`.

### (d) Sequencing

> **The implementing change lands before #224's conservation sweep exists to
> consume full-family `l` scans, and before any code path can mint a grid id
> ≥ `0x6200_0000`. Both conditions hold indefinitely today; neither is
> guaranteed to hold forever, and the second is the one to watch — a grid-id
> allocator is a feature, and this record should be cited in its review.**

## Consequences

- **One on-disk format changes; three do not.** `lease_key` rows move from 13
  bytes at `l‖grid‖entity` to 14 bytes at `l e ‖ grid ‖ entity`. Every
  balance, item and receipt key is bit-identical before and after, so the
  migration surface is exactly the registrar row family — and per (b), zero
  deployed rows of it exist.
- **The `l` family becomes uniformly disciplined**: four kinds, four ASCII
  discriminators, provably disjoint sub-spans, no exceptions comment. The
  guard's model and the family's reality coincide for the first time since
  the registrar row landed.
- **Test-only code grows by roughly 90 lines** — registry restructure (~40),
  scanner extension (~30), sub-span proof and inverted test (~20) — with zero
  runtime footprint and no new dependency. The audit mode adds ~40 lines to
  the `fdb` tier.
- **The next family byte question is unchanged, and now has a worked
  example.** The budget stays closed (zero clean bytes); `section_pin/` and
  `coord/leader` remain documented-but-unbuilt; [ADR-0032]'s rule and this
  record's clause (c) together show what sub-discrimination costs and buys.
- **Two harness consumers become structurally safe.** After the fix, nothing
  but receipts sorts in `[lr, ls)` — provably, by the sub-span proof of (c) —
  so the receipt scanners in `gates/p5-dupe-gauntlet` and `gates/p3-siblings` can never
  observe a foreign row, whatever grid ids the future mints.
- **AGENTS.md's decision-table row is owed by whoever holds that lane** (same
  constraint [ADR-0032] recorded); the index update lives in
  [DECISIONS.md](../DECISIONS.md) only.
- **Not decided here:** the implementation itself. This record specifies the
  key shape, the migration posture and the guard mechanism; #226's
  implementation change carries them into `keyspace.rs`.

## Alternatives considered

- **Move the registrar row to its own prefix byte.** The issue's second fork.
  Closed by Context §4's arithmetic: eighteen families, six range ends and two
  accepted-record allocations exhaust the twenty-six lowercase bytes; `y` and
  `z` are spoken for by accepted [ADR-0031]. Taking a range-end byte
  (`b h j q t x`) breaks the six families whose spans end there; taking `y`
  or `z` re-opens an accepted record's budget.
- **Consolidate all three lease kinds under one host byte** — e.g. `mc`/`mo`/
  `mr` under `m`, freeing `o` for the pool. Taxonomically the cleanest answer:
  one writer, one transaction, one family, and the pool regains a byte.
  Rejected on price: it migrates three key kinds instead of one, churns
  `load_cell`'s per-shard range-scan shape on the shard-startup hot path
  (`lease/fdb.rs:38-48` decodes `m` keys positionally), and buys a byte
  nothing needs — the unbuilt remainder is already routed to
  sub-discrimination or the multi-byte ADR by [ADR-0032]'s rule, and one
  reclaimed byte does not reopen that fork. Revisit only if a fourth lease
  kind ever appears.
- **Dual-read old and new shapes forever.** Protects zero rows (there are
  none), taxes every future reader of the registrar with a two-key lookup and
  a correctness caveat about which side wins on conflict. Rejected outright.
- **A startup migration sweep** rewriting old rows in place. Rejected in (b):
  a read-modify-write filter over the money-ledger family to rescue
  heartbeat-stale claims is disproportionate, and a wrong filter is a live
  risk to the very rows the family exists to protect. The audit mode achieves
  the visibility at none of the risk.
- **Silently ignore old rows** — ship the new builder, touch nothing else.
  Nothing corrupts, but nothing tells the operator either, and the repo's
  standing rule is that a skip must never read as a pass. Replaced by the
  loud audit mode.
- **Take `y` or `z` and re-point strike/jarchive.** Rejected: re-opens an
  accepted record ([ADR-0031] resolved question 4) to save a sub-discriminator
  byte, the exact move [ADR-0032]'s amendment exists to forbid.
- **Open the two-byte family space now.** [ADR-0032] priced this against five
  absent posture rows and declined; nothing in this record changes the
  calculus — one mis-homed key kind is not the trigger for touching every key
  builder and the on-disk format wholesale.

## Open questions

None. Every fork this record faced is priced above; what remains is
acceptance, which is the owner's, and implementation, which is #226's.

## Consequential edits this record requires of its implementer

- Delete the `keyspace.rs:2758-2763` deliberately-absent comment and give `l`
  its four sub-kinds in the registry.
- Update the historical narrative at `keyspace.rs:1004-1010` (which cites the
  undisciplined shape as present-tense) and `:2937-2948` (the recording
  test's doc) when inverting the test.
- Add the audit mode beside the other `fdb`-tier gates so
  `scripts/fdb-tests.sh`'s floor assertions see it.

[#226]: https://github.com/baadc0de/orrery/issues/226
[#224]: https://github.com/baadc0de/orrery/issues/224
[#234]: https://github.com/baadc0de/orrery/pull/234
[ADR-0031]: 0031-id-account-subspace.md
[ADR-0032]: 0032-enforcement-ramp.md
[ADR-0051]: 0051-v1-terrain-is-not-durable-state.md
