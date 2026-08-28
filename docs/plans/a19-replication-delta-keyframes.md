# A19 - Keyframes and deltas on the replication wire: pay for change, not for state

> Design research for the replication bandwidth ceiling: #621 (nightly red
> since 2026-08-24: peak 1,044,496 bits/s across 32 peers against the 1 Mbps
> clause, 2,266 packets shed against an allowance of 0) and the second of
> #650's two owner calls (v18 quadruples the Interceptor speed ceiling and
> adds a 25-byte replicated trail; its own projection is ~16.85% over budget
> before DEFLATE). Repository facts verified in a worktree at `a1e28065` on
> 2026-08-28; every `path:line` below was read before being cited. **Propose,
> not decide** - section 10 lists what stays with the owner. Nothing here
> amends docs/03-replication.md or any accepted ADR; section 4 states the
> relationship to docs/03 section 5.1 precisely.

## Verdict up front

**Sender-clocked keyframes with keyframe-referenced binary deltas, not
acked per-link baselines - yet.** Every replication send today is a full
absolute snapshot: a stateless broadcast of the entity's whole canonical
state, 20 times a second, to every peer whose 27-cell set contains it
(`gates/p1-swarm/src/bot.rs:1139-1204`). The arithmetic in section 1
reproduces last night's measured peak to within 0.3% from that shape alone,
and section 3 shows the wire is pricing *state*, which is large and mostly
constant, instead of *change*, which is physics-bounded and small: a parked
craft and a 480 m/s interceptor cost identically today, and v18's 4x speed
ceiling costs roughly one extra byte per axis once deltas are the unit.

The scheme in section 5: the sender keeps emitting today's exact absolute
message as a **keyframe** once a second, and between keyframes emits a new
`TAG_REPLICATION_DELTA` message - a skip/write binary patch of the current
canonical bytes against the sender's own last keyframe. No acks, no per-link
state, no receiver feedback: the same encoded bytes go to every recipient,
preserving `broadcast_state`'s encode-once shape, and because every delta
patches the *keyframe* rather than the previous delta, any delta fully
supersedes the one before it - which keeps the budget meter's FIFO shedding
sound exactly as its own doc comment argues it
(`crates/orrery_net/src/budget.rs:449-453`). Modeled effect (section 7):
~1,050 kbps worst-peer peak falls to ~470-550 kbps at v18 speeds *including*
trails, with the shed count expected at zero - the gate's allowances (0 and
162) stay exactly where #621 insists they stay, and go green by the traffic
shrinking, not by the criterion moving.

The acked-baseline design in docs/03-replication.md section 5.1 (encode
against the last state the receiver acknowledged) is not rejected - it is
the lightyear end-state of ADR-0004, and it stays the end-state. It is
declined *for this wire* because this wire is the interim hand-rolled path:
there is no entity-level ack channel to build on (the only ack in the tree,
`UplinkAck`, is a link-level loss counter -
`clients/regolith/src/net.rs:147-160`), and per-receiver encoding would
break the one-encode-31-sends broadcast shape for a gain section 5.6 shows
is about one byte per axis over the keyframe-referenced form.

## 1. The wire as it is, and the arithmetic that reproduces last night

