//! The F-4 differential parity harness, classes D-1 and D-2 (A10 §4).
//!
//! Per scenario, the harness executes two implementations from **identical
//! sealed inputs** — the same universe seed, the same absolute tick window,
//! the same initial population, and the same log-ordered inputs ([`Play`]'s
//! sealed artifact, produced once by the legacy side's own run) — and compares:
//!
//! - **D-1 state**: the per-tick state chain, by byte equality of the chain
//!   material ([`Class::D1State`]);
//! - **D-2 outcome**: the outcome chain — canonical event bytes in emission
//!   order, materialized identifiers in install order, routed delivery pairs
//!   in delivery order — by byte equality of the chain material
//!   ([`Class::D2Outcome`]).
//!
//! Both comparators compare **bytes, not lengths and not digests**: two runs
//! whose outcome records differ inside equal-length payloads are different
//! runs, and a comparator that could not see that is the A10 §4.2 blindness
//! this harness exists to end.
//!
//! # This lane is a self-differential, deliberately
//!
//! Today both sides are the *same* implementation driven twice: the legacy
//! side plays the scenario and seals its inputs; the candidate side replays
//! from exactly those sealed inputs through a fresh instance of the same
//! code. That proves the runner is deterministic and that the comparators
//! actually compare (a tampered candidate diverges and fails), before there
//! is any ECS to compare against. When the S7.4 candidate exists, only the
//! [`Subject`] changes; the runner, the sealed-input equality, the
//! comparators and the classification do not.
//!
//! # The refusals are the point
//!
//! Per A10 §4.4, the harness **refuses to produce a verdict** — never passes,
//! never fails — when a baseline is missing, when a side produced fewer than
//! the implemented classes' artifacts, or when version axes differ without a
//! mechanical bump. A harness that reports "pass" when it had nothing to
//! compare is the exact failure this lane exists to prevent, so every
//! refusal is a named [`Refusal`] variant with its own test.
//!
//! # Expected-difference classification (A10 §4.3)
//!
//! Keyed by version axes, never by judgement calls. The axes are
//! [`VersionAxes`]: `RulesetId.version`, the witness projection framing
//! version, and the per-component schema-version set. `RulesetId.digest` is
//! deliberately absent (A10 §4.3's X-1 caveat: the digest is a placeholder
//! constant today, so "equal digest" means "same constant" and carries no
//! information — a harness keyed on it would report identity between
//! arbitrarily different builds).
//!
//! - equal axes → any difference in any class is a
//!   [`Verdict::Failure`];
//! - bumped `RulesetId.version` → D-1/D-2 differences are the migration
//!   fixtures to commit as the new goldens ([`Verdict::MigrationFixture`]) —
//!   a classification, never a pass, because the rule's other half (D-3/D-4
//!   must still match for unchanged schemas) needs evidence this lane does
//!   not produce;
//! - bumped `SchemaVersion` on a component → D-3 routes through the F-6
//!   migration round-trip (unimplemented here); D-1/D-2 differences remain
//!   failures;
//! - bumped `projection_version` → D-4 refuses cross-version claim
//!   comparison (unimplemented here); D-1/D-2 differences remain failures.
//!
//! Where a rule refers to D-3/D-4 — not in this lane — the harness names the
//! unimplemented arm ([`Refusal::ClassNotImplemented`], and `not_compared` on
//! every verdict) rather than silently treating the class as equal. A
//! decrease on any axis, a changed schema membership, or more than one
//! bumped axis at once has no rule and is refused as
//! [`Refusal::UnclassifiableSkew`].

use std::collections::BTreeMap;

use orrery_core::{ComponentTypeId, CoreCodec};
use orrery_protocol::atrest::SchemaVersion;
use orrery_protocol::Tick;

use crate::game::Game;
use crate::scenario::{OutcomeEntry, Play, Scenario, SealedScenario, TickRecord};

