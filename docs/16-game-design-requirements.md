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
| G2.1e | **The jump is offline.** The season transition is a **maintenance window**, not a live event. The old system is closed, the migration and new-system seed run with no players connected, and the new system opens afterwards. | owner mandate |
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
- **The mothership is the one grid that is never discarded.** It is the durable root of player state: inventory aboard, research, blueprints, standing. It needs the strongest persistence guarantees in the game. Because the jump is offline (G2.1e) it is **not** a live handoff: it is a batch migration of the mothership grid into a freshly seeded system, run as an ops procedure with the cluster quiesced. No live-jump machinery is needed for it, and live jump design (G2.2a) must not be sized for it.
- **Season quests are shared, server-wide goals with a deadline.** Their progress is a value-bearing aggregate (contributions from many players) and therefore a quorum-attested write (R8). "Completed" is a consensus fact, and it triggers the season end, so it must be adjudicable by replay.
- **Two end conditions mean the deadline is a hard clock.** A wall-clock deadline is a global parameter that every node must agree on (ADR-0021). Either end condition schedules a maintenance window rather than triggering the transition in-tick; the window itself (drain and close the old system, verify the mothership journal is fully attested and durable, migrate, seed, reopen) is an ops procedure (doc 09) with a rehearsed runbook and a rollback point before the old system is discarded.

## G3 — Death, loss and insurance

| # | Requirement | Source |
|---|---|---|
| G3 | **Avatar death is temporary.** Death never ends progression; the avatar returns. | owner mandate |
| G3.1 | **Cloning.** Avatars resurrect as clones. Players pay **resource upkeep for clone quality**; a better-maintained clone is a better resurrection (what "quality" affects is settled in G6). | owner mandate |
| G3.2 | **Full loot.** Everything carried at death is dropped where the avatar died. **What is dropped is dropped**: no partial protection, no soulbound items, no automatic return. | owner mandate |
| G3.3 | **Opt-in insurance, in-game.** A player may insure items. On death, insurance **automatically creates procurement and delivery orders** that replace the insured items and bring them to the resurrected avatar. | owner mandate |
| G3.4 | **Retrieval.** The same insurance system can instead **retrieve the dropped items** (a recovery order against the death site) rather than procure new ones. | owner mandate |
| G3.5 | **Insurance is an economic actor**, not a menu option. Its orders are fulfilled through the trade and logistics systems (G1.6, G2.2c): procurement buys from players or stock, delivery is a flip-and-burn (or courier) run, retrieval is a trip to the corpse. | derived from G3.3, G3.4, G2.2c |

### Engineering consequences of G3

- **Death is a value transfer, not a state reset.** The drop is a persistence write that moves every carried item from the avatar to a loot container at the death site. Under PvP (G1.7) the killer is an interested party and often a witness, so the drop must be quorum-attested (R8) with the dropped set derived deterministically from the attested inventory, never from the client. Duplication on death (die, keep, and drop) is the primary exploit to design against.
- **The corpse is a durable, contested object.** It outlives the victim's session and is a PvP target like unattended cargo (G2). It needs an authority owner and a lifetime rule (decay, or until retrieved) that is a world parameter (ADR-0016).
- **Clone upkeep is a recurring resource sink** tied to the avatar, not to a session. It accrues while the player is offline and must be billed deterministically from durable state, so it is a lazily evaluated plan of the same kind as a flip-and-burn trajectory (G2), not a ticked process.
- **Resurrection location matters.** Where the clone wakes (mothership, a clone bay planetside, a ship) determines how far insurance must deliver and how exposed the fresh clone is. Clone bays are structures (G4) and therefore territory.
- **Insurance orders are automatic transactions on behalf of an absent player.** They create market orders, spend the player's resources, and dispatch craft without the player online. That is the first case of **system-initiated, player-owned intent**, and it must be signed in a way the attestation envelope (ADR-0027) can distinguish from live player input while still binding it to the player.
- **Insurance is a delivery pipeline, so it can be griefed.** Intercepted delivery runs and camped corpses are legitimate PvP but the insurance contract must specify the outcome (pay out, retry, or fail) deterministically. This is the same low-population adjudication problem as offline cargo interception (ADR-0029).
- **Full loot bounds per-avatar inventory to what can be lost.** The avatar's carried state is small and volatile; durable wealth lives in stockpiles (mothership, structures). This is a useful split for replication: carried inventory is hot state on the avatar entity, stockpiles are grid-owned and rarely replicated to non-owners.

## G4 — Territory: mothership, ships, planets

