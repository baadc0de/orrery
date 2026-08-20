# ADR-0024: Island drain is peer-driven, and never an evacuation

**Status:** Accepted · **Date:** 2026-08-20 · **Decision:** D24

This decision is normative once accepted. See the [ADR index](../DECISIONS.md)
for precedence, scope, and the complete decision set.

**Supersedes:** nothing. It **narrows one sentence** of
[docs/02 §5](../02-networking.md) — the `Drain` bullet — which described drain
as work the persistence tier performs on an island, and adds one parameter to
[D16](0016-parameter-reference.md)'s table.
[D6](0006-population-adaptive-topology.md) is **not** amended: its sentence
("the coordinator forms, merges, splits, and drains islands") stays true word
for word, because retiring the island record *is* the coordinator's act. What
D6 never said, and what §5 wrongly implied, is that the coordinator drives
anything inside `orrery_persistd`.
[D12](0012-backend-services.md)'s service inventory is unchanged, and that is
the point of the record: **no edge is added.**

## Context

Three facts about the landed tree, each read before it was written down here.

**The gateway has no coordinator connection, deliberately.**
`CoordinatorHandoutAuthority` holds only the coordinator's *public* keys and
verifies handouts a peer carries: "the gateway needs no connection to the
coordinator — the peer is the courier, exactly as it is for its identity
token" (`crates/orrery_persistd/src/gateway.rs:715-722`). Every coordinator
fact the persistence tier acts on today arrives couriered by the peer it is
about.

**`orrery_persistd` has no island concept at all.** Grepping `island` over
`crates/orrery_persistd/src/` returns three hits, all prose: a comment naming
the `p3-island` harness (`bin/persistd.rs:2050-2052`) and one doc line about a
witness that rejoins an island (`gateway.rs:421`). There is no `IslandId` in
the crate, no island-keyed state, and no message on the client↔gateway wire
(`crates/orrery_protocol/src/gateway.rs:33`, `GatewayMsg`) that names one. The
persistence tier shards by *cell*, and a cell is the only spatial unit it has.

**The coordinator already drains, and never sends `Drain`.**
`Registry::remove_peer_from_island` retains the departing peer out of the
island, and "if `drain` { self.islands.remove(&island_id) }"
(`crates/orrery_coordinator/src/registry.rs:271-283`) — the island record is
simply forgotten. `CoordMsg::Drain`
(`crates/orrery_protocol/src/coord.rs:395-401`) is constructed nowhere in the
tree; the coordinator server lists it among the variants a peer must not send
and ignores (`crates/orrery_coordinator/src/server.rs:540-543`), and the Bevy
client records it without acting: "Recorded rather than acted on: draining
releases leases and parks cells, which is authority's business (D7), not the
session layer's" (`crates/orrery_net/src/coordinator.rs:147-151`,
`:541-543`).

So the only thing genuinely undecided is what the message *means*, and whether
anything new has to be built for the §5 bullet to be true.

### What §5 promises, and what exists

`docs/02-networking.md:150` reads, today:

> **Drain:** last peer leaves → authority leases are released or expire (D7),
> cell actors checkpoint and quiesce (D11), cells are **parked**: no live
> authority, state served from the hot tier, optional catch-up simulation on
> next load.

Clause by clause, against the tree:

| §5 clause | What exists | Where |
|---|---|---|
| leases *released* | `LeaseMsg::Divest { to: None }` → `park_lease`, synchronously | `orrery_protocol/src/authority.rs:243-254`; `orrery_persistd/src/gateway.rs:5712` (`divest_lease`) |
| leases *expire* | 1 s sweep → `sweep_expired_leases` → park → redistribute | `orrery_persistd/src/gateway.rs:4476` (the sweep call) and `:3598`; `src/lease.rs:554` |
| (unwritten third path) | clean session teardown parks every held lease | `orrery_persistd/src/gateway.rs:5944` (`cleanup_peer_session`) |
| cell actors *checkpoint* | jittered per-shard timer, plus an immediate `QuiesceSignal::quiesce(cell)` | `orrery_persistd/src/checkpoint/scheduler.rs:170-205`, `:225-300` |
| cell actors *quiesce* | **nothing.** `CellActor` has `shutdown` (mailbox drain, drops the `Arc<Journal>`) and no quiesce | `orrery_persistd/src/actor.rs:1705-1711` |
| cells *parked* | the `PARKED` lease flag, per entity | `orrery_persistd/src/lease.rs:303-321`, `:461-475` |

