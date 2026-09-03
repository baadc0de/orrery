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
| G2.2b | **Escorts (light craft)** have **no jump capability**. They are **expendable equipment of a ship**: carried compacted in its inventory, inflated on demand when a crew member launches, and lost when destroyed (G4.16). Nobody owns an escort; the ship stocks them. | owner mandate (revised 2026-09-03) |
| G2.2c | **Flip-and-burn craft** use continuous-thrust physics (accelerate, flip, decelerate) with **slightly exaggerated** constants so that in-system travel takes **hours to days**. Used for automated resource delivery, planet and moon landing, and other tasks that are not time-sensitive. | owner mandate |
| G2.3 | **Two time scales of travel coexist.** Jump (seconds–minutes) is the player's tactical mobility; flip-and-burn (hours–days) is the logistics layer and continues whether or not the owning player is online. | derived from G2.2 |
| G2.4 | **Bodies are static within a season.** Rotation gives day–night (G9.1); positions are fixed by the season seed, so flip-and-burn plans, jump targets and intercept geometry are stable for the season. | owner decision 2026-09-03 |

### Engineering consequences of G2

- **Bodies are the unit of surface space.** Dozens of bodies, each landable, means dozens of surface grids plus one space grid, and the grid id is already part of the storage key (ADR-0022). Landing/take-off is a grid transition and an authority handoff, same class of event as boarding (G1 consequences).
- **A jump is a teleport across the spatial model.** Interest sets, cell standing (ADR-0030) and witness sets (ADR-0028) at the destination must be assembled before arrival. The seconds-to-minutes spool time is the budget for that handoff; it should be treated as a design parameter (ADR-0016), not just flavour.
- **Flip-and-burn ships are durable, unattended simulation.** A cargo run lasting a day outlives any session. Its trajectory is deterministic from a few parameters (burn start, thrust, mass, target), so it should be persisted as a **plan** and evaluated lazily, not ticked at 60 Hz. This is the first concrete case of "the world moves while nobody is there" and should settle the PvE durability question (G8) at the same time.
- **Unattended cargo is a PvP target.** Interception of an offline player's freighter must have an authority owner, witnesses and a deterministic outcome without the victim present. This is the hardest trust case in the game and is the design driver for the low-population path (ADR-0029).
- **Escorts are carried, not docked.** They travel compacted in the parent ship's inventory and exist as entities only while launched, so fleet movement is one grid moving, not a formation of owned grids. The nesting case that remains is a launched escort or mech re-entering its parent (compaction), which is an entity-to-item handoff rather than grid-in-grid ownership.
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
| G3.6 | **Clone printers are respawn points.** Anything with a clone printer is a place to resurrect: the **mothership**, **all jump-capable ships** (G2.2a), and **some defensive planetside structures** (G4.13). Escorts, landing craft and non-jump hulls have none. | owner mandate |
| G3.7 | **The player chooses the respawn point** among eligible printers (G3.6). | owner mandate |
| G3.8 | **Wrecks are loot too.** A destroyed mech or craft leaves behind its **hull and other resources**, which can be **salvaged** by anyone. | owner mandate |

### Engineering consequences of G3

