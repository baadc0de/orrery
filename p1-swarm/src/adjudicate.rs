//! The cluster's half of P4's demo criterion, in process (docs/07 §3 stage 4).
//!
//! A witness that files is only half a pipeline. The criterion says *detected,
//! escalated, **replay-adjudicated** with a deviation verdict within one
//! adjudication window*, and until a report is re-run by something that
//! believes nothing the reporter said, the last word belongs to the accuser.
//!
//! # Why this is thirty lines and not a dependency
//!
//! `orrery_persistd::AdjudicationExecutor` is the shipping version of exactly
//! this, and it is deliberately not used here. It is a version-keyed routing
//! table over boxed workers — the thing a cluster needs because it serves many
//! games across a rules upgrade — and taking it would pull tokio, tonic, fjall
//! and iroh into this tool's separate lockfile for one dispatch. The part that
//! decides anything is not in it: `orrery_witness::verify_report` checks the
//! reporter's signature and `orrery_core::verify_bundle` re-runs the evidence,
//! and both are pure functions this crate already depends on. What is
//! reproduced here is the *order* those two are applied in, which is the part
//! that carries an argument — see [`Adjudicator::judge`].
//!
//! One build is registered rather than three, because this harness plays one
//! game at one version. A report pinned to any other `RulesetId` resolves as
//! [`UnadjudicableReason::UnknownRuleset`], which is the same answer the
//! cluster gives past its retention window and is never a strike.

use std::collections::BTreeMap;

use orrery_core::verify_bundle;
use orrery_games::skirmish::{Skirmish, SKIRMISH_RULESET};
use orrery_protocol::{DiscrepancyReport, NodeId, UnadjudicableReason, UniverseSeed, Verdict};

/// Re-runs filed reports under the shipping rules.
#[derive(Debug, Clone, Copy)]
pub struct Adjudicator {
    seed: UniverseSeed,
}

/// What adjudicating a run's reports came to.
///
/// Verdict counts rather than the verdicts themselves: a conviction leg files
/// tens of reports per modified subject — one per disputed claim, per witness —
/// and a list of them would be a rate dressed up as evidence. What the
/// criterion asks is whether each modified subject was convicted at all, and
/// how long that took.
#[derive(Debug, Default, Clone)]
pub struct Docket {
    /// Reports re-run.
    pub adjudicated: u64,
    /// Verdicts that proved a deviation.
    pub confirms: u64,
    /// Verdicts that cleared the accused. **A conviction leg that produces
    /// these against a modified subject has found a hole in the evidence
    /// path**, not a clean peer.
    pub exonerates: u64,
    /// Verdicts that struck the reporter instead.
    pub evidence_forged: u64,
    /// Verdicts that decided nothing.
    pub unadjudicable: u64,
    /// Per convicted subject, the earliest tick a `Confirms` was reached at.
    ///
    /// Keyed by the subject's public key bytes so the map is `Serialize`-free
    /// and ordered; the swarm resolves them back to peer indices for the
    /// report.
    pub convicted_at: BTreeMap<[u8; 32], u64>,
}

impl Adjudicator {
    /// An adjudicator for one universe.
    #[must_use]
    pub fn new(seed: UniverseSeed) -> Self {
        Self { seed }
    }

    /// Adjudicate one report.
    ///
    /// The reporter's signature is checked first and **separately**, exactly as
    /// `orrery_persistd` does it. An unverifiable signature resolves as
    /// `Malformed` rather than `EvidenceForged`, because that verdict strikes
    /// the named reporter and an unverifiable signature is precisely the case
    /// where the name means nothing. Only after that does the evidence get
    /// re-run — a malformed accusation and a well-formed false one have to stay
    /// distinguishable.
    #[must_use]
    pub fn judge(&self, report: &DiscrepancyReport) -> Verdict {
        if orrery_witness::verify_report(report).is_err() {
            return Verdict::Unadjudicable(UnadjudicableReason::Malformed);
        }
        if report.bundle.ruleset != SKIRMISH_RULESET {
            return Verdict::Unadjudicable(UnadjudicableReason::UnknownRuleset);
        }
        verify_bundle(
            Skirmish::honest(),
            self.seed,
            report.subject,
            &report.bundle,
        )
    }
}

impl Docket {
    /// Re-run `report`, filed at simulated tick `at`, and record the verdict.
    pub fn record(&mut self, adjudicator: &Adjudicator, report: &DiscrepancyReport, at: u64) {
        self.adjudicated += 1;
        match adjudicator.judge(report) {
            Verdict::Confirms { .. } => {
                self.confirms += 1;
                // Earliest wins: the criterion is about how long a deviation
                // survives, and the tenth report against the same subject says
                // nothing about that.
                self.convicted_at
                    .entry(*report.subject.as_bytes())
                    .and_modify(|first| *first = (*first).min(at))
                    .or_insert(at);
            }
            Verdict::Exonerates => self.exonerates += 1,
            Verdict::EvidenceForged(_) => self.evidence_forged += 1,
            Verdict::Unadjudicable(_) => self.unadjudicable += 1,
        }
    }

    /// The tick a given subject was first convicted at.
    #[must_use]
    pub fn first_conviction(&self, subject: NodeId) -> Option<u64> {
        self.convicted_at.get(subject.as_bytes()).copied()
    }
}
