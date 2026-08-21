//! D29's low-population path: what may commit provisionally, what finalizes
//! it, and what an annulment writes.
//!
//! [D29](../../../../docs/adr/0029-low-population-path.md) decides all of this
//! and this module implements it; where a choice was open, the comment says so
//! and says which way it went. The one-paragraph version:
//!
//! An intent whose announced cell-epoch cannot supply `N` eligible witnesses
//! has nobody to co-sign it. P5's only fallback is a **provisional commit** —
//! durable, attributable, quarantined — followed by a **spot replay** that
//! either finalizes it or annuls it by writing the exact inverse. Field-host
//! witnessing, the other fallback `docs/07 §4.5` lists, is struck from P5 by
//! clause 1 and no part of it is built or stubbed here.
//!
//! # The three seams
//!
//! - [`classify`] — clause 3's `reversible(i)`. Runs at admission, needs no
//!   durable read, and decides whether an intent is the kind of thing an
//!   inverse can undo.
//! - [`ProvisionalStore`] — the durable half, implemented once against
//!   FoundationDB and once in memory, so the sweep logic below has exactly one
//!   implementation and the two tiers cannot drift on it.
//! - [`ProvisionalFinalizer`] — clause 7's scheduler. Sweeps unfinalized rows
//!   oldest first, fetches each one's evidence against the commitment it was
//!   committed under, routes it to [`crate::AdjudicationExecutor`], and writes
//!   the verdict back.
//!
//! # Not a relief valve
//!
//! The single most important property, and the one every change here has to
//! preserve: **an intent that could have been attested and simply was not is
//! refused, not committed provisionally.** Clause 2's admission function has
//! three outcomes and no fourth, and the third line is `otherwise → refuse`.
//! A provisional commit answers one specific fact about the world — that there
//! was nobody there to sign — and nothing else.

use orrery_protocol::{
    AccountId, AssetId, EvidenceBundle, EvidenceCommitment, ForgeryProof, Intent,
    UnadjudicableReason, Verdict, PROVISIONAL_FINALIZE_DEADLINE_MS,
};

use crate::keyspace::{ProvisionalHold, ProvisionalWrite};

use super::{ItemTransferArgs, RejectionCause, LEDGER_CREDIT_OP, LEDGER_ITEM_TRANSFER_OP};

/// The `args` width of [`LEDGER_CREDIT_OP`], duplicated from the private
/// constant in the parent module because this file decodes the same three
/// fields for a different question.
const LEDGER_CREDIT_ARGS_BYTES: usize = 24;

/// Clause 3's `reversible(i)`: whether the cluster can undo this intent's
/// entire durable effect by writing an inverse.
///
/// # What is admitted, and what the admission is for
///
/// **Value creation into escrow** — loot grants, crafting outputs,
/// progression, structure placement; concretely, any op whose credit and debit
/// are both inside the submitting account's own rows, plus every
/// `Ruleset`-opaque op, which this cluster's executor never applies and
/// therefore has nothing to reverse.
///
/// **Value transfer is refused**: any op naming a second account, and any
/// negative delta. The two-party trade — P5's reference intent — is therefore
/// refused in a low-population cell, never committed provisionally. D29 states
/// plainly that this is a real, player-visible product hole and the deliberate
/// price of a depth-1 annulment set: party exclusion removes both traders from
/// the eligible set anyway, so a trade in a two-person cell has no witnesses by
/// construction, and committing it provisionally would be committing the single
/// most cascade-prone operation on the least evidence.
///
/// # Why a negative delta is refused even though its inverse is a credit
///
/// Arithmetically a self-debit *is* reversible. It is refused because clause 3
/// is a rule about direction, not about arithmetic: the provisional path exists
/// to let a solo player loot and craft in an empty region, and a debit is a
/// sink. Refusing it costs nothing anyone asked for and keeps the classifier's
/// statement — "value only moves into the submitter's own rows" — true without
/// a caveat a later reader has to re-derive.
///
/// # Errors
///
/// The first [`RejectionCause`] this intent trips. Every one of them collapses
/// to a wire code that says *this is not the kind of intent that may be
/// committed unwitnessed*, which is a different sentence from "your ops were
/// wrong" and a different sentence again from "your attestations were wrong".
pub fn classify(intent: &Intent, account: Option<AccountId>) -> Result<(), RejectionCause> {
    // No session means no account, and an account is what the outstanding cap
    // is counted against, what the annulment notice is delivered to, and what
    // a strike is written against. A provisional commit the cluster cannot
    // attribute is a provisional commit nothing can be done about.
    let Some(account) = account else {
        return Err(RejectionCause::ProvisionalIneligible);
    };

    // Clause 6. An intent with no commitment is an intent the finalizer can
    // never hold to anything: it would commit, sit quarantined for five
    // minutes, and expire. Refusing now is free and resubmitting with a
    // commitment attached works; committing now converts the honest answer
    // into the one outcome clause 9(b) is arranged to avoid.
    if intent.evidence.is_none() {
        return Err(RejectionCause::ProvisionalNoEvidence);
    }

    for op in &intent.ops {
        match op.op {
            LEDGER_CREDIT_OP => {
                if op.args.len() != LEDGER_CREDIT_ARGS_BYTES {
                    return Err(RejectionCause::MalformedLedgerOp);
                }
                let field =
                    |i: usize| u64::from_le_bytes(op.args[i..i + 8].try_into().expect("slice len"));
                let credited = AccountId::new(field(0));
                let delta = i64::from_le_bytes(op.args[16..24].try_into().expect("slice len"));
                if credited != account || delta < 0 {
                    return Err(RejectionCause::ProvisionalIneligible);
                }
            }
            // A transfer names a second account by construction — the decode
            // is not even consulted, because there is no shape of transfer
            // this path admits. Decoding it anyway would invite a later reader
            // to add "unless the seller is also us", which is
            // `ItemTransferToSelf` and already refused above this path.
            LEDGER_ITEM_TRANSFER_OP => return Err(RejectionCause::ProvisionalIneligible),
            // `Ruleset`-opaque: this cluster's executor applies nothing for
            // it, so the inverse of its durable effect is the empty write set.
            _ => {}
        }
    }
    Ok(())
}

