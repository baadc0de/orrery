//! What holds a pilot to the populated volume, said out loud (#955).
//!
//! # Why this module exists
//!
//! #955's measurement was that the interest block is 1536 m across against a
//! 480 m/s ceiling, so the 2026-09-02 volunteer left the populated volume
//! under their own power by about t+10 s and spent the remaining fourteen
//! minutes generating honest, witnessed telemetry about an empty region.
//!
//! The ruleset's answer is the island tether (`craft-apply-tether`, v25): a
//! craft outside `ISLAND_BOUNDARY_MM` flies against a restoring drag. That is
//! the anchor, and it is a rule. This module is the other half of the same
//! finding — the half that says *nothing told them they had gone* — and it is
//! only ever a cue.
//!
//! # What it asserts: nothing
//!
//! Both readouts here are arithmetic over state the ruleset already published:
//!
//! * the tether line is derived from the client's own craft position and the
//!   two published constants the rule itself uses, so it describes a force
//!   that is already acting rather than predicting one;
//! * the bloom beacon reads `BloomDirector::site_pos`, `site_active_until` and
//!   `site_rocks_alive` — replicated fields, in-band, decided by the host.
//!
//! Nothing here is readable by intent submission, range, arc, lock or
//! collision code, exactly as `crate::aoi` and `crate::contact_arrows` are
//! not. A pilot who ignores every word of it flies precisely the same
//! trajectory.
//!
//! # The bloom is the reason to stay, and the skin was dropping it
//!
//! `sync_rendered_state` skips `RegolithState::BloomDirector` because a
//! director occupies no point in the lattice — correct, and unchanged. But
//! skipping the *body* had also silently discarded the *announcement*. The
//! director draws every bloom site within ±`BLOOM_CENTRAL_RADIUS_MM` (250 m)
//! of the origin, seeds ten rocks there, and `PilotScenario::BloomConvergence`
//! turns the whole bot island toward it. The campaign already concentrated its
//! content and already told the cohort where; the human client was the one
//! participant that never heard.

use bevy::prelude::*;
use orrery_core::{Executor, TICK_HZ};
use orrery_games::regolith::{
    state::RegolithState, Regolith, ISLAND_BOUNDARY_MM, TETHER_BAND_MM, TETHER_ESCAPE_SPEED_MMS,
};
use orrery_protocol::PersistId;

/// The announced bloom, as a pilot needs it: which way, how far, how long.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BloomBeacon {
    /// Metres from the observer to the announced site.
    pub range_m: f32,
    /// Whole seconds until the site closes.
    pub seconds_left: u64,
    /// Live rock lineages still in the site.
    pub rocks_alive: u16,
}

/// One tick's answer to "is anything holding me here, and to what".
#[derive(Resource, Debug, Clone, Copy, PartialEq, Default)]
pub struct AnchorView {
    /// How far outside the island edge the observer is, in metres, per the
    /// same per-axis box the rule uses. Zero inside.
    pub outside_m: f32,
    /// How far the tether has ramped in, 0.0 at the edge to 1.0 at full.
    pub tether_ramp: f32,
    /// The announced bloom, when a director has one and the client sees it.
    pub bloom: Option<BloomBeacon>,
}

/// Depth outside the island edge, in metres, for a position in metres.
///
/// The per-axis maximum, because [`ISLAND_BOUNDARY_MM`] is a square edge and
/// the rule tests each axis on its own. Measuring a radius here would be a
/// second, disagreeing definition of one boundary — the failure #499 and #502
/// already cost, and the reason `crate::aoi` takes its edge as an argument
/// rather than holding a copy.
#[must_use]
pub fn outside_island_m(pos: Vec3) -> f32 {
    let boundary = ISLAND_BOUNDARY_MM as f32 / 1_000.0;
    let axis = |v: f32| (v.abs() - boundary).max(0.0);
    axis(pos.x).max(axis(pos.y)).max(axis(pos.z))
}

