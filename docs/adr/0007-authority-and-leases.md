# ADR-0007: Authority: two-tier claims, cluster-arbitered leases

**Status:** Accepted; successor locality qualified by
[ADR-0026](0026-sibling-gateways.md) (*proposed*) ·
**Date:** 2026-08-11 · **Decision:** D7

This decision is normative. See the [ADR index](../DECISIONS.md) for precedence, scope, and the complete decision set.

Per Gaffer's Networked Physics in VR + HLA ownership services:

- **Weak authority** — acquired implicitly by interaction (collisions, damage, proximity pickup attempts); propagates recursively through contact islands (physics). Monotonic `auth_seq`.
- **Strong ownership** — acquired explicitly (grab, mount, inventory, player's own character); not stealable. Monotonic `own_seq`. Ownership beats authority; higher sequence wins ties.

**Arbitration — no gameplay host.** The persistence cluster's **lease registrar** is the arbiter: authority over a persistent entity = a TTL lease row `(entity_id → holder NodeId, auth_seq, own_seq, expiry)` acquired by compare-and-swap. Peers claim **optimistically** — simulate immediately, roll the claim back only if the CAS loses (Gaffer host-confirm pattern). Lease TTL **10 s**, heartbeat **2.5 s**; expiry auto-orphans entities of crashed peers; orphans are reassigned to the nearest interacting peer (NGO-style redistribution) or **parked** in the cluster (no live authority; state served from the hot tier; optional lazy catch-up simulation on next load). Ephemeral entities (projectiles, VFX) use in-island claims only and never touch the registrar.

Cooperative handoff = negotiated divestiture (current holder acks); crash handoff = unconditional (lease expiry). Cross-cell movement keeps the holder (hysteresis, [D5](0005-spatial-model.md)) and re-keys the entity's storage row on commit.

