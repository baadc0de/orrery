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
//! | a slot subset is not parity | a D-3 comparator that compares slot payloads and ignores the slot SET |
//! | a slot's bytes are compared | a D-3 comparator that compares the slot set and ignores the bytes |
//! | the journal is compared | a D-3 comparator blind to *when* a write happened |
//! | the claim is over quantized state | a D-4 projection that hashes the wrong bytes (the A7 X-C class, #738) |
//! | sub-lattice residue is not deviation | the same mutation seen from the false-conviction side |
//! | the adjudicator convicts a diverging candidate | a D-4 that merely diffs instead of adjudicating |
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
//! composition manifest carries the corpus these comparators are framed on;
//! Skirmish has a manifest too since #761, but no scenario corpus or golden
//! chains for the differential to replay against.

use std::collections::{BTreeMap, BTreeSet};

use orrery_core::{state_hash, CodecError, ComponentTypeId, CoreCodec, Quantized, Ruleset};
use orrery_games::diff::{
    claim_value, collect_artifacts, compare, cross_replay, run_differential, Baseline, Class,
    CrossReplay, Crossing, Difference, Refusal, Side, SideArtifacts, Skew, SlotKey, Subject,
    Verdict, VersionAxes,
};
use orrery_games::golden;
use orrery_games::regolith::{Regolith, REGOLITH_COMPOSITION};
use orrery_games::scenario::{play, Scenario, SCENARIOS};
use orrery_games::{game::Tamper, Game};
use orrery_protocol::atrest::SchemaVersion;
use orrery_protocol::{UniverseSeed, Verdict as AdjudicatedVerdict};

/// Regolith's one persisted component, read from the manifest that declares
/// it.
///
/// This used to reach past the manifest to `orrery_compose::registry::regolith`
/// because `REGOLITH_COMPOSITION.component_schemas` was empty (#750). It is
/// populated now, and the manifest is the harness's source for the schema
/// axis, as A10 §4.3 says it should be — the registry remains the reviewed
/// ledger the manifest is checked against, but the harness no longer needs to
/// know that. The row declares `P1` bulk persistence, so D-3 frames a real
/// slot per entity-tick and the classification is not vacuous — and since
/// #761 that declaration is the *only* statement of it, `classify_component`
/// having been retired.
const DECLARED_COMPONENT: ComponentTypeId = REGOLITH_COMPOSITION.component_schemas[0].id.component;

/// A second component named in the schema axis but **undeclared** by the
/// manifest, and so never persisted (D45 clause (c): no declaration, no
/// capability), held only so the schema-**membership** arm has a row it
/// can remove without taking the game's whole at-rest artifact with it.
/// Removing the persisted row instead would refuse on the missing D-3
/// artifact before the axes were ever compared, which is a different
/// refusal answering a different question.
const UNPERSISTED_COMPONENT: ComponentTypeId = ComponentTypeId(0x5353_1001);

/// D-3's at-rest slot set is exactly what the manifest declares persisted.
///
/// The named guard for #761's unified stage: **classification reaches D-3 from
/// the declaration and from nowhere else.** Before #761 the collector asked
/// `Regolith::classify_component`, and the manifest's capability table said
/// the same thing a second time; the two were kept in step by review, in the
/// one place a disagreement is a persistence-classification error that D-3
/// cannot catch because D-3 is what consumes it.
///
/// There is now no second statement, so the two *cannot* be made to disagree.
/// What is still expressible — and what this test kills — is the declaration
/// itself being wrong: flip `REGOLITH_COMPONENT_SCHEMAS`'s one row to `P0`
/// and the game persists nothing, which this test names rather than letting
/// the harness read an absent D-3 artifact as a differently-shaped refusal.
#[test]
fn d3_persists_exactly_the_components_the_manifest_declares_persisted() {
    let declared: BTreeSet<ComponentTypeId> = REGOLITH_COMPOSITION
        .component_schemas
        .iter()
        .filter(|schema| schema.capabilities.is_persisted())
        .map(|schema| schema.id.component)
        .collect();
    assert!(
        !declared.is_empty(),
        "REGOLITH_COMPOSITION must declare at least one persisted component: \
         it is the single source D-3 reads, and a game declaring none produces \
         no D-3 artifact at all"
    );

    let (legacy, _candidate) = honest_pair("solo");
    let d3 = legacy
        .d3
        .expect("a manifest declaring a persisted component must produce a D-3 artifact");
    let observed: BTreeSet<ComponentTypeId> = d3.slots.keys().map(|slot| slot.component).collect();
    assert_eq!(
        observed, declared,
        "D-3 must write a slot for exactly the components \
         REGOLITH_COMPOSITION.component_schemas declares persisted"
    );
    assert!(
        !observed.contains(&UNPERSISTED_COMPONENT),
        "a component the schema axis names but the manifest does not declare \
         has no capabilities, so it must write no at-rest slot"
    );
}