/// Every `ledger/bal/{account}/{asset}` row an intent names, in first-seen
/// order.
///
/// This is the read-and-write set clause 4's quarantine is checked against: an
/// intent may not name a row an unfinalized provisional commit wrote, and these
/// are the rows an intent can name. Item rows are absent from the list on
/// purpose — clause 3 refuses transfers on the provisional path, so no
/// provisional intent has ever written one and there is no held item row for an
/// intent to trip over. If that ever changes, this function is where the
/// second half goes.
#[must_use]
pub fn named_balances(intent: &Intent) -> Vec<(AccountId, AssetId)> {
    let mut named: Vec<(AccountId, AssetId)> = Vec::new();
    let mut push = |account: AccountId, asset: AssetId| {
        if !named.contains(&(account, asset)) {
            named.push((account, asset));
        }
    };
    for op in &intent.ops {
        match op.op {
            LEDGER_CREDIT_OP if op.args.len() == LEDGER_CREDIT_ARGS_BYTES => {
                let field =
                    |i: usize| u64::from_le_bytes(op.args[i..i + 8].try_into().expect("slice len"));
                push(AccountId::new(field(0)), AssetId::new(field(8)));
            }
            LEDGER_ITEM_TRANSFER_OP => {
                if let Ok(transfer) = ItemTransferArgs::decode(&op.args) {
                    // Both sides. The buyer's row is read (the sufficiency
                    // check) and the seller's is written (the blind credit),
                    // and clause 4 quarantines a provisional row against
                    // *either*.
                    push(transfer.buyer, transfer.asset);
                    push(transfer.seller, transfer.asset);
                }
            }
            _ => {}
        }
    }
    named
}

/// The balance writes a planned intent will apply, in the form annulment needs
/// to invert them.
///
/// Only [`super::PlannedWrite::BalanceAdd`] survives the conversion, and under
/// [`classify`] that is the only kind a provisional intent can produce — an
/// `ItemOwner` write would mean a transfer was admitted, which this path
/// refuses. The match is exhaustive rather than filtered so that adding a third
/// `PlannedWrite` kind is a compile error here instead of a silently
/// un-invertible effect.
pub(crate) fn provisional_writes(writes: &[super::PlannedWrite]) -> Vec<ProvisionalWrite> {
    writes
        .iter()
        .filter_map(|write| match write {
            super::PlannedWrite::BalanceAdd {
                account,
                asset,
                delta,
            } => Some(ProvisionalWrite {
                account: *account,
                asset: *asset,
                delta: *delta,
            }),
            super::PlannedWrite::ItemOwner { .. } => None,
        })
        .collect()
}

/// The deadline a provisional commit landing at `now_ms` carries.
#[must_use]
pub const fn finalize_by(now_ms: u64) -> u64 {
    now_ms.saturating_add(PROVISIONAL_FINALIZE_DEADLINE_MS)
}

/// What the finalizer decided about one provisional intent.
///
/// # The mapping is clause 7's table, and one arm of it is asymmetric
///
/// ```text
/// Verdict::Confirms        ->  Annulled, strike the submitter
/// Verdict::Exonerates      ->  Final,    no strike
/// Verdict::EvidenceForged  ->  Annulled, strike the submitter
/// Verdict::Unadjudicable   ->  Annulled, never a strike
/// ```
///
/// D29 clause 7 writes the first two arms as `Deviation` and `WithinTolerance`;
/// the landed enum spells them [`Verdict::Confirms`] and [`Verdict::Exonerates`]
/// (`orrery_protocol::verifiable`). Same four verdicts, same meanings — the
/// record was written against the prose of `docs/07` rather than against the
/// type, and this comment is the mapping so a reader holding the ADR does not
/// have to guess.
///
/// `EvidenceForged` is the asymmetric one. On the discrepancy-report path that
/// verdict protects an *accused* peer from a lying reporter and strikes the
/// reporter; here there is no third party, and the account that fabricated the
/// bundle is the account that submitted the intent. D29 flags this remapping as
/// the one thing in clause 7 worth a second look in review.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Finalization {
    /// The replay agreed with the submitter's history: promote the row to
    /// [`crate::keyspace::IntentFinality::Final`].
    Finalize,
    /// The replay disagreed, could not be performed, or the deadline passed:
    /// write the inverse and record
    /// [`crate::keyspace::IntentFinality::Annulled`].
    Annul {
        /// Whether the submitter is struck for it. `false` for every
        /// `Unadjudicable` cause, per D10 item 4: a cluster that cannot judge
        /// does not punish.
        strike: bool,
    },
}

impl Finalization {
    /// Clause 7's verdict table.
    #[must_use]
    pub const fn from_verdict(verdict: &Verdict) -> Self {
        match verdict {
            Verdict::Exonerates => Self::Finalize,
            Verdict::Confirms { .. } | Verdict::EvidenceForged(_) => Self::Annul { strike: true },
            Verdict::Unadjudicable(_) => Self::Annul { strike: false },
        }
    }

    /// Whether this is an annulment.
    #[must_use]
    pub const fn annuls(self) -> bool {
        matches!(self, Self::Annul { .. })
    }
}

