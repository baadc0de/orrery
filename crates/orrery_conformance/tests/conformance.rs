//! What the conformance crate has to get right before CI can trust it.
//!
//! Two distinct properties are under test here, and conflating them is how a
//! determinism suite ends up green and worthless:
//!
//! 1. **The corpus is reproducible.** Running it twice in one process must
//!    produce the same chain. This is the §8 golden-state rule and catches
//!    VC-4/VC-8 violations (hash iteration order, address hashing) instantly.
//! 2. **The comparator can fail.** A harness that reports agreement no matter
//!    what it is fed proves nothing, so every divergence class is fed to it
//!    deliberately and asserted on.
//!
//! The golden check is the third: it is what makes the plain `cargo test` job
//! on Windows and macOS a determinism gate in its own right, independent of the
//! artifact comparison.

use orrery_conformance::compare::{compare, Divergence};
use orrery_conformance::corpus::{run_all, run_case, Case, Report, CASES};
use orrery_conformance::ruleset::{Body, Command, Outcome};
use orrery_core::quantize::{QPos, QVel};
use orrery_core::ruleset::CoreCodec;
use orrery_protocol::PersistId;

/// Load the committed golden.
fn golden() -> Report {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/corpus/golden.json");
    let text = std::fs::read_to_string(path).expect("golden corpus is committed");
    serde_json::from_str(&text).expect("golden corpus parses")
}

#[test]
fn the_corpus_is_reproducible_in_process() {
    // §8's golden-state rule: two identical runs that diverge in-process are an
    // instant VC-4/VC-8 violation. This is the check that fails on the symptom
    // rather than on a spelling, which is why it matters more than the static
    // gates in scripts/core-gates.sh.
    for case in CASES {
        let a = run_case(case, false);
        let b = run_case(case, false);
        assert_eq!(a.chain, b.chain, "case {} is not reproducible", case.name);
        assert_eq!(a.final_states, b.final_states, "case {}", case.name);
    }
}

#[test]
fn retaining_per_tick_detail_does_not_change_the_result() {
    // The chain is what CI compares; the detail exists only to localize a
    // mismatch. If asking for detail changed the answer, a failing run could
    // not be diagnosed without perturbing the thing being diagnosed.
    for case in CASES {
        let compact = run_case(case, false);
        let detailed = run_case(case, true);
        assert_eq!(compact.chain, detailed.chain, "case {}", case.name);
        assert!(compact.tick_hashes.is_empty());
        assert!(!detailed.tick_hashes.is_empty());
    }
}

#[test]
fn this_platform_matches_the_committed_golden() {
    // The cross-platform gate, enforced from inside the ordinary test suite:
    // when this runs on the Windows and macOS legs of the matrix, a divergence
    // from the Linux-generated golden fails the build there. The artifact
    // comparison in CI is the same check with better diagnostics, not a
    // different one.
    let divergences = compare(&golden(), &run_all(false));
    assert!(
        divergences.is_empty(),
        "this platform diverges from the committed golden corpus: {}",
        divergences
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("; ")
    );
}

#[test]
fn the_comparator_reports_a_continuous_divergence() {
    let baseline = run_all(true);
    let mut drifted = baseline.clone();
    // One millimetre on one axis — the smallest representable drift, well
    // inside the 10 mm band. It must still fail: the band is what stops a
    // witness striking an honest player, not a licence for the corpus to move.
    drifted.cases[0].chain = "00".repeat(32);
    drifted.cases[0].final_states[0].pos_mm[0] += 1;

    let divergences = compare(&baseline, &drifted);
    assert_eq!(divergences.len(), 1);
    match &divergences[0] {
        Divergence::Case {
            name,
            max_pos_mm,
            discrete_differs,
            ..
        } => {
            assert_eq!(name, CASES[0].name);
            assert_eq!(*max_pos_mm, 1);
            assert!(!discrete_differs, "a position shift is not discrete state");
        }
        other => panic!("expected a case divergence, got {other:?}"),
    }
}

#[test]
fn the_comparator_flags_discrete_divergence_separately() {
    // A hit point that differs is categorically not platform drift (VC-5), and
    // the report has to say so — otherwise it reads as a tolerance question
    // when it is a correctness one.
    let baseline = run_all(true);
    let mut drifted = baseline.clone();
    drifted.cases[2].chain = "11".repeat(32);
    drifted.cases[2].final_states[0].hp -= 1;

    let divergences = compare(&baseline, &drifted);
    assert_eq!(divergences.len(), 1);
    match &divergences[0] {
        Divergence::Case {
            discrete_differs, ..
        } => assert!(discrete_differs),
        other => panic!("expected a case divergence, got {other:?}"),
    }
}

