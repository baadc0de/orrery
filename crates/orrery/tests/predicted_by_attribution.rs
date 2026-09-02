//! `PredictedBy` is produced by the shipped composition (#910, D8/D10).
//!
//! `orrery_predict` owns the [`PredictedBy`] type and `orrery_authority` owns
//! the fact it records; authority is the lower layer of the two and may not
//! depend upward on prediction, so no member crate can write it and no member
//! crate's tests can prove it is written. Before this file every insertion in
//! the workspace was by hand — a fixture, an example, or a gate harness — which
//! is exactly the gap #910 reported: `ReconciliationMonitor` keys every
//! residual on `(authority, persist_id)`, and a shipped client whose entities
//! carry no marker feeds it nothing at all.
//!
//! So these tests go in at the facade and never touch the component. They push
//! registrar traffic into `LeaseInbox` — the same door `island_drain.rs` uses,
//! and the same one the gateway adapter uses in production — and read
//! `PredictedBy` back off the entity. Delete
//! [`OrreryAuthorityAttributionPlugin`] from the group and every test here
//! fails on a `None`.
//!
//! # The same-frame assertion, and what it does not prove
//!
//! Each test asserts after **one** `app.update()` following the message: the
//! marker settles in the frame authority settles, because a marker that lags a
//! handoff by a frame misattributes the residuals of the one frame anybody
//! cares about.
//!
//! It does **not** prove the `ApplyDeferred` the attribution plugin schedules
//! is what makes that true. Deleting the barrier and leaving the bare
//! `.after(process_lease_replies)` edge still passes all five, because
//! `OrreryAuthorityPlugin` chains its own `Update` systems and a sync point
//! therefore already follows `process_lease_replies` inside that chain. The
//! explicit barrier is there to stop this crate depending on that — a schedule
//! shape it does not own and is not ordered against — and no test here can
//! distinguish the two, since both orderings are legal and one is merely more
//! likely. Recorded so the barrier is not later deleted as dead weight on the
//! strength of a green run.

use bevy::prelude::*;
use bevy::MinimalPlugins;
use bevy_state::app::StatesPlugin;

use orrery::{OrreryClientPlugins, OrreryConfig};
use orrery_authority::{AuthorityPhase, AuthorityState, LeaseInbox, PersistIdentity};
use orrery_games::Skirmish;
use orrery_net::CoordinatorConfig;
use orrery_predict::PredictedBy;
use orrery_protocol::{
    ClaimId, ExpireDisposition, ExpireReason, LeaseId, LeaseMsg, NodeId, PersistId, SeqPair,
};

fn node_id(seed: u8) -> NodeId {
    let mut bytes = [0u8; 32];
    bytes[0] = seed;
    iroh::SecretKey::from_bytes(&bytes).public()
}

const LOCAL: u8 = 1;
const PEER: u8 = 2;

/// A headless client running the shipped group, with its node identity seeded.
///
/// The coordinator address is set and never dialled, for the reason
/// `island_drain.rs` documents: an unset address installs the no-coordinator
/// membership fallback, which is not what any of this is about.
fn client() -> App {
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
    // Both, and before the first frame: `client_group.rs` records that the two
    // systems carrying identity across are not ordered against each other, so a
    // first frame can read one before the other has written it — and the one
    // that wins here would otherwise stamp every marker with the zero key.
    app.world_mut()
        .resource_mut::<orrery_persist_client::GatewaySession>()
        .node = node_id(LOCAL);
    app.world_mut().resource_mut::<AuthorityState>().node = node_id(LOCAL);
    app.update();
    assert_eq!(
        app.world().resource::<AuthorityState>().node,
        node_id(LOCAL),
        "the fixture's identity survived the first frame"
    );
    app
}

/// An entity this peer replicates but has not claimed. Deliberately spawned
/// with no `PredictedBy`: producing it is the thing under test.
fn replicated(app: &mut App, persist: PersistId) -> Entity {
    app.world_mut()
        .spawn((PersistIdentity(persist), AuthorityPhase::Remote))
        .id()
}

fn grant(app: &mut App, persist: PersistId, lease_id: LeaseId) {
    app.world_mut()
        .resource_mut::<LeaseInbox>()
        .0
        .push(LeaseMsg::Grant {
            claim_id: ClaimId::REGISTRAR,
            entity: persist,
            lease_id,
            seq: SeqPair::default(),
            ttl_ms: 600_000,
            prev_holder: None,
        });
}

fn expire(app: &mut App, persist: PersistId, lease_id: LeaseId, disposition: ExpireDisposition) {
    app.world_mut()
        .resource_mut::<LeaseInbox>()
        .0
        .push(LeaseMsg::Expire {
            entity: persist,
            lease_id,
            last_holder: Some(node_id(LOCAL)),
            reason: ExpireReason::Revoked,
            disposition,
        });
}

