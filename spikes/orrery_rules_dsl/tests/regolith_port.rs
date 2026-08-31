//! The before/after, compiled against real Regolith types.
//!
//! `crates/orrery_games/src/regolith/visibility.rs` holds the workspace's only
//! production `StateView::neighbor` call, at line 171. This file rewrites that
//! function's read plumbing in the declared form and checks the two agree on
//! the predicate they were always computing.
//!
//! **Nothing is migrated.** `orrery_games` does not depend on this crate;
//! `REGOLITH_SCHEDULE` is untouched; `visibility::verify_claims` still runs.
//! What is established here is narrower and worth stating exactly: the
//! declaration is *expressible* against Regolith's real ruleset, real state
//! enum and real order vocabulary, and the predicate it wraps needs no change.
//!
//! # What moved, and what did not
//!
//! `verify_visibility` (`visibility.rs:195-231`) and `verify_collision`
//! (`visibility.rs:233-300`) already take borrowed states rather than a view —
//! `verify_collision`'s signature is *literally* an applier's already:
//!
//! ```text
//! fn verify_collision(me: PersistId, own_state: &RegolithState, other_id: PersistId,
//!                     other_state: &RegolithState, overflowed: &mut bool) -> Option<CollisionResolution>
//! ```
//!
//! So the migration is not a rewrite of Regolith's rules. It is the deletion of
//! `NeighborReadSlot`, its `target` impl, `NEIGHBOR_READ_SLOTS`,
//! `MAX_NEIGHBOR_READS` and the `.map` that reads them — 25 lines
//! (`visibility.rs:24-61` plus `169-172`) — in favour of a six-line `slots:`
//! list. The predicates below are transcribed unchanged apart from being
//! reachable from outside the crate.

use orrery_core::geometry::segment_intersects_sphere;
use orrery_core::{OrderedInputs, QPos, QVel, Ruleset};
use orrery_games::regolith::archetype::Archetype;
use orrery_games::regolith::order::{Order, Outcome};
use orrery_games::regolith::state::{Craft, RegolithState, Rock, RockTier};
use orrery_games::regolith::{
    Regolith, LOCK_ACQUISITION_TICKS, MAX_NEIGHBOR_READS, OCCLUSION_MARGIN_MM,
};
use orrery_protocol::PersistId;
use orrery_rules_dsl::{recorded_reads, RecordedReads};

/// Stand-in for `visibility::VerifiedClaims`, which is `pub(crate)`.
#[derive(Debug, Default, PartialEq, Eq)]
struct PortLocals {
    visibility: Option<Outcome>,
}

recorded_reads! {
    /// Regolith's audited claims read, in the declared form.
    pub REGOLITH_CLAIM_READS {
        rules:   Regolith,
        locals:  PortLocals,
        system:  "verify-claims",
        targets: ClaimTargets,
        frames:  ClaimFrames,
        slots:   [
            /// The craft whose lock a cover claim challenges.
            cover_locker,
            /// The rock a cover claim names as occluder.
            cover_rock,
            /// The counterparty a collision claim names.
            collision,
        ],
        resolve: claim_targets,
        apply:   verify_claims,
    }
}

/// `visibility.rs:154-168`, unchanged in substance: the cooldown gate, the
/// first `ClaimCover`, the first `Collide`. What is gone is the `NeighborReadSlot`
/// enum this used to feed.
fn claim_targets(
    _reader: PersistId,
    own: &RegolithState,
    inputs: &OrderedInputs<'_, Order>,
) -> ClaimTargets {
    let cover = matches!(
        own,
        RegolithState::Craft(craft) if craft.cover_claim_cooldown == 0
    )
    .then(|| {
        inputs.iter().find_map(|order| match order {
            Order::ClaimCover { locker, rock } => Some((*locker, *rock)),
            _ => None,
        })
    })
    .flatten();
    let collision = inputs.iter().find_map(|order| match order {
        Order::Collide { other } => Some(*other),
        _ => None,
    });
    ClaimTargets {
        cover_locker: cover.map(|(locker, _)| locker),
        cover_rock: cover.map(|(_, rock)| rock),
        collision,
    }
}

