//! The F-4 differential parity harness, all four classes (A10 §4).
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
//!   ([`Class::D2Outcome`]);
//! - **D-3 persistence**: the encoded at-rest bytes the run produces — framed
//!   `(ComponentTypeId, SchemaVersion, payload)` slots per WP-3, plus the
//!   journal records a `feed_uplink`-shaped producer would queue — by byte
//!   equality **per slot** *and* **slot-set equality** ([`Class::D3Persistence`]);
//! - **D-4 witness**: per-entity per-tick claim values,
//!   `blake3(CoreCodec(quantize(state)))` (WP-1, WP-4), *and* the verdicts the
//!   **existing adjudicator** returns when a witness replays each side's log
//!   against the other side's claims ([`Class::D4Witness`]).
//!
//! Every comparator compares **bytes, not lengths and not digests**: two runs
//! whose outcome records differ inside equal-length payloads are different
//! runs, and a comparator that could not see that is the A10 §4.2 blindness
//! this harness exists to end.
//!
//! # Why four classes, and why none of them collapses into another
//!
//! Each class covers failures the others structurally cannot see, and each
//! blindness is demonstrated on this tree rather than asserted:
//!
//! - D-1 alone misses event-only outcomes — the A7 X-A class, and D-2's job.
//! - D-1 + D-2 miss an **encoding** change that leaves semantics alone: a
//!   candidate serializing a field in a different order produces identical
//!   chains and incompatible stored bytes. That is D-3's job, and exactly
//!   what WP-3's "witness framing ≡ persistence framing" rule exists to keep
//!   aligned. D-3's *slot-set* half is a second blindness inside the first: a
//!   candidate that writes a correct **subset** of slots agrees byte-for-byte
//!   on every slot it wrote.
//! - D-3 misses a projection that hashes the wrong bytes while persisting the
//!   right ones — the A7 X-C class, and D-4's job. #738 demonstrated it live
//!   on this tree: with canonical `quantize()` broken, **both** of Regolith's
//!   committed golden chains stayed green.
//!
//! # D-4's second half is the strongest leg, and it costs nothing new
//!
//! The witness pipeline already re-executes signed logs, so the harness
//! authors each side's run through the *shipped* producer
//! ([`InputLogProducer`] into [`AuthorityLog`]) and hands the crossed
//! evidence to the *shipped* adjudicator (`orrery_core::verify_bundle`).
//! A diverging candidate is therefore **convicted** by the instrument that
//! polices production, not merely diffed by a test — the parity argument is
//! made in the same terms a real dispute would be.
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
//! - bumped `RulesetId.version` → the differences are the migration fixtures
//!   to commit as the new goldens ([`Verdict::MigrationFixture`], which
//!   reports each class separately) — a classification, never a pass;
//! - bumped `SchemaVersion` on a component → D-3 differs by construction (the
//!   framed slot carries the schema version) and routes through the F-6
//!   migration round-trip, a fixture this harness does not build, so the arm
//!   is named ([`Refusal::ClassNotImplemented`]) rather than treated as
//!   equal; D-1/D-2/D-4 differences remain failures;
//! - bumped `projection_version` → D-4's claim values differ by construction,
//!   so the cross-version claim comparison is **refused**
//!   ([`Refusal::CrossVersionClaims`]) rather than reported as deviation —
//!   IV-2's false-deviation hazard, per A7 WP-6; D-1/D-2/D-3 differences
//!   remain failures.
//!
//! A decrease on any axis, a changed schema membership, or more than one
//! bumped axis at once has no rule and is refused as
//! [`Refusal::UnclassifiableSkew`].

use std::collections::{BTreeMap, BTreeSet};

use orrery_core::store::AuthorityLog;
use orrery_core::{
    state_hash, verify_bundle, ComponentTypeId, CoreClass, CoreCodec, Executor, InputLogProducer,
    Quantized, TickBackend,
};
use orrery_protocol::atrest::SchemaVersion;
use orrery_protocol::{
    EvidenceBundle, NodeId, PersistId, RecordKind, Tick, UniverseSeed,
    Verdict as AdjudicatedVerdict, MAX_ADJUDICATION_TICKS,
};
use serde::Serialize;

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
    /// The encoded at-rest bytes the run produces: framed
    /// `(ComponentTypeId, SchemaVersion, payload)` slots per WP-3, plus the
    /// journal records a `feed_uplink`-shaped producer would queue. Compared
    /// by byte equality **per slot** and by **slot-set equality**.
    D3Persistence,
    /// Per-entity per-tick claim values (`blake3(CoreCodec(quantize(state)))`,
    /// WP-1/WP-4), plus the verdicts the *existing* adjudicator returns when a
    /// witness replays each side's log against the other side's claims.
    D4Witness,
}

/// The classes the harness implements and compares.
///
/// All four, since S7.2/S7.3. That is what makes `not_compared` empty on a
/// full run and a real F-4 parity claim possible at all.
pub const IMPLEMENTED_CLASSES: [Class; 4] = [
    Class::D1State,
    Class::D2Outcome,
    Class::D3Persistence,
    Class::D4Witness,
];

/// The classes A10 §4.1 requires that the harness does not produce.
///
/// Empty since S7.3. Kept as a named constant rather than deleted, because
/// every verdict still reports it: a reader of a `Parity` verdict sees the
/// empty list and knows the claim covers all four classes, instead of having
/// to infer it from the absence of a field.
pub const UNIMPLEMENTED_CLASSES: [Class; 0] = [];

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

/// Where one at-rest component slot sits.
///
/// A named key rather than a bare tuple, and ordered `(tick, entity,
/// component)` so the natural [`BTreeMap`] iteration order *is* WP-2's
/// ascending-`PersistId` order within a tick and WP-3's ascending
/// `ComponentTypeId` order within an entity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct SlotKey {
    /// The tick whose post-step state this slot encodes.
    pub tick: Tick,
    /// The entity the bag belongs to.
    pub entity: PersistId,
    /// The component type the slot holds.
    pub component: ComponentTypeId,
}

