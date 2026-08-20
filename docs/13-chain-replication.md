# 13 - Chain Replication: Cross-Process Journal Mirroring

This document describes the P2 exit design for journal-chain replication in `orrery_persistd`: one primary process appends and a single follower process mirrors the journal so the bulk write path keeps its low-latency ack contract while improving node-loss recovery. It is an implementation-facing design note, not a normative source. If it conflicts with [ADR-0011](adr/0011-persistence.md) or [ADR-0012](adr/0012-backend-services.md), the applicable ADR wins.

Normative context:

- [08-persistence.md](08-persistence.md) for the journal, actor, checkpoint, and durability model.
- [09-services-and-ops.md](09-services-and-ops.md) for the persistence service's operational envelope.
- [10-crates.md](10-crates.md) for the `orrery_persistd` crate boundary and gRPC/tonic posture.
- [11-roadmap.md](11-roadmap.md) for the P2 demo gate and the phase sequence that this design must satisfy.

## 1. Scope

**In scope**

- A two-process replication topology: one primary `persistd` instance owns the journal for a shard set and one follower instance receives its committed records.
- A tonic/gRPC protocol for streaming committed journal records, acknowledging durable follower progress, and resuming after reconnect.
- Durable dedupe and restart reconstruction on the follower.
- Recovery and reconnect behavior when either side restarts or the network drops.
- Operational visibility, alarms, and rollout gates.

**Out of scope**

- Multi-follower quorum replication.
- Dynamic leader election or consensus.
- Per-record synchronous confirmation in the primary ack path.
- Cross-region write coordination.
- Replacing the persistence architecture described in [08-persistence.md](08-persistence.md).

The intent is narrow: make the existing async follower path a production-shaped recovery layer, not a second durability system.

## 2. Topology and ownership

The exit criterion uses a static two-process topology:

```mermaid
graph LR
    P[Primary persistd] -->|tonic/gRPC stream| F[Follower persistd]
    P -->|local journal ack| C[Clients]
    F -->|durable watermark| P
```

Rules:

- Exactly one primary owns a shard set at a time.
- Exactly one follower is designated for that primary at a time.
- The follower does not accept writes for the shard set it mirrors.
- The primary never waits on the follower to issue its client ack; the follower is downstream of the ack contract.
- Shard ownership must be fenced before a different process is allowed to serve the same shard set.

This matches the journal-first posture in [08-persistence.md](08-persistence.md) §4 and the two-process service shape in [09-services-and-ops.md](09-services-and-ops.md).

### Ownership constraints

- The primary is the sole source of ordering for a journal stream.
- The follower only appends records that the primary has already committed.
- A follower watermark is advisory for recovery and alarming, not a commit dependency.
- A given shard set is never replicated by more than one active follower in this design.

That keeps the failure model simple: one writer, one mirror, one recovery source.

## 3. gRPC protocol

Use tonic/gRPC for node-to-node transport, with one long-lived bidirectional stream per `(primary_id, follower_id, shard_set, epoch)` tuple. The durable chain identity is distinct from an ephemeral connection session, so reconnects preserve dedupe state without silently crossing ownership streams.

### 3.1 Stream identity

Suggested envelope:

```rust
pub struct DurableChainId {
    pub primary_node: NodeId,
    pub follower_node: NodeId,
    pub shard_set: ShardSetId,
    pub epoch: u64,
}

pub struct ChainSession {
    pub chain: DurableChainId,
    pub session_nonce: u128,
}
```

`DurableChainId` must change when:

- the ownership epoch changes,
- the shard set is reassigned,
- the follower is replaced,

`session_nonce` changes on every reconnect, but never participates in durable
dedupe or watermark lookup.

This prevents a stale primary from resuming onto a new follower session without an intentional restart handshake.

A follower reopened under a *different* epoch of the same chain family
(primary, follower, shard set) is refused rather than forked: rebuilding an
empty cursor under a fresh key would take a silent full re-stream into a second
physical copy of every record, at a healthy zero-byte lag, and leave promotion
permanently ambiguous. The refusal keys off two durable traces, and it needs
both:

- the mirrored-record provenance index, which carries a row per record; and
- the chain-state row, written by every follower load — **including a load that
  went on to receive nothing**.

