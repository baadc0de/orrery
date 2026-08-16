//! End-to-end checks that the D7 §5 and §6 paths are wired, not merely
//! implemented.
//!
//! The unit tests in `orrery_authority::contact` and
//! `orrery_authority::ephemeral` exercise the planner and the in-island total
//! order directly. These drive the shipping plugin instead, because most of the
//! ways these two features can be wrong are wiring failures: a planner that is
//! never run, a burst that never reaches the outbox, an ephemeral claim that
//! ends up in the *registrar's* outbox, or a denial that never reaches the
//! back-off state.

use bevy_app::App;
use bevy_ecs::prelude::*;
use bevy_ecs::system::RunSystemOnce;
use orrery_authority::contact::{
    ContactBody, ContactNode, ContactObservations, ContactStatus, ContactTick,
};
use orrery_authority::ephemeral::{
    Ephemeral, EphemeralId, EphemeralRegistry, IslandAuthoritative, IslandClaim, IslandClient,
    IslandInbox, IslandOutbox,
};
use orrery_authority::{
    AuthorityState, IslandBinding, LeaseInbox, LeaseOutbox, OrreryAuthorityPlugin, PersistIdentity,
};
use orrery_protocol::{
    CellId, ClaimBasis, ClaimKind, DenyReason, GridId, IslandId, LeaseMsg, NodeId, SeqPair, Tick,
};

fn node_id(seed: u8) -> NodeId {
    let mut bytes = [0u8; 32];
    bytes[0] = seed;
    iroh_base::SecretKey::from_bytes(&bytes).public()
}

/// A peer whose identity and island assignment have already propagated.
fn peer(seed: u8) -> App {
    let mut app = App::new();
    app.add_plugins(OrreryAuthorityPlugin);
    app.world_mut().resource_mut::<AuthorityState>().node = node_id(seed);
    *app.world_mut().resource_mut::<IslandBinding>() = IslandBinding {
        island: Some(IslandId::new(3)),
        epoch: 11,
    };
    app.update();
    app
}

fn body(node: ContactNode, status: ContactStatus) -> ContactBody {
    ContactBody {
        node,
        grid: GridId::ROOT,
        cell: CellId::ROOT,
        observed: SeqPair::default(),
        status,
    }
}

/// Publish the universe tick the solver stepped, which is the half of
/// [`ContactTick`] the host owns. `now_ms` is deliberately not written here:
/// the plugin drives it, and a test that set it by hand would be checking a
/// clock no shipping app has.
fn set_tick(app: &mut App, tick: u64) {
    app.world_mut().resource_mut::<ContactTick>().tick = Tick::new(tick);
}

/// Spawn `count` persistent bodies in a chain, the first one already held.
fn pile(app: &mut App, count: u64) -> Vec<Entity> {
    (0..count)
        .map(|index| {
            app.world_mut()
                .spawn(PersistIdentity(orrery_protocol::PersistId::new(index)))
                .id()
        })
        .collect()
}

fn report_chain(app: &mut App, count: u64) {
    let mut observations = app.world_mut().resource_mut::<ContactObservations>();
    observations.observe(body(
        ContactNode::Persistent(orrery_protocol::PersistId::new(0)),
        ContactStatus::Held,
    ));
    for index in 1..count {
        observations.observe(body(
            ContactNode::Persistent(orrery_protocol::PersistId::new(index)),
            ContactStatus::Claimable,
        ));
        observations.touch(
            ContactNode::Persistent(orrery_protocol::PersistId::new(index - 1)),
            ContactNode::Persistent(orrery_protocol::PersistId::new(index)),
        );
    }
}

