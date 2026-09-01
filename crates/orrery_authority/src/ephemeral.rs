//! Ephemeral entities: authority that never reaches the registrar (D7 §6).
//!
//! Projectiles, VFX and debris are the highest-frequency spawns in the game and
//! the least worth arbitrating: they exist for a second, they are never
//! persisted, and a durable consequence they cause travels the witness-attested
//! intent path rather than their own state. D7 therefore keeps them off the
//! lease registrar entirely — no `Claim`, no `Grant`, no fencing token, no
//! heartbeat. Putting a registrar round trip on the spawn path would price the
//! cheapest thing in the simulation at the cost of the most expensive one, and
//! would put a cluster RTT in front of the hit-registration story
//! (docs/05-prediction-rollback.md §7.1) that assumes firing is free.
//!
//! What replaces the registrar is *construction* rather than arbitration:
//!
//! - **Identity.** [`EphemeralId`] is island-scoped and partitioned by spawner,
//!   so any peer can mint one with no allocator and no round trip and two peers
//!   can never collide. There is nothing to serialize, so there is nothing to
//!   wait for.
//! - **Initial authority.** The spawner holds it, by construction (Fusion's
//!   spawner-gets-initial-authority rule). No claim is sent for a spawn.
//! - **Transfer.** An in-island [`IslandClaim`] under the *same* seq-pair
//!   comparison the registrar uses, resolved by every peer independently. With
//!   no arbiter present the comparison has to be a total order, so it borrows
//!   the degraded-mode tiebreak of D7 §4.4 verbatim: equal `(SeqPair,
//!   claim_tick)` is broken by the lowest `blake3(entity ‖ tick ‖ node ‖ island
//!   epoch)`. Raw lowest-`NodeId` is deliberately not the rule — NodeIds are
//!   self-generated keypairs, so an attacker could grind one that wins every
//!   contest offline.
//!
//! The type-level guarantee that this stays off the registrar is that nothing
//! in this module can reach [`crate::LeaseOutbox`]: ephemeral traffic is queued
//! on [`IslandOutbox`], and an ephemeral entity is marked
//! [`IslandAuthoritative`] rather than [`crate::LocallyAuthoritative`], which is
//! the only marker persistence will uplink.

use std::collections::BTreeMap;

use bevy_ecs::prelude::*;
use bevy_ecs::system::SystemParam;
use orrery_protocol::{IslandId, NodeId, SeqPair, Tick};
use serde::{Deserialize, Serialize};

/// Island-scoped identity for a non-persistent spawn (D7 §6).
///
/// The namespace is partitioned by spawner rather than allocated centrally,
/// which is the whole point: minting is a local increment, so a projectile
/// costs no round trip and works unchanged in the degraded mode of D7 §4.4
/// where there is no cluster to ask. `island` is carried so an id is
/// self-describing about the namespace it was minted in; uniqueness does not
/// depend on it, which is why an island merge needs no renumbering pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct EphemeralId {
    /// The island whose namespace this id was minted in.
    pub island: IslandId,
    /// The peer that spawned it.
    pub spawner: NodeId,
    /// Spawner-monotonic counter within the session.
    pub seq: u32,
}

impl EphemeralId {
    /// The ordering key. `NodeId` is an ed25519 public key, whose byte order is
    /// the only ordering every peer is guaranteed to agree on.
    fn key(&self) -> (u64, [u8; 32], u32) {
        (self.island.0, *self.spawner.as_bytes(), self.seq)
    }
}

impl Ord for EphemeralId {
    fn cmp(&self, other: &Self) -> core::cmp::Ordering {
        self.key().cmp(&other.key())
    }
}

impl PartialOrd for EphemeralId {
    fn partial_cmp(&self, other: &Self) -> Option<core::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// An in-island authority claim for an ephemeral entity.
///
/// This is the entire transfer protocol: one broadcast, no reply. Every peer
/// applies it through [`EphemeralRegistry::apply`] and reaches the same answer,
/// because the comparison is a total order over data all of them already have.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct IslandClaim {
    /// The ephemeral entity being claimed.
    pub entity: EphemeralId,
    /// The peer asserting authority.
    pub claimant: NodeId,
    /// The sequence pair the claimant is asserting, already incremented.
    pub seq: SeqPair,
    /// The tick the interaction happened on — the second key of the total
    /// order, so a peer that acted first wins a same-sequence contest.
    pub tick: Tick,
    /// The island manifest epoch the claimant held. It enters the tiebreak
    /// preimage so a winner cannot be predicted before the contest exists.
    pub epoch: u32,
}

