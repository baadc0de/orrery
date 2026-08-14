# ADR-0001: Requirements (settled with the project owner, 2026-08-11)

**Status:** Accepted · **Date:** 2026-08-11 · **Decision:** D1

This decision is normative. See the [ADR index](../DECISIONS.md) for precedence, scope, and the complete decision set.

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