The second was added after `scripts/p2-kill9-gate.sh`'s
`prove_epoch_fork_refused` leg walked straight through the refusal on
2026-08-17. Its load produced no durable writes at all (see §6 below), so the
mirror was empty, the record index held nothing, and the follower reopened at
the bumped epoch and served it. The proof was conditional on the traffic ahead
of it; opening the mirror is itself the durable fact the epoch is pinned to.

### What an empty mirror means

The chain is downstream of the journal ack path, and the journal is downstream
of the gateway's authority fence. A record that is never appended is never
mirrored. **An empty follower mirror after a load is therefore evidence about
the writes, not about chain replication** — check the primary's durable-ack
count and its `journal_commit_ms` samples before looking at the chain at all. A
primary that mirrored nothing because it committed nothing logs no chain line,
reports no `ChainFault`, and is behaving correctly.

#### Measured 2026-08-17: what stopped the writes

Traced end to end on a private single-node FDB (a second container on 4521, so
the box's shared cluster was only ever read from), release binaries, the gate's
`ci` seed profile, 15 s of load. The run never reached the chain at all: the
rig aborted in its claim phase with `gateway denied the lease claim for
PersistId(23) … NotEligible`, and the ack log was empty. The chain of causes,
each link read out of the tree at the cited line:

- `orrery-seed` commits `world/{grid}/{cell}/{entity}` rows and *deliberately*
  no `ckpt/{grid}/{shard}` row (docs/12-world-seeding.md §11.4). Confirmed on
  the cluster after seeding: 100 `w` keys, zero `c` keys.
- `FdbCheckpointStore::load` (`checkpoint/fdb.rs:391`) reads the `ckpt/` row
  *first* and returns `None` when it is absent — the `world/` scan that
  rebuilds `entities`/`by_cell` is inside the `Some` branch. So a seeded world
  is invisible to recovery: the actor comes up with an empty bag.
- A claim is granted only if it is `plausible`, and `plausible` requires
  `router.committed_entity_cell()` to resolve the entity to the claimed cell
  (`gateway.rs:2861`). That resolver asks the durable lease index and then the
  live actors (`cluster.rs:252`); `ColdFallbackRouter` forwards it verbatim to
  the live router (`cluster.rs:781`), so the cold tier — the one place the
  seeded world exists — is never consulted. Every claim is denied.
- `route_session_diff` fences every bulk write unconditionally, so with no
  lease nothing is appended, nothing is mirrored, and the follower's data
  directory stays at its empty-boot size.

The counter-experiment isolates it to that one missing row. Same binaries,
same cluster, same seeded world, with a single synthetic `ckpt/{0}/{ROOT}`
value planted by hand (node 1, watermark `0:0`, epoch 1) and nothing else
changed: **2 795 durable bulk acknowledgements**, a follower mirror holding
exactly those 2 795 records (all `ComponentDiff`, `epoch 1`, `grid 0`, read
back by scanning the follower journal directly), and
`prove_epoch_fork_refused` passing on it. Chain replication moved every
acknowledged record; nothing under `journal/` was ever at fault.

The repair therefore belongs upstream of the journal, and there are two honest
places for it — recovery seeding a shard from `world/` rows when the subtree
has rows but no checkpoint watermark (the watermark is then `0:0`, i.e. replay
the whole journal on top, which is exactly right), or `committed_entity_cell`
falling back to the cold reader. Planting the row from the harness is *not* one
of them: `scripts/p2-kill9-gate.sh` would then be proving durability against a
world it had doctored, which is the one thing the gate exists to refuse.

### 3.2 Append batch

Records move in batches so the transport amortizes framing and syscalls:

```rust
pub struct AppendBatch {
    pub session: ChainSession,
    pub batch_seq: u64,
    pub first_lsn: Lsn,
    pub records: Vec<JournalRecord>,
}
```

Rules:

- `records` are in primary LSN order.
- `batch_seq` is monotonic per stream and lets the follower reject reordered or replayed batches cheaply.
- The follower appends the records verbatim to its own journal.
- Batching is a transport optimization only; record ordering remains the journal's ordering.

### 3.3 Durable ACK and watermark

The follower replies after the batch is durably appended:

```rust
pub struct DurableAck {
    pub session: ChainSession,
    pub batch_seq: u64,
    pub durable_through: Lsn,
    pub follower_watermark: Lsn,
}
```

Meaning:

- `durable_through` is the highest primary LSN in the batch that is now safe on the follower.
- `follower_watermark` is the highest origin LSN the follower has durably persisted overall.
- The primary uses the watermark to advance lag gauges and to decide the replay start point after reconnect.

The primary must treat the watermark as monotonic per stream. If an ACK arrives with a lower watermark than the last seen one, the stream is stale and must be restarted.

## 4. Follower dedupe and restart reconstruction

The follower must survive duplicates and restarts without inventing a new write history.

### 4.1 Durable dedupe

The follower dedupes by durable stream identity plus primary LSN:

- `(durable_chain_id, record.lsn)` is the stable identity of a mirrored record.
- A record already persisted in the follower journal must be rejected as a harmless duplicate.
- Duplicate batches are expected during reconnects and retries; they are not exceptional.

Implementation consequence:

- The follower keeps a compact durable index or journal scan cursor for the highest contiguous primary LSN it has persisted for each active stream.
- The dedupe state must survive follower restarts, either by being reconstructed from the follower journal or by being stored alongside it.

### 4.2 Restart reconstruction

On follower restart:

1. Open the local journal.
2. Rebuild the highest contiguous watermark per durable chain from durable records.
3. Rebuild the dedupe cursor from the same durable source.
4. Resume accepting `AppendBatch` messages only after the durable chain identity is revalidated.

On primary restart:

1. Load the last locally committed LSN.
2. Query the follower's watermark for the previous stream.
3. Reject a watermark ahead of that committed LSN. It cannot be a resume point — the follower holds history this primary never wrote — and accepting it silently empties every batch and parks the replicator at a reported lag of zero. This is a named fault, not a retryable transport error.
4. Scan its local journal from `follower_watermark + 1` and resend that tail.
5. Replay any unconfirmed tail records as idempotent resends.

This is why the protocol needs a watermark rather than a simple last-seen batch id: the recovery point is an LSN, not a transport message counter.

**And it is why the follower's watermark bounds journal retention
([D20](adr/0020-journal-retention.md)).** Step 4 above rescans the *primary's*
journal from `follower_watermark + 1`, so a primary that has released records
below that point cannot resend them: the follower would be unrecoverable
rather than merely behind. The primary therefore clamps its release floor to
what the follower has confirmed durable, and blocks release entirely while a
registered chain has yet to report a watermark, or while its watermark is in
another LSN space (a promotion-adopted chain echoes the *source's* LSNs). A
chain that has stopped keeps its claim: an unreachable follower that is behind
is exactly the one a release would strand, so retention stalls — visibly, as
`ReleaseBlocked::ChainLag` — rather than proceeding.

The **follower's own mirror is not released at all**, and §4.1 is the reason.
Its dedupe cursor is reconstructed by walking the provenance index from batch
zero and stopping at the first gap, so releasing a prefix of that index
rebuilds an empty cursor and produces exactly the full re-stream this section
spends its length avoiding. Bounding a follower needs the rebuilt cursor
persisted as a keyed row and the reconstruction seeded from it instead of from
zero. Until that exists, a follower's journal grows with its uptime — the
residual D20 names.

A restart of the same owner is only "the previous stream" while the epoch is unchanged, and in the landed implementation it usually is not: shard activation bumps the ownership epoch on every activation, a clean restart included. §3.1 makes that a different `DurableChainId`, and §4.1 keys the follower's durable dedupe by it — so a follower reopened at the bumped epoch would rebuild an empty cursor and take a full re-stream into a second physical copy of every mirrored record, at a healthy zero-byte lag, leaving promotion permanently ambiguous.

Dropping the epoch from the dedupe key is **not** the fix: §3.1's identity rule is exactly what stops a superseded primary resuming onto a live follower session. Instead the follower detects the fork. When its journal already holds mirrored rows under a sibling identity differing only in epoch, it refuses to open the new one and names the missing restart handshake. That handshake is not designed yet, and a passive follower cannot invent it: it runs without a fence store (§2's follower accepts no writes and performs no activation), so it has no way to verify an epoch claim. Refusing loudly is the correct behaviour until the handshake exists; resolving a fork today is an operator action.

## 5. Failure and reconnect behavior

The design assumes failures happen while one side is still live.

### Primary-side failure

- If the primary process dies, the follower keeps its durable journal.
- The follower becomes the recovery source for the next primary instance.
- Recovery starts from the follower watermark, not from the last volatile in-memory batch.
- Client-facing bulk acks are still governed by the journal contract in [08-persistence.md](08-persistence.md); the chain follower only narrows the loss window after node loss.

### Follower-side failure

- If the follower dies, the primary logs the chain as degraded and keeps the local journal path alive.
- Client acks do not wait on the follower.
- The primary continues until the local durability window or ops policy says otherwise, but the RPO has widened back toward the journal-only window described in [08-persistence.md](08-persistence.md) §4.1.
- On follower restart, the primary reconnects and resumes from the follower's reconstructed watermark.

### Network partition

- Transient disconnects are treated as stream loss, not as logical data loss.
- The primary retries the same durable chain identity with a fresh session nonce until it is explicitly superseded.
- The follower rejects stale batches by durable chain identity, session nonce, and batch sequence.
- If the stream cannot be re-established within the alarm window, page ops and mark the shard set as running without chain protection.

### Reconnect rules

- Reconnect is always explicit, with a fresh `session_nonce` but the same durable chain identity.
- The primary must perform a watermark probe before sending new records.
- The follower must not accept writes for an old stream identity after a new identity is established.

That gives at-least-once transport with exactly-once durable outcomes.

## 6. Observability

Use the existing OpenTelemetry posture from [09-services-and-ops.md](09-services-and-ops.md) and expose chain-specific signals:

- Primary commit latency.
- Follower append latency.
- Primary-to-follower lag in bytes and age.
- Reconnect count per stream.
- Duplicate batch count.
- Watermark probe latency.
- Stream restarts per shard set.
- Journal/fsync errors on either side.

Recommended alerts:

- follower lag above the D11 target for more than a short grace window,
- duplicate batch rate rising above reconnect noise,
- watermark regression,
- or stream churn that suggests the topology is flapping.

Two of those need a caveat about what the counters actually measure.

**Reconnect count is a probe count, not a churn count.** A primary's watermark
probe is `GrpcChainTransport::follower_watermark`, which delegates straight to
`reconnect`, and every reconnect opens a session on the follower. So
`chain_reconnects_total` rises once per probe: a chain that is merely retrying a
failed push re-probes every 10 ms and drives it at roughly 100/s with no stream
having been lost at all. Alert on `chain_stream_restarts_total` for topology
flap, and read the session count next to the stall signals below.

**Duplicate batches are counted where they happen.** The follower increments
`chain_duplicate_batches_total` on the idempotent-replay branch — a batch whose
every row was already durable and whose provenance matched — and nowhere else.
A retry whose provenance disagrees is a failed precondition, not a duplicate,
which is what keeps the "above reconnect noise" alert meaningful.

Every batch and ACK should carry `stream_id`, `batch_seq`, `first_lsn`, `durable_through`, and `follower_watermark` in logs so replay investigations can correlate the two journals.

The series names for these signals are declared once, in `orrery_protocol::metrics` (`CHAIN_SERIES`), for the same reason the D16 latency names are: a producer and a consumer in different processes must spell them identically. Nothing emits them yet — `ChainReplicator::snapshot` is the reading a reporter would publish, and wiring it into `persistd`'s delta reporter is the next step.

One caveat about the lag alarm, because it used to be the only alarm here: it
advances only on a *successful* probe or push — `update_progress` is its sole
writer, and so is the follower watermark's. A chain that cannot make progress
therefore **freezes** both readings rather than growing them, and a follower
killed while the chain was caught up freezes them at zero. Gating an alert on
`lag_bytes > 0` cannot fire for that case at all: with no further appends the
replicator parks on the commit broadcast and makes no transport call to fail.

`ChainSnapshot` carries four more readings for exactly that reason, and they
are the ones that move when the first four do not:

| Reading | What it is |
|---|---|
| `running` | whether the replication task is alive. It exits on shutdown and on a fault, but it can also *panic* — at the `expect` guarding the adopted-history scan — and a panicked task publishes no fault at all. |
| `progress_age_ms` | time since the last successful probe or push: the age of `watermark` and `lag_bytes`, and the `chain_lag_age_ms` series. Measured entirely on the primary; nothing is added to the wire, and `ProgressReply` is positional postcard inside an opaque protobuf field, where a new field would be a hard cross-build incompatibility. |
| `failed_pushes` | pushes failed since the last successful one, reset by any progress. Zero on a healthy chain; climbing at the retry rate on a wedged one. |
| `behind` | whether the primary has committed past the follower watermark *right now*, read live from the journal rather than from the frozen gauge. Always false for a promotion-adopted chain, whose watermark is in the source's LSN space. |

`ChainSnapshot::stalled_for(grace)` is the composite an alert should read: the
chain has stopped, or it owes work (`behind`, or failing pushes) that it has
made no progress on for longer than `grace`.

Two conditions are faults rather than degradation, and stop the chain outright:
`FollowerAhead`, a follower holding history this primary never wrote, and
`PrimaryScanFailed`, the primary's own journal read failing under a live
replicator. A retried transport error is neither — it is degradation, and
`failed_pushes` is where it shows up.

## 7. Rollout and acceptance gates

Rollout should stay scriptable and reversible.

### Stage 0 - single-process simulation

- Primary and follower are separate logical components in one process.
- Use the in-process transport already present in the harness as the control case.
- Verify batch ordering, dedupe, and watermark advancement.

**Gate**

- A mirrored batch survives process-local replay and the follower watermark advances monotonically.

### Stage 1 - two processes, same host

- Run one primary process and one follower process on the same machine.
- Use tonic/gRPC for the transport.
- Kill and restart the follower while the primary keeps appending.

**Gate**

- The follower reconstructs its watermark from its own journal and resumes without duplicate durable records.

### Stage 2 - two processes, separate hosts

- Split primary and follower across distinct machines or VMs.
- Inject packet loss, latency, and reconnects.
- Verify that the primary resumes from the follower watermark after reconnect.

**Gate**

- A primary crash after follower durability does not lose acked records.
- A follower crash does not block client acks.
- Reconnects are idempotent and do not create duplicate journal history.

### Exit gate for P2

Tie this design to the P2 demo criterion in [11-roadmap.md](11-roadmap.md):

- `kill -9` the primary,
- restart from the follower's journal tail,
- observe that bulk state resumes from the mirrored journal without violating the ack contract,
- and confirm the chain lag alert stays within the documented envelope during steady-state load.

If that gate is green, the chain is good enough for the next persistence phase. If it is not, keep the design narrow and fix the recovery path before widening the topology.

**The gate is not green, and the chain is not what is blocking it.** As of
2026-08-17 `scripts/p2-kill9-gate.sh` drives 541,224 diffs into the primary and
receives **zero** acknowledgements, durable or provisional, and zero committed
intents. Both halves are refusals, and neither is in this design:

- Every `DiffUplink` the rig sends carries `lease_id: None`
  (`p2-load/src/main.rs:1401`, deliberately — the rig measures gateway latency,
  not authority arbitration), while `route_session_diff` sets
  `strict_authority: true` unconditionally
  (`crates/orrery_persistd/src/gateway.rs:3930`). `route_diff` substitutes the
  never-granted `LeaseId(0)` and `apply_fenced` rejects each one before the
  journal sees a byte.
- Every intent is rejected with `REASON_VALIDATION_FAILED`, because
  `ProductionIntentValidator` (`crates/orrery_persistd/src/bin/persistd.rs:71`)
  rejects unconditionally — persistd ships no ruleset.

So the gate's `acks.jsonl` is 1,024 recorded *rejections*, which satisfies its
`[[ -s $out/acks.jsonl ]]` non-empty check, and its `bulk_ack_ms` histogram is
fed by `UplinkScheduler::on_nack_at`
(`crates/orrery_persist_client/src/uplink.rs:279`), which records reply latency
for a NACK exactly as for an ACK. The D16 latency gate is therefore measuring
the round trip of a refusal. Closing the P2 criterion needs a leased write path
in the rig; until then the mirror is correctly empty and no change to this
document's mechanism will move the gate.