- **Death is a value transfer, not a state reset.** The drop is a persistence write that moves every carried item from the avatar to a loot container at the death site. Under PvP (G1.7) the killer is an interested party and often a witness, so the drop must be quorum-attested (R8) with the dropped set derived deterministically from the attested inventory, never from the client. Duplication on death (die, keep, and drop) is the primary exploit to design against.
- **The corpse is a durable, contested object.** It outlives the victim's session and is a PvP target like unattended cargo (G2). It needs an authority owner and a lifetime rule (decay, or until retrieved) that is a world parameter (ADR-0016).
- **Clone upkeep is a recurring resource sink** tied to the avatar, not to a session. It accrues while the player is offline and must be billed deterministically from durable state, so it is a lazily evaluated plan of the same kind as a flip-and-burn trajectory (G2), not a ticked process.
- **Resurrection location is player-chosen among eligible printers (G3.7)**, so the death screen is a query over printers the player may use (owner's crew, organization, mothership, capacity). Where the clone wakes determines how far insurance must deliver and how exposed the fresh clone is. A planetside printer is a structure and can be destroyed (G4.13), so a mission's forward respawn can be taken away mid-fight: losing the printer is the surface objective that matters most, for both sides.
- **A clone printer is a capability of a grid**, present on the mothership grid, on every jump-capable ship grid, and on the structure entities that have one. The respawn write (new avatar entity at printer, upkeep charged, loadout restored from insurance or empty) executes under that grid's authority and is witnessed there.
- **Insurance orders are automatic transactions on behalf of an absent player.** They create market orders, spend the player's resources, and dispatch craft without the player online. That is the first case of **system-initiated, player-owned intent**, and it must be signed in a way the attestation envelope (ADR-0027) can distinguish from live player input while still binding it to the player.
- **Insurance is a delivery pipeline, so it can be griefed.** Intercepted delivery runs and camped corpses are legitimate PvP but the insurance contract must specify the outcome (pay out, retry, or fail) deterministically. This is the same low-population adjudication problem as offline cargo interception (ADR-0029).
- **Wrecks are durable, contested objects like corpses (G3.8).** A destroyed vehicle becomes a salvage entity at the death site with a resource content derived deterministically from the attested loadout, under the same authority and lifetime rules as a corpse. Salvage is a resource faucet the macro service must account for, and a wreck field after a fleet engagement is a lot of entities in one place: it needs aggregation or decay so the grid does not fill with debris.
- **Full loot bounds per-avatar inventory to what can be lost.** The avatar's carried state is small and volatile; durable wealth lives in stockpiles (mothership, structures). This is a useful split for replication: carried inventory is hot state on the avatar entity, stockpiles are grid-owned and rarely replicated to non-owners.

## G4 — Territory: mothership, ships, planets

| # | Requirement | Source |
|---|---|---|
| G4 | **The mothership is a huge hub.** It contains **player-owned structures (apartments)** and **shared infrastructure**: NPCs, shops, transit, observation decks, hangars and the like. | owner mandate |
| G4.1 | **Seamless.** Minimal or no loading screens anywhere: within the mothership, mothership to ship, ship to space, space to surface. | owner mandate |
| G4.2 | **Ship ownership.** A ship belongs either to a **player organization** (G7) or to the **mothership**. **A player cannot own a ship.** | owner mandate (revised 2026-09-03) |
| G4.3 | **Command is gated.** An organization owns a ship only with **sufficient renown** and the **resources** to buy it; an individual commands one only with the renown for the title or certificate, and by assignment from the owner (organization or mothership). | owner mandate (revised 2026-09-03) |
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
| G4.16 | **Drop pods, landing craft, escorts and mechs are consumables.** All are "inflatable" equipment a ship carries compacted in cargo, counted as ship equipment, inflated on demand when a crew member needs one, and **expended** when used up or destroyed. | owner mandate (decision 8, extended 2026-09-03) |
| G4.17 | **NPC counter-pressure is real and fauna repopulates.** Balance target: **organized, well-equipped players can push ever forward** and meet **harder, more challenging resistance** as they do. | owner mandate (decision 9) |
| G4.18 | **Death aboard the mothership:** loot drops and is takeable **until NPC security reaches it**; security then **deposits it in the victim's apartment**, or in escrow if homeless. | owner mandate (decision 11) |
| G4.19 | **Teleport (a) site-to-site within the mothership**, and **(b) from anywhere to the mothership**, allowed only when **not engaged**, with a **cooldown**. No other teleport. | owner mandate (decision 12) |
| G4.20 | **Macro simulation is a separate service.** A dedicated service advances the slow-moving simulation on its own: buildings raised, **contracts issued**, **contracts taken by NPCs**, and **fulfilled without micro simulation**. The 60 Hz micro simulation never ticks this state. | owner mandate |
| G4.21 | **Capture by clearing, then NPC conversion.** Players clear and hold an enemy structure; the macro service then converts it to their faction over time, by the same NPC-driven building as G4.11. A captured printer becomes a forward respawn (G3.6). | owner decision 2026-09-03 |

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

## G5 — Economy: contracts, money, standing

| # | Requirement | Source |
|---|---|---|
| G5 | **The contract is the unit of economic activity.** Gathering, delivery, crafting, procurement, retrieval and clandestine work are all expressed as contracts. | derived from G4.20, G3.3 |
| G5.1 | **Issuers:** players, the mothership, insurance (G3.3), and NPC factions. | owner mandate |
| G5.2 | **Working against the mothership is allowed.** Players may take NPC-faction contracts for **clandestine ops** against the mothership's interests. Doing so may affect **renown and standing**. | owner mandate |
| G5.3 | **Player contracts are priced by the player.** A **suggest** feature computes a "fair" value from macro-service data; the player may ignore it. | owner mandate |
| G5.4 | **Automatic pricing** for mothership, insurance and NPC-faction contracts, computed by the macro service. | owner mandate |
| G5.5 | **Players get first refusal.** NPCs let a contract run for a while before taking it, so a player has the chance to take it first. | owner mandate |
| G5.6 | **Player-taken is micro, NPC-taken is macro.** A contract taken by a player is fulfilled in the micro simulation. Macro fulfilment is an optimisation for when no player is involved, and is expected to carry **a good chunk of the boring supply economy**. | owner mandate |
| G5.7 | **Contracts exchange money, work and goods in any combination.** There is a currency, but a contract need not use it. | owner mandate |
| G5.8 | **Two reputation quantities.** **Standing** is per-faction (G6.1); **renown** is the aggregate of standing with the mothership's own organizations (G6.2). Clandestine work trades standing with one faction for standing with another. | owner mandate, detailed in G6 |

### Engineering consequences of G5

- **The contract is a state machine with a ledger.** Issued → open (player window, G5.5) → taken (by player: micro; by NPC: macro) → fulfilled / failed / expired → settled. Every transition is a value-bearing write. The player-window timer and the NPC take-up decision live in the macro service; the take-by-player transition is the one place the two layers hand a job across, and it must be atomic (a contract cannot be taken twice).
- **Settlement is escrowed.** Money, goods and the promise of work are locked at issue or at take, and released at settlement, so neither party can default mid-contract and no item exists in two places. The escrow store from G4 generalises to contract escrow.
- **Micro fulfilment must be verifiable by the macro service.** "Delivered 40 t of ore to structure X" is a micro-layer fact that the macro ledger acts on. It must arrive as an attested event (R8, ADR-0027), not as a client claim, and the macro service is a consumer of the witnessed journal (ADR-0019), not of live entity state.
- **Macro and micro fulfilment must be ledger-equivalent.** The same contract, fulfilled by an NPC in the macro service or by a player in the micro layer, must produce the same resource movements. This is the invariant that keeps macro fulfilment an optimisation rather than a second economy, and it is a property-testable statement.
- **Fair-value suggestion is a read-only macro query** over prices, distances, travel time (G2.2c), risk and scarcity. It needs a price history, so the macro service keeps a time series of settlements.
- **Clandestine ops need concealment as a game fact.** A contract against the mothership must be hidden from the mothership's view (NPC security, other players' contract boards) until discovered, so contract visibility is per-viewer and the "discovered" transition is an attested event that triggers standing loss. Under open PvP (G4.9) this is also how players police each other.
- **Currency is a durable balance per player**, mothership-scoped (G4.10), with faucets and sinks owned by the macro service (contract pricing, upkeep G3.1/G4.14). Inflation control is a macro-service tuning problem and needs telemetry from the first playtest.
- **NPC take-up is the demand floor.** Because NPCs eventually take any priced contract, no player-issued contract starves; the delay in G5.5 is the parameter that decides how much of the economy is player-run and is a season-tunable value (ADR-0016).

