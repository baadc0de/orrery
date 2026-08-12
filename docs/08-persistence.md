# 08 — Persistence: Cell Actors, Journal, FoundationDB

The persistence cluster (`orrery_persistd`) is the sole writer of durable truth in Orrery. It answers the owner mandate "really really fast, horizontally scalable" with a two-tier design: an in-memory, single-writer **cell actor** tier fronted by a per-node append-only **journal** (journal commit < 2 ms server-internal, client-observed acks p99 < 5 ms in-region — never blocking the simulation), backed by **FoundationDB** as the strictly-serializable system of record (checkpoints for bulk state; synchronous transactions for anything with economic value). This document specifies the full write paths, the actor model and its recovery/split protocols, the journal and its honest durability windows, the complete FDB keyspace schema, a worked item-trade transaction, terrain and event-history handling, hotspot management, scaling math, backup/DR, at-rest schema versioning, and world seeding/content patching.

Normative source: [DECISIONS.md](DECISIONS.md) §D11 (with D5 for `CellId`/sharding, D7 for leases, D10 for attestations, D12 for the service inventory, D16 for parameters).

## 1. Architecture

One `orrery_persistd` process per cluster node hosts six components. Games link their `Ruleset` (D9) into their own `persistd` binary — the harness is a library.

```mermaid
graph LR
  subgraph clients ["Peers / field hosts"]
    P["orrery_persist_client<br/>(diff uplink · intents · area load)"]
  end
  subgraph node ["persistd node (1 of N)"]
    GW["Gateway<br/>(iroh endpoint, sig/attestation verify)"]
    REG["Lease registrar"]
    IV["Intent validator<br/>(linked Ruleset)"]
    CA["Cell actors<br/>(single writer per shard cell)"]
    J[("Per-node journal<br/>segmented append-only log")]
    ADJ["Adjudication executor<br/>(deterministic replay)"]
  end
  F["Follower journal<br/>(peer node)"]
  subgraph durable ["Durable tier"]
    FDB[("FoundationDB 7.3.x<br/>system of record")]
    OBJ[("Object storage<br/>Parquet event archive")]
  end
  TAIL["Archive tailer"]
  P -- "datagrams: diffs" --> GW
  P -- "streams: intents, loads, leases" --> GW
  GW --> CA
  GW --> IV --> FDB
  GW --> REG --> FDB
  CA --> J
  J -. "async chain replication ≤100 ms" .-> F
  CA -- "checkpoints, 20 s jittered" --> FDB
  TAIL -- "tails sealed segments" --> J
  TAIL --> OBJ
  TAIL -- "archive metadata" --> FDB
  ADJ --> FDB
```

- **Gateway** — the iroh-facing front door. Terminates peer connections (peers dial well-known addresses; no hole punching server-side, D3), verifies intent signatures and witness attestations, routes diffs to cell actors (local or remote via internal tonic/gRPC), serves area loads, and multiplexes lease traffic.
- **Cell actors** — one single-writer actor per *shard cell* (8×8×8 interest cells, D5/D16) holding that region's hot state in memory.
- **Journal** — one segmented append-only log per node, shared by all actors on the node, adaptively group-committed (< 2 ms server-internal, §4).
- **Lease registrar** — arbiter for authority leases (D7): CAS on `lease/{entity_id}` rows, TTL 10 s, heartbeat 2.5 s, batched. Logically a distinct component, physically it executes **inside each cell actor's single-writer event loop** (lease rows shard with their cells); the gateway routes lease traffic to the owning actor.
- **Intent validator** — runs `Ruleset` admission checks against hot state, then executes the FDB transaction.
- **Adjudication executor** — re-executes discrepancy-report evidence bundles (D10) in the headless `orrery_core` replay harness; retains the last **3** ruleset builds as version-keyed sidecar workers and routes each bundle by its `RulesetId` (bundles older than retention are *unadjudicable* — no strike, D10); verdicts produce write refusals/annulments and `strike/` rows.
- **Archive tailer** — compacts sealed journal segments into Parquet objects (event history, R7).

## 2. The two write classes

Everything durable enters through exactly one of two paths. The split is `Ruleset`-classified (D9): anything touching persistent *value* (items, currency, progression, structure placement) is critical; everything else (positions, health, world-entity state, terrain deltas) is bulk.

| Property | Bulk state | Critical operations |
|---|---|---|
| Trigger | replicon change-detection diffs, ~1–4 Hz per entity, priority-scheduled | signed, witness-attested intents (K=3 of N≥5, D10) |
| Transport | iroh unreliable datagrams | iroh reliable stream |
| Durability point | journal group-commit fsync (adaptive, < 2 ms server-internal) | FDB serializable commit |
| Ack target | **journal commit < 2 ms (server-internal) · client-observed ack p99 < 5 ms in-region** | **< 10 ms p99 in-region** |
| RPO | ≤ ~100 ms (chain-replicated journal, default) | **0** |
| Reaches FDB | checkpoint, **20 s jittered** cadence | synchronously, in the ack path |

### 2.1 Bulk path

```mermaid
sequenceDiagram
    participant A as Authoritative peer
    participant G as Gateway
    participant C as Cell actor
    participant J as Journal
    participant F as FoundationDB
    A->>G: EntityDiff{cell, entity, tick, components} (datagram)
    G->>C: mailbox: ApplyDiff
    C->>C: apply to in-memory state, mark dirty
    C->>J: append(JournalRecord)
    J-->>C: durable (adaptive group fsync, §4)
    C-->>G: lsn
    G-->>A: BulkAck{entity, tick, lsn}
    Note over G,A: journal commit < 2 ms (internal) · client ack p99 < 5 ms in-region
    Note over C,F: dirty set → copy-on-update checkpoint every 20 s (jittered)
```