/// The four artifact classes of A10 §4.1.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Class {
    /// The per-tick state chain (F-1's fold), compared as bytes.
    D1State,
    /// The outcome chain (F-2's fold: canonical event bytes, materialized
    /// identifiers, routed delivery pairs), compared as bytes.
    D2Outcome,
    /// Encoded at-rest bytes and journal records. **Not implemented in this
    /// lane** (S7.2's class); every rule that needs it names the gap instead
    /// of treating the class as equal.
    D3Persistence,
    /// Per-entity per-tick claim values and cross-replay verdicts. **Not
    /// implemented in this lane** (S7.3's class).
    D4Witness,
}

/// The classes this lane implements and compares.
pub const IMPLEMENTED_CLASSES: [Class; 2] = [Class::D1State, Class::D2Outcome];

/// The classes A10 §4.1 requires that this lane deliberately does not
/// produce. Every verdict names them rather than implying equality.
pub const UNIMPLEMENTED_CLASSES: [Class; 2] = [Class::D3Persistence, Class::D4Witness];

impl Class {
    /// The class's stable name, for reports and refusal output.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Class::D1State => "D-1 state",
            Class::D2Outcome => "D-2 outcome",
            Class::D3Persistence => "D-3 persistence",
            Class::D4Witness => "D-4 witness",
        }
    }
}

/// The version axes A10 §4.3 classifies expected differences by.
///
/// Supplied per side by the caller: they are build-identity facts (the
/// ruleset id, the composition manifest's projection version, the component
/// schema table), not things a run observes. See the module docs for why the
/// ruleset digest is not among them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionAxes {
    /// `RulesetId.version` of the build this side runs.
    pub ruleset_version: u32,
    /// The witness projection framing version (A7 WP-6) of the build this
    /// side runs.
    pub projection_version: u32,
    /// The per-component schema versions of the build this side runs.
    pub schema_versions: BTreeMap<ComponentTypeId, SchemaVersion>,
}

/// Which side of a differential an artifact set came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    /// The baseline-pinned implementation.
    Legacy,
    /// The implementation under comparison.
    Candidate,
}

/// One side's artifacts for one scenario.
///
/// `d1`/`d2` are `None` when the side did not produce that class at all — a
/// future seam-backed subject that cannot observe materialization or routed
/// delivery, say — and the harness refuses rather than comparing the
/// remainder (A10 §4.4's partial-artifact refusal).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SideArtifacts {
    /// Which side produced this set.
    pub side: Side,
    /// The version axes the side declares.
    pub axes: VersionAxes,
    /// Digest of the sealed inputs the side ran from.
    pub sealed_digest: [u8; 32],
    /// D-1 artifact: the state-chain material, as bytes.
    pub d1: Option<Vec<u8>>,
    /// D-2 artifact: the outcome-chain material, as bytes.
    pub d2: Option<Vec<u8>>,
}