impl IslandClaim {
    /// The deterministic tiebreak digest of D7 §4.4. Lowest wins.
    fn tiebreak(&self) -> [u8; 32] {
        let mut hasher = blake3::Hasher::new();
        hasher.update(&self.entity.island.0.to_le_bytes());
        hasher.update(self.entity.spawner.as_bytes());
        hasher.update(&self.entity.seq.to_le_bytes());
        hasher.update(&self.tick.0.to_le_bytes());
        hasher.update(self.claimant.as_bytes());
        hasher.update(&self.epoch.to_le_bytes());
        *hasher.finalize().as_bytes()
    }

    /// Whether this claim supersedes `known` under the in-island total order.
    ///
    /// Ordering is `SeqPair` first (the registrar's own rule, INV-3/INV-4),
    /// then the *earlier* tick, then the tiebreak digest. Earlier-tick-wins is
    /// the right sense at equal sequence: both claimants incremented from the
    /// same base, so neither had seen the other, and the interaction that
    /// actually happened first is the one every peer can agree happened first.
    #[must_use]
    pub fn supersedes(&self, known: &IslandClaim) -> bool {
        if self.seq != known.seq {
            return self.seq.supersedes(known.seq);
        }
        if self.tick != known.tick {
            return self.tick.0 < known.tick.0;
        }
        self.tiebreak() < known.tiebreak()
    }
}

/// What applying a remote [`IslandClaim`] did to local state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EphemeralOutcome {
    /// First this peer has heard of the entity; the claim is now the record.
    Recorded,
    /// The claim won and authority moved.
    Accepted {
        /// The peer that held it until now.
        from: NodeId,
    },
    /// The claim lost to what this peer already knew, and was discarded.
    Superseded,
}

/// One entity's in-island authority record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct EphemeralEntry {
    claim: IslandClaim,
}

/// The peer's view of who holds every ephemeral entity it knows about.
///
/// Deliberately not a lease table: there is no fencing token, no expiry and no
/// heartbeat, because there is no arbiter to fence against. Correctness rests
/// on every peer applying the same total order to the same broadcasts.
#[derive(Debug, Resource)]
pub struct EphemeralRegistry {
    node: NodeId,
    island: Option<IslandId>,
    epoch: u32,
    next_seq: u32,
    entries: BTreeMap<EphemeralId, EphemeralEntry>,
}

impl Default for EphemeralRegistry {
    fn default() -> Self {
        Self {
            node: NodeId::from_bytes(&[0; 32]).expect("zero node id is valid"),
            island: None,
            epoch: 0,
            next_seq: 0,
            entries: BTreeMap::new(),
        }
    }
}

impl EphemeralRegistry {
    /// Bind the registry to this peer's identity.
    pub fn set_node(&mut self, node: NodeId) {
        self.node = node;
    }

    /// The identity this registry mints and claims under.
    #[must_use]
    pub fn node(&self) -> NodeId {
        self.node
    }

    /// Adopt the island membership this peer currently has.
    ///
    /// Existing ids keep their original `island`: they are unique by spawner
    /// regardless, so a merge or an epoch bump costs nothing. Only the epoch
    /// used in *new* claims moves, which is what makes the tiebreak
    /// unpredictable across a topology change.
    pub fn set_island(&mut self, island: Option<IslandId>, epoch: u32) {
        self.island = island;
        self.epoch = epoch;
    }

    /// The island this peer mints into, if the coordinator has assigned one.
    #[must_use]
    pub fn island(&self) -> Option<IslandId> {
        self.island
    }

