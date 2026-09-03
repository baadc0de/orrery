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

### Engineering consequences of G4

- **The mothership is one grid at the population of the whole server**, not the 32–128 of a typical area (R6). It is the interest-management and replication stress case, and its shared infrastructure (transit, shops) is NPC-driven state that every player observes. It needs either sub-grids (decks, hangars) with seamless transitions, or an interest model that scales past R6 for a single grid. This decision belongs in ADR-0005/0006 and should be made early.
- **Seamlessness is a grid-transition requirement.** Every boundary the player crosses (apartment door, hangar, airlock, undock, orbit, atmosphere, drop pod, cave mouth) is an authority or grid handoff that must complete within the player's movement, with prediction (ADR-0008) covering the gap. The number of distinct transition kinds is now large: enumerate them and make each a tested case.
- **Ownership is a first-class attribute** on ships, structures and apartments, with the mothership as a legal owner alongside players. Mothership-owned ships are the on-ramp (G4.3) and are shared, so their use must be scheduled or contested by rule.
- **Renown is a durable, non-transferable progression track** distinct from resources; it crosses seasons (G2.1d) and gates authority over shared assets. It is a reputation ledger and must be attested like any value-bearing write.
- **Nesting depth is now known:** avatar → mech → drop pod → ship → mothership, five levels. The authority chain and the interest radius per level must be designed for that depth, not two.
- **Surface structures are the territory game and are disposable.** They need build, upkeep, damage and destruction rules but no cross-season persistence. Their journals are retained only for the season (ADR-0020).
- **Existing occupants and fauna are the PvE baseline** (G8). "Displacing" them means NPC territory is a real quantity that shrinks as players build. NPC presence must therefore be durable state for the season, not respawned decoration.

### G4 — open decisions

Numbered so they can be answered one at a time.

1. **PvP scope on the mothership.** Is the mothership a safe zone (no weapons, no theft), partially safe (duels, sanctioned arenas), or fully open? This is the single biggest driver of the trust model at high population.
2. **One mothership or several.** If every player shares one mothership, PvP is intra-faction rivalry on the surface and in space. If there are several (rival factions, or one per shard), PvP is inter-faction and each mothership is its own root grid. This also fixes what G7 grouping means.
3. **Who may build on the surface.** Individuals, groups (G7), or only the mothership's collective effort? And who owns the resulting structure and its extracted resources?
4. **Structure conflict rules.** Can players destroy or capture each other's structures? Offline raiding allowed, or only within a declared window (siege timers)? What is salvaged from a destroyed structure?
5. **Exclusion or claim mechanics.** Does a structure claim a radius that blocks others from building, or is territory purely what you can physically hold?
6. **Apartment scarcity and tenure.** Fixed number of apartments? Rent or upkeep? Reassignment on inactivity? Are they tradeable?
7. **Hangar and docking capacity.** Is docking space finite and contested, and what happens to a ship whose owner cannot dock?
8. **Landing craft and drop pod ownership.** Player, ship, or mothership assets? Are they lost on a bad insertion?
9. **NPC counter-pressure.** Do displaced occupants retake territory, escalate, or stay displaced? Does fauna repopulate?
10. **Upkeep and decay of surface structures.** Do abandoned structures decay within a season, and can others take them over?
11. **Corpse and loot location on the mothership.** If G3 death can happen aboard, where does the loot go and who can take it (ties to 1).
12. **Transit as fast travel.** Mothership transit implies teleport-like movement within one grid; is that also allowed ship-to-ship or only within the hub?

## Open — to be settled by the owner

Sections reserved; each becomes a `G<n>` block when decided.

- G5 — Economy rules (currency, NPC sinks/faucets, player-only trade)
- G6 — Progression (skills, blueprints, ship/mech tiers)
- G7 — Population and grouping (factions, crews, guilds)
- G8 — PvE content model (NPC ships, fauna, missions)
- G9 — Time (real-time, accelerated, day/night, orbital mechanics)
