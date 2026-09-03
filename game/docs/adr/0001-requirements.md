# Game ADR-0001: Requirements (settled with the project owner, 2026-09-03)

**Status:** Accepted · **Date:** 2026-09-03 · **Decision:** GD1

This decision is normative for the *Mothership* game project. See the [game ADR index](../DECISIONS.md). It is distinct from Orrery's trail; Orrery ADRs that change to satisfy it cite the `G` numbers below.

The full requirement text, with per-section engineering consequences, is [00-requirements.md](../00-requirements.md). This record fixes the decision set and its numbering; the document is the authoritative wording.

| # | Decision | Key rows |
|---|---|---|
| G1 | Persistent-universe first-person game: gunplay, mech piloting, spacecraft, gathering, crafting, trade, PvP, PvE | G1.1–G1.8 |
| G2 | One solar system per season, dozens of static bodies; mothership jumps offline between seasons; three craft classes (jump-capable, escorts as ship consumables, flip-and-burn) | G2.1–G2.4 |
| G3 | Temporary death by cloning with upkeep; full loot; opt-in insurance issuing procurement, delivery and retrieval orders; clone printers as player-chosen respawn points; wrecks are salvage | G3.1–G3.8 |
| G4 | Mothership as seamless hub with apartments and shared infrastructure; ships owned only by organizations or the mothership; NPC-driven surface building; open PvP with NPC security and renown; consumable vehicles; capture by clearing then NPC conversion; recall teleport; separate macro simulation service | G4.1–G4.21a |
| G5 | Contract economy: players, mothership, insurance and NPC factions issue; player pricing with fair-value suggestion; player first refusal; player-taken is micro, NPC-taken is macro; any mix of money, work, goods | G5.1–G5.8 |
| G6 | Progression is standing plus possessions; renown is a weighted sum of mothership organization standings with per-gate floors; items grant abilities; ships are items owned by organizations; clonable-crew core loop; owner-defined loadouts modified by on-body items | G6.1–G6.9 |
| G7 | Every player in exactly one organization, training organization by default; one leader who kicks and passes leadership unilaterally; contract-only crews; NPC pilot fallback and discretionary whole-ship emergency teleport; full friendly fire; ad-hoc squads; identity reveal only on death; rivalry as friend-or-foe indicator; defection as strategy | G7.1–G7.12 |
| G8 | Organized spacefaring NPC factions, deeper is meaner, active fleet tactics; fauna as hazard and harvest; clandestine missions; scripted encounters; handcrafted plus generated season quests (handcrafted set ends the season); clone charges as a resource; macro-only when unobserved; the mothership faction is one of many in mechanism, distinct in configuration | G8.1–G8.11 |
| G9 | Seasons of 1–3 months wall-clock; real-time clock with accelerated day–night; teleport is the only time compression; flip-and-burn transit is a spatial anchor for intercept missions | G9.1–G9.4 |

## Consequences for Orrery

The engineering consequences listed under each section of the requirements document are inputs to Orrery's ADR trail, not decisions of this record. The ones expected to need new Orrery ADRs first:

- a **macro simulation service** and its materialise/fold contract with the 60 Hz layer (G4.20, G8);
- **viewer-dependent replication** for hidden identity, decoys and unscouted trajectories (G7.8, G8.2, G9);
- **mothership-scale interest management**, one grid at whole-server population (G4);
- **item-to-entity and entity-to-item transitions** under prediction for consumable vehicles, packed ships and captured structures (G4.15, G4.16, G4.21);
- **ledger-equivalent macro and micro contract fulfilment** (G5).

## Open items

Tracked at the end of [00-requirements.md](../00-requirements.md#open-items).