    /// Mint an id for a locally spawned ephemeral entity and take authority
    /// over it (spawner-gets-initial-authority). No message is produced: a
    /// spawn is not a transfer, and every peer learns the spawn from the
    /// replicated entity itself.
    ///
    /// `None` when this peer has no island assignment to mint into, or when the
    /// per-session counter is exhausted — both cases mean an id could not be
    /// guaranteed unique, and an ambiguous id is worse than no spawn.
    pub fn spawn(&mut self, tick: Tick) -> Option<EphemeralId> {
        let island = self.island?;
        let entity = EphemeralId {
            island,
            spawner: self.node,
            seq: self.next_seq,
        };
        self.next_seq = self.next_seq.checked_add(1)?;
        self.entries.insert(
            entity,
            EphemeralEntry {
                claim: IslandClaim {
                    entity,
                    claimant: self.node,
                    seq: SeqPair::default(),
                    tick,
                    epoch: self.epoch,
                },
            },
        );
        Some(entity)
    }

    /// Take weak authority over an ephemeral entity this peer did not spawn —
    /// the §6.1 mid-flight transfer, where a missile entering its target's
    /// contact island becomes the target's peer's problem to simulate.
    ///
    /// Applied locally *before* the claim goes out and with no reply expected:
    /// there is nothing to confirm it. `None` when this peer already holds it,
    /// or when the entity is unknown — claiming something never observed would
    /// invent an entity on every peer that trusted the claim.
    pub fn claim(&mut self, entity: EphemeralId, tick: Tick) -> Option<IslandClaim> {
        let known = self.entries.get(&entity)?.claim;
        if known.claimant == self.node {
            return None;
        }
        let claim = IslandClaim {
            entity,
            claimant: self.node,
            seq: SeqPair {
                own_seq: known.seq.own_seq,
                auth_seq: known.seq.auth_seq.checked_add(1)?,
            },
            tick,
            epoch: self.epoch,
        };
        self.entries.insert(entity, EphemeralEntry { claim });
        Some(claim)
    }

    /// Record an entity spawned by another peer, so a later claim for it has
    /// something to compare against.
    pub fn observe(&mut self, claim: IslandClaim) {
        self.entries
            .entry(claim.entity)
            .or_insert(EphemeralEntry { claim });
    }

    /// Apply a claim broadcast by another peer.
    pub fn apply(&mut self, claim: IslandClaim) -> EphemeralOutcome {
        match self.entries.get(&claim.entity).copied() {
            None => {
                self.entries.insert(claim.entity, EphemeralEntry { claim });
                EphemeralOutcome::Recorded
            }
            Some(known) if claim.supersedes(&known.claim) => {
                self.entries.insert(claim.entity, EphemeralEntry { claim });
                EphemeralOutcome::Accepted {
                    from: known.claim.claimant,
                }
            }
            Some(_) => EphemeralOutcome::Superseded,
        }
    }

    /// Forget an entity that has ceased to exist — a projectile that hit, an
    /// effect that finished. Nothing durable is written, which is the point.
    pub fn despawn(&mut self, entity: EphemeralId) {
        self.entries.remove(&entity);
    }

    /// The peer this registry believes writes `entity`.
    #[must_use]
    pub fn holder(&self, entity: EphemeralId) -> Option<NodeId> {
        self.entries.get(&entity).map(|entry| entry.claim.claimant)
    }

    /// Whether the local peer is the writer for `entity`.
    #[must_use]
    pub fn is_local(&self, entity: EphemeralId) -> bool {
        self.holder(entity) == Some(self.node)
    }

    /// The sequence pair currently associated with `entity`.
    #[must_use]
    pub fn seq(&self, entity: EphemeralId) -> Option<SeqPair> {
        self.entries.get(&entity).map(|entry| entry.claim.seq)
    }

