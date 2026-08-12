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

use std::collections::HashSet;
use std::ops::RangeInclusive;

use bevy_ecs::prelude::*;
use bevy_math::Vec3;

use crate::config::SpatialConfig;
use crate::hysteresis::GridPosition;
use crate::plugin::LocalPlayer;

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
/// The first `cap` (nearest) entries are high-rate; the rest are proxies.
#[must_use]
pub fn split_high_rate<K: Copy>(ranked: Vec<(K, f32)>, cap: usize) -> (Vec<K>, Vec<(K, f32)>) {
    let cap = cap.min(ranked.len());
    let (near, far) = ranked.split_at(cap);
    (
        near.iter().map(|(k, _)| *k).collect(),
        far.iter().map(|(k, d)| (*k, *d)).collect(),
    )
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

/// Recompute the high-rate set and proxy tags from positions (P1 core).
///
/// Nearest `high_rate_cap` entities get the [`HighRate`] marker; every other
/// in-AOI candidate gets a [`Proxy`] with a distance-interpolated rate. Tags
/// are reconciled so each entity holds exactly one of the two.
pub fn update_interest_set(
    cfg: Res<SpatialConfig>,
    mut commands: Commands,
    mut selection: ResMut<InterestSelection>,
    player: Query<&GridPosition, With<LocalPlayer>>,
    candidates: Query<(Entity, &GridPosition), Without<LocalPlayer>>,
) {
    let Ok(center) = player.single() else {
        return;
    };

    let ranked = rank_by_distance(center.0, candidates.iter().map(|(e, p)| (e, p.0)));
    let (high_rate, proxies) = split_high_rate(ranked, cfg.high_rate_cap);

    let high: HashSet<Entity> = high_rate.iter().copied().collect();
    // Compute each proxy's refresh rate once, from its linear distance.
    let proxy_rates: Vec<(Entity, f32)> = proxies
        .iter()
        .map(|(e, d)| (*e, proxy_rate_hz(d.sqrt(), &cfg.proxy_hz)))
        .collect();

    for entity in candidates.iter().map(|(e, _)| e) {
        if high.contains(&entity) {
            commands.entity(entity).remove::<Proxy>().insert(HighRate);
        } else {
            let rate = proxy_rates
                .iter()
                .find(|(e, _)| *e == entity)
                .map(|(_, r)| *r)
                .expect("every proxy has a recorded rate");
            commands
                .entity(entity)
                .remove::<HighRate>()
                .insert(Proxy { rate_hz: rate });
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