## G6 — Progression: standing, renown, items, ships

| # | Requirement | Source |
|---|---|---|
| G6 | **Progression is standing plus possessions.** There is no separate skill tree: what a player can do is what their standing unlocks and what their items grant. | derived from G6.1–G6.4 |
| G6.1 | **Standing** is gained and lost against **NPC factions** and against **mothership NPC organizations**, which are segmented (**security, logistics, industry, research**, and the like). Each is its own ledger. | owner mandate |
| G6.2 | **Renown** is the **cumulative standing across the mothership's NPC organizations**. It gates **mothership services**: teleports (G4.19), **clone grades** (G3.1), **apartment luxury** (G4.14), **rights to dock** (G4.15), ship command (G4.3), and so on. | owner mandate |
| G6.2a | **Renown is a weighted sum of the mothership organization standings, with per-gate floors.** Each gated service may additionally require a minimum standing in a named organization (docking → logistics, clone grade → research, and so on). | owner decision 2026-09-03 |
| G6.3 | **Items grant abilities and buffs.** Players accumulate and spend resources, craft or purchase items, and those items are what give abilities and buffs. | owner mandate |
| G6.4 | **Ships are items**, an **extremely expensive** investment, and **only organizations and the mothership can own them** (G4.2). | owner mandate (revised 2026-09-03) |
| G6.5 | **The core loop is clonable crew.** Players join a **mission** on a ship, use the ship's resources (escorts, mechs, pods, G4.16), fight in **fleet and mech engagements**, die, **respawn aboard the ship** and try again. No player is out doing content alone in a jump-capable hull. Ownership is never the player's burden. | owner mandate (revised 2026-09-03) |
| G6.6 | **Organizations pool resources to own a ship**, subject to organization renown (G4.3), and **assign a captain** from their ranks. | owner mandate |
| G6.6a | **Organizations have standing and renown of their own**, distinct from their members'. | derived from G4.3, G6.6 |
| G6.8 | **Loadouts are the owner's.** The ship's owner defines the **mech and craft loadouts** and **supplies them when the ship loads** for a mission. Crew **choose a loadout** from those available. | owner mandate |
| G6.9 | **On-body items modify vehicles.** A player's own equipment may **dramatically influence** the mechs and craft they pilot. | owner mandate |
| G6.7 | **Standing and renown cross seasons** (G2.1d); they are progression, not world state. Items cross only if aboard the mothership. | derived from G2.1d |