The ack is issued **after** the record's group fsync completes — the ack *is* the durability contract. The two targets are measured at different points: **journal commit < 2 ms** from actor append to fsync completion (server-internal); **client-observed ack p99 < 5 ms** from datagram send to `BulkAck` receipt (in-region, RTT included). `orrery_persist_client` keeps unacked diffs buffered and resends on reconnect (records are idempotent: keyed by `(entity, tick)`, last-writer-wins per component within an entity's single-writer stream). This design — unreliable datagrams + application-level acks + idempotent records — is **normative** for the bulk uplink; the reliable stream carries only intents, loads, and leases ([02-networking.md](02-networking.md)). Nothing in this path touches FDB, so bulk throughput is bounded by journal appends, not database ops.

**Epoch-fenced acks (split-brain guard).** An actor may issue durable acks only while its shard-ownership epoch (§3.4) is **confirmed fresh**: it heartbeats an FDB read version roughly every **1 s** and treats its epoch as stale after a **3 s staleness bound** — deliberately below the failure-detection + re-placement time, so a partitioned former owner falls silent before a replacement can be fenced in and serving. While stale, the actor downgrades to **provisional acks**, which the client treats as unacked (kept buffered, resent to the new owner). Every `JournalRecord` carries the epoch it was appended under, so recovery replay discards records from a superseded epoch; §4.1 quantifies the residual window.

**Bulk-path validation.** The cell actor runs the stateless `Ruleset` invariant validators (D9/D10 — the same speed/acceleration/rate/impossible-value checks witnesses run) on inbound diffs: **mandatory** for entities in cells with fewer than N witness candidates — closing the solo-player-in-an-empty-cell hole, where no witness set exists to observe the author — and **sampled** elsewhere. Violations are rejected (NACK) or flagged to the adjudication pipeline.

### 2.2 Critical path

```mermaid
sequenceDiagram
    participant A as Initiating peer
    participant W as Witness set (K=3 of N≥5)
    participant G as Gateway
    participant V as Intent validator
    participant F as FoundationDB
    A->>W: intent + local context
    W-->>A: co-signatures
    A->>G: SignedIntent{intent_id, ops, attestations} (stream)
    G->>G: verify author sig + K-of-N attestations<br/>against cluster-seeded cell-epoch witness set
    G->>V: Ruleset admission check vs. hot cell state
    V->>F: serializable optimistic txn (read–check–write)
    F-->>V: commit 1.5–2.5 ms, or conflict → retry
    V-->>G: outcome + receipt
    G-->>A: IntentAck (p99 < 10 ms)
```

Two-stage validation, deliberately: the hot-state `Ruleset` check is a **fast admission filter** (reject obviously invalid intents without an FDB round trip, using live positions/inventory the actor already holds); the **FDB transaction is the sole authority** for ledger state — it re-reads and re-checks every durable invariant inside the transaction. Hot state mirrors ledger rows; it never owns them. This is the Diablo II lesson (D10) enforced structurally: no client, and no in-memory tier, can mint value.

## 3. Cell actor model

### 3.1 Single writer, mailbox, state

A cell actor is a tokio task owning all hot state for one shard cell — the persistence-side twin of the single-writer invariant (D2). All mutation flows through its mailbox; readers get snapshots via message, never shared mutable access.

```rust
// orrery_persistd (sketch)
pub enum CellMsg {
    ApplyDiff(EntityDiff, AckHandle),
    ReadSnapshot { cells: SmallVec<[CellId; 27]>, reply: mpsc::Sender<SnapshotPage> },
    Precheck(LedgerPrecheck, oneshot::Sender<PrecheckVerdict>),   // intent admission
    Rekey { entity: PersistId, from: CellId, to: CellId },        // cross-cell movement commit (D7)
    Checkpoint(CheckpointCause),          // Timer{jitter} | Quiesce | PreSplit
    Split { epoch: Epoch, children: [ShardCellId; 8] },
    Shutdown,
}

pub struct CellActorState {
    shard: ShardCellId,
    epoch: Epoch,                                   // fencing token (§3.4)
    entities: HashMap<PersistId, EntityRecord>,     // per-component encoded bytes + dirty bits
    by_cell: HashMap<CellId, kiddo::KdTree<f32, 3>>,// intra-cell spatial queries (D5)
    terrain: HashMap<CellId, ChunkState>,           // base + pending deltas (§8)
    dirty: DirtySet,                                // entities touched since last checkpoint
    ckpt_watermark: Lsn,                            // journal LSN covered by last checkpoint
}
```

`EntityRecord` stores components as individually `postcard`-encoded byte slices (`orrery_protocol` types): diffs apply by slot replacement without decoding, checkpoints serialize by concatenation, and the actor never needs the game's component types — only the `Ruleset` does.

### 3.2 Placement: rendezvous hashing over shard cells

Shard cells map to nodes by **rendezvous (HRW) hashing**: `owner(shard) = argmax_n weight_n · h(shard_id, node_id)`. Properties we need and get: no central assignment table for the common case, minimal disruption when nodes join/leave (only shards whose argmax changed move), and capacity weighting. The authoritative placement record (for fencing and splits, §3.4/§3.5) lives in FDB at `actor/{shard_cell_id}`; gateways cache the HRW result and repair on epoch-mismatch NACKs.

### 3.3 Why an in-memory tier at all