/// Which entity-tick a witness claim value commits to (WP-1: the unit of
/// witness commitment is one entity-tick).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct ClaimKey {
    /// The tick claimed.
    pub tick: Tick,
    /// The entity claimed for.
    pub entity: PersistId,
}

/// The D-3 artifact: what a run left at rest.
///
/// Two halves, and A10 §4.1 requires **both** comparisons: byte equality per
/// slot *and* slot-set equality. A candidate that writes a correct subset of
/// slots is not at parity, and a comparator that only walked the slots both
/// sides happen to hold could not see that.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PersistenceArtifact {
    /// The framed `(ComponentTypeId, SchemaVersion, payload)` slots, keyed by
    /// where they sit. The bytes are the at-rest framing itself, not the raw
    /// payload: an encoding change that leaves semantics alone shows up here
    /// and nowhere else (A10 §4.2, and D-3's whole reason to exist).
    pub slots: BTreeMap<SlotKey, Vec<u8>>,
    /// The journal records a `feed_uplink`-shaped producer would have queued,
    /// in queue order, each encoded to its at-rest bytes.
    pub journal: Vec<Vec<u8>>,
}

/// The D-4 artifact: what a run committed to, and the evidence a witness
/// would adjudicate it on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WitnessArtifact {
    /// Per-entity per-tick claim values, `blake3(CoreCodec(quantize(state)))`
    /// (WP-1, WP-4). Computed by [`claim_value`] from the state, never lifted
    /// from a hash the run already recorded — a projection that hashes the
    /// wrong bytes is precisely what this class exists to catch, and reusing
    /// the run's own hash would be assuming the answer.
    pub claims: BTreeMap<ClaimKey, [u8; 32]>,
    /// One self-verifying [`EvidenceBundle`] per initially spawned entity,
    /// produced by the *shipped* authority-side producer
    /// ([`InputLogProducer`] into [`AuthorityLog`]) over this side's run. The
    /// cross-replay leg swaps halves of these between sides.
    pub bundles: BTreeMap<PersistId, EvidenceBundle>,
}

/// One side's artifacts for one scenario.
///
/// A class is `None` when the side did not produce it at all — a future
/// seam-backed subject that cannot observe materialization or routed
/// delivery, or a build whose schema table declares nothing persisted — and
/// the harness refuses rather than comparing the remainder (A10 §4.4's
/// partial-artifact refusal).
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
    /// D-3 artifact: the at-rest slots and the journal records.
    pub d3: Option<PersistenceArtifact>,
    /// D-4 artifact: the claim values and the evidence bundles.
    pub d4: Option<WitnessArtifact>,
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
    /// The classification reached a rule that needs evidence this harness
    /// does not produce. Today that is exactly one rule: A10 §4.3 routes a
    /// **schema bump**'s D-3 differences through the F-6 migration
    /// round-trip — old bytes must still load — and F-6 is a different
    /// fixture. Named per class, never silently treated as equality.
    ClassNotImplemented {
        /// The classes the classification reached but the harness does not
        /// produce evidence for.
        classes: Vec<Class>,
    },
    /// A **projection bump**: D-4's claim values differ by construction, so
    /// A10 §4.3 has the harness compare each side against its own version's
    /// projection and **refuse** the cross-version claim comparison outright.
    /// This is a deliberate refusal, not a gap — comparing across projection
    /// versions is IV-2's false-deviation hazard, and a harness that did it
    /// would manufacture convictions out of a framing change.
    CrossVersionClaims {
        /// The legacy side's projection framing version.
        legacy: u32,
        /// The candidate side's projection framing version.
        candidate: u32,
    },
    /// D-4's second half — the cross-replay through the existing adjudicator
    /// — was not run for this comparison, so the class is half-produced. All
    /// of a class or no parity claim.
    CrossReplayNotRun,
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
    /// The two sides wrote **different sets of at-rest slots**. Half of D-3's
    /// comparator, and the half a payload-only comparison cannot see: a
    /// candidate producing a correct *subset* of slots agrees byte-for-byte
    /// on every slot it did write.
    SlotSet {
        /// Slots the legacy side wrote and the candidate did not.
        missing: Vec<SlotKey>,
        /// Slots the candidate wrote and the legacy side did not.
        extra: Vec<SlotKey>,
    },
    /// One slot both sides wrote holds different framed bytes.
    SlotBytes {
        /// Where the slot sits.
        slot: SlotKey,
        /// Byte offset of the first differing byte inside the framed slot.
        first_divergence: usize,
        /// Legacy framed length, in bytes.
        legacy_len: usize,
        /// Candidate framed length, in bytes.
        candidate_len: usize,
    },
    /// The journal a `feed_uplink`-shaped producer would have queued differs.
    JournalRecords {
        /// Index of the first record that differs, or of the first record one
        /// side does not have.
        first_divergence: usize,
        /// Number of records the legacy side queued.
        legacy_len: usize,
        /// Number of records the candidate side queued.
        candidate_len: usize,
    },
    /// The two sides committed to different **sets** of entity-ticks.
    ClaimSet {
        /// Entity-ticks the legacy side claimed and the candidate did not.
        missing: Vec<ClaimKey>,
        /// Entity-ticks the candidate claimed and the legacy side did not.
        extra: Vec<ClaimKey>,
    },
    /// One entity-tick both sides claimed carries a different claim value.
    ClaimValue {
        /// The entity-tick.
        claim: ClaimKey,
        /// The legacy side's `blake3(CoreCodec(quantize(state)))`.
        legacy: [u8; 32],
        /// The candidate side's.
        candidate: [u8; 32],
    },
    /// The **existing adjudicator** returned something other than
    /// [`AdjudicatedVerdict::Exonerates`] when one side's claims were
    /// replayed against the other side's log. A diverging candidate is
    /// *convicted* here, by the same instrument that polices production.
    CrossReplay {
        /// Which way round the halves were crossed.
        crossing: Crossing,
        /// The entity whose window was adjudicated.
        entity: PersistId,
        /// What `orrery_core::verify_bundle` returned.
        verdict: AdjudicatedVerdict,
    },
}