/// The committed baseline manifest (A10 §4.4): the commit the goldens were
/// generated at, the axes they were generated under, and the golden tables
/// themselves. A comparison whose legacy side is not pinned to one of these
/// is refused, not run.
#[derive(Debug, Clone)]
pub struct Baseline {
    /// The commit the committed tables were generated at.
    pub commit: &'static str,
    /// The axes the committed tables were generated under.
    pub axes: VersionAxes,
    /// The committed D-1 golden chains, by scenario name.
    pub chains: Vec<(&'static str, [u8; 32])>,
    /// The committed D-2 golden chains, by scenario name.
    pub outcome_chains: Vec<(&'static str, [u8; 32])>,
}

impl Baseline {
    fn chain(&self, scenario: &str) -> Option<[u8; 32]> {
        self.chains
            .iter()
            .find(|(name, _)| *name == scenario)
            .map(|(_, chain)| *chain)
    }

    fn outcome_chain(&self, scenario: &str) -> Option<[u8; 32]> {
        self.outcome_chains
            .iter()
            .find(|(name, _)| *name == scenario)
            .map(|(_, chain)| *chain)
    }

    /// Whether the baseline covers the scenario at all. A baseline whose
    /// tables do not name the scenario is a missing baseline for this
    /// comparison (A10 §4.4: no baseline, no run), not an empty one.
    fn covers(&self, scenario: &str) -> bool {
        self.chain(scenario).is_some() && self.outcome_chain(scenario).is_some()
    }
}

/// One side of a differential: an implementation, and the version axes it
/// declares. Both sides of the self-differential are built from the same
/// constants; when the ECS candidate exists, only this struct changes.
#[derive(Debug, Clone)]
pub struct Subject<G: Game> {
    /// Stable label for reports and refusal output.
    pub label: &'static str,
    /// The implementation. Consumed by the run: a fresh instance per side.
    pub game: G,
    /// The version axes the implementation declares.
    pub axes: VersionAxes,
}

/// Why the harness refused to produce a verdict (A10 §4.4).
///
/// A refusal is not a pass and not a failure: nothing was compared, so no
/// parity claim of any kind is available.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// No committed baseline manifest pins the legacy side — either none was
    /// supplied, or the supplied one does not cover the scenario.
    MissingBaseline,
    /// A side produced fewer than the implemented classes' artifacts. All
    /// implemented classes or no parity claim.
    PartialArtifacts {
        /// The side that came up short.
        side: Side,
        /// The classes it did not produce.
        missing: Vec<Class>,
    },
    /// An axis differs in a way no §4.3 rule covers: a version decrease, a
    /// changed schema membership, or more than one bumped axis at once.
    /// Sorting differences into "expected" here would be guesswork.
    UnclassifiableSkew(Skew),
    /// The classification reached a rule that needs D-3/D-4 evidence, which
    /// this lane does not produce. Named per class, never silently treated
    /// as equality.
    ClassNotImplemented {
        /// The classes the classification reached but this lane does not
        /// produce.
        classes: Vec<Class>,
    },
    /// The two sides did not run from identical sealed inputs, so no
    /// comparison between them would mean anything.
    SealedInputsDiverged {
        /// The legacy side's sealed-input digest.
        legacy: [u8; 32],
        /// The candidate side's sealed-input digest.
        candidate: [u8; 32],
    },
}

/// An axis skew no §4.3 rule classifies.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Skew {
    /// The ruleset version moved backwards.
    Ruleset {
        /// The baseline side's ruleset version.
        legacy: u32,
        /// The candidate side's ruleset version.
        candidate: u32,
    },
    /// The projection framing version moved backwards.
    Projection {
        /// The baseline side's projection version.
        legacy: u32,
        /// The candidate side's projection version.
        candidate: u32,
    },
    /// A component's schema version moved backwards.
    Schema {
        /// The component whose version moved backwards.
        component: ComponentTypeId,
        /// The baseline side's schema version for it.
        legacy: SchemaVersion,
        /// The candidate side's schema version for it.
        candidate: SchemaVersion,
    },
    /// The two sides declare different component-schema memberships.
    SchemaMembership,
    /// More than one axis bumped at once: §4.3's rules each assume a single
    /// bumped axis, and combining them would be a judgement call.
    Multiple,
}

/// One class's observed difference.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Difference {
    /// The chain materials are different bytes, first at `first_divergence`.
    ChainBytes {
        /// Byte offset of the first differing byte.
        first_divergence: usize,
        /// Legacy material length, in bytes.
        legacy_len: usize,
        /// Candidate material length, in bytes.
        candidate_len: usize,
    },
    /// The legacy side's chain digest does not match its committed baseline.
    BaselineDigest {
        /// The committed digest.
        expected: [u8; 32],
        /// The digest the legacy side produced.
        actual: [u8; 32],
    },
}

