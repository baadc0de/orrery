//! The campaign's engagement budget, checked against the framework it is
//! sized in.
//!
//! `orrery_games` is Bevy-free, so it cannot read `SpatialConfig`; it restates
//! the hysteresis margin as `CAMPAIGN_HYSTERESIS_PER_MILLE`. This crate
//! depends on both, so it is the one place the restatement can be held to the
//! framework's own default — and the one place the geometry can be played out
//! rather than asserted from the same formula the constant was derived with.
//!
//! #545: Heavy's 940 m envelope out-ranged a 512 m edge's guarantee by roughly
//! 2x. The edge stays at 512 m — a block wide enough to swallow the encounter
//! would delete the interest-churn surface the campaign exists to exercise —
//! so the weapon table was cut to fit instead.
//!
//! The bound the table is held to is §7's two-body `edge − 2m`, because
//! membership compares two independently hysteretic commitments. See
//! `CAMPAIGN_COMMITMENT_LAGS`.

use orrery_games::regolith::{
    campaign_engagement_budget_m, campaign_guaranteed_aoi_radius_m, weapon::WeaponKind,
    CAMPAIGN_CELL_EDGE_M, CAMPAIGN_COMMITMENT_LAGS, CAMPAIGN_HYSTERESIS_PER_MILLE,
    MAX_ENGAGEMENT_RANGE_MM, MAX_TARGET_RADIUS_MM,
};
use orrery_protocol::DEFAULT_CELL_EDGE_M;
use orrery_spatial::{pairwise_aoi_radius_m, SpatialConfig};

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

    let framework_budget = pairwise_aoi_radius_m(CAMPAIGN_CELL_EDGE_M as f32, framework) as f64;
    let campaign_budget = campaign_engagement_budget_m(CAMPAIGN_CELL_EDGE_M);
    assert!(
        (framework_budget - campaign_budget).abs() < 1e-4,
        "the campaign derives a {campaign_budget} m pairwise budget but the framework derives \
         {framework_budget} m"
    );

    let quantum = CAMPAIGN_CELL_EDGE_M / DEFAULT_CELL_EDGE_M;
    assert!(
        (quantum - quantum.round()).abs() < 1e-9,
        "the campaign edge {CAMPAIGN_CELL_EDGE_M} m is not a whole coarsening of the \
         framework's {DEFAULT_CELL_EDGE_M} m"
    );
}

/// The **highest** cell a body at `pos_m` may still be committed to.
///
/// `orrery_spatial`'s commitment latches: a body up to `margin` past a face is
/// still committed to the cell it came from. A body that has just moved down
/// across a face therefore still holds the cell above it.
fn highest_committed_cell(pos_m: f64, edge_m: f64) -> i64 {
    let margin = edge_m * CAMPAIGN_HYSTERESIS_PER_MILLE as f64 / 1_000.0;
    (pos_m + margin).div_euclid(edge_m) as i64
}

/// The **lowest** cell a body at `pos_m` may still be committed to: the mirror
/// case, a body that has just moved up across a face and still holds the cell
/// below it.
fn lowest_committed_cell(pos_m: f64, edge_m: f64) -> i64 {
    let margin = edge_m * CAMPAIGN_HYSTERESIS_PER_MILLE as f64 / 1_000.0;
    (pos_m - margin).div_euclid(edge_m) as i64
}

/// Played out rather than restated, and with **both** lags in it.
///
/// The observer sits a full margin outside its committed cell, and the target
/// — which latches its own commitment independently — sits a full margin
/// inside the block's face while still committed to the cell beyond it.
/// Neither body is where its cell says it is, and interest membership compares
/// the cells.
///
/// This is the relationship #545 is about. Widen any weapon past the budget
/// and the far target's committed cell leaves the block here, without this
/// test naming a single weapon or a single distance.
#[test]
fn a_target_at_the_longest_resolvable_range_is_still_in_the_observers_27_cells() {
    let edge = CAMPAIGN_CELL_EDGE_M;
    let range_m = MAX_ENGAGEMENT_RANGE_MM as f64 / 1_000.0;
    let margin = edge * CAMPAIGN_HYSTERESIS_PER_MILLE as f64 / 1_000.0;

    // Lag one: the observer is `margin` below the face of cell 0 and still
    // committed to it — the deepest commitment can lag.
    let observer = -margin;
    assert_eq!(
        highest_committed_cell(observer, edge),
        0,
        "the fixture must place the observer in its worst-case committed cell"
    );

    // Lag two, in whichever direction is adversarial: a target below the
    // observer may still hold the cell under it, one above may still hold the
    // cell over it.
    let below = lowest_committed_cell(observer - range_m, edge);
    let above = highest_committed_cell(observer + range_m, edge);
    for (cell, side) in [(below, "below"), (above, "above")] {
        assert!(
            (-1..=1).contains(&cell),
            "a target {range_m} m {side} the observer commits to cell {cell}, outside \
             the 3x3x3 block, at a {edge} m edge"
        );
    }

    // Tightness: the budget is the boundary, not slack. A metre past it does
    // leave the block on the binding side, so the check above is measuring the
    // edge of the guarantee rather than sitting comfortably inside it.
    let budget_m = campaign_engagement_budget_m(edge);
    assert!(
        !(-1..=1).contains(&lowest_committed_cell(observer - budget_m - 1.0, edge)),
        "past the engagement budget must leave the block, or this fixture proves nothing"
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
        let cell = lowest_committed_cell(observer - envelope_m, edge);
        assert!(
            (-1..=1).contains(&cell),
            "{kind:?} reaches {envelope_m} m, which leaves the observer's 27 cells"
        );
    }
}

/// The two-lag budget is the reason the table has real headroom rather than
/// the 20.8 m Stock used to have. It must actually be stricter than the §7
/// figure, or naming it changes nothing.
#[test]
fn the_engagement_budget_is_stricter_than_the_single_lag_guarantee() {
    let guaranteed = campaign_guaranteed_aoi_radius_m(CAMPAIGN_CELL_EDGE_M);
    let budget = campaign_engagement_budget_m(CAMPAIGN_CELL_EDGE_M);
    assert!(
        budget < guaranteed,
        "a budget of {budget} m is no stricter than the {guaranteed} m guarantee"
    );

    // The budget gives up one margin per lag beyond the first, which is the
    // whole content of `CAMPAIGN_COMMITMENT_LAGS`: membership compares two
    // committed cells, so two bodies may each be a margin from where their
    // cell says they are.
    let margin = CAMPAIGN_CELL_EDGE_M * CAMPAIGN_HYSTERESIS_PER_MILLE as f64 / 1_000.0;
    let given_up = (CAMPAIGN_COMMITMENT_LAGS - 1) as f64 * margin;
    assert!(
        (guaranteed - budget - given_up).abs() < 1e-9,
        "the budget must give up one margin per lag past the first: \
         {guaranteed} - {budget} is not {given_up}"
    );
}
