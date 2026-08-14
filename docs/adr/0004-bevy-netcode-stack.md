# ADR-0004: Bevy netcode stack: build on aeronet → bevy_replicon → lightyear

**Status:** Accepted · **Date:** 2026-08-11 · **Decision:** D4

This decision is normative. See the [ADR index](../DECISIONS.md) for precedence, scope, and the complete decision set.

We build **on top of** the consolidated ecosystem stack, not beside it:

- **`aeronet` 0.21** — Bevy-native session/IO abstraction. We ship the missing piece: an **iroh IO layer** (`orrery_aeronet_iroh`, upstreamable as `aeronet_iroh`; an unpublished in-repo prototype exists in the aeronet repo to mirror).
- **`bevy_replicon` 0.42** — backend-agnostic replication: registered-component diffs, per-client visibility, remote events. Its visibility API is the substrate for cell-based interest management, and its change-detection stream is what the persistence uplink consumes.
- **`lightyear` 0.29** — client-side prediction + rollback/reapply, snapshot interpolation, delta compression, priority accumulation, rooms, lag compensation, avian integration; runs on replicon since 0.27.

Orrery's genuinely novel crates are the ones **nobody has**: the iroh IO layer, the authority-lease/handoff protocol, the witnessing layer, the spatial cell system, and the entire persistence tier (every surveyed crate assumes transient in-memory state).

**Risks accepted:** lightyear's API churn (4 breaking releases in 10 months) and single-maintainer bus factor on lightyear/aeronet — mitigated by pinning versions per Orrery release and contributing authority-model hardening upstream rather than forking. **Rejected:** naia (attractive per-entity authority delegation, but UDP/WebRTC only, trails Bevy); bevy_quinnet/renet2/nevy (nothing over aeronet+iroh); building replication from scratch (replicon is exactly the right substrate).