One craft update, gate path (`gates/p1-swarm/src/bot.rs:1181-1184`):
`encode_replication_compressed(&(state.to_canonical(), cell, entity, tick + 1))`
- an anonymous 4-tuple, postcard-encoded, sub-tagged, channel-tagged, then
channel-tagged *again* by `send_peer_packets` (the deliberate double tag,
`clients/regolith/src/campaign.rs:1628-1638`, pinned since #387).

| piece | bytes | note |
|---|---|---|
| channel tag, outer (`peer_link`) | 1 | `crates/orrery_net/src/peer_link.rs:225` |
| channel tag + sub-tag `0xE6` | 2 | `crates/orrery_protocol/src/channels.rs:117,127-133` |
| varint length of canonical body | 2 | postcard `Vec<u8>` prefix |
| canonical `Craft` body | 132 | `CRAFT_ENCODED_LEN`, `crates/orrery_games/src/regolith/state.rs:283` |
| `CellId` | 10 | level-21 interest ids (`INTEREST_LEVEL = MAX_LEVEL = 21`, `crates/orrery_protocol/src/cell.rs:38,162`) put Morton bits up to bit 63 (`cell.rs:225-230`), so the plain-u64 serialize (`cell.rs:421-425`) varints to 9-10 B |
| `PersistId` + tick varints | 3-5 | tick is 3 B for most of an hour |
| **message on the wire** | **~150-152** | |
| datagram overhead | 60 | `DATAGRAM_OVERHEAD_BYTES`, `crates/orrery_net/src/budget.rs:48` |
| **charged per recipient per send** | **~210-212** | `datagram_wire_bytes`, `budget.rs:60-63` |

The nightly criterion population is 32 peers, all inside one another's
27-cell sets in the witnessed leg (`--min-cells 1`,
`scripts/p1-swarm-gate.sh:171-174`), one authored craft each (the gate legs
never enable campaign rocks - `campaign: args.external_peer`,
`gates/p1-swarm/src/main.rs:661`), sending every 3rd tick of 60 Hz
(`swarm.rs:1350`, default `send_hz` 20 at `swarm.rs:105`):

```text
210.6 B x 8 bit x 20 Hz x 31 recipients = 1,044,600 bits/s
measured nightly peak (#621)            = 1,044,496 bits/s
```

That is the whole story of the red clause: the peak *is* the
full-interest-set broadcast, byte for byte. The residual (~0.1%, the
tick-varint width drifting and a trickle of non-replication bytes) is below
the noise floor of the model. #649's DEFLATE (merged as `070a6705`) trims
what it can from a 146-byte high-entropy body - the full-branch probe
measured 919 kbps, an ~12% cut - and #650's trails would put ~124 kbps back
(`.claude/worktrees/v18`, commit `8f9f7dcd`). Compression of a snapshot is a
constant factor on the wrong quantity. The quantity is wrong because:

**There is no per-link, per-entity send state anywhere.** `broadcast_state`
is stateless; the receiver holds only `Replica(NodeId)` + `LastSeen(u64)`
with `REPLICA_TTL_TICKS = 120` (`bot.rs:320-350`). No baselines, no
sequence numbers, no keyframe/delta discriminator in the `TAG_REPLICATION`
grammar, and the shedding at `peer_link.rs:248-258` is FIFO with no
priorities. The interest-ranking machinery in
`crates/orrery_spatial/src/interest.rs:25-49` (high-rate set, 1-4 Hz
proxies) is receiver-side scaffolding the send gate does not consult; the
only thing that decides what goes on the wire is
`entry.cells.contains(&cell)` at `bot.rs:1186`.

## 2. Where the bytes go

Of the ~211 B charged per recipient per send:

- **60 B (28%)** - datagram overhead. Untouchable per message; only
  coalescing (one datagram per link per send tick, already the docs/03
  section 5.3 posture) reduces it, and only for senders with >1 entity.
- **~19 B (9%)** - envelope: three tag bytes, length prefix, 10 B of
  `CellId` that changes at most every few *seconds* (committed cell,
  hysteretic by design - `bot.rs:1146-1153`), identity and tick.
- **132 B (63%)** - canonical `Craft`. Of these, roughly 56 B are
  kinematics that genuinely change every tick (pos 24, vel 24, yaw+pitch 8 -
  `state.rs:39-92`), and ~76 B are monotone counters, lock state, cooldowns
  and flags that change on *events*: hull, shield, shots, damage_dealt,
  pickups, kills, lock_target and friends. Between two 50 ms sends, most of
  those 76 bytes are byte-identical. So are the *high* bytes of every
  kinematic `i64`: position moves by millimetres-per-interval amounts that
  live in the low 2-3 bytes of each little-endian lane.

The wire spends 132 B to move ~15-35 bytes of actual change. That ratio,
not the compression level, is the budget problem.

## 3. The entropy that is actually on the wire

Physics bounds the change per 50 ms send interval; the constants are all in
the tree.

| quantity | bound | changed low bytes per i64/i32 lane |
|---|---|---|
| pos delta, gate cruise (32 m/s, `CRUISE_MPS`, `gates/p1-swarm/src/bot.rs:94`) | 1,600 mm | 2 of 8 per axis |
| pos delta, v17 Interceptor max (120 m/s, `crates/orrery_games/src/regolith/archetype.rs:94`) | 6,000 mm | 2 of 8 |
| pos delta, v18 Interceptor max (480 m/s, worktree `v18`) | 24,000 mm | 2-3 of 8 |
| vel delta under full thrust (60 m/s^2, `archetype.rs:95`) | 3,000 mm/s | 2 of 8; **0 while coasting** |
| yaw delta at gate turn rates | ~thousands of urad | 2 of 4; pitch input-locked to 0 (`state.rs:53`) |
| event fields (hull, shots, locks, ...) | event-driven | 0 almost every interval |

Two consequences worth stating plainly:

1. **Delta size is speed-logarithmic; snapshot size is speed-invariant.**
   Quadrupling the speed ceiling (v18) adds about one changed byte per
   position axis. The full-snapshot wire charges 132 B for a parked craft
   and 132 B for a 480 m/s one. Deltas make v18's speed nearly free on the
   wire; snapshots make it invisible - which sounds fine until the *trail*
   (25 B of new always-carried state) lands on every one of the 620
   sends-per-second a peer makes, which is exactly #650's +124 kbps.
2. **Most sends of most entities are near-empty.** A Pickup (42 B body,
   `state.rs`) is byte-identical between grabs; a coasting craft changes
   ~8-14 bytes. Today each still costs a full body plus 60 B of datagram.

These are analytic bounds, not measurements. Lane 1 of the decomposition
(section 9) is a measurement leg that XORs consecutive canonical states in
the gate and histograms changed-byte counts per body type, so the codec's
size assumptions are grounded before the codec exists. Estimated
distributions used in section 7: cruise craft patch 15-25 B, combat craft
patch 25-40 B, static bodies 0 B.

## 4. The design space

Three shapes were considered. The constraint that sorts them is stated
first: **the current path has no receiver feedback and encodes once per
entity per send** (`bot.rs:1178-1188` builds one payload and clones it per
recipient). Anything per-link multiplies encode work by ~31 and needs an
ack channel that does not exist.

**(a) Acked per-link baselines** - docs/03-replication.md section 5.1, the
Gaffer lineage, and what lightyear ships. Smallest deltas (previous-send
reference: ~1-2 changed bytes per axis). Cost: a per-link ring buffer of
past states, `(entity, tick)` acks piggybacked on a return stream, per-link
encoding, and baseline-invalidation rules across authority changes. This is
the right end-state *inside lightyear* when the D4 stack replaces the
hand-rolled path; hand-building it into a wire the stack will delete is the
wrong place for that complexity. Declined here, not amended anywhere.

**(b) Chained deltas** (each delta references the previous delta).
Smallest of all and trivially sender-clocked - and rejected outright: one
lost or *shed* datagram desynchronizes the chain until the next keyframe,
and it silently breaks the argument `is_sheddable` rests on ("a dropped
update is superseded 50 ms later by the next",
`crates/orrery_net/src/budget.rs:449-453`). A scheme that makes FIFO
shedding corrupting instead of lossy would have to rebuild the shedder too.

**(c) Sender-clocked keyframes + keyframe-referenced deltas** - chosen.
The sender emits an absolute keyframe on its own schedule (no feedback),
and every delta patches the last keyframe, not the last delta. Deltas are
mutually superseding, so loss and shedding cost exactly one 50 ms update,
same as today. Encode-once survives: keyframe bytes and delta bytes are
identical for every recipient. The price relative to (a): a delta's
reference is up to 1 s old, so residuals are position-drift-sized
(<= 480,000 mm at v18 max = 3 low bytes per axis) instead of
per-interval-sized (2 bytes) - about one byte per axis per lane, per
section 3's table. That byte buys the removal of the entire ack/ring/
per-link apparatus.

## 5. The scheme, precisely

### 5.1 Keyframes are today's messages, unchanged

A keyframe is byte-for-byte the existing absolute message -
`TAG_REPLICATION` / `TAG_REPLICATION_COMPRESSED`
(`crates/orrery_protocol/src/channels.rs:117,125`) carrying
`(canonical, cell, entity, tick)`. No new decoder for it; a receiver that
does not understand deltas degrades to 1 Hz absolute state instead of
breaking (`decode_sub_tagged` returns `None` on an unknown marker,
`channels.rs:152-183`, so old code drops delta datagrams silently).

Cadence: `KEYFRAME_EVERY_SENDS = 20` proposed (1 Hz at the 20 Hz send
rate), staggered per entity by `PersistId` so a sender with several
authored entities does not burst all keyframes on one send tick. The value
is an owner knob (section 10); the receiver freshness TTL it must stay
under is `REPLICA_TTL_TICKS = 120` = 2 s (`bot.rs:329`), and keyframes
become the liveness heartbeat (section 5.4).

### 5.2 The delta message

```text
[Channel::State][TAG_REPLICATION_DELTA = 0xEB]   (new sub-tag; value owner-confirmed)
postcard tuple:
  entity   : PersistId          varint, 1-2 B
  tick     : u64                varint, ~3 B
  kf_age   : u16                varint, 1 B    # ticks back to the referenced keyframe
  cell     : Option<CellId>     1 B None; ~11 B only on committed-cell change
  patch    : Vec<u8>            varint len + patch body
```

`cell` rides only when the committed cell changed since the keyframe -
D2's single-writer commitment stays the only source (`bot.rs:1146-1153`);
the receiver holds the last delivered value per replica. This alone deletes
10 B from 19 of every 20 messages.

### 5.3 The patch body: skip/write over canonical bytes

The patch is a byte-level diff of `to_canonical(current)` against
`to_canonical(keyframe state)`, treating both as opaque:

```text
patch    := new_len:varint , op*
op       := skip:varint , write_len:varint , write_len bytes of literal
             # alternating; skip copies bytes from the keyframe at the same
             # offset; a zero skip or zero write is legal so runs alternate
             # freely; output beyond the keyframe's length is always literal
```

Chosen over a schema-aware per-field codec (`Diffable`, the docs/03
section 2 registration surface) deliberately: it is one implementation in
`orrery_protocol`, game-agnostic, works identically for `Craft`, `Rock`,
`Pickup`, `BloomDirector` and every future ruleset with zero coupling to
`orrery_games`, handles v18's variable-length trail tail for free
(`new_len` differs, tail is literal), and requires no zigzag or
quantization knowledge because the canonical encoding is already quantized
fixed-width LE (`state.rs:320-364`) whose change-locality section 3
established. The estimated overhead versus schema-aware is a few token
bytes per contiguous changed run. DEFLATE composes if it ever wins, via the
established smaller-only rule (`channels.rs:197-221`), but at 30-45 B per
message it will not, and that is fine.

**Reconstruction is exact by construction and checked by law:**
`apply(kf_canonical, patch) == to_canonical(current)` byte-for-byte, which
means the receiver's reconstructed state hashes to the author's
`state_hash`. That keeps the one place adjudication-adjacent code touches
replication-delivered state working: `try_reanchor` accepts a
replication-delivered sample only when `orrery_core::state_hash` over it
equals the claim's hash (`crates/orrery_witness/src/witness.rs:1609-1627`).
A delta wire that reconstructs exact canonical bytes *strengthens* that
path; one that reconstructed approximately would kill it.

### 5.4 Elision, liveness, new subscribers

- **Empty deltas are not sent.** If the patch is empty and the cell
  unchanged, the sender emits nothing this interval. Static bodies
  (pickups between grabs, idle directors) drop from 20 Hz x ~160 B wire to
  1 Hz x ~110 B. Keyframes at 1 Hz keep `LastSeen` inside the 2 s TTL.
- **A newly interested peer** (roster refresh adds it to the entity's
  27-cell audience, `swarm.rs:1152-1159`) gets the cached encoded keyframe
  bytes immediately on the next send tick, then deltas. One full message,
  no per-link encoder state - the cache is the same bytes everyone got.
- **Authority movement**: a new authority has no keyframe history, so its
  first send is necessarily a keyframe - the docs/03 section 7 "absolute
  on auth change" behaviour falls out with no rule.

### 5.5 Loss and shedding behaviour

- Lost/shed **delta**: the next delta supersedes it entirely (all deltas
  reference the keyframe). Cost: one 50 ms update - identical to today.
  FIFO shedding at `peer_link.rs:248-258` remains sound unmodified.
- Lost/shed **keyframe**: deltas referencing it are undecodable on that
  link; the receiver drops them (proposed counter: `deltas_unanchored`)
  and coasts on the previous keyframe's reconstruction until the next
  keyframe - bounded staleness of one keyframe interval, inside the 2 s
  TTL, masked the way any 1 s outage is today. Under the impaired profile
  (3-5% loss) that is a ~4% chance per entity-link-second of a <= 1 s
  stale window. If the owner wants it smaller: send each keyframe twice on
  consecutive send ticks (+~5% wire) or shorten the cadence - both knobs,
  neither machinery.
- Post-delta the worst peer runs at ~50% budget (section 7), so the meter
  should not trip at all; the shed allowances stay 0/162 and the gate goes
  green by traffic, not by allowance. #621's rule is honoured literally.

### 5.6 What stays out, on purpose

No per-link priority accumulator, no proxy rates, no interest-set changes,
no `Diffable` registration surface, no lightyear migration - docs/03
sections 4-5 keep owning those futures. This node is one wire optimization
of the interim path, designed to be *deleted whole* when the D4 stack
lands, and cheap enough (~one codec file, ~two integration sites) that
deleting it costs nothing to mourn.

## 6. What it must not touch

Hard lines, in order of blast radius; each is a review checklist item for
every lane in section 9.

1. **The adjudication path reads none of this.** `verify_bundle` is a pure
   function of `LogFrame`/`StateClaim` bytes
   (`crates/orrery_core/src/replay.rs:323,331-442`); the claim preimage
   commits to `input_head` and `state_hash` only
   (`crates/orrery_core/src/log.rs:329-341`). Untouchable:
   `CoreCodec::to_canonical`, `InputRecord` payload encoding,
   `claim_preimage` field order, `NeighborFrame` semantics. The delta codec
   consumes canonical bytes; it never defines them.
2. **Lane charging must stay honest.** `lane_of` charges anything on
   `Channel::State` not positively witness-tagged to Replication
   (`crates/orrery_net/src/budget.rs:456-477`) - `0xEB` lands there by
   default, and a pin test must say so, because the doc comment's threat
   ("a caller can never make its traffic cheaper by leaving the tag off")
   applies to new tags too.
3. **H2 (ADR-0050, Proposed): replication behaviour may not consult
   hearsay or summary state in either direction**
   (`docs/adr/0050-knowledge-tiers.md`). This scheduler consults only the
   sender's own tick counter, its own keyframe, and the D2 roster - state
   it, and keep it true.
4. **F(E) staleness must not lengthen** (ADR-0050 clause on
   replicated-state facts): every delta reconstructs a *full* canonical
   state at the full 20 Hz, so the presentation tier's fact stream is
   unchanged in cadence; only the loss-tail behaviour (5.5) differs, and
   it is bounded.
5. **`try_reanchor` needs reconstructable full state** - satisfied by 5.3's
   exactness law. (Finding, not blocker: `Witness::observe` currently has
   no production caller - the samples map is fed only by tests
   (`crates/orrery_witness/tests/detection.rs`), so this path is latent
   either way. Noted in section 11.)
6. **Version pinning is manual.** Nothing derives compatibility from the
   wire format (ADR-0049 section 1: the digest is a placeholder constant).
   Precedent: #649 changed this same wire, bumped nothing, and rode the
   v16-to-v17 ruleset rebuild. The owner picks the axis (section 10);
   `PROTOCOL_VERSION` exact-equality admission is at
   `crates/orrery_protocol/src/gateway.rs:182-184`.

## 7. The arithmetic after

Per recipient, per second, one v18 combat craft (worst realistic case;
patch sizes from section 3's bounds pending lane 1's measurement):

| message | count/s | wire B each | B/s |
|---|---|---|---|
| keyframe (today's message + trail) | 1 | ~235 | 235 |
| delta (pos 9 + vel 9 + yaw 3 + events ~4 + trail amortized ~2 + tokens ~10 + envelope ~11 + tags 3 + overhead 60) | 19 | ~110 | 2,090 |
| **total** | | | **2,325 B/s = 18.6 kbps** |

Times 31 recipients: **~577 kbps** worst-peer peak at v18 with trails -
versus ~1,168 kbps projected for v18 on the snapshot wire (#650's number),
and 1,044 kbps measured today without trails at v17 speeds. The gate's
cruise-only legs model lower (patch 15-25 B): **~470-490 kbps**, shed
expected 0 against allowance 0.

The honest ceiling underneath: **the datagram overhead floor is
60 B x 20 Hz x 31 = 298 kbps** regardless of payload - more than half of
the post-delta spend. The levers past it are known and out of scope here:
coalescing multiple entities per datagram per link (docs/03 section 5.3;
only helps multi-entity senders), proxy-rate tiers for far entities
(docs/03 section 4.2; the `interest.rs` scaffolding is already receiver-side
in the tree), and send-rate itself. This node's claim is only that delta
coding roughly halves the peak and restores v18's headroom; it does not
claim the last word on the budget.

## 8. How to measure

**Before building anything** - lane 1 grounds the size model: a
`--delta-stats` capture in `gates/p1-swarm` that, per send interval, XORs
each entity's canonical bytes against (a) the previous send and (b) the
last would-be keyframe, and prints per-body-type histograms of changed-byte
counts and patch-size estimates (p50/p95/max), on the existing clean and
impaired legs. Pure observation, no wire change; it either confirms
section 7's 15-40 B patch estimates or corrects this node before a codec
exists. It reuses the simulated-hour property (`scripts/p1-swarm-gate.sh`
header): deterministic from the seed, so the histogram is reproducible.

**After** - the existing gate is already the settling instrument, and it
must not move: peak clause `worst_peak_upload_bits > budget_bits`
(`gates/p1-swarm/src/swarm.rs:1966-1972`), shed allowances 0 and 162
(`scripts/p1-swarm-gate.sh:117-174`), replication kB in the report
(`main.rs:964-970`). Additions are counters only: keyframe/delta message
and byte split in `LaneTally`-adjacent reporting, `deltas_unanchored`, and
the keyframe-loss stale-window distribution on the impaired leg. Success is
the same criterion the nightly runs tonight, green with allowances intact,
plus the split showing keyframes at ~5% of replication messages.

## 9. Decomposition

Five lanes, house issue format, strictly ordered 1 -> 2 -> 3 -> 5 with 4
parallel to 3. All are **propose-only drafts** - filing them is the owner's
call after this node is judged.

### Lane 1 - measure the change, not the state (type:measurement)

Digest tree: NO (gate only). Blocked on nothing; gates lanes 2-5.

Why: every size in this node's section 7 is an analytic bound. The codec's
grammar and the keyframe cadence should be chosen against a measured
changed-byte distribution, not a derivation.

Acceptance criterion: a `--delta-stats` flag on `gates/p1-swarm` emitting,
for the clean and impaired hours, per-body-type histograms of changed
bytes vs previous send and vs 1 Hz keyframe, into the JSON report.

Files (exclusive): `gates/p1-swarm/**`. Do not touch `crates/**`.

MUTATION CHECK: no production stage changes; the honest form is a
verification transcript.

```
Verify:  run the clean leg with --delta-stats on two different seeds
Expect:  identical histograms per seed across two runs (simulated time,
         no wall clock); craft p95 changed-bytes-vs-keyframe well under
         the 132-byte body
```

### Lane 2 - the keyframe/delta codec (type:task)

Digest tree: NO - wire envelope only; canonical encoding is consumed, not
defined. Blocked on lane 1 (sizes may adjust the token grammar).

Why: section 5.2-5.3. One new sub-tag `TAG_REPLICATION_DELTA`, one
skip/write patch codec, pure functions beside
`encode_replication_compressed` in `crates/orrery_protocol/src/channels.rs`.

Acceptance criterion: a named property
`a_delta_patch_reconstructs_the_authors_canonical_bytes_exactly` - for
arbitrary (keyframe, current) canonical pairs including length-changing
ones, `apply(kf, encode(kf, cur)) == cur`, and `state_hash` equality on
executor-generated pairs; plus
`an_unknown_state_sub_tag_is_dropped_not_misparsed` and a lane pin
`a_delta_datagram_is_charged_to_the_replication_lane` against
`budget.rs::lane_of`.

Files (exclusive): `crates/orrery_protocol/src/channels.rs`,
`crates/orrery_net/src/budget.rs` (test only), tests.

MUTATION CHECK: the guarded stage is the patch apply loop.

```
Break:   drop the tail rule (output beyond the keyframe length copies
         zeros instead of literals)
Expect:  the reconstruction property fails by name on a length-growing
         pair (the v18 trail case); the fixed-length legs stay green
Revert:  green; git status clean
```

Second required transcript, the inverse:

```
Break:   emit skip runs over bytes that differ
Expect:  the property fails on the fixed-length leg; state_hash equality
         fails with it
```

### Lane 3 - sender and receiver in the gate (type:task)

Digest tree: NO. Blocked on lane 2.

Why: sections 5.1, 5.4, 5.5 in `gates/p1-swarm/src/bot.rs` -
keyframe cadence and stagger, cached keyframe bytes, immediate keyframe to
a newly interested peer, empty-delta elision, receiver keyframe store and
`deltas_unanchored`.

Acceptance criterion: three named legs -
`a_delta_stream_reconstructs_the_same_replica_states_as_the_snapshot_stream`
(byte equality of the receiver's replica trajectory against a full-snapshot
control run, same seed);
`a_newly_interested_peer_receives_a_keyframe_before_any_delta`;
`a_shed_or_lost_delta_is_fully_superseded_by_the_next` (drop an arbitrary
delta; the replica converges on the following delta, not the following
keyframe).

Files (exclusive): `gates/p1-swarm/src/**`.

MUTATION CHECK: the guarded stage is the delta reference.

```
Break:   encode each delta against the previously sent state instead of
         the keyframe
Expect:  the superseding leg fails by name (the replica stays wrong until
         the next keyframe); the reconstruction leg stays green
Revert:  green; git status clean
```

Second transcript:

```
Break:   remove the immediate keyframe on roster-added peers
Expect:  the new-subscriber leg fails by name; the late-join clause of the
         gate fails with it
```

### Lane 4 - client parity (type:task)

Digest tree: NO. Blocked on lane 2; parallel to lane 3.

Why: the shipping client's broadcast still sends the plain uncompressed
snapshot - `clients/regolith/src/campaign.rs:1076` builds
`encode_state_broadcast`, which calls `encode_replication`, not the
`_compressed` form (`campaign.rs:1619-1642`): #649's DEFLATE never reached
the human client's own sends. This lane brings the client onto the same
keyframe/delta path as the gate bot (and closes that gap in passing), so a
human peer's upload obeys the same arithmetic - #603's three humans are
exactly the peers with residential upload.

Acceptance criterion: the existing double-tag fixture
(`campaign.rs:2169`-region) extended to pin the keyframe and delta wire
bytes byte-identically to the gate bot's for the same state pair.

Files (exclusive): `clients/regolith/src/campaign.rs`, its tests.

MUTATION CHECK:

```
Break:   leave the client on plain full-state encode_replication
Expect:  the parity fixture fails by name
Revert:  green; git status clean
```

### Lane 5 - the settling measurement (type:measurement)

Digest tree: NO. Blocked on lanes 3 and 4.

Why: section 8. The nightly criterion, unchanged, is the judge; this lane
records the before/after table (peak, shed, replication kB, keyframe share,
`deltas_unanchored`, witness coverage) against #621 and reports to the
owner. **No allowance moves.** If the numbers disappoint, that is a finding
against this node, not a reason to touch the criterion.

## 10. Owner-reserved decisions

1. **Whether to build this at all** versus waiting for the D4 lightyear
   migration to bring acked baselines wholesale. This node's case: the
   nightly is red now, v18 is queued now, and the scheme is small and
   disposable - but that is a judgement, not a derivation.
2. **Version axis for the wire change**: bump `PROTOCOL_VERSION` 6 -> 7, or
   ride the v18 ruleset rebuild the way #649 rode v17's, with the campaign
   `client_rev` pin doing admission. Precedent exists for both; #649 set
   the softer one.
3. **Keyframe cadence** (proposed 20 sends = 1 Hz) and whether impaired
   links double-send keyframes. Bounded-staleness trade, section 5.5.
4. **Tag value** `0xEB` (next free after `0xEA`,
   `channels.rs:117-125,237-241`).
5. **Ordering against #650**: this node recommends lane 1 immediately,
   lanes 2-5 landing before or with v18 - v18's own commit message defers
   its bandwidth call to exactly this mechanism - but sequencing releases
   is the owner's.
6. **Whether lane 4 folds in or splits out** the client's missing #649
   compression adoption.

## 11. Findings, and what could not be verified

- **The measured peak is the model.** 210.6 B x 20 Hz x 31 recipients
  reproduces 1,044,496 bits/s to 0.1% (section 1). The residual is
  unattributed; nothing in this node depends on it.
- **`clients/regolith` never adopted #649's compression** on its own state
  broadcast (`campaign.rs:1076`, `:1640`) - the gate bot did
  (`bot.rs:1181`). Found while tracing call sites; folded into lane 4.
- **`Witness::observe` has no production caller** - the replication-fed
  sample map behind `try_reanchor` (`witness.rs:1609-1627`) is populated
  only by `crates/orrery_witness/tests/detection.rs`. The constraint in
  section 6 item 5 is therefore currently latent; it is honoured anyway
  because wiring `observe` up is presumably coming, and a wire that made it
  impossible would be a trap laid for the future.
- **Not verified: patch-size distributions.** All section 7 numbers
  downstream of the 15-40 B patch estimates are analytic (section 3's
  physics bounds). Lane 1 exists to replace them with measurements before
  the codec's grammar is frozen.
- **Not verified: which nightly leg produced the 1,044,496 figure.** #621
  attributes it to the P4 clause; the arithmetic matches the pure
  replication broadcast with no witness-lane contribution, which suggests
  the peak sample predates witness traffic on that link or the witness
  share is below the sampling resolution. Worth one look when lane 5 reads
  the numbers, not load-bearing here.
- **v18 facts were read from the worktree branch** (`.claude/worktrees/v18`,
  commits `d41eba68`, `8f9f7dcd`), not from `main` - trail constants,
  speed ceilings and the +124 kbps projection all live there until #650
  merges.
