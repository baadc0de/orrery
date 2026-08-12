# Orrery — Architecture Decision Record

**Status:** Accepted (initial architecture) · **Date:** 2026-08-11 · **Naming:** the `orrery` prefix is provisional and mechanically replaceable.

Orrery is a set of Rust crates for the [Bevy](https://bevy.org) game engine (0.19) providing peer-to-peer multiplayer with QUIC + NAT hole punching, client-side prediction with rollback/reapply, and a horizontally scalable, low-latency clustered persistence service for very large persistent universes with strong spatial locality. It is a *framework*: games bring their own rules; every tunable below is a configurable parameter with the stated default.

This document records every architectural decision, its alternatives, and why they were rejected. The numbered docs in this directory expand each area; this file is their normative source. Where they conflict, this file wins.

---

## D1. Requirements (settled with the project owner, 2026-08-11)

| # | Requirement | Decision source |
|---|---|---|
| R1 | P2P networking, QUIC preferred, NAT hole punching, reuse existing crates | owner mandate |
| R2 | Client-side prediction with rollback/reapply | owner mandate |
| R3 | Remote persistence: "really really fast", horizontally scalable (clustered) | owner mandate |
| R4 | Very big universe; players interact mostly with nearby things | owner mandate |
| R5 | Simulation model: **per-entity authority + prediction** (not deterministic lockstep) | owner choice |
| R6 | Scale: **32–128 players per area** typical | owner choice |
| R7 | Persist **everything**: player state, world entities, terrain/bulk edits, event history | owner choice |
| R8 | Trust: **witness-based validation** — passive witnessing via prediction error, deterministic replay adjudication, quorum-attested persistence writes, strike/blacklist with decay ("amended witnessing") | owner choice |
| R9 | Platforms: **native only** (Windows/Linux/macOS); no WASM path required | owner choice |
| R10 | Pacing: **fast action** — 60 Hz fixed simulation tick | owner choice |
| R11 | Storage: **custom hot tier + proven durable store** | owner choice |

## D2. Simulation model: per-entity authority state replication with prediction

Every replicated entity has **exactly one authority** (a peer or a field host) at any instant — the single-writer invariant (Photon Fusion's rule). The authority simulates the entity and replicates state; other interested peers **predict** locally and **rollback/reapply** when authoritative state disagrees; entities outside the prediction set are **snapshot-interpolated**.

**Rejected: deterministic lockstep-rollback (GGPO/ggrs).** It requires every peer to hold and resimulate identical world state — incompatible with streaming/partial interest sets, late join, and peer churn; resim cost scales with world size, not interest size (SnapNet: 60 Hz absorbing 300 ms leaves ~1.1 ms/frame sim budget); and it rests on bit-perfect cross-platform float determinism that neither avian nor rapier can guarantee under SIMD/parallel execution in 2026 (Photon Quantum solved this only by rewriting math in fixed point). Deterministic replay is still used — but *scoped and offline*, in the verifiable core (D9), never as the live sync model.

**Rejected: Croquet-style synchronized computation.** Latency floor = reflector RTT, monolithic serializable VM — conflicts with a streaming world. Its cheap-relay idea survives in our relay/coordinator tier.

## D3. Transport: iroh 1.0 (P2P QUIC + hole punching + relays)

The P2P transport is **[iroh](https://github.com/n0-computer/iroh) 1.x**: QUIC connections dialed by ed25519 public key (`NodeId`), with NAT hole punching coordinated through stateless, self-hostable relays (`iroh-relay`), ~90% direct-connection success in production (~95% of bytes on direct paths), relay fallback for the rest. Connections start over the relay immediately and migrate to the direct path via QUIC multipath (iroh ≥1.0 runs on `noq`, which implements RFC 9221 datagrams, QUIC multipath, QUIC Address Discovery, and the IETF QUIC NAT-traversal draft). One connection carries **unreliable datagrams** (state replication) and **reliable streams** (control, bulk transfer) with no head-of-line blocking between them.

- The relay fleet is **self-hosted** (`iroh-relay`: public IP + DNS + ACME; ≥3 regions at launch: 2×US/EU + Asia). Relays double as the hole-punch rendezvous; the ~5–10% permanently-relayed tail (CGNAT↔CGNAT, UDP-blocked networks) is a **product requirement**, provisioned for, not an edge case.
- iroh `NodeId`s are the **transport identity everywhere**: peer↔peer, peer↔cluster. The persistence gateway and coordinator are ordinary iroh nodes at well-known public addresses (no punching needed server-side).

**Rejected:** rust-libp2p + DCUtR (~70% measured punch success, 13-month release gap); DIY punching over quinn with STUN/ICE crates (reproduces iroh at DIY quality; still needs a relay fleet); matchbox/WebRTC as primary (native webrtc-rs is a heavy ICE+DTLS+SCTP stack; only wins if WASM parity mattered — R9 says it doesn't). **Hedge:** all networking goes through the `aeronet_io` abstraction, so a raw-quinn backend (LAN, dedicated server, tests) is a drop-in; if iroh's identity layer ever chafes, `noq` is usable standalone.

## D4. Bevy netcode stack: build on aeronet → bevy_replicon → lightyear

We build **on top of** the consolidated ecosystem stack, not beside it:

- **`aeronet` 0.21** — Bevy-native session/IO abstraction. We ship the missing piece: an **iroh IO layer** (`orrery_aeronet_iroh`, upstreamable as `aeronet_iroh`; an unpublished in-repo prototype exists in the aeronet repo to mirror).
- **`bevy_replicon` 0.42** — backend-agnostic replication: registered-component diffs, per-client visibility, remote events. Its visibility API is the substrate for cell-based interest management, and its change-detection stream is what the persistence uplink consumes.
- **`lightyear` 0.29** — client-side prediction + rollback/reapply, snapshot interpolation, delta compression, priority accumulation, rooms, lag compensation, avian integration; runs on replicon since 0.27.

Orrery's genuinely novel crates are the ones **nobody has**: the iroh IO layer, the authority-lease/handoff protocol, the witnessing layer, the spatial cell system, and the entire persistence tier (every surveyed crate assumes transient in-memory state).

**Risks accepted:** lightyear's API churn (4 breaking releases in 10 months) and single-maintainer bus factor on lightyear/aeronet — mitigated by pinning versions per Orrery release and contributing authority-model hardening upstream rather than forking. **Rejected:** naia (attractive per-entity authority delegation, but UDP/WebRTC only, trails Bevy); bevy_quinnet/renet2/nevy (nothing over aeronet+iroh); building replication from scratch (replicon is exactly the right substrate).

## D5. Spatial model: one 64-bit cell ID does triple duty

The universe is partitioned by a **hierarchical uniform integer grid**, canonically aligned with `big_space`'s `GridCell` (integer cell coords + local `f32` transform; solves float precision at huge coordinates). A single sortable **`CellId(u64)`** serves as:

1. **Replication interest group** — peers subscribe to their cell + the 3×3×3 neighborhood (27 cells), mapped to replicon visibility/rooms (Unreal Replication Graph / Fortnite precedent);
2. **Storage shard key prefix** — `[cell_id][entity_id]` in a range-sharded keyspace, so "load everything near me" is a handful of contiguous range scans;
3. **Authority/handoff unit** — leases, island membership, field-host promotion, and hotspot splitting all operate on cells.

**`CellId` encoding (S2-style, parent = prefix):** offset-binary (unsigned-shifted) cell coords at the finest level (21 bits/axis → ±2²⁰ cells/axis), Morton-interleaved into 63 bits, truncated to `3·level` bits, followed by a single `1` sentinel bit then zeros. Sorted order = spatial locality; a parent cell's entire subtree is one key range. Morton for the runtime/network ID (cheapest); optional Hilbert mapping (via `lindel`) at the storage layer only if scan locality measurably matters. Games needing more range use nested `big_space` grids or a `u128` feature.

**Parameters (defaults):** interest-level cell edge ≈ AOI radius (default **128 m**); shard level = interest level −3 (one shard cell = 8×8×8 interest cells); handoff hysteresis margin = **10% of cell edge** (an entity keeps its cell/authority while inside the overlap zone — SpatialOS anti-thrash lesson).

**Nested grids (moving reference frames).** A carrier whose contents move together and interact mostly with each other (a crewed ship, an inhabited planet) is a *nested* `big_space` grid with its own `CellId` space (`GridId` carried alongside); its velocity lives at the grid root, never in its contents — so a 500 m/s cruiser crosses cells as *one* entity, and crew walk at 5 m/s in ship space under ordinary witness validation. Frame crossings are teleports when frames are stationary relative to each other (docking) and continuous **frame migrations** otherwise (EVA), logged as `FrameChange` records so replay stays closed across the basis change. Interaction requires frame coincidence; cross-frame observation sees the carrier root as one entity. Elaborated in `01-spatial-model.md` §13.

**Rejected:** S2/H3 proper (spherical geodesy is wrong for abstract 3D space; we copy S2's bit layout only); adaptive octrees as the *partition* unit (unstable group IDs, handoff storms; octrees/k-d trees — `kiddo` — are per-cell in-memory query structures only); Voronoi/VAST overlays (academically elegant, never shipped, no storage story); `bevy_spatial` (stalled at Bevy 0.16). **Risk:** `big_space` 0.12 targets Bevy 0.18 — budget a small upstream port to 0.19.

## D6. Topology: population-adaptive, per island

An **island** is one replication session: a connected set of populated cells and the peers in them (Elite Dangerous's central-servers-form-P2P-islands pattern). The coordinator (D12) forms, merges, splits, and drains islands as players move. Topology within an island adapts to live population:

| Regime | Population | Topology |
|---|---|---|
| Mesh | ≤ 8 | Full mesh over iroh; every peer connects to every peer. |
| Interest mesh | 9–32 | Partial mesh: connections only to interest-set peers (Donnybrook pattern) — each peer maintains a bounded high-rate set (default **24** entities) and receives 1–4 Hz extrapolated proxies for the rest. |
| Promoted | > 32 sustained (with hysteresis) | Coordinator spins up a **field host** — a headless Bevy instance that assumes cell-entity authority; peers keep authority over their own player entities (validated by the host). Clients experience it as just another authority peer. |

The mesh ceiling of ~32 is empirical (Donnybrook: fast games cap at 16–32 interacting players on consumer uplinks; receive bandwidth ~12·n kb/s). **Never elected-player-host with host migration** — the single most repeated failure in shipped P2P (For Honor's retreat to dedicated servers; CoD "host migration failed") — the field host is *infrastructure*, spawned/despawned by the coordinator, never a player's machine. Upload budget per peer: ≤ **1 Mbps** sustained; field hosts run in datacenters where hot-cell egress up to ~**35 Mbps** at the 128-player ceiling (≈13 Mbps at 64) is fine.

## D7. Authority: two-tier claims, cluster-arbitered leases

Per Gaffer's Networked Physics in VR + HLA ownership services:

- **Weak authority** — acquired implicitly by interaction (collisions, damage, proximity pickup attempts); propagates recursively through contact islands (physics). Monotonic `auth_seq`.
- **Strong ownership** — acquired explicitly (grab, mount, inventory, player's own character); not stealable. Monotonic `own_seq`. Ownership beats authority; higher sequence wins ties.

**Arbitration — no gameplay host.** The persistence cluster's **lease registrar** is the arbiter: authority over a persistent entity = a TTL lease row `(entity_id → holder NodeId, auth_seq, own_seq, expiry)` acquired by compare-and-swap. Peers claim **optimistically** — simulate immediately, roll the claim back only if the CAS loses (Gaffer host-confirm pattern). Lease TTL **10 s**, heartbeat **2.5 s**; expiry auto-orphans entities of crashed peers; orphans are reassigned to the nearest interacting peer (NGO-style redistribution) or **parked** in the cluster (no live authority; state served from the hot tier; optional lazy catch-up simulation on next load). Ephemeral entities (projectiles, VFX) use in-island claims only and never touch the registrar.

Cooperative handoff = negotiated divestiture (current holder acks); crash handoff = unconditional (lease expiry). Cross-cell movement keeps the holder (hysteresis, D5) and re-keys the entity's storage row on commit.

## D8. Prediction, rollback, interpolation (lightyear-configured)

Each peer is "the Overwatch server" for entities it holds authority over; Gambetta-style sequence-numbered input reconciliation applies **per entity**, not globally.

- **Fixed tick:** 60 Hz (16.67 ms). Network send rate **20 Hz** default (to 30 Hz for small islands), delta-compressed against last-acked baselines with a per-link priority accumulator (Gaffer snapshot-compression lineage).
- **Predicted set:** own player + entities under local authority + locally-initiated interactions. **Rollback window ≤ 9 ticks (~150 ms)**; beyond that, snap + reconcile. Resimulation budget: predicted-subset step must stay ≈1 ms; resim spikes amortized over ≤2 render frames (spiral-of-death guard).
- **Remote entities:** snapshot interpolation with a **2-send-interval buffer (~100 ms)** (Source's cl_interp reference); extrapolated 1–4 Hz proxies outside the high-rate interest set.
- **Hits/interactions in P2P:** shooter evaluates against its interpolated view with bounded rewind ≤ **200 ms**; the *target's* authority validates the effect; durable consequences (loot, death, XP) commit only via the intent path (D11). Above ~250 ms RTT to a target's authority, hit *presentation* prediction is disabled (Overwatch's ~220 ms precedent).
- Replicated physics state is **quantized identically on writer and reader** before use (prevents re-divergence).
- **Tick basis:** all islands share a **universe-global tick counter** (`Tick` = u64, 60 Hz) anchored to a coordinator-issued universe epoch. Island merges never re-base ticks — signed logs, RNG seeds, witness epochs, and journal records all reference absolute ticks. lightyear's internal u32 tick is bridged (offset-mapped) at the `orrery_predict` boundary.

## D9. Verifiable core: scoped determinism for replay, not for sync

A `Ruleset` (game-supplied trait implementation) defines the **verifiable core**: the subset of simulation whose outcomes touch persistent value — movement limits, combat resolution, loot rolls, crafting, trading. Requirements on core rules only:

- Fixed 60 Hz tick, inputs totally ordered per entity per tick; per-tick, per-entity seeded deterministic RNG (`rand_chacha` from `(universe_seed, entity, tick)`).
- State quantized at tick boundaries; integer/fixed-point math for discrete outcomes (damage, currency, loot — exact), `libm`-backed float math + **tolerance bands** for continuous state (position/velocity — compared within ε, default ε_pos = 1 cm, ε_vel = 1 cm/s, sustained-error window 250 ms).
- Pure step function `step(state_view, inputs) → (state', events)` — headless-runnable outside Bevy (the cluster links the same `Ruleset` for adjudication and parked-cell catch-up).

Each peer maintains a **PeerReview-style tamper-evident log** for its authoritative core entities: per-tick input records + periodic state-claim hashes, hash-chained and signed with the peer's NodeId key, streamed to the **cell-epoch witness set** (≤N peers, coordinator-seeded — D10; in the promoted regime, the field host) piggybacked on replication datagrams, with gap repair over the reliable control stream (this is cheap: sparse inputs, **one frame signature per send per link**, truncated rolling heads; full heads ride the 2 Hz state claims). Any holder of the log segment + a start snapshot can deterministically re-execute a disputed window and produce **unforgeable evidence** of deviation. Cosmetic simulation (ragdolls, particles, non-persistent physics) is unconstrained.

## D10. Witnessing: passive detection, adjudicated replay, attested writes

Amended witnessing (owner-approved). Prediction *is* the witness:

1. **Continuous cheap checks** (every interested peer, free): stateless invariant validators on received state (speed/acceleration caps, teleport detection, rate limits, impossible-value checks) + the reconciliation-error monitor from D8 + **continuous re-execution of streamed input logs by cell-epoch witness-set members** for core entities outside the observer's predicted set (kinematic movement replay at ~µs/tick cost — this is the witness signal for non-interacting remote players, which prediction alone does not cover).
2. **Discrepancy escalation:** sustained tolerance-band violation or invariant breach → the observer requests the disputed window's signed input-log segment + t₀ claim, re-executes it in the verifiable core, and on mismatch files a **discrepancy report** (evidence bundle: log segment, claimed vs. computed hashes) to the cluster's adjudication service.
3. **Adjudication:** the cluster re-executes the window itself with the same `Ruleset` (deterministic; the evidence is self-verifying). Verdict → in-session **authority correction broadcast** (quorum state becomes authoritative; peers reconcile) + durable **write refusal/annulment** + a **strike** on the account. Failed evidence is split: **provable fabrication** (a subject signature the reporter attested as verified fails verification) strikes the *reporter*; **adjudicator-side failures** (unavailable ruleset version, retention miss, oversize window) are merely *unadjudicable* — never a strike, rate-limited instead.
4. **Attested intents:** persistence-critical operations (D11) carry **K-of-N witness co-signatures** (default K=3 of N≥5) from a witness set **seeded by the coordinator per cell-epoch** (committed through the gateway to FDB; never self-chosen — anti-collusion), drawn from the entity's interest set (they already have the context), **excluding all parties to the intent** (matched on accounts and every NodeId bound to them; if exclusion leaves < N eligible candidates, the low-population fallback applies). The K *required* co-signers are a **deterministic per-intent subset** derived from the epoch seed and intent id (no attestation shopping); reseeds are rate-limited (min epoch interval 10 s; churn-triggered reseeds only on gateway-observed organic disconnects, with per-account cooldowns); the epoch announcement publishes a commitment (hash) to the seed key, revealed at epoch end for retroactive verifiability. Low-population fallback (< N candidates): field-host witness, or **provisional commit** — flagged rows finalized after cluster-side spot replay.
5. **Strikes & identity:** strikes decay (default half-life 14 days); thresholds: temporary quarantine (writes require full cluster-side validation) → cooldown → ban. Identities are accounts (D12) binding NodeIds, with acquisition cost (Sybil resistance). Tolerance bands + "multiple rollbacks" thresholds keep honest players with packet loss / platform drift out of the strike pipeline.

**Documented limits (accepted):** aimbot-class cheats (legal inputs) and fog-of-war/ESP leaks (peers necessarily receive nearby state) are *not* preventable in P2P — mitigations are server-side-secret state (unopened loot rolls, out-of-interest players stay cluster-side, revealed late) and telemetry/statistical detection. **Peer IP exposure and targeted DoS** (booter attacks as a gameplay weapon) are likewise inherent to P2P — mitigations: optional relay-only privacy mode (latency cost), reconnection grace before orphan redistribution of player-bound entities, and telemetry correlating disconnects with in-game adversaries. GTA Online is the cautionary tale for validating too late; Diablo II closed realms for why the cluster is the sole writer of durable truth.

## D11. Persistence: in-memory cell actors + journal, FoundationDB system of record

**"Really really fast" = don't make the game wait for a database.** Two write classes:

- **Bulk state** (positions, health, world-entity state, terrain deltas): the authoritative peer uplinks replicon change-detection diffs (~1–4 Hz per entity, priority-scheduled) over its iroh connection to the **persistence gateway**, which routes to the **cell actor** — a single-writer, in-memory actor owning that cell's hot state (placement: rendezvous hashing over shard cells; hotspot cells split/relocate on player-count telemetry). The actor applies the diff, appends to a **per-node segmented append-only journal** (group commit, fsync cadence ~2 ms), and acks. **Targets: journal commit < 2 ms (server-internal, adaptive group commit — fsync immediately when the disk is idle); client-observed ack p99 < 5 ms in-region.** Acks are **epoch-fenced**: an actor may issue durable acks only while its shard-ownership epoch is confirmed fresh within a bounded staleness window (split-brain guard; journal records carry the epoch).
- **Critical operations** (item/currency transfers, trades, loot grants, progression, structure placement — `Ruleset`-classified): signed, witness-attested **intents**. The gateway verifies signatures + attestations, runs `Ruleset` validation against hot state, and executes a **FoundationDB serializable optimistic transaction** (read-check-write across both parties' rows). This is the anti-duplication mechanism — no locks, no LWT contention cliffs, conflicts just retry. **Commit p99 target: < 10 ms in-region** (FDB commit 1.5–2.5 ms at <75% load).

**Durable tier: FoundationDB 7.3.x** (7.4 tracked as upgrade candidate) (strictly serializable transactions; 0.1–1 ms reads; linear scaling demonstrated to 8.2 M ops/s; mature `foundationdb-rs` 0.11 binding; 3–5 node clusters manageable via fdb-kubernetes-operator or systemd). FDB limits (5 s txn, 10 KB keys, 100 KB values, 10 MB txn) fit the checkpoint pattern; oversized blobs (terrain chunks) shard across rows.

- **Checkpointing:** cell actors write copy-on-update checkpoints to FDB on a **20 s jittered cadence** (immediate on cell quiesce), Cornell-VLDB style. Keyspace: `world/{cell_id}/{entity_id} → components` (range-sharded, Morton/Hilbert prefix = D5 locality); `player/{account_id} → …`; `ledger/…` for balances/items; `lease/{entity_id}`; `chunk/{cell_id}/{n}` for terrain.
- **Durability windows:** intents: **RPO 0** (synchronous FDB). Bulk: journal on local NVMe → RPO ≈ 0 if disk survives, ≤ checkpoint cadence if node is lost; optional **chain-replicated journal** (1 async follower) → RPO ≤ ~100 ms. Default deployment: chain replication on.
- **Event history (R7):** the journal *is* the event source — a tailer compacts it to an archive (object storage, Parquet) with configurable retention; supports griefing rollback (inverse-op replay by cell/actor/time-range), offline-progress computation, desync forensics, analytics.
- **Terrain/bulk edits:** chunk-oriented, cell-aligned; deltas-vs-base in the journal, periodically compacted into chunk snapshot rows (≤100 KB shards, Minecraft sparse-elision precedent). Every `TerrainDelta` is **attributed to and fenced by the editing player's lease** and invariant-checked at the cell actor (reach/rate/tool); destructive or high-value edits route through intents. Live edits replicate P2P on the reliable per-cell stream, ordered by `(cell, tick)`.
- **At-rest schema versioning:** component bags carry a per-component schema version; `Ruleset`-registered migrations run lazily on checkpoint-load/area-read plus an optional background sweep; journal and archive records carry their encoding version so replay, catch-up, and griefing rollback can decode history. Migrations must span ≥2 adjacent versions.
- **World seeding & IDs:** an offline import tool (built on the persistd harness) bulk-writes designed content into `world/`/`chunk/` rows, mints `PersistId`s, and records a content-version row for later diff/patch deploys. Live minting: intent commit receipts carry cluster-minted `PersistId`s; peers also hold **journaled block grants** (contiguous id ranges leased per session, usable offline). `universe_seed` is generated once per universe and held in the secret store (it is security-relevant per D9).
- **Bulk-path validation:** cell actors run the stateless `Ruleset` invariant validators on inbound bulk diffs — mandatory for entities in cells with fewer than N witness candidates (the solo-cell exposure), sampled elsewhere — rejecting or flagging violations. CRDTs deliberately absent from the hot path (single writer per cell); noted as a future option for offline build modes only.
- **Area load:** client enters an area → gateway serves the 27-cell neighborhood via FDB range scans + live actor deltas, streamed nearest-first; **< 50 ms to first page-in** target.

**Rejected:** ScyllaDB as primary (best raw write throughput, superb Rust driver — but LWTs are the wrong trade-safety tool; runner-up if sustained writes exceed a modest FDB cluster); building a general replicated store on openraft (pre-1.0, chaos-testing incomplete; FDB's own lesson: the simulator is the hard part — our custom layer stays *thin and single-purpose*); Redis/Valkey/Dragonfly as record store (async replication loses acked writes); Aerospike (CE caps kill "very large"); TiKV (client officially non-production); sled (stalled). Local engine for journal/staging: **fjall 3.x** (active, pure Rust) or raw segmented logs; not RocksDB unless profiling demands it.

## D12. Backend service inventory (what we operate)

Five services, all Rust, all speaking iroh QUIC externally (tonic/gRPC internally where boring is better):

| Service | Crate | Role |
|---|---|---|
| **Identity** | `orrery_identity` | Accounts, NodeId binding, session tokens, strike/reputation ledger, bans. |
| **Relay fleet** | `iroh-relay` (ops config) | Hole-punch rendezvous + relay fallback; ≥3 regions; stateless. |
| **Coordinator** | `orrery_coordinator` | Coarse presence tracking; island form/merge/split/drain; NodeId handout; witness-set seeding per cell-epoch; field-host orchestration (promotion at >32 sustained); the Elite `edServer` role. |
| **Persistence cluster** | `orrery_persistd` | Gateway, cell actors, journal, FDB checkpointing, lease registrar, intent validation, adjudication executor (retains the last **3** ruleset builds as version-keyed workers so evidence pinned to older rules stays adjudicable across hotfixes). Ships as a library harness — games link their `Ruleset` into their own `persistd` binary. |
| **Field hosts** | `orrery_field_host` | Headless Bevy instances for promoted cells and low-pop witness fallback; elastically scheduled by the coordinator. |

Plus telemetry (OpenTelemetry throughout; audit/anti-cheat pipeline consuming discrepancy reports and state-hash cross-checks — ClickHouse or similar, ops choice). **Nothing else**: no game-simulation servers exist until a cell exceeds the mesh ceiling. Netsplit posture: P2P sim continues without the cluster (intents queue, durable commits pause); no cluster = degraded, not dead.

## D13. Physics & determinism posture

**avian3d** for presentation/gameplay physics (Bevy-native, lightyear integration). Verifiable-core movement/combat uses framework-provided deterministic kinematic character movement + integer combat math (D9) — *not* the full physics engine. Contested physics objects (crates, vehicles) replicate under weak-authority contact-island propagation with quantize-both-sides; their persistence writes are bulk-class (not witness-attested) unless the `Ruleset` says otherwise. rapier documented as the alternative. Cross-platform bit determinism of full physics is explicitly **not** assumed anywhere.

## D14. Pinned versions (Aug 2026)

Bevy 0.19 · lightyear 0.29 · bevy_replicon 0.42 · aeronet 0.21 · iroh 1.0.x (noq) · avian3d 0.7 · big_space 0.12 (needs 0.19 port — tracked risk) · FoundationDB 7.3.x / foundationdb-rs 0.11 · fjall 3.x · kiddo 6.x · rand_chacha 0.9.

## D15. Crate set

Engine-agnostic core (no Bevy dependency): `orrery_protocol`, `orrery_core`. Server binaries are Bevy-free except `orrery_field_host`.

| Crate | Kind | Responsibility |
|---|---|---|
| `orrery_protocol` | lib | Wire & data types: `CellId`, intents, leases, attestations, evidence bundles, log frames/state claims; canonical scalars (`Tick` = u64 universe ticks, `PersistId` = u64 cluster-minted, `CellId` = NonZeroU64, `RulesetId` = version + build digest); postcard/bitcode encoding; versioning. |
| `orrery_aeronet_iroh` | lib | iroh IO layer for `aeronet_io` (upstream candidate). |
| `orrery_net` | Bevy plugin | Session lifecycle: coordinator client, island membership, peer connect/disconnect, channel policy (datagrams=state, streams=control/bulk), relay-path telemetry. |
| `orrery_spatial` | Bevy plugin | `CellId` math + `big_space` integration; AOI subscription (27-cell), replicon visibility mapping, interest-set selection, hysteresis, proxy extrapolation. |
| `orrery_authority` | Bevy plugin | Weak/strong claims, sequence numbers, optimistic lease client, handoff, orphan recovery, contact-island propagation. |
| `orrery_predict` | Bevy plugin | lightyear configuration for per-entity authority; reconciliation-error monitor (witness signal); rollback budget guard. |
| `orrery_core` | lib | Verifiable core: `Ruleset` trait, fixed-tick executor, deterministic RNG, quantization, tolerance comparators, signed hash-chained input logs, replay harness (headless). |
| `orrery_witness` | Bevy plugin | Invariant validators, discrepancy detection/reports, attestation co-signing, evidence assembly. |
| `orrery_persist_client` | Bevy plugin | Gateway session, area load/subscribe, diff uplink scheduler, intent submission + offline queue, prediction of intent outcomes. |
| `orrery_persistd` | lib+bin harness | Cell actors, journal, FDB checkpoint/restore, lease registrar, gateway, intent validation, adjudication executor, hotspot splitting. |
| `orrery_coordinator` | bin | Presence, islands, witness seeding, promotion, field-host scheduling. |
| `orrery_identity` | bin | Accounts, tokens, strikes, bans. |
| `orrery_field_host` | bin | Headless Bevy authority host (promoted cells, witness fallback, parked-cell catch-up execution). |

Dependency spine: `protocol` ← everything; `core` ← {witness, persistd, field_host, game}; client plugins compose as a `OrreryClientPlugins` group.

## D16. Parameter reference (defaults)

| Parameter | Default | Parameter | Default |
|---|---|---|---|
| Sim tick | 60 Hz | Lease TTL / heartbeat | 10 s / 2.5 s |
| Send rate | 20 Hz (≤30) | Journal fsync group | ~2 ms |
| Rollback window | 9 ticks (150 ms) | Checkpoint cadence | 20 s jittered |
| Interp buffer | 100 ms | Bulk ack p99 (client / journal) | < 5 ms / < 2 ms |
| High-rate interest set | 24 entities | Intent commit p99 | < 10 ms |
| Proxy rate | 1–4 Hz | Area first-page-in | < 50 ms |
| Hit rewind cap | 200 ms | Witness quorum | K=3 of N≥5 |
| Cell edge (interest) | 128 m | Strike half-life | 14 days |
| Shard cell | 8×8×8 interest cells | Peer upload budget | ≤1 Mbps |
| Hysteresis margin | 10% cell edge | Mesh→promotion threshold | >32 sustained |
| ε_pos / ε_vel / window | 1 cm / 1 cm·s⁻¹ / 250 ms | Adjudication window max | 3 s (180 ticks) |
| Epoch reseed min interval | 10 s | Ruleset builds retained (adjudication) | 3 |
| Hot-cell egress (promoted) | ≤ 35 Mbps | Witness-log fan-out | witness set only (≤ 7 links) |

## D17. Known risks & open questions

1. **Upstream churn/bus factor:** lightyear, aeronet, big_space are single-maintainer; lightyear authority transfer self-described "in flux." Mitigation: version pinning, upstream contributions, replicon-direct fallback (own prediction layer) documented as plan B.
2. **noq fork drift** from quinn; iroh relay economics at scale (self-hosted fleet sizing for the relayed tail).
3. **Witness tuning:** tolerance bands vs. false-positive strikes needs empirical calibration (packet loss, platform drift); the strike pipeline must launch in shadow mode (telemetry-only) first.
4. **FDB ops learning curve** for a small team; hotspot pre-splitting under crowd events (FDB issue #11510 pattern) needs load-shedding design.
5. **Field-host cost model:** promotion threshold vs. infrastructure spend is a live-ops dial; worst case (every cell hot) converges to client-server economics by design.
6. **Open:** cross-island consistency for fast travelers (island merge latency); parked-cell catch-up semantics (lazy vs. scheduled); economy-wide invariant auditing cadence; mod/plugin distribution of `Ruleset` to cluster (games recompile `persistd` — acceptable?).
7. **Open — D18 (terrain↔entity promotion):** the lazy terrain↔entity promotion specification ([08-persistence.md](08-persistence.md) §10.1; [06-verifiable-core.md](06-verifiable-core.md) §6/§9; [05-prediction-rollback.md](05-prediction-rollback.md) §7.2; [03-replication.md](03-replication.md) §9.7) is written but **not yet ratified as a decision**. If adopted, anchor it as **D18** — it amends D9 (adds the `TerrainPromotion` record source to the tamper-evident log) and D11 (adds journal record kinds, the `section_pin/` keyspace family, and id-stability minting); the escrowed-release variant (08 §10.1.7) would touch D7 (it is deliberately **excluded** from the base mechanism). Until then it is a **non-normative proposal** and the README status line is unchanged.

## Document map

| Doc | Covers |
|---|---|
| `00-overview.md` | Goals, constraints, system diagram, subsystem tour, glossary |
| `01-spatial-model.md` | D5 — grid, `CellId`, big_space, AOI, hysteresis, hotspots, nested grids (moving reference frames) |
| `02-networking.md` | D3, D6 — iroh, relays, islands, topology regimes, channels, budgets |
| `03-replication.md` | D4, D8(bandwidth) — replicon/lightyear stack, interest sets, delta/priority |
| `04-authority.md` | D7 — claims, leases, handoff, orphans, promotion interplay |
| `05-prediction-rollback.md` | D8 — timelines, prediction sets, reconciliation, interpolation, hits |
| `06-verifiable-core.md` | D9 — Ruleset, determinism scoping, logs, replay harness |
| `07-witnessing.md` | D10 — threat model, protocol, adjudication, strikes, limits |
| `08-persistence.md` | D11 — cell actors, journal, FDB schema, intents, terrain, event archive |
| `09-services-and-ops.md` | D12 — service inventory, deployment, scaling, failure modes, telemetry |
| `10-crates.md` | D15 — workspace, per-crate API sketches, dependency graph |
| `11-roadmap.md` | Build phases, milestones, D17 risks |
| `references.md` | Bibliography |
