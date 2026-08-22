# ADR-0037: An unavailable witness epoch refuses with a bounded cure; only measured low population is provisional

**Status:** Accepted · **Date:** 2026-08-22 · **Decision:** D37

> **This record amends the accepted [D27](0027-attestation-envelope.md) and
> [D28](0028-witness-set-seeding.md), and consequentially updates accepted
> [D30](0030-cell-epoch-standing.md) and
> [D31](0031-id-account-subspace.md).**
> It applies an **erratum** to D27 clause (e): cases 2 and 3 refuse with the
> cures below instead of committing provisionally. It applies the same erratum
> to D28 clause (g)'s parenthetical restatement of the stale case. D29 clause 2
> stands word for word. D30 clause (c)'s temporary issue pointer and its
> comparison against D27, and D31 clause (f)'s matching analogy, are
> consequentially updated; neither record's decision changes.

The distinction between erratum and amendment is historical, not semantic.
D27, D28, and D29 became Accepted together in commit `95ca344e`; D29's
three-outcome admission function was already present when D27 clause (e)'s
incompatible provisional cases were accepted. The contradiction therefore
existed at acceptance. No later change made a formerly coherent D27 clause
wrong. This is a correction to what the accepted set said on that day, in the
annotation style [D29](0029-low-population-path.md) and
[D36](0036-binding-rate-window.md) establish, not a claim that a newer world
superseded a once-correct rule.

This decision is normative only if accepted. See the
[ADR index](../DECISIONS.md) for precedence, scope, and the complete decision
set.

**Supersedes:** D27 clause (e), cases 2 and 3, and only those cases; D28 clause
(g)'s parenthetical “or take #144's provisional path” insofar as it restates
the stale case. D27 clause (e) case 1, D28's grace window, D29 clauses 1–9,
D30's standing predicate, and “never silent full admission” are unchanged.

Out of scope: changing the provisional path, its reversibility classifier,
quarantine, replay, deadline, or annulment; changing the grace duration;
adding a wire cause; rollout policy; and Rust edits. The comments that Rust
owners must correct are named in clause (d), but this record changes only
`docs/adr/` and the index.

## Context

### The accepted set has no total function until one clause yields

D27 clause (e) sends an announcement that is unknown, mismatched, or stale
past grace to provisional commit. D29 clause 2 admits provisionally only when
the gateway has the announced epoch record and can establish
`low_pop(i) = |elig(i)| < N`. Without a resolved announcement, `E(c,e)` and
therefore `elig(i)` do not exist. The two statements cannot both be evaluated.

D29 supplies the total function the provisional implementation actually
needs:

```
attested(i)                                   -> commit Final
!attested(i) && low_pop(i) && reversible(i)   -> commit Provisional
otherwise                                     -> refuse

low_pop(i) = |elig(i)| < N, with the signed announcement in hand
```

The landed enforcement follows it. `check_attestation_quorum` returns
`UnknownEpoch` when handle resolution misses and `EpochStale` after the
resolved announcement's usability window. Both flow through the general
refusal arm. Only `LowPopulationEpoch` reaches `Admission::Provisional`, in
both required and shadow evaluation
(`crates/orrery_persistd/src/intent/mod.rs:1337-1378`, `:1872-1893`). The tests
name unknown and stale separately and assert both refusals (`:3272-3312`).

### Absence is submitter-selectable; low population is a signed fact

The coordinator sends every selected peer a signed announcement, and
`GatewayMsg::WitnessEpoch` lets that peer courier it to the gateway. The
gateway acknowledges the presentation with `WitnessEpochAck`. A submitter
that holds an announcement can therefore select `UnknownEpoch` merely by
withholding one message. A branch selected at zero cost by the party being
checked must not contain “admit”.

The pre-commit difference is plain in a populated cell:

```
announcement presented:   K = 3 required co-signatures before durable commit
announcement withheld:    0 required co-signatures before durable commit
```

Routing the second line to provisional commit would let a submitter replace
three hidden, coordinator-drawn plausibility checks with a five-minute
quarantine. Mandatory spot replay does not restore the removed check. D29
clause 7 proves that replay establishes the submitter's claimed history is
self-consistent, signed, chain-continuous, and reproducible under the pinned
build; witnesses additionally compare the transition with their own
replicated views, including whether an intent was grafted onto a history
nobody observed. Thus `3 -> 0` is strictly more permissive before commit on a
security axis replay does not reproduce. D29 clause 3 forbids that trade.

`low_pop(i)` has the opposite shape. It is computed from a verified,
coordinator-signed announcement after party exclusion. The submitter may try
to manufacture the predicate by adding parties, but D29 deliberately prices
that attempt with quarantine, certain replay, delayed spendability, and
reversibility. Unknown or stale input has no corresponding bound because
there is no eligible vector to count.