/// Which halves the D-4 cross-replay swapped.
///
/// Both directions are run. The adjudicating build is always the one that
/// produced the *log* being re-executed: that is what makes each direction a
/// real question — "does this build's own re-execution agree with what the
/// other build signed?" — rather than a second look at the same answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Crossing {
    /// Legacy-produced claims adjudicated against candidate-produced logs,
    /// under the candidate's rules.
    LegacyClaimsCandidateLogs,
    /// Candidate-produced claims adjudicated against legacy-produced logs,
    /// under the legacy rules.
    CandidateClaimsLegacyLogs,
}

impl Crossing {
    /// The crossing's stable name, for reports and failure output.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Crossing::LegacyClaimsCandidateLogs => "legacy claims against candidate logs",
            Crossing::CandidateClaimsLegacyLogs => "candidate claims against legacy logs",
        }
    }
}

/// Every verdict the existing adjudicator returned for one differential.
///
/// A10 §4.1's D-4 comparator is "claim equality; **both replays
/// verdict-clean**". This carries the second half.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CrossReplay {
    /// One verdict per `(crossing, entity)`, in crossing then entity order.
    pub verdicts: Vec<(Crossing, PersistId, AdjudicatedVerdict)>,
}

impl CrossReplay {
    /// The verdicts that are not [`AdjudicatedVerdict::Exonerates`].
    #[must_use]
    pub fn unclean(&self) -> Vec<(Crossing, PersistId, AdjudicatedVerdict)> {
        self.verdicts
            .iter()
            .filter(|(_, _, verdict)| *verdict != AdjudicatedVerdict::Exonerates)
            .copied()
            .collect()
    }
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
        /// Whether the D-3 at-rest bytes or journal differed.
        d3_differs: bool,
        /// Whether the D-4 claims or cross-replay verdicts differed.
        d4_differs: bool,
        /// The classes the rule requires that the harness does not produce.
        /// Empty since S7.3: the rule's other half — D-3/D-4 must still match
        /// for unchanged schemas — is now *checked*, and a bump that moved
        /// them is a [`Verdict::Failure`] rather than a fixture.
        unmet: Vec<Class>,
    },
    /// The harness refuses to produce a verdict (A10 §4.4).
    Refused(Refusal),
}

// ---------------------------------------------------------------------------
// D-3 — persistence
// ---------------------------------------------------------------------------

/// The postcard shape of one at-rest component slot.
///
/// Byte-identical to `orrery_persistd::schema::ComponentBag`'s wire slot —
/// same field order, same widths, same encoder — because A10 §4.1 asks D-3
/// for *the encoded at-rest bytes the run produces*, and a differential that
/// invented its own framing would be measuring its own invention. It is
/// transcribed rather than imported only because `orrery_persistd` is a
/// tokio/FoundationDB service crate and this one is deliberately headless
/// (`Cargo.toml`'s no-tokio, no-OS-services rule). `payload` is a `Vec<u8>`
/// where the bag uses `bytes::Bytes`; postcard encodes both as a
/// length-prefixed byte sequence, so the bytes are the same bytes.
#[derive(Serialize)]
struct WireSlot {
    component: u32,
    schema_version: SchemaVersion,
    payload: Vec<u8>,
}

/// The postcard shape of one journal record.
///
/// The fields a [`DiffUplink`](orrery_protocol::DiffUplink) carries that a
/// headless run can answer for: which entity, at which tick, what kind of
/// record, the framed component and its schema version, the payload, and the
/// per-entity client sequence. Cell, grid, lease and authority sequence are
/// deliberately absent — they are placement and tenure facts owned by the
/// live cluster, not by the rules, and a differential that keyed on them
/// would be comparing deployment topology rather than implementations.
#[derive(Serialize)]
struct WireJournalRecord {
    entity: u64,
    tick: u64,
    kind: RecordKind,
    component: u32,
    schema_version: SchemaVersion,
    payload: Vec<u8>,
    seq: u64,
}

/// One `(entity, component)` change-detection stream, named rather than a
/// bare tuple key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct DiffStream {
    entity: PersistId,
    component: ComponentTypeId,
}

/// How many per-item differences one class reports before truncating.
///
/// A tampered candidate diverges in essentially every slot of a 600-tick
/// island run; carrying five thousand of them into a verdict serves nobody.
/// The verdict is still a failure, and the first few name where to look.
const REPORTED_DIFFERENCES: usize = 8;

/// The components a side actually writes at rest, in `ComponentTypeId`
/// ascending order (WP-3).
///
/// `Cosmetic` components are never persisted (`CoreClass`'s own contract), so
/// they contribute no slot. Everything else does.
fn persisted_components<G: Game>(
    game: &G,
    schemas: &BTreeMap<ComponentTypeId, SchemaVersion>,
) -> Vec<(ComponentTypeId, SchemaVersion)> {
    schemas
        .iter()
        .filter(|(component, _)| {
            !matches!(game.classify_component(**component), CoreClass::Cosmetic)
        })
        .map(|(component, version)| (*component, *version))
        .collect()
}

