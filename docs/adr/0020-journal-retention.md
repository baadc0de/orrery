# ADR-0020: Journal retention and the recovery budget

**Status:** Accepted; residual closed by
[ADR-0023](0023-follower-journal-retention.md) · **Date:** 2026-08-20 ·
**Decision:** D20

This decision is normative. See the [ADR index](../DECISIONS.md) for
precedence, scope, and the complete decision set.

**Supersedes:** nothing. It closes a consequence
[D19](0019-indexed-waldb-journal.md) recorded and deliberately left open, and
adds two parameters to [D16](0016-parameter-reference.md)'s table; the rest of
both records stays accepted.

## Context

D19 made the indexed wal-db journal the default and wrote down what it did not
yet know:

> Opening the journal rebuilds indexes in one forward WAL scan. Startup work
> and index memory are therefore linear in retained journal metadata and
> records. Segment retention and future persisted index footers must be
> measured before treating arbitrarily old journals as free to open.

Two facts about the implementation as it stood:

- **`truncate_before` was never called.** The string appears nowhere in
  `crates/orrery_persistd/src/`. Neither backend dropped a segment, and the
  Fjall fallback never deleted a record either. A node's journal grew for as
  long as the node ran.
- **The archive tailer that docs/08 §14 makes the gate for deleting segments
  is a P6 deliverable and does not exist.** So there was no mechanism whose
  absence explained the growth: the growth was simply unbounded.

### The measurement

`crates/orrery_persistd/tests/journal_open_scaling.rs` grows one journal in
50 000-record steps, closing and reopening it between them, and reports what
the reopen costs at each cumulative size. Measured on a local NVMe
(`CT2000T700SSD3`, ext4), 1 400-byte payloads, warm page cache:

| records | on-disk | index rebuild (`Journal::open`) | full replay scan | RSS after open |
|---:|---:|---:|---:|---:|
| 50 000 | 74 MB | 197 ms | 175 ms | 9.2 MB |
| 200 000 | 296 MB | 789 ms | 689 ms | 21.0 MB |
| 400 000 | 592 MB | 1 575 ms | 1 376 ms | 39.8 MB |
| 600 000 | 887 MB | 2 362 ms | 2 081 ms | 59.2 MB |

Linear, with no inflection — the worst step deviates from the fitted slope by
0.7%: **3.94 µs and ~95 bytes of index per record**, or 2.66 ms of open per
megabyte of journal. The scan column is what a recovery adds on top when its
checkpoint watermark is older than the whole journal.

Every figure in this section is re-derived from
[`docs/data/p2-journal-open-2026-08-20.jsonl`](../data/p2-journal-open-2026-08-20.jsonl)
and its host record by `scripts/p2-journal-open-report.py`, whose
mutation-checked `--self-test` runs in `scripts/check.sh`. The arrival rate the
extrapolation below uses is read from D19's own gate evidence rather than
transcribed, so the two records cannot drift apart. The host is a developer
workstation (Ryzen 9 9950X3D, `CT2000T700SSD3` NVMe, ext4 `noatime`), not the
qualified `c4d-standard-32-lssd` the D19 pairs ran on: what it supports is a
slope, not an absolute.

The curve is a **floor**, not a worst case: it was taken with the journal in
page cache, and a restart after a host failure reads from the device.

### What the slope means at the load the gate already runs

The P2 kill-9 gate's load phase is 30 seconds and produces ~540 800 durable
acknowledgements and ~780 MB of journal
(`docs/data/p2-journal-raw-2026-08-20.jsonl`) — **~18 000 records/s, ~26 MB/s**.
Extrapolating the slope at that rate, on a node that never releases anything:

| uptime | journal on disk | index rebuild at open |
|---|---:|---:|
| 1 minute | 1.6 GB | 4.3 s |
| 1 hour | 93.7 GB | 4.3 min |
| 1 day | 2.25 TB | 1.7 h |

