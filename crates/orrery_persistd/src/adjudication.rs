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

    /// D29 clause 7's second entry point: finalize one provisional intent by
    /// spot replay.
    ///
    /// # Why this cannot be `adjudicate` with a synthesised report
    ///
    /// [`Self::adjudicate`] opens by verifying the *reporter's* signature, and
    /// that check has no meaning here: a provisional finalization has no
    /// reporter and no accusation. There is nobody claiming anything — the
    /// cluster is checking its own outstanding work. Wrapping the bundle in a
    /// [`DiscrepancyReport`] would mean the cluster signing an accusation
    /// against a player in order to satisfy a check it wrote for a different
    /// situation, which is a worse fiction than a second entry point.
    ///
    /// What *is* shared is the executor itself, and deliberately:
    /// [`RETAINED_BUILDS`] is the scarce resource, and two registries would
    /// give two answers to "which build adjudicates this window" — the exact
    /// failure the version-keyed routing exists to prevent.
    ///
    /// # The commitment check comes first
    ///
    /// Before any replay, the fetched bundle must reproduce the
    /// [`EvidenceCommitment`] the intent was committed under. That is the only
    /// property clause 6 traded the attached evidence for: the submitter
    /// pinned the history before it knew what the cluster would ask, so a
    /// bundle that does not match is a substituted one. It is answered with
    /// `EvidenceForged` — and note that this verdict strikes the *submitter*
    /// here, where on the report path it protects an accused peer by striking
    /// the reporter. On this path the evidence's author and the intent's
    /// submitter are one account, which is the asymmetry D29 flags for review.
    ///
    /// # What spot replay proves, exactly
    ///
    /// That the submitter's claimed history across `[t₀, t_intent)` is
    /// self-consistent, correctly signed, chain-continuous, and reproduces its
    /// own state claims under the pinned build. It does **not** re-check the
    /// ledger invariant — that check already ran, inside the serializable
    /// transaction, and D11 keeps that transaction the sole authority over
    /// durable truth. The gap this closes is the one attestation would have
    /// closed: whether the intent was grafted onto a history nobody saw.
    ///
    /// Stating it narrowly matters, because a reader who believes finalization
    /// re-audits the economy will not build the conservation auditor that P5
    /// exit still owes.
    #[must_use]
    pub fn finalize_provisional(
        &self,
        subject: NodeId,
        commitment: &orrery_protocol::EvidenceCommitment,
        bundle: &orrery_protocol::EvidenceBundle,
        log_head: orrery_protocol::ChainHash,
    ) -> Verdict {
        if !crate::intent::provisional::commitment_matches(commitment, bundle, log_head) {
            return crate::intent::provisional::mismatch_verdict();
        }
        // Routed by the **commitment's** ruleset, not the bundle's. They are
        // equal — `commitment_matches` just proved it — and taking it from the
        // commitment is what makes that equality load-bearing rather than
        // incidental: the build that judges the window is the build the
        // submitter pinned at commit time, and no later-fetched artifact can
        // move it.
        let Some(registered) = self
            .builds
            .iter()
            .find(|registered| registered.id == commitment.ruleset)
        else {
            // Never a strike. `RETAINED_BUILDS` bounds both workloads, so an
            // intent pinning a build older than the last three is annulled
            // with nobody at fault — a new way for an honest player to lose an
            // item during a rules upgrade, and the same trade the report path
            // already accepted.
            return crate::intent::provisional::unknown_ruleset_verdict();
        };
        (registered.worker)(subject, bundle)
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