Refusal also matches D12's netsplit posture: simulation continues, intents
queue, and durable commits pause. An unavailable announcement is degraded
availability. Turning its absence into a durable provisional write would not
be a pause.

## Decision

### (a) D27 yields; the disposition is total and has no fourth arm

> **D27 clause (e) cases 2 and 3 are corrected to refusal with an explicit
> cure. D29 clause 2 stands verbatim. A gateway never infers low population
> from an unavailable announcement and never routes `UnknownEpoch` or
> `EpochStale` to provisional commit.**

The complete disposition is:

| Condition | Result | Change |
|---|---|---|
| Resolved superseded epoch still inside `first_seen_ms + epoch_ms + accept_grace_ms` | judge against the epoch named by the intent | none |
| `UnknownEpoch`: the handle resolves to no accepted announcement | refuse; present the named announcement and retry | D27 (e) cases 2–3 corrected |
| `EpochStale`: the named announcement resolves but its usability window ended | refuse distinctly; collect under the current epoch and retry | D27 (e) case 2 corrected |
| `LowPopulationEpoch`: announcement in hand and `|elig(i)| < N` | D29 provisional path, subject to `reversible(i)` | none |

With D16's defaults, the first row remains usable for
`epoch_ms + accept_grace_ms = 30 s + 30 s = 60 s` from the accepting gateway's
`first_seen_ms`. “Never refusal” is corrected for the two unavailable-input
rows only. “Never silent full admission” survives in every row.

`UnknownEpoch` and `EpochStale` share a disposition but not a threat reading.
Unknown means the gateway is behind or the submitter withheld a public signed
input. Stale means the gateway did resolve the claimed epoch and the submitter
is presenting an attestation more than a full epoch plus grace after first
observation; that is replay-shaped. It remains a distinct cause rather than a
signature failure so operators can tell recovery traffic from forgery.

### (b) Refusal is clause 3's minimum, not a weakened provisional path

> **Neither refusal enters D29's provisional machinery. No provisional row,
> quarantine, evidence commitment, replay job, finalization deadline, or
> annulment is created for `UnknownEpoch` or `EpochStale`.**

In the permissiveness order, refusal creates no durable effect and exposes no
value. It is strictly below both Final and Provisional commit, so D29 clause 3
holds trivially. The amendment changes no cell of D29's comparison:

```
                       attested path       D29 provisional path
spendable at commit    yes                 no
P(spot replay)         0                   1
time to usable value   0                   D_finalize
```

It instead says that an attacker cannot enter the right column by suppressing
the signed record needed to establish that the column applies.

### (c) Every availability loss has a cure, counted in round trips

Let `RTT_G` be one peer-to-gateway request/reply round and `RTT_W` one parallel
co-sign request/reply round to the announced witnesses. A normal intent already
costs one `RTT_G`; the counts below state the additional cost after the
condition is encountered.

**Un-couriered live epoch.** A client must present the announcement for every
handle it is about to name. `WitnessEpoch` is bounded at 2,048 bytes and is
acked. Presenting it immediately before the intent on the same ordered path,
or pipelining both, adds **0 RTT** to a normal submission. Waiting for
`WitnessEpochAck` before sending the intent is permitted and adds **1 RTT_G**.
If an honest client first learns through an `UnknownEpoch` refusal, that failed
attempt has already cost **1 RTT_G**; pipelining presentation and retry costs
**1 further RTT_G**. A peer that never received an announcement has no local
cure: it queues until the coordinator delivers one, then pays the same one
retry round. During a genuine coordinator partition that wait is intentionally
unbounded; durable settlement pauses rather than admitting on missing input.

**D26 successor with a cold cache.** The durable
`epoch/{grid}/{cell}/{epoch}` row and `epoch-handle/{handle}` index written by
the first resolving intent are the successor's recovery source, and every peer
in the cell can re-present the signed envelope. Pre-hydration from the durable
row costs the honest submitter **0 RTT**. If the peer discovers the cold cache
by refusal, presentation plus retry is **1 further RTT_G** when pipelined. The
availability bound is therefore one client retry round after discovery, not a
30-second epoch. A row does not exist when no earlier intent made that epoch
durable; peer re-presentation is the cure in that case.

**Epoch turnover.** Inside the 60-second usable window there is no cure and no
extra round: the old attestations are judged normally. Past it, the failed
submission costs **1 RTT_G**. Re-collecting under the current announcement is
one parallel **RTT_W**, bounded by D16's 150 ms co-sign budget, and retrying is
one **RTT_G**. Thus the cure after refusal is two sequential rounds,
`1 RTT_W + 1 RTT_G`; including the failed attempt, the observed recovery is
`2 RTT_G + 1 RTT_W`. Presenting the current announcement can be pipelined
before the retry and adds no separate round.

