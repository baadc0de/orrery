//! The bounded high-rate interest set and 1–4 Hz extrapolated proxies (D6).
//!
//! Cells are the coarse filter; a second-stage, distance-precise filter picks
//! the **24-entity high-rate set** from the in-AOI population (the Donnybrook
//! pattern, docs/03-replication.md). Every remaining entity in range degrades
//! to a low-rate **extrapolated proxy** so the receive cost stays bounded
//! (`~12·n kb/s`, SIGCOMM 2008) while perceptual fidelity is preserved by
//! keeping the closest entities high-rate.
//!
//! Selection is purely distance-based (nearest-first), recomputed each update —
//! it is deliberately *not* incremental, matching the AOI diff philosophy
//! (docs/01-spatial-model.md §6). Proxies refresh between 1 and 4 Hz (D16),
//! nearer proxies refreshing faster.

use core::time::Duration;
use std::collections::{HashMap, HashSet};
use std::ops::RangeInclusive;

use bevy_ecs::prelude::*;
use bevy_math::Vec3;
use bevy_time::{Real, Time};

use crate::config::SpatialConfig;
use crate::hysteresis::GridPosition;
use crate::plugin::{AoiSubscription, Cell, LocalPlayer};

/// The AOI radius in grid units (one 3×3×3 cell block extends 1.5 cells from
/// its center). Used to interpolate proxy refresh rates across the visible
/// range.
pub const AOI_RADIUS_GRID: f32 = 1.5;

/// Marker: this replicated entity is in the bounded high-rate interest set.
#[derive(Debug, Clone, Copy, Component)]
pub struct HighRate;

/// A low-rate extrapolated proxy for an out-of-set entity (D6).
#[derive(Debug, Clone, Copy, Component)]
pub struct Proxy {
    /// Refresh rate in Hz, within the config's `proxy_hz` range.
    pub rate_hz: f32,
}

/// The current interest selection, exposed for telemetry and tests.
#[derive(Debug, Default, Resource)]
pub struct InterestSelection {
    /// Entities in the bounded high-rate set, nearest-first.
    pub high_rate: Vec<Entity>,
    /// Out-of-set entities and their proxy refresh rate.
    pub proxies: Vec<(Entity, f32)>,
}

impl InterestSelection {
    /// Whether `entity` is in the bounded high-rate set.
    #[must_use]
    pub fn is_high_rate(&self, entity: Entity) -> bool {
        self.high_rate.contains(&entity)
    }

    /// Whether `entity` is proxied, and at what rate.
    #[must_use]
    pub fn proxy_rate(&self, entity: Entity) -> Option<f32> {
        self.proxies
            .iter()
            .find(|(e, _)| *e == entity)
            .map(|(_, rate)| *rate)
    }
}

/// Rank `candidates` by squared distance from `center`, nearest-first.
///
/// Engine-agnostic; the Bevy system feeds `Entity` keys. Ties are broken by the
/// original iteration order (stable sort).
#[must_use]
pub fn rank_by_distance<K: Copy>(
    center: Vec3,
    candidates: impl Iterator<Item = (K, Vec3)>,
) -> Vec<(K, f32)> {
    let mut ranked: Vec<(K, f32)> = candidates
        .map(|(key, pos)| (key, pos.distance_squared(center)))
        .collect();
    ranked.sort_by(|a, b| a.1.total_cmp(&b.1));
    ranked
}

/// Split a ranked list into the bounded high-rate set and the proxies.
///
/// The first `cap` (nearest) entries are high-rate; the rest are proxies. This
/// is the memoryless cut — see [`select_high_rate`] for the one the design
/// actually specifies, which this is the `incumbents.is_empty()` case of.
#[must_use]
pub fn split_high_rate<K: Copy>(ranked: Vec<(K, f32)>, cap: usize) -> (Vec<K>, Vec<(K, f32)>) {
    let cap = cap.min(ranked.len());
    let (near, far) = ranked.split_at(cap);
    (
        near.iter().map(|(k, _)| *k).collect(),
        far.iter().map(|(k, d)| (*k, *d)).collect(),
    )
}

/// The margin a challenger must beat an incumbent by to take its slot (D6,
/// docs/03-replication.md §4.1).
pub const EVICTION_MARGIN: f32 = 0.15;

