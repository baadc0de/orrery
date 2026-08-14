# ADR-0017: Known risks & open questions

**Status:** Accepted · **Date:** 2026-08-11 · **Decision:** D17

This decision is normative. See the [ADR index](../DECISIONS.md) for precedence, scope, and the complete decision set.

1. **Upstream churn/bus factor:** lightyear, aeronet, big_space are single-maintainer; lightyear authority transfer self-described "in flux." Mitigation: version pinning, upstream contributions, replicon-direct fallback (own prediction layer) documented as plan B.
2. **noq fork drift** from quinn; iroh relay economics at scale (self-hosted fleet sizing for the relayed tail).
3. **Witness tuning:** tolerance bands vs. false-positive strikes needs empirical calibration (packet loss, platform drift); the strike pipeline must launch in shadow mode (telemetry-only) first.
4. **FDB ops learning curve** for a small team; hotspot pre-splitting under crowd events (FDB issue #11510 pattern) needs load-shedding design.
5. **Field-host cost model:** promotion threshold vs. infrastructure spend is a live-ops dial; worst case (every cell hot) converges to client-server economics by design.
6. **Open:** cross-island consistency for fast travelers (island merge latency); parked-cell catch-up semantics (lazy vs. scheduled); economy-wide invariant auditing cadence; mod/plugin distribution of `Ruleset` to cluster (games recompile `persistd` — acceptable?).
7. **Open — D18 (terrain↔entity promotion):** the lazy terrain↔entity promotion specification ([08-persistence.md](../08-persistence.md) §10.1; [06-verifiable-core.md](../06-verifiable-core.md) §6/§9; [05-prediction-rollback.md](../05-prediction-rollback.md) §7.2; [03-replication.md](../03-replication.md) §9.7) is written but **not yet ratified as a decision**. If adopted, anchor it as **D18** — it amends [D9](0009-verifiable-core.md) (adds the `TerrainPromotion` record source to the tamper-evident log) and [D11](0011-persistence.md) (adds journal record kinds, the `section_pin/` keyspace family, and id-stability minting); the escrowed-release variant (08 §10.1.7) would touch [D7](0007-authority-and-leases.md) (it is deliberately **excluded** from the base mechanism). Until then it is a **non-normative proposal** and the README status line is unchanged.