#[test]
fn the_comparator_localizes_the_first_diverging_tick() {
    let baseline = run_all(true);
    let mut drifted = baseline.clone();
    drifted.cases[0].chain = "22".repeat(32);
    let target = drifted.cases[0].tick_hashes[17].clone();
    drifted.cases[0].tick_hashes[17].hash = "33".repeat(32);

    match &compare(&baseline, &drifted)[0] {
        Divergence::Case {
            first_tick, entity, ..
        } => {
            assert_eq!(*first_tick, Some(target.tick));
            assert_eq!(*entity, Some(target.entity));
        }
        other => panic!("expected a case divergence, got {other:?}"),
    }
}

#[test]
fn a_different_ruleset_is_incomparable_rather_than_failing() {
    // Comparing two builds of different rules says nothing about determinism.
    // Answering "incomparable" rather than "diverged" is the same distinction
    // adjudication makes when a bundle predates retention (D11).
    let baseline = run_all(false);
    let mut other = baseline.clone();
    other.ruleset_version += 1;

    match &compare(&baseline, &other)[0] {
        Divergence::Incomparable { .. } => {}
        other => panic!("expected incomparable, got {other:?}"),
    }
}

#[test]
fn identical_reports_compare_equal() {
    // The pass condition, asserted explicitly: a comparator that never returns
    // empty would fail every build for the wrong reason.
    let report = run_all(true);
    assert!(compare(&report, &report.clone()).is_empty());
}

#[test]
fn body_encoding_round_trips() {
    // Canonicality is the whole requirement (docs/06 §3): two builds that
    // encode the same state differently produce different state hashes and
    // therefore a false deviation.
    let body = Body {
        pos: QPos {
            x: -12_345,
            y: 6_789,
            z: 0,
        },
        vel: QVel { x: 42, y: -7, z: 1 },
        heading_urad: 3_141_593,
        hp: 900,
        shield: -3,
        roll_fold: 0xDEAD_BEEF_CAFE_F00D,
    };
    let bytes = body.to_canonical();
    assert_eq!(Body::decode(&bytes).expect("round trip"), body);
}

#[test]
fn command_and_outcome_encodings_round_trip() {
    let commands = [
        Command::Thrust {
            accel_mmss: -4_000,
            turn_urad: 100_000,
        },
        Command::Attack {
            target: PersistId::new(9),
            power: 31,
        },
        Command::Damage { amount: -12 },
    ];
    for command in commands {
        let bytes = command.to_canonical();
        assert_eq!(Command::decode(&bytes).expect("round trip"), command);
    }

    let outcome = Outcome::DamageApplied {
        target: PersistId::new(4),
        amount: 17,
    };
    assert_eq!(
        Outcome::decode(&outcome.to_canonical()).expect("round trip"),
        outcome
    );
}

#[test]
fn combat_cases_actually_exercise_the_discrete_path() {
    // A corpus whose combat cases never landed a hit would compare bit-exact
    // integers that nothing ever changed — green, and proving nothing about
    // VC-5. Assert the fixtures do what their names claim.
    for case in CASES.iter().filter(|c| c.combat) {
        let digest = run_case(case, false);
        let damaged = digest
            .final_states
            .iter()
            .any(|s| s.hp < 1_000 || s.shield < 250);
        assert!(
            damaged,
            "combat case {} never applied damage — it is not testing VC-5",
            case.name
        );
    }
}

#[test]
fn isolating_every_entity_changes_nothing() {
    // The sharp end of the neighbour rule. `combat-isolated` runs each entity
    // in its own single-entity executor, so `StateView::neighbor` answers
    // `None` for everything — which is exactly what an adjudicator sees, since
    // `ReplayHarness::load_claimed_snapshot` installs one entity.
    //
    // If a rule ever branches on a neighbour's live state, this equality is
    // what breaks. It has to be asserted here rather than left to the golden,
    // because such a branch can be invisible in the attacker's own state hash:
    // the reference ruleset draws its roll and folds it into `roll_fold`
    // *before* any liveness test, so the attacker hashes identically either way
    // and only the emitted event differs. `verify_bundle` would pass; the
    // target's chain would be wrong.
    let isolated = CASES
        .iter()
        .find(|c| c.isolated)
        .expect("the corpus carries an isolated case");
    let shared = Case {
        name: "shared-twin",
        isolated: false,
        ..*isolated
    };

    let a = run_case(isolated, false);
    let b = run_case(&shared, false);
    assert_eq!(
        a.chain, b.chain,
        "isolated execution diverges from shared — a rule is reading a neighbour's live state"
    );
    assert_eq!(a.final_states, b.final_states);
}

#[test]
fn continuous_state_actually_moves() {
    // Likewise for VC-6: if nothing moved, `libm` was never meaningfully
    // exercised and the matrix would agree trivially.
    for case in CASES {
        let digest = run_case(case, false);
        let moved = digest
            .final_states
            .iter()
            .any(|s| s.pos_mm != [0, 0, 0] && s.vel_mms != [0, 0, 0]);
        assert!(moved, "case {} never moved anything", case.name);
    }
}
