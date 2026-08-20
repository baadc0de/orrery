//! The drain wire, checked against a real `App` (D24, issue #113).
//!
//! `crates/orrery_net` proves what a drain order does to membership, and
//! `crates/orrery_authority` proves what a `Divest` does to a held lease.
//! Neither can prove the thing that was actually missing: that an order landing
//! on the coordinator session releases the leases and closes the ephemeral
//! namespace, in one frame, in the composition a game ships. The two halves
//! live in crates that may not name each other — `orrery_authority` is the
//! lower layer and carries no dependency on `orrery_net` — so this facade is
//! the only place the round trip exists to be tested.
//!
//! # The property these tests are guarding
//!
//! D24 §(b) makes the order *advisory*: the coordinator sends it only if the
//! departing peer's session happens to still be open, and on the departure mode
//! a drain most needs to survive — a crash — there is no session and no notice.
//! The registrar's 1 s expiry sweep parks every row within `TTL + S = 11 s`
//! regardless. So everything here is a latency optimisation over a backstop
//! that is already correct, and
//! [`a_peer_that_never_hears_the_order_is_left_exactly_as_it_was`] is the test
//! that says so in code rather than in prose.

use bevy::prelude::*;
use bevy::MinimalPlugins;
use bevy_state::app::StatesPlugin;

use orrery::{OrreryClientPlugins, OrreryConfig};
use orrery_authority::ephemeral::EphemeralRegistry;
use orrery_authority::{
    AuthorityPhase, AuthorityState, IslandBinding, LeaseInbox, LeaseOutbox, LocallyAuthoritative,
    PersistIdentity,
};
use orrery_games::Skirmish;
use orrery_net::island::IslandMembership;
use orrery_net::{CoordinatorConfig, CoordinatorLink};
use orrery_protocol::coord::{IslandManifest, PeerEntry, TopologyRegime};
use orrery_protocol::{
    CellId, ClaimId, IslandId, LeaseId, LeaseMsg, NodeId, PersistId, SeqPair, Tick,
};

const ISLAND: IslandId = IslandId::new(42);

fn node_id(seed: u8) -> NodeId {
    let mut bytes = [0u8; 32];
    bytes[0] = seed;
    iroh::SecretKey::from_bytes(&bytes).public()
}

/// A headless client holding island `ISLAND`, with `held` leases granted.
///
/// The coordinator address is set — and never dialled — so that
/// `follow_sessions_without_coordinator` is not installed: the no-coordinator
/// fallback re-derives membership from the connected-session set every frame,
/// which would put the island back the frame after the drain took it away and
/// would be testing the fallback rather than the drain.
///
/// Identity is seeded on both `GatewaySession` and `AuthorityState`, for the
/// reason `client_group.rs` documents: the two systems that carry it across are
/// not ordered against each other, so a first frame can read one before the
/// other has written it.
fn client_holding(held: &[PersistId]) -> (App, Vec<Entity>) {
    let config = OrreryConfig::default().with_coordinator(CoordinatorConfig {
        address: Some(iroh::EndpointAddr::new(node_id(3))),
        ..CoordinatorConfig::default()
    });
    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    app.add_plugins(StatesPlugin);
    app.add_plugins(OrreryClientPlugins::<Skirmish>::new(config));
    // lightyear's replication backend builds its channel map in `finish`, and a
    // frame run without it panics inside `receive_client_packets`.
    app.finish();

    let local = node_id(1);
    app.world_mut()
        .resource_mut::<orrery_persist_client::GatewaySession>()
        .node = local;
    app.world_mut().resource_mut::<AuthorityState>().node = local;

    let manifest = IslandManifest {
        island: ISLAND,
        epoch: 7,
        cells: vec![CellId::ROOT],
        regime: TopologyRegime::Mesh,
        peers: vec![
            PeerEntry {
                node: local,
                cells: vec![CellId::ROOT],
            },
            PeerEntry {
                node: node_id(2),
                cells: vec![CellId::ROOT],
            },
        ],
    };
    app.world_mut()
        .resource_mut::<IslandMembership>()
        .apply_manifest(&manifest, local)
        .expect("the first manifest is never stale");
    app.update();

    // Grants rather than direct state surgery: `AuthorityState`'s lease map is
    // private, and going in the front door is also what installs the `Authority`
    // component whose `seq` the divestiture has to carry forward.
    let entities: Vec<Entity> = held
        .iter()
        .map(|persist| {
            app.world_mut()
                .spawn((PersistIdentity(*persist), AuthorityPhase::Remote))
                .id()
        })
        .collect();
    for (index, persist) in held.iter().enumerate() {
        app.world_mut()
            .resource_mut::<LeaseInbox>()
            .0
            .push(LeaseMsg::Grant {
                claim_id: ClaimId::REGISTRAR,
                entity: *persist,
                lease_id: LeaseId(index as u64 + 1),
                seq: SeqPair::default(),
                ttl_ms: 600_000,
                prev_holder: None,
            });
    }
    app.update();
    for entity in &entities {
        assert!(
            app.world().get::<LocallyAuthoritative>(*entity).is_some(),
            "the fixture holds the lease it is about to divest"
        );
    }

    // The frames above queued this peer's own control traffic. The assertions
    // are about what the *drain* frame emits, so the outbox starts empty.
    app.world_mut().resource_mut::<LeaseOutbox>().0.clear();
    (app, entities)
}