#[test]
fn a_contact_report_becomes_weak_claims_stamped_with_the_tick_contact_happened_on() {
    // The failure this catches: a propagation planner that nothing runs, or one
    // whose output never reaches the gateway outbox — the shape in which "we
    // implemented contact propagation" is true and no claim is ever sent. The
    // tick matters on its own: `ClaimBasis::Contact{tick}` is the evidence the
    // registrar's plausibility gate and the D9 input log both read, so a claim
    // stamped with the wrong tick is unfalsifiable evidence.
    let mut app = peer(2);
    let _entities = pile(&mut app, 4);
    set_tick(&mut app, 1_234);
    report_chain(&mut app, 4);

    app.update();

    let claims: Vec<_> = app
        .world()
        .resource::<LeaseOutbox>()
        .0
        .iter()
        .filter_map(|message| match message {
            LeaseMsg::Claim {
                entity,
                kind,
                basis,
                tick,
                ..
            } => Some((*entity, *kind, *basis, *tick)),
            _ => None,
        })
        .collect();

    assert_eq!(claims.len(), 3, "the whole chain must be claimed");
    for (index, (entity, kind, basis, tick)) in claims.iter().enumerate() {
        assert_eq!(*entity, orrery_protocol::PersistId::new(index as u64 + 1));
        assert_eq!(*kind, ClaimKind::Weak);
        assert_eq!(
            *basis,
            ClaimBasis::Contact {
                tick: Tick::new(1_234)
            }
        );
        assert_eq!(*tick, Tick::new(1_234));
    }
}

#[test]
fn a_registrar_denial_stops_the_next_ticks_reclaim_of_the_same_body() {
    // The failure this catches: the planner never learning what the registrar
    // answered, so a refused body is re-claimed on every subsequent contact
    // report. That is not merely wasteful — D7 §10 routes sustained claim
    // pressure at the witness/strike pipeline, so the bug makes an honest
    // client look like a griefer.
    let mut app = peer(2);
    let entities = pile(&mut app, 2);
    set_tick(&mut app, 10);
    report_chain(&mut app, 2);
    app.update();

    let claim_id = app
        .world()
        .resource::<LeaseOutbox>()
        .0
        .iter()
        .find_map(|message| match message {
            LeaseMsg::Claim { claim_id, .. } => Some(*claim_id),
            _ => None,
        })
        .expect("the contact produced a claim");
    app.world_mut().resource_mut::<LeaseOutbox>().0.clear();

    app.world_mut()
        .resource_mut::<LeaseInbox>()
        .0
        .push(LeaseMsg::Deny {
            claim_id: Some(claim_id),
            entity: orrery_protocol::PersistId::new(1),
            reason: DenyReason::Held {
                holder: node_id(9),
                seq: SeqPair {
                    own_seq: 0,
                    auth_seq: 4,
                },
            },
            retry_after_ms: 250,
        });
    set_tick(&mut app, 11);
    report_chain(&mut app, 2);
    app.update();
    assert!(
        app.world().resource::<LeaseOutbox>().0.is_empty(),
        "a body refused this tick must not be re-claimed on the next one"
    );

    // Still contested a quarter-second later, and now the back-off has lapsed.
    // Real time, because the plugin's own driver is what the planner reads.
    std::thread::sleep(std::time::Duration::from_millis(300));
    set_tick(&mut app, 30);
    report_chain(&mut app, 2);
    app.update();
    assert!(
        !app.world().resource::<LeaseOutbox>().0.is_empty(),
        "back-off must expire, or a contested body is abandoned forever"
    );
    let _ = entities;
}

