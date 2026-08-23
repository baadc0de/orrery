# ADR-0039: Bound the hot tier with pressure-triggered, durability-gated shard eviction

**Status:** Proposed · **Date:** 2026-08-23 · **Decision:** D39

This record is non-normative until accepted. See the [ADR index](../DECISIONS.md)
for precedence, scope, and the complete accepted decision set. Acceptance is
reserved to the owner.

**Supersedes:** nothing. It proposes the missing memory-bound half of
[ADR-0011]'s in-memory cell-actor tier. It reuses [ADR-0011]'s checkpoint and
recovery path, [ADR-0016]'s checkpoint cadence, [ADR-0021]'s additive public-API
rule, and the `actor/` fencing row narrowed by [ADR-0026]. It does not amend any
accepted record while Proposed.

Out of scope: island drain — [ADR-0024] decides that drain neither creates nor
repairs this gap; the `GatewayMsg::Quiesce { cell }` wire message D24 considered
and rejected; and the gateway's idle-*peer*-registry eviction. This record adds
no coordinator edge and no peer-controlled checkpoint command.

## Context

### 1. The bound claimed in prose does not exist in the tree

`CellRuntime` installs one in-memory actor per owned **shard cell**. That actor
holds every entity and spatial index beneath the shard. Once installed, it is
removed only by runtime teardown, split, or live ownership handover; there is
no idle-state lifecycle. A checkpoint persists the dirty state and advances
`ckpt/{grid}/{shard}`, then leaves the actor and all of its state resident.

The unit of this proposal is therefore the whole shard actor, called `s`
below — not one interest cell inside its maps, not an island, and not a peer
session. Partial eviction inside one actor would retain the mailbox, fence and
most indexes while inventing a second recovery unit. It is a different design.

The current resident set has the simple and unbounded shape

```text
R = sum resident_bytes(s)  for every shard actor ever activated on this node

today:    activated(s)  -> resident(s) until split, handover, or shutdown
proposed: activated(s)  -> resident(s) -> dormant(s) -> activated(s) ...
```

Calling `QuiesceSignal::quiesce(s)` changes no term in `R`. It puts `s` on an
in-process scheduler channel; the scheduler attempts a checkpoint and keeps the
actor. The method's `bool` reports only whether enqueueing succeeded. It is not
a checkpoint-completion acknowledgement and cannot be used as proof that any
watermark committed.

### 2. Durability and residency are separate promises

For a frozen actor, let

```text
tail(s) = greatest journal LSN represented by the actor's in-memory state
ckpt(s) = LSN in the last successfully committed ckpt/{grid}/{s} row

safe_to_drop(s)  =>  ckpt(s) >= tail(s)
```

The implication is one-way deliberately. A journal append is durable enough
to acknowledge a bulk write, but eviction is allowed to discard the only live
object that knows how that uncheckpointed tail composes into state. Treating
"journaled" as "safe to drop" silently turns every local eviction into a
recovery event dependent on retained journal availability. This record takes
the stricter precondition requested by the defect: the checkpoint watermark
must cover the frozen tail before memory is released.

The analogy is a dirty page cache. A clean page may be discarded and faulted
back from its backing store. A dirty page must first finish writeback; putting
it on the writeback queue is not the same event as writeback completion.

### 3. The reload is a real latency cliff

[docs/08 §3.4] already defines the only admissible load path: fence, load the
checkpoint, replay the tail, then open the mailbox. On the measured 128-shard
fresh-cluster run, activation cost **386 ms**, runtime recovery **63 ms**, and
readiness **503 ms**. Those are batch measurements, not a dishonest
`386 / 128` single-shard promise: scans, CASes and bounded concurrency do not
scale linearly.

Eviction trades resident bytes for a future page fault. If `p` is the fraction
of accesses that encounter a dormant shard and `L_load` is its §3.4 latency,
then the added mean delay is

```text
E[added latency] = p * L_load
```

and once `p >= 0.01`, a cold load can enter the access-latency p99 even when
the mean looks small. A burst touching 128 dormant shards must be compared
with the measured `386 ms + 63 ms = 449 ms` activation-and-recovery work, not
with the ordinary hot-memory path. The remaining 54 ms to the measured 503 ms
readiness line is process work outside those two reported stages.

### 4. Checkpoint pressure is measured pressure

[ADR-0023] ran the same 128 shards at three checkpoint cadences:

