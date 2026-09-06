//! Proximity grab: which pickup the player's craft may claim, and when the
//! skin says so.
//!
//! Nothing here adjudicates. The ruleset owns the reach
//! ([`orrery_games::regolith::GRAB_RADIUS_MM`]) and the grant
//! ([`orrery_games::regolith::within_grab_reach`], which the pickup's own step
//! calls); this module reads replicated state through that same predicate and
//! decides only *whether to emit an order*. A pickup this module calls
//! claimable can still be denied — another ship's `GrabAttempt` may be ordered
//! first, or the replica the skin read may be stale. That is the ordinary
//! contested-pickup outcome (#320) and the skin never pre-empts it.
//!
//! Two rules shape the emitter:
//!
//! * **Read the threshold, never re-derive it.** The only distance test here
//!   is the ruleset's own function. If the skin invented its own "near
//!   enough", client and host would disagree about reach — the #499/#505
//!   class of bug.
//! * **Edge-triggered.** A `Grab` every tick inside 25 m is an order-volume
//!   problem against D46's emission cap. [`GrabLatch`] emits at most one
//!   `Grab` per tick and at most once per pickup per approach: a pickup is
//!   latched when its order goes out and unlatched only once the craft has
//!   left the ruleset's reach of it.

use std::collections::BTreeSet;

use bevy::prelude::Resource;
use orrery_core::Executor;
use orrery_games::regolith::state::RegolithState;
use orrery_games::regolith::{within_grab_reach, GRAB_RADIUS_MM};
use orrery_games::Regolith;
use orrery_protocol::PersistId;

/// The ruleset's grab reach in metres, for the world-space overlay.
///
/// Derived from [`GRAB_RADIUS_MM`] at compile time so the drawn ring cannot
/// drift from the adjudicated distance.
#[allow(clippy::cast_precision_loss)]
pub const GRAB_RADIUS_M: f32 = GRAB_RADIUS_MM as f32 / 1_000.0;

/// The statement the legend and the HUD both make about grabbing.
///
/// There is no grab key to name: the owner's decision (#568) is that the skin
/// emits the order on approach — "pickups are collected by flying into them".
/// The legend from #564 states that mechanism instead of listing a binding,
/// and reads this constant, so the panel and the legend cannot drift apart.
/// It is phrased short enough to sit on one line of both panels, which
/// `legend::legend_fits_the_default_720_line_window` proves for the legend.
pub const PICKUP_STATEMENT: &str = "Fly into a pickup to collect it.";

/// One live pickup as the skin sees it this tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PickupReach {
    /// The pickup's entity id.
    pub entity: PersistId,
    /// Straight-line separation from the player's craft, in millimetres.
    pub range_mm: i64,
    /// Whether the ruleset's own reach predicate holds for this separation.
    pub claimable: bool,
}

/// This tick's reading of every pickup the client can see.
#[derive(Resource, Debug, Clone, Default, PartialEq, Eq)]
pub struct ReachView {
    /// Live (unclaimed, unexpired) pickups, nearest first.
    pub live: Vec<PickupReach>,
    /// Every pickup within the ruleset's reach, claimed or not. The latch
    /// releases a pickup only once it leaves this set.
    pub in_reach: BTreeSet<PersistId>,
}

impl ReachView {
    /// The nearest live pickup, which is what the HUD reports.
    #[must_use]
    pub fn nearest(&self) -> Option<PickupReach> {
        self.live.first().copied()
    }

    /// Reads every pickup out of the executor against `me`'s position.
    ///
    /// Returns an empty view when the client holds no craft state for `me`:
    /// with no position there is no honest distance to report, and a grab
    /// emitted from a guess would be one the ruleset refuses.
    #[must_use]
    pub fn read(executor: &Executor<Regolith>, me: PersistId) -> Self {
        let Some(RegolithState::Craft(own)) = executor.state(me) else {
            return Self::default();
        };
        let mut live = Vec::new();
        let mut in_reach = BTreeSet::new();
        for entity in executor.entities().copied().collect::<Vec<_>>() {
            let Some(RegolithState::Pickup(pickup)) = executor.state(entity) else {
                continue;
            };
            let within = within_grab_reach(pickup.pos, own.pos);
            if within {
                in_reach.insert(entity);
            }
            if pickup.claimed_by.is_none() && !pickup.expired {
                live.push(PickupReach {
                    entity,
                    range_mm: range_mm(pickup.pos, own.pos),
                    claimable: within,
                });
            }
        }
        // Nearest first, then by id: two pickups at the same distance must
        // order the same way on every client, or two replicas of the same
        // approach would emit for different pickups.
        live.sort_by_key(|reach| (reach.range_mm, reach.entity.0));
        Self { live, in_reach }
    }
}

/// Which pickups already had their order sent on this approach.
#[derive(Resource, Debug, Clone, Default)]
pub struct GrabLatch {
    latched: BTreeSet<PersistId>,
}

