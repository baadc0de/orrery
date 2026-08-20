# ADR-0023: Follower journal retention and the P2 retention clause

**Status:** Accepted · **Date:** 2026-08-20 · **Decision:** D23

This decision is normative. See the [ADR index](../DECISIONS.md) for
precedence, scope, and the complete decision set.

**Supersedes:** nothing. It closes the two things
[D20](0020-journal-retention.md) recorded as open — the follower's unbounded
mirror, and a P2 gate that covered retention incidentally — exactly as D20
closed the consequence [D19](0019-indexed-waldb-journal.md) had left. D20's
decision text stays accepted in full; two of its *consequences* are discharged
here, and one sentence of [docs/13 §4.2](../13-chain-replication.md) is
replaced. D16's `journal_open_ms` parameter is unchanged and becomes enforced.

## Context

D20 bounded a primary's journal by the minimum of what its shards have
checkpointed and what its chain follower has confirmed. It wrote down what it
did not do:

> A journal holding follower provenance therefore reports
> `ReleaseBlocked::FollowerProvenance` and reclaims nothing. Bounding it needs
> the rebuilt cursor persisted as a keyed metadata row and `rebuild_cursor`
> seeded from it instead of from zero; that is the next step.

and, about the gate:

> Its load phase is 30 seconds against a 20 s ± 5 s checkpoint cadence over 128
> shards, so whether retention fires — and how often — is an accident of jitter
> rather than something the harness arranges.

Both matter more than they read. **Chain replication is on by default**, so the
shipping two-node deployment has one bounded journal and one unbounded one: the
follower's mirror, and the index rebuilt from it at every open, still grow with
uptime at D20's measured **3.94 µs and ~95 bytes per record**. At the gate's own
arrival rate (~18 000 records/s) that is the same 94 GB and 4.3-minute restart
after an hour that D20 exists to prevent — on the node whose entire purpose is
to be startable when the other one dies. And a gate that passes whether or not
a release happened is evidence that retention is *harmless*, not that it works.

### Why the obvious answer is the wrong one

The primary's floor is "what my shards have checkpointed". Transcribed to a
follower, that reads "what *its* shards have checkpointed" — and it is wrong in
two compounding ways.

**A follower folds no mirrored record into an actor.** Mirrored records are
appended to the journal (`append_replicated_indexed`) and never routed to a
cell actor, so a follower's actors have no state derived from them and their
checkpoint watermark says nothing about the mirror. Reading their empty
watermark as a floor of `0:0` is not conservative — it is meaningless — and it
is what would keep the mirror pinned even with the block removed.

**And in the landed implementation there are no actors at all.**
`run_follower` opens no runtime, no scheduler, no fence store and no gateway:
"mirrored records are its only writes". The checkpoint cadence that drives
retention everywhere else does not exist on that process.

What a promotion actually needs from the mirror is the tail the durable tier
does not already hold. The durable tier is FoundationDB, shared, and what is in
it was put there by the **primary's** checkpoints — which is exactly the
quantity the primary already computes to bound its own journal. So the floor a
follower needs is not a number it can derive; it is a number the primary
already has and does not send.

### And the cursor

The follower's dedupe cursor is reconstructed at open by walking the provenance
index from batch zero and stopping at the first gap (docs/13 §4.2). Releasing a
prefix of that index leaves a walk that finds its first gap immediately and
reports an empty cursor — a follower that believes it has mirrored nothing. The
primary then re-streams its whole journal into a second physical copy of every
record, at a reported lag of zero, which is the failure `refuse_sibling_epoch`
exists to *detect* rather than one anything repairs. The cursor is already
persisted after every batch (`set_chain_grpc_state`); it was simply never read
back as anything but a repair hint.

## Decision

**1. The primary's retention floor travels on the chain.** Every
`AppendBatch` frame and every reconnect handshake carries `primary_floor` — the
primary's own `released_floor()`, in the primary's LSN space. The replicator
reads it from the journal at each push, so the follower learns what the primary
has released at the same rate it learns what the primary has written, which is
the rate at which its mirror grows. It is one monotone LSN and the follower
takes the maximum, so a lost frame costs a cadence of retention and nothing
else. `ChainTransport::note_primary_floor` defaults to a no-op: a transport
that cannot carry the floor leaves the mirror pinned, which is the pre-D23
behaviour and is safe.