/// Collect the D-3 artifact: the at-rest slots the run wrote, and the journal
/// a `feed_uplink`-shaped producer would have queued.
///
/// `None` when the side's declared schema table names nothing persisted. That
/// is a side that produced no D-3 artifact at all, and the harness refuses on
/// the incompleteness rather than reading an empty slot set as agreement —
/// two sides that both persist nothing are not thereby at parity.
///
/// The journal is **change-detected**, like the system it is shaped after:
/// `feed_uplink` queues one record per *changed* replicated component per
/// tick, with a per-entity monotone sequence. A component's first appearance
/// is a [`RecordKind::Spawn`]; every later change is a
/// [`RecordKind::ComponentDiff`]; a tick that changed nothing queues nothing.
/// That is why the journal is a second comparison and not a restatement of
/// the slots: it carries *when* a write happened, which the slot table does
/// not.
#[must_use]
pub fn collect_persistence<G: Game>(
    game: &G,
    played: &Play<G>,
    schemas: &BTreeMap<ComponentTypeId, SchemaVersion>,
) -> Option<PersistenceArtifact> {
    let components = persisted_components(game, schemas);
    if components.is_empty() {
        return None;
    }

    let mut slots: BTreeMap<SlotKey, Vec<u8>> = BTreeMap::new();
    let mut journal: Vec<Vec<u8>> = Vec::new();
    let mut queued: BTreeMap<DiffStream, Vec<u8>> = BTreeMap::new();
    let mut sequences: BTreeMap<PersistId, u64> = BTreeMap::new();

    for record in &played.log {
        // `TickRecord::entries` is already ascending `PersistId` (VC-4, WP-2).
        for entry in &record.entries {
            let payload = entry.state.to_canonical();
            for (component, schema_version) in &components {
                let framed = postcard::to_stdvec(&WireSlot {
                    component: component.0,
                    schema_version: *schema_version,
                    payload: payload.clone(),
                })
                .expect("an at-rest component slot serializes");
                slots.insert(
                    SlotKey {
                        tick: record.tick,
                        entity: entry.entity,
                        component: *component,
                    },
                    framed,
                );

                let stream = DiffStream {
                    entity: entry.entity,
                    component: *component,
                };
                let held = queued.get(&stream);
                if held.is_some_and(|held| held == &payload) {
                    continue;
                }
                let kind = if held.is_none() {
                    RecordKind::Spawn
                } else {
                    RecordKind::ComponentDiff
                };
                let sequence = sequences.entry(entry.entity).or_insert(0);
                journal.push(
                    postcard::to_stdvec(&WireJournalRecord {
                        entity: entry.entity.0,
                        tick: record.tick.0,
                        kind,
                        component: component.0,
                        schema_version: *schema_version,
                        payload: payload.clone(),
                        seq: *sequence,
                    })
                    .expect("a journal record serializes"),
                );
                *sequence += 1;
                queued.insert(stream, payload.clone());
            }
        }
    }

    Some(PersistenceArtifact { slots, journal })
}

/// D-3's comparator: **byte equality per slot, and slot-set equality**.
///
/// Both halves, and neither is decorative. Byte equality per slot is what
/// catches an encoding change that leaves semantics alone — a candidate
/// serializing a field in a different order produces identical D-1/D-2 chains
/// and incompatible stored bytes (A10 §4.2). Slot-set equality is what
/// catches a candidate that writes a correct *subset*: every slot it wrote
/// agrees byte-for-byte, so a comparison that walked only the slots both
/// sides hold would report parity over a persistence layer that silently
/// dropped rows.
fn persistence_differences(
    legacy: &PersistenceArtifact,
    candidate: &PersistenceArtifact,
) -> Vec<Difference> {
    let mut found = Vec::new();

    let legacy_keys: BTreeSet<SlotKey> = legacy.slots.keys().copied().collect();
    let candidate_keys: BTreeSet<SlotKey> = candidate.slots.keys().copied().collect();
    if legacy_keys != candidate_keys {
        found.push(Difference::SlotSet {
            missing: legacy_keys
                .difference(&candidate_keys)
                .copied()
                .take(REPORTED_DIFFERENCES)
                .collect(),
            extra: candidate_keys
                .difference(&legacy_keys)
                .copied()
                .take(REPORTED_DIFFERENCES)
                .collect(),
        });
    }

    let mut byte_differences = 0usize;
    for (slot, legacy_bytes) in &legacy.slots {
        let Some(candidate_bytes) = candidate.slots.get(slot) else {
            continue;
        };
        if legacy_bytes == candidate_bytes {
            continue;
        }
        byte_differences += 1;
        if byte_differences > REPORTED_DIFFERENCES {
            continue;
        }
        found.push(Difference::SlotBytes {
            slot: *slot,
            first_divergence: first_divergence(legacy_bytes, candidate_bytes),
            legacy_len: legacy_bytes.len(),
            candidate_len: candidate_bytes.len(),
        });
    }

    if legacy.journal != candidate.journal {
        found.push(Difference::JournalRecords {
            first_divergence: legacy
                .journal
                .iter()
                .zip(candidate.journal.iter())
                .position(|(left, right)| left != right)
                .unwrap_or_else(|| legacy.journal.len().min(candidate.journal.len())),
            legacy_len: legacy.journal.len(),
            candidate_len: candidate.journal.len(),
        });
    }

    found
}

// ---------------------------------------------------------------------------
// D-4 — witness
// ---------------------------------------------------------------------------

/// The witness projection over one state: `blake3(CoreCodec(quantize(state)))`
/// (A7 WP-1 for the unit, WP-4 for the order).
///
/// **The quantization is part of the projection, not an assumption about the
/// state handed in.** #738 is the whole reason: with canonical `quantize()`
/// broken, both of Regolith's committed golden chains stayed green, because
/// every in-tree state already stores lattice integers and the snap is a
/// no-op on them. A comparator that hashed whatever bytes it was given —
/// or that lifted the hash the run had already recorded — would inherit
/// exactly that blindness, which is the A7 X-C class this class exists to
/// end.
#[must_use]
pub fn claim_value<S: CoreCodec + Clone + Quantized>(state: &S) -> [u8; 32] {
    let mut projected = state.clone();
    projected.quantize();
    state_hash(&projected)
}

/// The transport identity both sides of a differential author under.
///
/// One key, not two: the sides are two *builds* of one peer's rules, not two
/// peers. Crossing a legacy claim with a candidate frame has to leave both
/// signatures verifiable, or the adjudicator would return
/// `EvidenceForged` for every crossing and the leg would prove nothing.
/// Fixed bytes, never generated: no OS entropy, no clock.
const WITNESS_AUTHORITY_KEY: [u8; 32] = [0x77; 32];

/// Ticks between signed claims. Divides both `T0` and the adjudication
/// window, so a window opens and closes on a claim.
const CLAIM_EVERY: u64 = 20;

/// Ticks per signed frame. Divides [`CLAIM_EVERY`], which
/// [`InputLogProducer`] requires so claims land on complete frame boundaries.
const FRAME_TICKS: u16 = 10;

fn witness_authority() -> iroh_base::SecretKey {
    iroh_base::SecretKey::from_bytes(&WITNESS_AUTHORITY_KEY)
}