| cadence | approximate starts/s | `journal_commit_ms` p99 |
|---|---:|---:|
| 20 s | `128 / 20 = 6.4` | 30 ms |
| 5 s | `128 / 5 = 25.6` | 30 ms |
| 2 s | `128 / 2 = 64` | 75 ms |

The tenfold-cadence arm did not buy free durability: `journal_commit_ms` p99
moved from 30 ms to 75 ms. It ran on a device that failed D19's qualification,
so the absolute latency is not a production budget; the amplification result
is still the evidence this proposal must respect. A policy that checkpointed
every idle transition would merely move the unbounded resource from memory to
storage latency.

## Proposed decision

### (a) Trigger: pressure hysteresis, with idle and lease-free eligibility

> **A node starts an eviction round only when accounted shard-state bytes
> exceed `H = 0.80 B`, and stops when they reach `L = 0.70 B`, where `B` is a
> deployment-supplied hot-state memory budget. Within a round it considers
> whole shard actors in least-recently-accessed order. A candidate is eligible
> only after 60 s with no read, write, lease operation, or mailbox work and
> only when no live lease exists anywhere beneath that shard.**

All four values become D16 parameters if this record is accepted:

```text
HOT_STATE_BUDGET_BYTES       = B       deployment input; no universal default
EVICTION_HIGH_WATER          = 0.80 B
EVICTION_LOW_WATER           = 0.70 B
EVICTION_IDLE                = 60 s
```

The 10 percentage-point gap is hysteresis. Without it, a shard whose reload
puts `R` one byte over `H` can be evicted again immediately:

```text
without hysteresis:  R = H-1 -> load x -> R = H+x-1 -> evict -> repeat
with hysteresis:     trigger at R > .80B, reclaim through R <= .70B
                     next round needs at least .10B of net growth
```

`B` is explicit because RSS is not an actor-state budget: allocators, journal
indexes, QUIC buffers and FDB clients occupy the same process. The accounting
must include each actor's entity bytes, spatial indexes, terrain, tombstones
and mailbox-owned state. If accounting cannot attribute an object, it does not
subtract that object from the process reserve by wishful thinking.

The 60 s idle interval is three base checkpoint periods and longer than the
worst ordinary jittered interval (`20 s + 5 s = 25 s`). It gives an idle shard
at least two ordinary opportunities to become clean before pressure asks for
an extra one. No-live-leases is eligibility, not durability: it prevents the
policy from manufacturing a gameplay handoff, but does not prove the state is
checkpointed.

If `R > H` and no shard qualifies, the node reports `memory_pressure_blocked`
with resident bytes and rejection counts and sheds **new ownership
activations**; a request for already-placed dormant state remains the loader
under (d). The node does not evict a live or dirty shard to make a graph turn
green. Thus the bound is operational rather than magical:

```text
R_after <= L                         when eligible bytes >= R_before - L
R_after  = R_before                  when eligible bytes = 0
```

The second line is an alarm and admission event, never a durability exception.

### (b) Safety: freeze, checkpoint through the tail, revalidate, then drop

> **An actor may leave memory only after admission is closed, its mailbox is
> drained, and a successful checkpoint-completion result names a committed
> watermark `w` with `w >= tail(s)` for the same actor epoch. Enqueue success
> is never that result. Any intervening access, lease, epoch change, or
> checkpoint failure cancels the candidate.**

The eviction turn is serialized with routing for `s` and has this order:

```text
evict_candidate(s):
    lock lifecycle(s)
    require resident_bytes > H
    require idle(s) >= 60s and live_leases(s) = 0
    mark s Evicting; close admission; drain mailbox
    t = tail_lsn(s); e = epoch(s)

    if checkpoint_watermark(s) < t:
        await immediate_checkpoint(s) -> Completed { watermark: w, epoch: e' }
    else:
        w = checkpoint_watermark(s); e' = e

    require w >= t and e' = e
    require no access and live_leases(s) = 0 since mark
    require CAS actor[s]: Active(e) -> Dormant(e)
    shutdown actor; remove actor and accounted bytes
    unlock lifecycle(s)
```

The scheduler seam therefore gains an additive completion-bearing request;
the present `QuiesceSignal` stays public as its request half under D21, but its
current `bool` remains enqueue status only. An implementation that calls
`quiesce(s).await` and immediately drops the actor violates this clause.

Checkpoint failure reopens admission and leaves the actor resident. A partial
multi-transaction checkpoint cannot qualify because D11 commits the watermark
row last; without that final row, `w` did not advance. This reuses the existing
checkpoint commit protocol instead of inventing an `evicted=true` durability
bit.

