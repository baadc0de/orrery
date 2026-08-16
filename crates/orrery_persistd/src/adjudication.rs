//! The adjudication executor (docs/07 §3 stage 4, D11/D12).
//!
//! A discrepancy report arrives pinned to the `RulesetId` its subject ran. The
//! executor routes it to the matching **version-keyed worker** and re-runs the
//! window there. It does not adjudicate under whatever build happens to be
//! current: rules change, and judging an old window under new rules would
//! manufacture deviations out of a version bump.
//!
//! **Retention is three builds** (D16). Older than that is
//! [`UnadjudicableReason::UnknownRuleset`] — never a strike. That asymmetry is
//! deliberate and it points at the cluster: an operator who ships four rules
//! versions in a week has made old disputes unjudgeable, and the honest answer
//! is to say so rather than to punish whoever reported one.
//!
//! Verdicts are reached by `orrery_core::verify_bundle`, which is a pure
//! function of the evidence. The cluster therefore re-runs what it was sent
//! rather than believing the reporter — the whole point of a self-verifying
//! bundle.

use std::collections::VecDeque;

use orrery_core::verify_bundle;
use orrery_core::Ruleset;
use orrery_protocol::{
    DiscrepancyReport, NodeId, RulesetId, UnadjudicableReason, UniverseSeed, Verdict,
};

/// How many rules builds the cluster keeps adjudicable at once (D16).
pub const RETAINED_BUILDS: usize = 3;

/// One registered rules build, boxed so the executor is not generic over a
/// single `Ruleset`.
///
/// A cluster serves many games' worth of history across a rules upgrade, and
/// making the executor generic would force one binary per build. The closure
/// captures the concrete build and hands back a verdict.
type Worker = Box<dyn Fn(NodeId, &orrery_protocol::EvidenceBundle) -> Verdict + Send + Sync>;

struct Registered {
    id: RulesetId,
    worker: Worker,
}

/// Routes reports to the matching rules build and re-runs the window.
pub struct AdjudicationExecutor {
    seed: UniverseSeed,
    /// Newest last. Bounded at [`RETAINED_BUILDS`]; registering a fourth
    /// retires the oldest, which is what makes `UnknownRuleset` reachable and
    /// therefore worth testing.
    builds: VecDeque<Registered>,
}

impl AdjudicationExecutor {
    /// An executor for one universe, with no builds registered yet.
    #[must_use]
    pub fn new(seed: UniverseSeed) -> Self {
        Self {
            seed,
            builds: VecDeque::new(),
        }
    }

    /// Register a rules build as adjudicable, retiring the oldest past the cap.
    ///
    /// `factory` is called per report rather than once, because a replay needs
    /// its own executor and a `Ruleset` is a cheap pure value — cheaper than
    /// requiring `Clone` from every game.
    pub fn register<R: Ruleset>(&mut self, factory: fn() -> R) {
        let id = factory().id();
        let seed = self.seed;
        let worker: Worker =
            Box::new(move |authority, bundle| verify_bundle(factory(), seed, authority, bundle));
        self.builds.retain(|registered| registered.id != id);
        self.builds.push_back(Registered { id, worker });
        while self.builds.len() > RETAINED_BUILDS {
            self.builds.pop_front();
        }
    }