/// Where the finalizer gets the [`EvidenceBundle`] a commitment names.
///
/// # The named weakness, restated where the code is
///
/// In an empty cell the submitter is usually the only source of the evidence
/// that would convict it. `docs/07 §5` already has the cluster assemble a
/// segment "from other witness-set peers holding the stream", and in a
/// low-population cell there may be none. D29 does not pretend otherwise; it
/// removes the incentive instead. Failing to produce a bundle that matches the
/// commitment, inside the deadline, is **annulment** — so losing the evidence
/// gains the submitter exactly what a deviation verdict would have, and the
/// non-cooperation is scored the way `docs/07 §6` already scores it.
///
/// A deployment with no source configured therefore annuls everything it
/// commits provisionally, five minutes later, with no strike. That is a loud,
/// correct failure rather than a quiet one: the cluster is minting durable
/// value in cells nothing is checking, which is the posture P5 exists to
/// abolish.
#[async_trait::async_trait]
pub trait EvidenceSource: Send + Sync {
    /// Fetch the bundle for `hold`, and the submitter's chain head that the
    /// commitment pinned.
    ///
    /// `None` is "could not be obtained" and is not distinguished from
    /// "refused": both are non-cooperation, both annul, and a source that
    /// wanted them distinguished would be asking the finalizer to weigh an
    /// excuse.
    async fn fetch(&self, hold: &ProvisionalHold) -> Option<FetchedEvidence>;
}

/// One fetched bundle, with the chain head the fetcher folded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchedEvidence {
    /// The bundle to replay.
    pub bundle: EvidenceBundle,
    /// The submitter's full chain head at the window's end, as the fetcher
    /// computed it. Compared against the commitment, never trusted into it.
    pub log_head: orrery_protocol::ChainHash,
}

/// The durable operations the finalizer needs, so the sweep logic has one
/// implementation across the FoundationDB and in-memory tiers.
///
/// Every method is a whole serializable transaction on the FDB side. In
/// particular [`Self::annul`] is **one** transaction — the inverse writes, the
/// finality flip, the restamped GC deadline and the compensating receipt all
/// commit together or not at all, which is what makes an annulment a fact
/// rather than a process that can be interrupted halfway.
#[async_trait::async_trait]
pub trait ProvisionalStore: Send + Sync {
    /// Every unfinalized provisional intent, oldest commit first.
    async fn outstanding(&self) -> Result<Vec<ProvisionalHold>, super::IntentError>;

    /// Promote `hold`'s intent to `Final`. Writes nothing to the ledger: the
    /// effects have been durable since the provisional commit, and
    /// finalization is a statement about the history behind them.
    async fn finalize(&self, hold: &ProvisionalHold) -> Result<(), super::IntentError>;

    /// Write the exact inverse of `hold`'s writes, flip the row to `Annulled`,
    /// restamp its GC deadline, append a compensating receipt, and release the
    /// hold — in one transaction.
    async fn annul(&self, hold: &ProvisionalHold) -> Result<(), super::IntentError>;
}

/// What one sweep did, for the operator and for a test.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FinalizerReport {
    /// Provisional intents examined.
    pub examined: u64,
    /// Intents promoted to `Final` by an agreeing replay.
    pub finalized: u64,
    /// Intents annulled by a disagreeing replay, a missing bundle, or a
    /// commitment mismatch.
    pub annulled: u64,
    /// Of those, the ones annulled because their deadline had passed.
    ///
    /// Counted separately because it is the one number that is an **incident**
    /// rather than a workload. Clause 9(b): the cluster is arranged to refuse
    /// new provisional intents long before it expires old ones, so a nonzero
    /// value here means admission control did not hold.
    pub expired: u64,
    /// Annulments that carry a strike (`Deviation` or `EvidenceForged`).
    pub struck: u64,
}

/// Clause 7's scheduler: a sweep over unfinalized rows, oldest first, at a
/// sampling rate of **1**.
///
/// # Why the executor is shared and the scheduler is not
///
/// [`crate::AdjudicationExecutor`] is a pure router over version-keyed builds,
/// and `RETAINED_BUILDS` is the scarce resource. Two registries would give two
/// answers to "which build adjudicates this window", which is the failure the
/// version-keyed routing was designed to prevent. But the two workloads have
/// nothing else in common:
///
/// | | discrepancy adjudication | provisional finalization |
/// |---|---|---|
/// | trigger | a peer files a report | a durable row exists in `Provisional` |
/// | queue | event-driven, per-account rate-limited | a sweep over unfinalized rows, oldest first |
/// | entry | `adjudicate(&DiscrepancyReport)` | [`crate::AdjudicationExecutor::finalize_provisional`] |
/// | sampling | sampled, prioritised by strike score | 1, always |
///
/// # The sampling rate is 1 and is not a parameter
///
/// `docs/07 §4.5` allows "100% for high-value intents … sampled for the rest",
/// and D29 clause 3 deletes the dial rather than defaulting it: a rate `p < 1`
/// hands an attacker an unexamined durable commit with probability `1 − p` per
/// attempt, farmable by repetition, and in a low-population cell there is by
/// construction no independent check to cover the residue. There is no field
/// here to set, so nobody adds the dial back without reopening the record.
///
/// The load this buys is affordable, which is why the choice was available:
/// a full 180-tick single-entity bundle measures under 5 ms, so one executor
/// core clears roughly 200 windows per second — against `r_lowpop`, the intent
/// rate in cells too empty to witness, which is by definition the rate in the
/// parts of the world nobody is in.
pub struct ProvisionalFinalizer<'a> {
    judge: &'a dyn ReplayJudge,
    store: &'a dyn ProvisionalStore,
    evidence: &'a dyn EvidenceSource,
}