/// How far the tether has ramped in at `outside_m` metres outside the edge.
#[must_use]
pub fn tether_ramp(outside_m: f32) -> f32 {
    let band = TETHER_BAND_MM as f32 / 1_000.0;
    if band <= 0.0 {
        return 0.0;
    }
    (outside_m / band).clamp(0.0, 1.0)
}

impl AnchorView {
    /// Reads one tick out of the shared headless executor.
    ///
    /// Pure: it copies published fields into plain numbers and returns. It
    /// writes nothing back, so it cannot change which orders the pipeline
    /// emits.
    #[must_use]
    pub fn read(executor: &Executor<Regolith>, me: PersistId) -> Self {
        let own = match executor.state(me) {
            Some(RegolithState::Craft(craft)) => Some(craft.pos),
            _ => None,
        };
        let Some(own_pos) = own else {
            return Self::default();
        };
        let (x, y, z) = own_pos.to_metres();
        #[allow(clippy::cast_possible_truncation)]
        let observer = Vec3::new(x as f32, y as f32, z as f32);
        let outside_m = outside_island_m(observer);

        // The director's own clock is the only honest source for "how long is
        // left": `site_active_until` is an absolute tick on the same
        // island-local clock `clock_tick` advances, so the difference is a
        // fact the host published rather than a local countdown that could
        // drift away from it.
        let mut bloom = None;
        for entity in executor.entities().copied() {
            let Some(RegolithState::BloomDirector(director)) = executor.state(entity) else {
                continue;
            };
            let (Some(site), Some(until)) = (director.site_pos, director.site_active_until) else {
                continue;
            };
            let (sx, sy, sz) = site.to_metres();
            #[allow(clippy::cast_possible_truncation)]
            let site_m = Vec3::new(sx as f32, sy as f32, sz as f32);
            bloom = Some(BloomBeacon {
                range_m: observer.distance(site_m),
                seconds_left: until.saturating_sub(director.clock_tick) / u64::from(TICK_HZ),
                rocks_alive: director.site_rocks_alive,
            });
            break;
        }

        Self {
            outside_m,
            tether_ramp: tether_ramp(outside_m),
            bloom,
        }
    }
}

/// The tether line: what the rule is doing to this craft, right now.
///
/// Inside the island it says so and stops. There is no third state, because
/// the rule has only two: outside the edge on some axis, or not.
#[must_use]
pub fn tether_line(view: &AnchorView) -> String {
    if view.outside_m <= 0.0 {
        return "IN ISLAND".to_owned();
    }
    let escape = TETHER_ESCAPE_SPEED_MMS as f32 / 1_000.0;
    // At full ramp the number is the rule's own settled outward speed; below
    // it the tether is still coming in, and saying "holding at N m/s" would
    // assert a speed the craft has not reached. Hence two phrasings.
    if view.tether_ramp >= 1.0 {
        format!(
            "OUTSIDE ISLAND {:.0} m - TETHER FULL, OUTBOUND {escape:.0} m/s",
            view.outside_m
        )
    } else {
        format!(
            "OUTSIDE ISLAND {:.0} m - TETHER {:.0}%",
            view.outside_m,
            view.tether_ramp * 100.0
        )
    }
}