/// Collect the D-4 artifact: the claim values, and the evidence a witness
/// would adjudicate this side on.
///
/// The bundles come from the **shipped** producer pair — `InputLogProducer`
/// signing frames and claims into an `AuthorityLog`, which assembles them —
/// driven over a fresh execution of this side's own sealed inputs. Nothing
/// about the evidence is reconstructed here: A10 §4.1's "costs nothing new"
/// is only true if the harness uses the production path, and a hand-built
/// bundle would be a fixture for the harness rather than evidence about the
/// implementation.
#[must_use]
pub fn collect_witness<G: Game + Clone>(game: &G, played: &Play<G>) -> Option<WitnessArtifact> {
    let mut claims = BTreeMap::new();
    for record in &played.log {
        for entry in &record.entries {
            claims.insert(
                ClaimKey {
                    tick: record.tick,
                    entity: entry.entity,
                },
                claim_value(&entry.state),
            );
        }
    }
    let bundles = authored_bundles(game, &played.sealed)?;
    Some(WitnessArtifact { claims, bundles })
}

/// Re-execute the sealed inputs under `game`, authoring the signed log an
/// authority would have streamed, and assemble one bundle per initially
/// spawned entity.
///
/// The window is the run's first [`MAX_ADJUDICATION_TICKS`] — D16's 3 s
/// adjudication window, which is the largest window the shipped adjudicator
/// will accept, so a longer scenario is adjudicated over the same window a
/// real dispute would open.
///
/// Only the initially installed entities get bundles: an entity materialized
/// mid-window has no claim at the window's start, so no window can be opened
/// on it, and inventing one would be assembling evidence no authority could
/// have produced. *Initially installed* includes the scenario's world
/// population as well as its players — both are in the executor before the
/// first tick, and reconstructing only the player half would build a bundle
/// set over a population the run never had.
fn authored_bundles<G: Game + Clone>(
    game: &G,
    sealed: &SealedScenario<G>,
) -> Option<BTreeMap<PersistId, EvidenceBundle>> {
    let first = sealed.tick_window.first.0;
    let span = sealed
        .tick_window
        .end_exclusive
        .0
        .saturating_sub(first)
        .min(MAX_ADJUDICATION_TICKS);
    if span == 0 {
        return None;
    }
    let window = (Tick::new(first), Tick::new(first + span));

    let players: Vec<PersistId> = (1..=sealed.initial_entities).map(PersistId::new).collect();
    let spawned: Vec<(PersistId, G::CoreState)> = players
        .iter()
        .enumerate()
        .map(|(slot, entity)| {
            (
                *entity,
                game.spawn(*entity, u64::try_from(slot).expect("slot fits u64")),
            )
        })
        .collect();
    // The same population `scenario::replay` installs, reconstructed the same
    // way: a bundle set built over a different population would be evidence
    // about a run that never happened.
    let seeded_world = crate::scenario::world_population(
        game,
        sealed.initial_entities,
        sealed.initial_world_entities,
    );
    let entities: Vec<PersistId> = players
        .iter()
        .copied()
        .chain(seeded_world.iter().map(|(entity, _)| *entity))
        .collect();
    let ruleset = game.id();
    let mut executor = Executor::new(game.clone(), sealed.seed);
    for (entity, state) in spawned {
        executor.insert(entity, state);
    }
    for (entity, state) in seeded_world {
        executor.insert(entity, state);
    }

    let key = witness_authority();
    let mut log = AuthorityLog::default();
    let mut producers: BTreeMap<PersistId, InputLogProducer> = BTreeMap::new();
    for entity in &entities {
        let mut producer = InputLogProducer::new(
            key.clone(),
            *entity,
            ruleset,
            first,
            CLAIM_EVERY,
            FRAME_TICKS,
        );
        // The anchor commits to the state as installed — quantized by
        // `insert` (VC-7) — not to the raw spawn, because that is the state
        // the first tick actually reads.
        let state = executor.state(*entity)?.clone();
        let claim = producer.anchor(first, &state);
        log.record_claim(claim, state.to_canonical());
        producers.insert(*entity, producer);
    }

    for record in &sealed.input_log {
        if record.tick.0 >= window.1 .0 {
            break;
        }
        for entry in &record.entries {
            if producers.contains_key(&entry.entity) {
                // Claim first, from pre-step state; then log exactly the
                // inputs about to be applied; then execute. That order is the
                // producer's contract, and getting it wrong would put a claim
                // on a chain head that never existed.
                let state = executor.state(entry.entity)?.clone();
                let producer = producers
                    .get_mut(&entry.entity)
                    .expect("the producer was just found");
                if let Some(claim) = producer.cut_claim(record.tick.0, &state) {
                    log.record_claim(claim, state.to_canonical());
                }
                producer.log_inputs(record.tick.0, &entry.inputs);
            }
            let outcome = executor.step_entity(entry.entity, record.tick, &entry.inputs)?;
            if let Some(producer) = producers.get_mut(&entry.entity) {
                producer.log_neighbor_frames(record.tick.0, &outcome.neighbor_frames);
                producer.log_tick_hash(outcome.state_hash);
                if let Some(authored) = producer.cut_frame(record.tick.0) {
                    let frame_first = authored.frame.first_tick.0;
                    for (offset, hash) in authored.tick_hashes.iter().enumerate() {
                        log.record_tick_hash(
                            entry.entity,
                            Tick::new(
                                frame_first + u64::try_from(offset).expect("offset fits u64"),
                            ),
                            *hash,
                        );
                    }
                    log.record_frame(authored.frame, authored.transitions);
                }
            }
        }
    }

    // The closing claim. A window must end at a claim tick or it contains
    // nothing the subject can be held to (docs/06 §7), and the tick that
    // closes the window is never executed inside it — so its pre-step claim
    // has to be cut here rather than in the loop above.
    for entity in &entities {
        let state = executor.state(*entity)?.clone();
        let producer = producers.get_mut(entity).expect("one producer per entity");
        if let Some(claim) = producer.cut_claim(window.1 .0, &state) {
            log.record_claim(claim, state.to_canonical());
        }
    }

    let mut bundles = BTreeMap::new();
    for entity in &entities {
        // An entity whose window is not covered claim-to-claim authored no
        // evidence at all, and there is nothing to open a window on. That is
        // not hypothetical: `InputLogProducer::cut_frame` refuses to cut a
        // frame with no pending records, so an entity that received **no
        // inputs across the whole window** — which is every seeded
        // `regolith.world` entity under the shipped honest pilot, measured by
        // `tests/world_scenario.rs` — produces no frames and no tick hashes.
        // Skipping it is the honest answer; fabricating a bundle would be
        // assembling evidence no authority could have produced. Both sides
        // skip the same entities, because both ran the same sealed inputs,
        // and `cross_replay` already crosses only the entities present on
        // both sides.
        //
        // **What that costs, stated rather than buried:** D-4's cross-replay
        // half reaches only the entities that author frames. Its claim-value
        // half reaches every entity-tick in the run. `tests/world_scenario.rs`
        // pins both the reach and the reason, so an entity that *should* have
        // authored evidence and silently stopped fails a named test rather
        // than quietly leaving the class.
        let Some(claimed) = log.claimed_hashes(*entity, window) else {
            continue;
        };
        bundles.insert(*entity, log.assemble_bundle(*entity, window, claimed).ok()?);
    }
    // No bundle at all is not "nothing to compare and therefore equal": it is
    // a side that produced no D-4 evidence, and the harness owes a refusal
    // rather than a class it silently did not check.
    if bundles.is_empty() {
        return None;
    }
    Some(bundles)
}

