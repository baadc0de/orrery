# ADR-0012: Backend service inventory (what we operate)

**Status:** Accepted · **Date:** 2026-08-11 · **Decision:** D12

This decision is normative. See the [ADR index](../DECISIONS.md) for precedence, scope, and the complete decision set.

Five services, all Rust, all speaking iroh QUIC externally (tonic/gRPC internally where boring is better):

| Service | Crate | Role |
|---|---|---|
| **Identity** | `orrery_identity` | Accounts, NodeId binding, session tokens, strike/reputation ledger, bans. |
| **Relay fleet** | `iroh-relay` (ops config) | Hole-punch rendezvous + relay fallback; ≥3 regions; stateless. |
| **Coordinator** | `orrery_coordinator` | Coarse presence tracking; island form/merge/split/drain; NodeId handout; witness-set seeding per cell-epoch; field-host orchestration (promotion at >32 sustained); the Elite `edServer` role. |
| **Persistence cluster** | `orrery_persistd` | Gateway, cell actors, journal, FDB checkpointing, lease registrar, intent validation, adjudication executor (retains the last **3** ruleset builds as version-keyed workers so evidence pinned to older rules stays adjudicable across hotfixes). Ships as a library harness — games link their `Ruleset` into their own `persistd` binary. |
| **Field hosts** | `orrery_field_host` | Headless Bevy instances for promoted cells and low-pop witness fallback; elastically scheduled by the coordinator. |

Plus telemetry (OpenTelemetry throughout; audit/anti-cheat pipeline consuming discrepancy reports and state-hash cross-checks — ClickHouse or similar, ops choice). **Nothing else**: no game-simulation servers exist until a cell exceeds the mesh ceiling. Netsplit posture: P2P sim continues without the cluster (intents queue, durable commits pause); no cluster = degraded, not dead.