Because the alternative was surveyed and loses: Redis-class stores acknowledge writes that async replication can lose on failover ([Redis cluster docs](https://redis.io/docs/latest/operate/oss_and_stack/management/scaling/)), and in-process state in the persistence node is strictly cheaper than a network hop to a cache that has the same durability posture. The literature agrees on the shape: single authoritative actor per region + append-only event journal + write-behind checkpointing is the recommended MMO persistence structure ([Cornell VLDB 2009](https://www.cs.cornell.edu/~tuancao/2009-VLDB-Checkpoint.pdf), [Netherite](https://arxiv.org/pdf/2103.00033)). CRDTs are deliberately absent from the hot path — single writer per cell makes them unnecessary (noted future option for offline build modes only, D11).

### 3.4 Restart and recovery

On assuming shard `S` (cold start, node replacement, or relocation):

1. **Fence:** CAS `actor/{S}` from `(old_node, e)` to `(self, e+1)` in one FDB transaction. The new epoch `e+1` is the fencing token; every subsequent checkpoint transaction *reads* `actor/{S}` and aborts if the epoch moved — a zombie actor (network-partitioned former owner) can never commit a stale checkpoint, because its commit would conflict with the CAS.
2. **Load checkpoint:** range-scan `world/{cell_id}/…` for all interest cells in `S`, plus `chunk/{cell_id}/…`; read `ckpt/{S}` for the journal watermark `(node_id, lsn)` of the last checkpoint.
3. **Replay tail:** replay journal records for `S`'s cells with `lsn > watermark` — from the local journal if restarting in place, from the **chain-replication follower** if the node died, from the archive if both are gone.
4. **Open mailbox**, bump gateway routing.

Recovery time is bounded by checkpoint size + ≤20 s of journal tail — seconds, not minutes, per the [Cornell copy-on-update analysis](https://www.cs.cornell.edu/~tuancao/2009-VLDB-Checkpoint.pdf).

### 3.5 Hotspot split / relocate

Range-sharding on a space-filling curve concentrates a crowd's writes on one shard — the [FDB #11510 hotspot pattern](https://github.com/apple/foundationdb/issues/11510) — so the actor tier splits *ahead* of the storage tier feeling it. Telemetry per actor: player count in shard (from coordinator presence), mailbox depth, append rate.

```mermaid
sequenceDiagram
    participant T as Telemetry/coordinator
    participant P as Parent actor (epoch e)
    participant F as FoundationDB
    participant K as Child actors (epoch e+1)
    participant G as Gateways
    T->>P: split threshold sustained (with hysteresis)
    P->>F: txn: write actor/{child_i} = (HRW owner, e+1) ×8, mark actor/{S} = Splitting
    P->>P: quiesce-flush: immediate checkpoint (PreSplit), stop accepting diffs (NACK epoch)
    K->>F: load child cell ranges from checkpoint
    K->>P: replay journal tail (cell-indexed, prefix-filtered per child)
    K-->>G: ready(epoch e+1)
    G->>G: reroute on epoch bump; retried diffs land on children
    P->>F: retire actor/{S}
```

The same machinery with one child at the same level is a **relocate** (move a hot shard to an underloaded node, overriding HRW via the `actor/` row). Diffs NACKed during the handover window (target: < 1 s) are client-buffered and retried — invisible to gameplay because bulk acks are not in the frame loop. Merges run the protocol in reverse when children fall below the low-water mark for a sustained period.

## 4. Journal design

Per-node (not per-actor: one fsync stream per disk is the point), segmented, append-only:

- **Segments** of 128 MiB, named by monotonic sequence; a segment is *sealed* when full or on rotation, then immutable — the archive tailer's unit of work.
- **Group commit — adaptive:** appends from all actors on the node accumulate in a ring; the committer issues `fdatasync` **immediately when the disk is idle** (a lone record pays only device latency) and falls back to **~0.5 ms batching** under load (or when the batch hits a size cap), keeping append→durable **< 2 ms server-internal** (D16). Every waiter in the batch resolves on one fsync. On NVMe this sustains hundreds of thousands of records/s at up to ~2 000 fsyncs/s.
- **Record** (`orrery_protocol` sketch):

```rust
pub struct JournalRecord {
    pub lsn: Lsn,             // (segment_seq, offset) — node-local, monotonic
    pub cell: CellId,         // Morton CellId (NonZeroU64, D5) — the index key
    pub entity: PersistId,    // stable persistent id, u64 (never a Bevy Entity)
    pub tick: Tick,           // u64 universe tick (D8)
    pub epoch: Epoch,         // shard-ownership epoch at append (§2.1/§3.4 fence)
    pub author: NodeId,       // authoritative peer that produced the op
    pub kind: RecordKind,     // ComponentDiff | TerrainDelta | Spawn | Despawn | Rekey | CheckpointMark
    pub payload: Bytes,       // postcard
    pub crc: u32,
}
```

`Spawn` records carry the new entity's **`PersistId`**, minted one of two ways (D11): peer-side from a **journaled block grant** — a contiguous range of ids (default **4096**), leased to the peer per session through the gateway, allocated by atomic add on `pid/next` (§6) and recorded in the journal, so grants survive restarts and remain usable **offline** — or cluster-side inside an intent transaction, returned in the commit receipt (§7) for intent-created entities. The replicated `PersistId` component (owner-written, maintained by `orrery_persist_client`) carries the id into every peer's world and is the canonical Bevy `Entity` ↔ `PersistId` mapping ([03-replication.md](03-replication.md)).

- **Cell index:** a per-segment sparse index `(cell_id → offset list)` written as a segment footer, so recovery and split replay read only the relevant cells' records, not the whole segment. Implementation: raw segmented files with the footer index, or [fjall 3.x](https://fjall-rs.github.io/post/fjall-3/) (active, pure Rust, ~42 K LOC vs. RocksDB's ~700 K) if we want its compaction machinery; **not RocksDB unless profiling demands it** (D11).
- **Chain replication — default ON:** each node streams its journal to exactly one async follower (next node in HRW order over node ids; ops-overridable, placed in a different AZ). The follower persists segments verbatim. Replication is async — it is *not* in the ack path — with lag monitored and alarmed above 100 ms.

### 4.1 Honest durability windows

What an **acked** write survives, by failure and mode:

| Failure | Bulk, journal-only (chain OFF) | Bulk, chain replication ON (**default**) | Critical (FDB) |
|---|---|---|---|
| `persistd` process crash (disk intact) | 0 loss (acked ⇒ fsynced) | 0 loss | 0 loss |
| Node/disk loss | up to **20 s + jitter** (last checkpoint) | ≤ **~100 ms** (follower lag) | 0 loss |
| Node **and** its follower lost | — | up to 20 s + jitter | 0 loss |
| AZ loss (follower cross-AZ) | up to 20 s + jitter | ≤ ~100 ms | 0 loss (FDB spans AZs in-region) |
| Zombie actor (partitioned former owner) | acks issued inside the ≤ **3 s** staleness bound (§2.1) may cover records replay discards as superseded-epoch | same ≤ 3 s residual | 0 loss (txn epoch-checks `actor/{S}`) |
| Region loss | last archive + FDB backup | last archive + FDB backup | last FDB backup point |

Unacked in-flight diffs are always the client's to resend; the table is about acked data. The zombie row is the **residual split-brain window** the epoch fence cannot close: a partitioned former owner can keep acking for up to the 3 s read-version staleness bound before downgrading to provisional acks (§2.1). Because the bound sits below failure-detection + re-placement time, a successor is normally not yet serving during that window, so the residual is a bounded theoretical exposure, not an expected loss path; records journaled under the superseded epoch are discarded at replay and the client's resend to the new owner closes the gap. We state the journal-only column because chain replication is a deployment toggle: single-node dev setups run with it off and accept the 20 s window.

## 5. FoundationDB as the system of record

Why FDB (pinned **7.3.x**, 7.4 tracked as upgrade candidate; `foundationdb-rs` 0.11 — D14):

- **Strictly serializable, interactive optimistic transactions across arbitrary keys** — the anti-duplication mechanism for trades needs no application locks and has no LWT contention cliffs; conflicting transactions simply retry ([FDB performance](https://apple.github.io/foundationdb/performance.html), [SIGMOD paper](https://www.foundationdb.org/files/fdb-paper.pdf)).
- **Latency:** reads 0.1–1 ms, commits **1.5–2.5 ms** at <75% load — which is why the intent path can promise < 10 ms p99 end-to-end.
- **Scale:** linear scaling demonstrated to **8.2 M ops/s** on 384 processes; per-core ~55 K reads/s, ~20 K writes/s (SSD engine).
- **Correctness pedigree:** the deterministic-simulation-tested core is [the canonical argument](https://jbaker.io/2022/05/09/project-loom-for-distributed-systems/) against rolling our own; the Rust binding is validated hourly against thousands of BindingTester seeds and has ~15 M downloads ([foundationdb-rs](https://github.com/foundationdb-rs/foundationdb-rs)).
- **Ops:** 3–5 node clusters via [fdb-kubernetes-operator](https://github.com/foundationdb/fdb-kubernetes-operator) or systemd + `fdbcli`; Apache 2.0, no licensing caps.

The [known limits](https://apple.github.io/foundationdb/known-limitations.html) and how the design respects each:

| FDB limit | Design consequence |
|---|---|
| 5 s transaction duration | all transactions are short read–check–write sets (intents) or bounded checkpoint batches; long scans (area load, archive) use continuation ranges across transactions |
| ~10 KB key | keys are tuple-encoded ids, tens of bytes |
| ~100 KB value | entity rows sharded if oversized; terrain chunks stored as ≤100 KB shard rows `chunk/{cell_id}/{n}` |
| 10 MB transaction | checkpoints split into multiple transactions by key range; the per-shard watermark row commits **last**, so a partially applied checkpoint is simply re-run (rows are idempotent overwrites) |
| multi-DC commit tail (~22 ms mean / 281 ms p99.9 with satellites) | one FDB cluster **per region**; no cross-region transactions (D11 latency targets are in-region) |

Hot-key conflict retries are application responsibility under optimistic concurrency — addressed by the actor tier absorbing bulk writes (FDB sees 20 s aggregates, not 4 Hz streams) and by §11 pre-splitting.

## 6. Keyspace schema

**This table is the single source for the cluster keyspace** — [09-services-and-ops.md](09-services-and-ops.md) and [10-crates.md](10-crates.md) reference it rather than restating rows. All subspaces are allocated once via the Directory layer; keys below show the logical tuple encoding. `{cell_id}` is the 64-bit Morton `CellId` (`NonZeroU64` — the sentinel bit guarantees non-zero, D5) written big-endian, so FDB's sorted keyspace inherits Morton order: **a shard cell's subtree is one contiguous range** (parent = prefix), and "everything near me" is a handful of range scans. Morton has locality discontinuities at power-of-2 boundaries, so neighborhood reads always enumerate the 27 explicit cell ranges rather than one raw span; an optional **Hilbert remap** (via [lindel](https://lib.rs/crates/lindel)) can be applied at the storage layer only, behind the same trait, if scan locality measurably matters.

| Subspace / key | Value | Writer | Notes |
|---|---|---|---|
| `world/{cell_id}/{entity_id}` | component bag (postcard), per-component slots | cell actor (checkpoint) | primary bulk state; row split `.../{k}` if > 100 KB; `cell_id` is grid-relative — interpreted within its grid's `CellId` space (see `grid/` row) |
| `grid/{grid_id}` | `(parent GridId, origin transform, velocity, status)` | cell actor (checkpoint) | nested-grid frame registry ([01-spatial-model.md](01-spatial-model.md) §13): a carrier's motion re-keys *this one row*, never its contents; `world/` keys are read per-grid |
| `world/{cell_id}/{entity_id}` *(tombstone)* | despawn marker w/ GC deadline | cell actor | cleared by checkpoint GC pass |
| `player/{account_id}` | profile, progression, settings | intent path | critical-class |
| `player/{account_id}/loc` | `(cell_id, entity_id)` | cell actor on rekey | login placement pointer |
| `ledger/bal/{account_id}/{asset_id}` | integer balance | **FDB txn only** | currency; integer math (D9) |
| `ledger/item/{item_uid}` | `(owner_ref, item_state)` | **FDB txn only** | unique items; single ownership row = anti-dupe invariant |
| `ledger/receipt/{versionstamp}` | `(intent_id, parties, ops)` | FDB txn (versionstamped key) | trade audit trail, strictly ordered |
| `intent/{intent_id}` | outcome digest | FDB txn | idempotency: duplicate submission returns recorded outcome |
| `lease/{entity_id}` | `(holder NodeId, seq: SeqPair(auth_seq, own_seq), lease_id, expires_at, flags, group)` | lease registrar (CAS) | D7; TTL 10 s, heartbeat 2.5 s; `lease_id` = monotonic fencing token (gateway drops stale-`lease_id` uplinks); `flags`: `PLAYER_BOUND`/`STRONG_HELD`/`PROVISIONAL`/`PARKED`; `group` = attached children; full field semantics in [04-authority.md](04-authority.md) (canonical) |
| `chunk/{cell_id}/{n}` | terrain shard ≤ 100 KB | cell actor (compaction) | §8 |
| `chunk/{cell_id}/meta` | `(shard_count, base_version, encoding)` | cell actor | |
| `actor/{shard_cell_id}` | `(owner node, epoch, status)` | split/fence protocol | placement + fencing (§3.4) |
| `ckpt/{shard_cell_id}` | `(node_id, journal lsn, epoch, time)` | cell actor | recovery watermark |
| `jarchive/{node_id}/{segment_seq}` | `(object key, cell ranges, lsn span, checksum)` | archive tailer | journal-archive metadata |
| `id/{account_id}` | account record, bound NodeIds, tokens | `orrery_identity` | canonical identity subspace; Sybil cost anchor (D10) |
| `strike/{account_id}/{versionstamp}` | `(weight, decay t½=14 d, evidence ref)` | adjudication executor | read by identity for quarantine/ban thresholds |
| `epoch/{cell_id}` | witness-epoch record: seed-key commitment (blake3), epoch bounds, revealed key at epoch end | coordinator (via gateway) | D10 witness-set seeding; commitment published in the epoch announcement, key revealed for retroactive verifiability |
| `coord/leader` | coordinator leader lease (TTL) | coordinator (CAS) | active + warm-standby failover ([09-services-and-ops.md](09-services-and-ops.md)) |
| `pid/next` | next unallocated `PersistId` (atomic add) | gateway (block grants) · intent path | block grants: contiguous ranges (default **4096**) leased per session, journaled, usable offline (§4) |
| `content/version` | `(content build id, manifest digest)` | offline import tool | designed-content diff/patch on later deploys (§17) |

## 7. Worked example: item trade

Player A sells `item_uid = X` to player B for 500 gold. The intent (built by `orrery_persist_client`, co-signed by K=3 witnesses from the cluster-seeded cell-epoch set) arrives at the gateway; signatures and attestations are verified **before** the transaction — the txn trusts the gateway's verdict and re-checks only *durable* facts. Sketch against `foundationdb-rs`:

```rust
// orrery_persistd intent executor (sketch — retry loop is db.run's)
db.run(|trx, _| async move {
    // 0. Idempotency: replayed intent returns the recorded outcome.
    if let Some(prev) = trx.get(&key_intent(intent_id), false).await? {
        return Ok(Outcome::decode(&prev)?);            // duplicate delivery
    }
    // 1. Read set — these reads register conflict ranges.
    let item = trx.get(&key_item(x_uid), false).await?
        .ok_or(Reject::NoSuchItem)?;
    let bal_b = trx.get(&key_bal(b, GOLD), false).await?;
    // 2. Checks — durable invariants only (sigs/attestations verified upstream).
    ensure!(Item::decode(&item)?.owner == a,   Reject::NotOwner);
    ensure!(u64_le(&bal_b) >= 500,             Reject::Insufficient);
    ruleset.validate_trade(&trx_view, &intent).await?;  // game-level durable rules
    // 3. Writes.
    trx.set(&key_item(x_uid), &Item { owner: b, ..item }.encode());
    trx.atomic_op(&key_bal(b, GOLD), &(-500i64).to_le_bytes(), MutationType::Add);
    trx.atomic_op(&key_bal(a, GOLD), &500i64.to_le_bytes(),  MutationType::Add);
    trx.set_versionstamped_key(&key_receipt_vs(), &receipt.encode());
    trx.set(&key_intent(intent_id), &Outcome::Committed.encode());
    Ok(Outcome::Committed)
}).await
```

Semantics that make this the anti-dupe mechanism:

- **Serializable read–check–write.** If any concurrent transaction commits a write intersecting this transaction's read set between our read version and commit (B spends the same gold elsewhere; A trades X to C), the resolver rejects the commit with `not_committed`; `db.run` re-runs the closure — which then re-reads the new state and *fails the check honestly* (`Insufficient` / `NotOwner`). Double-spend requires two commits over the same `ledger/item/{X}` read — impossible by construction.
- **Atomic adds** on balances avoid read conflicts on the *credit* side (A's balance is blind-incremented) while the *debit* side keeps its read so the balance check is enforced.
- **Idempotency row** converts at-least-once intent delivery (client retries on timeout) into exactly-once outcomes.
- **Id minting in the receipt:** intents that create entities (crafting outputs, loot grants) allocate `PersistId`s inside the transaction (atomic add on `pid/next`, §6) and return them in the commit receipt — the client's predicted entity binds to its durable id on ack (§4 covers the peer-side block-grant path for bulk-class spawns).
- **Bounded retries:** after 5 conflict retries or `Reject`, the gateway returns a definitive refusal; the client's predicted intent outcome (D8) rolls back. Cross-cell trades need nothing special — the transaction spans arbitrary keys regardless of which nodes host the parties' cell actors.

## 8. Checkpointing

Cell actors checkpoint **copy-on-update**: applying a diff to a dirty-flagged entity first detaches the record from the in-progress checkpoint's view (persistent-data-structure style), so checkpoint serialization never pauses the mailbox — the scheme the [Cornell VLDB study](https://www.cs.cornell.edu/~tuancao/2009-VLDB-Checkpoint.pdf) found minimizes in-game latency impact and recovery time for MMO workloads. Cadence: **20 s, jittered per shard** (spreads FDB write load; prevents cluster-wide checkpoint synchronization). The checkpoint writes only the dirty set, in ≤10 MB transaction batches, then commits `ckpt/{shard}` (watermark LSN + epoch fence read) last. **Quiesce-flush:** a cell whose last player leaves (coordinator signal) checkpoints immediately and the actor may be parked (D7) — hot memory is bounded by *populated* cells, not universe size.

## 9. Area load

Client enters an area → `orrery_persist_client` requests the 27-cell neighborhood (D5) over a reliable stream. The gateway partitions the cells: **live cells** (an actor holds them) are served from actor memory — authoritative, ≥ checkpoint freshness; **cold cells** are served by FDB range scans over `world/{cell_id}/…` + `chunk/{cell_id}/…` (contiguous by Morton prefix). Pages stream **nearest-first** (center cell, then face/edge/corner neighbors by distance), so the client can spawn-in against page one; target **< 50 ms to first page-in** (one actor snapshot or one in-region range scan — FDB reads are 0.1–1 ms — plus serialization and one RTT). Subsequent motion turns loads into incremental single-cell fetches at the AOI leading edge, and live diffs flow via replication (03-replication.md), not the load path. For a nested-grid area (a ship's interior, [01-spatial-model.md](01-spatial-model.md) §13) the load is one `grid/{grid_id}` frame read plus the normal 27-cell scans *in the ship's grid* — the frame row tells the client where the ship is; the contents come from the ship's own `CellId` space.

## 10. Terrain and bulk edits

Terrain is chunk-oriented and cell-aligned (one chunk = one interest cell subdivided into sections). Edits are **bulk-class**: a `TerrainDelta{cell, section, op}` journal record on the standard bulk ack path (§2.1). Every delta is **attributed to and fenced by the editing player's own `PLAYER_BOUND` lease** — the record's author must hold that lease, so edits are attributable per account and a peer cannot edit as someone else. The cell actor invariant-checks each delta before applying: **reach** (the edit lies within interaction range of the editor's committed position), **rate** (per-account edit-rate caps), **tool** (the `Ruleset` confirms the editor holds the claimed capability); violations are rejected or flagged (§2.1). **Destructive or high-value edits** (`Ruleset`-classified — structure demolition, protected-region changes) are not bulk at all: they route through the witness-attested intent path (§2.2). Live edits replicate peer-to-peer on the reliable per-cell stream ordered by `(cell, tick)`, with late joiners fetching compacted chunks from the gateway — the replication side is specified in [03-replication.md](03-replication.md) (terrain delta replication). The actor holds `base + delta list` per chunk; compaction (on checkpoint cadence, or when deltas exceed 25% of base size) folds deltas into a new base and rewrites `chunk/{cell_id}/{n}` snapshot rows, each ≤ 100 KB to respect the value limit. **Sparse elision** is mandatory: empty/homogeneous sections are not stored — the [Minecraft chunk format](https://minecraft.wiki/w/Chunk_format) precedent (empty sections elided; [region files](https://minecraft.wiki/w/Region_file_format) bundling nearby chunks is exactly our Morton-prefix locality, done with files). Untouched procedural terrain costs zero rows: absence of `chunk/` keys means "regenerate from seed".

## 11. Event history and the archive

The journal **is** the event source (R7). The archive tailer consumes sealed segments, re-sorts records into `(cell_id, tick)` order, and writes Parquet objects to object storage, recording each under `jarchive/{node_id}/{segment_seq}`. Local segments are deleted only after (a) the checkpoint watermark has passed them and (b) the archive object is verified — the journal disk holds minutes-to-hours, the archive holds the configured retention (default: 30 days full-fidelity, aggregated statistics thereafter). Consumers:

- **Griefing rollback:** administrative inverse-op replay — select archive records by `(cell range, author/account, time range)`, generate inverse operations (terrain delta inverses, entity state restores from the preceding checkpoint), and apply them as administrative intents through the critical path (audited, attributable).
- **Offline progress / parked-cell catch-up:** on reload of a parked cell, the field host runs `Ruleset` catch-up (D7); the archive supplies the input history where catch-up depends on past events.
- **Desync forensics and adjudication context** (07-witnessing.md), and analytics export (Parquet is directly queryable by the telemetry stack, D12).

## 12. Hotspot management

Two tiers of defense against the crowd-event failure mode ([FDB issue #11510](https://github.com/apple/foundationdb/issues/11510): continuous-keyspace write hotspots cause storage-server queue growth):

1. **Actor tier absorbs rate.** FDB never sees per-tick or per-diff traffic — only 20 s checkpoint aggregates and intents. A 500-player crowd in one shard is a journal problem (which group commit handles) before it is an FDB problem.
2. **Pre-split on telemetry.** The coordinator's presence counts drive the §3.5 split protocol *ahead* of saturation (players trending toward a shard → split early, cheaply, while the parent is still healthy). Checkpoint jitter plus split children landing on distinct HRW owners spreads the resulting FDB write ranges. Under extreme load, load-shedding order is explicit: bulk ack latency degrades first (clients buffer), checkpoint cadence stretches next (durability window widens, alarmed), intents shed **last** and only by admission-queue backpressure — economic operations keep RPO 0 or fail loudly, never silently.

## 13. Scaling math

**Shared capacity assumptions** — the sizing basis for the whole backend; [09-services-and-ops.md](09-services-and-ops.md) §11 cites this table rather than restating it (invented for sizing, not ADR-normative):

| Assumption | Value | At 10 k CCU |
|---|---|---|
| Authored core entities per player | 1–2 typical | witness-log/adjudication sizing ([06-verifiable-core.md](06-verifiable-core.md), [07-witnessing.md](07-witnessing.md)) |
| Hot world entities per player | 4 | 40 k hot entities |
| Diff records/s per player | 10 (own avatar at 4 Hz + world entities at 1–4 Hz, priority-scheduled) | **100 k records/s** cluster-wide |
| Journal record size | ~260 B | **~26 MB/s** journal write bandwidth |
| Intent rate per player | 0.05 intents/s (≈ 6 reads + 5 writes each) | **500 intents/s** |
| Checkpoint dirty set | ~40 k entities per 20 s window | 2 k checkpoint writes/s to FDB |
| Cluster conclusion | — | **3–4 `persistd` nodes + 3–5 FDB nodes** |

FDB per-core throughput 55 K reads / 20 K writes (SSD engine), triple replication.

| | 1 k players | 10 k | 100 k |
|---|---|---|---|
| Diff appends/s (cluster) | 10 k | 100 k | 1 M |
| Journal write bandwidth | ~2.6 MB/s | ~26 MB/s | ~260 MB/s |
| Checkpoint writes/s to FDB (dirty ÷ 20 s) | 200 | 2 k | 20 k |
| Intent txns/s → FDB ops/s | 50 → ~550 | 500 → ~5.5 k | 5 k → ~55 k |
| Total FDB ops/s (×3 replication on writes) | ~2 k | ~17 k | ~170 k |
| `persistd` nodes (~150 K appends/s each, + follower traffic, + HRW headroom) | 2 | **3–4** | 12–16 |
| FDB cluster | 3 nodes | **3–5** | 6–9 |

Every column is linear in players — no quadratic terms — and the 100 k column sits two orders of magnitude under FDB's demonstrated 8.2 M ops/s ceiling. The binding resource at scale is `persistd` memory for hot state (populated cells only, §8 quiesce), then journal bandwidth; both shard by HRW.

## 14. Backup and disaster recovery

- **FDB:** continuous backup (`fdbbackup` agents) to object storage with periodic restore drills; this is the region-loss recovery point for all critical-class data. One FDB cluster per region (§5); cross-region is DR, not replication.
- **Journal archive** (§11) doubles as bulk-state DR: region-loss recovery = restore FDB backup, then replay archived journal ranges newer than the backup's checkpoint watermarks.
- **Restore order:** FDB restore → `actor/` registry reset (all epochs bumped) → actors cold-start from checkpoints → archive tail replay → gateways open. Leases all expire naturally (TTL 10 s) — authority re-establishes via normal CAS claims.

## 15. Failure modes and edge cases

| Case | Behavior |
|---|---|
| Zombie actor after partition | epoch fence (§3.4): its checkpoint txn conflicts on `actor/{shard}` and aborts; its journal appends are ignored at replay (superseded epoch); its durable acks stop within the 3 s read-version staleness bound (§2.1) — past it, only provisional acks the client treats as unacked (residual window quantified in §4.1) |
| Gateway routes to stale actor during split | actor NACKs with current epoch + child map; client-side retry re-routes; bulk path absorbs the <1 s window |
| FDB unavailable (netsplit posture, D12) | bulk path keeps journaling and acking (durability window widens to journal+follower); checkpoints and intents queue; P2P sim continues — degraded, not dead |
| Journal disk full / fsync stall | bulk acks shed first (clients buffer unacked diffs); intents unaffected (FDB path); alarm before shed via watermark telemetry |
| Checkpoint > 10 MB | multi-transaction batches; watermark row commits last → partial checkpoint is invisible and re-run idempotently |
| Entity > 100 KB | row sharding `world/{cell}/{entity}/{k}`; read as one range |
| Duplicate/replayed intent | `intent/{intent_id}` idempotency row returns the recorded outcome |
| Contended trade hot key (one famous vendor NPC) | conflict-retry with backoff; ledger rows are per-account so contention is per-party, not global; atomic-add credits remove the widest conflict range |
| Cross-cell trade, parties on different nodes | FDB txn spans keys regardless of actor placement; admission prechecks query both actors, but only durable checks decide |
| Crashed peer mid-uplink | lease expiry (10 s) orphans entities (D7); last acked diff is the durable state; unacked tail is lost by design (bulk class) |

## 16. At-rest schema versioning and migration

Games change their components; a persistent universe keeps rows written by every version that ever shipped. The scheme (D11):

- **Per-component schema version in the bag.** Every component slot in a `world/…` component bag (and every player/ledger row) carries its schema version alongside the postcard payload — versioning is per *component*, not per snapshot, so unrelated components migrate independently and the cell actor still never needs to decode game types (§3.1).
- **`Ruleset`-registered migration functions.** The `Ruleset` registers per-component migration functions; each must span **≥ 2 adjacent versions**, so the cluster composes v→v+1 steps into a chain that walks any historical row forward to current.
- **Lazy application.** Migrations run on **checkpoint-load and area-read** — a row upgrades when next touched and is written back at current version by the next checkpoint. An optional **background sweep** walks cold ranges at low priority, bounding how far behind any row can fall (and letting old migration code retire on a schedule).
- **History decodes too.** Journal and archive records carry their **encoding version**, so recovery replay (§3.4), parked-cell catch-up, and griefing rollback (§11) can decode records written under any retained version. Adjudication has the parallel mechanism: the executor keeps the last 3 ruleset builds as version-keyed workers routed by `RulesetId` (§1).

## 17. World seeding and content patching

A designed world has to get *into* the keyspace before the first player connects:

- **Offline import tool, built on the `persistd` harness.** The importer links the same library as the server binary (D12) and bulk-writes designed content — entities into `world/{cell_id}/{entity_id}`, terrain into `chunk/{cell_id}/{n}` — via direct FDB batch loads (no gateway, no journal: there is nothing to replay yet), minting `PersistId`s from the same `pid/next` allocator (§6) so designed and dynamic entities share one id space.
- **Content-version row.** Each import records a content build id + manifest digest under `content/version` (§6). Later deploys **diff** the new manifest against the recorded one and **patch** only changed rows; seeded rows that players have since modified are not clobbered — they are flagged for a `Ruleset`-defined merge policy instead of overwritten.
- **`universe_seed` custody.** The universe's procedural seed is generated **once per universe** and held in the operator's secret store — it is security-relevant per D9 (the verifiable core's per-entity, per-tick deterministic RNG derives from it). `persistd` nodes and the import tool read it from the secret store at startup; it never appears in the keyspace or in client-visible state.

## 18. Rejected alternatives

| Alternative | One-line reason | Source |
|---|---|---|
| **ScyllaDB** (runner-up) | best raw write throughput (p99 < 7 ms at 7 M TPS) and a first-class Rust driver, but LWTs are Paxos-based, 3–4× slower, single-partition, with pathological contention behavior — the wrong trade-safety tool; revisit only if sustained writes exceed a modest FDB cluster (~>500 K entity-writes/s) | [Scylla LWT](https://www.scylladb.com/2020/07/15/getting-the-most-out-of-lightweight-transactions-in-scylla/), [petabyte benchmark](https://thenewstack.io/what-we-learned-benchmarking-petabyte-scale-workloads-with-scylladb/) |
| **openraft general store** | pre-1.0, incomplete chaos testing; FDB's lesson is that the deterministic simulator is the hard part — our custom layer stays thin and single-purpose instead | [openraft](https://github.com/databendlabs/openraft), [simulation-first](https://jbaker.io/2022/05/09/project-loom-for-distributed-systems/) |
| **Redis / Valkey / Dragonfly** as record store | async replication loses acknowledged writes on failover; AOF `everysec` loses up to 1 s | [Redis persistence](https://redis.io/docs/latest/operate/oss_and_stack/management/persistence/) |
| **Aerospike** | CE caps at 8 nodes / ~5 TiB — kills "very large"; strong consistency is Enterprise-only | [CE limits](https://discuss.aerospike.com/t/clarification-about-the-limit-of-cluster-size-for-community-edition/9925) |
| **TiKV** | official Rust client ships "not suitable for production" in 2026 | [tikv-client](https://github.com/tikv/client-rust) |
| **sled** for journal/staging | stalled (stable from 2021); fjall 3.x or raw segmented logs instead | [sled](https://crates.io/crates/sled), [fjall](https://fjall-rs.github.io/post/fjall-3/) |
| **SpacetimeDB** | validates database-as-game-server (BitCraft) but replaces the Bevy + P2P architecture rather than persisting under it | [SpacetimeDB](https://spacetimedb.com/) |

Cross-references: cell math and sharding in [01-spatial-model.md](01-spatial-model.md); the diff uplink's replicon source in [03-replication.md](03-replication.md); leases in [04-authority.md](04-authority.md); attestations and adjudication in [07-witnessing.md](07-witnessing.md); deployment and telemetry in [09-services-and-ops.md](09-services-and-ops.md); client-side API (`orrery_persist_client`) in [10-crates.md](10-crates.md).