/// One side's signed claims over the other side's signed log.
///
/// Everything a verdict may rest on comes from `claims_from`; everything
/// re-executed comes from `logs_from`. The advisory hint fields are left
/// empty on purpose: `verify_bundle` documents that no verdict may rest on
/// them, and carrying either side's numbers across would invite a reader to
/// believe otherwise.
fn crossed(claims_from: &EvidenceBundle, logs_from: &EvidenceBundle) -> EvidenceBundle {
    EvidenceBundle {
        ruleset: claims_from.ruleset,
        entity: claims_from.entity,
        window_start: claims_from.window_start,
        window_end: claims_from.window_end,
        t0_claim: claims_from.t0_claim.clone(),
        t0_snapshot: claims_from.t0_snapshot.clone(),
        frames: logs_from.frames.clone(),
        sibling_heads: logs_from.sibling_heads.clone(),
        disputed_claims: claims_from.disputed_claims.clone(),
        claimed_hashes: Vec::new(),
        computed_hashes: Vec::new(),
    }
}

/// D-4's second half: run the **existing adjudicator** with legacy-produced
/// claims against candidate-produced logs, and vice versa.
///
/// A10 §4.1 calls this the strongest leg and the cheapest, and both halves of
/// that are true here: the witness pipeline already re-executes signed logs,
/// so this is `orrery_core::verify_bundle` — the function `persistd` reaches
/// a real verdict with — called twice per entity. A candidate that diverges
/// is therefore *convicted* by the instrument that polices production, not
/// merely diffed by a harness.
///
/// The adjudicating build is the one that produced the log being
/// re-executed. Frames carry inputs, and both sides ran from one sealed input
/// log, so the frames are the same on both sides and it is the *claims* that
/// carry each build's answer: crossing them is what puts one build's signed
/// answer in front of the other build's re-execution.
#[must_use]
pub fn cross_replay<G: Game + Clone>(
    legacy_game: &G,
    candidate_game: &G,
    legacy: &WitnessArtifact,
    candidate: &WitnessArtifact,
    seed: UniverseSeed,
) -> CrossReplay {
    let authority: NodeId = witness_authority().public();
    let mut verdicts = Vec::new();
    for (entity, legacy_bundle) in &legacy.bundles {
        let Some(candidate_bundle) = candidate.bundles.get(entity) else {
            continue;
        };
        verdicts.push((
            Crossing::LegacyClaimsCandidateLogs,
            *entity,
            verify_bundle(
                candidate_game.clone(),
                seed,
                authority,
                &crossed(legacy_bundle, candidate_bundle),
            ),
        ));
    }
    for (entity, candidate_bundle) in &candidate.bundles {
        let Some(legacy_bundle) = legacy.bundles.get(entity) else {
            continue;
        };
        verdicts.push((
            Crossing::CandidateClaimsLegacyLogs,
            *entity,
            verify_bundle(
                legacy_game.clone(),
                seed,
                authority,
                &crossed(candidate_bundle, legacy_bundle),
            ),
        ));
    }
    CrossReplay { verdicts }
}

/// D-4's comparator, first half: claim equality, by value and by set.
fn claim_differences(legacy: &WitnessArtifact, candidate: &WitnessArtifact) -> Vec<Difference> {
    let mut found = Vec::new();
    let legacy_keys: BTreeSet<ClaimKey> = legacy.claims.keys().copied().collect();
    let candidate_keys: BTreeSet<ClaimKey> = candidate.claims.keys().copied().collect();
    if legacy_keys != candidate_keys {
        found.push(Difference::ClaimSet {
            missing: legacy_keys
                .difference(&candidate_keys)
                .copied()
                .take(REPORTED_DIFFERENCES)
                .collect(),
            extra: candidate_keys
                .difference(&legacy_keys)
                .copied()
                .take(REPORTED_DIFFERENCES)
                .collect(),
        });
    }
    let mut value_differences = 0usize;
    for (claim, legacy_value) in &legacy.claims {
        let Some(candidate_value) = candidate.claims.get(claim) else {
            continue;
        };
        if legacy_value == candidate_value {
            continue;
        }
        value_differences += 1;
        if value_differences > REPORTED_DIFFERENCES {
            continue;
        }
        found.push(Difference::ClaimValue {
            claim: *claim,
            legacy: *legacy_value,
            candidate: *candidate_value,
        });
    }
    found
}

