//! The F-4 differential harness under its own lane (A10 §4, #745).
//!
//! What is asserted here, and the failure each named test exists to catch:
//!
//! | Test | The failure it names |
//! |---|---|
//! | self-differential reports parity | the runner is not deterministic, or a comparator compares nothing |
//! | the baseline pins the legacy side | a baseline that pins nothing — the comparison would be against "whatever this build now is" |
//! | no baseline refuses | the harness reporting "pass" when it had nothing to compare (A10 §4.4's first refusal) |
//! | partial artifacts refuse | a one-class comparison being reported as parity (A10 §4.4's second refusal) |
//! | a diverging candidate fails | comparators that cannot see a real divergence (the #738 blindness) |
//! | version skew refuses | differences being sorted into "expected" by guesswork (A10 §4.4's third refusal) |
//! | ruleset bump classifies | §4.3's migration-fixture arm, and the D-3/D-4 half named as unmet |
//! | schema/projection bumps route | the unimplemented D-3/D-4 arms named rather than treated as equal |
//! | D-2 compares bytes | a comparator comparing chain length instead of chain bytes |
//!
//! Both sides of every run here are the same implementation driven twice —
//! the deliberate self-differential of S7.1: it proves the runner, the
//! sealed-input equality and the comparators before any ECS exists.
//!
//! The version axes come from Regolith's real constants —
//! `Regolith::META` and
//! [`REGOLITH_COMPOSITION`](orrery_games::regolith::REGOLITH_COMPOSITION) —
//! except where a test declares a bump, which is how a migration comparison
//! is declared. Regolith is the subject because it is the game whose
//! composition manifest carries the projection and schema axes; Skirmish has
//! no manifest yet.

use std::collections::BTreeMap;

use orrery_core::{ComponentTypeId, Ruleset};
use orrery_games::diff::{
    collect_artifacts, compare, run_differential, Baseline, Class, Difference, Refusal, Side, Skew,
    Subject, Verdict, VersionAxes,
};
use orrery_games::golden;
use orrery_games::regolith::{Regolith, REGOLITH_COMPOSITION};
use orrery_games::scenario::{play, Scenario, SCENARIOS};
use orrery_games::{game::Tamper, Game};
use orrery_protocol::atrest::SchemaVersion;

/// The campaign's composition manifest carries no component schemas yet, so
/// the schema-bump arms are exercised with one declared component. When S7.2
/// populates the table, these tests switch to a real row.
const DECLARED_COMPONENT: ComponentTypeId = ComponentTypeId(0x5353_1001);

fn regolith_axes() -> VersionAxes {
    let mut schema_versions: BTreeMap<ComponentTypeId, SchemaVersion> = REGOLITH_COMPOSITION
        .component_schemas
        .iter()
        .map(|schema| (schema.id.component, schema.id.version))
        .collect();
    schema_versions.insert(DECLARED_COMPONENT, 0);
    VersionAxes {
        ruleset_version: Regolith::META.ruleset.version,
        projection_version: REGOLITH_COMPOSITION.projection_version.0,
        schema_versions,
    }
}

fn regolith_baseline() -> Baseline {
    Baseline {
        commit: "68cc738",
        axes: regolith_axes(),
        chains: Regolith::GOLDEN_CHAINS.to_vec(),
        outcome_chains: golden::REGOLITH_OUTCOMES.to_vec(),
    }
}

fn subject(label: &'static str, game: Regolith, axes: VersionAxes) -> Subject<Regolith> {
    Subject { label, game, axes }
}

fn scenario(name: &str) -> &'static Scenario {
    SCENARIOS
        .iter()
        .find(|scenario| scenario.name == name)
        .unwrap_or_else(|| panic!("{name}: not in the scenario table"))
}

/// The headline exit criterion: two implementations — here the same one,
/// deliberately — driven from identical sealed inputs produce byte-identical
/// D-1 and D-2 chains, and the verdict names what was and was not compared.
#[test]
fn a_self_differential_from_identical_sealed_inputs_reports_parity() {
    let baseline = regolith_baseline();
    for scenario in SCENARIOS {
        let verdict = run_differential(
            subject("legacy", Regolith::honest(), regolith_axes()),
            subject("candidate", Regolith::honest(), regolith_axes()),
            scenario,
            Some(&baseline),
        );
        assert_eq!(
            verdict,
            Verdict::Parity {
                compared: vec![Class::D1State, Class::D2Outcome],
                not_compared: vec![Class::D3Persistence, Class::D4Witness],
            },
            "{}/{}: the self-differential did not report parity",
            "regolith",
            scenario.name
        );
    }
}