fn regolith_axes() -> VersionAxes {
    let mut schema_versions: BTreeMap<ComponentTypeId, SchemaVersion> = REGOLITH_COMPOSITION
        .component_schemas
        .iter()
        .map(|schema| (schema.id.component, schema.id.version))
        .collect();
    assert!(
        schema_versions.contains_key(&DECLARED_COMPONENT),
        "the manifest must declare the persisted component these axes are framed on"
    );
    schema_versions.insert(UNPERSISTED_COMPONENT, orrery_protocol::atrest::SCHEMA_V0);
    VersionAxes {
        ruleset_version: Regolith::META.ruleset.version,
        projection_version: REGOLITH_COMPOSITION.projection_version.0,
        schema_versions,
    }
}

/// Both sides' artifacts for one scenario, played twice from the same build.
///
/// The pair a comparator test needs: identical by construction, so any
/// difference a test then introduces is the only difference there is.
fn honest_pair(name: &str) -> (SideArtifacts, SideArtifacts) {
    let scenario = scenario(name);
    let legacy_played = play(Regolith::honest(), scenario);
    let candidate_played = play(Regolith::honest(), scenario);
    (
        collect_artifacts(
            &Regolith::honest(),
            &legacy_played,
            Side::Legacy,
            regolith_axes(),
        ),
        collect_artifacts(
            &Regolith::honest(),
            &candidate_played,
            Side::Candidate,
            regolith_axes(),
        ),
    )
}