**2. A mirror is released to the local position of the first row at or above
that floor.** A mirrored record keeps its *origin* LSN inside the record and
takes an independent local key, so the two spaces do not have to agree. For
chain `c` with primary floor `F` and mirror rows `r` (each with an origin
`origin(r)` and the local position `local(r)` it was written to):

```
cut(c) = min { local(r) : r ∈ mirror(c), origin(r) ≥ F }      (journal tail if empty)
```

Chain order is journal order — a gap or a reorder is refused at append — so the
rows at or above `F` are a suffix and the cut is its first element. The floor
the release asks for is then the lower of the two authorities, each binding
only the records it is about:

```
floor = min( checkpoint_floor  if this journal holds any originated record,
             cut(c)            for every mirrored chain c )
```

A journal with no originated records is not bounded by the checkpoint floor at
all; a journal that holds one *is*, and an uncheckpointed shard still abstains
for the whole journal (D20 rule 2). `Journal::retention_floor` computes it,
because the two counts it turns on — originated records, mirrored rows — are
facts only the journal holds.

**3. The persisted cursor seeds the rebuild, and only the retained suffix is
validated.** `rebuild_cursor` starts from the durable cursor row and walks the
provenance index from the batch after it, applying the same batch-completeness,
ordinal, predecessor and span checks it always did. The seed is a starting
point and never an answer: records are durable *before* the row that names them
is written, so a cursor can be one batch behind its own index, and the walk
must still advance over every retained batch above it.

**The seed is consulted only when retention has actually removed something.**
At floor `0:0` the index still holds every batch and the provenance rows — which
are written atomically with the records themselves — remain the stronger
source; the cursor row is not read at all and this path behaves exactly as it
did before. Above a floor, a batch the seed *ends at* that survived whole must
end where the seed says it does, or the open fails loudly rather than starting
from a cursor the index contradicts.

**4. Why a release can never cut an unacknowledged batch.** D20 rule 3 clamps
the primary's floor to the follower watermark it has confirmed:

```
F ≤ W_follower = last_lsn of the last batch the follower acknowledged
```

so every mirror row below `cut(c)` belongs to a batch the follower had already
acknowledged — and the cursor is written before that acknowledgement returns.
The release can therefore truncate a batch in half (the floor is a checkpoint
watermark, not a batch boundary) but never one the persisted cursor has not
already passed. That is why an incomplete batch *below* the seed is expected
rather than a gap, and why one *above* it is still a stop.

**5. Two named refusals replace one.** `ReleaseBlocked::MirrorCursorAbsent`:
the journal mirrors a chain with no durable cursor row to seed from — the shape
a pre-D23 binary leaves — and is not released. `ReleaseBlocked::MirrorLag`: the
chain's primary has advertised no floor, or none past the floor already in
force, so that mirror is pinned where it is. Both are outcomes reported on the
cadence, not errors: a journal that is not shrinking has to be able to say
which precondition is holding it.

**6. Promotion adopts a released mirror.** `adopt_chain_history` walks the same
index and is seeded the same way, from the same cursor. Without that, the first
follower release would turn every later promotion into "cannot adopt chain
history with a batch gap" — a node that refuses to start rather than one that
starts short. The adopted cutoff is unchanged: the seed's watermark *is* the
released prefix's end, so a promotion reports the same `recovery_cutoff` it
would have without retention.

**7. A passive follower drives retention on a timer.** `run_follower` spawns
`spawn_mirror_retention`, which calls the same release on the same cadence the
checkpoint scheduler uses — with no local floor at all, because on that process
there is nothing to checkpoint. `--no-journal-retention` turns it off there
exactly as it does on a primary.

**8. Retention is a mandatory P2 gate clause.** `scripts/p2-kill9-gate.sh` now
runs every node at a cadence its load phase outlasts
(`--checkpoint-interval-ms`, **5 000 ms** in the harness — measured, see below)
and fails unless:

- **both** nodes' own reporters recorded a `journal_retention` record with at
  least one release and a floor past `0:0` — the primary bounded by its
  checkpoints, the follower by the floor the primary sent it;
- every node's `journal_open_ms`, reported on its readiness line, is under
  D16's **2 000 ms** budget;
- and the recovery verifier still matches every pre-crash acknowledgement,
  which is the clause the other two exist to keep honest.

The cadence flag is a harness lever, not a claim about the deployed cadence:
the mechanism under test is the release, and the release is driven by
checkpoint rounds either way. Both clauses are mutation-checked in the script's
offline `--self-test`, which runs per commit.