/// Every `Divest` in the outbox, as `(entity, successor)`.
fn divestitures(app: &App) -> Vec<(PersistId, Option<NodeId>)> {
    app.world()
        .resource::<LeaseOutbox>()
        .0
        .iter()
        .filter_map(|message| match message {
            LeaseMsg::Divest { entity, to, .. } => Some((*entity, *to)),
            _ => None,
        })
        .collect()
}

#[test]
fn a_drain_releases_every_held_lease_and_closes_the_island_in_one_frame() {
    // The whole wire, end to end. One frame, because the deadline D24 §(d)
    // stamps is one lease TTL away and a drain that took a frame per hop would
    // be spending that budget on scheduling.
    let (mut app, _entities) = client_holding(&[PersistId::new(61), PersistId::new(62)]);
    app.world_mut().resource_mut::<CoordinatorLink>().drain = Some((ISLAND, 10_000));

    app.update();

    // One `Divest { to: None }` per held entity: the sanctioned cooperative
    // release (D7 §5). `to: None` because the island is being retired rather
    // than handed over — naming a successor would be an evacuation, which
    // D24 §(b) declines to invent.
    let mut divested = divestitures(&app);
    divested.sort_by_key(|(entity, _)| *entity);
    assert_eq!(
        divested,
        vec![(PersistId::new(61), None), (PersistId::new(62), None),],
        "one release per held lease, and no successor"
    );

    // And the same frame closed the namespace, through the existing facade
    // wire: `leave()` cleared `IslandMembership`, `bind_island_membership`
    // carried that into `IslandBinding`, and `track_island_binding` carried it
    // into the registry.
    assert_eq!(app.world().resource::<IslandBinding>().island, None);
    assert!(!app.world().resource::<IslandMembership>().is_member());
    assert_eq!(app.world().resource::<EphemeralRegistry>().island(), None);

    // The order is consumed, so a redialled session — which keeps membership
    // across the drop by design — does not drain again.
    assert!(app.world().resource::<CoordinatorLink>().drain.is_none());
}

#[test]
fn a_drained_peer_cannot_mint_ephemeral_ids() {
    // `EphemeralRegistry::spawn` bails on its first line without an island
    // (`let island = self.island?`), which is exactly the guard the island wire
    // exists to feed. A peer that went on minting after its island was retired
    // would be issuing ids into a namespace nobody else recognises.
    let (mut app, _entities) = client_holding(&[PersistId::new(63)]);
    assert!(
        app.world_mut()
            .resource_mut::<EphemeralRegistry>()
            .spawn(Tick::new(1))
            .is_some(),
        "a peer with an island assignment mints before the drain"
    );

    app.world_mut().resource_mut::<CoordinatorLink>().drain = Some((ISLAND, 10_000));
    app.update();

    assert!(
        app.world_mut()
            .resource_mut::<EphemeralRegistry>()
            .spawn(Tick::new(2))
            .is_none(),
        "the namespace is closed once the island is drained"
    );
}

#[test]
fn a_drain_naming_another_island_releases_nothing() {
    // `CoordMsg` rides unreliable datagrams, so a duplicate or a straggler
    // naming an island this peer has since left is the expected case, not the
    // anomalous one. Acting on it would release leases this peer is still the
    // legitimate writer for — and leave the entities unowned until the TTL runs
    // out, which is the outage the fencing token exists to prevent.
    let (mut app, entities) = client_holding(&[PersistId::new(64)]);
    app.world_mut().resource_mut::<CoordinatorLink>().drain = Some((IslandId::new(7), 10_000));

    app.update();

    assert!(divestitures(&app).is_empty(), "nothing was released");
    assert!(app
        .world()
        .get::<LocallyAuthoritative>(entities[0])
        .is_some());
    assert_eq!(app.world().resource::<IslandBinding>().island, Some(ISLAND));
    assert!(
        app.world().resource::<CoordinatorLink>().drain.is_none(),
        "and the unusable order is dropped rather than kept"
    );
}

#[test]
fn a_peer_that_never_hears_the_order_is_left_exactly_as_it_was() {
    // The advisory property (D24 §(b)), at the only layer that can hold it.
    //
    // A crashed peer has no session to be told on, and the registrar's expiry
    // sweep drains the island regardless within `TTL + S = 11 s`. So the
    // undelivered case must be indistinguishable, on this side, from no order
    // at all: no partial release, no half-left island, nothing a later system
    // could read as "a drain is in progress". If honouring the order were
    // load-bearing rather than an optimisation, this is the test that would
    // have to assert something *changed* — and it deliberately cannot.
    //
    // What is out of reach here, and is said plainly rather than faked: this
    // process cannot assert that the island still drains. That happens in
    // `orrery_persistd`'s `sweep_expired_leases`, in another process, against a
    // wall clock this test does not advance — and it is proved where it lives.
    // The half that *is* in scope is that the peer does nothing on its own.
    let (mut app, entities) = client_holding(&[PersistId::new(65), PersistId::new(66)]);
    assert!(app.world().resource::<CoordinatorLink>().drain.is_none());

    for _ in 0..8 {
        app.update();
    }

    assert!(
        divestitures(&app).is_empty(),
        "no order, no release: the peer keeps writing until the registrar says otherwise"
    );
    for entity in &entities {
        assert!(app.world().get::<LocallyAuthoritative>(*entity).is_some());
    }
    assert_eq!(app.world().resource::<IslandBinding>().island, Some(ISLAND));
    assert!(app.world().resource::<IslandMembership>().is_member());
    assert_eq!(
        app.world().resource::<EphemeralRegistry>().island(),
        Some(ISLAND),
        "and the namespace stays open, because nothing has retired the island"
    );
}