Every clause except "quiesce" is already implemented, **per entity and per
cell, with no island in sight**. The `QuiesceSignal` that does exist is an
immediate-checkpoint request whose doc says the *coordinator* asks for it
(`scheduler.rs:170-174`); it is `pub`, it has exactly one caller in the whole
tree — `crates/orrery_persistd/tests/checkpoint_restore.rs:429` — and there is
no wire message by which any coordinator could reach it.

### The two ways to close the gap

**Add a coordinator→gateway control edge.** It is absent from D12's five-service
inventory, and it inverts the courier model at `gateway.rs:715-722` — the
gateway would have to hold a *connection* to a coordinator, not merely its
public keys. Worse, it makes drain stall on coordinator availability, and
`docs/09-services-and-ops.md:15` states the coordinator's blast radius as "No
*new* islands, merges, promotions, or witness epochs; **running islands
unaffected**". An island that cannot finish draining while the coordinator is
down is an island affected by the coordinator being down.

**Or observe that the work is already done by the existing paths**, and say so.

## Decision

### (a) Drain is peer-driven

> **An island drain is executed entirely by the departing peer and the
> persistence cluster's existing per-entity paths; the coordinator's only act
> is to retire the island record and, at most, to notify the peer — no
> coordinator→gateway control edge exists, and D12's service inventory gains
> no edge from this record.**

A reviewer looking for the sentence that settles whether a coordinator→gateway
edge exists should point at that one.