| # | Requirement | Source |
|---|---|---|
| G4 | **The mothership is a huge hub.** It contains **player-owned structures (apartments)** and **shared infrastructure**: NPCs, shops, transit, observation decks, hangars and the like. | owner mandate |
| G4.1 | **Seamless.** Minimal or no loading screens anywhere: within the mothership, mothership to ship, ship to space, space to surface. | owner mandate |
| G4.2 | **Ship ownership.** A ship belongs either to a **player** or to the **mothership**. | owner mandate |
| G4.3 | **Command is gated.** Owning and commanding a ship is locked behind both a **resource** progression and a **renown** progression (a title or certificate). | owner mandate |
| G4.4 | **Ships are the way into space.** A ship undocks from the mothership and takes its occupants to a mission, whatever it may be. | owner mandate |
| G4.5 | **Most ships cannot land.** Surface insertion uses **landing craft** and **drop pods** for mechs. | owner mandate |
| G4.6 | **Dismount.** An avatar may leave its mech, and must in order to traverse buildings and natural structures such as caves. | owner mandate |
| G4.7 | **No foothold at season start.** When a season opens, the mothership's forces hold nothing on any body. They build structures and set up resource extraction while **staving off or displacing existing occupants and fauna**. | owner mandate |
| G4.8 | **Planetary territory is temporary by construction.** Everything built on a body is left behind at the season jump (G2.1d). | derived from G2.1d, G4.7 |
| G4.9 | **PvP aboard the mothership is fully open.** No mechanical safe zone. Deterrence is in-world: **NPC security** responds to violence, and aggressors pay in **renown** (G4.3), which gates command and shared assets. | owner mandate (G4 open decision 1, answered 2026-09-03) |
| G4.10 | **One mothership** for the first release; **multiple** eventually. Design nothing that assumes a single root grid, but ship with one. | owner mandate (decision 2) |
| G4.11 | **Surface building is NPC-driven.** Players have agency in **establishing the conditions** for building (clearing, securing, supplying, choosing) but the actual construction is **long-term NPC activity**. Players do not place structures. | owner mandate (decision 3) |
| G4.12 | **Deterrence radius.** Defensive structures project a deterrence radius. Because placement is organic and NPC-driven, this radius only influences **other NPC factions**; it is not a player claim mechanic. | owner mandate (decision 5) |
| G4.13 | **Surface structures do not decay** but **take damage and can be destroyed**. | owner mandate (decisions 4, 10) |
| G4.14 | **Apartments require upkeep.** Failing upkeep makes the player **homeless**; the apartment's contents are **escrowed** until a new apartment is purchased. | owner mandate (decision 6) |
| G4.15 | **Docked ships are packed.** A ship left docked is eventually **packed into the mothership** to free docking space (stored, not lost). | owner mandate (decision 7) |
| G4.16 | **Drop pods and landing craft are consumables.** They are "inflatable" equipment a ship carries in cargo, counted as ship equipment and **expended** on use. | owner mandate (decision 8) |
| G4.17 | **NPC counter-pressure is real and fauna repopulates.** Balance target: **organized, well-equipped players can push ever forward** and meet **harder, more challenging resistance** as they do. | owner mandate (decision 9) |
| G4.18 | **Death aboard the mothership:** loot drops and is takeable **until NPC security reaches it**; security then **deposits it in the victim's apartment**, or in escrow if homeless. | owner mandate (decision 11) |
| G4.19 | **Teleport (a) site-to-site within the mothership**, and **(b) from anywhere to the mothership**, allowed only when **not engaged**, with a **cooldown**. No other teleport. | owner mandate (decision 12) |
| G4.20 | **Macro simulation is a separate service.** A dedicated service advances the slow-moving simulation on its own: buildings raised, **contracts issued**, **contracts taken by NPCs**, and **fulfilled without micro simulation**. The 60 Hz micro simulation never ticks this state. | owner mandate |

### Engineering consequences of G4

