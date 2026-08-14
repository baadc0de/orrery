# ADR-0008: Prediction, rollback, interpolation (lightyear-configured)

**Status:** Accepted · **Date:** 2026-08-11 · **Decision:** D8

This decision is normative. See the [ADR index](../DECISIONS.md) for precedence, scope, and the complete decision set.

Each peer is "the Overwatch server" for entities it holds authority over; Gambetta-style sequence-numbered input reconciliation applies **per entity**, not globally.

- **Fixed tick:** 60 Hz (16.67 ms). Network send rate **20 Hz** default (to 30 Hz for small islands), delta-compressed against last-acked baselines with a per-link priority accumulator (Gaffer snapshot-compression lineage).
- **Predicted set:** own player + entities under local authority + locally-initiated interactions. **Rollback window ≤ 9 ticks (~150 ms)**; beyond that, snap + reconcile. Resimulation budget: predicted-subset step must stay ≈1 ms; resim spikes amortized over ≤2 render frames (spiral-of-death guard).
- **Remote entities:** snapshot interpolation with a **2-send-interval buffer (~100 ms)** (Source's cl_interp reference); extrapolated 1–4 Hz proxies outside the high-rate interest set.
- **Hits/interactions in P2P:** shooter evaluates against its interpolated view with bounded rewind ≤ **200 ms**; the *target's* authority validates the effect; durable consequences (loot, death, XP) commit only via the intent path ([D11](0011-persistence.md)). Above ~250 ms RTT to a target's authority, hit *presentation* prediction is disabled (Overwatch's ~220 ms precedent).
- Replicated physics state is **quantized identically on writer and reader** before use (prevents re-divergence).
- **Tick basis:** all islands share a **universe-global tick counter** (`Tick` = u64, 60 Hz) anchored to a coordinator-issued universe epoch. Island merges never re-base ticks — signed logs, RNG seeds, witness epochs, and journal records all reference absolute ticks. lightyear's internal u32 tick is bridged (offset-mapped) at the `orrery_predict` boundary.