### Engineering consequences of G6

- **Standing is a set of per-faction ledgers on the player**, each a durable signed integer (or bounded scalar) with an attested change log. Renown is a **derived** value, computed from the mothership organizations' ledgers, never stored independently, so it cannot drift from its inputs. The weights and per-gate floors (G6.2a) are ruleset parameters (ADR-0021) and changing them re-derives every player's renown at once.
- **Renown gates are threshold checks at service boundaries** (teleport, clone, dock, command). Each check happens where the service is authoritative (mothership grid or macro service), reads attested standing, and is itself witnessable, so a client cannot claim a grade it lacks. The list of gated services will grow; keep it a table in the ruleset, not code.
- **Items are the capability system.** "Ability" and "buff" are properties of an item in the ruleset, resolved on equip, so the avatar's effective capabilities are a pure function of attested inventory. This makes full loot (G3.2) also a loss of capability, and it means hit registration and movement (ADR-0008) take their parameters from equipment, which prediction must know before the tick.
- **A ship is an item with a grid inside it.** Packing (G4.15) is the item form; undocking materialises the grid. The owner is always an organization or the mothership, never an avatar, so ownership is an **organization-level attribute** and **captain assignment** is a separate, revocable right. Ownership and command are different attributes, and both are attested.
- **Jump-capable ships are respawn points (G3.6).** A mission member who dies respawns at the ship's printer, paying clone upkeep (G3.1) and dropping loot at the death site (G3.2). Printer capacity is the clone charges loaded aboard (G8.7).
- **A mission is provisioned before undock (G6.8).** The owner's loadout definitions and the stocked consumables are the mission's budget, fixed at load time and drawn down as crew launch. Loadout choice by a crew member is a claim against that budget, so it is an attested reservation, not a client pick. Contested picks (two players want the last heavy mech) resolve in tick order under the ship grid's authority.
- **Vehicle parameters are a function of (loadout, pilot inventory) (G6.9).** The effective mech or craft is computed from the owner's loadout plus the pilot's attested on-body items at inflate time, and re-derived if the pilot's equipment changes. Prediction (ADR-0008) needs both inputs before the vehicle spawns, and the witness set must be able to reproduce the derivation, so it is a pure ruleset function (ADR-0021).
- **Escorts and mechs are spawned from ship inventory under prediction.** A crew member requesting a launch causes an item-to-entity transformation (G4 consequences) at the ship, and the entity is owned by the ship, piloted by the player. When it is destroyed the item is gone; when it returns it may be compacted again. The player never carries the vehicle; only their avatar and its loadout are theirs to lose.
- **Organizations are a first-class entity before G7 says anything else.** They own property, hold a resource pool, and grant rights to members. That is a ledger with a membership list and a rights table, mothership-scoped (G4.10).
- **Missions are the only multiplayer unit.** Every player reaches space and the surface as crew of a ship owned by someone else, so the ship grid's authority, its crew list, its inventory of consumable vehicles, and the captain's control of undock, jump, launch and recall are the primary group-play mechanics. Crew rights when the owning organization has no officer aboard need a rule.
- **"Extremely expensive" is a tuning target with telemetry**, not a number: the macro service should report ships per organization and mission participation per active player, and the season economy is tuned so that ships stay rare and missions stay full.