/// The D-4 cross-replay for a pair of artifact sets, under the builds that
/// produced them.
fn crossing(
    name: &str,
    legacy_game: &Regolith,
    candidate_game: &Regolith,
    legacy: &SideArtifacts,
    candidate: &SideArtifacts,
) -> CrossReplay {
    cross_replay(
        legacy_game,
        candidate_game,
        legacy.d4.as_ref().expect("legacy produced D-4"),
        candidate.d4.as_ref().expect("candidate produced D-4"),
        UniverseSeed([scenario(name).seed_byte; 32]),
    )
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
                compared: vec![
                    Class::D1State,
                    Class::D2Outcome,
                    Class::D3Persistence,
                    Class::D4Witness,
                ],
                not_compared: vec![],
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
    let (legacy, candidate) = honest_pair("solo");
    let cross = crossing(
        "solo",
        &Regolith::honest(),
        &Regolith::honest(),
        &legacy,
        &candidate,
    );

    let mut partial = candidate.clone();
    partial.d2 = None;
    let verdict = compare(legacy.clone(), partial, Some(&cross));
    assert_eq!(
        verdict,
        Verdict::Refused(Refusal::PartialArtifacts {
            side: Side::Candidate,
            missing: vec![Class::D2Outcome],
        }),
        "a three-class comparison was reported as a verdict"
    );

    // The same refusal on the legacy side, by symmetry.
    let mut partial = legacy.clone();
    partial.d1 = None;
    let verdict = compare(partial, candidate.clone(), Some(&cross));
    assert_eq!(
        verdict,
        Verdict::Refused(Refusal::PartialArtifacts {
            side: Side::Legacy,
            missing: vec![Class::D1State],
        }),
        "a legacy side missing its state chain produced a verdict"
    );

    // A side that persists nothing produced no D-3 artifact at all, and the
    // refusal names D-3 rather than reading two empty slot tables as
    // agreement.
    let mut partial = candidate.clone();
    partial.d3 = None;
    let verdict = compare(legacy.clone(), partial, Some(&cross));
    assert_eq!(
        verdict,
        Verdict::Refused(Refusal::PartialArtifacts {
            side: Side::Candidate,
            missing: vec![Class::D3Persistence],
        }),
        "a side with no at-rest artifact produced a verdict"
    );

    // D-4 is two halves, and half a class is no class: the claim values alone
    // are not a D-4 comparison while the cross-replay has not run.
    let verdict = compare(legacy, candidate, None);
    assert_eq!(
        verdict,
        Verdict::Refused(Refusal::CrossReplayNotRun),
        "a comparison without the cross-replay leg produced a verdict"
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
    for class in [
        Class::D1State,
        Class::D2Outcome,
        Class::D3Persistence,
        Class::D4Witness,
    ] {
        assert!(
            differences.iter().any(|(found, _)| *found == class),
            "{} did not differ for a damage-inflated candidate: {differences:?}",
            class.name()
        );
    }
    // And the conviction: the *existing* adjudicator, not this harness,
    // returned a non-exonerating verdict in both directions.
    for crossing in [
        Crossing::LegacyClaimsCandidateLogs,
        Crossing::CandidateClaimsLegacyLogs,
    ] {
        assert!(
            differences
                .iter()
                .any(|(class, difference)| *class == Class::D4Witness
                    && matches!(
                        difference,
                        Difference::CrossReplay { crossing: found, .. } if *found == crossing
                    )),
            "{} did not convict a damage-inflated candidate: {differences:?}",
            crossing.name()
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
                axes.schema_versions.remove(&UNPERSISTED_COMPONENT);
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
            d3_differs: true,
            d4_differs: true,
            unmet: vec![],
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
            d3_differs: false,
            // Nothing moved: the axes are *declared* build identity, while
            // both sides here are one build. A real ruleset bump would carry
            // a different `RulesetId` into every claim and frame, and the
            // adjudicator would refuse the crossing as `UnknownRuleset` —
            // which is the same signal by a different route, and why this
            // arm classifies rather than passing.
            d4_differs: false,
            unmet: vec![],
        },
        "a ruleset bump without differences was reported as something other than \
         a fixture classification"
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
            classes: vec![Class::D3Persistence],
        }),
        "a schema bump without differences was not refused to the F-6 round-trip arm"
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
        Verdict::Refused(Refusal::CrossVersionClaims {
            legacy: REGOLITH_COMPOSITION.projection_version.0,
            candidate: REGOLITH_COMPOSITION.projection_version.0 + 1,
        }),
        "a projection bump was not refused as a cross-version claim comparison"
    );
}