/// What a differential run concluded — or that it refused to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Equal axes; every implemented class compared byte-identical. **Not a
    /// full F-4 parity claim**: `not_compared` names the classes this lane
    /// does not produce, and a parity claim over all four is S7.2/S7.3's to
    /// close.
    Parity {
        /// The classes that were compared byte-for-byte.
        compared: Vec<Class>,
        /// The classes A10 §4.1 requires that were not produced.
        not_compared: Vec<Class>,
    },
    /// Equal axes (or an axis whose bump does not excuse the class), and a
    /// difference. Any difference in any class is a failure (A10 §4.3).
    Failure {
        /// The classes that differed, and where.
        differences: Vec<(Class, Difference)>,
    },
    /// A ruleset-version bump: D-1/D-2 differences are the migration
    /// fixtures to commit as the new goldens with the bump. A
    /// classification, never a pass — `unmet` names the rule's other half
    /// (D-3/D-4 must still match for unchanged schemas), which this lane
    /// cannot discharge.
    MigrationFixture {
        /// Whether the D-1 state chain differed.
        d1_differs: bool,
        /// Whether the D-2 outcome chain differed.
        d2_differs: bool,
        /// The classes the rule requires that this lane does not produce.
        unmet: Vec<Class>,
    },
    /// The harness refuses to produce a verdict (A10 §4.4).
    Refused(Refusal),
}

/// D-1's comparator: byte equality of the state-chain material.
///
/// The whole material, in order — not its length, and not a digest of it.
fn d1_chain_bytes_equal(legacy: &[u8], candidate: &[u8]) -> bool {
    legacy == candidate
}

/// D-2's comparator: byte equality of the outcome-chain material.
///
/// The whole material, in order — not its length, and not a digest of it:
/// two runs whose outcome records differ inside equal-length payloads are
/// different runs, and this is the comparison that must see that.
fn d2_chain_bytes_equal(legacy: &[u8], candidate: &[u8]) -> bool {
    legacy == candidate
}

/// The first place two chain materials disagree, for failure output.
fn chain_difference(legacy: &[u8], candidate: &[u8]) -> Difference {
    let first_divergence = legacy
        .iter()
        .zip(candidate.iter())
        .position(|(left, right)| left != right)
        .unwrap_or_else(|| legacy.len().min(candidate.len()));
    Difference::ChainBytes {
        first_divergence,
        legacy_len: legacy.len(),
        candidate_len: candidate.len(),
    }
}