That is a stress rig rather than a modelled population, and the honest reading
is the shape, not the hour: **restart time and journal disk both scale with
total uptime**, and neither has a bound or a budget to fail against. A node
that has run long enough cannot be restarted inside any operational window,
and the failure arrives as a slow drift rather than as an error.

### What the gate says

The P2 kill-9 gate was run with retention on and off — one binary, one host,
one build, the arms differing only by `persistd --no-journal-retention`, which
this change adds precisely so the comparison is a flag rather than a rebuild.

**On a qualified host, the gate passes with retention on.** Four alternating
arms on an ephemeral `c4d-standard-32-lssd` in `us-central1-b` — the shape
D19's Phase 4 pairs ran on — with the journal on local NVMe (ext4 `noatime`)
and FoundationDB 7.3.77 beside it. The host cleared D19's own `fio`
qualification first, on an idle box: 470 IOPS sustained per job at
**`fdatasync` p99 0.06 ms, max 0.17 ms**, against a requirement of max < 1 ms.

| arm | gate | `journal_commit_ms` p99 | max | recovery | releases |
|---|---|---:|---:|---|---:|
| retention **on** | **pass** | 1 ms | 4 ms | pass | 13 |
| retention off | pass | 1 ms | 15 ms | pass | 0 |
| retention off | pass | 1 ms | 20 ms | pass | 0 |
| retention **on** | **pass** | 1 ms | 50 ms | pass | 17 |

Retention was demonstrably *active* in both of its arms rather than idle: 13
and 17 releases inside a 30-second load phase, the first of them dropping
233 208 records out of the index. Every arm's recovery verifier passed against
every pre-crash acknowledgement — 10 000 bulk rows and ~15 900 intents checked
against ~541 000 eligible durable acks — so an acknowledged write survived a
`kill -9` on a journal that had been released out from under it seventeen
times.

The maxima column is left in and deliberately not summarized: each is a single
sample out of ~535 000, they scatter in both directions (4 ms and 50 ms with
retention, 15 ms and 20 ms without), and they support no ordering between the
arms. What the gate judges is the p99, and that is 1 ms in every arm. Evidence:
[`docs/data/p2-retention-gate-2026-08-20.json`](../data/p2-retention-gate-2026-08-20.json).

One methodological note worth carrying: an earlier `fio` on the same host,
taken *while a build was competing for the device*, reported the same p99 with
a **143 ms** maximum. The qualification measures contention as readily as it
measures the device, so it belongs on an idle box — before the load, not beside
it.

**The same comparison on an unqualified host says the same thing, negatively.**
Four arms on the self-hosted runner failed the 2 ms `journal_commit_ms` budget
identically with retention on (9 ms p99) and off (8–9 ms p99), because that
box's bare `fdatasync` p99 is **7.045 ms** with a 104 ms maximum and it cannot
hold the offered 470 IOPS — it fails D19's qualification outright. Retention
did not create that tail and turning it off did not remove it. Evidence:
[`docs/data/p2-retention-control-2026-08-20.json`](../data/p2-retention-control-2026-08-20.json).

**One thing the gate does not do well.** Its load phase is 30 seconds against a
20 s ± 5 s checkpoint cadence over 128 shards, so whether retention fires — and
how often — is an accident of jitter rather than something the harness
arranges. It fired 14–16 times in these runs and 0 in one earlier run on the
other host. Making retention a *covered clause* rather than an incidental one
is follow-up work — **done in [D23](0023-follower-journal-retention.md)**,
which sets the cadence the harness needs and fails the run unless both nodes'
floors advanced and every journal open came in under the budget below.

## Decision

**1. The journal is bounded by a retention floor, and the checkpoints set it.**
`Journal::release_before(lsn)` drops every record below `lsn` from the index and
reclaims the segments that hold only released records. The checkpoint
scheduler calls it once per cadence round with the floor its checkpoints
establish. `CheckpointConfig::retention` defaults to **on**.