/// The pin is real: a legacy side whose chain does not reproduce its
/// committed baseline fails with the digest named, rather than comparing
/// against "whatever this build now is".
#[test]
fn the_baseline_pins_the_legacy_side_to_its_committed_goldens() {
    let mut stale = regolith_baseline();
    stale.chains[0].1[0] ^= 0xff;
    let verdict = run_differential(
        subject("legacy", Regolith::honest(), regolith_axes()),
        subject("candidate", Regolith::honest(), regolith_axes()),
        scenario("solo"),
        Some(&stale),
    );
    let Verdict::Failure { differences } = verdict else {
        panic!("a drifting baseline produced {verdict:?} instead of failing");
    };
    assert!(
        differences
            .iter()
            .any(|(class, difference)| *class == Class::D1State
                && matches!(difference, Difference::BaselineDigest { .. })),
        "the drift was not named on the D-1 class: {differences:?}"
    );
}

/// A10 §4.4's first refusal: no committed baseline, no run — not a pass, not
/// a failure, a refusal. Both shapes: no baseline at all, and a baseline
/// whose tables do not cover the scenario being compared.
#[test]
fn a_differential_without_a_baseline_refuses_to_produce_a_verdict() {
    for scenario in SCENARIOS {
        let verdict = run_differential(
            subject("legacy", Regolith::honest(), regolith_axes()),
            subject("candidate", Regolith::honest(), regolith_axes()),
            scenario,
            None,
        );
        assert_eq!(
            verdict,
            Verdict::Refused(Refusal::MissingBaseline),
            "{}: a baseline-less run produced a verdict",
            scenario.name
        );
    }

    // A baseline that covers no scenario is a missing baseline for this
    // comparison.
    let mut empty = regolith_baseline();
    empty.chains.clear();
    empty.outcome_chains.clear();
    let verdict = run_differential(
        subject("legacy", Regolith::honest(), regolith_axes()),
        subject("candidate", Regolith::honest(), regolith_axes()),
        scenario("solo"),
        Some(&empty),
    );
    assert_eq!(
        verdict,
        Verdict::Refused(Refusal::MissingBaseline),
        "a baseline without this scenario's goldens produced a verdict"
    );
}

/// A10 §4.4's second refusal: a side that produced fewer than the
/// implemented classes' artifacts yields no verdict — the one class it did
/// produce is not compared, because a one-class parity claim is exactly the
/// partial-matrix pass this harness exists to refuse.
#[test]
fn a_side_that_produced_only_one_class_refuses_rather_than_passes() {
    let scenario = scenario("solo");
    let legacy_played = play(Regolith::honest(), scenario);
    let candidate_played = play(Regolith::honest(), scenario);

    let mut partial = collect_artifacts(&candidate_played, Side::Candidate, regolith_axes());
    partial.d2 = None;
    let verdict = compare(
        collect_artifacts(&legacy_played, Side::Legacy, regolith_axes()),
        partial,
    );
    assert_eq!(
        verdict,
        Verdict::Refused(Refusal::PartialArtifacts {
            side: Side::Candidate,
            missing: vec![Class::D2Outcome],
        }),
        "a one-class comparison was reported as a verdict"
    );

    // The same refusal on the legacy side, by symmetry.
    let mut partial = collect_artifacts(&legacy_played, Side::Legacy, regolith_axes());
    partial.d1 = None;
    let verdict = compare(
        partial,
        collect_artifacts(&candidate_played, Side::Candidate, regolith_axes()),
    );
    assert_eq!(
        verdict,
        Verdict::Refused(Refusal::PartialArtifacts {
            side: Side::Legacy,
            missing: vec![Class::D1State],
        }),
        "a legacy side missing its state chain produced a verdict"
    );
}