### (c) Fencing: `Dormant` is non-serving, and reload uses §3.4's CAS

> **`FenceStatus` gains an appended `Dormant` variant. Eviction CASes the exact
> active row `Active(owner=n, epoch=e)` to `Dormant(owner=n, epoch=e)` before
> dropping memory. The next access uses the existing §3.4 activation CAS from
> that exact dormant row to `Active(owner=n', epoch=e+1)`, then loads and opens
> the actor in the existing order. No second epoch, lock, or ownership table
> is introduced.**

Appending matters because postcard encodes enum variants by index. `Dormant`
is non-serving (`FenceStatus::serves() == false`), so the durable placement row
never advertises an absent actor as live. Keeping `e` on the transition into
`Dormant` makes the transition a close, not a new writer; incrementing on
activation makes every reopened actor a new fenced incarnation.

For reload, the sole admissible state machine is

```text
Active(n,e) --checkpoint covered--> Dormant(n,e)
Dormant(n,e) --CAS exact row-------> Active(n',e+1)
Active(n,e) --stale checkpoint-----> conflict after Dormant CAS
Dormant(n,e) --two reloaders-------> exactly one CAS winner
```

If another node wins first, the local reload sees the CAS conflict and repairs
routing exactly as §3.4 does today. If the evicted actor wakes after the close,
its checkpoint transaction re-reads a non-`Active` row and fails. This is the
existing fence doing its existing job; duplicating the epoch in an eviction
registry would create two authorities and is prohibited.

### (d) Miss cost: the requesting access waits for the ordinary load path

> **The first read, write, or lease operation against a dormant shard is the
> loader and waits for §3.4 steps 1–4. Followers wait behind the same
> per-shard lifecycle future. No empty page, provisional actor, or stale cached
> state is served while activation is incomplete.**

The latency cliff is an explicit consequence, not an implementation bug. The
implementation must publish at least `evictions_total`, `dormant_shards`,
`reloads_total`, `reload_ms`, `reload_waiters`, `eviction_bytes`, and
`memory_pressure_blocked`. Promotion from shadow mode requires a load test that
reports hot-path and reload-path histograms separately and includes a
128-dormant-shard burst beside the existing **386 ms activation / 63 ms
recovery** reference. A blended p99 can hide the very cliff this policy adds.

Worked example: let `B = 40 GiB`, so `H = 32 GiB` and `L = 28 GiB`. At
`R = 34 GiB`, the round owes at least `34 - 28 = 6 GiB` of eligible actors.
Evicting five 1 GiB actors reaches 29 GiB and is insufficient; the sixth
reaches 28 GiB and closes the round. If one of those shards is touched next,
its request pays the full fence + checkpoint load + tail replay before the
mailbox opens. Six gigabytes reclaimed is not six gigabytes deleted; it is six
gigabytes converted into future load latency.

### (e) Write amplification: clean first, then one bounded forced flush

> **An eviction round consumes already-clean candidates first. Only while
> `R > H` may it force checkpoints for dirty candidates, with one forced
> checkpoint in flight and a token bucket of one start per second, burst one,
> per persistd process. The round stops issuing forced checkpoints at
> `R <= L`. Idle transitions alone never trigger a checkpoint.**

For `N` equally weighted shards at interval `C`, ordinary checkpoint starts
average `N/C`. The proposed forced-start cap `F = 1/s` bounds start-count
amplification by

```text
A_starts <= (N/C + F) / (N/C) = 1 + F*C/N

N=128, C=20s, F=1/s:
A_starts <= 1 + 20/128 = 1.15625       (at most +15.625%)

D23's 2s arm:
A_starts = (128/2) / (128/20) = 10     (+900%)
```

The comparison is intentionally starts, not bytes. Dirty sets are unequal, so
`+15.625%` is **not** a write-byte or latency guarantee. The implementation
must also report forced versus ordinary checkpoint bytes and latency, and the
eviction gate must compare `journal_commit_ms` with eviction disabled, shadow,
and enforcing. A measured regression outside the accepted storage budget
blocks enforcement; pressure then sheds activations rather than silently
raising `F`.

The cap means a 100-dirty-shard emergency takes at least 100 s to start every
forced flush. That is deliberate. Memory pressure is not authority to recreate
the 10x cadence whose measured p99 rose from 30 ms to 75 ms. Operators needing
faster relief must add capacity or explicitly change this record and D16.

## Consequences if accepted