**2. The floor is the minimum over the shards a node hosts, and an
uncheckpointed shard abstains.** A record is releasable only once *every* shard
that could still need it has folded it into a durable checkpoint. A shard that
has never checkpointed contributes no floor at all rather than being skipped —
its whole history is still delta.

**3. A chain follower's watermark bounds the floor too.** A follower that falls
behind resumes by rescanning the *primary's* journal from its own watermark, so
the release point is clamped to what the follower has confirmed durable. A
chain that is registered but has not yet probed blocks release entirely, as
does a promotion-adopted chain, whose watermark is in the source's LSN space
and is not comparable with this journal's. A registration lasts for the life of
the journal: a chain that has stopped keeps its claim, because an unreachable
follower that is behind is precisely the one a release would strand.

**4. Crossing the floor is an error, never a short answer.**
`JournalError::Released { requested, floor }` fails a scan that starts below the
floor. `CellRuntime::open` reads from the floor rather than from zero and
refuses to open when any shard's checkpoint watermark is below it — that
combination is a checkpoint older than its own journal, and serving it would be
silent data loss. A shard with no coverage claim at all is skipped rather than
refused, because rule 2 means it can only have appeared after the release.

**5. No WAL call happens under the index lock.** The group committer appends,
syncs, and *then* takes the index write lock to record where the records
landed; so the lock order is WAL first, index second, for the release path too.
A release that held the index lock across its own `append`/`sync` inverts that
— the committer waits for the index while the release waits for the WAL — and
the two wedge. That is not hypothetical: the first implementation did exactly
this, survived every single-threaded test, and hung the workspace suite. The
release therefore snapshots what it needs under a short read lock, does its WAL
work with no lock held, and takes the write lock again only to prune. A
separate mutex serializes releases against each other, which is what makes the
short sections safe. `releases_interleave_with_concurrent_appends` holds it,
with its deadline enforced on the test's own thread rather than by a future on
the wedged runtime.

**6. The release is durable before it is destructive.** The order is: take the
physical cut; re-anchor the keyed metadata the surviving suffix needs (chain
state, adoption markers, the latter pruned of records the release drops); write
a release marker carrying the floor, the next LSN and the committed watermark;
`sync`; and only then drop segments and prune the index. A crash anywhere
before the barrier reopens at the old floor with a duplicate copy of some
metadata, which replay folds idempotently. The marker carries the two positions
that would otherwise be *derived* from records that are no longer there —
without them a journal released to empty reopens at 0:0 and re-mints LSNs it
has already acknowledged to clients.

**7. Two D16 parameters.** `journal_retention` (default on) and
`journal_open_ms` — a **budget of 2 000 ms** for `Journal::open` on a node
within its retention floor, which is the measured cost of ~500 000 records and
a deliberately loose ceiling over the ~20 s of records a 20 s checkpoint
cadence can leave behind. It is a budget to be measured against, not a
mechanism: nothing enforces it yet, and the first thing that should is the P2
gate's recovery phase.

**8. Retention is switchable.** `persistd --no-journal-retention` turns it off,
which is what a bisect needs — one binary, one host, one build — and what an
operator needs when a journal has to be kept for forensics. It is not a tuning
knob: with it off, both journal disk and the index rebuilt from it at every
open grow with the node's uptime.

**9. The Fjall fallback does not implement retention.** It answers the release
call with `ReleaseBlocked::Unsupported` rather than refusing it, so the driver
runs identically under either backend and a Fjall journal that never shrinks
reports a reason. D19 keeps that backend as a rollback path, not as a second
shipping configuration; carrying a second durable retention mechanism with its
own crash ordering there would be cost paid on a path that does not meet the
P2 latency criterion anyway.

## Consequences

- **Restart cost stops tracking uptime and starts tracking the checkpoint
  cadence.** With a 20 s cadence (D16) and the gate's arrival rate, the
  retained journal is on the order of 20 s of records — under a gigabyte, an
  open of well under a second — rather than however long the node has been up.