fn marker(app: &App, entity: Entity) -> Option<PredictedBy> {
    app.world().get::<PredictedBy>(entity).copied()
}

/// A grant is the acquisition half of the acceptance criterion: authority
/// arrives, and the marker naming its holder arrives with it.
#[test]
fn a_registrar_grant_installs_the_marker_in_the_frame_it_settles() {
    const P: PersistId = PersistId::new(910);
    let mut app = client();
    let entity = replicated(&mut app, P);
    assert_eq!(
        marker(&app, entity),
        None,
        "the fixture inserts nothing; the entity starts unattributed"
    );

    grant(&mut app, P, LeaseId(1));
    app.update();

    assert_eq!(
        marker(&app, entity),
        Some(PredictedBy {
            authority: node_id(LOCAL),
            persist_id: P,
        }),
        "the holder is this peer, so the marker names this peer — the reading \
         the F-7 handoff fixture depends on"
    );
}

/// Inheritance: the registrar hands over an entity this peer never claimed.
/// `AuthorityEvent::Inherited` rather than `Granted`, same settled `Authority`,
/// same stamp — the marker is pinned to the authority record, not to which
/// event announced it.
#[test]
fn an_inherited_entity_is_attributed_without_ever_having_been_claimed() {
    const P: PersistId = PersistId::new(911);
    let mut app = client();
    let entity = replicated(&mut app, P);

    grant(&mut app, P, LeaseId(4));
    app.update();

    assert_eq!(
        marker(&app, entity).map(|held| held.authority),
        Some(node_id(LOCAL)),
    );
    assert_eq!(
        marker(&app, entity).map(|held| held.persist_id),
        Some(P),
        "the marker carries the cluster-minted id evidence references, not a \
         Bevy Entity"
    );
}

/// The loss half, with a successor: authority moves and the marker follows it
/// to the new holder rather than being dropped. A residual observed after the
/// handoff belongs to the successor's track, which is only true if the marker
/// repoints.
#[test]
fn losing_authority_to_a_successor_repoints_the_marker() {
    const P: PersistId = PersistId::new(912);
    let mut app = client();
    let entity = replicated(&mut app, P);

    grant(&mut app, P, LeaseId(1));
    app.update();
    assert_eq!(
        marker(&app, entity).map(|held| held.authority),
        Some(node_id(LOCAL)),
        "precondition: this peer holds it"
    );

    expire(
        &mut app,
        P,
        LeaseId(1),
        ExpireDisposition::Reassigned { to: node_id(PEER) },
    );
    app.update();

    assert_eq!(
        marker(&app, entity),
        Some(PredictedBy {
            authority: node_id(PEER),
            persist_id: P,
        }),
        "the marker names whoever writes the entity now, not whoever used to"
    );
}

/// The loss half with no successor. `Parked` and `Free` leave `holder: None`,
/// and there is no authority to attribute a residual to — so the marker is
/// removed rather than left naming a peer that has stopped writing the entity.
/// Leaving it would be worse than never writing it: the monitor would go on
/// charging a departed holder's track for corrections nobody caused.
#[test]
fn a_parked_entity_loses_its_marker_entirely() {
    const P: PersistId = PersistId::new(913);
    let mut app = client();
    let entity = replicated(&mut app, P);

    grant(&mut app, P, LeaseId(1));
    app.update();
    assert!(marker(&app, entity).is_some(), "precondition: attributed");

    expire(&mut app, P, LeaseId(1), ExpireDisposition::Parked);
    app.update();

    assert_eq!(
        marker(&app, entity),
        None,
        "a parked entity has no holder, so it has no attribution"
    );
}

/// A lease renewal re-inserts `Authority` with the same holder every heartbeat.
/// The stamp is guarded on the compared value, so the marker is not re-inserted
/// — `Changed<PredictedBy>` stays meaningful for anything downstream that wants
/// to react to an actual authority move.
#[test]
fn an_unchanged_holder_does_not_dirty_the_marker() {
    const P: PersistId = PersistId::new(914);
    let mut app = client();
    let entity = replicated(&mut app, P);

    grant(&mut app, P, LeaseId(1));
    app.update();
    let installed = app
        .world()
        .entity(entity)
        .get_ref::<PredictedBy>()
        .expect("the grant attributed it")
        .last_changed();

    // Two quiet frames: heartbeats and the lease clock run, the holder does not
    // move.
    app.update();
    app.update();

    let now = app
        .world()
        .entity(entity)
        .get_ref::<PredictedBy>()
        .expect("still attributed")
        .last_changed();
    assert_eq!(
        installed, now,
        "a holder that never moved must not dirty its marker every frame"
    );
}
