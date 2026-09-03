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

## G2 — Universe scale and topology

| # | Requirement | Source |
|---|---|---|
| G2 | **One solar system at a time.** The live universe is a single solar system of **dozens of bodies** (star, planets, moons; asteroids/stations as bodies or as content on them). | owner mandate |
| G2.1 | **Seasonal.** A season is a bounded period during which one solar system is live. At the end of a season the **mothership jumps to a new system**; the old system is discarded. | owner mandate (confirmed 2026-09-03) |
| G2.1a | **Season quests.** At season start players receive **season-length quests**: acquire resources, research technology, stockpile goods, craft. | owner mandate |
| G2.1b | **Two end conditions.** The season ends when the quests are **completed** or when **time runs out**, whichever comes first. | owner mandate |
| G2.1c | **Mothership.** A persistent, jump-capable hull that carries the players (and whatever they have stockpiled aboard it) between systems. It is the container for everything that survives a season boundary. | owner mandate |
| G2.1d | **Reset without losing progression.** The season jump discards accumulated world state ("crud") in the old system, but **progression carries over**. What counts as progression versus world state is settled in G3/G6; the mothership boundary is the rule of thumb: aboard it persists, left behind is gone. | owner mandate |
| G2.2 | **Three spacecraft classes**, distinguished by how they move: | owner mandate |
| G2.2a | **Fast-response craft** carry a **jump drive**. Jump travel takes **seconds to minutes** anywhere in the system. | owner mandate |
| G2.2b | **Escorts** have **no jump capability**. They fight alongside and are moved by other means (carried, or flip-and-burn). | owner mandate |
| G2.2c | **Flip-and-burn craft** use continuous-thrust physics (accelerate, flip, decelerate) with **slightly exaggerated** constants so that in-system travel takes **hours to days**. Used for automated resource delivery, planet and moon landing, and other tasks that are not time-sensitive. | owner mandate |
| G2.3 | **Two time scales of travel coexist.** Jump (seconds–minutes) is the player's tactical mobility; flip-and-burn (hours–days) is the logistics layer and continues whether or not the owning player is online. | derived from G2.2 |

### Engineering consequences of G2

- **Bodies are the unit of surface space.** Dozens of bodies, each landable, means dozens of surface grids plus one space grid, and the grid id is already part of the storage key (ADR-0022). Landing/take-off is a grid transition and an authority handoff, same class of event as boarding (G1 consequences).
- **A jump is a teleport across the spatial model.** Interest sets, cell standing (ADR-0030) and witness sets (ADR-0028) at the destination must be assembled before arrival. The seconds-to-minutes spool time is the budget for that handoff; it should be treated as a design parameter (ADR-0016), not just flavour.
- **Flip-and-burn ships are durable, unattended simulation.** A cargo run lasting a day outlives any session. Its trajectory is deterministic from a few parameters (burn start, thrust, mass, target), so it should be persisted as a **plan** and evaluated lazily, not ticked at 60 Hz. This is the first concrete case of "the world moves while nobody is there" and should settle the PvE durability question (G8) at the same time.
- **Unattended cargo is a PvP target.** Interception of an offline player's freighter must have an authority owner, witnesses and a deterministic outcome without the victim present. This is the hardest trust case in the game and is the design driver for the low-population path (ADR-0029).
- **Escorts imply formation and carriage.** Either escorts ride inside a jump-capable hull (nesting, G1) or they arrive by flip-and-burn ahead of time. Both make fleet movement a planned, persisted thing.
- **Seasons are the persistence lifecycle.** A season boundary is the retention rule (ADR-0020, ADR-0023): the old system's grids are dropped wholesale, a fresh seed (doc 12) produces the next system, and only the mothership's contents cross. This bounds unbounded growth of terrain edits, wrecks and abandoned structures by construction rather than by garbage collection.
- **The mothership is the one grid that is never discarded.** It is the durable root of player state: inventory aboard, research, blueprints, standing. It needs the strongest persistence guarantees in the game and its jump is a whole-population migration, the extreme case of the jump handoff above.
- **Season quests are shared, server-wide goals with a deadline.** Their progress is a value-bearing aggregate (contributions from many players) and therefore a quorum-attested write (R8). "Completed" is a consensus fact, and it triggers the season end, so it must be adjudicable by replay.
- **Two end conditions mean the deadline is a hard clock.** A wall-clock deadline is a global parameter that every node must agree on (ADR-0021), and the end-of-season sequence (stop accepting writes to the old system, migrate, seed, reopen) is an ops procedure (doc 09) that needs a rehearsed runbook.

## Open — to be settled by the owner

Sections reserved; each becomes a `G<n>` block when decided.

- G3 — Death, loss and persistence of the avatar and its possessions
- G4 — Ownership and territory (structures, bases, claims)
- G5 — Economy rules (currency, NPC sinks/faucets, player-only trade)
- G6 — Progression (skills, blueprints, ship/mech tiers)
- G7 — Population and grouping (factions, crews, guilds)
- G8 — PvE content model (NPC ships, fauna, missions)
- G9 — Time (real-time, accelerated, day/night, orbital mechanics)