- **The mothership is one grid at the population of the whole server**, not the 32–128 of a typical area (R6). It is the interest-management and replication stress case, and its shared infrastructure (transit, shops) is NPC-driven state that every player observes. It needs either sub-grids (decks, hangars) with seamless transitions, or an interest model that scales past R6 for a single grid. This decision belongs in ADR-0005/0006 and should be made early.
- **Seamlessness is a grid-transition requirement.** Every boundary the player crosses (apartment door, hangar, airlock, undock, orbit, atmosphere, drop pod, cave mouth) is an authority or grid handoff that must complete within the player's movement, with prediction (ADR-0008) covering the gap. The number of distinct transition kinds is now large: enumerate them and make each a tested case.
- **Ownership is a first-class attribute** on ships, structures and apartments, with the mothership as a legal owner alongside players. Mothership-owned ships are the on-ramp (G4.3) and are shared, so their use must be scheduled or contested by rule.
- **Renown is a durable, non-transferable progression track** distinct from resources; it crosses seasons (G2.1d) and gates authority over shared assets. It is a reputation ledger and must be attested like any value-bearing write.
- **Nesting depth is now known:** avatar → mech → drop pod → ship → mothership, five levels. The authority chain and the interest radius per level must be designed for that depth, not two.
- **Surface structures are the territory game and are disposable.** They need build, upkeep, damage and destruction rules but no cross-season persistence. Their journals are retained only for the season (ADR-0020).
- **Open PvP on the mothership means the trust model has no safe grid.** Hit registration, loot drops and witnessing (R8) must hold at full-server population in one hub, and every bystander is a candidate witness. NPC security is server-authoritative PvE (G8) that must react within the same tick budget as the players it polices, and renown penalties are attested writes triggered by adjudicated violence, so a false accusation (or a missed one) is a reputation exploit. Crime detection needs a deterministic definition (who fired first, who was where) that replay can settle.
- **Two simulations, one world (G4.20).** The **macro service** owns slow state: NPC factions, construction, deterrence, fauna population, the contract market (issue, NPC take-up, fulfilment) and lazily evaluated plans (flip-and-burn logistics G2, clone upkeep G3, insurance orders G3). It is server-authoritative, runs on its own clock, and writes to the same durable store (R7) as the micro layer. The **micro layer** (60 Hz, per-entity authority, witnessed) owns whatever is currently near a player. The contract between them is the design problem: **materialise** macro state into entities when a player arrives, **fold** micro outcomes (a destroyed structure, a killed patrol, a delivered cargo) back into macro state when they leave, and never let both own the same fact at once. A contract taken by an NPC and fulfilled at macro level must produce the same ledger effect as one fulfilled by a player at micro level. This service belongs in doc 09 and needs its own ADR.
- **Escrow is a durable, mothership-scoped store** for a player's items outside any entity: homeless contents and security-collected loot both land there. It is the second player-state root after carried inventory and is never exposed to other players, so it can be replicated to its owner only.
- **Recall-to-mothership from anywhere is a teleport exit from any grid.** "Not engaged" must be a deterministic, witnessable predicate (recent damage dealt or taken, proximity to hostiles) or it becomes the universal PvP escape. The cooldown is a per-avatar durable timer. What happens to the mech, ship, or pod left behind on recall must be specified: it stays as an unattended object (G2 consequences apply).
- **Packing docked ships and expending pods are inventory transformations**, ship-to-item and item-to-entity. Both are value-bearing writes and both are grid transitions (the packed ship leaves the docking grid). Pods in particular are a "consumable that becomes a vehicle mid-flight", a spawn-under-prediction case for ADR-0008.
- **The single-mothership release must not bake in a singleton.** Ownership, escrow, renown and teleport targets take a mothership id from day one, even if only one value exists.
- **Existing occupants and fauna are the PvE baseline** (G8). "Displacing" them means NPC territory is a real quantity that shrinks as players build. NPC presence must therefore be durable state for the season, not respawned decoration.

### G4 — open decisions

Numbered so they can be answered one at a time.

1. ~~PvP scope on the mothership.~~ **Answered: fully open, with NPC security and renown as deterrents (G4.9).**
2. ~~One mothership or several.~~ **Answered: one now, multiple later (G4.10).**
3. ~~Who may build on the surface.~~ **Answered: NPC-driven building; players set conditions (G4.11).**
4. ~~Structure conflict rules.~~ **Answered: structures are damageable and destructible, do not decay (G4.13); capture unstated.**
5. ~~Exclusion or claim mechanics.~~ **Answered: deterrence radius, NPC-vs-NPC only (G4.12).**
6. ~~Apartment scarcity and tenure.~~ **Answered: upkeep; homelessness with escrow (G4.14).**
7. ~~Hangar and docking capacity.~~ **Answered: idle docked ships are packed (G4.15).**
8. ~~Landing craft and drop pod ownership.~~ **Answered: consumable ship equipment (G4.16).**
9. ~~NPC counter-pressure.~~ **Answered: NPCs push back, fauna repopulates, difficulty scales with push (G4.17).**
10. ~~Upkeep and decay of surface structures.~~ **Answered: no decay (G4.13); takeover unstated.**
11. ~~Corpse and loot location on the mothership.~~ **Answered: drops until NPC security collects, then apartment or escrow (G4.18).**
12. ~~Transit as fast travel.~~ **Answered: intra-mothership and recall-to-mothership only, not engaged, cooldown (G4.19).**

## Open — to be settled by the owner

Sections reserved; each becomes a `G<n>` block when decided.

- G5 — Economy rules (currency, NPC sinks/faucets, player-only trade)
- G6 — Progression (skills, blueprints, ship/mech tiers)
- G7 — Population and grouping (factions, crews, guilds)
- G8 — PvE content model (NPC ships, fauna, missions)
- G9 — Time (real-time, accelerated, day/night, orbital mechanics)