## G7 — Organizations, crews, squads, rivalry

| # | Requirement | Source |
|---|---|---|
| G7 | **Every player is always a member of exactly one organization.** | owner mandate |
| G7.1 | **Training organization.** A default organization exists on the mothership. Every character joins it at creation and **returns to it automatically** when kicked from or leaving any other organization. | owner mandate |
| G7.2 | **Joining and leaving is player activity** and may require **fees, upkeep, and standing or renown gates**. | owner mandate |
| G7.3 | **One guild leader per organization.** The leader **passes leadership unilaterally** and is the **only member who can kick**. | owner mandate |
| G7.4 | **Mission crew is contract-only** (G5). Eligibility may be **scoped to an organization**, like any other contract gate. | owner mandate |
| G7.5 | **NPC pilot fallback.** When neither CO nor XO is aboard, an NPC pilot takes the ship: **holds steady**, and if damage is critical **requests an emergency mothership teleport-out** of the ship. | owner mandate |
| G7.5a | **Emergency teleport-out moves the whole ship and everyone aboard** to the mothership. It is **discretionary**: granted or refused based on **mothership and owner resources**, and it **takes time to execute** once granted. | owner mandate |
| G7.6 | **Full friendly fire, everywhere.** Anyone can turn on anyone, including their own crew and organization. NPC security responds as it does everywhere (G4.9). | owner mandate |
| G7.7 | **Squads are ad hoc**, with basic **text and voice** comms. A squad may be marked **open** so random players find it through social tools. | owner mandate |
| G7.8 | **Identity reveal only on death.** In clandestine ops (G5.2), identity is revealed and standing lost **only on corpse capture**. Being seen or contacted costs nothing. | owner mandate |
| G7.9 | **Rivalry is the core PvP mechanic.** Organizations can hold rivalries, **loot each other for resources** and **interrupt each other's missions**. Gathering is slow; stealing is faster but risky. | owner mandate |
| G7.9a | **Anyone can loot anyone.** An **official rivalry** is a declared state that serves as a **friend-or-foe indicator** in the UI; it grants no permission and imposes no restriction. | owner mandate |
| G7.10 | **Defection is a strategy.** Aligning with opposing NPC factions can bring **alternative, powerful items**, at the risk of both failing to procure resources and losing standing. | owner mandate |
| G7.11 | **CO and XO** are the commanding and executive officer roles on a ship, assigned by the owner (G4.3, G6.6). | derived from G7.5 |
| G7.12 | **Crew rights without an officer aboard.** Contract crew may launch stocked loadouts and respawn, but may not move, undock or jump the ship, nor open non-loadout cargo. The NPC pilot (G7.5) holds position. | owner decision 2026-09-03 |