/// The comparator sensitivity proof: a genuinely diverging candidate — a
/// tampered build that keeps the honest ruleset identity — fails under equal
/// axes, in both classes. This is the property #738 showed the golden
/// battery cannot provide on its own.
#[test]
fn a_genuinely_diverging_candidate_fails_under_equal_axes() {
    let tampered = Regolith::tampered(Tamper::DamageInflation).expect("regolith has this tamper");
    // A tampered build keeps the honest ruleset identity: that is the whole
    // point of a cheat, and why the axes below are equal.
    assert_eq!(
        tampered.id(),
        Regolith::META.ruleset,
        "the tampered build changed its claimed identity"
    );
    let verdict = run_differential(
        subject("legacy", Regolith::honest(), regolith_axes()),
        subject("candidate", tampered, regolith_axes()),
        scenario("island"),
        Some(&regolith_baseline()),
    );
    let Verdict::Failure { differences } = verdict else {
        panic!("a tampered candidate produced {verdict:?} instead of failing");
    };
    for class in [Class::D1State, Class::D2Outcome] {
        assert!(
            differences.iter().any(|(found, _)| *found == class),
            "{} did not differ for a damage-inflated candidate: {differences:?}",
            class.name()
        );
    }
}

/// One declared skew, applied to a candidate side's axes.
type SkewCase = (&'static str, Box<dyn Fn(&mut VersionAxes)>);

/// A10 §4.4's third refusal: an axis that differs without a mechanical bump
/// — a version decrease, a changed schema membership, or more than one
/// bumped axis — is unclassifiable, because sorting it into "expected" would
/// be a judgement call. Each case here goes through the full runner, with
/// the skew declared on the candidate side.
#[test]
fn version_skew_without_a_bump_is_unclassifiable() {
    let baseline = regolith_baseline();
    let skew_cases: [SkewCase; 4] = [
        (
            "ruleset regressed",
            Box::new(|axes: &mut VersionAxes| {
                axes.ruleset_version -= 1;
            }),
        ),
        (
            "projection regressed",
            Box::new(|axes: &mut VersionAxes| {
                axes.projection_version -= 1;
            }),
        ),
        (
            "schema membership changed",
            Box::new(|axes: &mut VersionAxes| {
                axes.schema_versions.remove(&DECLARED_COMPONENT);
            }),
        ),
        (
            "two axes bumped at once",
            Box::new(|axes: &mut VersionAxes| {
                axes.ruleset_version += 1;
                axes.projection_version += 1;
            }),
        ),
    ];
    for (case, mutate) in &skew_cases {
        let mut axes = regolith_axes();
        mutate(&mut axes);
        let verdict = run_differential(
            subject("legacy", Regolith::honest(), regolith_axes()),
            subject("candidate", Regolith::honest(), axes),
            scenario("solo"),
            Some(&baseline),
        );
        // Every case refuses; none is classified as expected.
        let Verdict::Refused(Refusal::UnclassifiableSkew(skew)) = &verdict else {
            panic!("{case}: the skew produced {verdict:?} instead of a refusal");
        };
        if *case == "two axes bumped at once" {
            assert_eq!(skew, &Skew::Multiple, "{case}: wrong skew");
        }
    }
}

/// §4.3's migration-fixture arm: a bumped `RulesetId.version` turns D-1/D-2
/// differences into the new goldens to commit — a classification, never a
/// pass, with the rule's D-3/D-4 half named as unmet. The equal-chains arm
/// is classified too: a bump that moved no chain is still not a parity
/// verdict while D-3/D-4 are unproduced.
#[test]
fn a_ruleset_bump_classifies_d1_d2_differences_as_migration_fixtures() {
    let mut bumped = regolith_axes();
    bumped.ruleset_version += 1;

    // Differences under the bump: the migration fixtures.
    let tampered = Regolith::tampered(Tamper::DamageInflation).expect("regolith has this tamper");
    let verdict = run_differential(
        subject("legacy", Regolith::honest(), regolith_axes()),
        subject("candidate", tampered, bumped.clone()),
        scenario("island"),
        Some(&regolith_baseline()),
    );
    assert_eq!(
        verdict,
        Verdict::MigrationFixture {
            d1_differs: true,
            d2_differs: true,
            unmet: vec![Class::D3Persistence, Class::D4Witness],
        },
        "a ruleset bump with differences was not classified as a migration fixture"
    );

    // No differences under the bump: still not a parity claim.
    let verdict = run_differential(
        subject("legacy", Regolith::honest(), regolith_axes()),
        subject("candidate", Regolith::honest(), bumped),
        scenario("solo"),
        Some(&regolith_baseline()),
    );
    assert_eq!(
        verdict,
        Verdict::MigrationFixture {
            d1_differs: false,
            d2_differs: false,
            unmet: vec![Class::D3Persistence, Class::D4Witness],
        },
        "a ruleset bump without differences was reported as something other than \
         a fixture classification with unmet classes"
    );
}

/// §4.3's schema-bump arm: D-3 routes through the F-6 migration round-trip
/// (unimplemented in this lane), and D-1/D-2 are unchanged — differences
/// remain failures under a schema bump.
#[test]
fn a_schema_bump_keeps_d1_d2_differences_failures_and_names_d3() {
    let mut bumped = regolith_axes();
    bumped.schema_versions.insert(DECLARED_COMPONENT, 1);

    // Equality under the bump: the D-3 arm is named, not treated as equal.
    let verdict = run_differential(
        subject("legacy", Regolith::honest(), regolith_axes()),
        subject("candidate", Regolith::honest(), bumped.clone()),
        scenario("solo"),
        Some(&regolith_baseline()),
    );
    assert_eq!(
        verdict,
        Verdict::Refused(Refusal::ClassNotImplemented {
            classes: vec![Class::D3Persistence, Class::D4Witness],
        }),
        "a schema bump without differences was not refused to the unimplemented arm"
    );

    // Differences under the bump: D-1/D-2 unchanged, so still failures.
    let tampered = Regolith::tampered(Tamper::DamageInflation).expect("regolith has this tamper");
    let verdict = run_differential(
        subject("legacy", Regolith::honest(), regolith_axes()),
        subject("candidate", tampered, bumped),
        scenario("island"),
        Some(&regolith_baseline()),
    );
    let Verdict::Failure { differences } = verdict else {
        panic!("a schema bump with differences produced {verdict:?} instead of failing");
    };
    assert!(
        differences
            .iter()
            .any(|(class, _)| *class == Class::D2Outcome),
        "the D-2 difference disappeared under a schema bump: {differences:?}"
    );
}

/// §4.3's projection-bump arm: D-4 refuses cross-version claim comparison
/// (unimplemented in this lane), and D-1/D-2 are unchanged.
#[test]
fn a_projection_bump_refuses_cross_version_claims() {
    let mut bumped = regolith_axes();
    bumped.projection_version += 1;

    let verdict = run_differential(
        subject("legacy", Regolith::honest(), regolith_axes()),
        subject("candidate", Regolith::honest(), bumped),
        scenario("solo"),
        Some(&regolith_baseline()),
    );
    assert_eq!(
        verdict,
        Verdict::Refused(Refusal::ClassNotImplemented {
            classes: vec![Class::D4Witness],
        }),
        "a projection bump was not refused to the unimplemented D-4 arm"
    );
}

/// The D-2 comparator compares the chain's BYTES, not its length: two
/// outcome-chain materials that differ inside equal-length payloads are
/// different runs, and the verdict must say so.
#[test]
fn the_d2_comparator_compares_bytes_not_lengths() {
    let scenario = scenario("solo");
    let legacy = collect_artifacts(
        &play(Regolith::honest(), scenario),
        Side::Legacy,
        regolith_axes(),
    );
    let mut candidate = collect_artifacts(
        &play(Regolith::honest(), scenario),
        Side::Candidate,
        regolith_axes(),
    );

    // The materials are identical bytes; flip one payload byte. Same length,
    // different bytes — the exact shape a length comparator cannot see.
    let last = candidate.d2.as_mut().expect("candidate produced D-2").len() - 1;
    candidate.d2.as_mut().expect("candidate produced D-2")[last] ^= 0xff;
    {
        let (legacy_d2, candidate_d2) = (
            legacy.d2.as_deref().expect("legacy produced D-2"),
            candidate.d2.as_deref().expect("candidate produced D-2"),
        );
        assert_eq!(
            legacy_d2.len(),
            candidate_d2.len(),
            "the fixture must differ at equal length or the test is vacuous"
        );
        assert_ne!(
            legacy_d2, candidate_d2,
            "the fixture must differ in bytes or the test is vacuous"
        );
    }

    let verdict = compare(legacy, candidate);
    let Verdict::Failure { differences } = verdict else {
        panic!("byte-different, length-equal chains produced {verdict:?} instead of failing");
    };
    assert!(
        differences
            .iter()
            .any(|(class, _)| *class == Class::D2Outcome),
        "the D-2 difference was not named: {differences:?}"
    );
}