- **The event history in released records is not archived anywhere yet.**
  docs/08 §14 requires local segments to survive until an archive object is
  verified (R7), and the archive tailer is a P6 deliverable. Until it exists,
  a released record is gone. This is the journal disk holding
  "minutes-to-hours", which is what §14 specifies it holds — but it is a real
  change from the accidental "everything, forever", and anything that wants the
  full event history must land the tailer first. The tailer, when built,
  contributes one more watermark to the same minimum; it does not need a
  different mechanism.
- **A follower's own mirror is still unbounded (the residual — closed by
  [D23](0023-follower-journal-retention.md), which sends this primary's floor
  down the chain and seeds the cursor from the durable row).**
  `chain_grpc::rebuild_cursor` reconstructs a follower's durable cursor by
  walking its provenance index from batch zero and stopping at the first gap,
  so releasing a prefix of that index would rebuild an empty cursor and cost a
  full re-stream — the failure `refuse_sibling_epoch` exists to catch. A
  journal holding follower provenance therefore reports
  `ReleaseBlocked::FollowerProvenance` and reclaims nothing. Bounding it needs
  the rebuilt cursor persisted as a keyed metadata row and `rebuild_cursor`
  seeded from it instead of from zero; that is the next step, and it is
  deliberately not taken in the same change as the primary's.
- **The on-disk format gains a variant.** `RawEntry::Release` is appended to
  the `RawEnvelope::V1` enum. New readers read old journals unchanged; an
  **older binary cannot read a journal a newer one has released**, because
  postcard resolves enum variants by index. Per D19 this is the compatibility
  review: the upgrade is one-way, and a rollback to a pre-D20 binary requires a
  drain and checkpoint rather than a restart.
- **`CheckpointTarget::checkpoint` and `CellRuntime::checkpoint_shard` return
  the watermark they wrote** instead of `()`. Reading it back out of the
  durable tier would cost a store round trip per shard per cadence.
- **The P2 gate holds with retention on** — 2/2 arms on a qualified host,
  alternating with 2/2 controls, every recovery verified. What the gate does
  *not* yet do is require a release to have happened: it covers retention
  incidentally, on checkpoint jitter, so it is evidence that retention is
  harmless rather than a clause that would fail if retention broke.
- **Retention is measured by two harnesses, not one.**
  `journal_open_scaling.rs` is the `#[ignore]`d curve above;
  `journal_retention.rs` runs on every commit and holds the properties —
  the index bound, the loud refusal below the floor, LSN monotonicity across a
  full release, the floor surviving a journal that was never closed, the chain
  clamp, and (as its own `#[ignore]`d arm, since wal-db reclaims whole 128 MiB
  segments) that the disk actually comes back.

## Alternatives considered

- **Persisted index footers instead of retention** (D19 names them as the other
  half of the sentence): a per-segment index footer would cut the *rebuild*
  cost without bounding the *disk*, and it does not remove the growth of index
  memory at open. Retention bounds both. Footers remain worth having later for
  the retained window itself; they are not a substitute.
- **Retention off by default until the archive tailer lands.** Rejected: it
  leaves the shipping default with an unbounded journal for the whole of P5,
  which is the state this decision exists to end. The archive is a *precondition
  for keeping history*, not a precondition for the journal being bounded, and
  §14 already scopes the journal disk to minutes-to-hours.
- **A time- or size-based retention window** (keep the last N minutes or N GB).
  Rejected: neither is a correctness bound. The only defensible floor is what
  the durable tier and the follower have actually taken responsibility for, and
  that is exactly what the checkpoint watermark and the follower watermark
  report.
- **Releasing on a timer, independent of checkpoints.** Rejected: the floor can
  only move when the shard holding it lowest checkpoints, so a timer would add
  a cadence whose useful firings are a subset of the checkpoint rounds'.