    /// How many ephemeral entities this peer is tracking.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether this peer is tracking no ephemeral entities.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// In-island claims waiting for the island transport to broadcast them.
///
/// Deliberately a different queue from [`crate::LeaseOutbox`]: the gateway
/// adapter drains that one, and an ephemeral claim must never end up in it.
#[derive(Debug, Default, Resource)]
pub struct IslandOutbox(pub Vec<IslandClaim>);

/// In-island claims received from island peers.
#[derive(Debug, Default, Resource)]
pub struct IslandInbox(pub Vec<IslandClaim>);

/// The island identity of a non-persistent entity.
#[derive(Debug, Clone, Copy, Component)]
pub struct Ephemeral(pub EphemeralId);

/// This peer simulates and replicates the entity **in-island only**.
///
/// Distinct from [`crate::LocallyAuthoritative`] on purpose, and the reason the
/// D7 §6 rule is enforced rather than merely documented: persistence uplinks
/// key off that marker, so an ephemeral entity carrying this one can never be
/// persisted no matter what game code does with it.
#[derive(Debug, Clone, Copy, Component)]
pub struct IslandAuthoritative;

/// A change in who writes an ephemeral entity, visible to game code.
#[derive(Debug, Clone, Copy, Message, PartialEq, Eq)]
pub enum IslandAuthorityEvent {
    /// This peer took over an ephemeral entity another peer was simulating.
    Adopted {
        /// ECS entity now simulated locally.
        entity: Entity,
        /// The peer it was taken from.
        from: NodeId,
    },
    /// Another peer's claim superseded this peer's authority.
    Yielded {
        /// ECS entity no longer simulated locally.
        entity: Entity,
        /// The peer that now writes it.
        to: NodeId,
    },
}

/// Ergonomic command interface for spawning and claiming ephemeral entities.
///
/// The counterpart to [`crate::LeaseClient`], and deliberately a *separate*
/// one: there is no claim correlation, no fencing token and no reply, so a
/// shared interface would have to make all three optional and would invite the
/// mistake this crate exists to prevent — an ephemeral entity acquiring the
/// persistence uplink marker.
#[derive(SystemParam)]
pub struct IslandClient<'w, 's> {
    commands: Commands<'w, 's>,
    registry: ResMut<'w, EphemeralRegistry>,
    outbox: ResMut<'w, IslandOutbox>,
}

impl IslandClient<'_, '_> {
    /// Spawn a locally authored ephemeral entity onto an existing ECS entity.
    ///
    /// No message is emitted: spawner-gets-initial-authority means the spawn
    /// *is* the claim, and peers learn it from the replicated entity.
    pub fn spawn(&mut self, entity: Entity, tick: Tick) -> Option<EphemeralId> {
        let id = self.registry.spawn(tick)?;
        self.commands
            .entity(entity)
            .insert((Ephemeral(id), IslandAuthoritative));
        Some(id)
    }

    /// Take an ephemeral entity another peer is simulating, and broadcast it.
    ///
    /// Local authority is installed immediately, before the broadcast leaves:
    /// there is no arbiter to confirm it, so waiting would only stall the
    /// terminal phase of whatever just made contact.
    pub fn claim(&mut self, entity: Entity, id: EphemeralId, tick: Tick) -> Option<IslandClaim> {
        let claim = self.registry.claim(id, tick)?;
        self.commands.entity(entity).insert(IslandAuthoritative);
        self.outbox.0.push(claim);
        Some(claim)
    }