/// The D-2 comparator compares the chain's BYTES, not its length: two
/// outcome-chain materials that differ inside equal-length payloads are
/// different runs, and the verdict must say so.
#[test]
fn the_d2_comparator_compares_bytes_not_lengths() {
    let (legacy, mut candidate) = honest_pair("solo");
    let cross = crossing(
        "solo",
        &Regolith::honest(),
        &Regolith::honest(),
        &legacy,
        &candidate,
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

    let verdict = compare(legacy, candidate, Some(&cross));
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

// ---------------------------------------------------------------------------
// D-3 — persistence
// ---------------------------------------------------------------------------

/// D-3's slot-set half, and the failure it exists to catch: **a candidate that
/// writes a correct subset of slots is not at parity.**
///
/// The fixture is built so a *payload-only* comparator provably cannot see
/// it. Every slot the candidate still holds is asserted byte-identical to the
/// legacy side's, and the journal is untouched — so the only observable
/// difference in the whole class is the missing key. A comparator that walked
/// the slots both sides hold and compared their bytes would report parity
/// over a persistence layer that silently dropped a row.
#[test]
fn a_candidate_producing_a_subset_of_slots_is_not_parity() {
    let (legacy, mut candidate) = honest_pair("solo");
    let cross = crossing(
        "solo",
        &Regolith::honest(),
        &Regolith::honest(),
        &legacy,
        &candidate,
    );

    let legacy_slots = &legacy.d3.as_ref().expect("legacy produced D-3").slots;
    let dropped: SlotKey = *legacy_slots
        .keys()
        .nth(legacy_slots.len() / 2)
        .expect("the run wrote at least one slot");

    {
        let persistence = candidate.d3.as_mut().expect("candidate produced D-3");
        assert!(
            persistence.slots.remove(&dropped).is_some(),
            "the fixture removed a slot the candidate never wrote"
        );
        assert_eq!(
            persistence.slots.len() + 1,
            legacy_slots.len(),
            "the fixture must differ by exactly one slot or it is not a subset"
        );
        // Vacuity, stated: everything the candidate still holds agrees
        // byte-for-byte, so nothing but the slot SET distinguishes the sides.
        for (slot, bytes) in &persistence.slots {
            assert_eq!(
                legacy_slots.get(slot),
                Some(bytes),
                "{slot:?}: the fixture perturbed a payload; the test would not be \
                 isolating the slot-set half"
            );
        }
    }
    assert_eq!(
        legacy.d3.as_ref().expect("legacy produced D-3").journal,
        candidate
            .d3
            .as_ref()
            .expect("candidate produced D-3")
            .journal,
        "the fixture perturbed the journal; the test would not be isolating the \
         slot-set half"
    );

    let verdict = compare(legacy, candidate, Some(&cross));
    let Verdict::Failure { differences } = verdict else {
        panic!("a candidate writing a subset of slots produced {verdict:?} instead of failing");
    };
    assert!(
        differences
            .iter()
            .any(|(class, difference)| *class == Class::D3Persistence
                && matches!(
                    difference,
                    Difference::SlotSet { missing, extra }
                        if missing.contains(&dropped) && extra.is_empty()
                )),
        "the missing slot was not named on the D-3 class: {differences:?}"
    );
}

/// D-3's per-slot half: the framed bytes are compared, not merely the set of
/// keys, and not their lengths.
///
/// The fixture flips one byte inside one slot's framed bytes. The slot sets
/// are equal, the framed lengths are equal, and the journal is untouched — so
/// a comparator that checked set equality alone, or lengths, provably cannot
/// distinguish the sides. This is D-3's own reason to exist: an encoding
/// change that leaves semantics alone produces identical D-1/D-2 chains and
/// incompatible stored bytes (A10 §4.2).
#[test]
fn the_d3_comparator_compares_slot_bytes_not_only_the_slot_set() {
    let (legacy, mut candidate) = honest_pair("solo");
    let cross = crossing(
        "solo",
        &Regolith::honest(),
        &Regolith::honest(),
        &legacy,
        &candidate,
    );

    let touched: SlotKey = *legacy
        .d3
        .as_ref()
        .expect("legacy produced D-3")
        .slots
        .keys()
        .next()
        .expect("the run wrote at least one slot");
    {
        let persistence = candidate.d3.as_mut().expect("candidate produced D-3");
        let bytes = persistence
            .slots
            .get_mut(&touched)
            .expect("both sides wrote this slot");
        let last = bytes.len() - 1;
        bytes[last] ^= 0xff;
    }

    let legacy_slots = &legacy.d3.as_ref().expect("legacy produced D-3").slots;
    let candidate_slots = &candidate.d3.as_ref().expect("candidate produced D-3").slots;
    assert!(
        legacy_slots.keys().eq(candidate_slots.keys()),
        "the fixture must keep the slot sets equal or the test is vacuous"
    );
    assert_eq!(
        legacy_slots[&touched].len(),
        candidate_slots[&touched].len(),
        "the fixture must differ at equal length or a length comparator would pass"
    );
    assert_ne!(
        legacy_slots[&touched], candidate_slots[&touched],
        "the fixture must differ in bytes or the test is vacuous"
    );

    let verdict = compare(legacy, candidate, Some(&cross));
    let Verdict::Failure { differences } = verdict else {
        panic!("byte-different, set-equal slots produced {verdict:?} instead of failing");
    };
    assert!(
        differences
            .iter()
            .any(|(class, difference)| *class == Class::D3Persistence
                && matches!(difference, Difference::SlotBytes { slot, .. } if *slot == touched)),
        "the differing slot was not named: {differences:?}"
    );
}

/// D-3's second artifact: the journal a `feed_uplink`-shaped producer would
/// have queued.
///
/// The fixture drops one queued record while leaving every slot in place, so
/// the slot table — set and bytes — is identical on both sides. A comparator
/// that stopped at the slots would call this parity, and it is not: the
/// journal carries *when* a write happened, which the slot table does not.
#[test]
fn the_d3_journal_is_compared_and_not_only_the_slots() {
    let (legacy, mut candidate) = honest_pair("solo");
    let cross = crossing(
        "solo",
        &Regolith::honest(),
        &Regolith::honest(),
        &legacy,
        &candidate,
    );

    {
        let persistence = candidate.d3.as_mut().expect("candidate produced D-3");
        assert!(
            persistence.journal.len() > 1,
            "the fixture needs a journal with something in it"
        );
        persistence.journal.remove(persistence.journal.len() / 2);
    }

    let legacy_persistence = legacy.d3.as_ref().expect("legacy produced D-3");
    let candidate_persistence = candidate.d3.as_ref().expect("candidate produced D-3");
    assert_eq!(
        legacy_persistence.slots, candidate_persistence.slots,
        "the fixture must leave the slots identical or it is not isolating the journal"
    );
    assert_ne!(
        legacy_persistence.journal, candidate_persistence.journal,
        "the fixture must change the journal or the test is vacuous"
    );

    let verdict = compare(legacy, candidate, Some(&cross));
    let Verdict::Failure { differences } = verdict else {
        panic!("a short journal produced {verdict:?} instead of failing");
    };
    assert!(
        differences
            .iter()
            .any(|(class, difference)| *class == Class::D3Persistence
                && matches!(difference, Difference::JournalRecords { .. })),
        "the journal difference was not named: {differences:?}"
    );
}

// ---------------------------------------------------------------------------
// D-4 — witness
// ---------------------------------------------------------------------------

/// A state deliberately **off** the millimetre lattice until `quantize()`
/// snaps it.
///
/// Every `CoreState` in this workspace already stores lattice integers, which
/// is exactly why #738 got through: with canonical `quantize()` broken, both
/// of Regolith's committed golden chains stayed green, because the snap is a
/// no-op on every shipped fixture and the scenarios structurally cannot reach
/// the stage. A D-4 pin written against a shipped game would inherit that
/// blindness, so the projection is pinned against a state that is off the
/// lattice by construction — the same device
/// `orrery_conformance/tests/quantize_pin.rs` uses, applied to the
/// differential harness's own projection.
#[derive(Debug, Clone, PartialEq, Eq)]
struct OffLattice {
    /// Micrometres. The lattice is millimetres, so anything that is not a
    /// multiple of 1000 sits between two lattice points.
    x_um: i64,
}

impl OffLattice {
    const LATTICE_UM: i64 = 1_000;
}

impl CoreCodec for OffLattice {
    fn encode(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.x_um.to_le_bytes());
    }

    fn decode(bytes: &[u8]) -> Result<Self, CodecError> {
        let raw: [u8; 8] = bytes.try_into().map_err(|_| CodecError("bad length"))?;
        Ok(Self {
            x_um: i64::from_le_bytes(raw),
        })
    }
}

impl Quantized for OffLattice {
    fn quantize(&mut self) {
        self.x_um = self.x_um.div_euclid(Self::LATTICE_UM) * Self::LATTICE_UM;
    }
}

/// D-4's projection is `blake3(CoreCodec(quantize(state)))` — WP-1's unit,
/// WP-4's order — and the quantization is part of the projection rather than
/// an assumption about the state handed in.
///
/// This is the A7 X-C class stated as a test: a projection that hashes the
/// wrong bytes while the run persists the right ones. The vacuity self-check
/// is the load-bearing half — a fixture that happened to sit on the lattice
/// would make raw and quantized bytes identical and the assertion below would
/// hold for a comparator that never quantized at all.
#[test]
fn the_d4_claim_is_over_the_quantized_state_not_the_raw_one() {
    let raw = OffLattice { x_um: 1_567 };
    let mut snapped = raw.clone();
    snapped.quantize();

    assert_ne!(
        raw.to_canonical(),
        snapped.to_canonical(),
        "the fixture is on the lattice, so this test would pin nothing"
    );
    assert_eq!(
        claim_value(&raw),
        state_hash(&snapped),
        "the claim is not the hash of the quantized state"
    );
    assert_ne!(
        claim_value(&raw),
        state_hash(&raw),
        "the claim hashed the raw state — the A7 X-C failure, live"
    );
}

/// The same mutation seen from the side that would convict an honest peer.
///
/// Two states that differ only *below* the lattice are the same state as far
/// as the canonical projection is concerned: they quantize to one point, and
/// a claim commits to what replication and persistence saw (VC-7, WP-4). A
/// D-4 comparator that hashed raw bytes would report these as a deviation —
/// IV-2's false-deviation hazard, and a false conviction discovered in the
/// field rather than here.
#[test]
fn d4_reads_sub_lattice_residue_as_agreement_not_deviation() {
    let left = OffLattice { x_um: 1_567 };
    let right = OffLattice { x_um: 1_099 };

    assert_ne!(
        state_hash(&left),
        state_hash(&right),
        "the two fixtures have identical raw bytes, so this test would pin nothing"
    );
    assert_eq!(
        claim_value(&left),
        claim_value(&right),
        "sub-lattice residue was read as a claim difference"
    );
}

/// D-4's second half, on an honest self-differential: the **existing
/// adjudicator** — `orrery_core::verify_bundle`, the function `persistd`
/// reaches real verdicts with — exonerates in both directions.
///
/// The vacuity guard is the count: a cross-replay that adjudicated nothing at
/// all would trivially have no unclean verdict, and would be the "our
/// fixtures cannot tell" result this harness exists to distinguish from
/// parity.
#[test]
fn the_existing_adjudicator_exonerates_an_honest_self_differential() {
    let (legacy, candidate) = honest_pair("duel");
    let cross = crossing(
        "duel",
        &Regolith::honest(),
        &Regolith::honest(),
        &legacy,
        &candidate,
    );

    // Two entities in `duel`, two crossings each.
    assert_eq!(
        cross.verdicts.len(),
        4,
        "the cross-replay did not adjudicate both entities in both directions: {:?}",
        cross.verdicts
    );
    for crossing in [
        Crossing::LegacyClaimsCandidateLogs,
        Crossing::CandidateClaimsLegacyLogs,
    ] {
        assert_eq!(
            cross
                .verdicts
                .iter()
                .filter(|(found, _, _)| *found == crossing)
                .count(),
            2,
            "{} adjudicated the wrong number of entities",
            crossing.name()
        );
    }
    for (crossing, entity, verdict) in &cross.verdicts {
        assert_eq!(
            *verdict,
            AdjudicatedVerdict::Exonerates,
            "{}: entity {entity:?} was not exonerated on an honest self-differential",
            crossing.name()
        );
    }
    assert!(
        cross.unclean().is_empty(),
        "an honest self-differential produced unclean verdicts: {:?}",
        cross.unclean()
    );
}

/// A diverging candidate is **convicted**, not merely diffed: the adjudicator
/// returns `Confirms` on the crossed evidence, which is the same verdict a
/// witness would carry into a real dispute.
#[test]
fn the_existing_adjudicator_convicts_a_diverging_candidate() {
    let tampered = Regolith::tampered(Tamper::DamageInflation).expect("regolith has this tamper");
    let scenario = scenario("duel");
    let legacy_played = play(Regolith::honest(), scenario);
    let candidate_played = play(tampered, scenario);
    let legacy = collect_artifacts(
        &Regolith::honest(),
        &legacy_played,
        Side::Legacy,
        regolith_axes(),
    );
    let candidate = collect_artifacts(
        &tampered,
        &candidate_played,
        Side::Candidate,
        regolith_axes(),
    );

    let cross = crossing("duel", &Regolith::honest(), &tampered, &legacy, &candidate);
    let unclean = cross.unclean();
    assert!(
        !unclean.is_empty(),
        "a damage-inflating candidate was exonerated by the adjudicator"
    );
    for crossing in [
        Crossing::LegacyClaimsCandidateLogs,
        Crossing::CandidateClaimsLegacyLogs,
    ] {
        assert!(
            unclean.iter().any(|(found, _, verdict)| *found == crossing
                && matches!(verdict, AdjudicatedVerdict::Confirms { .. })),
            "{} did not confirm a deviation: {unclean:?}",
            crossing.name()
        );
    }
}