### Engineering consequences of G7

- **Organization membership is a total function** from avatar to organization, never null. The training organization is the mothership-owned default (G4.10 mothership id applies) and membership changes are attested writes with a fee/gate check at the boundary, the same shape as renown gates (G6). Kick is a leader-only write; leadership transfer is a two-party write (or leader-only if the leader may assign unilaterally: to confirm).
- **Roles on a ship are a small rights table**: owner (organization or mothership), CO, XO, crew. The NPC pilot is a server-side actor that holds authority over the ship grid's movement when no CO/XO is aboard, so the ship grid always has a controller and never drifts unowned. Kick and leadership transfer are leader-only writes (G7.3).
- **Emergency teleport-out is a whole-grid migration under fire (G7.5a).** Unlike the offline season jump (G2.1e) this is live: the ship grid, its crew and its inventory move to the mothership while under attack. The **discretionary grant** is a macro-service decision (owner and mothership resources are ledgers there) and the **execution delay** is the counter-play window: attackers can finish the ship, or board it, before it leaves. The request, grant and completion are attested events, and the ship is engaged by definition, so this path is the one exception to the not-engaged predicate of G4.19, paid for in resources and time rather than in eligibility. Whatever is not aboard when the timer expires (launched escorts, mechs on the surface) stays behind as unattended objects.
- **Friendly fire everywhere means no team flag in the damage model.** Damage is faction-agnostic; standing and NPC security are the only consequences. This simplifies hit registration (no ally check) and moves all of the cost into the standing ledger and the crime definition (G4.9 consequences).
- **Squads are ephemeral and outside the simulation.** Text, voice and the open-squad directory are social services (doc 09), not replicated state; they need presence and a directory, and nothing about them is witnessed. Voice is a separate transport concern.
- **Identity-on-death makes the corpse an evidence object.** Corpse capture (G3.2 loot container) is the attested event that reveals identity and applies the standing penalty. The avatar entity as seen live must therefore carry a **viewer-dependent identity**: an unrevealed avatar replicates as anonymous to non-crew, and only the death write attaches the durable identity. This is a replication-filter requirement (ADR-0003) and a witnessing subtlety: witnesses attest the entity id, not the player, until reveal.
- **Rivalry is a value-transfer game and the macro service must model it.** Looting an organization's structure or convoy is a micro-layer event whose ledger effect (resources move from one organization to another) folds into macro state; interrupting a mission is a micro event whose macro effect is contract failure. Both feed the risk term of the fair-value suggestion (G5.3).
- **Official rivalry is presentation, not rules (G7.9a).** It is a declared relation between two organizations, replicated so clients can colour friend-or-foe, and the simulation never reads it. Combined with viewer-dependent identity (G7.8) the indicator can only mark what the viewer is allowed to know: an unrevealed rival shows as unknown, not as hostile.
- **Defection items are a parallel tech tree keyed on NPC-faction standing (G7.10)**, so faction standing is not a score but an unlock table, and the same renown-gate mechanism (G6) evaluates it at the NPC faction's authority (macro service or a faction-controlled surface structure).

## G8 — PvE: NPC factions, encounters, quests