**The one place that looked like a counter-example is the P6 promotion warrant,
and it is couriered.** [docs/04](../04-authority.md) §8 step 1 has the
coordinator issue `{cell_ids, host: NodeId, epoch, expiry, signature}` "to the
field host and the registrar", which reads as a second delivery and would be
the edge this record declines. It is not one: step 2 has the host send
`Claim{basis: Promotion{warrant}}`, so the registrar receives the warrant **on
the host's own claim** and verifies its signature against the coordinator's
public keys — exactly the courier already used for interest handouts
(`gateway.rs:715-722`: "the gateway needs no connection to the coordinator —
the peer is the courier, exactly as it is for its identity token"). §8 step 1
is reworded here to say so. Field-host promotion therefore adds no edge either,
and a future record that wants one must argue for it on its own terms rather
than inherit it from this wording.

The drain predicate is per entity, and there is no island term in it. For an
island `I` over cell set `C(I)`, with `E(I)` the persistent entities whose
lease rows key into those cells:

```
parked(e)   ⟺  lease_row(e).flags ∋ PARKED           (lease.rs:303)
drained(I)  ⟺  ∀ e ∈ E(I).  parked(e) ∨ holder(e) ∉ peers(I)
```

Three paths establish `parked(e)`, and the cluster needs none of them to be
told about `I`:

```
1. explicit divest    Divest{to: None} → park_lease        latency ≈ 1 RTT
2. session teardown   cleanup_peer_session → park_lease    latency ≈ 1 RTT
3. expiry sweep       expires_at ≤ now → sweep_expired      latency ≤ TTL + S
```

with `TTL = 10 s` (`LEASE_TTL_MS`, `lease.rs:25`, D7/D16) and `S = 1 s`, the
gateway's sweep period (`gateway.rs:3600`). Path 3 is the one that makes drain
*coordinator-independent by construction*: it fires on a wall clock inside the
gateway process, so

```
T_drain(I)  ≤  T_last_peer_gone + TTL + S  =  T_last_peer_gone + 11 s
```

holds whether the coordinator is up, down, partitioned, or has never heard of
`I`. Nothing an added edge could do would improve that bound; it could only
introduce a second way for it to fail.

### (b) `Drain` never targets a populated island

> **`CoordMsg::Drain` may be sent only for an island whose population has
> already reached zero, its one legitimate recipient is the peer whose
> departure emptied it, and `deadline` therefore means "comply before this
> instant" — a compliance horizon for that peer's outstanding divestitures —
> and never "you will be evicted at this instant".**

D6 and §5 fix the trigger at population zero
(`docs/02-networking.md:143`: `Active --> Draining: population reaches 0`).
The order is emitted, if at all, in the same coordinator turn that processes
the last departure, on a session that is by definition still open for exactly
one more message. Concretely, the coordinator's turn becomes:

```
on peer_left(p, I):
    remove_peer_from_island(p, I)
    if peers(I) = ∅:
        if session(p) is open:
            send p  Drain { island: I, deadline: now_ms + DRAIN_GRACE }
        forget I
```

**The message is advisory, and must remain redundant.** If `p` crashed, there
is no session and no recipient — and path 3 above still drains `I` inside
11 s. A drain that is only correct when the notice is delivered would be a
drain that fails on exactly the departure mode (a crash) it most needs to
handle. So the notice buys latency, not correctness: it converts an
11-second expiry drain into a one-RTT cooperative one, on the majority of
departures, which are graceful.

**Evacuation is out.** Draining a *populated* island would mean relocating
live peers, which needs a destination island, a manifest handover and a state
re-page — and those already exist under different names: that is Merge and
Split (§5), not Drain. Nothing in the tree can evacuate, and this record
declines to invent it. If region evacuation or host maintenance ever needs it,
it is a new ADR that supersedes this clause and names its own mechanism; it is
not a reinterpretation of a `deadline` field.

### (c) "Checkpoint and quiesce" reduces to parking plus the ordinary cadence

> **With no per-island or per-actor quiesce API built, "cell actors checkpoint
> and quiesce" reduces to: every affected lease row is parked by one of the
> three paths in (a), and the cell's state reaches durability on the ordinary
> 20 s jittered checkpoint cadence (D16) — no cell actor is stopped, no actor
> state is dropped, and the only "quiesce" in the crate is
> `QuiesceSignal::quiesce(cell)`, an immediate-checkpoint request with no wire
> path and no production caller.**

"Quiesce" in §5 was doing two jobs and can do only one of them today:

- *Stop accepting writes* — unnecessary. A parked row has no holder, so no
  peer holds a fencing token for it, so nothing authorized can write it. The
  registrar's CAS is the quiesce.
- *Flush and release memory* — half true. The checkpoint half is real (the
  scheduler's per-shard timer, or a quiesce-flush). The release half does not
  exist: **grepping `evict` over `crates/orrery_persistd/src/` returns three
  hits, all in the gateway's idle-*peer*-registry eviction**
  (`gateway.rs:2501`, `:2627`, `:3607`); there is no cell-state eviction path
  anywhere in the crate. `scheduler.rs:172-174`'s claim that quiesce-flushing
  keeps "hot memory bounded by *populated* cells, not universe size" is
  therefore a statement about a path that is not built yet.

Worst-case time from the last peer leaving to the drained island's state being
durable, with nothing cooperating and no quiesce-flush:

```
T_durable  ≤  TTL + S      +  (interval + jitter)
           ≤  (10 + 1) s   +  (20 + 5) s       =  36 s
```

and with a graceful divest plus the quiesce-flush that already exists, one RTT
plus one checkpoint write. Neither number is a budget this record sets; they
are what the tree does, written down so the next person does not have to
re-derive them.

### (d) The drain grace: 10 s

`DRAIN_GRACE = 10 s`, exactly D7's lease TTL, added to D16's table.

The choice is forced rather than tuned. Let `G` be the grace the coordinator
stamps into `deadline`:

- `G < TTL` asks a peer to finish inside a window the registrar cannot
  observe faster than `TTL` anyway — the deadline would pass while the
  backstop is still counting, so the coordinator would be asserting a state it
  cannot check.
- `G > TTL` is dead time: by `TTL + S` the sweep has parked every row
  regardless, so a longer grace names an instant after the drain is already
  complete.
- `G = TTL` makes the cooperative deadline and the uncooperative backstop
  coincide to within one sweep period `S = 1 s`, which introduces **no third
  timer** into a system that already has two.

So `deadline = now_ms + 10_000`, and a peer that has not divested by then is
not punished — it is simply no longer the reason anything is waiting.

## Consequences

- **`docs/02-networking.md:150` is narrowed by this record and rewritten to
  match.** The old bullet read as though the persistence tier performed an
  island-scoped operation; the new one names the three parking paths, the
  checkpoint cadence, and the absence of a coordinator edge. The `Draining`
  state at line 143-144 is unchanged — the state machine was always right; the
  bullet under it was not.
- **`CoordMsg::Drain` stays on the wire, unchanged, still unsent.** No field
  is added or removed: postcard keys enum fields by declaration order, so
  removing `deadline` would break every deployed peer's decode to save eight
  bytes on a message nobody sends. It now has a written meaning, which is what
  it lacked.
- **Nothing is built by this record.** It is a decision that the existing
  paths are the drain, so the peer-side drain task it unblocks is small: honour
  an advisory `Drain` by divesting held leases in the named island before the
  deadline, and nothing else. `orrery_net::CoordinatorLink::drain`
  (`coordinator.rs:147-151`) is already where that hand-off lands.
- **A doc comment in the persistence crate is now wrong and this record does
  not fix it.** `checkpoint/scheduler.rs:170-174` and `:200-202` attribute the
  quiesce signal to "the coordinator", which under (a) can never reach it.
  Correcting those two comments — and deciding whether `QuiesceSignal` should
  stay `pub` with no caller — is a code change, out of scope here.
- **The "hot memory is bounded by populated cells" claim is now traceable to
  the gap that makes it false.** There is no cell-state eviction path. Drain
  does not create that problem and does not fix it; it is named here so it is
  not mistaken for something drain already handles.
- **Drain survives a coordinator outage, and that is now a stated property
  rather than an accident.** `docs/09-services-and-ops.md:15`'s "running
  islands unaffected" extends to islands in the act of ceasing to run.
- **Two records still say drain is deferred.** `docs/04-authority.md:44` and
  `docs/11-roadmap.md:305` both list "coordinator-driven island drain" as a P3
  follow-on. Under (a) the phrase names something that will never be built;
  both lines want rewording to "peer-driven island drain (D24)" once this is
  accepted. Neither file is edited here — they are owned elsewhere this round.

## Alternatives considered

- **A coordinator→gateway drain RPC (coordinator-driven drain).** The shape
  the §5 bullet implied. Rejected on three independent grounds, any one of
  which is sufficient: it adds a service edge D12's inventory does not have; it
  makes drain completion depend on coordinator availability, contradicting
  `docs/09-services-and-ops.md:15`; and it is *redundant* — the registrar's 1 s
  expiry sweep already parks every row within `TTL + S` with no message at all,
  so the edge would buy a latency improvement the peer's own `Divest` buys more
  cheaply. It also inverts the trust direction at `gateway.rs:715-722`, where
  the gateway holds coordinator *keys* and not a coordinator *session*.
- **Teaching `orrery_persistd` about islands.** The prerequisite for the
  option above, and worse than it: an `IslandId` in the persistence tier is a
  coordinator concept whose entire content is a set of cells the tier already
  shards by, kept coherent across merge, split and epoch bumps by a second
  copy of the coordinator's state machine. Rejected as a duplicated authority
  over a fact the cell key already carries.
- **Letting `Drain` evacuate a populated island.** Rejected under (b): the
  mechanism it would need is Merge/Split, the trigger D6 defines is population
  zero, and at that trigger there is no populated island to evacuate. Reopening
  this needs a superseding ADR, not a reading of `deadline`.
- **Deleting the `deadline` field, since drain fires at population zero.**
  Tempting and wrong: it is a wire-breaking change (postcard positional
  encoding) to remove a field that has a coherent meaning under (b), and the
  cooperative path is exactly where a horizon is useful.
- **Stopping cell actors on drain (`CellActor::shutdown`).** Rejected.
  `actor.rs:1705-1711` is the runtime's lifecycle path — it drains the mailbox
  and releases the journal's file lock — not a per-island one. Stopping actors
  when an island empties would make the next arrival pay an actor cold start on
  §5's *Form* path, to reclaim memory that no eviction path reclaims today
  anyway.
- **A `GatewayMsg::Quiesce { cell }` so the departing peer can force the
  flush.** Genuinely attractive — it would collapse `T_durable` from ~36 s to
  one RTT — and rejected for now because it hands any authenticated peer an
  unmetered way to force immediate checkpoints of any cell it names, which is
  write amplification against the same device the journal is on (the effect
  D23 measured at a 10× cadence: `journal_commit_ms` p99 from 30 ms to 75 ms).
  If hot-memory bounding later needs it, it comes back gated by the peer's own
  interest coverage and drawing on the same NodeId-scoped claim bucket a `Divest` already
  draws from (`gateway.rs:4739-4751`), like every other lease-control message.
- **A dedicated drain-completion acknowledgement back to the coordinator.**
  Rejected: the coordinator has already forgotten the island by then
  (`registry.rs:280-282`), so the ack would arrive about a record that no
  longer exists, and nothing would read it.
- **Amending D6.** Rejected as unnecessary: D6 says the coordinator drains
  islands, and under this record it still does — it is the actor that decides
  an island is over and retires it. Only §5's operational bullet overclaimed.