/// D-4's comparator, second half: both replays verdict-clean.
///
/// Truncation is **per crossing**, not over the flat list: a populous
/// scenario produces one verdict per entity per direction, and truncating the
/// flat list would let the first direction's volume hide the second
/// direction's convictions entirely — a report that says one crossing failed
/// when both did.
fn cross_replay_differences(cross: &CrossReplay) -> Vec<Difference> {
    let unclean = cross.unclean();
    let mut found = Vec::new();
    for crossing in [
        Crossing::LegacyClaimsCandidateLogs,
        Crossing::CandidateClaimsLegacyLogs,
    ] {
        found.extend(
            unclean
                .iter()
                .filter(|(found_crossing, _, _)| *found_crossing == crossing)
                .take(REPORTED_DIFFERENCES)
                .map(|(crossing, entity, verdict)| Difference::CrossReplay {
                    crossing: *crossing,
                    entity: *entity,
                    verdict: *verdict,
                }),
        );
    }
    found
}

/// The first byte offset at which two materials disagree.
fn first_divergence(legacy: &[u8], candidate: &[u8]) -> usize {
    legacy
        .iter()
        .zip(candidate.iter())
        .position(|(left, right)| left != right)
        .unwrap_or_else(|| legacy.len().min(candidate.len()))
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
    Difference::ChainBytes {
        first_divergence: first_divergence(legacy, candidate),
        legacy_len: legacy.len(),
        candidate_len: candidate.len(),
    }
}

/// The implemented classes a side did not produce.
fn missing_classes(side: &SideArtifacts) -> Vec<Class> {
    IMPLEMENTED_CLASSES
        .iter()
        .copied()
        .filter(|class| match class {
            Class::D1State => side.d1.is_none(),
            Class::D2Outcome => side.d2.is_none(),
            Class::D3Persistence => side.d3.is_none(),
            Class::D4Witness => side.d4.is_none(),
        })
        .collect()
}