impl GrabLatch {
    /// The pickup to emit `Order::Grab` for this tick, if any.
    ///
    /// At most one order per tick, and at most one per pickup per approach.
    /// A pickup leaves the latch when the craft leaves the ruleset's reach of
    /// it, so a second pass over the same pickup — one that was contested and
    /// lost, or one whose grant is still in flight — grabs again.
    pub fn select(&mut self, view: &ReachView) -> Option<PersistId> {
        self.latched.retain(|entity| view.in_reach.contains(entity));
        let chosen = view
            .live
            .iter()
            .find(|reach| reach.claimable && !self.latched.contains(&reach.entity))?
            .entity;
        self.latched.insert(chosen);
        Some(chosen)
    }

    /// Pickups currently held by the latch, for tests and the F3 pane.
    #[must_use]
    pub fn latched(&self) -> &BTreeSet<PersistId> {
        &self.latched
    }
}

/// The own-craft panel's pickup line.
///
/// With nothing live in view it states the mechanism, which is the legend line
/// #564 gains; with a pickup in view it reports that pickup's separation
/// against the ruleset's reach, so the player can read both numbers rather
/// than judge a sub-pixel distance on screen.
#[must_use]
pub fn caption(view: &ReachView) -> String {
    match view.nearest() {
        None => PICKUP_STATEMENT.to_owned(),
        Some(reach) if reach.claimable => format!(
            "PICKUP IN REACH  {} m / {} m",
            reach.range_mm / 1_000,
            GRAB_RADIUS_MM / 1_000
        ),
        Some(reach) => format!(
            // ASCII only: this client loads no font asset, so a non-ASCII
            // separator draws as a box. The rule is asserted for the anchor
            // and the legend (`anchor.rs`, `legend.rs`) and now here.
            "PICKUP {} m  |  reach {} m",
            reach.range_mm / 1_000,
            GRAB_RADIUS_MM / 1_000
        ),
    }
}