    /// Forget an ephemeral entity that ceased to exist.
    pub fn despawn(&mut self, id: EphemeralId) {
        self.registry.despawn(id);
    }
}

/// Apply received in-island claims to ECS state.
///
/// The local registry is authoritative for the *decision*; the ECS markers only
/// follow it. A claim this peer already lost to is dropped without touching the
/// world, so a duplicated or reordered broadcast is a no-op — the same property
/// `process_lease_replies` gets from the fencing token, obtained here from the
/// total order instead.
pub fn process_island_claims(
    mut commands: Commands,
    mut inbox: ResMut<IslandInbox>,
    mut registry: ResMut<EphemeralRegistry>,
    entities: Query<(Entity, &Ephemeral)>,
    mut events: MessageWriter<IslandAuthorityEvent>,
) {
    let local = registry.node;
    for claim in std::mem::take(&mut inbox.0) {
        let outcome = registry.apply(claim);
        let EphemeralOutcome::Accepted { from } = outcome else {
            continue;
        };
        let Some((entity_ref, _)) = entities.iter().find(|(_, id)| id.0 == claim.entity) else {
            continue;
        };
        if claim.claimant == local {
            commands.entity(entity_ref).insert(IslandAuthoritative);
            events.write(IslandAuthorityEvent::Adopted {
                entity: entity_ref,
                from,
            });
        } else if from == local {
            commands.entity(entity_ref).remove::<IslandAuthoritative>();
            events.write(IslandAuthorityEvent::Yielded {
                entity: entity_ref,
                to: claim.claimant,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{LeaseOutbox, LocallyAuthoritative, OrreryAuthorityPlugin};
    use bevy_app::App;

    fn node_id(seed: u8) -> NodeId {
        let mut bytes = [0u8; 32];
        bytes[0] = seed;
        iroh_base::SecretKey::from_bytes(&bytes).public()
    }

    /// An app whose island binding and identity have already reached the
    /// ephemeral registry, which is what one `update` past plugin build gets
    /// a real client.
    fn island_app(seed: u8) -> App {
        let mut app = App::new();
        app.add_plugins(OrreryAuthorityPlugin::default());
        app.world_mut().resource_mut::<crate::AuthorityState>().node = node_id(seed);
        *app.world_mut().resource_mut::<crate::IslandBinding>() = crate::IslandBinding {
            island: Some(IslandId::new(3)),
            epoch: 11,
        };
        app.update();
        app
    }

    fn registry(seed: u8) -> EphemeralRegistry {
        let mut registry = EphemeralRegistry::default();
        registry.set_node(node_id(seed));
        registry.set_island(Some(IslandId::new(3)), 11);
        registry
    }

    #[test]
    fn spawning_a_projectile_queues_no_registrar_traffic() {
        // The failure this catches: routing ephemeral spawns through the lease
        // registrar, which puts a cluster round trip on the firing path.
        let mut app = island_app(2);
        let projectile = app
            .world_mut()
            .resource_mut::<EphemeralRegistry>()
            .spawn(Tick::new(120))
            .expect("an island-assigned peer can mint");
        app.update();

        assert!(app
            .world()
            .resource::<EphemeralRegistry>()
            .is_local(projectile));
        assert!(
            app.world().resource::<LeaseOutbox>().0.is_empty(),
            "an ephemeral spawn must never reach the lease registrar"
        );
        assert!(app.world().resource::<IslandOutbox>().0.is_empty());
    }

    #[test]
    fn an_ephemeral_entity_never_gets_the_persistence_uplink_marker() {
        // The failure this catches: reusing `LocallyAuthoritative` for
        // ephemeral authority, which would let a projectile be uplinked and
        // persisted.
        let mut app = island_app(2);
        let id = app
            .world_mut()
            .resource_mut::<EphemeralRegistry>()
            .spawn(Tick::new(1))
            .expect("mint");
        let entity = app
            .world_mut()
            .spawn((Ephemeral(id), IslandAuthoritative))
            .id();

        // A remote peer takes it mid-flight, then hands it back.
        app.world_mut()
            .resource_mut::<IslandInbox>()
            .0
            .push(IslandClaim {
                entity: id,
                claimant: node_id(9),
                seq: SeqPair {
                    own_seq: 0,
                    auth_seq: 1,
                },
                tick: Tick::new(4),
                epoch: 11,
            });
        app.update();

        assert!(app.world().get::<IslandAuthoritative>(entity).is_none());
        assert!(
            app.world().get::<LocallyAuthoritative>(entity).is_none(),
            "no ephemeral path may ever install the persistence uplink marker"
        );
    }

    #[test]
    fn a_mid_flight_transfer_resolves_with_no_grant_round_trip() {
        // The failure this catches: an ephemeral claim that waits for a reply.
        // The claimant must be simulating the missile on the same tick it
        // decided to, or the terminal phase stutters for a cluster RTT.
        let mut local = registry(2);
        let remote_spawn = IslandClaim {
            entity: EphemeralId {
                island: IslandId::new(3),
                spawner: node_id(9),
                seq: 0,
            },
            claimant: node_id(9),
            seq: SeqPair::default(),
            tick: Tick::new(10),
            epoch: 11,
        };
        local.observe(remote_spawn);

        let claim = local
            .claim(remote_spawn.entity, Tick::new(12))
            .expect("a peer may take an ephemeral entity it did not spawn");

        assert!(local.is_local(remote_spawn.entity));
        assert_eq!(
            claim.seq,
            SeqPair {
                own_seq: 0,
                auth_seq: 1
            }
        );
    }

    #[test]
    fn a_stale_ephemeral_claim_cannot_take_authority_back() {
        // The failure this catches: applying in-island claims in arrival order
        // rather than by the total order, so a delayed broadcast reinstates a
        // superseded writer.
        let mut peer = registry(2);
        let entity = EphemeralId {
            island: IslandId::new(3),
            spawner: node_id(9),
            seq: 7,
        };
        let older = IslandClaim {
            entity,
            claimant: node_id(9),
            seq: SeqPair {
                own_seq: 0,
                auth_seq: 1,
            },
            tick: Tick::new(10),
            epoch: 11,
        };
        let newer = IslandClaim {
            entity,
            claimant: node_id(4),
            seq: SeqPair {
                own_seq: 0,
                auth_seq: 2,
            },
            tick: Tick::new(11),
            epoch: 11,
        };

        assert_eq!(peer.apply(older), EphemeralOutcome::Recorded);
        assert_eq!(
            peer.apply(newer),
            EphemeralOutcome::Accepted { from: node_id(9) }
        );
        assert_eq!(peer.apply(older), EphemeralOutcome::Superseded);
        assert_eq!(peer.holder(entity), Some(node_id(4)));
    }

    #[test]
    fn simultaneous_ephemeral_claims_converge_on_the_same_winner_everywhere() {
        // The failure this catches: an in-island tiebreak that is not a total
        // order, which splits an island into two peers each simulating the same
        // projectile and each believing it won.
        let entity = EphemeralId {
            island: IslandId::new(3),
            spawner: node_id(1),
            seq: 5,
        };
        let contenders: Vec<IslandClaim> = [4u8, 7, 13, 21]
            .into_iter()
            .map(|seed| IslandClaim {
                entity,
                claimant: node_id(seed),
                seq: SeqPair {
                    own_seq: 0,
                    auth_seq: 1,
                },
                tick: Tick::new(90),
                epoch: 11,
            })
            .collect();

        // Every arrival order, on every peer, has to end at one holder.
        let mut winners = std::collections::BTreeSet::new();
        for rotation in 0..contenders.len() {
            for observer in [2u8, 8] {
                let mut peer = registry(observer);
                for offset in 0..contenders.len() {
                    peer.apply(contenders[(rotation + offset) % contenders.len()]);
                }
                winners.insert(*peer.holder(entity).expect("a holder").as_bytes());
            }
        }
        assert_eq!(
            winners.len(),
            1,
            "in-island arbitration must converge without an arbiter"
        );
    }

    #[test]
    fn the_tiebreak_is_not_simply_the_lowest_node_id() {
        // The failure this catches: tiebreaking on raw NodeId order, which lets
        // an attacker grind a low-sorting keypair offline and win every
        // uncontested-sequence contest forever (D7 §4.4).
        let entity = EphemeralId {
            island: IslandId::new(3),
            spawner: node_id(1),
            seq: 5,
        };
        let claim = |seed: u8, tick: u64| IslandClaim {
            entity,
            claimant: node_id(seed),
            seq: SeqPair {
                own_seq: 0,
                auth_seq: 1,
            },
            tick: Tick::new(tick),
            epoch: 11,
        };
        // The same pair of claimants, contesting on different ticks: if node id
        // order decided, one of them would win both.
        let mut lowest_node_won = Vec::new();
        for tick in 1..40u64 {
            let a = claim(3, tick);
            let b = claim(200, tick);
            lowest_node_won.push(a.supersedes(&b));
        }
        assert!(
            lowest_node_won.iter().any(|won| *won) && lowest_node_won.iter().any(|won| !*won),
            "the tiebreak must not reduce to node id order"
        );
    }

    #[test]
    fn a_peer_with_no_island_assignment_cannot_mint_an_id() {
        // The failure this catches: minting into a namespace the coordinator
        // has not handed out, which produces ids two peers can both believe
        // they own.
        let mut orphan = EphemeralRegistry::default();
        orphan.set_node(node_id(2));
        assert!(orphan.spawn(Tick::new(1)).is_none());
        assert!(orphan.is_empty());
    }
}