    /// The builds currently adjudicable, oldest first.
    pub fn retained(&self) -> impl Iterator<Item = RulesetId> + '_ {
        self.builds.iter().map(|registered| registered.id)
    }

    /// Adjudicate one report.
    ///
    /// The reporter's signature is checked first and separately: a report
    /// nobody signed cannot be attributed to an account, and attribution is the
    /// only thing that makes `EvidenceForged` actionable. That check is *not*
    /// a judgement of the evidence — a well-formed accusation and a false one
    /// have to stay distinguishable.
    #[must_use]
    pub fn adjudicate(&self, report: &DiscrepancyReport) -> Verdict {
        if orrery_witness::verify_report(report).is_err() {
            // Unsigned or tampered-in-transit. Not `EvidenceForged`: that
            // verdict strikes the named reporter, and an unverifiable
            // signature is exactly the case where the name means nothing.
            return Verdict::Unadjudicable(UnadjudicableReason::Malformed);
        }
        let Some(registered) = self
            .builds
            .iter()
            .find(|registered| registered.id == report.bundle.ruleset)
        else {
            return Verdict::Unadjudicable(UnadjudicableReason::UnknownRuleset);
        };
        (registered.worker)(report.subject, &report.bundle)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use orrery_core::{CodecError, CoreCodec, OrderedInputs, Quantized, StateView, StepOutput};
    use orrery_protocol::{ChainHash, EvidenceBundle, PersistId, StateClaim, Tick};

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct Empty;

    impl CoreCodec for Empty {
        fn encode(&self, _out: &mut Vec<u8>) {}
        fn decode(_bytes: &[u8]) -> Result<Self, CodecError> {
            Ok(Self)
        }
    }

    impl Quantized for Empty {
        fn quantize(&mut self) {}
    }

    macro_rules! build {
        ($name:ident, $version:expr) => {
            struct $name;
            impl Ruleset for $name {
                type CoreState = Empty;
                type CoreInput = Empty;
                type CoreEvent = Empty;
                fn id(&self) -> RulesetId {
                    RulesetId {
                        version: $version,
                        digest: [$version as u8; 32],
                    }
                }
                fn step(
                    &self,
                    _view: &mut StateView<'_, Empty>,
                    _inputs: &OrderedInputs<'_, Empty>,
                    _rng: &mut orrery_core::TickRng,
                ) -> StepOutput<Empty> {
                    StepOutput::default()
                }
            }
        };
    }

    build!(V1, 1);
    build!(V2, 2);
    build!(V3, 3);
    build!(V4, 4);

    fn key(seed: u8) -> iroh_base::SecretKey {
        iroh_base::SecretKey::from_bytes(&[seed; 32])
    }

    fn bundle(ruleset: RulesetId) -> EvidenceBundle {
        let subject = key(1);
        let mut claim = StateClaim {
            entity: PersistId::new(1),
            chain_epoch: 0,
            tick: Tick::new(0),
            input_head: ChainHash::EMPTY,
            state_hash: *blake3::hash(&[]).as_bytes(),
            prev_claim: [0; 32],
            ruleset,
            sig: subject.sign(b"x"),
        };
        orrery_core::log::sign_claim(&subject, &mut claim);
        EvidenceBundle {
            ruleset,
            entity: PersistId::new(1),
            window_start: Tick::new(0),
            window_end: Tick::new(1),
            t0_claim: claim,
            t0_snapshot: bytes::Bytes::new(),
            frames: Vec::new(),
            sibling_heads: Vec::new(),
            disputed_claims: Vec::new(),
            claimed_hashes: vec![[0; 32]],
            computed_hashes: vec![[0; 32]],
        }
    }

    fn executor() -> AdjudicationExecutor {
        let mut executor = AdjudicationExecutor::new(UniverseSeed([1; 32]));
        executor.register(|| V1);
        executor.register(|| V2);
        executor.register(|| V3);
        executor
    }

    #[test]
    fn only_three_builds_stay_adjudicable() {
        let mut executor = executor();
        executor.register(|| V4);
        let retained: Vec<_> = executor.retained().map(|id| id.version).collect();
        assert_eq!(retained, vec![2, 3, 4], "the oldest build retires");
    }

    #[test]
    fn a_report_for_a_retired_build_is_undecidable_not_a_strike() {
        // Rules-version skew is the cluster's gap. Striking a reporter for it
        // would punish someone for an operator's release cadence.
        let mut executor = executor();
        executor.register(|| V4);
        let report = orrery_witness::sign_report(&key(2), key(1).public(), bundle(V1.id()));
        assert_eq!(
            executor.adjudicate(&report),
            Verdict::Unadjudicable(UnadjudicableReason::UnknownRuleset)
        );
    }

    #[test]
    fn a_report_is_routed_to_the_build_its_subject_ran() {
        // Judging an old window under current rules would manufacture
        // deviations out of a version bump.
        let executor = executor();
        for build in [V1.id(), V2.id(), V3.id()] {
            let report = orrery_witness::sign_report(&key(2), key(1).public(), bundle(build));
            assert!(
                !matches!(
                    executor.adjudicate(&report),
                    Verdict::Unadjudicable(UnadjudicableReason::UnknownRuleset)
                ),
                "build {build:?} should be adjudicable"
            );
        }
    }

    #[test]
    fn an_unsigned_report_is_malformed_rather_than_forged() {
        // `EvidenceForged` strikes the *named* reporter, and an unverifiable
        // signature is precisely the case where that name means nothing.
        let executor = executor();
        let mut report = orrery_witness::sign_report(&key(2), key(1).public(), bundle(V1.id()));
        report.reporter = key(9).public();
        assert_eq!(
            executor.adjudicate(&report),
            Verdict::Unadjudicable(UnadjudicableReason::Malformed)
        );
    }

    #[test]
    fn re_registering_a_build_does_not_consume_a_retention_slot() {
        // A restart re-registers the same builds; if each registration ate a
        // slot, a bounce would silently retire adjudicable history.
        let mut executor = executor();
        executor.register(|| V3);
        executor.register(|| V3);
        let retained: Vec<_> = executor.retained().map(|id| id.version).collect();
        assert_eq!(retained, vec![1, 2, 3]);
    }
}