fn range_mm(a: orrery_core::QPos, b: orrery_core::QPos) -> i64 {
    i64::try_from(crate::combat::integer_sqrt(
        a.distance_squared(b).unsigned_abs(),
    ))
    .unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use orrery_core::QPos;
    use orrery_games::regolith::state::{Craft, Pickup, RegolithState};
    use orrery_games::regolith::{archetype::Archetype, weapon::WeaponKind, PICKUP_TTL_TICKS};

    const ME: PersistId = PersistId::new(1);

    fn pickup_at(x_mm: i64) -> Pickup {
        Pickup {
            pos: QPos {
                x: x_mm,
                y: 0,
                z: 0,
            },
            kind: WeaponKind::Heavy,
            expires_at: PICKUP_TTL_TICKS,
            ttl_remaining: PICKUP_TTL_TICKS,
            claimed_by: None,
            claimed_at: None,
            expired: false,
        }
    }

    fn executor(craft_x_mm: i64, pickups: &[(PersistId, Pickup)]) -> Executor<Regolith> {
        let mut executor =
            Executor::<Regolith>::new(Regolith::honest(), orrery_protocol::UniverseSeed([7; 32]));
        let craft = Craft::spawned(
            Archetype::Interceptor,
            QPos {
                x: craft_x_mm,
                y: 0,
                z: 0,
            },
            0,
        );
        executor.insert(ME, RegolithState::Craft(craft));
        for (entity, pickup) in pickups {
            executor.insert(*entity, RegolithState::Pickup(pickup.clone()));
        }
        executor
    }

    #[test]
    fn reach_is_the_rulesets_own_radius_not_a_skin_constant() {
        let just_inside = executor(0, &[(PersistId::new(9), pickup_at(GRAB_RADIUS_MM))]);
        let just_outside = executor(0, &[(PersistId::new(9), pickup_at(GRAB_RADIUS_MM + 1))]);
        assert!(
            ReachView::read(&just_inside, ME)
                .nearest()
                .unwrap()
                .claimable
        );
        assert!(
            !ReachView::read(&just_outside, ME)
                .nearest()
                .unwrap()
                .claimable
        );
    }

    #[test]
    fn one_grab_per_pickup_per_approach() {
        let inside = ReachView::read(&executor(0, &[(PersistId::new(9), pickup_at(10_000))]), ME);
        let mut latch = GrabLatch::default();
        assert_eq!(latch.select(&inside), Some(PersistId::new(9)));
        for _ in 0..120 {
            assert_eq!(latch.select(&inside), None, "the latch re-armed in reach");
        }
        // Fly out of reach and back: a second approach is a second order.
        let outside = ReachView::read(
            &executor(0, &[(PersistId::new(9), pickup_at(GRAB_RADIUS_MM * 4))]),
            ME,
        );
        assert_eq!(latch.select(&outside), None);
        assert_eq!(latch.select(&inside), Some(PersistId::new(9)));
    }

    #[test]
    fn a_claimed_pickup_is_never_grabbed_again() {
        let mut claimed = pickup_at(1_000);
        claimed.claimed_by = Some(PersistId::new(2));
        let view = ReachView::read(&executor(0, &[(PersistId::new(9), claimed)]), ME);
        assert!(view.nearest().is_none(), "a claimed pickup is not live");
        assert_eq!(GrabLatch::default().select(&view), None);
    }

    #[test]
    fn an_expired_pickup_is_never_grabbed() {
        let mut expired = pickup_at(1_000);
        expired.expired = true;
        expired.ttl_remaining = 0;
        let view = ReachView::read(&executor(0, &[(PersistId::new(9), expired)]), ME);
        assert_eq!(GrabLatch::default().select(&view), None);
    }

    #[test]
    fn two_pickups_in_reach_are_each_grabbed_once_nearest_first() {
        let view = ReachView::read(
            &executor(
                0,
                &[
                    (PersistId::new(9), pickup_at(20_000)),
                    (PersistId::new(8), pickup_at(5_000)),
                ],
            ),
            ME,
        );
        let mut latch = GrabLatch::default();
        assert_eq!(latch.select(&view), Some(PersistId::new(8)));
        assert_eq!(latch.select(&view), Some(PersistId::new(9)));
        assert_eq!(latch.select(&view), None);
    }

    /// The whole path, through the ordinary ruleset steps: the skin selects a
    /// pickup, `human_orders` authors `Order::Grab`, the craft's own step
    /// stamps the position onto `GrabAttempted`, delivery turns that into the
    /// pickup's `GrabAttempt`, and the pickup grants.
    ///
    /// This is what pins `Grab` as the client-authored order. Emitting
    /// `GrabAttempt` instead would be silently inert here: the craft step
    /// ignores it, so no delivery would ever reach the pickup.
    #[test]
    fn flying_into_a_pickup_claims_it_through_the_ordinary_ruleset_path() {
        use crate::intent::{Controls, IntentPipeline};
        use orrery_games::Game;
        use orrery_protocol::{Tick, UniverseSeed};

        let pickup_id = PersistId::new(9);
        let mut executor = executor(0, &[(pickup_id, pickup_at(10_000))]);
        let view = ReachView::read(&executor, ME);
        let chosen = GrabLatch::default()
            .select(&view)
            .expect("a pickup 10 m away is inside the ruleset's reach");
        assert_eq!(chosen, pickup_id);

        let pipeline = IntentPipeline::new(UniverseSeed([7; 32]), ME, 0, vec![]);
        let tick = Tick::new(1);
        let orders = pipeline.human_orders(
            tick,
            Controls {
                grab: Some(chosen),
                ..Controls::default()
            },
        );
        let outcome = executor
            .step_entity(ME, tick, &orders)
            .expect("the craft is installed");
        let delivered: Vec<_> = outcome
            .events
            .iter()
            .filter_map(|event| executor.ruleset().deliver(event))
            .filter(|(target, _)| *target == pickup_id)
            .map(|(_, order)| order)
            .collect();
        assert_eq!(delivered.len(), 1, "one grab attempt reached the pickup");
        executor
            .step_entity(pickup_id, tick, &delivered)
            .expect("the pickup is installed");
        let Some(RegolithState::Pickup(after)) = executor.state(pickup_id) else {
            panic!("the pickup is still installed");
        };
        assert_eq!(
            after.claimed_by,
            Some(ME),
            "the ruleset did not grant the claim"
        );
    }

    #[test]
    fn caption_states_the_mechanism_when_no_pickup_is_live() {
        assert_eq!(caption(&ReachView::default()), PICKUP_STATEMENT);
        let near = ReachView::read(&executor(0, &[(PersistId::new(9), pickup_at(10_000))]), ME);
        assert!(caption(&near).contains("IN REACH"));
        let far = ReachView::read(&executor(0, &[(PersistId::new(9), pickup_at(400_000))]), ME);
        assert!(caption(&far).contains("400 m"));
    }

    /// The rule `anchor.rs` and `legend.rs` each assert for their own lines,
    /// applied to this module's: no font asset is loaded, so a non-ASCII
    /// character draws as a box. The out-of-reach caption carried a U+00B7
    /// middle dot, which is the branch a pickup in view but out of reach
    /// shows — the common one.
    #[test]
    fn every_caption_is_ascii_because_no_font_asset_is_loaded() {
        let views = [
            ReachView::default(),
            ReachView::read(&executor(0, &[(PersistId::new(9), pickup_at(10_000))]), ME),
            ReachView::read(&executor(0, &[(PersistId::new(9), pickup_at(400_000))]), ME),
        ];
        for view in &views {
            let line = caption(view);
            assert!(line.is_ascii(), "non-ASCII renders as a box: {line:?}");
        }
    }
}
