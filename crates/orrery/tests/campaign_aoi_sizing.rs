//! The campaign's AOI sizing, checked against the framework it is sized in.
//!
//! `orrery_games` is Bevy-free, so it cannot read `SpatialConfig`; it restates
//! the hysteresis margin as `CAMPAIGN_HYSTERESIS_PER_MILLE`. This crate
//! depends on both, so it is the one place the restatement can be held to the
//! framework's own default — and the one place the geometry can be played out
//! rather than asserted from the same formula the constant was derived with.
//!
//! #545: Heavy's 900 m envelope out-ranged a 512 m edge's 460.8 m guarantee by
//! roughly 2x. #520 settled the same class by sizing the edge to the reach
//! that existed at the time; this holds it to the reach the table *has*.

use orrery_games::regolith::{
    weapon::WeaponKind, CAMPAIGN_CELL_EDGE_M, CAMPAIGN_HYSTERESIS_PER_MILLE,
    MAX_ENGAGEMENT_RANGE_MM, MAX_TARGET_RADIUS_MM,
};
use orrery_protocol::DEFAULT_CELL_EDGE_M;
use orrery_spatial::SpatialConfig;

/// The margin the campaign sizes against must be the margin the framework
/// actually applies, or the sizing is arithmetic about nothing.
#[test]
fn the_campaign_aoi_uses_the_frameworks_own_hysteresis_margin() {
    let framework = SpatialConfig::default().hysteresis_frac;
    let campaign = CAMPAIGN_HYSTERESIS_PER_MILLE as f32 / 1_000.0;
    assert!(
        (framework - campaign).abs() < 1e-6,
        "the campaign sizes against {campaign} but the framework commits with {framework}"
    );

    let quantum = CAMPAIGN_CELL_EDGE_M / DEFAULT_CELL_EDGE_M;
    assert!(
        (quantum - quantum.round()).abs() < 1e-9,
        "the campaign edge {CAMPAIGN_CELL_EDGE_M} m is not a whole coarsening of the \
         framework's {DEFAULT_CELL_EDGE_M} m"
    );
}

/// Which cell a position commits to, worst case: commitment lags by up to the
/// margin, so a body `margin` deep into a neighbour is still committed to the
/// cell it came from.
fn committed_cell_worst_case(pos_m: f64, edge_m: f64) -> i64 {
    let margin = edge_m * CAMPAIGN_HYSTERESIS_PER_MILLE as f64 / 1_000.0;
    // Approached from below: the body has crossed the face at 0 by less than
    // the margin, so it still holds the lower cell.
    (pos_m + margin).div_euclid(edge_m) as i64
}

/// Played out rather than restated: put the observer where commitment lags
/// worst, put a target at the longest range the ruleset will still resolve a
/// shot at, and check the target's cell is one of the observer's 27.
///
/// This is the relationship #545 is about. Widen any weapon past what the
/// edge guarantees and the far target leaves the block here, without this
/// test naming a single weapon or a single distance.
#[test]
fn a_target_at_the_longest_resolvable_range_is_still_in_the_observers_27_cells() {
    let edge = CAMPAIGN_CELL_EDGE_M;
    let range_m = MAX_ENGAGEMENT_RANGE_MM as f64 / 1_000.0;
    let margin = edge * CAMPAIGN_HYSTERESIS_PER_MILLE as f64 / 1_000.0;

    // The observer sits `margin` past the face of cell 0 while still
    // committed to it — the deepest commitment can lag.
    let observer = -margin;
    assert_eq!(
        committed_cell_worst_case(observer, edge),
        0,
        "the fixture must place the observer in its worst-case committed cell"
    );

    for &direction in &[-1.0f64, 1.0] {
        let target = observer + direction * range_m;
        let cell = target.div_euclid(edge) as i64;
        assert!(
            (-1..=1).contains(&cell),
            "a target {range_m} m away sits in cell {cell}, outside the observer's \
             3x3x3 block, at a {edge} m edge"
        );
    }

    // Tightness: the guarantee is a guarantee, not slack. One margin further
    // than the guaranteed radius does leave the block, so the check above is
    // measuring the boundary rather than sitting comfortably inside it.
    let beyond = observer - (edge - margin) - 1.0;
    assert!(
        !(-1..=1).contains(&(beyond.div_euclid(edge) as i64)),
        "past edge - margin must leave the block, or this fixture proves nothing"
    );
}

/// Every weapon, not just the longest: a shorter weapon paired with a wider
/// target must fit too.
#[test]
fn every_weapon_paired_with_the_widest_target_fits_the_guarantee() {
    let edge = CAMPAIGN_CELL_EDGE_M;
    let margin = edge * CAMPAIGN_HYSTERESIS_PER_MILLE as f64 / 1_000.0;
    let observer = -margin;

    for kind in WeaponKind::ALL {
        let envelope_m = (kind.weapon().reach_mm() + MAX_TARGET_RADIUS_MM) as f64 / 1_000.0;
        let cell = (observer - envelope_m).div_euclid(edge) as i64;
        assert!(
            (-1..=1).contains(&cell),
            "{kind:?} reaches {envelope_m} m, which leaves the observer's 27 cells"
        );
    }
}