| # | Requirement | Source |
|---|---|---|
| G8 | **The existing occupants are organized, spacefaring forces** with ships, ground forces and structures. Tech level varies: some are primitive, some **far exceed** player technology. | owner mandate |
| G8.1 | **The deeper, the meaner.** Opposition strength depends on **location** and **depth of penetration** into a faction's territory. | owner mandate |
| G8.2 | **NPC fleets are active.** Factions field ships, execute **fleet manoeuvres** against players, plant **decoys** and **bait**, and run **ambush tactics**. | owner mandate |
| G8.3 | **Fauna is hazard and harvest only.** A local threat to avatars and a resource source (hide, biomass, exotic reagents). Never a mech- or ship-scale threat; no taming. | owner decision 2026-09-03 |
| G8.4 | **Clandestine missions.** Some NPC factions offer missions for rewards: thwart NPC or player forces on a mission, raid another NPC faction in their stead, and the like. These pay in **items and exotic, high-value resources**. | owner mandate |
| G8.5 | **Difficulty scales by numbers and tech**, with **scripted encounters** and **objective triggers**. | owner mandate |
| G8.6 | **Season quests are both handcrafted per season and continuously auto-generated** from system content. | owner mandate |
| G8.7 | **Clone charges are a limited, replenishable resource**, stocked like any other (ship printers G3.6, G6.8). | owner mandate |
| G8.8 | **Environmental hazards exist but are few.** The core opposition is an active force. | owner mandate |
| G8.9 | **NPCs drop items and wrecks** under the same rules as players (G3.2, G3.8). | owner mandate |
| G8.10 | **NPC factions are macro-only when no player is present**, exactly like the mothership faction (G4.20). | owner mandate |
| G8.11 | **The mothership faction is one faction among many** in principle and mechanism: the same macro machinery runs it and its opponents. Its **configuration** (tech, structures, abilities, and that players belong to it by default, G7.1) is very different from theirs, as each NPC faction's is from the others. | owner mandate (confirmed 2026-09-03) |

### Engineering consequences of G8

- **Symmetric faction model.** The macro service runs every faction, the mothership's included, through one faction simulation: territory, structures, fleets, contracts, standing. Asymmetry is data (tech level, aggression, depth curves), not code. This is the single biggest simplification available and should be an ADR constraint: no mothership-specific macro logic.
- **Depth is a field over the system**, per faction, computed by the macro service from territory (deterrence radii G4.12, structures, fleet presence). Difficulty parameters (numbers, tech tier) are functions of that field at a location, so "the deeper, the meaner" is a lookup, not a script. Player expansion reshapes the field, which is how G4.17 scaling happens.
- **NPC fleet tactics are micro-layer AI with macro-layer intent.** The macro service decides *that* a faction ambushes a convoy route or baits a mission; the micro layer executes the manoeuvre when players arrive, with server-side authority (NPC entities never have a player authority owner, R5 applies only to player-owned entities). Decoys and bait are entities whose replicated appearance differs from their attested truth, the same viewer-dependent replication as hidden identity (G7.8), so the replication filter must be able to lie by ruleset.
- **Scripted encounters and objective triggers are ruleset content**, distributed with the season (ADR-0021) and evaluated deterministically so witnesses agree on when a trigger fired. Triggers are attested events that the macro service consumes (a trigger can complete a quest step or move a faction).
- **Quest generation is a macro-service producer.** Handcrafted quests are season data; generated ones are derived from macro state (which resources exist where, which faction holds what) by a generator that must be deterministic given the macro journal so the same season replays the same quests. Both feed the season-end condition (G2.1b) as attested aggregates.
- **Clone charges are inventory.** A printer with zero charges does not respawn; a respawn consumes one under the printer grid's authority. Charges are craftable/purchasable (G6.3), delivered by contract (G5), and the mission's ticket budget (G6 open question) is simply the charges loaded at undock. The player-chosen respawn point (G3.7) lists only printers with charges.
- **NPC loot and wrecks reuse the corpse and salvage entities** (G3 consequences), with content derived from the NPC's attested loadout. High-value clandestine rewards are contract settlements (G5), so the exotic-resource faucet is metered by the macro service like any other.
- **Materialise and fold are the whole PvE pipeline.** A faction's macro state (fleet positions, structure health, patrol plans) materialises into micro entities when a player enters range and folds back (kills, damage, looted stock, triggered scripts) when the last player leaves. Because factions never run micro without a player, the fold must be complete: nothing that happened in micro may be lost, and nothing that did not happen may be invented. The fold is derived from the witnessed journal (ADR-0019), not from the last authoritative snapshot.
- **Environmental hazards are ruleset fields** (radiation, pressure, weather) that modify avatar and vehicle parameters (G6.9 derivation) and are few enough to be per-body constants for the first release.

