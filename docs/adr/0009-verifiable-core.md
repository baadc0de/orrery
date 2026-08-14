# ADR-0009: Verifiable core: scoped determinism for replay, not for sync

**Status:** Accepted · **Date:** 2026-08-11 · **Decision:** D9

This decision is normative. See the [ADR index](../DECISIONS.md) for precedence, scope, and the complete decision set.

A `Ruleset` (game-supplied trait implementation) defines the **verifiable core**: the subset of simulation whose outcomes touch persistent value — movement limits, combat resolution, loot rolls, crafting, trading. Requirements on core rules only:

- Fixed 60 Hz tick, inputs totally ordered per entity per tick; per-tick, per-entity seeded deterministic RNG (`rand_chacha` from `(universe_seed, entity, tick)`).
- State quantized at tick boundaries; integer/fixed-point math for discrete outcomes (damage, currency, loot — exact), `libm`-backed float math + **tolerance bands** for continuous state (position/velocity — compared within ε, default ε_pos = 1 cm, ε_vel = 1 cm/s, sustained-error window 250 ms).
- Pure step function `step(state_view, inputs) → (state', events)` — headless-runnable outside Bevy (the cluster links the same `Ruleset` for adjudication and parked-cell catch-up).

Each peer maintains a **PeerReview-style tamper-evident log** for its authoritative core entities: per-tick input records + periodic state-claim hashes, hash-chained and signed with the peer's NodeId key, streamed to the **cell-epoch witness set** (≤N peers, coordinator-seeded — [D10](0010-witnessing.md); in the promoted regime, the field host) piggybacked on replication datagrams, with gap repair over the reliable control stream (this is cheap: sparse inputs, **one frame signature per send per link**, truncated rolling heads; full heads ride the 2 Hz state claims). Any holder of the log segment + a start snapshot can deterministically re-execute a disputed window and produce **unforgeable evidence** of deviation. Cosmetic simulation (ragdolls, particles, non-persistent physics) is unconstrained.

