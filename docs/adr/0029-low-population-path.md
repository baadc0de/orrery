# ADR-0029: The P5 low-population path: quarantined provisional commit, mandatory spot replay, forward-written annulment

**Status:** Accepted · **Date:** 2026-08-20 · **Decision:** D29

This decision is normative once accepted. See the [ADR index](../DECISIONS.md)
for precedence, scope, and the complete decision set.

**Supersedes:** nothing. It **narrows** [D10](0010-witnessing.md) item 4's
low-population clause (`0010-witnessing.md:12`) for the duration of P5 by
striking the first of its two fallbacks, and it **contradicts two phrases** in
[docs/07 §4.5](../07-witnessing.md) — the priority ordering at `:202` and
"spendable optimistically … sampled for the rest" at `:203`. Under the index's
precedence rule an accepted ADR wins over an expansion document, so those
phrases are corrected by this record rather than by a competing reading; the
edits owed to `docs/07` and `docs/11` are named in Consequences and are not
made here. [D11](0011-persistence.md)'s two load-bearing claims — intents are
RPO 0 and the FDB serializable transaction is the sole authority
(`0011-persistence.md:13`, `:18`) — are **not** weakened by any clause below,
and clause 4 exists to say exactly why.

**Siblings.** The attestation envelope and the required-`K` derivation are
#142; witness-set seeding and the `epoch/{cell_id}` record are #143; the
executor's item transfer is #145. This record consumes their outputs and
duplicates none of them. Where a clause here depends on a choice they have not
published yet, it is listed under Open questions rather than guessed.

## Context

Five facts about the landed tree and the accepted set, each read before it was
written here.

### 1. The first fallback names a crate that does not exist

`docs/07-witnessing.md:202` is the first item of §4.5's ordered list:

> 1. **Field-host witness.** If a field host (`orrery_field_host`) is present
>    or cheaply schedulable, the coordinator seats it as a witness with weight
>    K−1 — one infrastructure witness plus one peer witness satisfies quorum.

There is no `orrery_field_host` directory under `crates/` — the workspace holds
fourteen crates and that is not one of them. The roadmap places the crate's
substance in **P6**: `docs/11-roadmap.md:878` opens P6's crate list with
"`orrery_field_host` (promoted-cell authority, parked-cell catch-up
execution)", and P6's first deliverable (`:881`) is the promotion/demotion
machinery — a coordinator that "spins up a headless Bevy instance". P5's own
crate list hedges at `docs/11-roadmap.md:862` with "`orrery_field_host`
(witness-fallback mode only)", which is the sentence that keeps the fiction
alive: it implies a cheap witness-only build of a crate whose scheduling,
promotion, warrant and lifecycle are all P6 work.

A "cheaply schedulable" infrastructure witness is not cheap. It needs the
coordinator to decide *when* to seat one, an elastic scheduler to start it, a
warrant path so the gateway will believe it, a `Ruleset` link so it can judge
plausibility at all (`0009-verifiable-core.md:13`: the cluster links the same
`Ruleset`), and a demotion path so an empty region does not hold a process
forever. Every one of those is on P6's list. So for the whole of P5 the ordered
list at `:202-203` has one reachable item, and a reader following it in order
reaches a dead end first.

### 2. The second fallback is equally unbuilt, and today's path admits everything

`BaselineIntentValidator` counts attestations and verifies the ones present,
and requires none. Its own contract says so: "Attestations are not *required*
(P5 owes the K-of-N threshold), but a present one must be real"
(`crates/orrery_persistd/src/intent/mod.rs:239-243`). The code matches — the
signature loop is guarded by `if !intent.attestations.is_empty()`
(`intent/mod.rs:332`), so an intent carrying **zero** attestations reaches
`Ok(IntentPrecheck { .. })` at `intent/mod.rs:350` and commits. The three
attestation rejection causes that exist (`intent/mod.rs:186-193`) are
`TooManyAttestations` against a cap of 16 (`:152`), `DuplicateAttestation`, and
`BadAttestation`; none of them is a threshold. The doc comment is explicit that
`cell_epoch` "is carried, not checked: nothing here knows which witness set it
names" (`intent/mod.rs:263-265`).

So there is at present no *attested* path to be a fallback *from*, and no
provisional path to fall back *to*. Both are P5's to build, and this record
decides the second one's semantics before either is written.

### 3. There is no provisional marker on the intent side, and two unrelated things already own the word

`IntentOutcome` has exactly two arms, `Committed { tick, minted }` and
`Rejected { reason }` (`crates/orrery_protocol/src/persist.rs:352-372`). The
executor writes one of them into `intent/{intent_id}` inside the transaction
and is done (`crates/orrery_persistd/src/intent/fdb.rs:479-487`; key builder at
`crates/orrery_persistd/src/keyspace.rs:479`; the row type
`IntentRow { outcome, gc_deadline_ms }` at `keyspace.rs:503-510`). The client's
mirror is terminal in the same way: `IntentStatus`
(`crates/orrery_persist_client/src/intents.rs:61-74`) ends at
`Committed(Tick)` / `Rejected(IntentOutcome)`, the match that assigns it is
exhaustive over the two arms (`intents.rs:320-334`), and `drop_completed`
(`intents.rs:338-340`) exists so the game can forget an intent "once it has
observed the terminal status".

Meanwhile the workspace already spends the word "provisional" on two unrelated
mechanisms:

| Existing | What it means | Where |
|---|---|---|
| `LeaseFlags::PROVISIONAL` | "Client is operating conservatively while the gateway is unavailable" | `crates/orrery_protocol/src/authority.rs:78-79` |
| `BulkAckDisposition::Provisional` | a **bulk** ack whose ownership fence was stale, so it is not durable recovery evidence | `crates/orrery_persistd/src/gateway.rs:546-555` |

The second is the dangerous neighbour, and the code already draws the line this
record must keep drawn: "Intents do not consult this interface: their
`Committed` reply remains an RPO-0 statement about the intent executor"
(`gateway.rs:538-539`). The client honours it by treating a provisional bulk
ack as unacked and resending (`crates/orrery_persist_client/src/uplink.rs:351-352`,
`:372-374`) — a resend policy that would be exactly wrong for an intent, whose
whole idempotency story is the durable row.

### 4. Annulment has no mechanism, and one of the two ledger effects has no inverse recorded

Grepping `annul` over `crates/` returns **nothing**. `docs/07:203` promises
value "annullable by journal compensation until finalized" and
`0010-witnessing.md:11` calls for durable "write refusal/annulment"; no code
writes a compensating record. The harness ledger op is a blind
`MutationType::Add` on `ledger/bal/{account}/{asset}`
(`intent/fdb.rs:675-698`, the credit side of `docs/08` §7's worked trade at
`08-persistence.md:3287`) with no inverse stored anywhere. Two effects of a
committed intent are structurally irreversible even in principle:
`PersistId`s minted from the executor's block grant, whose gaps are already
documented as "an intentional permanent gap" (`intent/fdb.rs:66-70`), and the
versionstamped `ledger/receipt/{versionstamp}` audit row
(`08-persistence.md:3225`), which is a strictly-ordered history and must not be
rewritten.

### 5. The idempotency row's retention is stamped and never swept