/// The replay half of finalization, behind a seam.
///
/// [`crate::AdjudicationExecutor`] is the implementation and clause 7 requires
/// it to be — sharing that executor is what keeps `RETAINED_BUILDS` the single
/// answer to "which build adjudicates this window". The trait exists so the
/// *sweep* — the deadline rule, the verdict table, the order of operations —
/// can be tested without registering a `Ruleset` build, which would make the
/// test for clause 9(a)'s "expiry annuls, never finalizes" rule depend on a
/// replay it is specifically asserting never happens.
pub trait ReplayJudge: Send + Sync {
    /// Replay `bundle` against `commitment`, attributing the verdict to
    /// `subject`.
    fn judge_provisional(
        &self,
        subject: orrery_protocol::NodeId,
        commitment: &EvidenceCommitment,
        bundle: &EvidenceBundle,
        log_head: orrery_protocol::ChainHash,
    ) -> Verdict;
}

impl ReplayJudge for crate::AdjudicationExecutor {
    fn judge_provisional(
        &self,
        subject: orrery_protocol::NodeId,
        commitment: &EvidenceCommitment,
        bundle: &EvidenceBundle,
        log_head: orrery_protocol::ChainHash,
    ) -> Verdict {
        self.finalize_provisional(subject, commitment, bundle, log_head)
    }
}

impl<'a> ProvisionalFinalizer<'a> {
    /// A finalizer over one replay judge, one store and one evidence source.
    #[must_use]
    pub fn new(
        judge: &'a dyn ReplayJudge,
        store: &'a dyn ProvisionalStore,
        evidence: &'a dyn EvidenceSource,
    ) -> Self {
        Self {
            judge,
            store,
            evidence,
        }
    }

    /// Sweep every outstanding provisional intent once.
    ///
    /// Deadline first, replay second, and the order is the point: an intent
    /// past its deadline is annulled **without** being replayed, because
    /// replaying it could only produce `Finalize`, and finalizing at the
    /// deadline is exactly the behaviour clause 9(a) refuses. Auto-finalizing
    /// would make "outlast the replay queue" a strategy and convert a
    /// denial-of-service against the adjudication fleet into a dupe vector.
    ///
    /// # Errors
    ///
    /// The first store failure. A sweep that cannot read its own index has no
    /// safe way to continue, and the intents it did not reach keep their
    /// deadlines — the next sweep sees them again, and an outage long enough to
    /// pass a deadline annuls rather than finalizes, which is the fail-closed
    /// direction.
    pub async fn sweep(&self, now_ms: u64) -> Result<FinalizerReport, super::IntentError> {
        let mut report = FinalizerReport::default();
        for hold in self.store.outstanding().await? {
            report.examined += 1;
            let (action, expired) = if now_ms >= hold.finalize_by_ms {
                (Finalization::Annul { strike: false }, true)
            } else {
                (self.judge(&hold).await, false)
            };
            match action {
                Finalization::Finalize => {
                    self.store.finalize(&hold).await?;
                    report.finalized += 1;
                }
                Finalization::Annul { strike } => {
                    self.store.annul(&hold).await?;
                    report.annulled += 1;
                    report.expired += u64::from(expired);
                    report.struck += u64::from(strike);
                }
            }
        }
        Ok(report)
    }

    /// Fetch this hold's evidence, check it against the commitment, and replay
    /// it.
    async fn judge(&self, hold: &ProvisionalHold) -> Finalization {
        let Some(fetched) = self.evidence.fetch(hold).await else {
            // Non-cooperation. Not a strike: the account may be offline, and
            // D10 item 4 keeps "the cluster could not judge" and "the account
            // cheated" apart. The value is gone either way, which is what
            // removes the incentive to lose evidence on purpose.
            return Finalization::Annul { strike: false };
        };
        let verdict = self.judge.judge_provisional(
            hold.subject,
            &hold.commitment,
            &fetched.bundle,
            fetched.log_head,
        );
        Finalization::from_verdict(&verdict)
    }
}

/// Whether a fetched bundle is the one a commitment named.
///
/// Field by field, with no tolerance anywhere: the commitment is 124 bytes of
/// fixed-width fields and the bundle either reproduces all of them or is a
/// different history. This is the whole property clause 6 traded the attached
/// evidence for — the submitter cannot substitute a friendlier segment after
/// the fact, because the segment it must produce was pinned before it knew what
/// the cluster would ask.
#[must_use]
pub fn commitment_matches(
    commitment: &EvidenceCommitment,
    bundle: &EvidenceBundle,
    log_head: orrery_protocol::ChainHash,
) -> bool {
    *commitment
        == EvidenceCommitment::from_bundle(
            bundle,
            orrery_core::log::claim_hash(&bundle.t0_claim),
            log_head,
        )
}

/// The verdict a commitment mismatch produces.
///
/// `EvidenceForged`, not `Unadjudicable`: a bundle that does not reproduce a
/// commitment the submitter itself signed is a bundle the submitter
/// substituted, and on this path the evidence's author and the intent's
/// submitter are the same account. `UnadjudicableReason::Malformed` would be
/// the answer if the *fetcher* were suspect, and it is not — the fetcher is
/// the cluster.
///
/// The proof carried is [`ForgeryProof::CommitmentMismatch`], the arm added for
/// exactly this: nothing failed to verify — the signatures on the substituted
/// history are perfectly good signatures over a different history — and what is
/// proven is the substitution itself.
#[must_use]
pub const fn mismatch_verdict() -> Verdict {
    Verdict::EvidenceForged(ForgeryProof::CommitmentMismatch)
}

