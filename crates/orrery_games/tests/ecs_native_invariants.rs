//! **SPIKE #793 — propose-only. Do not merge.**
//!
//! The differential behind `docs/spikes/ecs-native-game-code.md`.
//!
//! Three arms over one corpus:
//!
//! 1. the shipped checks (`regolith::invariants::INVARIANTS`),
//! 2. the Bevy-free control (`regolith::native::CONTROL_INVARIANTS`),
//! 3. the `bevy_ecs`-native rewrite (`regolith::native::NativeInvariants`).
//!
//! Two obligations, and the file discharges both explicitly:
//!
//! * **Agreement.** Arms 2 and 3 must reach the same verdict as arm 1 on every
//!   sample in the corpus. A prettier rewrite that answers differently has not
//!   rewritten the check, it has replaced it.
//! * **Order.** Permuted *insertion* orders must yield equal sorted findings,
//!   and at least one pair of permutations must **disagree** on the unsorted
//!   ones. The second half is the load-bearing one: without it, the sort could
//!   be decorating an order that was already canonical, and the file would pass
//!   every assertion while measuring nothing — the same "agreement would be
//!   luck" failure `tier_h_projection_differential.rs` guards against.

use orrery_core::{evaluate, InvariantKind, QPos, QVel};
use orrery_games::regolith::archetype::Archetype;
use orrery_games::regolith::invariants::INVARIANTS;
use orrery_games::regolith::native::{
    Finding, NativeInvariants, Sample, CONTROL_INVARIANTS, STAGE1_SYSTEMS,
};
use orrery_games::regolith::state::{BloomDirector, Craft, Pickup, RegolithState, Rock, RockTier};
use orrery_games::regolith::weapon::WeaponKind;
use orrery_protocol::{PersistId, Tick};

const TICK: Tick = Tick::new(1_000);

/// One corpus row: a previous sample, a current sample, and a label.
struct Row {
    label: &'static str,
    previous: Option<RegolithState>,
    current: RegolithState,
    elapsed: u32,
}

fn craft(pos: QPos, vel: QVel, hull: i32) -> RegolithState {
    let mut craft = Craft::spawned(Archetype::Interceptor, pos, 0);
    craft.vel = vel;
    craft.hull = hull;
    if hull == 0 {
        craft.respawn_in = 1;
    }
    RegolithState::Craft(craft)
}

fn rock(pos: QPos, vel: QVel) -> RegolithState {
    RegolithState::Rock(Rock::spawned(RockTier::Large, 0, pos, vel))
}

fn pickup(pos: QPos) -> RegolithState {
    RegolithState::Pickup(Pickup::spawned(pos, WeaponKind::Stock, 60))
}

fn director() -> RegolithState {
    RegolithState::BloomDirector(BloomDirector::spawned())
}

fn mm(x: i64) -> QPos {
    QPos { x, y: 0, z: 0 }
}

fn mms(x: i64) -> QVel {
    QVel { x, y: 0, z: 0 }
}