/// `visibility.rs:195-231`, transcribed. The signature is what changed: it took
/// a `&StateView` only to call `view.own()` and `view.entity()`, both of which
/// the declared form hands it directly.
fn verify_claims(
    reader: PersistId,
    own: &RegolithState,
    targets: &ClaimTargets,
    frames: &ClaimFrames,
    _inputs: &OrderedInputs<'_, Order>,
    locals: &mut PortLocals,
) {
    locals.visibility = verify_visibility(reader, own, targets, frames);
}

fn verify_visibility(
    target_id: PersistId,
    own: &RegolithState,
    targets: &ClaimTargets,
    frames: &ClaimFrames,
) -> Option<Outcome> {
    let RegolithState::Craft(target) = own else {
        return None;
    };
    if target.cover_claim_cooldown > 0 {
        return None;
    }
    let (Some(RegolithState::Craft(locker)), Some(RegolithState::Rock(rock))) =
        (&frames.cover_locker, &frames.cover_rock)
    else {
        return None;
    };
    let locker_id = targets.cover_locker?;
    // `visibility.rs:209`'s one surviving identity guard: an entity cannot be
    // both the locker and the occluder. The other two guards there —
    // `locker_id == target_id` and `rock_id == target_id` — are unrepresentable
    // outcomes in this form, because `StateView::neighbor` frames the reader's
    // own row absent and the applier receives `None`, not a state.
    if Some(locker_id) == targets.cover_rock {
        return None;
    }
    if locker.lock_target != Some(target_id) || locker.lock_progress != LOCK_ACQUISITION_TICKS {
        return None;
    }
    let radius = rock
        .tier
        .limits()
        .radius_mm
        .saturating_sub(OCCLUSION_MARGIN_MM);
    Some(Outcome::LockVisibility {
        locker: locker_id,
        target: target_id,
        occluded: segment_intersects_sphere(locker.pos, target.pos, rock.pos, radius),
    })
}

#[test]
fn the_declared_cap_is_regoliths_published_cap() {
    // `MAX_NEIGHBOR_READS` is `NEIGHBOR_READ_SLOTS.len()` at
    // `visibility.rs:47`, restated at `mod.rs:158` and returned from
    // `Ruleset::max_neighbor_reads` at `mod.rs:608`. In the declared form there
    // is one statement of it and the ruleset reads it off the type.
    assert_eq!(
        <ClaimFrames as RecordedReads>::MAX_NEIGHBOR_READS,
        MAX_NEIGHBOR_READS,
    );
    assert_eq!(
        <ClaimFrames as RecordedReads>::SLOT_NAMES,
        ["cover_locker", "cover_rock", "collision"],
    );
    assert_eq!(Regolith::honest().max_neighbor_reads(), MAX_NEIGHBOR_READS);
}

#[test]
fn the_ported_predicate_still_refuses_a_locker_that_is_also_the_occluder() {
    // `visibility.rs:209`'s surviving identity guard, exercised. The other two
    // guards on that line — `locker_id == target_id` and `rock_id ==
    // target_id` — cannot be written here at all: the applier receives states,
    // and `StateView::neighbor` frames the reader's own row absent, so a
    // self-read arrives as `None` rather than as an identifier to compare.
    let target_id = PersistId::new(1);
    let shared = PersistId::new(2);

    let mut locker = Craft::spawned(Archetype::Interceptor, QPos::default(), 0);
    locker.lock_target = Some(target_id);
    locker.lock_progress = LOCK_ACQUISITION_TICKS;
    let own = RegolithState::Craft(Craft::spawned(Archetype::Interceptor, QPos::default(), 0));
    let rock = Rock::spawned(RockTier::Large, 0, QPos::default(), QVel::default());

    let frames = ClaimFrames {
        cover_locker: Some(RegolithState::Craft(locker)),
        cover_rock: Some(RegolithState::Rock(rock)),
        collision: None,
    };
    let colliding = ClaimTargets {
        cover_locker: Some(shared),
        cover_rock: Some(shared),
        collision: None,
    };
    assert_eq!(
        verify_visibility(target_id, &own, &colliding, &frames),
        None
    );

    let distinct = ClaimTargets {
        cover_locker: Some(shared),
        cover_rock: Some(PersistId::new(3)),
        collision: None,
    };
    assert!(
        matches!(
            verify_visibility(target_id, &own, &distinct, &frames),
            Some(Outcome::LockVisibility { locker, target, .. }) if locker == shared && target == target_id,
        ),
        "the emitted outcome names the locker, which only the targets can supply",
    );
}