### (d) Consequential edits, exhaustively named

1. **D27 clause (e) — erratum.** Cases 2 and 3 and the concluding “never
   refusal” sentence receive the correction in clause (a). Case 1 and “never
   silent full admission” remain normative.
2. **D28 clause (g) — erratum.** The stale-case parenthetical “(or take
   #144's provisional path)” is corrected to re-collection under the current
   epoch and retry. It repeated the D27 side of a contradiction already
   present when D28 and D29 were accepted together.
3. **D30 clause (c) — consequential update, not an erratum.** Its neutral
   pointer to issue #208 becomes a pointer to this proposed resolution. Its
   following comparison must stop grouping `UnknownEpoch` and `EpochStale`
   with `LowPopulationEpoch` once this record is accepted. D30's
   `NoStandingInCell` refusal and evaluation order do not change.
4. **D31 clause (f) — consequential update, not an erratum.** The sentence
   comparing binding-miss demotion with D27's old direction is no longer a
   valid analogy. D31 itself still demotes through `LowPopulationEpoch` with a
   resolved announcement, so its decision and arithmetic are unchanged.
5. **`intent/mod.rs` comments — implementation follow-up, neither ADR
   category.** The landed control flow is right. The module-level prose around
   current lines 736–746, the cause comments around 438–470 and 505–509, and
   the analogy around 1251–1259 still say or imply that all three causes are
   provisional. The `check_at` discussion around 1728–1751 and the match arms
   around 1872–1893 correctly identify `LowPopulationEpoch` as the sole
   trigger; the former must replace its “deliberate divergence” discussion
   with a citation to this record. Rust owners must correct the stale comments;
   this branch does not edit their files.
6. **The ADR index — administrative.** `docs/DECISIONS.md` gains D37 as
   Proposed. No accepted status changes until the owner accepts this record.

### (e) Conformance and reversal

The implementation from #206 already has the decided shape: unknown and stale
refuse; low population alone may become provisional; all three remain distinct
operator causes and collapse to the attestation-quorum wire reason. Acceptance
therefore requires comment and citation repair, not a routing change. Existing
refusal tests remain the behavioral assertions.

Reopen this decision if production telemetry shows either
`UnknownEpoch`/`EpochStale` refusals where the client held and presented a valid
announcement at materially more than noise, or post-handover refusal bursts
lasting materially longer than one retry round. The expected present-first
failure rate is approximately zero. If ordered presentation plus intent cannot
fit the existing 2,048-byte envelope and reliable path budgets, the zero-RTT
cure premise has failed and the decision must be repriced. A coordinator
partition by itself is not a reversal trigger; queued durable settlement is
the accepted netsplit behavior.

## Consequences

- An attacker cannot turn a populated cell into the low-population path by
  withholding the announcement. Required pre-commit scrutiny cannot fall from
  `K = 3` to zero on a submitter-controlled branch.
- Honest traffic pays no extra round when clients follow present-first. A
  cache miss discovered by refusal costs one failed gateway round and one
  retry round; stale recovery additionally costs one 150 ms co-sign round.
- A genuine coordinator partition stops durable settlement for peers that do
  not hold a usable announcement. Simulation and local witnessing continue;
  intents queue.
- D29 remains the sole definition of provisional eligibility. No new durable
  state, wire variant, parameter, FoundationDB family, or amplification path is
  introduced.
- This record corrects accepted history explicitly. It does not use the safer
  implementation as implicit precedence over an accepted ADR.

## Alternatives considered

- **Let D27 stand and add an announcement-free population estimate.** Rejected.
  Live presence is submitter-manufacturable, a cached previous set does not
  describe the named epoch, and a self-chosen set is the failure witnessing
  exists to prevent. Any estimate would create a second low-population
  predicate outside D29 and would need its own signed fact.
- **Provisional on `UnknownEpoch`, refuse only `EpochStale`.** Rejected. It
  rewards the cheaper and more submitter-selectable of the two triggers and
  permits `3 -> 0` pre-commit scrutiny in populated cells.
- **Provisional on stale because the gateway once trusted the announcement.**
  Rejected. Past grace, the reveal and turnover assumptions have changed, and
  D28 already gives the honest submitter a full 60-second usable window. One
  co-sign round under the current epoch is the bounded cure.
- **Refuse `LowPopulationEpoch` too.** Safe but contrary to D29's product
  decision: solo loot, crafting, and progression in legitimately sparse cells
  would stop despite a signed record proving why witnesses are unavailable.
  That would reopen D29 rather than reconcile it.
- **Call the correction an amendment because D29 was drafted separately.**
  Rejected by history. Draft order does not control accepted architecture.
  D27, D28, and D29 became normative together in `95ca344e`; the contradiction
  was present at that instant, so the accepted text was wrong rather than later
  overtaken.