/// The verdict a build older than the retained window produces.
///
/// Never a strike. `RETAINED_BUILDS` bounds both adjudication workloads, so a
/// provisional intent pinning a build older than the last three finalizes as
/// `Unadjudicable` and is annulled — a new way for an honest player to lose an
/// item during a rules upgrade, and the same trade the report path already
/// accepted for the same reason.
#[must_use]
pub const fn unknown_ruleset_verdict() -> Verdict {
    Verdict::Unadjudicable(UnadjudicableReason::UnknownRuleset)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::intent::tests_support::{credit_args, provisional_intent, transfer_args};
    use orrery_protocol::IntentOp;

    fn account(id: u64) -> AccountId {
        AccountId::new(id)
    }

    #[test]
    fn a_self_credit_with_a_commitment_is_provisional_eligible() {
        let intent = provisional_intent(
            1,
            vec![IntentOp {
                op: LEDGER_CREDIT_OP,
                args: credit_args(7, 3, 100),
            }],
            true,
        );
        assert_eq!(classify(&intent, Some(account(7))), Ok(()));
    }

    #[test]
    fn a_credit_to_another_account_is_refused() {
        let intent = provisional_intent(
            2,
            vec![IntentOp {
                op: LEDGER_CREDIT_OP,
                args: credit_args(8, 3, 100),
            }],
            true,
        );
        assert_eq!(
            classify(&intent, Some(account(7))),
            Err(RejectionCause::ProvisionalIneligible)
        );
    }

    #[test]
    fn a_negative_self_credit_is_refused_as_a_sink() {
        let intent = provisional_intent(
            3,
            vec![IntentOp {
                op: LEDGER_CREDIT_OP,
                args: credit_args(7, 3, -100),
            }],
            true,
        );
        assert_eq!(
            classify(&intent, Some(account(7))),
            Err(RejectionCause::ProvisionalIneligible)
        );
    }

    #[test]
    fn a_transfer_is_refused_however_it_is_shaped() {
        // D29 clause 3's most consequential refusal, and the one with a
        // product cost: the reference two-party trade does not work in an
        // empty region, at all.
        let intent = provisional_intent(
            4,
            vec![IntentOp {
                op: LEDGER_ITEM_TRANSFER_OP,
                args: transfer_args(1, 8, 7, 3, 10),
            }],
            true,
        );
        assert_eq!(
            classify(&intent, Some(account(7))),
            Err(RejectionCause::ProvisionalIneligible)
        );
    }

    #[test]
    fn an_intent_with_no_commitment_is_refused_rather_than_committed_to_expire() {
        let intent = provisional_intent(
            5,
            vec![IntentOp {
                op: LEDGER_CREDIT_OP,
                args: credit_args(7, 3, 100),
            }],
            false,
        );
        assert_eq!(
            classify(&intent, Some(account(7))),
            Err(RejectionCause::ProvisionalNoEvidence)
        );
    }

    #[test]
    fn an_unauthenticated_connection_is_refused() {
        let intent = provisional_intent(
            6,
            vec![IntentOp {
                op: LEDGER_CREDIT_OP,
                args: credit_args(7, 3, 100),
            }],
            true,
        );
        assert_eq!(
            classify(&intent, None),
            Err(RejectionCause::ProvisionalIneligible)
        );
    }

    #[test]
    fn opaque_ops_are_eligible_because_the_cluster_applies_nothing_for_them() {
        let intent = provisional_intent(
            7,
            vec![IntentOp {
                op: 100,
                args: bytes::Bytes::from_static(b"opaque"),
            }],
            true,
        );
        assert_eq!(classify(&intent, Some(account(7))), Ok(()));
    }

    #[test]
    fn named_balances_covers_both_sides_of_a_transfer() {
        let intent = provisional_intent(
            8,
            vec![
                IntentOp {
                    op: LEDGER_ITEM_TRANSFER_OP,
                    args: transfer_args(1, 8, 7, 3, 10),
                },
                IntentOp {
                    op: LEDGER_CREDIT_OP,
                    args: credit_args(7, 3, 5),
                },
            ],
            true,
        );
        // Buyer and seller, deduplicated against the credit's identical row.
        assert_eq!(
            named_balances(&intent),
            vec![(account(7), AssetId::new(3)), (account(8), AssetId::new(3)),]
        );
    }

    // ── The durable path, end to end, on the in-memory tier ────────────
    //
    // Every assertion below is one of #150's acceptance items, and each test
    // is named for the sentence it proves rather than for the function it
    // calls: the point of this file is a set of guarantees, and a reader
    // checking D29 against the tree should be able to find each clause by
    // reading the test names.

    use crate::intent::{IntentExecutor, MemIntentExecutor};
    use crate::keyspace::{IntentFinality, ProvisionalHold};
    use orrery_protocol::{IntentOutcome, PersistId, StateClaim, Tick};

    const GOLD: u64 = 3;
    const ALICE: u64 = 7;

    fn alice() -> AccountId {
        AccountId::new(ALICE)
    }

    /// A provisional-eligible intent that names **no** ledger row: one
    /// `Ruleset`-opaque op.
    ///
    /// The shape most of the quarantine tests need, and the reason is worth
    /// stating because it is a real consequence of clause 4 rather than a test
    /// convenience: two provisional credits to the *same*
    /// `ledger/bal/{account}/{asset}` row cannot coexist, because the second
    /// one names a row the first one holds. So an account's outstanding
    /// provisional set is at most one intent **per balance row** it touches,
    /// plus any number of intents that touch none. That is stricter than
    /// clause 9(b)'s cap of eight and it is the quarantine doing the bounding,
    /// not the cap — which is the correct order of those two mechanisms:
    /// containment first, value-at-risk dial second.
    fn opaque(id: u128) -> Intent {
        provisional_intent(
            id,
            vec![orrery_protocol::IntentOp {
                op: 100,
                args: bytes::Bytes::from_static(b"opaque"),
            }],
            true,
        )
    }

    /// A provisional-eligible intent: one self-credit of `delta` gold.
    fn loot(id: u128, delta: i64) -> Intent {
        provisional_intent(
            id,
            vec![orrery_protocol::IntentOp {
                op: LEDGER_CREDIT_OP,
                args: credit_args(ALICE, GOLD, delta),
            }],
            true,
        )
    }

    /// A replay judge that answers with a fixed verdict and counts how often
    /// it was asked.
    struct FixedJudge {
        verdict: Verdict,
        asked: std::sync::atomic::AtomicUsize,
    }

    impl FixedJudge {
        fn new(verdict: Verdict) -> Self {
            Self {
                verdict,
                asked: std::sync::atomic::AtomicUsize::new(0),
            }
        }

        fn asked(&self) -> usize {
            self.asked.load(std::sync::atomic::Ordering::Relaxed)
        }
    }

    impl ReplayJudge for FixedJudge {
        fn judge_provisional(
            &self,
            _subject: orrery_protocol::NodeId,
            _commitment: &EvidenceCommitment,
            _bundle: &EvidenceBundle,
            _log_head: orrery_protocol::ChainHash,
        ) -> Verdict {
            self.asked
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            self.verdict
        }
    }

    /// An evidence source that always produces a bundle. Its contents do not
    /// matter to these tests — [`FixedJudge`] never looks — because what is
    /// under test here is the sweep, not the replay.
    struct AlwaysFetches;

    #[async_trait::async_trait]
    impl EvidenceSource for AlwaysFetches {
        async fn fetch(&self, _hold: &ProvisionalHold) -> Option<FetchedEvidence> {
            Some(FetchedEvidence {
                bundle: empty_bundle(),
                log_head: orrery_protocol::ChainHash::EMPTY,
            })
        }
    }

    /// An evidence source that never produces one — the empty-cell case D29
    /// clause 6 names as this path's known weakness.
    struct NeverFetches;

    #[async_trait::async_trait]
    impl EvidenceSource for NeverFetches {
        async fn fetch(&self, _hold: &ProvisionalHold) -> Option<FetchedEvidence> {
            None
        }
    }

    fn empty_bundle() -> EvidenceBundle {
        let key = crate::intent::tests_support::issuer_key();
        EvidenceBundle {
            ruleset: orrery_protocol::RulesetId {
                version: 1,
                digest: [7; 32],
            },
            entity: PersistId::new(11),
            window_start: Tick::new(100),
            window_end: Tick::new(160),
            t0_claim: StateClaim {
                entity: PersistId::new(11),
                chain_epoch: 0,
                tick: Tick::new(100),
                input_head: orrery_protocol::ChainHash::EMPTY,
                state_hash: [0; 32],
                prev_claim: [0; 32],
                ruleset: orrery_protocol::RulesetId {
                    version: 1,
                    digest: [7; 32],
                },
                sig: key.sign(b"placeholder"),
            },
            t0_snapshot: bytes::Bytes::new(),
            frames: Vec::new(),
            sibling_heads: Vec::new(),
            disputed_claims: Vec::new(),
            claimed_hashes: Vec::new(),
            computed_hashes: Vec::new(),
        }
    }

    #[tokio::test]
    async fn a_provisional_commit_is_durable_visible_and_attributable() {
        // D29 clause 4's first three adjectives, and they are not rhetoric:
        // the balance really moved, the row really says what happened, and the
        // account it is charged to is on the hold.
        let exec = MemIntentExecutor::new();
        let outcome = exec
            .execute_provisional(&loot(1, 100), alice())
            .await
            .expect("no executor error");
        let IntentOutcome::Provisional { finalize_by, .. } = outcome else {
            panic!("expected a Provisional outcome, got {outcome:?}");
        };
        assert!(finalize_by > 0, "the deadline is stamped at commit");
        assert_eq!(
            exec.balance(alice(), AssetId::new(GOLD)),
            100,
            "the value is real: a provisional commit is a commit"
        );
        let row = exec.intent_row(1).expect("durable row");
        assert_eq!(row.finality, IntentFinality::Provisional);
        assert_eq!(row.finalize_by_ms, finalize_by);
        let holds = exec.provisional_holds(alice());
        assert_eq!(holds.len(), 1);
        assert_eq!(holds[0].intent_id, 1);
        assert_eq!(holds[0].account, alice());
    }

    #[tokio::test]
    async fn a_provisional_row_cannot_be_an_input_to_another_intent() {
        // **The assertion this issue exists for.** Clause 4 bounds the
        // annulment set at exactly one intent, and it does so by making a
        // provisional output an input to nothing. If this test ever fails, the
        // set that must be reversed on annulment becomes the transitive
        // closure of everything derived from the intent — across accounts, and
        // including intents that have since finalized — which is a problem
        // with no correct answer rather than a bigger version of this one.
        let exec = MemIntentExecutor::new();
        exec.execute_provisional(&loot(1, 100), alice())
            .await
            .expect("no executor error");

        // A second intent naming the same `ledger/bal/{alice}/{gold}` row. It
        // is an ordinary, fully attested credit — the refusal is about the
        // row's state, not about this intent's own standing.
        let spend = provisional_intent(
            2,
            vec![orrery_protocol::IntentOp {
                op: LEDGER_CREDIT_OP,
                args: credit_args(ALICE, GOLD, 5),
            }],
            true,
        );
        assert_eq!(
            exec.execute(&spend).await.expect("no executor error"),
            IntentOutcome::Rejected {
                reason: orrery_protocol::REASON_PROVISIONAL_INPUT
            },
            "a row an unfinalized provisional commit wrote is an input to nothing"
        );
        assert_eq!(
            exec.balance(alice(), AssetId::new(GOLD)),
            100,
            "and the refusal applied nothing"
        );

        // Finalizing releases the quarantine, and the same intent then works
        // unchanged — which is the fact `REASON_PROVISIONAL_INPUT` exists to
        // convey and `REASON_VALIDATION_FAILED` could not have.
        let hold = exec.provisional_holds(alice()).remove(0);
        exec.finalize(&hold).await.expect("finalize");
        assert!(matches!(
            exec.execute(&spend).await.expect("no executor error"),
            IntentOutcome::Committed { .. }
        ));
        assert_eq!(exec.balance(alice(), AssetId::new(GOLD)), 105);
    }

    #[tokio::test]
    async fn a_second_provisional_credit_to_a_held_row_is_refused_too() {
        // Clause 4 does not exempt the account that created the hold. The rule
        // is about the row's state, and "no intent may name a provisionally
        // committed row" means no intent — which is what keeps the annulment
        // set at exactly one intent even for a submitter looting the same
        // currency twice in a row.
        let exec = MemIntentExecutor::new();
        exec.execute_provisional(&loot(1, 100), alice())
            .await
            .expect("no executor error");
        assert_eq!(
            exec.execute_provisional(&loot(2, 100), alice())
                .await
                .expect("no executor error"),
            IntentOutcome::Rejected {
                reason: orrery_protocol::REASON_PROVISIONAL_INPUT
            }
        );
    }

    #[tokio::test]
    async fn the_per_account_cap_refuses_rather_than_waiting_to_annul() {
        // Clause 9(b). The cluster's answer to a finalizer that cannot keep up
        // is to stop admitting, not to start expiring: a refusal costs the
        // player nothing it had, and an expiry destroys value the cluster
        // already promised.
        // Opaque ops, so that the quarantine — which is *stricter* per balance
        // row than the cap is per account — is not what does the refusing.
        // This test is about the cap and has to reach it.
        let exec = MemIntentExecutor::new();
        for id in 0..orrery_protocol::PROVISIONAL_OUTSTANDING_CAP {
            let outcome = exec
                .execute_provisional(&opaque(id as u128), alice())
                .await
                .expect("no executor error");
            assert!(matches!(outcome, IntentOutcome::Provisional { .. }));
        }
        let over = orrery_protocol::PROVISIONAL_OUTSTANDING_CAP as u128;
        assert_eq!(
            exec.execute_provisional(&opaque(over), alice())
                .await
                .expect("no executor error"),
            IntentOutcome::Rejected {
                reason: orrery_protocol::REASON_PROVISIONAL_CAP
            }
        );
        assert_eq!(
            exec.provisional_holds(alice()).len(),
            orrery_protocol::PROVISIONAL_OUTSTANDING_CAP,
            "the refused intent took no slot"
        );
    }

    #[tokio::test]
    async fn spot_replay_finalizes_an_agreeing_intent() {
        let exec = MemIntentExecutor::new();
        exec.execute_provisional(&loot(1, 100), alice())
            .await
            .expect("no executor error");
        let judge = FixedJudge::new(Verdict::Exonerates);
        let report = ProvisionalFinalizer::new(&judge, &exec, &AlwaysFetches)
            .sweep(0)
            .await
            .expect("sweep");
        assert_eq!(
            (report.examined, report.finalized, report.annulled),
            (1, 1, 0)
        );
        assert_eq!(judge.asked(), 1, "sampling rate 1: every row is replayed");
        assert_eq!(
            exec.intent_row(1).expect("row").finality,
            IntentFinality::Final
        );
        assert!(
            exec.provisional_holds(alice()).is_empty(),
            "finalization releases the quarantine"
        );
        assert_eq!(
            exec.balance(alice(), AssetId::new(GOLD)),
            100,
            "finalization writes nothing to the ledger: the effects were already there"
        );
    }

    #[tokio::test]
    async fn spot_replay_annuls_a_disagreeing_intent_by_writing_the_inverse() {
        let exec = MemIntentExecutor::new();
        exec.execute_provisional(&loot(1, 100), alice())
            .await
            .expect("no executor error");
        let receipts_before = exec.receipts().len();
        let judge = FixedJudge::new(Verdict::Confirms {
            at: Tick::new(140),
            kind: orrery_protocol::DeviationKind::DiscreteMismatch,
        });
        let report = ProvisionalFinalizer::new(&judge, &exec, &AlwaysFetches)
            .sweep(0)
            .await
            .expect("sweep");
        assert_eq!(
            (
                report.finalized,
                report.annulled,
                report.struck,
                report.expired
            ),
            (0, 1, 1, 0),
            "a proven deviation annuls and strikes, and is not an expiry"
        );
        assert_eq!(
            exec.balance(alice(), AssetId::new(GOLD)),
            0,
            "the inverse is written forward: +100 then -100"
        );
        assert_eq!(
            exec.intent_row(1).expect("row").finality,
            IntentFinality::Annulled,
            "and the reversal is recorded distinguishably from a finalization"
        );
        assert_eq!(
            exec.receipts().len(),
            receipts_before + 1,
            "annulment appends a compensating receipt; it never removes one"
        );
    }

    #[tokio::test]
    async fn a_missing_bundle_annuls_without_a_strike() {
        // Clause 6's named weakness, and its answer: in an empty cell the
        // submitter is often the only source of the evidence that would
        // convict it, so failing to produce it gains exactly what a deviation
        // verdict would have. No strike, because "the cluster could not judge"
        // and "the account cheated" are different findings.
        let exec = MemIntentExecutor::new();
        exec.execute_provisional(&loot(1, 100), alice())
            .await
            .expect("no executor error");
        let judge = FixedJudge::new(Verdict::Exonerates);
        let report = ProvisionalFinalizer::new(&judge, &exec, &NeverFetches)
            .sweep(0)
            .await
            .expect("sweep");
        assert_eq!((report.annulled, report.struck), (1, 0));
        assert_eq!(judge.asked(), 0, "there was nothing to replay");
        assert_eq!(exec.balance(alice(), AssetId::new(GOLD)), 0);
    }

    #[tokio::test]
    async fn the_deadline_annuls_and_never_auto_finalizes() {
        // Clause 9(a). Auto-finalizing at the deadline would make "outlast the
        // replay queue" a strategy, converting a denial-of-service against the
        // adjudication fleet into a dupe vector. The judge here would have
        // said `Exonerates`; it is never asked.
        let exec = MemIntentExecutor::new();
        exec.execute_provisional(&loot(1, 100), alice())
            .await
            .expect("no executor error");
        let hold = exec.provisional_holds(alice()).remove(0);
        let judge = FixedJudge::new(Verdict::Exonerates);
        let report = ProvisionalFinalizer::new(&judge, &exec, &AlwaysFetches)
            .sweep(hold.finalize_by_ms)
            .await
            .expect("sweep");
        assert_eq!(
            (
                report.finalized,
                report.annulled,
                report.expired,
                report.struck
            ),
            (0, 1, 1, 0),
            "expiry annuls, is counted as an incident, and strikes nobody"
        );
        assert_eq!(judge.asked(), 0, "a past-deadline row is not replayed");
        assert_eq!(
            exec.intent_row(1).expect("row").finality,
            IntentFinality::Annulled
        );
        assert_eq!(exec.balance(alice(), AssetId::new(GOLD)), 0);
    }

    #[tokio::test]
    async fn a_replayed_provisional_intent_returns_provisional_not_a_second_commit() {
        let exec = MemIntentExecutor::new();
        let first = exec
            .execute_provisional(&loot(1, 100), alice())
            .await
            .expect("no executor error");
        let second = exec
            .execute_provisional(&loot(1, 100), alice())
            .await
            .expect("no executor error");
        assert_eq!(second, first, "the idempotency row answers, unchanged");
        assert_eq!(
            exec.balance(alice(), AssetId::new(GOLD)),
            100,
            "and nothing was applied twice"
        );
        assert_eq!(
            exec.provisional_holds(alice()).len(),
            1,
            "nor did the replay take a second slot against the cap"
        );
    }

    #[tokio::test]
    async fn a_replayed_annulled_intent_is_refused_and_does_not_reapply() {
        let exec = MemIntentExecutor::new();
        exec.execute_provisional(&loot(1, 100), alice())
            .await
            .expect("no executor error");
        let judge = FixedJudge::new(Verdict::Exonerates);
        ProvisionalFinalizer::new(&judge, &exec, &NeverFetches)
            .sweep(0)
            .await
            .expect("sweep");
        assert_eq!(
            exec.execute_provisional(&loot(1, 100), alice())
                .await
                .expect("no executor error"),
            IntentOutcome::Rejected {
                reason: orrery_protocol::REASON_INTENT_ANNULLED
            },
            "the row survives its reversal so the replay can be answered"
        );
        assert_eq!(
            exec.balance(alice(), AssetId::new(GOLD)),
            0,
            "and the replay applies nothing"
        );
    }

    #[tokio::test]
    async fn the_gc_interlock_refuses_to_sweep_a_provisional_row() {
        // Clause 9(c). A row saying "provisional" can never vanish under a
        // replay, because it is not sweepable while provisional — whatever its
        // deadline says. That is what closes the dupe vector a swept
        // idempotency row would open.
        let exec = MemIntentExecutor::new();
        exec.execute_provisional(&loot(1, 100), alice())
            .await
            .expect("no executor error");
        let row = exec.intent_row(1).expect("row");
        assert!(
            !crate::keyspace::sweepable(&row, u64::MAX),
            "not sweepable at any clock while unresolved"
        );
        let hold = exec.provisional_holds(alice()).remove(0);
        exec.annul(&hold).await.expect("annul");
        let annulled = exec.intent_row(1).expect("row");
        assert!(
            !crate::keyspace::sweepable(&annulled, annulled.gc_deadline_ms - 1),
            "an annulled row still serves its retention"
        );
        assert!(
            crate::keyspace::sweepable(&annulled, annulled.gc_deadline_ms),
            "and becomes sweepable once it expires, restamped from the annulment"
        );
    }

    #[tokio::test]
    async fn the_sweep_takes_the_oldest_first() {
        // Clause 7's queue discipline, asserted because the alternative — a
        // `HashMap` iteration order — would make the finalizer's fairness a
        // property of the allocator.
        let exec = MemIntentExecutor::new();
        for id in 1..=3u128 {
            exec.execute_provisional(&opaque(id), alice())
                .await
                .expect("no executor error");
        }
        let holds = exec.outstanding().await.expect("outstanding");
        assert_eq!(
            holds.iter().map(|hold| hold.intent_id).collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
    }

    #[test]
    fn a_substituted_bundle_does_not_match_the_commitment() {
        // Clause 6's whole trade: 124 bytes instead of an unbounded blob, in
        // exchange for the one property the intent path needed — the submitter
        // cannot present a friendlier history after the fact.
        let commitment = crate::intent::tests_support::commitment();
        let mut bundle = empty_bundle();
        bundle.window_end = Tick::new(161);
        assert!(!commitment_matches(
            &commitment,
            &bundle,
            orrery_protocol::ChainHash::EMPTY
        ));
        assert!(matches!(
            Finalization::from_verdict(&mismatch_verdict()),
            Finalization::Annul { strike: true }
        ));
    }

    #[test]
    fn the_verdict_table_is_clause_7s() {
        assert_eq!(
            Finalization::from_verdict(&Verdict::Exonerates),
            Finalization::Finalize
        );
        assert_eq!(
            Finalization::from_verdict(&Verdict::Unadjudicable(
                UnadjudicableReason::UnknownRuleset
            )),
            Finalization::Annul { strike: false }
        );
        // `EvidenceForged` strikes here, where the report path would have
        // struck the reporter instead: on this path the evidence's author and
        // the intent's submitter are one account.
        assert!(matches!(
            Finalization::from_verdict(&mismatch_verdict()),
            Finalization::Annul { strike: true }
        ));
    }
}
