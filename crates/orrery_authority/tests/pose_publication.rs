//! End-to-end checks for the game-owned canonical pose publication seam.
//!
//! The unit tests in `hit.rs` prove the ring and validator in isolation. These
//! drive the shipping authority plugin from registrar grant through the
//! host-supplied queue, because the missing production writer was a wiring
//! failure rather than a ring-algorithm failure.

use bevy_app::App;
use orrery_authority::{
    AuthorityPhase, AuthorityState, CanonicalPosePublications, HitRules, LeaseInbox,
    OrreryAuthorityPlugin, PersistIdentity, PoseHistory, PoseSample,
};
use orrery_protocol::{
    ClaimId, HitClaim, HitOutcome, HitRefusal, HitSurface, HitWindow, InterpBasis, LatticePoint,
    LeaseId, LeaseMsg, PersistId, QuantizedDir, QuantizedRay, SeqPair, Tick, WeaponRef,
};

const TARGET: PersistId = PersistId::new(41);
const UNHELD: PersistId = PersistId::new(42);
const WINDOW: HitWindow = HitWindow::new(12, 4);

fn sample(x: i64) -> PoseSample {
    PoseSample {
        position: LatticePoint::new(x, 0, 0),
        hit_radius: 250,
    }
}

fn authority() -> App {
    let mut app = App::new();
    app.add_plugins(OrreryAuthorityPlugin::default().with_hit_window(WINDOW));
    app.world_mut()
        .spawn((PersistIdentity(TARGET), AuthorityPhase::Remote));
    app.world_mut()
        .spawn((PersistIdentity(UNHELD), AuthorityPhase::Remote));
    app.world_mut()
        .resource_mut::<LeaseInbox>()
        .0
        .push(LeaseMsg::Grant {
            claim_id: ClaimId::REGISTRAR,
            entity: TARGET,
            lease_id: LeaseId(7),
            seq: SeqPair::default(),
            ttl_ms: 10_000,
            prev_holder: None,
        });
    app.update();
    assert_eq!(
        app.world()
            .resource::<AuthorityState>()
            .local_lease_id(TARGET),
        Some(LeaseId(7)),
        "fixture must hold the target before publishing its pose"
    );
    app
}

fn publish(app: &mut App, entity: PersistId, tick: u64, pose: PoseSample) {
    app.world_mut()
        .resource_mut::<CanonicalPosePublications>()
        .publish(entity, Tick::new(tick), pose);
}

#[test]
fn published_canonical_poses_are_recorded_once_per_tick_with_their_published_bounds() {
    let mut app = authority();

    // Several canonical ticks may complete before the authority's `Update`.
    // A duplicate for one `(tick, entity)` replaces the pending value rather
    // than causing a second record operation.
    publish(&mut app, TARGET, 10_001, sample(1));
    publish(&mut app, TARGET, 10_002, sample(2));
    publish(&mut app, TARGET, 10_002, sample(20));
    assert_eq!(
        app.world().resource::<CanonicalPosePublications>().len(),
        2,
        "the queue is unique per canonical tick and entity"
    );
    app.update();

    let history = app.world().resource::<PoseHistory>();
    assert_eq!(
        history.retained(TARGET),
        Some((Tick::new(10_001), Tick::new(10_002)))
    );
    assert_eq!(history.pose_at(TARGET, Tick::new(10_002)), Some(sample(20)));

    publish(&mut app, TARGET, 10_003, sample(3));
    publish(&mut app, TARGET, 10_004, sample(4));
    publish(&mut app, TARGET, 10_005, sample(5));
    app.update();

    let history = app.world().resource::<PoseHistory>();
    assert_eq!(
        history.retained(TARGET),
        Some((Tick::new(10_002), Tick::new(10_005))),
        "the four-slot ring moves with published universe ticks, not Update sample times"
    );
    assert_eq!(history.pose_at(TARGET, Tick::new(10_001)), None);
    assert_eq!(history.pose_at(TARGET, Tick::new(10_005)), Some(sample(5)));
}

struct TestHitRules;

impl HitRules for TestHitRules {
    fn weapon_reach(&self, weapon: WeaponRef) -> Option<u32> {
        (weapon == WeaponRef(9)).then_some(20_000)
    }
}

fn claim_at(tick: u64) -> HitClaim {
    HitClaim {
        shooter: PersistId::new(7),
        target: TARGET,
        weapon: WeaponRef(9),
        fire_tick: Tick::new(tick),
        basis: InterpBasis::exact(Tick::new(tick)),
        ray: QuantizedRay {
            origin: LatticePoint::default(),
            direction: QuantizedDir::new(1, 0, 0),
        },
        claimed: HitSurface(3),
        input_seq: 11,
    }
}

#[test]
fn a_held_entity_without_a_host_publication_refuses_the_claim_as_basis_not_retained() {
    let app = authority();
    let claim = claim_at(700);

    let verdict = app
        .world()
        .resource::<PoseHistory>()
        .validate(&claim, &TestHitRules);
    assert_eq!(
        verdict.outcome,
        HitOutcome::Rejected(HitRefusal::BasisNotRetained {
            tick: Tick::new(700),
            oldest_retained: None,
            newest_retained: None,
        }),
        "without the host contract, the held target has a ring but no canonical basis"
    );
}

#[test]
fn a_hit_claim_validates_end_to_end_against_a_game_published_pose() {
    let mut app = authority();
    let pose = sample(5_000);
    publish(&mut app, TARGET, 700, pose);
    app.update();

    let claim = claim_at(700);

    let verdict = app
        .world()
        .resource::<PoseHistory>()
        .validate(&claim, &TestHitRules);
    assert_eq!(verdict.claim, claim.key());
    assert_eq!(verdict.target, TARGET);
    assert_eq!(verdict.claimed, HitSurface(3));
    assert_eq!(
        verdict.outcome,
        HitOutcome::Accepted {
            applied_at: Tick::new(701),
            pose: pose.position,
        },
        "the first end-to-end hit must cross publication, retention and validation"
    );
}

#[test]
fn a_pose_published_for_an_unheld_entity_is_ignored_and_never_enters_the_ring() {
    let mut app = authority();
    publish(&mut app, UNHELD, 88, sample(8_800));
    app.update();

    let history = app.world().resource::<PoseHistory>();
    assert!(!history.holds(UNHELD));
    assert_eq!(history.retained(UNHELD), None);
    assert_eq!(history.pose_at(UNHELD, Tick::new(88)), None);
}