**9. Retention state is telemetry, on both roles.** `persistd --metrics-jsonl`
emits a `journal_retention` record whenever the floor, the release count, the
dropped-record count or the blocking reason changes — absolute totals, because
a gauge that reset every interval would hide a floor that stopped moving an
interval ago, which is the shape the failure has. The readiness line carries
`journal_open_ms`, measured inside `Journal::open` by the node that paid it
rather than derived from two log timestamps.

### What the gate says

Three arms on this repository's self-hosted box, same binaries, same seeded
world, run back to back against a throwaway single-node FoundationDB
(2026-08-20). The cadence is the only variable. Every figure comes out of the
runs' own artifacts, collected in
[`docs/data/p2-follower-retention-2026-08-20.json`](../data/p2-follower-retention-2026-08-20.json):

| cadence | primary releases | follower releases | promoted `journal_open_ms` | `journal_commit_ms` p50 / p99 | retention clause |
|---|---:|---:|---:|---|---|
| 20 s (D16's own) | 30 | **0** | **2 905 ms** | 8 ms / 30 ms | **fails** |
| **5 s** | 237 | 5 | 764 ms | 8 ms / 30 ms | passes |
| 2 s | 140 | 10 | 300 ms | 15 ms / 75 ms | passes |

Four things come out of it.

**The unbounded mirror breaks the budget in thirty seconds, and this is the
measurement rather than the extrapolation.** In the arm where the follower
released nothing, the promoted node's `Journal::open` took **2 905 ms** —
past D16's 2 000 ms — after a *thirty-second* load. Bounding the mirror took
that to 764 ms and then to 300 ms as the floor advanced harder. D20 could only
reach this number by extrapolating its slope; here it is, paid by the one node
whose whole purpose is to open quickly when the other one dies.

**The clause fails when retention does not happen, which is the point.** At
D16's own cadence a 30-second load does not contain one follower release: the
primary's first floor needs all 128 shards to have checkpointed once (~20 s),
and the follower's own timer has already fired by the time that floor reaches
it. Two cadences of lag do not fit in the window. The run stopped with
`follower: retention released nothing (blocked: already released to this
floor)` — a configuration that says nothing about retention, refused rather
than counted.

**The harness cadence is measured, not picked.** At 2 s the clause passes and
the *measurement* degrades: 128 shards checkpointing ten times as often is ten
times the checkpoint write traffic on a device that already cannot hold its
offered IOPS, and the p99 this gate judges goes from 30 ms to 75 ms. 5 s buys
the clause without moving the number — p50 and p99 identical to D16's own
cadence on this box — so 5 s is what the harness sets.

**Retention was active and recovery was still exact.** The 2 s arm released the
primary 140 times and the follower 10, dropping 487 269 and 469 115 index
entries, and its promoted node then verified **every pre-crash
acknowledgement**: `pass`, 534 000 eligible durable acks, 10 000 bulk rows and
15 838 intents checked, against a mirror whose prefix had been released out
from under it ten times and whose chain history was adopted from the persisted
cursor. The bumped-epoch refusal fired on that same released directory.

**What these arms do not establish.** Every one of them fails the 2 ms
`journal_commit_ms` budget, because this box's bare `fdatasync` p99 is 7.045 ms
and it fails D19's device qualification outright — exactly as D20 recorded for
both of *its* arms on the same machine, retention on and off. The latency
verdict here is the device's, not retention's; whether the strengthened
criterion passes end to end is the next nightly's answer, on a qualified host.

## Consequences

- **The two-node deployment is bounded on both nodes.** A follower's mirror now
  holds the primary's unreleased window plus one retention cadence of lag,
  instead of everything the primary ever wrote. Its restart cost stops tracking
  uptime for the same reason a primary's did (D20), and the node that has to be
  startable on demand is the one this was still false for.
- **A follower's mirror is now bounded by a number another process sends it.**
  That is a real coupling and it is the correct one — the mirror exists to
  complete what the durable tier holds, and only the primary knows what that
  is — but it means a primary that stops advertising (an old binary, a chain
  that never pushes) leaves the mirror pinned. That is reported as
  `MirrorLag`, not silence.
- **The wire format gains two optional fields**, and the on-disk format gains
  nothing. `AppendBatchRequest` and `ReconnectRequest` are postcard structs, so
  a pre-D23 node cannot decode a D23 frame: **the chain upgrade is one-way and
  the follower must be upgraded first** (it ignores nothing it cannot parse —
  it refuses the frame). Since the follower is the passive half and is already
  restarted before the primary in every runbook, that is the existing order.
  No `RawEntry` variant is added, so a D20 journal opens unchanged here.
- **`ReleaseBlocked::FollowerProvenance` is gone**, replaced by the two
  variants in rule 5. Anything matching on it fails to compile, which is the
  intended outcome: its meaning ("a mirror is never released") is no longer
  true.
- **The dedupe index carries the local position in memory.** `chain_records`
  values became `{provenance, local}` so a release can prune the rows whose
  records it dropped — the key holds the *origin* LSN, which is not comparable
  with a floor. Both halves come out of the one `RawEntry::Record` that carried
  the record and its provenance, at commit and at recovery alike, so this is
  one `Lsn` of index memory per mirrored record and no second durable copy.
- **A replayed batch below the floor is refused, not re-mirrored.** The rows
  that would prove it a duplicate are gone, so the follower answers
  `failed_precondition` rather than appending a second physical copy. It is
  unreachable in the protocol — the primary resumes from the watermark the
  follower reports, which is above the floor — and it is a refusal rather than
  a silent second copy precisely because the alternative is the ambiguity that
  makes promotion impossible.
- **The gate now consumes a cadence parameter**, so a failure at
  `--checkpoint-interval-ms 2000` that would pass at 20 s is a real difference
  in load: 128 shards checkpointing ten times as often is ten times the
  checkpoint write traffic against FDB, competing with the same device the
  journal is on. The latency clause is unchanged and still judges
  `journal_commit_ms` p99 against D16's 2 ms, so a cadence that broke the
  measurement would fail the gate rather than quietly relax it.
- **A promoted node's floor check becomes load-bearing.**
  `CellRuntime::open` refuses to open when a shard's checkpoint watermark is
  below the journal's retention floor (D20 rule 4). On a promoted node the
  watermarks come from FDB rows the *primary* wrote, in the primary's LSN
  space, while the floor is a position in the mirror's own — and the two
  coincide by construction on a pure mirror (the same records, the same
  `encoded_len`, both cursors starting at `0:0`), which is the same
  coincidence the replay's `covers(position)` test already rests on. What D23
  changes is that this floor was previously always `0:0` on a follower, so the
  check was vacuous there and is not any more. The cut itself does not depend
  on the coincidence — it is read out of the mirror rows, which carry both
  positions — and the boundary case is exact rather than lucky: the floor a
  primary releases to is a record it retains, and the mirror row for that same
  record is the one the cut lands on. Making the promoted node's comparison
  translate between the two spaces, rather than assume they agree, is the
  cleanup this names and does not take.
- **`journal_open_ms` stops being a number nothing checks.** It is D20's
  budget, enforced at the three opens this harness performs.

## Alternatives considered

- **Let the follower checkpoint its mirror into its own durable tier.** This is
  the shape that makes a follower's floor local, and it is a different system:
  a passive follower would need a runtime, a fence store and write access to
  the same FDB keyspace the primary owns, which is precisely the ownership the
  chain design exists to keep single-writer (docs/13 §2). Rejected as inverting
  D11 §6's fence, not merely as cost.
- **Keep the mirror pinned until the P6 archive tailer lands.** Rejected for
  the reason D20 rejected the same argument for primaries: the archive is a
  precondition for *keeping history*, not for the journal being bounded, and
  deferring leaves the default deployment with one unbounded journal for the
  whole of P5.
- **Express the floor as a batch sequence rather than an LSN.** Tempting,
  because batches are the follower's own unit and a batch boundary never cuts a
  record. Rejected: the primary's floor is a checkpoint watermark, which has no
  batch to be the boundary of, and mapping it to one on the primary would mean
  the primary tracking which batch carried which record — state the follower
  already has, in the index this uses.
- **A dedicated `SetFloor` RPC.** Rejected as a message whose delivery would
  need its own retry and ordering rules to carry a value that is idempotent,
  monotone and already piggybacked on a frame that has both.
- **Releasing the mirror on the follower's own timer, without a primary
  floor** (keep the last N minutes). Rejected for D20's reason, which does not
  weaken here: a time or size window is not a correctness bound, and the only
  defensible floor is what some durable tier has taken responsibility for.
- **Validating the whole provenance index against the seed.** Rejected: below
  the floor there is nothing to validate against, and above it the index is
  already the authority. What is checked is the one overlap that can disagree —
  the batch the seed ends at, when the release kept it whole.
