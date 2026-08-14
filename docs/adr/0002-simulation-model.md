# ADR-0002: Simulation model: per-entity authority state replication with prediction

**Status:** Accepted · **Date:** 2026-08-11 · **Decision:** D2

This decision is normative. See the [ADR index](../DECISIONS.md) for precedence, scope, and the complete decision set.

Every replicated entity has **exactly one authority** (a peer or a field host) at any instant — the single-writer invariant (Photon Fusion's rule). The authority simulates the entity and replicates state; other interested peers **predict** locally and **rollback/reapply** when authoritative state disagrees; entities outside the prediction set are **snapshot-interpolated**.

**Rejected: deterministic lockstep-rollback (GGPO/ggrs).** It requires every peer to hold and resimulate identical world state — incompatible with streaming/partial interest sets, late join, and peer churn; resim cost scales with world size, not interest size (SnapNet: 60 Hz absorbing 300 ms leaves ~1.1 ms/frame sim budget); and it rests on bit-perfect cross-platform float determinism that neither avian nor rapier can guarantee under SIMD/parallel execution in 2026 (Photon Quantum solved this only by rewriting math in fixed point). Deterministic replay is still used — but *scoped and offline*, in the verifiable core ([D9](0009-verifiable-core.md)), never as the live sync model.

**Rejected: Croquet-style synchronized computation.** Latency floor = reflector RTT, monolithic serializable VM — conflicts with a streaming world. Its cheap-relay idea survives in our relay/coordinator tier.

