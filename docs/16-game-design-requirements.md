# 16 — Game design requirements

**Status:** Draft · **Owner:** project owner · **Date:** 2026-09-03

This document records the game-design requirements that dictate how Orrery is developed from here on. It is normative in the same sense as [ADR-0001](adr/0001-requirements.md): every engineering decision must be traceable to a requirement here or to an ADR that cites one. Requirements are numbered `G<n>` so they can be cited alongside the `R<n>` engineering requirements. Once settled they are promoted to an ADR and indexed in [DECISIONS.md](DECISIONS.md).

## G1 — Genre

| # | Requirement | Source |
|---|---|---|
| G1 | **Persistent-universe first-person game.** One shared, continuously-running universe (no sessions, no instanced matches as the primary mode). Player-controlled first-person avatar as the base unit of play. | owner mandate |
| G1.1 | **Gunplay.** First-person infantry combat with ranged weapons. | owner mandate |
| G1.2 | **Mech piloting.** Players can board and pilot mechs: large ground vehicles with their own hull, weapons and movement model. | owner mandate |
| G1.3 | **Spacecraft.** Players can board and pilot spacecraft; play spans surface and space. | owner mandate |
| G1.4 | **Resource gathering.** Raw materials are extracted from the world. | owner mandate |
| G1.5 | **Crafting.** Gathered materials are turned into items, vehicles and structures by players. | owner mandate |
| G1.6 | **Trade.** Players exchange goods with each other; an economy exists between them. | owner mandate |
| G1.7 | **PvP.** Players can fight and harm each other. | owner mandate |
| G1.8 | **PvE.** The world contains non-player threats and content. | owner mandate |

### Engineering consequences of G1

Stated here so the pull on the architecture is visible; each line becomes a design question below or a decision in an ADR.

- **Three movement scales in one authority model.** Avatar (m/s), mech (tens of m/s, large collider), spacecraft (km/s, orbital or free-flight). Per-entity authority (R5) and the spatial model (ADR-0005) must handle an entity whose interest radius and velocity change by orders of magnitude when the player boards a vehicle.
- **Nesting.** An avatar inside a mech inside a spacecraft is one authority chain, not three independent entities. Boarding and disembarking are authority handoffs and must be witnessable (R8).
- **Gunplay at 60 Hz (R10)** sets the prediction/rollback budget (ADR-0008): hit registration is the most latency-sensitive thing in the game and is the reference case for the verifiable core (ADR-0009).
- **Gathering, crafting and trade are all persistence writes (R7)** with economic value, so they are the reference case for quorum-attested writes and for the strike/blacklist model. Duplication is the attack to design against.
- **PvP means the trust model is adversarial by default.** Every witness may be an opponent of the party it is witnessing.
- **PvE means the universe simulates without players present**, or appears to. Whether NPC state is durable or regenerated is a decision to be made (see ADR-0051 for the terrain precedent).

## Open — to be settled by the owner

Sections reserved; each becomes a `G<n>` block when decided.

- G2 — Universe scale and topology (how many bodies, how far apart, travel time between them)
- G3 — Death, loss and persistence of the avatar and its possessions
- G4 — Ownership and territory (structures, bases, claims)
- G5 — Economy rules (currency, NPC sinks/faucets, player-only trade)
- G6 — Progression (skills, blueprints, ship/mech tiers)
- G7 — Population and grouping (factions, crews, guilds)
- G8 — PvE content model (NPC ships, fauna, missions)
- G9 — Time (real-time, accelerated, day/night, orbital mechanics)