/// Compare one side's artifacts against the other's and classify the result
/// by the version axes (A10 §4.3, §4.4).
///
/// `cross` carries D-4's second half — the verdicts the existing adjudicator
/// returned when each side's claims were replayed against the other's log
/// ([`cross_replay`]). It is a separate argument because that leg is a
/// property of the *pair*, not of either side: neither side can produce it
/// alone. `None` means the leg was not run, and half a class is no class
/// (A10 §4.4's partial-artifact rule).
///
/// This is the verdict stage the runner uses; it is public so the refusal
/// and comparator behaviours are exercisable at exactly the seam they guard.
#[must_use]
pub fn compare(
    legacy: SideArtifacts,
    candidate: SideArtifacts,
    cross: Option<&CrossReplay>,
) -> Verdict {
    // Completeness before anything: all implemented classes or no verdict.
    let legacy_missing = missing_classes(&legacy);
    if !legacy_missing.is_empty() {
        return Verdict::Refused(Refusal::PartialArtifacts {
            side: Side::Legacy,
            missing: legacy_missing,
        });
    }
    let candidate_missing = missing_classes(&candidate);
    if !candidate_missing.is_empty() {
        return Verdict::Refused(Refusal::PartialArtifacts {
            side: Side::Candidate,
            missing: candidate_missing,
        });
    }
    // D-4 is two halves and both are required.
    let Some(cross) = cross else {
        return Verdict::Refused(Refusal::CrossReplayNotRun);
    };

    // Sealed-input equality: the runner drove both sides from one sealed log,
    // and proves it here rather than assuming it.
    if legacy.sealed_digest != candidate.sealed_digest {
        return Verdict::Refused(Refusal::SealedInputsDiverged {
            legacy: legacy.sealed_digest,
            candidate: candidate.sealed_digest,
        });
    }

    let legacy_d1 = legacy.d1.as_deref().unwrap_or_default();
    let candidate_d1 = candidate.d1.as_deref().unwrap_or_default();
    let legacy_d2 = legacy.d2.as_deref().unwrap_or_default();
    let candidate_d2 = candidate.d2.as_deref().unwrap_or_default();
    let d1_equal = d1_chain_bytes_equal(legacy_d1, candidate_d1);
    let d2_equal = d2_chain_bytes_equal(legacy_d2, candidate_d2);

    let d3_differences = persistence_differences(
        legacy.d3.as_ref().expect("completeness was checked"),
        candidate.d3.as_ref().expect("completeness was checked"),
    );
    let mut d4_differences = claim_differences(
        legacy.d4.as_ref().expect("completeness was checked"),
        candidate.d4.as_ref().expect("completeness was checked"),
    );
    d4_differences.extend(cross_replay_differences(cross));

    let d3_equal = d3_differences.is_empty();
    let d4_equal = d4_differences.is_empty();

    let differences = |classes: &[Class]| -> Vec<(Class, Difference)> {
        let mut found = Vec::new();
        if classes.contains(&Class::D1State) && !d1_equal {
            found.push((Class::D1State, chain_difference(legacy_d1, candidate_d1)));
        }
        if classes.contains(&Class::D2Outcome) && !d2_equal {
            found.push((Class::D2Outcome, chain_difference(legacy_d2, candidate_d2)));
        }
        if classes.contains(&Class::D3Persistence) {
            found.extend(
                d3_differences
                    .iter()
                    .map(|difference| (Class::D3Persistence, difference.clone())),
            );
        }
        if classes.contains(&Class::D4Witness) {
            found.extend(
                d4_differences
                    .iter()
                    .map(|difference| (Class::D4Witness, difference.clone())),
            );
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
            // A rules bump moves state, and everything downstream of state
            // moves with it: the chains, the bytes those states are stored
            // as, and the claims committing to them. All four are the
            // migration fixtures to commit with the bump. A classification,
            // never a pass — the axis that must *not* have moved is the
            // schema table, and that is checked above rather than asserted
            // here.
            Skew::Ruleset { .. } => Verdict::MigrationFixture {
                d1_differs: !d1_equal,
                d2_differs: !d2_equal,
                d3_differs: !d3_equal,
                d4_differs: !d4_equal,
                unmet: UNIMPLEMENTED_CLASSES.to_vec(),
            },
            Skew::Projection { .. } => {
                // D-1/D-2/D-3 are unchanged under a projection bump:
                // differences there are still failures. D-4's claim values
                // differ by construction, so the cross-version comparison is
                // refused outright rather than reported as deviation (A10
                // §4.3, A7 WP-6, IV-2's false-deviation hazard).
                if !d1_equal || !d2_equal || !d3_equal {
                    Verdict::Failure {
                        differences: differences(&[
                            Class::D1State,
                            Class::D2Outcome,
                            Class::D3Persistence,
                        ]),
                    }
                } else {
                    Verdict::Refused(Refusal::CrossVersionClaims {
                        legacy: legacy.axes.projection_version,
                        candidate: candidate.axes.projection_version,
                    })
                }
            }
            Skew::Schema { .. } => {
                // D-1/D-2/D-4 are unchanged under a schema bump: differences
                // there are still failures. D-3 differs by construction — the
                // framed slot carries the schema version — and routes through
                // the F-6 migration round-trip, which is a different fixture.
                if !d1_equal || !d2_equal || !d4_equal {
                    Verdict::Failure {
                        differences: differences(&[
                            Class::D1State,
                            Class::D2Outcome,
                            Class::D4Witness,
                        ]),
                    }
                } else {
                    Verdict::Refused(Refusal::ClassNotImplemented {
                        classes: vec![Class::D3Persistence],
                    })
                }
            }
            // `bumped` is built only from these three arms.
            Skew::SchemaMembership | Skew::Multiple => unreachable!("not a bump"),
        };
    }

    // All axes equal: any difference in any class is a failure.
    if !d1_equal || !d2_equal || !d3_equal || !d4_equal {
        return Verdict::Failure {
            differences: differences(&IMPLEMENTED_CLASSES),
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
    // Both halves of the population, because "initial population" above is a
    // claim about the whole of it: two sides seeded with different world
    // populations must not be able to present equal sealed digests and be
    // compared as if they had run the same scenario.
    hasher.update(&sealed.initial_world_entities.to_le_bytes());
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
/// outcome-chain material serialized from the run's per-tick outcome records;
/// D-3 is the at-rest slot table and journal the run would have written; D-4
/// is the claim values and the evidence a witness would adjudicate. A run
/// that cannot produce one of them yields `None` for it, and the harness
/// refuses on the incompleteness rather than comparing the classes it has.
///
/// `game` is the side's own build: D-3 asks it which components are persisted
/// and D-4 re-executes under it, so passing the other side's build here would
/// silently compare a run against itself.
#[must_use]
pub fn collect_artifacts<G: Game + Clone>(
    game: &G,
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
    let d3 = collect_persistence(game, played, &axes.schema_versions);
    let d4 = collect_witness(game, played);
    SideArtifacts {
        side,
        axes,
        sealed_digest: sealed_digest(&played.sealed),
        d1: Some(state_chain_material(&played.log)),
        d2,
        d3,
        d4,
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
pub fn run_differential<G: Game + Clone>(
    legacy: Subject<G>,
    candidate: Subject<G>,
    scenario: &Scenario,
    baseline: Option<&Baseline>,
) -> Verdict {
    run_differential_on(
        legacy,
        candidate,
        scenario,
        baseline,
        Backends {
            legacy: Executor::new,
            candidate: Executor::new,
        },
    )
}

/// The substrate each side of a differential runs on.
///
/// A named record rather than two positional closures: at the call site that
/// finally makes S7.4's claim, which backend is the legacy one and which is
/// the candidate is the whole meaning of the run, and a reader must not have
/// to count arguments to find out.
pub struct Backends<L, C> {
    /// Builds the baseline-pinned side's substrate from its build and seed.
    pub legacy: L,
    /// Builds the side-under-comparison's substrate.
    pub candidate: C,
}

/// Run the differential with each side on its own storage-and-scheduling
/// substrate (S7.4, #745).
///
/// This is the generalisation [`run_differential`] is now a special case of:
/// both sides on [`Executor`]. It is what lets the S7.4 claim be made at all,
/// because the ECS backend lives at the seam (`orrery_sim_host`) and this
/// crate is Bevy-free by `core-gates.sh` clause 1 — the harness therefore has
/// to accept a backend it cannot name.
///
/// Everything the comparison rests on is unchanged and unchangeable by the
/// backend: the legacy side still plays the scenario and seals the inputs, the
/// candidate still replays from *those* sealed inputs and nothing else, the
/// legacy side is still pinned to its committed baseline before anything is
/// compared, and the sealed-input digests are still proved equal. A backend
/// that wanted to pass by rewriting the pilot would be caught by the sealed
/// digest; one that wanted to pass by rewriting the goldens would be caught by
/// the pin.
#[must_use]
pub fn run_differential_on<G, L, C, BL, BC>(
    legacy: Subject<G>,
    candidate: Subject<G>,
    scenario: &Scenario,
    baseline: Option<&Baseline>,
    backends: Backends<L, C>,
) -> Verdict
where
    G: Game + Clone,
    BL: TickBackend<G>,
    BC: TickBackend<G>,
    L: FnOnce(G, UniverseSeed) -> BL,
    C: FnOnce(G, UniverseSeed) -> BC,
{
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

    // Drive the legacy side; it seals the inputs both sides run from. The
    // build is kept, not consumed: D-3 asks it what is persisted and D-4's
    // cross-replay adjudicates under it.
    let legacy_game = legacy.game.clone();
    let legacy_played = crate::scenario::play_with(legacy.game, scenario, backends.legacy);
    let legacy_artifacts = collect_artifacts(
        &legacy_game,
        &legacy_played,
        Side::Legacy,
        legacy.axes.clone(),
    );

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
    let candidate_game = candidate.game.clone();
    let candidate_played =
        crate::scenario::replay_with(candidate.game, &legacy_played.sealed, backends.candidate);
    let candidate_artifacts = collect_artifacts(
        &candidate_game,
        &candidate_played,
        Side::Candidate,
        candidate.axes,
    );

    // D-4's second half: the existing adjudicator, run in both directions.
    // Only possible once both sides produced their witness artifacts, which
    // `compare` then refuses on if either did not.
    let cross = match (
        legacy_artifacts.d4.as_ref(),
        candidate_artifacts.d4.as_ref(),
    ) {
        (Some(legacy_witness), Some(candidate_witness)) => Some(cross_replay(
            &legacy_game,
            &candidate_game,
            legacy_witness,
            candidate_witness,
            legacy_played.sealed.seed,
        )),
        _ => None,
    };

    compare(legacy_artifacts, candidate_artifacts, cross.as_ref())
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