- Hot state becomes bounded when enough idle, lease-free state exists; when it
  does not, admission sheds and telemetry says why. The record does not claim
  an unconditional RSS bound.
- The implementation adds actor byte accounting, per-shard lifecycle gates,
  `Dormant`, completion-bearing checkpoint requests, on-demand activation, and
  the metrics and shadow/enforcing gate above. **None of that is implemented by
  this Proposed record.**
- [ADR-0016] gains `B`, `H`, `L`, the 60 s idle threshold and the forced-flush
  limiter on acceptance. They are not accepted defaults while this record is
  Proposed.
- [docs/08 §8]'s current coordinator-signal and "hot memory is bounded by
  populated cells" sentence is false today. Acceptance requires rewriting it
  to distinguish the implemented checkpoint from this eviction lifecycle; the
  expansion document must not lead the ADR.
- `QuiesceSignal` remains public because D21 freezes the surface and because it
  is the request half of the proposed completion-bearing seam. If this proposal
  is rejected without a replacement eviction design, the type has no
  production caller and should be removed through the D21 process rather than
  retained as speculative surface forever.
- The permanent regression harness must prove two negative cases: a candidate
  whose `ckpt` watermark is below its frozen tail remains resident, and a
  reloader that loses the `Dormant(e) -> Active(e+1)` CAS serves nothing. It
  must also prove byte-identical reload after a covered checkpoint.

## Alternatives considered

- **Evict on the last player leaving.** Rejected three times over: population
  is not the same fact as no live lease; D24 gives the coordinator no path to
  persistd; and an idle transition would let peers manufacture checkpoints.
  Island drain is neither the trigger nor the transport for this policy.
- **Restore `GatewayMsg::Quiesce { cell }`.** Rejected as D24 rejected it: a
  peer-controlled wire request is a storage-amplification primitive. The
  request in this proposal is in-process and raised only by the node's own
  pressure controller under its own budget.
- **Evict every actor after 60 s, pressure or not.** Rejected: it maximizes
  cold-load frequency even when memory is plentiful and turns the idle timer
  into an implicit checkpoint cadence. Time ranks candidates; pressure is the
  trigger.
- **Drop after journal fsync and replay on reload.** Rejected: it weakens the
  safety precondition into dependence on the local journal's retained tail and
  makes eviction race D20/D23 retention. `ckpt(s) >= tail(s)` is simpler and
  makes the checkpoint itself the eviction proof.
- **Keep the `actor/` row Active while memory is absent.** Rejected: D26's
  serving predicate says `Active` means the named node serves the shard. A row
  that advertises a nonexistent mailbox is a false routing fact.
- **Delete the actor row on eviction.** Rejected: absence conflates "never
  assigned" with "durable state is cold", throws away placement, and invites
  bootstrap logic around a lifecycle event. `Dormant` preserves the fact and
  keeps the next epoch behind the ordinary CAS.
- **Reuse `CellActor::shutdown` without a lifecycle state.** Rejected:
  `shutdown` drains a task during runtime teardown; it proves neither the
  checkpoint watermark nor exclusion against a concurrent route. It is the
  final mechanical drop after the proposed guards, not the policy.
- **Evict individual interest cells from a shard actor.** Rejected for this
  record: the fence, mailbox, checkpoint watermark and recovery unit are the
  shard. Partial eviction would require per-subcell dirty watermarks and mixed
  hot/cold query semantics inside one actor, a larger architecture than the
  defect needs.
- **Unbounded immediate flushing until `R <= L`.** Rejected by D23's measured
  10x-cadence result. Memory pressure does not suspend the journal latency
  contract.

## Open implementation questions

The five policy questions above are settled by the proposal. These sequencing
questions do not alter their answers:

1. Whether the first implementation may introduce `Dormant` before
   [ADR-0038]'s W1 at-rest envelope work, or must sequence after it.
   `FenceStatus` is already postcard-encoded durable state, so the implementing
   change must name its absent-old-reader rule either way.
2. Which admission response carries memory-pressure retry guidance. This
   record decides that activation sheds; the exact additive protocol shape is
   implementation work under D21.

[ADR-0011]: 0011-persistence.md
[ADR-0016]: 0016-parameter-reference.md
[ADR-0021]: 0021-ruleset-distribution.md
[ADR-0023]: 0023-follower-journal-retention.md
[ADR-0024]: 0024-island-drain.md
[ADR-0026]: 0026-sibling-gateways.md
[ADR-0038]: 0038-at-rest-schema-versioning.md
[docs/08 §3.4]: ../08-persistence.md