## G9 — Time

| # | Requirement | Source |
|---|---|---|
| G9 | **A season is 1–3 months of wall-clock time.** In-fiction: the longest a mothership dares stay in-system before overwhelming forces endanger it (G2.1, G8.1). | owner mandate |
| G9.1 | **Mostly real time.** The game clock runs at wall-clock rate except that **day–night cycles are accelerated** so they occur more often. | owner mandate |
| G9.2 | **Teleportation is the time-saver** (G4.19, G2.2a). No other time compression. | owner mandate |
| G9.3 | **Flip-and-burn time is a spatial anchor.** Hours-to-days transit exists to give intercept and defend missions a **place and time to happen** and a **believable risk of failure**, not to be waited through. | owner mandate |
| G9.4 | **The season clock is the fiction's pressure.** Overwhelming force accumulating over the season is what ends it if the quests do not (G2.1b). | derived from G9, G8.1 |

### Engineering consequences of G9

- **One monotonic season clock**, wall-clock rate, agreed by every node (ADR-0021 parameter) and journaled, from which the deadline (G2.1b), contract windows (G5.5), upkeep accrual (G3.1, G4.14), recall cooldowns (G4.19) and flip-and-burn plans (G2.2c) are all evaluated. Day–night is a **separate derived clock** (season time times a per-body multiplier plus phase) that only presentation and hazard/ruleset fields read; nothing value-bearing keys on it.
- **The macro service can advance the season clock unattended** because everything slow is a plan evaluated lazily against it. A season of 1–3 months means macro state accumulates for up to 90 days, which sets the journal retention floor for a season (ADR-0020) and the size of the end-of-season migration (G2.1e).
- **Flip-and-burn plans are intercept geometry.** A cargo run is a known trajectory over known time, so an interceptor can be positioned by a rival organization or an NPC faction (G8.2) at a computable point. The macro service must expose trajectories to those entitled to see them (owner, and anyone who has scouted them by ruleset), which is another viewer-dependent view. Failure risk is real because the run resolves either macro (nobody came) or micro (someone did), and the fold (G8 consequences) decides the outcome.
- **Accumulating pressure is a macro parameter**, a faction-aggression term that rises with season time, so the last weeks of a season are harder by design. It can be the same field as depth (G8 consequences) with a time-dependent scale.
- **Nothing in the micro layer depends on wall-clock time.** The 60 Hz tick (R10) is the only clock the verifiable core sees; season time enters micro only as an attested input (the tick at which the season clock read T), so replay (R8) never needs the real clock.

## Open items

Loose ends collected from the sections above.

- **Fair-value formula, NPC take-up delay, renown weights and floors** (G5.3, G5.5, G6.2a): numbers, to be tuned with telemetry.
- **Capture hold condition** (G4.21): what "holding" a cleared structure means (presence, time, supplies) before conversion starts.
- **Promotion to ADR**: once the owner marks this document settled, G1–G9 become ADR-0052 (superseding the scope of ADR-0001 where they overlap) and the engineering consequences are triaged into follow-up ADRs, in particular the macro service (G4.20), the two-layer materialise/fold contract (G8), viewer-dependent replication (G7.8, G8.2), and mothership-scale interest management (G4).