/// The bloom beacon line: the campaign's own reason to stay.
///
/// ASCII only, like every other line this client draws: no font asset is
/// loaded, so a non-ASCII glyph renders as an empty box.
#[must_use]
pub fn bloom_line(view: &AnchorView) -> String {
    let Some(bloom) = view.bloom else {
        // Not "no bloom": a client that cannot see a director has not learned
        // that there is none. The two are different facts and the honest line
        // is the one that does not claim the stronger.
        return "NO BLOOM ANNOUNCED".to_owned();
    };
    format!(
        "BLOOM {:.0} m - {} ROCKS, {}s LEFT",
        bloom.range_m, bloom.rocks_alive, bloom.seconds_left
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn boundary_m() -> f32 {
        ISLAND_BOUNDARY_MM as f32 / 1_000.0
    }

    #[test]
    fn the_island_edge_is_a_box_and_not_a_radius() {
        let b = boundary_m();
        // A craft at the box corner is on the edge, not 41% outside it, which
        // is exactly what a radius would have claimed.
        assert_eq!(outside_island_m(Vec3::new(b, 0.0, b)), 0.0);
        assert_eq!(outside_island_m(Vec3::new(b + 100.0, 0.0, b)), 100.0);
        // Every axis is measured, because the rule measures every axis.
        assert_eq!(outside_island_m(Vec3::new(0.0, -b - 250.0, 0.0)), 250.0);
    }

    #[test]
    fn the_ramp_matches_the_rules_band_and_saturates() {
        let band = TETHER_BAND_MM as f32 / 1_000.0;
        assert_eq!(tether_ramp(0.0), 0.0);
        assert!((tether_ramp(band / 2.0) - 0.5).abs() < 1e-6);
        assert_eq!(tether_ramp(band), 1.0);
        assert_eq!(tether_ramp(band * 9.0), 1.0, "the ramp never exceeds full");
    }

    #[test]
    fn the_tether_line_says_nothing_while_a_craft_is_inside() {
        let view = AnchorView::default();
        assert_eq!(tether_line(&view), "IN ISLAND");
    }

    #[test]
    fn a_partly_ramped_tether_never_claims_a_speed_the_craft_has_not_reached() {
        let band = TETHER_BAND_MM as f32 / 1_000.0;
        let outside_m = band / 4.0;
        let view = AnchorView {
            outside_m,
            tether_ramp: tether_ramp(outside_m),
            bloom: None,
        };
        let line = tether_line(&view);
        assert!(line.contains("TETHER 25%"), "{line}");
        assert!(
            !line.contains("OUTBOUND"),
            "a tether still ramping in must not quote the settled speed: {line}"
        );
    }

    #[test]
    fn a_full_tether_quotes_the_rulesets_own_escape_speed() {
        let band = TETHER_BAND_MM as f32 / 1_000.0;
        let view = AnchorView {
            outside_m: band * 2.0,
            tether_ramp: 1.0,
            bloom: None,
        };
        let line = tether_line(&view);
        assert!(line.contains("TETHER FULL"), "{line}");
        assert!(line.contains("33 m/s"), "{line}");
    }

    #[test]
    fn an_unseen_director_is_reported_as_unheard_and_not_as_absent() {
        assert_eq!(bloom_line(&AnchorView::default()), "NO BLOOM ANNOUNCED");
    }

    #[test]
    fn the_beacon_prints_the_directors_own_numbers() {
        let view = AnchorView {
            outside_m: 0.0,
            tether_ramp: 0.0,
            bloom: Some(BloomBeacon {
                range_m: 812.4,
                seconds_left: 47,
                rocks_alive: 9,
            }),
        };
        assert_eq!(bloom_line(&view), "BLOOM 812 m - 9 ROCKS, 47s LEFT");
    }

    #[test]
    fn every_line_is_ascii_because_no_font_asset_is_loaded() {
        let band = TETHER_BAND_MM as f32 / 1_000.0;
        let views = [
            AnchorView::default(),
            AnchorView {
                outside_m: band / 3.0,
                tether_ramp: tether_ramp(band / 3.0),
                bloom: Some(BloomBeacon {
                    range_m: 1.0,
                    seconds_left: 1,
                    rocks_alive: 1,
                }),
            },
            AnchorView {
                outside_m: band * 3.0,
                tether_ramp: 1.0,
                bloom: None,
            },
        ];
        for view in views {
            for line in [tether_line(&view), bloom_line(&view)] {
                assert!(line.is_ascii(), "non-ASCII renders as a box: {line}");
            }
        }
    }
}