#[test]
fn a_projectile_impact_claims_the_crate_and_keeps_the_projectile_off_the_registrar() {
    // The failure this catches: the two entity classes routed through one code
    // path. Either the projectile ends up in a `Claim` to the registrar — the
    // per-shot round trip D7 §6 exists to prevent — or the crate it hit is
    // treated as ephemeral and its authority change is never durable.
    let mut app = peer(2);
    let projectile_id = app
        .world_mut()
        .resource_mut::<EphemeralRegistry>()
        .spawn(Tick::new(1))
        .expect("an island-assigned peer can mint");
    app.world_mut()
        .spawn((Ephemeral(projectile_id), IslandAuthoritative));
    let crate_entity = app
        .world_mut()
        .spawn(PersistIdentity(orrery_protocol::PersistId::new(77)))
        .id();

    // The crate is the *held* body and the projectile is what it touched: a
    // guided missile entering its target's contact island (D7 §6.1).
    let incoming = EphemeralId {
        island: IslandId::new(3),
        spawner: node_id(9),
        seq: 4,
    };
    app.world_mut()
        .resource_mut::<EphemeralRegistry>()
        .observe(IslandClaim {
            entity: incoming,
            claimant: node_id(9),
            seq: SeqPair::default(),
            tick: Tick::new(2),
            epoch: 11,
        });
    app.world_mut().spawn(Ephemeral(incoming));

    set_tick(&mut app, 60);
    {
        let mut observations = app.world_mut().resource_mut::<ContactObservations>();
        observations.observe(body(
            ContactNode::Persistent(orrery_protocol::PersistId::new(77)),
            ContactStatus::Held,
        ));
        observations.observe(body(
            ContactNode::Ephemeral(incoming),
            ContactStatus::Claimable,
        ));
        observations.touch(
            ContactNode::Persistent(orrery_protocol::PersistId::new(77)),
            ContactNode::Ephemeral(incoming),
        );
    }
    app.update();

    assert!(
        app.world()
            .resource::<LeaseOutbox>()
            .0
            .iter()
            .all(|message| !matches!(message, LeaseMsg::Claim { .. })),
        "an ephemeral body must never produce a registrar claim"
    );
    let island: Vec<_> = app.world().resource::<IslandOutbox>().0.clone();
    assert_eq!(island.len(), 1);
    assert_eq!(island[0].entity, incoming);
    assert_eq!(island[0].claimant, node_id(2));
    assert_eq!(
        island[0].seq,
        SeqPair {
            own_seq: 0,
            auth_seq: 1
        }
    );
    assert!(app
        .world()
        .resource::<EphemeralRegistry>()
        .is_local(incoming));
    let _ = crate_entity;
}

#[test]
fn a_thrown_ephemeral_reaches_its_catcher_on_both_peers_without_an_arbiter() {
    // The cooperative-handoff chain of the P3 demo, on the class of entity that
    // has no registrar to arbitrate it. The failure this catches: an in-island
    // claim applied only by the claimant, so the thrower keeps simulating a
    // body the catcher also simulates — the double-writer INV-1 forbids, with
    // no lease to detect it.
    let mut thrower = peer(2);
    let mut catcher = peer(5);

    let thrower_view = thrower.world_mut().spawn_empty().id();
    let ball = thrower
        .world_mut()
        .run_system_once(move |mut client: IslandClient| client.spawn(thrower_view, Tick::new(1)))
        .expect("the spawn system runs")
        .expect("an island-assigned peer can mint");
    let spawn_claim = IslandClaim {
        entity: ball,
        claimant: node_id(2),
        seq: SeqPair::default(),
        tick: Tick::new(1),
        epoch: 11,
    };
    catcher
        .world_mut()
        .resource_mut::<EphemeralRegistry>()
        .observe(spawn_claim);
    let catcher_view = catcher.world_mut().spawn(Ephemeral(ball)).id();

    // The catcher's peer claims it on contact and broadcasts once.
    let claim = catcher
        .world_mut()
        .run_system_once(move |mut client: IslandClient| {
            client.claim(catcher_view, ball, Tick::new(40))
        })
        .expect("the claim system runs")
        .expect("the catcher may take an ephemeral it did not spawn");
    catcher.update();
    assert_eq!(
        catcher.world().resource::<IslandOutbox>().0,
        vec![claim],
        "the claim must be broadcast, or no other peer ever yields"
    );
    thrower
        .world_mut()
        .resource_mut::<IslandInbox>()
        .0
        .push(claim);
    thrower.update();

    assert_eq!(
        thrower.world().resource::<EphemeralRegistry>().holder(ball),
        Some(node_id(5))
    );
    assert_eq!(
        catcher.world().resource::<EphemeralRegistry>().holder(ball),
        Some(node_id(5))
    );
    assert!(
        thrower
            .world()
            .get::<IslandAuthoritative>(thrower_view)
            .is_none(),
        "the thrower must stop writing the moment the claim supersedes it"
    );
    assert!(catcher
        .world()
        .get::<IslandAuthoritative>(catcher_view)
        .is_some());
}