/// How often set membership is re-evaluated (docs/03-replication.md §4.1).
///
/// Membership is *sticky between rescores*. Together with the margin this is
/// what bounds flap frequency: §9.5 names two near-tied entities alternating in
/// and out of the 24 slots as the failure, because each swap resets an
/// interpolation buffer and churns a baseline, and a player sees that as a
/// stutter.
pub const RESCORE_HZ: f32 = 1.0;

/// Choose the high-rate set, keeping incumbents unless clearly beaten.
///
/// `ranked` is nearest-first with **squared** distances. An incumbent holds its
/// slot until a challenger is more than [`EVICTION_MARGIN`] closer; free slots
/// go to the best challengers regardless.
///
/// Comparing on squared distance means the margin is squared too: being 15%
/// closer is `d_challenger · 1.15² ≤ d_incumbent`. Applying the raw 15% to
/// squared values would silently ask for a ~7% margin instead.
#[must_use]
pub fn select_high_rate<K: Copy + PartialEq>(
    ranked: &[(K, f32)],
    incumbents: &[K],
    cap: usize,
) -> Vec<K> {
    let margin = (1.0 + EVICTION_MARGIN) * (1.0 + EVICTION_MARGIN);

    // Incumbents that are still candidates at all, best first.
    let mut held: Vec<(K, f32)> = ranked
        .iter()
        .filter(|(key, _)| incumbents.contains(key))
        .copied()
        .collect();
    let mut challengers: Vec<(K, f32)> = ranked
        .iter()
        .filter(|(key, _)| !incumbents.contains(key))
        .copied()
        .collect();

    // Free slots go to the best challengers: holding a slot empty against a
    // newcomer would bound flap by starving the set.
    while held.len() < cap && !challengers.is_empty() {
        held.push(challengers.remove(0));
    }

    // Contested slots: the worst incumbent yields only to a decisively better
    // challenger. Both lists are sorted, so one pass from each end suffices.
    for challenger in challengers {
        let Some((worst_index, worst)) = held
            .iter()
            .enumerate()
            .max_by(|a, b| a.1 .1.total_cmp(&b.1 .1))
            .map(|(index, entry)| (index, entry.1))
        else {
            break;
        };
        if challenger.1 * margin <= worst {
            held[worst_index] = challenger;
        }
    }

    held.sort_by(|a, b| a.1.total_cmp(&b.1));
    held.into_iter().map(|(key, _)| key).collect()
}

/// The proxy refresh rate for an entity at `dist` grid units, within
/// `proxy_hz`. Nearest proxies refresh fastest (max Hz), the AOI edge the
/// slowest (min Hz).
#[must_use]
pub fn proxy_rate_hz(dist: f32, proxy_hz: &RangeInclusive<f32>) -> f32 {
    let min_hz = *proxy_hz.start();
    let max_hz = *proxy_hz.end();
    let t = (dist / AOI_RADIUS_GRID).clamp(0.0, 1.0);
    max_hz - t * (max_hz - min_hz)
}

/// How long a freshly demoted entity's proxy stream takes to decay from the
/// maximum rate to its scored one (docs/03-replication.md §9.5).
pub const DEMOTION_RAMP: Duration = Duration::from_secs(5);

/// The rate a demoted entity actually gets, ramping down from the maximum.
///
/// **This is what makes churn invisible.** §9.5's answer to interest-set
/// flapping is not to prevent demotion — a crowd moving past you genuinely
/// changes who your nearest two dozen entities are — but to cover it: a
/// just-demoted entity keeps streaming at the top of the proxy range and decays
/// to its scored rate over [`DEMOTION_RAMP`], so an entity that re-promotes
/// shortly after never went below a rate the client could interpolate through.
/// Dropping it straight to a distance-scored 1 Hz is the stutter the player
/// sees.
///
/// Returns the scored rate unchanged for an entity that has been a proxy longer
/// than the ramp, or was never in the set to begin with.
#[must_use]
pub fn ramped_proxy_rate(
    scored_hz: f32,
    since_demotion: Option<Duration>,
    proxy_hz: &RangeInclusive<f32>,
) -> f32 {
    let Some(elapsed) = since_demotion else {
        return scored_hz;
    };
    if elapsed >= DEMOTION_RAMP {
        return scored_hz;
    }
    let max_hz = *proxy_hz.end();
    let t = elapsed.as_secs_f32() / DEMOTION_RAMP.as_secs_f32();
    // Never *below* the scored rate: the ramp is a floor, not a replacement.
    (max_hz - t * (max_hz - scored_hz)).max(scored_hz)
}

