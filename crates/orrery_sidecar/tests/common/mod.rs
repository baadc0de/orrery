//! Shared fixture: a running sidecar that holds its one entity.
//!
//! Shared by two integration binaries, each of which uses part of it; the
//! allow is the usual cost of a `tests/common` module rather than a signal.

#![allow(dead_code)]

use bevy::prelude::*;
use bevy::time::TimeUpdateStrategy;
use lightyear::prelude::{LocalTimeline, LocalTimelineSync, NetworkingMetadata, P2P};

use orrery_authority::{AuthorityState, LeaseInbox};
use orrery_protocol::{ClaimId, LeaseId, LeaseMsg, PersistId, SeqPair};
use orrery_sidecar::{secret, sidecar, spawn_predicted};

/// The entity the fixture simulates, holds, and is shot at.
pub const ENTITY: PersistId = PersistId::new(898);

/// A sidecar with Lightyear's prediction pipeline live and a registrar grant
/// for [`ENTITY`], stepped `ticks` canonical ticks.
///
/// The grant matters: `record_published_held_poses` filters publications by
/// the settled live-fence set, so an ungranted entity gets an empty ring and
/// every claim against it is refused as `BasisNotRetained`. That is the
/// correct behaviour and it would hide the thing these tests are checking.
pub fn held_sidecar(seed: u8, ticks: u32) -> (App, Entity) {
    let key = secret(seed);
    let authority = key.public();
    let mut app = sidecar(key, true);
    // A declared P2P session is sufficient to turn on Lightyear's real
    // prediction pipeline; #896 already proves the facade's iroh bridge.
    app.world_mut().spawn(P2P);
    warm_up(&mut app);
    let predicted = spawn_predicted(&mut app, authority, ENTITY);
    grant(&mut app, ENTITY);

    for _ in 0..ticks {
        app.update();
    }
    (app, predicted)
}

/// Settle the networking topology and switch to one fixed tick per update.
pub fn warm_up(app: &mut App) {
    app.insert_resource(TimeUpdateStrategy::FixedTimesteps(0));
    app.update();
    app.update();
    assert!(
        app.world().resource::<NetworkingMetadata>().mode.is_p2p(),
        "the prediction pipeline is off: topology did not settle on P2P"
    );
    app.world_mut()
        .resource_mut::<LocalTimelineSync>()
        .set_synced(true);
    app.insert_resource(TimeUpdateStrategy::FixedTimesteps(1));
}

/// Hand the app a registrar grant for `entity` and prove it settled.
pub fn grant(app: &mut App, entity: PersistId) {
    app.world_mut()
        .resource_mut::<LeaseInbox>()
        .0
        .push(LeaseMsg::Grant {
            claim_id: ClaimId::REGISTRAR,
            entity,
            lease_id: LeaseId(7),
            seq: SeqPair::default(),
            ttl_ms: 10_000,
            prev_holder: None,
        });
    app.update();
    assert_eq!(
        app.world()
            .resource::<AuthorityState>()
            .local_lease_id(entity),
        Some(LeaseId(7)),
        "the fixture must hold the entity before its poses may be retained"
    );
}

/// The session tick the app is on.
pub fn session_tick(app: &App) -> u32 {
    app.world().resource::<LocalTimeline>().tick().0
}
