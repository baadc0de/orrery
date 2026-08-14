# ADR-0006: Topology: population-adaptive, per island

**Status:** Accepted · **Date:** 2026-08-11 · **Decision:** D6

This decision is normative. See the [ADR index](../DECISIONS.md) for precedence, scope, and the complete decision set.

An **island** is one replication session: a connected set of populated cells and the peers in them (Elite Dangerous's central-servers-form-P2P-islands pattern). The coordinator ([D12](0012-backend-services.md)) forms, merges, splits, and drains islands as players move. Topology within an island adapts to live population:

| Regime | Population | Topology |
|---|---|---|
| Mesh | ≤ 8 | Full mesh over iroh; every peer connects to every peer. |
| Interest mesh | 9–32 | Partial mesh: connections only to interest-set peers (Donnybrook pattern) — each peer maintains a bounded high-rate set (default **24** entities) and receives 1–4 Hz extrapolated proxies for the rest. |
| Promoted | > 32 sustained (with hysteresis) | Coordinator spins up a **field host** — a headless Bevy instance that assumes cell-entity authority; peers keep authority over their own player entities (validated by the host). Clients experience it as just another authority peer. |

The mesh ceiling of ~32 is empirical (Donnybrook: fast games cap at 16–32 interacting players on consumer uplinks; receive bandwidth ~12·n kb/s). **Never elected-player-host with host migration** — the single most repeated failure in shipped P2P (For Honor's retreat to dedicated servers; CoD "host migration failed") — the field host is *infrastructure*, spawned/despawned by the coordinator, never a player's machine. Upload budget per peer: ≤ **1 Mbps** sustained; field hosts run in datacenters where hot-cell egress up to ~**35 Mbps** at the 128-player ceiling (≈13 Mbps at 64) is fine.