/// Compare one side's artifacts against the other's and classify the result
/// by the version axes (A10 §4.3, §4.4).
///
/// This is the verdict stage the runner uses; it is public so the refusal
/// and comparator behaviours are exercisable at exactly the seam they guard.
#[must_use]
pub fn compare(legacy: SideArtifacts, candidate: SideArtifacts) -> Verdict {
    // Completeness before anything: all implemented classes or no verdict.
    let legacy_missing: Vec<Class> = IMPLEMENTED_CLASSES
        .iter()
        .copied()
        .filter(|class| match class {
            Class::D1State => legacy.d1.is_none(),
            Class::D2Outcome => legacy.d2.is_none(),
            Class::D3Persistence | Class::D4Witness => false,
        })
        .collect();
    if !legacy_missing.is_empty() {
        return Verdict::Refused(Refusal::PartialArtifacts {
            side: Side::Legacy,
            missing: legacy_missing,
        });
    }
    let candidate_missing: Vec<Class> = IMPLEMENTED_CLASSES
        .iter()
        .copied()
        .filter(|class| match class {
            Class::D1State => candidate.d1.is_none(),
            Class::D2Outcome => candidate.d2.is_none(),
            Class::D3Persistence | Class::D4Witness => false,
        })
        .collect();
    if !candidate_missing.is_empty() {
        return Verdict::Refused(Refusal::PartialArtifacts {
            side: Side::Candidate,
            missing: candidate_missing,
        });
    }

    // Sealed-input equality: the runner drove both sides from one sealed log,
    // and proves it here rather than assuming it.
    if legacy.sealed_digest != candidate.sealed_digest {
        return Verdict::Refused(Refusal::SealedInputsDiverged {
            legacy: legacy.sealed_digest,
            candidate: candidate.sealed_digest,
        });
    }

    let d1_equal = d1_chain_bytes_equal(
        legacy.d1.as_deref().unwrap_or_default(),
        candidate.d1.as_deref().unwrap_or_default(),
    );
    let d2_equal = d2_chain_bytes_equal(
        legacy.d2.as_deref().unwrap_or_default(),
        candidate.d2.as_deref().unwrap_or_default(),
    );

    let differences = || -> Vec<(Class, Difference)> {
        let mut found = Vec::new();
        if !d1_equal {
            found.push((
                Class::D1State,
                chain_difference(
                    legacy.d1.as_deref().unwrap_or_default(),
                    candidate.d1.as_deref().unwrap_or_default(),
                ),
            ));
        }
        if !d2_equal {
            found.push((
                Class::D2Outcome,
                chain_difference(
                    legacy.d2.as_deref().unwrap_or_default(),
                    candidate.d2.as_deref().unwrap_or_default(),
                ),
            ));
        }
        found
    };

    // A10 §4.3, keyed by the version axes and by nothing else.
    let ruleset = axis_delta(legacy.axes.ruleset_version, candidate.axes.ruleset_version);
    let projection = axis_delta(
        legacy.axes.projection_version,
        candidate.axes.projection_version,
    );
    let schema = schema_delta(
        &legacy.axes.schema_versions,
        &candidate.axes.schema_versions,
    );

    // Skew without a bump: a decrease anywhere, a changed schema membership,
    // or more than one bumped axis. Each would need a judgement call to
    // classify, and §4.3 forbids exactly that.
    let mut bumped: Vec<Skew> = Vec::new();
    match ruleset {
        AxisDelta::Equal => {}
        AxisDelta::Bumped => bumped.push(Skew::Ruleset {
            legacy: legacy.axes.ruleset_version,
            candidate: candidate.axes.ruleset_version,
        }),
        AxisDelta::Regressed => {
            return Verdict::Refused(Refusal::UnclassifiableSkew(Skew::Ruleset {
                legacy: legacy.axes.ruleset_version,
                candidate: candidate.axes.ruleset_version,
            }));
        }
    }
    match projection {
        AxisDelta::Equal => {}
        AxisDelta::Bumped => bumped.push(Skew::Projection {
            legacy: legacy.axes.projection_version,
            candidate: candidate.axes.projection_version,
        }),
        AxisDelta::Regressed => {
            return Verdict::Refused(Refusal::UnclassifiableSkew(Skew::Projection {
                legacy: legacy.axes.projection_version,
                candidate: candidate.axes.projection_version,
            }));
        }
    }
    match &schema {
        SchemaDelta::Equal => {}
        SchemaDelta::Bumped(components) => {
            if let Some(component) = components.first() {
                bumped.push(Skew::Schema {
                    component: *component,
                    legacy: legacy
                        .axes
                        .schema_versions
                        .get(component)
                        .copied()
                        .unwrap_or_default(),
                    candidate: candidate
                        .axes
                        .schema_versions
                        .get(component)
                        .copied()
                        .unwrap_or_default(),
                });
            }
        }
        SchemaDelta::Incoherent(skew) => {
            return Verdict::Refused(Refusal::UnclassifiableSkew(skew.clone()))
        }
    }
    if bumped.len() > 1 {
        return Verdict::Refused(Refusal::UnclassifiableSkew(Skew::Multiple));
    }

    if !bumped.is_empty() {
        // Exactly one axis is bumped; route by which.
        return match &bumped[0] {
            Skew::Ruleset { .. } => Verdict::MigrationFixture {
                d1_differs: !d1_equal,
                d2_differs: !d2_equal,
                unmet: UNIMPLEMENTED_CLASSES.to_vec(),
            },
            Skew::Projection { .. } => {
                // D-1/D-2 are unchanged under a projection bump: differences
                // are still failures. Equality leaves only D-4 to compare,
                // which refuses cross-version claims (A10 §4.3, A7 WP-6).
                if !d1_equal || !d2_equal {
                    Verdict::Failure {
                        differences: differences(),
                    }
                } else {
                    Verdict::Refused(Refusal::ClassNotImplemented {
                        classes: vec![Class::D4Witness],
                    })
                }
            }
            Skew::Schema { .. } => {
                // D-1/D-2/D-4 are unchanged under a schema bump: D-1/D-2
                // differences are still failures. Equality leaves D-3 (the
                // F-6 round-trip) and D-4 to compare, which this lane does
                // not produce.
                if !d1_equal || !d2_equal {
                    Verdict::Failure {
                        differences: differences(),
                    }
                } else {
                    Verdict::Refused(Refusal::ClassNotImplemented {
                        classes: vec![Class::D3Persistence, Class::D4Witness],
                    })
                }
            }
            // `bumped` is built only from these three arms.
            Skew::SchemaMembership | Skew::Multiple => unreachable!("not a bump"),
        };
    }

    // All axes equal: any difference in any class is a failure.
    if !d1_equal || !d2_equal {
        return Verdict::Failure {
            differences: differences(),
        };
    }
    Verdict::Parity {
        compared: IMPLEMENTED_CLASSES.to_vec(),
        not_compared: UNIMPLEMENTED_CLASSES.to_vec(),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AxisDelta {
    Equal,
    Bumped,
    Regressed,
}

fn axis_delta(legacy: u32, candidate: u32) -> AxisDelta {
    if candidate == legacy {
        AxisDelta::Equal
    } else if candidate > legacy {
        AxisDelta::Bumped
    } else {
        AxisDelta::Regressed
    }
}

enum SchemaDelta {
    Equal,
    Bumped(Vec<ComponentTypeId>),
    Incoherent(Skew),
}

fn schema_delta(
    legacy: &BTreeMap<ComponentTypeId, SchemaVersion>,
    candidate: &BTreeMap<ComponentTypeId, SchemaVersion>,
) -> SchemaDelta {
    // Membership is part of the axis: a component appearing or disappearing
    // is not a bump, it is a different build shape.
    if legacy.len() != candidate.len()
        || !legacy
            .keys()
            .all(|component| candidate.contains_key(component))
    {
        return SchemaDelta::Incoherent(Skew::SchemaMembership);
    }
    let mut bumped = Vec::new();
    for (component, legacy_version) in legacy {
        let candidate_version = candidate[component];
        match candidate_version.cmp(legacy_version) {
            core::cmp::Ordering::Equal => {}
            core::cmp::Ordering::Greater => bumped.push(*component),
            core::cmp::Ordering::Less => {
                return SchemaDelta::Incoherent(Skew::Schema {
                    component: *component,
                    legacy: *legacy_version,
                    candidate: candidate_version,
                });
            }
        }
    }
    if bumped.is_empty() {
        SchemaDelta::Equal
    } else {
        SchemaDelta::Bumped(bumped)
    }
}

/// Digest the sealed inputs: seed, tick window, initial population, and
/// every sealed input in log order. The equality both sides of a
/// differential are held to.
#[must_use]
pub fn sealed_digest<G: Game>(sealed: &SealedScenario<G>) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(&sealed.seed.0);
    hasher.update(&sealed.tick_window.first.0.to_le_bytes());
    hasher.update(&sealed.tick_window.end_exclusive.0.to_le_bytes());
    hasher.update(&sealed.initial_entities.to_le_bytes());
    hasher.update(
        &u64::try_from(sealed.input_log.len())
            .expect("input log length fits u64")
            .to_le_bytes(),
    );
    for tick in &sealed.input_log {
        hasher.update(&tick.tick.0.to_le_bytes());
        hasher.update(
            &u64::try_from(tick.entries.len())
                .expect("entry count fits u64")
                .to_le_bytes(),
        );
        for entry in &tick.entries {
            hasher.update(&entry.entity.0.to_le_bytes());
            hasher.update(
                &u64::try_from(entry.inputs.len())
                    .expect("input count fits u64")
                    .to_le_bytes(),
            );
            for input in &entry.inputs {
                let canonical = input.to_canonical();
                hasher.update(
                    &u64::try_from(canonical.len())
                        .expect("canonical input length fits u64")
                        .to_le_bytes(),
                );
                hasher.update(&canonical);
            }
        }
    }
    *hasher.finalize().as_bytes()
}

/// Collect one side's artifacts from its run.
///
/// D-1 is the state-chain material serialized from the run's log; D-2 is the
/// outcome-chain material serialized from the run's per-tick outcome records.
/// A run whose outcome records do not cover its own tick window yields no D-2
/// artifact, and the harness refuses on the incompleteness rather than
/// comparing the one class it has.
#[must_use]
pub fn collect_artifacts<G: Game>(
    played: &Play<G>,
    side: Side,
    axes: VersionAxes,
) -> SideArtifacts {
    let d2 = if played.outcome_entries.len() == played.log.len() {
        Some(outcome_chain_material(
            played.sealed.tick_window.first,
            &played.outcome_entries,
        ))
    } else {
        None
    };
    SideArtifacts {
        side,
        axes,
        sealed_digest: sealed_digest(&played.sealed),
        d1: Some(state_chain_material(&played.log)),
        d2,
    }
}

/// The D-1 artifact: every per-tick state-hash link, in execution order —
/// tick, then `PersistId` ascending within the tick.
fn state_chain_material<G: Game>(log: &[TickRecord<G>]) -> Vec<u8> {
    let mut bytes = Vec::new();
    for record in log {
        bytes.extend_from_slice(&record.tick.0.to_le_bytes());
        bytes.extend_from_slice(
            &u64::try_from(record.entries.len())
                .expect("entry count fits u64")
                .to_le_bytes(),
        );
        for entry in &record.entries {
            bytes.extend_from_slice(&entry.entity.0.to_le_bytes());
            bytes.extend_from_slice(&entry.hash);
        }
    }
    bytes
}

/// The D-2 artifact: every tick's outcome material — canonical event bytes in
/// emission order, materialized identifiers in install order, routed delivery
/// pairs in delivery order — in WP-2's ascending-`PersistId` order across
/// emitters.
///
/// This is the same element sequence [`crate::scenario::fold_outcome_tick`]
/// hashes, with a tick header in front: byte equality of these materials is
/// byte equality of the whole fold history, which is what the D-2 comparator
/// owes A10 §4.1.
fn outcome_chain_material<I: CoreCodec>(
    first_tick: Tick,
    ticks: &[Vec<OutcomeEntry<I>>],
) -> Vec<u8> {
    let mut bytes = Vec::new();
    for (offset, entries) in ticks.iter().enumerate() {
        let tick = Tick::new(first_tick.0 + u64::try_from(offset).expect("tick offset fits u64"));
        bytes.extend_from_slice(&tick.0.to_le_bytes());
        bytes.extend_from_slice(
            &u64::try_from(entries.len())
                .expect("entry count fits u64")
                .to_le_bytes(),
        );
        let mut ordered: Vec<&OutcomeEntry<I>> = entries.iter().collect();
        ordered.sort_by_key(|entry| entry.entity);
        for entry in ordered {
            bytes.extend_from_slice(&entry.entity.0.to_le_bytes());
            bytes.extend_from_slice(
                &u64::try_from(entry.events.len())
                    .expect("event count fits u64")
                    .to_le_bytes(),
            );
            for event in &entry.events {
                bytes.extend_from_slice(
                    &u64::try_from(event.len())
                        .expect("event length fits u64")
                        .to_le_bytes(),
                );
                bytes.extend_from_slice(event);
            }
            bytes.extend_from_slice(
                &u64::try_from(entry.materialized.len())
                    .expect("materialized count fits u64")
                    .to_le_bytes(),
            );
            for entity in &entry.materialized {
                bytes.extend_from_slice(&entity.0.to_le_bytes());
            }
            bytes.extend_from_slice(
                &u64::try_from(entry.delivered.len())
                    .expect("delivery count fits u64")
                    .to_le_bytes(),
            );
            for delivery in &entry.delivered {
                bytes.extend_from_slice(&delivery.target.0.to_le_bytes());
                let canonical = delivery.input.to_canonical();
                bytes.extend_from_slice(
                    &u64::try_from(canonical.len())
                        .expect("input length fits u64")
                        .to_le_bytes(),
                );
                bytes.extend_from_slice(&canonical);
            }
        }
    }
    bytes
}

/// Run the differential: the legacy side plays the scenario and seals its
/// inputs; the candidate side replays from exactly those sealed inputs; the
/// harness pins the legacy side to its committed baseline, proves the
/// sealed-input equality, and classifies the comparison by the version axes.
///
/// This is a **self-differential** when both subjects are the same
/// implementation — which is the lane's deliberate starting point: it proves
/// the runner is deterministic and the comparators actually compare, before
/// there is any ECS to compare against.
#[must_use]
pub fn run_differential<G: Game>(
    legacy: Subject<G>,
    candidate: Subject<G>,
    scenario: &Scenario,
    baseline: Option<&Baseline>,
) -> Verdict {
    // A10 §4.4: no baseline, no run. A comparison against "whatever the
    // legacy side happens to be" is not a comparison.
    let Some(baseline) = baseline else {
        return Verdict::Refused(Refusal::MissingBaseline);
    };
    // The baseline covers this scenario, or it is not a baseline for it.
    if !baseline.covers(scenario.name) {
        return Verdict::Refused(Refusal::MissingBaseline);
    }
    // The legacy side *is* the baseline build: its axes must be the
    // committed ones. (A candidate-side bump is how a migration comparison
    // is declared; a legacy-side drift is a comparison against the wrong
    // baseline.)
    if let Some(skew) = axis_skew(&baseline.axes, &legacy.axes) {
        return Verdict::Refused(Refusal::UnclassifiableSkew(skew));
    }

    // Drive the legacy side; it seals the inputs both sides run from.
    let legacy_played = crate::scenario::play(legacy.game, scenario);
    let legacy_artifacts = collect_artifacts(&legacy_played, Side::Legacy, legacy.axes);

    // Pin: the legacy side must reproduce its committed goldens, class by
    // class. A legacy side that does not is drift, and the comparison would
    // be against whatever this build now is.
    let mut drift = Vec::new();
    match baseline.chain(scenario.name) {
        Some(expected) if expected != legacy_played.chain => {
            drift.push((
                Class::D1State,
                Difference::BaselineDigest {
                    expected,
                    actual: legacy_played.chain,
                },
            ));
        }
        _ => {}
    }
    match baseline.outcome_chain(scenario.name) {
        Some(expected) if expected != legacy_played.outcome_chain => {
            drift.push((
                Class::D2Outcome,
                Difference::BaselineDigest {
                    expected,
                    actual: legacy_played.outcome_chain,
                },
            ));
        }
        _ => {}
    }
    if !drift.is_empty() {
        return Verdict::Failure { differences: drift };
    }

    // Drive the candidate from the legacy side's sealed inputs — and from
    // nothing else. This is the sealed-input equality: the candidate cannot
    // silently replace the baseline's pilot or delivery history while
    // claiming a differential comparison.
    let candidate_played = crate::scenario::replay(candidate.game, &legacy_played.sealed);
    let candidate_artifacts = collect_artifacts(&candidate_played, Side::Candidate, candidate.axes);

    compare(legacy_artifacts, candidate_artifacts)
}

/// The first mechanical skew between two axis sets, if any. Used for the
/// legacy-side baseline pin: the legacy axes must equal the committed ones,
/// because the legacy side *is* the baseline build.
fn axis_skew(baseline: &VersionAxes, side: &VersionAxes) -> Option<Skew> {
    if baseline.ruleset_version != side.ruleset_version {
        return Some(Skew::Ruleset {
            legacy: baseline.ruleset_version,
            candidate: side.ruleset_version,
        });
    }
    if baseline.projection_version != side.projection_version {
        return Some(Skew::Projection {
            legacy: baseline.projection_version,
            candidate: side.projection_version,
        });
    }
    schema_delta(baseline.axes_schema(), side.axes_schema()).incoherent()
}

impl VersionAxes {
    fn axes_schema(&self) -> &BTreeMap<ComponentTypeId, SchemaVersion> {
        &self.schema_versions
    }
}

impl SchemaDelta {
    fn incoherent(self) -> Option<Skew> {
        match self {
            SchemaDelta::Equal => None,
            SchemaDelta::Bumped(_) => None,
            SchemaDelta::Incoherent(skew) => Some(skew),
        }
    }
}