/// The corpus. Every arm sees exactly these rows, in this order.
///
/// It covers all four `RegolithState` variants, both directions of every
/// check, the first-sample (`previous: None`) case, the respawn edge each
/// kinematic check exempts, and a kind change between samples.
fn corpus() -> Vec<Row> {
    vec![
        Row {
            label: "craft at rest",
            previous: Some(craft(mm(0), mms(0), 100)),
            current: craft(mm(0), mms(0), 100),
            elapsed: 1,
        },
        Row {
            label: "craft over its speed cap",
            previous: Some(craft(mm(0), mms(0), 100)),
            current: craft(mm(0), mms(10_000_000), 100),
            elapsed: 1,
        },
        Row {
            label: "craft accelerating impossibly",
            previous: Some(craft(mm(0), mms(0), 100)),
            current: craft(mm(0), mms(400_000), 100),
            elapsed: 1,
        },
        Row {
            label: "craft teleporting",
            previous: Some(craft(mm(0), mms(0), 100)),
            current: craft(mm(900_000_000), mms(0), 100),
            elapsed: 1,
        },
        Row {
            label: "craft on the respawn edge, exempt",
            previous: Some(craft(mm(0), mms(0), 0)),
            current: craft(mm(900_000_000), mms(400_000), 100),
            elapsed: 1,
        },
        Row {
            label: "craft first sample, no history",
            previous: None,
            current: craft(mm(0), mms(400_000), 100),
            elapsed: 0,
        },
        Row {
            label: "rock at rest",
            previous: Some(rock(mm(0), mms(0))),
            current: rock(mm(0), mms(0)),
            elapsed: 1,
        },
        Row {
            label: "rock over its speed cap",
            previous: Some(rock(mm(0), mms(0))),
            current: rock(mm(0), mms(9_000_000)),
            elapsed: 1,
        },
        Row {
            label: "rock teleporting",
            previous: Some(rock(mm(0), mms(0))),
            current: rock(mm(900_000_000), mms(0)),
            elapsed: 1,
        },
        Row {
            label: "pickup holding still",
            previous: Some(pickup(mm(500))),
            current: pickup(mm(500)),
            elapsed: 1,
        },
        Row {
            label: "pickup that moved at all",
            previous: Some(pickup(mm(0))),
            current: pickup(mm(900_000_000)),
            elapsed: 1,
        },
        Row {
            label: "bloom director, no kinematics",
            previous: Some(director()),
            current: director(),
            elapsed: 1,
        },
        Row {
            label: "an entity that changed kind",
            previous: Some(craft(mm(0), mms(0), 100)),
            current: rock(mm(0), mms(0)),
            elapsed: 1,
        },
    ]
}

fn subject(index: usize) -> PersistId {
    PersistId::new(index as u64 + 1)
}

/// Arm 1 and arm 2 share a shape: run a table of `Invariant`s over one sample.
fn table_findings(
    invariants: &[orrery_core::Invariant<RegolithState>],
    rows: &[Row],
) -> Vec<Finding> {
    let mut findings: Vec<Finding> = rows
        .iter()
        .enumerate()
        .filter_map(|(index, row)| {
            let sample = Sample {
                entity: subject(index),
                current: &row.current,
                tick: TICK,
                previous: row.previous.as_ref(),
                elapsed_ticks: row.elapsed,
            };
            evaluate(invariants, &sample.as_invariant_sample())
                .err()
                .map(|violation| Finding {
                    subject: subject(index),
                    validator: violation.validator,
                    kind: violation.kind,
                })
        })
        .collect();
    findings.sort_unstable();
    findings
}

/// Arm 3, over one insertion permutation.
fn native_findings(rows: &[Row], order: &[usize]) -> NativeInvariants {
    let mut native = NativeInvariants::new(TICK);
    for &index in order {
        let row = &rows[index];
        native.insert(&Sample {
            entity: subject(index),
            current: &row.current,
            tick: TICK,
            previous: row.previous.as_ref(),
            elapsed_ticks: row.elapsed,
        });
    }
    native.run();
    native
}

/// The insertion orders under test. More than one, and provably distinct.
fn permutations(len: usize) -> Vec<Vec<usize>> {
    let ascending: Vec<usize> = (0..len).collect();
    let descending: Vec<usize> = (0..len).rev().collect();
    // A fixed interleave, not a shuffle: a random permutation would make a
    // failure unreproducible, which is the wrong property for a differential.
    let interleaved: Vec<usize> = (0..len)
        .map(|slot| {
            if slot % 2 == 0 {
                slot / 2
            } else {
                len - 1 - slot / 2
            }
        })
        .collect();
    vec![ascending, descending, interleaved]
}

/// The shipped checks report **the first** failure per sample, not all of
/// them; the native pass reports every system that fired. Comparing them
/// directly would be comparing two different questions, so both sides are
/// reduced to "which entities did stage 1 flag, and could the shipped arm have
/// reported this validator".
fn flagged(findings: &[Finding]) -> Vec<PersistId> {
    let mut subjects: Vec<PersistId> = findings.iter().map(|finding| finding.subject).collect();
    subjects.sort_unstable();
    subjects.dedup();
    subjects
}