/// Recompute the high-rate set and proxy tags from positions (P1 core).
///
/// Nearest `high_rate_cap` entities get the [`HighRate`] marker; every other
/// **in-AOI** candidate gets a [`Proxy`] with a distance-interpolated rate.
/// Tags are reconciled so each entity holds at most one of the two.
///
/// # Cells gate this, distance only orders it
///
/// Only entities whose committed [`Cell`] is in the AOI are ranked at all.
/// Cells are the coarse filter and distance is the second stage (D5/D6) — a
/// version that ranked every entity in the world would hand a 1 Hz proxy to
/// something a hundred kilometres away, and the receive-cost bound the whole
/// Donnybrook pattern rests on is over the *in-range* population, not the
/// global one.
///
/// Entities outside the AOI are stripped of both tags rather than left holding
/// the last one they had, because a stale `HighRate` marker on something that
/// has left the neighbourhood is indistinguishable from a current one wherever
/// the tags are read.
// A Bevy system's parameters are its dependencies, and this one genuinely needs
// all of them: config, the AOI gate, a clock for the rescore cadence, two pieces
// of carried state, and the two queries. Bundling them into a `SystemParam` to
// satisfy the count would hide the dependency list rather than shorten it.
#[allow(clippy::too_many_arguments)]
pub fn update_interest_set(
    cfg: Res<SpatialConfig>,
    aoi: Res<AoiSubscription>,
    time: Res<Time<Real>>,
    mut commands: Commands,
    mut selection: ResMut<InterestSelection>,
    mut last_rescore: Local<Option<Duration>>,
    mut demoted_at: Local<Vec<(Entity, Duration)>>,
    player: Query<&GridPosition, With<LocalPlayer>>,
    candidates: Query<(Entity, &GridPosition, &Cell), Without<LocalPlayer>>,
) {
    let Ok(center) = player.single() else {
        return;
    };

    let in_aoi: Vec<(Entity, Vec3)> = candidates
        .iter()
        .filter(|(_, _, cell)| aoi.contains(cell.0))
        .map(|(entity, pos, _)| (entity, pos.0))
        .collect();

    let ranked = rank_by_distance(center.0, in_aoi.iter().copied());

    // Membership is re-evaluated at `RESCORE_HZ`, not every frame. Between
    // rescores incumbents keep their slots — that stickiness, with the eviction
    // margin, is what bounds flap (docs/03-replication.md §4.1, §9.5). An
    // entity that has left the AOI drops out regardless, because it is no
    // longer a candidate at all.
    let now = time.elapsed();
    let previously_high: Vec<Entity> = selection.high_rate.clone();
    let period = Duration::from_secs_f32(1.0 / RESCORE_HZ.max(f32::EPSILON));
    let due = last_rescore.is_none_or(|last| now.saturating_sub(last) >= period);
    let incumbents: Vec<Entity> = if due {
        *last_rescore = Some(now);
        selection.high_rate.clone()
    } else {
        // Not due: hold the set as it stands, minus anyone no longer eligible.
        selection
            .high_rate
            .iter()
            .copied()
            .filter(|entity| ranked.iter().any(|(key, _)| key == entity))
            .collect()
    };

    let high_rate = if due {
        select_high_rate(&ranked, &incumbents, cfg.high_rate_cap)
    } else {
        let mut held = incumbents.clone();
        // Fill vacancies immediately even off-cadence: a slot left empty until
        // the next rescore is a second of an entity nobody is watching.
        for (key, _) in &ranked {
            if held.len() >= cfg.high_rate_cap {
                break;
            }
            if !held.contains(key) {
                held.push(*key);
            }
        }
        held
    };
    let proxies: Vec<(Entity, f32)> = ranked
        .iter()
        .filter(|(key, _)| !high_rate.contains(key))
        .copied()
        .collect();

    let high: HashSet<Entity> = high_rate.iter().copied().collect();

    // Anyone who just lost a slot starts their demotion ramp now.
    for entity in &previously_high {
        if !high.contains(entity) && !demoted_at.iter().any(|(e, _)| e == entity) {
            demoted_at.push((*entity, now));
        }
    }
    // A re-promoted entity is no longer ramping, and a ramp expires on time.
    //
    // Deliberately *not* conditioned on the entity still being a candidate: the
    // ramp is a function of time since demotion, and dropping the record the
    // moment an entity blips out of the AOI for a frame would hand it the bare
    // scored rate the instant it came back. The 5 s expiry keeps the list
    // bounded without needing that.
    demoted_at
        .retain(|(entity, at)| !high.contains(entity) && now.saturating_sub(*at) < DEMOTION_RAMP);

    // Each proxy's refresh rate: scored by distance, floored by the ramp for
    // anyone recently demoted (docs/03-replication.md §9.5).
    let proxy_rates: Vec<(Entity, f32)> = proxies
        .iter()
        .map(|(entity, squared)| {
            let scored = proxy_rate_hz(squared.sqrt(), &cfg.proxy_hz);
            let since = demoted_at
                .iter()
                .find(|(e, _)| e == entity)
                .map(|(_, at)| now.saturating_sub(*at));
            (*entity, ramped_proxy_rate(scored, since, &cfg.proxy_hz))
        })
        .collect();
    let rates: HashMap<Entity, f32> = proxy_rates.iter().copied().collect();

    for (entity, _, cell) in &candidates {
        if !aoi.contains(cell.0) {
            commands
                .entity(entity)
                .remove::<HighRate>()
                .remove::<Proxy>();
        } else if high.contains(&entity) {
            commands.entity(entity).remove::<Proxy>().insert(HighRate);
        } else if let Some(rate_hz) = rates.get(&entity) {
            commands
                .entity(entity)
                .remove::<HighRate>()
                .insert(Proxy { rate_hz: *rate_hz });
        }
    }

    selection.high_rate = high_rate;
    selection.proxies = proxy_rates;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vec3(x: f32, y: f32, z: f32) -> Vec3 {
        Vec3::new(x, y, z)
    }

    #[test]
    fn rank_is_nearest_first() {
        let center = vec3(0.0, 0.0, 0.0);
        let candidates = [
            (0u8, vec3(5.0, 0.0, 0.0)),
            (1, vec3(1.0, 0.0, 0.0)),
            (2, vec3(3.0, 0.0, 0.0)),
        ];
        let ranked = rank_by_distance(center, candidates.into_iter());
        let keys: Vec<_> = ranked.iter().map(|(k, _)| *k).collect();
        assert_eq!(keys, vec![1, 2, 0]);
    }

    #[test]
    fn split_respects_cap() {
        let ranked = vec![(0u8, 1.0), (1, 2.0), (2, 3.0), (3, 4.0)];
        let (near, far) = split_high_rate(ranked, 2);
        assert_eq!(near, vec![0, 1]);
        assert_eq!(far, vec![(2, 3.0), (3, 4.0)]);
        // Cap larger than the population keeps everything high-rate.
        let (near, far) = split_high_rate(vec![(0u8, 1.0), (1, 2.0)], 24);
        assert_eq!(near, vec![0, 1]);
        assert!(far.is_empty());
    }

    #[test]
    fn an_incumbent_holds_its_slot_against_a_marginally_closer_challenger() {
        // The failure this exists for: two near-tied entities alternating in
        // and out of the set, each swap resetting an interpolation buffer and
        // churning a baseline. A player sees that as a stutter.
        // Squared distances: incumbent at 100 (d=10), challenger at 90 (d≈9.49)
        // — 5% closer, well inside the 15% margin.
        let held = select_high_rate(&[(1u8, 90.0), (0u8, 100.0)], &[0], 1);
        assert_eq!(held, vec![0], "a 5% edge does not take a slot");
    }

    #[test]
    fn a_decisively_closer_challenger_does_take_the_slot() {
        // The margin must not become a moat: an entity that really is much
        // nearer has to get high-rate treatment, or the set stops tracking the
        // world at all.
        // Challenger at d=8 vs incumbent d=10 is 20% closer, past the margin.
        let held = select_high_rate(&[(1u8, 64.0), (0u8, 100.0)], &[0], 1);
        assert_eq!(held, vec![1]);
    }

    #[test]
    fn the_margin_is_applied_in_the_space_the_distances_are_in() {
        // `rank_by_distance` returns *squared* distances. Applying 15% to a
        // squared value asks for a ~7% margin instead — the kind of error that
        // halves a mitigation without failing anything.
        // Exactly 15% closer: d = 10 / 1.15 = 8.6957, squared = 75.6.
        let boundary = 100.0 / ((1.0 + EVICTION_MARGIN) * (1.0 + EVICTION_MARGIN));
        assert_eq!(
            select_high_rate(&[(1u8, boundary * 0.99), (0u8, 100.0)], &[0], 1),
            vec![1],
            "just past the margin evicts"
        );
        assert_eq!(
            select_high_rate(&[(1u8, boundary * 1.01), (0u8, 100.0)], &[0], 1),
            vec![0],
            "just inside it does not"
        );
    }

    #[test]
    fn free_slots_go_to_challengers_without_any_margin() {
        // Stickiness must not starve the set. An empty slot held against a
        // newcomer would bound flap by replicating nothing.
        let held = select_high_rate(&[(0u8, 1.0), (1u8, 2.0), (2u8, 3.0)], &[0], 3);
        assert_eq!(held, vec![0, 1, 2]);
    }

    #[test]
    fn an_incumbent_that_left_the_candidates_loses_its_slot() {
        // Walking out of the AOI ends membership regardless of the margin —
        // the margin protects near-ties, not absentees.
        let held = select_high_rate(&[(1u8, 500.0)], &[0], 1);
        assert_eq!(held, vec![1]);
    }

    #[test]
    fn with_no_incumbents_selection_is_just_the_nearest() {
        // The first frame, and the property `split_high_rate` already had.
        let ranked: [(u8, f32); 3] = [(0, 3.0), (1, 1.0), (2, 2.0)];
        let mut sorted = ranked;
        sorted.sort_by(|a, b| a.1.total_cmp(&b.1));
        assert_eq!(select_high_rate(&sorted, &[], 2), vec![1, 2]);
    }

    #[test]
    fn a_just_demoted_entity_streams_at_the_top_of_the_range() {
        // §9.5's actual answer to flapping: not preventing demotion, but
        // covering it. An entity dropped straight to its distance-scored 1 Hz
        // is the stutter a player sees.
        let hz = 1.0..=4.0;
        assert_eq!(ramped_proxy_rate(1.0, Some(Duration::ZERO), &hz), 4.0);
    }

    #[test]
    fn the_ramp_decays_to_the_scored_rate_and_stops_there() {
        let hz = 1.0..=4.0;
        let scored = 1.5;
        let early = ramped_proxy_rate(scored, Some(Duration::from_secs(1)), &hz);
        let late = ramped_proxy_rate(scored, Some(Duration::from_secs(4)), &hz);
        assert!(early > late, "the ramp decays");
        assert!(late > scored, "but has not arrived yet at 4 s");
        assert_eq!(
            ramped_proxy_rate(scored, Some(DEMOTION_RAMP), &hz),
            scored,
            "and lands exactly on the scored rate"
        );
    }

    #[test]
    fn the_ramp_is_a_floor_and_never_slows_an_entity_down() {
        // A near proxy already scores above the ramp; taking the ramp's value
        // unconditionally would *reduce* its rate for five seconds.
        let hz = 1.0..=4.0;
        assert_eq!(
            ramped_proxy_rate(4.0, Some(Duration::from_secs(2)), &hz),
            4.0
        );
    }

    #[test]
    fn an_entity_that_was_never_in_the_set_gets_its_scored_rate() {
        let hz = 1.0..=4.0;
        assert_eq!(ramped_proxy_rate(1.2, None, &hz), 1.2);
        assert_eq!(
            ramped_proxy_rate(1.2, Some(DEMOTION_RAMP + Duration::from_secs(1)), &hz),
            1.2
        );
    }

    #[test]
    fn proxy_rate_is_in_range_and_monotonic() {
        let hz = 1.0..=4.0;
        let near = proxy_rate_hz(0.0, &hz);
        let mid = proxy_rate_hz(0.75, &hz);
        let far = proxy_rate_hz(AOI_RADIUS_GRID, &hz);
        assert_eq!(near, 4.0);
        assert_eq!(far, 1.0);
        assert!(mid > 1.0 && mid < 4.0);
        assert!(near >= mid && mid >= far);
    }

    #[test]
    fn selection_tracks_tags() {
        let e1 = Entity::from_bits(1);
        let e2 = Entity::from_bits(2);
        let selection = InterestSelection {
            high_rate: vec![e1],
            proxies: vec![(e2, 2.5)],
        };
        assert!(selection.is_high_rate(e1));
        assert!(!selection.is_high_rate(e2));
        assert_eq!(selection.proxy_rate(e2), Some(2.5));
        assert_eq!(selection.proxy_rate(e1), None);
    }
}