`INTENT_ROW_RETENTION_MS` is 1 h (`intent/fdb.rs:64`) and every commit stamps
`gc_deadline_ms: now_ms() + INTENT_ROW_RETENTION_MS` (`intent/fdb.rs:483`),
matching `docs/08-persistence.md:3226` ("default **1 h**, swept by the same
checkpoint pass that GCs despawn tombstones … A client's offline intent queue
TTL must be shorter than this, or a replay after a long netsplit can
double-apply"). The sweep does not exist: `keyspace::intent_range_start`
(`keyspace.rs:486-495`) has no caller anywhere in the workspace, and
`intent_key` has exactly two (`intent/fdb.rs:439` and a test at
`crates/orrery_persistd/tests/intent_commit.rs:492`). So the retention-vs-finalization
race is **latent, not live** — which is the moment to decide it, because the
sweep will be written against whatever this record says.

## Decision

### 1. Field-host witnessing is struck from P5

> **For the whole of P5 there is exactly one low-population fallback —
> provisional commit — and no design, schedule, test or document may assume a
> field-host witness is available; `docs/07 §4.5`'s ordered list has one
> reachable item, not two.**

`orrery_field_host` stays a P6 crate (`docs/11-roadmap.md:878`) and this record
does not build, stub, or reserve any part of it. The "witness-fallback mode
only" hedge at `docs/11-roadmap.md:862` is withdrawn: a field host cheap enough
to seat on demand is a field host with a scheduler, a warrant and a demotion
path, and those are the P6 deliverable itself.

The consequence is not neutral and is stated plainly: P5 loses the option that
was supposed to keep infrastructure in the loop for empty regions, so **every**
low-population intent in P5 is judged by cluster replay after the fact rather
than by an infrastructure witness before the fact. Clauses 3 through 9 exist to
make that survivable.

### 2. The population predicate is a gateway-side function of the *announced* epoch record

> **A low-population intent is one whose announced cell-epoch witness set, with
> that intent's parties removed, holds fewer than N eligible members; the
> predicate is evaluated by the persistence gateway at admission, against the
> durable `epoch/{cell_id}` record for the epoch the intent's tick falls in,
> and never against live presence or against anything the submitter supplies.**

Write it out. For an intent `i` submitted against cell-epoch `(c, e)`:

```
E(c,e)       = the announced selected set in epoch/{cell_id}          (07 §4.1, #143)
P(i)         = parties to i — accounts, and every NodeId bound to them (07:158)
elig(i)      = E(c,e) \ P(i)
K_req(i)     ⊆ elig(i),  |K_req(i)| = K,  derived per-intent          (07 §4.2, #142)

attested(i)  ⟺  K_req(i) ⊆ { a.witness : a ∈ i.attestations }
                 ∧ every such signature verifies                      (intent/mod.rs:332-348)
low_pop(i)   ⟺  |elig(i)| < N
```

with the D16 defaults `K = 3`, floor `N = 5` (`0016-parameter-reference.md:18`).
Admission is then a total function with three outcomes and no fourth:

```
attested(i)                                   →  commit, finality = Final
¬attested(i) ∧ low_pop(i) ∧ reversible(i)     →  commit, finality = Provisional
otherwise                                     →  refuse
```

`reversible(i)` is clause 3. The third line is the one that matters most: an
intent that is neither attested nor low-population is **refused**, not
committed provisionally. Provisional commit is not a general-purpose relief
valve for a missing signature; it is the answer to one specific fact about the
world, namely that there was nobody there to sign.

**Why the announced record and not live presence.** Live presence is exactly
what a submitter can manufacture — drop your friends' sessions and the cell is
suddenly empty. The announced set is coordinator-seeded, rate-limited against
reseed grinding (`07:156`), and durable, so `|elig(i)|` is a fact about a
committed record rather than about the instant of submission. Presence still
matters for *reachability*, and `07:232` already settles that: validity is
judged against the epoch's announced set, not current presence.

**Manufacturing low population is still possible, and is deliberately made
unattractive.** An attacker who brings alt accounts into a cell and makes them
all parties to its own intent shrinks `elig(i)` below `N` and forces this path.
Clause 3 is the answer: the path it forces itself onto is strictly worse for it
than the attested path in every dimension it cares about.

### 3. The provisional path must never be more permissive than the attested path

> **Every property of the provisional path is chosen so that an intent
> committed provisionally yields its submitter strictly less than the same
> intent committed with attestations — less spendability, less certainty, and
> more scrutiny — and any future change that would make the provisional path
> cheaper, faster, or less examined than the attested path is a violation of
> this record.**

This is the constraint from which clauses 4, 5, 7 and 9 are derived rather than
chosen, so it is worth stating as an inequality. For the three dimensions a
cheater optimises:

```
                       attested path        provisional path
spendable at commit    yes                  no          (clause 4)
P(spot replay)         0                    1           (clause 7)
time to usable value   0                    D_finalize  (clause 9)
```

Every cell of the right column is worse. That is what makes clause 2's
manufactured-low-population attack self-defeating: it costs alt accounts and
buys a guaranteed replay of your own fabricated history.

**Corollary — sampling is deleted, not tuned.** `docs/07:203` allows "100% for
high-value intents by `Ruleset` classification, sampled for the rest". A
sampling rate `p < 1` on this path hands an attacker an unexamined durable
commit with probability `1 − p` per attempt, farmable by repetition, and in a
low-population cell there is by construction no independent check to cover the
residue — `07:247` says so in as many words ("With no eligible witness
candidates, nobody re-executes the streamed log"). So on the P5 provisional
path the finalization sampling rate is **1, and is not a tunable parameter**.
Sampling remains correct where §4.5 is not in play, because there the
attestations are the primary check and replay is the audit; here replay is the
*only* check.

The load this buys is affordable, which is why the choice is available at all.
`07:236` measures a full 180-tick single-entity bundle at **< 5 ms**, so one
executor core clears ~200 windows/s; `MAX_ADJUDICATION_TICKS` is 180
(`crates/orrery_protocol/src/verifiable.rs:299`). The requirement is

```
r_lowpop  ≤  200 · cores          (windows/s)
```

where `r_lowpop` is the cluster's low-population intent rate — the intent rate
in cells too empty to witness, which is by definition the rate in the parts of
the world nobody is in. A single core covers 200 such intents per second.

**Classification: `reversible(i)`.** An intent is provisional-eligible only if
the cluster can undo its entire durable effect by writing an inverse (clause
8). Concretely, P5 admits on this path only intents whose ops write rows the
committing cluster owns and whose value does not leave the submitter's own
account:

- **admitted** — value *creation into escrow*: loot grants, crafting outputs,
  progression, structure placement, and any `Ruleset`-classified op whose
  credit and debit are both inside the submitting account's rows;
- **refused** — value *transfer*: any op naming a second account, any currency
  sink the cluster cannot re-credit, anything a `Ruleset` marks real-money
  adjacent, and any op whose effect is read by another account before
  finalization.

The two-party trade — P5's reference intent (`docs/11-roadmap.md:866`) — is
therefore **refused** in a low-population cell, never committed provisionally.
That is not a loss: party exclusion (`07:158`) removes both traders from
`elig(i)` anyway, so a trade in a two-person cell has no witnesses by
construction, and committing it provisionally would be committing the single
most cascade-prone operation on the least evidence.

### 4. "Provisional" means quarantined, not optimistically spendable

> **A provisionally committed effect is durable, visible, and attributable, and
> it is not an input to anything: no intent may name a provisionally committed
> row among its reads or writes until that row's originating intent is
> finalized, and the gateway refuses any intent that tries — so the annulment
> set of a provisional commit is always exactly that one intent.**

This narrows `docs/07:203`'s "Provisional value is spendable optimistically",
and the narrowing is the whole cascade defence. State the two options as
arithmetic. Let `A(i)` be the set of intents that must be reversed when `i` is
annulled, and let `d` be the number of onward intents permitted per provisional
output before finalization:

```
spendable          |A(i)|  unbounded — the transitive closure of everything
                           derived from i, across accounts, across cells,
                           including intents that have since *finalized*
quarantined (this) |A(i)| = 1
```

The right-hand column is not a smaller version of the left; it is a different
problem. A cascade that reaches a *finalized* intent forces the cluster to
reverse a commit it has already certified, which destroys the meaning of
finalization — and a cascade that crosses accounts takes value from a player
who did nothing wrong and could not have known. There is no compensation
algorithm that repairs either. Containment at depth 1 removes the possibility
instead of managing it, and it is checkable in one predicate at admission
rather than by a graph traversal at annulment.

What each observer may do with a provisional commit:

- **The submitting client** sees the item, holds it, and may display it. It
  must not present it as final, and its local intent prediction (D8,
  `intents.rs:76-80`) **holds** — it neither rolls back (the value is real) nor
  resolves terminally (`drop_completed` at `intents.rs:338-340` must not fire
  on a provisional status).
- **Any other client** sees nothing it can act on: nothing crosses accounts
  before finalization, by clause 3's classification.
- **The cluster** treats the row as ordinary durable state for reads and as
  ineligible input for writes.
- **The `Ruleset`** is told, so a game may render "pending". It is not asked to
  enforce anything: enforcement is the gateway's, because a modified client
  will not cooperate and a `Ruleset` seam that the cheater's own process
  evaluates is not a control.

**The gate `07:203` offers to games is not exposed in P5.** That line says
"games can gate irreversible sinks … on finalization"; under clause 3 those
sinks are refused outright on this path, so there is no gate for a game to set
and no configuration surface to get wrong. A game that wants a *narrower*
policy than clause 3 may refuse more via its `Ruleset` classification; it may
not widen it.

### 5. The durable representation is a finality field, and the wire gains a third outcome

> **The durable state of a provisional commit is a `finality` field on the
> existing `intent/{intent_id}` row — `Provisional` | `Final` | `Annulled` — and
> the client is told the truth by a third `IntentOutcome` arm, which is a wire
> change that bumps `PROTOCOL_VERSION` to 2.**

Server side: `keyspace::IntentRow` (`keyspace.rs:503-510`) gains `finality` and
the finalization deadline; no new key family, no second subspace, and the
idempotency read at `intent/fdb.rs:439-443` keeps returning one row. A separate
`provisional/{intent_id}` family was considered and rejected under Alternatives:
it would put the answer to "did this intent happen" in two rows that a crash
between them can disagree about, in the one code path whose entire purpose is
that there is only ever one answer.

Wire side: `IntentOutcome` (`persist.rs:352-372`) gains

```
Provisional { tick, minted, finalize_by }
```

carried in the existing `GatewayReply::IntentAck`
(`crates/orrery_protocol/src/gateway.rs:214-220`), and `IntentStatus`
(`intents.rs:61-74`) gains a matching non-terminal state. **The verdict on
versioning: this bumps `PROTOCOL_VERSION` from 1 to 2**
(`crates/orrery_protocol/src/protocol.rs:13`). postcard keys enum variants by
declaration order, so appending an arm is safe to encode-old/decode-new and
unsafe in the other direction: a version-1 client receiving `Provisional` fails
to decode. **Accepted with the window closed.** The operator's decision on acceptance is
that the cluster supports version 2 **only** — the `PROTOCOL_VERSION − 1`
acceptance window is dropped rather than kept. That is a broader change than
this record needs and it simplifies this clause rather than complicating it:
there is no version-1 client to degrade to refusal, so clause 2's second line
has no second branch, and `IntentStatus`'s non-terminal state has no
compatibility caveat. The system is pre-release and has no external clients, so
dual-version support was complexity that had not been earned. The `protocol.rs`
change is wider than the intent path and belongs to whoever implements this
record: the window is closed once, for all traffic, not per message family.

The acceptance window was `PROTOCOL_VERSION` and `PROTOCOL_VERSION − 1`
(`protocol.rs:3-5`), so a cluster deploying 2 still serves version-1 clients —
and a version-1 client must therefore be **refused** the provisional path
rather than sent an arm it cannot read: for a client that negotiated version 1,
clause 2's second line degrades to refusal. That is the correct degradation,
and it is the same answer clause 2 already gives for every non-eligible intent.

The exhaustive match at `intents.rs:320-334` means the compiler names every
place that must decide, which is the reason to spend an enum arm rather than a
boolean.

> **The intent-side provisional commit shares no name, field, counter or metric
> with `BulkAckDisposition::Provisional` (`gateway.rs:546-555`) or
> `LeaseFlags::PROVISIONAL` (`authority.rs:78-79`); they are three unrelated
> mechanisms and the only thing they have in common is an English word.**

Concretely: intent-side counters carry the `intent_provisional_` prefix, bulk
counters keep theirs, and no dashboard panel or gate assertion may sum across
them. `gateway.rs:538-539`'s existing statement — "Intents do not consult this
interface: their `Committed` reply remains an RPO-0 statement about the intent
executor" — stays true word for word, because a provisional intent reply is
`Provisional`, not `Committed`.

### 6. Evidence is committed to on the intent path and fetched on the finalization path

> **A provisional commit stores a fixed-size *commitment* to the evidence — the
> ruleset id, subject entity, tick window, the `t₀` claim hash and the
> submitter's log head — and never the evidence itself; the finalizer fetches
> the `EvidenceBundle` out of band at replay time, so the intent transaction's
> size and the < 10 ms commit budget are untouched.**

`docs/07:203` asks for "full evidence attached (submitter's log segment
covering the intent window, t₀ claim)". That is an `EvidenceBundle`
(`crates/orrery_protocol/src/verifiable.rs:208-245`), which carries
`t0_snapshot: Bytes`, up to 180 `frames`, `sibling_heads` and `disputed_claims`
— unbounded in principle and routinely far over FDB's 100 KB value limit
(`0011-persistence.md:15`). Attaching it would mean sharding a blob across rows
inside the very transaction whose p99 budget is 10 ms
(`0011-persistence.md:13`, `0016-parameter-reference.md:16`). This record
declines that outright:

```
on the intent path:      commitment  =  (RulesetId, PersistId, [t0, t_intent), blake3(t0_claim), log_head)
                         ≈ 4 + 8 + 16 + 32 + 32  ≈ 92 bytes, fixed
on the finalization path: bundle fetched from the submitter, or from any
                         witness-set peer holding the stream (07:115)
```

The commitment is chain-anchored, so the bundle the finalizer later receives
either matches it or is refused — the submitter cannot substitute a friendlier
history after the fact, which is the only property the intent path actually
needed from "attached evidence".

**Named weakness: in an empty cell the submitter is usually the only source of
the evidence that would convict it.** `07:115` already has the cluster assemble
a segment "from other witness-set peers holding the stream", and in a
low-population cell there may be none. This record does not pretend otherwise;
it removes the incentive instead. Failure to produce a bundle matching the
commitment, within the deadline, is **annulment** (clause 8), so losing the
evidence gains the submitter exactly what a deviation verdict would have. It is
scored as non-cooperation, which `07:207` already weights at 1.0 and `07:115`
already converts to *suspected* status.

**Budget consumed, stated rather than waved at.** The intent transaction pays
one additional ~92-byte write into a row it was already writing — no extra
round trip, no extra key, no conflict range it did not already register. The
finalization fetch is entirely off the intent path and consumes the
adjudication fleet's budget, sized in clause 3.

### 7. Finalization is spot replay on a second entry point of the existing executor

> **Finalization re-executes the intent's window in the cluster's verifiable
> core under the ruleset the submitter pinned, using `AdjudicationExecutor`'s
> retained version-keyed builds through a second entry point that takes a
> commitment and a fetched bundle rather than a `DiscrepancyReport`; the
> verdict is written back to the same `intent/{intent_id}` row, and no separate
> executor, ruleset registry or retention window is created.**

The existing executor is a pure router over retained builds
(`crates/orrery_persistd/src/adjudication.rs:44-51`, `RETAINED_BUILDS = 3` at
`:29`), and its only entry point today is
`adjudicate(&DiscrepancyReport) -> Verdict` (`:85-108`), which begins by
verifying the *reporter's* signature — a check with no meaning here, because a
provisional finalization has no reporter and no accusation. So the executor is
shared and the *scheduler* is not:

| | discrepancy adjudication | provisional finalization |
|---|---|---|
| trigger | a peer files a report | a durable row exists in `Provisional` |
| queue | event-driven, per-account rate-limited (`07:236`) | a sweep over unfinalized rows, oldest first |
| entry | `adjudicate(&DiscrepancyReport)` | takes `(commitment, EvidenceBundle)` |
| sampling | sampled, prioritised by strike score | 1, always (clause 3) |

Sharing the executor is deliberate: `RETAINED_BUILDS = 3` is the scarce
resource, and two registries would give two answers to "which build adjudicates
this window", which is the failure `07:237` designed the version-keyed routing
to prevent.

**What spot replay proves, exactly.** It proves the submitter's claimed history
across `[t₀, t_intent)` is self-consistent, correctly signed, chain-continuous,
and reproduces the submitter's own state claims under the pinned build. It does
**not** re-check the ledger invariant — that check already ran, inside the
serializable transaction, and `0011-persistence.md:18` keeps it the sole
authority over durable truth. The gap spot replay closes is the one attestation
would have closed: whether the intent was grafted onto a history nobody saw
(`07:189`). Stating this narrowly matters, because a reader who believes
finalization re-audits the economy will not build the conservation auditor that
`docs/11-roadmap.md:917` owes at P5 exit.

**How a later reader tells the two apart.** The `finality` field is the record,
and it is on the same row as the outcome, so any reader of `intent/{intent_id}`
gets both in one read. The four verdicts map as:

```
Verdict::Deviation        →  finality = Annulled,  strike the submitter (07:207, 3.0)
Verdict::WithinTolerance  →  finality = Final,     no strike
Verdict::EvidenceForged   →  finality = Annulled,  strike the submitter — here the
                             submitter *is* the evidence's author, unlike the
                             report path where this verdict strikes the reporter
Verdict::Unadjudicable    →  see clause 9; never a strike (0010-witnessing.md:11)
```

The `EvidenceForged` remapping is the one asymmetry worth flagging in review:
on the report path that verdict protects an accused peer from a lying reporter
(`07:234`), and on this path there is no third party — the account that
fabricated the bundle is the account that submitted the intent.

### 8. Annulment is a forward-written inverse, never an erasure

> **Annulling a provisional intent means committing a cluster-issued
> compensating intent that applies the exact inverse of the original ops, flips
> the originating row's `finality` to `Annulled` with a fresh GC deadline, and
> appends its own `ledger/receipt` row — all in one serializable transaction;
> nothing is deleted, and the original commit remains visible in the ledger's
> history forever.**

The mechanism reuses what exists. `docs/08` §7's transaction shape
(`08-persistence.md:3267-3300`) is unchanged, the receipt row is
versionstamped and strictly ordered (`08-persistence.md:3225`), and the inverse
op is the same primitive D11 already names for griefing rollback
(`0011-persistence.md:19`: "inverse-op replay by cell/actor/time-range"). Under
clause 4 the read set of the compensating transaction is the originating
intent's write set and nothing else, so it cannot conflict with a downstream
intent — because there are none.

**What annulment does not reverse, honestly:**

- **`PersistId`s.** Minted from the executor's durable block grant and gone;
  `intent/fdb.rs:66-70` already calls such gaps "an intentional permanent gap".
  The annulled entity's id is never reissued, which is the only safe outcome.
- **The audit trail.** `ledger/receipt/{versionstamp}` rows are append-only by
  construction. An annulment adds a receipt; it does not remove one. A reader of
  the trade history sees the commit and the reversal, in that order, which is
  what an auditor needs and what a player owed an explanation needs.
- **Journal and archive records.** The journal is the event source
  (`0011-persistence.md:19`) and its tailer may already have compacted the
  commit into the Parquet archive. Compensation appends; it does not rewrite
  history that has left the cluster.
- **What the player saw.** The client displayed the item, possibly for minutes.
  Annulment is a visible removal and there is no version of this that does not
  feel like one. That cost is the reason clause 3 refuses transfers and clause 9
  keeps the window short.
- **Anything a second account observed.** Vacuous under clause 4 — nothing
  crossed — and it is only vacuous because of clause 4.

**Who is told.** The submitter, on the existing unsolicited-push path
(`GatewayReply::Lease` already carries pushes the client did not ask for; an
annulment notice is the same shape), and on next login if it was offline —
which the durable `Annulled` row makes possible without any delivery guarantee.
The identity service is told only on `Deviation` and `EvidenceForged`; a
`Unadjudicable` annulment strikes nobody, per `0010-witnessing.md:11`. The
operator is told always, because a nonzero annulment rate is either an attack
or a `Ruleset` bug and both need the alarm `07:237` already describes.

### 9. The bound: a short deadline, a per-account cap, and admission halt before expiry

> **A commit may stay provisional for at most `provisional_finalize_deadline`
> (default 5 min); reaching that deadline annuls the intent as `Unadjudicable`
> with no strike; and because expiry is a fault indicator rather than a routine
> outcome, the cluster's response to a finalizer that cannot keep up is to stop
> admitting provisional intents — bounded by a per-account outstanding cap —
> long before it starts annulling old ones.**

Three sub-decisions, each with its reason.

**(a) The deadline annuls; it does not auto-finalize.** Auto-finalizing at the
deadline would make "outlast the replay queue" a strategy, converting a
denial-of-service against the adjudication fleet into a dupe vector — which is
the GTA Online failure `docs/11-roadmap.md:860` names as the cautionary tale.
Fail-closed is also the *correct degradation*: if the finalizer is down, then
nothing in a low-population cell is being checked by anything, and continuing
to mint durable value there is precisely the posture P5 exists to abolish. Under
clause 4 the annulled value was quarantined anyway, so the honest player's loss
is bounded by what they gained in an unwitnessed cell during the outage — and
the alternative loses that same value to every attacker permanently.

**(b) The routine response is refusal, not expiry.** Let `C` be the per-account
outstanding cap and `D` the deadline. Value at risk is bounded per account and,
through admission, cluster-wide:

```
VaR(account)  ≤  C · v_max                     v_max = the largest single-intent value
                                                       the Ruleset classifies as
                                                       provisional-eligible
outstanding(account) = C   →  further low-population intents from that account are
                              REFUSED (clause 2, third line), not queued
backlog > D − margin       →  the cluster stops admitting provisional intents entirely
```

Refusal is a defined, liability-free answer that the admission function already
produces. Expiry is not: it is the one outcome that destroys value the cluster
already promised. So the system is arranged to reach the first and not the
second, and an expiry in production is an incident.

**(c) The GC interlock.** Clause 5 puts `finality` on the same row as the
outcome, so the sweep `docs/08-persistence.md:3226` promises (and which
`keyspace.rs:486-495` still has no caller for) is written with one extra
condition:

```
sweepable(row)  ⟺  row.finality ∈ {Final, Annulled}  ∧  now_ms ≥ row.gc_deadline_ms
```

and the annulment transaction **restamps** `gc_deadline_ms` to
`now_ms + INTENT_ROW_RETENTION_MS`, so an annulled row retains for an hour from
the *annulment*, not from the commit. That closes the dupe vector the issue
names: a row saying "provisional" can never vanish under a replay, because it
is not sweepable while provisional, and an annulled row outlives a client
offline queue whose TTL `08-persistence.md:3226` already requires to be shorter
than the retention. The two constants must satisfy

```
D_finalize (5 min)  ≪  INTENT_ROW_RETENTION_MS (1 h)
```

which they do by a factor of twelve, so a provisional row is always resolved
with ≥ 55 minutes of retention left and the interlock is never the thing doing
the work — it is the assertion that catches it if the deadline is ever raised
carelessly.

**The parameter values are chosen, not measured.** 5 minutes exceeds an
evidence fetch (one RTT, plus a retry, plus a reconnect window for a submitter
whose lease has lapsed at 10 s — `0007-authority-and-leases.md:14`) by three
orders of magnitude, and sits an order of magnitude under the retention that
bounds it. `C = 8` is the per-account value-at-risk dial and has no derivation
at all; it is set low because nothing in a low-population cell should be
producing eight unfinalized intents at once, and an account that is has already
told the operator something. Both go into [D16](0016-parameter-reference.md)
marked proposed by this record, and both should be re-derived from the first
shadow-mode telemetry rather than defended.

## Consequences

- **`docs/07-witnessing.md` §4.5 is contradicted by this record and must be
  rewritten when it is accepted.** Three specific corrections are owed, and
  this record deliberately does not make them (the file is owned elsewhere this
  round): `:202`'s field-host item is not available in P5 and the "priority
  order" reading must go, since an ordered list with one reachable item is not
  an order; `:203`'s "spendable optimistically" is narrowed to quarantined by
  clause 4; and `:203`'s "sampled for the rest" is fixed at 100% on this path
  by clause 3. `:196` (§4.4's collusion analysis) and `:246-247` (§8's residual
  limits) stay true as written and get **stronger** — a colluding pod that
  lands on the provisional path now faces certain replay and cannot spend what
  it fabricates.
- **`docs/11-roadmap.md` owes two edits.** `:862`'s "`orrery_field_host`
  (witness-fallback mode only)" comes out of P5's crate list under clause 1, and
  `:865`'s deliverable "low-population fallbacks: field-host witness or
  provisional commit" loses its first disjunct. `:878` is unchanged: the crate
  stays exactly where it is.
- **A wire change and a `PROTOCOL_VERSION` bump land in P5.** The third
  `IntentOutcome` arm is the first protocol break since `PROTOCOL_VERSION` was
  introduced, and it exercises the rolling-upgrade window at `protocol.rs:3-5`
  for real. Version-1 clients keep working and simply cannot use the
  provisional path.
- **Capability lost: two-party trades do not work in empty regions, at all.**
  Clause 3 refuses transfers on this path and party exclusion leaves a
  two-person cell with no witnesses, so a trade between the only two players in
  a dead region is refused until a third eligible account arrives. This is a
  real, player-visible product hole, and it is the deliberate price of a depth-1
  annulment set. If it proves intolerable, the fix is a field-host witness in
  P6 — not a widening of clause 4.
- **Capability deferred: the game-facing finalization gate.** `07:203` offered
  games a hook to gate irreversible sinks on finalization; clause 4 refuses
  those sinks outright instead, so the hook has nothing to gate and is not built
  in P5. A game wanting a finer policy gets one when transfers become
  provisional-eligible, which is not this record.
- **The intent-row sweep is now specified before it is written.** Clause 9's
  `sweepable` predicate is the contract for the pass `08-persistence.md:3226`
  promises and `keyspace.rs:486-495` has no caller for. Whoever writes it
  inherits a condition rather than discovering the race in production.
- **The P5 dupe gauntlet gains an arm this record must be asserted against.**
  The replay arm (`docs/11-roadmap.md:870`(a)) currently asserts on the
  idempotency key; it now also has to assert that a replayed *provisional*
  intent returns `Provisional` and not a second commit, and that a replayed
  *annulled* intent returns `Annulled` and does not re-apply.
- **The adjudication fleet gets a second workload with a different shape.**
  `07:236`'s sizing ("a couple of cores absorb even attack-volume report
  floods") assumed report-driven, sampled, rate-limited work. Provisional
  finalization is sweep-driven and unsampled. Clause 3 shows one core covers 200
  low-population intents/s, but the fleet's fairness queue now has two
  producers, and the per-account rate limits that shed a report flood do not
  apply to a workload the cluster generated itself.
- **`AdjudicationExecutor` gains an entry point and no new retention.**
  `RETAINED_BUILDS = 3` (`adjudication.rs:29`) continues to bound both
  workloads, which means a provisional intent pinning a build older than the
  last three finalizes as `Unadjudicable` → annulled with no strike. That is a
  new way for an honest player to lose an item during a rules upgrade, and it is
  the same trade `07:237` already accepted for reports.
- **Nothing in D11 is weakened.** Intents remain RPO 0: a `Provisional` reply is
  sent after the FDB transaction resolves, exactly as `Committed` is
  (`intent/mod.rs:18-22`), and it is a durability statement, not a hedge about
  whether the write landed. The FDB transaction remains the sole authority over
  durable truth; finalization audits the *history behind* an intent, never
  re-adjudicates the ledger.

## Alternatives considered

- **Refuse outright: no durable value moves in a cell that cannot witness.**
  The safest possible answer, and the one this record would take if
  low-population cells were rare. They are not: under D6's population-adaptive
  topology most of a persistent universe is empty most of the time, and every
  region has dead hours. Blanket refusal means a solo player cannot loot, craft
  or progress across the majority of the world — a product failure, not a safety
  win, and one that pushes players toward exactly the crowded cells the topology
  is trying to relieve. Rejected in favour of *partial* refusal: clause 3 keeps
  the refusal for the classes where annulment cannot repair the damage, which is
  the half of this alternative that was actually load-bearing.
- **`docs/07:203` as written — spendable optimistically, with journal
  compensation cleaning up.** Rejected under clause 4's arithmetic. The
  compensation set is the transitive closure of everything derived from the
  annulled intent, it crosses accounts, and it reaches intents that have already
  finalized. There is no implementation of that which is both correct and
  explicable to the player it takes value from, and "journal compensation" names
  a mechanism (`docs/07:203`) that exists nowhere in `crates/` today.
- **A depth limit instead of a quarantine — allow one onward trade, annul both.**
  Rejected: the bound is arbitrary, it still crosses an account boundary (which
  is the property that makes cascades unfixable, not the depth), and it requires
  the graph traversal at annulment time that the quarantine removes. Depth 1 is
  the only depth whose annulment set is a constant.
- **Auto-finalize at the deadline instead of annulling.** Rejected under clause
  9(a): it makes exhausting the adjudication fleet a winning strategy, and it
  converts an availability incident into a durable-value incident.
- **A sampling rate below 100%, per `07:203`.** Rejected under clause 3. On this
  path there is no independent check to cover the unsampled residue — `07:247`
  says exactly that — so `1 − p` is a farmable probability of an unexamined
  commit. The parameter is deleted rather than defaulted to 1, so nobody adds
  the dial back later without reopening this record.
- **Retro-attestation: when the cell repopulates before the deadline, let the
  new witness set attest the pending intent and finalize it without replay.**
  Genuinely attractive — it would cut the finalizer's load and shorten the
  quarantine. Rejected because witnesses attest *plausibility in their own
  replicated view at the time* (`07:184-190`): a peer that arrived after the
  window has no view of it, so its signature would attest to nothing while
  looking exactly like one that attests to something. That is a worse failure
  than the load it saves.
- **Attach the `EvidenceBundle` to the intent transaction, sharded across rows
  per `0011-persistence.md:15`.** Rejected under clause 6: it puts an unbounded
  blob write inside the one transaction with a 10 ms p99 budget, to obtain a
  property (the submitter cannot substitute a friendlier history) that a
  92-byte commitment already gives.
- **A separate `provisional/{intent_id}` row family instead of a field on the
  existing row.** Rejected: it makes "did this intent happen" a two-row question
  in the exact code path whose purpose is that there is only one answer
  (`intent/fdb.rs:6-10`), and it adds a key family — which under
  [D22](0022-grid-id-in-the-storage-key.md) would carry a `GridId` discriminator
  — to store one enum.
- **A boolean flag on the wire rather than a third `IntentOutcome` arm.**
  Rejected: `Committed { .. } + provisional: true` is a `Committed` that is not
  committed, and every existing reader — including `intents.rs:320-334`'s
  exhaustive match and `drop_completed` at `:338-340` — would keep compiling
  while becoming wrong. An enum arm makes the compiler name every site that has
  to decide.
- **Reusing `BulkAckDisposition::Provisional`'s vocabulary and counters.**
  Rejected in clause 5's second normative sentence. They are opposites in the
  one way that matters: a provisional *bulk* ack means "resend this", and a
  provisional *intent* outcome means "never resend this, the durable row is
  already there". A shared counter would average two mechanisms with contrary
  remediations.
- **Keeping field-host witnessing in P5 as a "cheap" witness-only build.**
  Rejected under clause 1. Every part of what makes a field host seatable —
  scheduling, warrant, `Ruleset` link, demotion — is P6's deliverable, and a
  record that leaves the option nominally open buys P5 an unbuilt dependency in
  exchange for nothing.

## Open questions

- **The eligible-set definition is #142's and #143's, and clause 2's predicate
  consumes it.** If #142 derives the required subset from a pool that is not
  `E(c,e) \ P(i)` — for instance if it admits candidates outside the announced
  set, or defines `N` per-epoch rather than as the D16 floor — then
  `low_pop(i)`'s threshold moves with it. This record fixes the *shape* of the
  predicate (announced record, party-excluded, gateway-evaluated) and defers the
  set's membership rule to those records rather than guessing it.
- **Whether the gateway can read the announced set at intent admission at all.**
  Clause 2 assumes `epoch/{cell_id}` is durably readable by the gateway on the
  intent path, which is #143's to establish. If it is not — if the announcement
  is only a signed message the submitter couriers — then the predicate's input
  becomes submitter-supplied and the whole clause needs a different evaluator.
  This is the single dependency most likely to change a decision here.
- **Where the finalizer runs.** Clause 3's sizing works in `persistd` or in
  D12's version-keyed sidecar fleet, and clause 7 shares the executor either
  way. The deployment choice is P5 ops work and is not made here.
- **Whether deadline expiry should be shadow-mode-only during the enforcement
  ramp.** D17.3 (`0017-risks-and-open-questions.md:9`) requires the strike
  pipeline to launch telemetry-only, and annulment-on-expiry destroys value
  without a strike, so it is not obviously covered by that requirement.
  Enforcement rollout policy is #105's and explicitly out of this record's
  scope; the flag is named, its default is not set here.
- **`v_max` — the largest single-intent value a `Ruleset` may classify as
  provisional-eligible — has no bound in this record.** Clause 9's
  `VaR ≤ C · v_max` is only a bound if `v_max` is one, and classification is the
  game's. A cap belongs either in the `Ruleset` contract or in D16, and this
  record does not choose.
- **The compensating intent's issuer identity.** Clause 8 says "cluster-issued",
  and the cluster has no signing identity on the intent path today — every
  `Intent` carries an `issuer` bound to an authenticated connection
  (`intent/mod.rs:271-273`). Whether annulment is an `Intent` with a cluster
  issuer, or a privileged executor path that bypasses admission entirely, is an
  implementation question with a security answer, and #150 should not discover
  it late.
- **Whether the quarantine is visible in the `Ruleset` state view.** Clause 4
  says a game may render "pending", which implies the provisional flag reaches
  the `Ruleset`'s view of a row. That is a change to a frozen harness API
  ([D21](0021-ruleset-distribution.md)) and therefore needs its own decision if
  it turns out to be more than presentation.