#[test]
fn the_native_pass_flags_exactly_what_the_shipped_checks_flag() {
    let rows = corpus();
    let shipped = table_findings(INVARIANTS, &rows);
    let native = native_findings(&rows, &(0..rows.len()).collect::<Vec<_>>()).findings();

    assert_eq!(
        flagged(&shipped),
        flagged(&native),
        "the bevy_ecs-native rewrite disagrees with the shipped checks about \
         which entities stage 1 flags"
    );
    // Not vacuous: the corpus must actually contain violations, or the
    // assertion above is `[] == []`.
    assert!(
        shipped.len() >= 6,
        "the corpus stopped producing violations; agreement above is vacuous"
    );
}

#[test]
fn the_native_pass_agrees_on_every_row_individually() {
    let rows = corpus();
    for (index, row) in rows.iter().enumerate() {
        let one = vec![Row {
            label: row.label,
            previous: row.previous.clone(),
            current: row.current.clone(),
            elapsed: row.elapsed,
        }];
        let shipped = table_findings(INVARIANTS, &one);
        let native = native_findings(&one, &[0]).findings();
        assert_eq!(
            shipped.is_empty(),
            native.is_empty(),
            "row {index} ({}) : shipped reported {shipped:?}, native reported {native:?}",
            row.label
        );
        // `evaluate` short-circuits on the **first** failing check in table
        // order; the native pass runs every system and reports all of them.
        // That is a genuine behavioural difference the spike records rather
        // than papers over, so the relation asserted is containment, not
        // equality: whatever the shipped arm reported must be among what the
        // native arm found.
        if let Some(shipped) = shipped.first() {
            assert!(
                native.iter().any(|found| found.kind == shipped.kind),
                "row {index} ({}) : shipped said {:?}, native found {native:?}",
                row.label,
                shipped.kind
            );
        }
    }
}

#[test]
fn the_bevy_free_control_agrees_with_the_shipped_checks() {
    let rows = corpus();
    let shipped: Vec<Finding> = table_findings(INVARIANTS, &rows)
        .into_iter()
        .filter(|finding| {
            matches!(
                finding.kind,
                InvariantKind::SpeedCap | InvariantKind::AccelerationCap
            )
        })
        .collect();
    let control = table_findings(CONTROL_INVARIANTS, &rows);
    assert_eq!(
        flagged(&shipped),
        flagged(&control),
        "the Bevy-free control arm disagrees with the shipped checks"
    );
    assert!(
        !control.is_empty(),
        "the control arm found nothing; agreement above is vacuous"
    );
}

#[test]
fn permuted_insertion_orders_yield_equal_sorted_findings() {
    let rows = corpus();
    let mut baseline: Option<Vec<Finding>> = None;
    for order in permutations(rows.len()) {
        let findings = native_findings(&rows, &order).findings();
        match &baseline {
            None => baseline = Some(findings),
            Some(first) => assert_eq!(
                *first, findings,
                "insertion order {order:?} changed the canonicalized findings"
            ),
        }
    }
    assert!(baseline.is_some_and(|findings| !findings.is_empty()));
}

/// The half that proves the sort is load-bearing.
///
/// If this ever fails, it does not mean the native pass became deterministic —
/// it means the permutation stopped reaching the archetype layout, and every
/// assertion in the test above became a tautology.
#[test]
fn some_permutation_disagrees_on_the_unsorted_findings() {
    let rows = corpus();
    let unsorted: Vec<Vec<Finding>> = permutations(rows.len())
        .into_iter()
        .map(|order| native_findings(&rows, &order).unsorted_findings())
        .collect();
    assert!(
        unsorted.iter().any(|left| *left != unsorted[0]),
        "no permutation changed query-visit order, so the canonical sort in \
         NativeInvariants::findings is not measured by this file"
    );
}

#[test]
fn the_stage1_schedule_declares_every_system_it_runs() {
    // `stage1_schedule` asserts this internally on construction; running a
    // pass is what executes that assertion.
    let rows = corpus();
    native_findings(&rows, &[0]);
    assert_eq!(STAGE1_SYSTEMS.len(), 5);
}
