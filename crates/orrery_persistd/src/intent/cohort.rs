//! D32 clause (e)'s known-honest cohort, durable: the recorded sample
//! decisions `|H| ≥ 100` is assembled from
//! ([D32](../../../../docs/adr/0032-enforcement-ramp.md)).
//!
//! # Why this exists
//!
//! [`HonestCohort`](super::ramp::HonestCohort) is an in-process set. It has no
//! persistence and no sampling path, so a shadow period cannot outlive the
//! process that counted it — every restart starts from `|H| = 0`, and clause
//! (e)'s `|H| ≥ 100` cannot be assembled across restarts at all.
//!
//! That is not a tooling gap beside the predicate; it is the predicate's
//! decisive term. The question a promotion actually turns on is whether an
//! operator can tell *"this control would have refused 40 honest players"*
//! from *"it would have refused 40 cheats"* — and `HonestCohort`'s
//! armed/natural split is the mechanism that answers it. The split is
//! therefore carried into the durable rows verbatim: each row names its half,
//! and the report renders the two separately, so a promotion reviewer can tell
//! a hundred bots from a hundred players.
//!
//! # Membership is a recorded decision, never an inference
//!
//! D32 clause (e): membership must be *"derivable from durable facts plus a
//! recorded sample decision — never from 'seemed fine'"*. This module stores
//! exactly that pair:
//!
//! - **The recorded sample decision** is the row itself: which half, when,
//!   and the reason the human gave. Written once; resampling an existing
//!   member is refused, and moving an account between halves is the explicit
//!   remove-then-sample path, so history is never rewritten in place.
//! - **The durable facts** are checked at sampling time, per half:
//!   - *Natural* — the account row exists
//!     ([`crate::keyspace::account_key`], whose `created_ms` is D31's durable
//!     account-age fact), the account is past the probation window, and the
//!     account's [`crate::keyspace::strike_key`] span is empty. The strike
//!     ledger is the archive of upheld adverse adjudications; a shadow-stamped
//!     row is still an upheld finding — the mode stamp governs whether it
//!     *punishes*, not whether the finding happened. During the ramp every row
//!     is shadow-stamped, so "zero *live* findings" would sample nobody, which
//!     would defeat the cohort the clause exists to build.
//!   - *Armed* — no fact beyond the decision itself. An armed-honest
//!     account's honesty is a property of the operator's harness, and the
//!     operator attesting *this account is harness-driven* is the fact. A
//!     store that re-derived it would be scoring its own homework.
//!
//! The checks run in the same FoundationDB transaction that writes the row,
//! so a member cannot be sampled and then have its facts drift before the
//! write commits, and a sampled row is always backed by facts that held at
//! commit time.
//!
//! # Why these rows carry no signature
//!
//! A `ramp/{control}` posture row commands the fleet's enforcement, which is
//! why D32 clause (i) authenticates it at the reader. A cohort row commands
//! nothing: it names a member of a measurement population, and the meter's
//! counters — not the roster — are where every figure in the artifact comes
//! from. A forged membership row cannot manufacture a clean `fp_count` (that
//! counts actual would-have-acted events by accounts that really produced
//! traffic), cannot inflate `coverage` (an inactive member has no qualifying
//! activity to add), and cannot promote anything — clause (f)'s asymmetry is
//! untouched by anything this module writes. The residual trust is the sample
//! decision itself, which is the record's own design, not a mechanism this
//! module invents. Possession of the cluster file is therefore the custody
//! boundary here, as it already is for the artifact producer's reads.
//!
//! # Keyspace
//!
//! `rampc/{account}` — the `b"vc"` sub-span of the registered `v` family, per
//! D32 clause (c)'s allocation rule. See
//! [`crate::keyspace::cohort_key`] for the discriminator argument.

use orrery_protocol::AccountId;
use serde::{Deserialize, Serialize};

#[cfg(feature = "fdb")]
use super::ramp::HonestCohort;

/// D32 clause (e)'s probation, as a default rather than a law.
///
/// The record's natural half is "accounts older than the 7-day probation";
/// D33 clause (d) makes the window deployment configuration, so this is a
/// dial ([`FdbHonestCohortStore::with_probation_ms`]) with the documented
/// value as its default. A deployment that dials identity's probation
/// elsewhere should dial this to match: the sampler verifies the durable fact
/// against the window it was given.
pub const DEFAULT_PROBATION_MS: u64 = 7 * 24 * 60 * 60 * 1_000;

/// The longest `reason` a membership row carries.
///
/// [`super::ramp::RampPosture`]'s reason documents the same 256-byte writer
/// bound, for the same reason: a bounded free-text field cannot become a
/// storage amplifier on the one field an operator controls.
pub const MAX_COHORT_REASON_BYTES: usize = 256;

/// Which of D32 clause (e)'s two halves a member belongs to.
///
/// The split is the point of the whole cohort. *Armed* accounts are
/// operator-driven automation; *natural* accounts are real players past
/// probation with a clean archive, sampled in by a human. A promotion
/// reviewer reading `|H| = 100` needs to know which hundred they are looking
/// at, so the half is stored per member and never merged away.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CohortHalf {
    /// Operator-controlled accounts acting honestly under automation.
    Armed,
    /// Accounts past probation with a clean archive, sampled in by a human.
    Natural,
}

impl CohortHalf {
    /// The stable label, for logs and the sampling tool's output.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Armed => "armed",
            Self::Natural => "natural",
        }
    }
}

/// One recorded sample decision, the value stored at `rampc/{account}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CohortMemberRow {
    /// Which half the account was sampled into.
    pub half: CohortHalf,
    /// Unix milliseconds at which the decision was recorded.
    pub decided_at_ms: u64,
    /// The human's reason, bounded by [`MAX_COHORT_REASON_BYTES`].
    pub reason: String,
}

/// Why a sample was refused.
///
/// Every variant is a finding an operator can act on, and none of them is a
/// silent success: a cohort assembled from rows that half-failed their checks
/// would be exactly the "seemed fine" the record refuses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SampleRefusal {
    /// The account already has a membership row.
    ///
    /// One recorded decision per account. To move an account between halves,
    /// remove the row first — the explicit operator act — and sample again.
    AlreadySampled {
        /// The account.
        account: AccountId,
        /// The half the existing row recorded.
        half: CohortHalf,
    },
    /// No identity account row exists, so the probation fact cannot be
    /// verified. Sampling the account anyway would replace a durable fact
    /// with an assumption.
    AccountUnknown {
        /// The account.
        account: AccountId,
    },
    /// The account row's `created_ms` is inside the probation window.
    OnProbation {
        /// The account.
        account: AccountId,
        /// When the account was created, from the account row.
        created_ms: u64,
        /// The window this store verified against.
        probation_ms: u64,
        /// The clock the check ran with.
        now_ms: u64,
    },
    /// The account's strike-ledger span is non-empty: an upheld adverse
    /// adjudication exists, whatever its mode stamp.
    AdverseFinding {
        /// The account.
        account: AccountId,
    },
    /// The reason exceeds [`MAX_COHORT_REASON_BYTES`].
    ReasonTooLong(usize),
}

impl std::fmt::Display for SampleRefusal {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AlreadySampled { account, half } => write!(
                formatter,
                "account {} already has a membership row (half: {}); \
                 remove it first to record a different decision",
                account.0,
                half.as_str()
            ),
            Self::AccountUnknown { account } => write!(
                formatter,
                "account {} has no identity row, so its probation fact \
                 cannot be verified; sampling it would replace a durable fact \
                 with an assumption",
                account.0
            ),
            Self::OnProbation {
                account,
                created_ms,
                probation_ms,
                now_ms,
            } => write!(
                formatter,
                "account {} is on probation: created at {created_ms}, \
                 which is inside the {probation_ms} ms window as of {now_ms}",
                account.0
            ),
            Self::AdverseFinding { account } => write!(
                formatter,
                "account {} has an upheld adverse finding in its \
                 strike-ledger span; the natural half requires zero",
                account.0
            ),
            Self::ReasonTooLong(len) => write!(
                formatter,
                "the sample reason is {len} bytes; the writer bound is \
                 {MAX_COHORT_REASON_BYTES}",
            ),
        }
    }
}

impl std::error::Error for SampleRefusal {}

/// Failure reading or writing the durable cohort.
#[derive(Debug)]
pub enum CohortError {
    /// The sampler refused this sample, with the reason.
    Refused(SampleRefusal),
    /// A FoundationDB transaction failed, or a row did not decode.
    Store(String),
}

impl std::fmt::Display for CohortError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Refused(refusal) => write!(formatter, "sample refused: {refusal}"),
            Self::Store(what) => write!(formatter, "cohort store: {what}"),
        }
    }
}

impl std::error::Error for CohortError {}

/// Durable home of the known-honest cohort: one `rampc/{account}` row per
/// member, checked and written in one transaction.
///
/// Reads through [`Self::load`] rebuild the in-process
/// [`HonestCohort`](super::ramp::HonestCohort) an artifact producer hands to
/// [`RampMeter::snapshot`](super::ramp::RampMeter::snapshot), so `|H|` is
/// assembled from every decision ever recorded rather than from whatever one
/// process happens to remember.
#[cfg(feature = "fdb")]
pub struct FdbHonestCohortStore {
    db: std::sync::Arc<foundationdb::Database>,
    probation_ms: u64,
}

#[cfg(feature = "fdb")]
impl FdbHonestCohortStore {
    /// Construct from the process-scoped FDB context, with D32's documented
    /// probation window.
    #[must_use]
    pub fn from_context(context: &crate::FdbContext) -> Self {
        Self {
            db: context.database(),
            probation_ms: DEFAULT_PROBATION_MS,
        }
    }

    /// Verify the natural half against this probation window instead of the
    /// documented default. See [`DEFAULT_PROBATION_MS`] for why this is a
    /// dial.
    #[must_use]
    pub fn with_probation_ms(mut self, probation_ms: u64) -> Self {
        self.probation_ms = probation_ms;
        self
    }

    /// The window the natural half is verified against.
    #[must_use]
    pub const fn probation_ms(&self) -> u64 {
        self.probation_ms
    }

    /// Record one sample decision, checking the natural half's durable facts
    /// in the same transaction that writes the row.
    ///
    /// `now_ms` is injected rather than read here, so a test can state the
    /// clock it is claiming facts about; the sampling tool passes wall time.
    ///
    /// # Errors
    ///
    /// [`CohortError::Refused`] for every [`SampleRefusal`];
    /// [`CohortError::Store`] for transaction or decode failures.
    pub async fn sample(
        &self,
        account: AccountId,
        half: CohortHalf,
        reason: &str,
        decided_at_ms: u64,
        now_ms: u64,
    ) -> Result<(), CohortError> {
        if reason.len() > MAX_COHORT_REASON_BYTES {
            return Err(CohortError::Refused(SampleRefusal::ReasonTooLong(
                reason.len(),
            )));
        }
        let row = CohortMemberRow {
            half,
            decided_at_ms,
            reason: reason.to_owned(),
        };
        let probation_ms = self.probation_ms;
        let db = std::sync::Arc::clone(&self.db);
        // The transaction's failure type is `FdbBindingError`, so the
        // domain-level refusal rides out as the inner `Ok` half and the
        // flattening happens below, where it can be typed.
        db.run(move |trx, _| {
            let row = row.clone();
            async move {
                // The refusal names the half the *stored* row is in, not the
                // one being asked for. An operator resampling an armed
                // account as natural is told which half it is already in —
                // the only fact that tells them whether to remove-and-resample
                // or to leave it alone. Reporting the requested half here
                // would answer their question with their own question, in the
                // one module whose entire purpose is keeping the two halves
                // distinguishable.
                if let Some(existing) = trx
                    .get(&crate::keyspace::cohort_key(account), false)
                    .await
                    .map_err(store_err("read cohort row"))?
                {
                    let existing: CohortMemberRow =
                        postcard::from_bytes(&existing).map_err(store_err("decode cohort row"))?;
                    return Ok(Err(SampleRefusal::AlreadySampled {
                        account,
                        half: existing.half,
                    }));
                }
                if matches!(half, CohortHalf::Natural) {
                    let account_bytes = trx
                        .get(crate::keyspace::account_key(account).as_ref(), false)
                        .await
                        .map_err(store_err("read account row"))?;
                    let Some(account_bytes) = account_bytes else {
                        return Ok(Err(SampleRefusal::AccountUnknown { account }));
                    };
                    let account_row: crate::keyspace::AccountRow =
                        postcard::from_bytes(&account_bytes)
                            .map_err(store_err("decode account row"))?;
                    if now_ms.saturating_sub(account_row.created_ms) < probation_ms {
                        return Ok(Err(SampleRefusal::OnProbation {
                            account,
                            created_ms: account_row.created_ms,
                            probation_ms,
                            now_ms,
                        }));
                    }
                    use futures::TryStreamExt;
                    let strikes_start = crate::keyspace::strike_account_range_start(account);
                    let strikes_end = crate::keyspace::strike_account_range_end(account);
                    let mut findings = trx.get_ranges_keyvalues(
                        foundationdb::RangeOption {
                            begin: foundationdb::KeySelector::first_greater_or_equal(
                                &strikes_start,
                            ),
                            end: foundationdb::KeySelector::first_greater_or_equal(&strikes_end),
                            limit: Some(1),
                            ..foundationdb::RangeOption::default()
                        },
                        false,
                    );
                    if findings
                        .try_next()
                        .await
                        .map_err(store_err("scan strikes"))?
                        .is_some()
                    {
                        return Ok(Err(SampleRefusal::AdverseFinding { account }));
                    }
                }
                let encoded =
                    postcard::to_allocvec(&row).map_err(store_err("encode cohort row"))?;
                trx.set(&crate::keyspace::cohort_key(account), &encoded);
                Ok(Ok(()))
            }
        })
        .await
        .map_err(|error: foundationdb::FdbBindingError| {
            CohortError::Store(format!("sample cohort transaction: {error}"))
        })?
        .map_err(CohortError::Refused)
    }

    /// One account's recorded decision, if it has one.
    ///
    /// # Errors
    ///
    /// [`CohortError::Store`] for transaction or decode failures.
    pub async fn read(&self, account: AccountId) -> Result<Option<CohortMemberRow>, CohortError> {
        let db = std::sync::Arc::clone(&self.db);
        let value: Option<Vec<u8>> = db
            .run(move |trx, _| async move {
                Ok(trx
                    .get(&crate::keyspace::cohort_key(account), false)
                    .await
                    .map_err(store_err("read cohort row"))?
                    .map(|bytes| bytes.as_ref().to_vec()))
            })
            .await
            .map_err(|error: foundationdb::FdbBindingError| {
                CohortError::Store(format!("read cohort transaction: {error}"))
            })?;
        value
            .map(|bytes| {
                postcard::from_bytes(&bytes)
                    .map_err(|error| CohortError::Store(format!("decode cohort row: {error}")))
            })
            .transpose()
    }

    /// Remove one account's membership row.
    ///
    /// An operator correction, not a membership property: the reason trail
    /// for a removal lives in the tool that performed it, and D32 open
    /// question 2's journal shadow is where append-only history eventually
    /// lands.
    ///
    /// # Errors
    ///
    /// [`CohortError::Store`] for transaction failures.
    pub async fn remove(&self, account: AccountId) -> Result<(), CohortError> {
        let db = std::sync::Arc::clone(&self.db);
        db.run(move |trx, _| async move {
            trx.clear(&crate::keyspace::cohort_key(account));
            Ok(())
        })
        .await
        .map_err(|error: foundationdb::FdbBindingError| {
            CohortError::Store(format!("remove cohort transaction: {error}"))
        })
    }

    /// Every recorded decision, in account order.
    ///
    /// # Errors
    ///
    /// [`CohortError::Store`] for transaction or decode failures.
    pub async fn members(&self) -> Result<Vec<(AccountId, CohortMemberRow)>, CohortError> {
        use futures::TryStreamExt;

        let db = std::sync::Arc::clone(&self.db);
        db.run(|trx, _| async move {
            let cohort_start = crate::keyspace::cohort_range_start();
            let cohort_end = crate::keyspace::cohort_range_end();
            let mut stream = trx.get_ranges_keyvalues(
                foundationdb::RangeOption {
                    begin: foundationdb::KeySelector::first_greater_or_equal(&cohort_start),
                    end: foundationdb::KeySelector::first_greater_or_equal(&cohort_end),
                    ..foundationdb::RangeOption::default()
                },
                false,
            );
            let mut members = Vec::new();
            while let Some(kv) = stream.try_next().await.map_err(store_err("scan cohort"))? {
                let key = kv.key();
                if key.len() != 10 {
                    return Err(store_err("cohort key width")(
                        "key does not carry an 8-byte account id",
                    ));
                }
                let account = AccountId::new(u64::from_be_bytes(
                    key[2..10].try_into().expect("checked 8 bytes above"),
                ));
                let row: CohortMemberRow =
                    postcard::from_bytes(kv.value()).map_err(store_err("decode cohort row"))?;
                members.push((account, row));
            }
            Ok(members)
        })
        .await
        .map_err(|error: foundationdb::FdbBindingError| {
            CohortError::Store(format!("scan cohort transaction: {error}"))
        })
    }

    /// Rebuild the in-process cohort from every recorded decision.
    ///
    /// The artifact producer's entry point: the halves land in
    /// [`HonestCohort`]'s own sets, so
    /// [`RampMeter::snapshot`](super::ramp::RampMeter::snapshot) reports the
    /// armed/natural split exactly as clause (e) requires it reported.
    ///
    /// # Errors
    ///
    /// [`CohortError::Store`] for transaction or decode failures.
    pub async fn load(&self) -> Result<HonestCohort, CohortError> {
        let mut cohort = HonestCohort::new();
        for (account, row) in self.members().await? {
            match row.half {
                CohortHalf::Armed => cohort.arm(account),
                CohortHalf::Natural => cohort.sample(account),
            }
        }
        Ok(cohort)
    }
}

/// Lift a step description into the mapper every `db.run` closure needs at
/// each fallible step: a transaction's failure type is
/// [`foundationdb::FdbBindingError`], and a custom error keeps the step name
/// on the way out.
#[cfg(feature = "fdb")]
fn store_err<E: std::fmt::Display>(
    what: &'static str,
) -> impl Fn(E) -> foundationdb::FdbBindingError {
    move |error| {
        foundationdb::FdbBindingError::new_custom_error(Box::new(CohortStepError(format!(
            "{what}: {error}"
        ))))
    }
}

/// The error wrapper `new_custom_error` needs: a named step that failed
/// inside a transaction, carrying the underlying reason.
#[cfg(feature = "fdb")]
#[derive(Debug)]
struct CohortStepError(String);

#[cfg(feature = "fdb")]
impl std::fmt::Display for CohortStepError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[cfg(feature = "fdb")]
impl std::error::Error for CohortStepError {}

#[cfg(all(test, feature = "fdb"))]
mod tests {
    use super::*;

    /// The dev-cluster convention `intent/fdb.rs` established: skip without a
    /// cluster, and let `scripts/fdb-tests.sh` turn the skip into a gate.
    fn fdb_cluster_file() -> Option<String> {
        if let Ok(path) = std::env::var("ORRERY_FDB_CLUSTER_FILE") {
            return Some(path);
        }
        let local = std::path::Path::new(".fdb-dev/fdb.cluster");
        local.exists().then(|| local.display().to_string())
    }

    fn store() -> FdbHonestCohortStore {
        let cluster = fdb_cluster_file().expect("cluster file for cohort tests");
        let context = crate::FdbContext::connect(&cluster).expect("connect");
        FdbHonestCohortStore::from_context(&context)
    }

    fn account(id: u64) -> AccountId {
        AccountId::new(id)
    }

    /// Write raw bytes at one `ya` key for `member`, in the same
    /// versionstamped shape the ledger's own filing uses. The sampler checks
    /// *existence* in the span — an upheld finding — not the row's contents,
    /// so the probe needs no evidence, reporter or ruleset.
    async fn seed_strike_probe(member: AccountId) {
        let db = db();
        let param = crate::keyspace::strike_versionstamped_key(member);
        db.run(move |trx, _| async move {
            trx.atomic_op(
                &param,
                b"cohort probe",
                foundationdb::options::MutationType::SetVersionstampedKey,
            );
            Ok(())
        })
        .await
        .expect("seed strike probe");
    }

    async fn clear_strike_span(member: AccountId) {
        use futures::TryStreamExt;
        let db = db();
        let start = crate::keyspace::strike_account_range_start(member);
        let end = crate::keyspace::strike_account_range_end(member);
        db.run(move |trx, _| {
            let (start, end) = (start.clone(), end.clone());
            async move {
                let mut stream = trx.get_ranges_keyvalues(
                    foundationdb::RangeOption {
                        begin: foundationdb::KeySelector::first_greater_or_equal(start.as_slice()),
                        end: foundationdb::KeySelector::first_greater_or_equal(end.as_slice()),
                        ..foundationdb::RangeOption::default()
                    },
                    false,
                );
                let mut doomed = Vec::new();
                while let Some(kv) = stream.try_next().await? {
                    doomed.push(kv.key().to_vec());
                }
                for key in doomed {
                    trx.clear(&key);
                }
                Ok(())
            }
        })
        .await
        .expect("clear strike span");
    }

    fn db() -> std::sync::Arc<foundationdb::Database> {
        crate::FdbContext::connect(&fdb_cluster_file().expect("cluster"))
            .expect("connect")
            .database()
    }

    /// `load()` rebuilds both halves from the rows alone, so `|H|` survives
    /// the process that took the decisions — clause (e)'s assembly across
    /// restarts, which the in-process cohort could not do at all.
    #[tokio::test]
    async fn the_durable_cohort_rebuilds_both_halves_across_a_store() {
        let store = store();
        let id = u64::from_be_bytes(*b"cohrt001");
        store.remove(account(id)).await.expect("clean slate");
        store
            .remove(account(id + 0x100))
            .await
            .expect("clean slate");

        // The natural half's fact comes from an identity account row; seed
        // one the way identity would have, aged past the window.
        let db = db();
        let key = crate::keyspace::account_key(account(id + 0x100)).to_vec();
        let value = postcard::to_allocvec(&crate::keyspace::AccountRow {
            created_ms: 0,
            ..crate::keyspace::AccountRow::default()
        })
        .expect("encode");
        db.run(move |trx, _| {
            let (key, value) = (key.clone(), value.clone());
            async move {
                trx.set(&key, &value);
                Ok(())
            }
        })
        .await
        .expect("seed account row");

        store
            .sample(
                account(id),
                CohortHalf::Armed,
                "p1 swarm harness operator #3",
                1_000,
                1_000,
            )
            .await
            .expect("armed sampling records the operator's decision");
        store
            .sample(
                account(id + 0x100),
                CohortHalf::Natural,
                "handed a day pass by the owner",
                2_000,
                // The seeded account row was created at 0, so the first
                // instant past the window is the fact the check needs.
                store.probation_ms() + 1,
            )
            .await
            .expect("natural sampling records the human's decision");

        let loaded = store.load().await.expect("rebuild");
        // Per-account assertions, not span totals: the dev cluster is shared,
        // and another test's (or lane's) row may be in the span while this
        // load runs. What this test owns is where ITS accounts landed.
        assert!(
            loaded.armed.contains(&account(id)),
            "the armed decision rebuilt into the armed half"
        );
        assert!(
            loaded.natural.contains(&account(id + 0x100)),
            "the natural decision rebuilt into the natural half"
        );
        assert!(
            !loaded.natural.contains(&account(id)) && !loaded.armed.contains(&account(id + 0x100)),
            "neither half absorbed the other's member"
        );
        assert!(loaded.contains(account(id)) && loaded.contains(account(id + 0x100)));
        assert!(loaded.len() >= 2, "both members are in the union");

        let row = store.read(account(id)).await.expect("read").expect("row");
        assert_eq!(row.half, CohortHalf::Armed);
        assert_eq!(row.decided_at_ms, 1_000);
        assert_eq!(row.reason, "p1 swarm harness operator #3");

        db.run(move |trx, _| async move {
            trx.clear(crate::keyspace::account_key(account(id + 0x100)).as_ref());
            Ok(())
        })
        .await
        .expect("cleanup account row");
        store.remove(account(id)).await.expect("cleanup");
        store.remove(account(id + 0x100)).await.expect("cleanup");
        let emptied = store.load().await.expect("rebuild");
        assert!(
            !emptied.contains(account(id)) && !emptied.contains(account(id + 0x100)),
            "removal is real, so a test cohort cannot leak into an artifact"
        );
    }

    /// A second decision for an account that already has one is refused:
    /// history is never rewritten in place, and moving between halves is the
    /// explicit remove-then-sample path.
    #[tokio::test]
    async fn resampling_a_member_is_refused_rather_than_rewritten() {
        let store = store();
        let id = u64::from_be_bytes(*b"cohrt002");
        store.remove(account(id)).await.expect("clean slate");
        store
            .sample(
                account(id),
                CohortHalf::Armed,
                "first decision",
                1_000,
                1_000,
            )
            .await
            .expect("first decision records");

        // Resampled into the *other* half, so the assertion below can tell
        // "the row it found" from "the half you asked for". Resampling armed
        // as armed cannot: both answers are `Armed`, and a refusal that
        // echoed the request back would pass it.
        let refused = store
            .sample(
                account(id),
                CohortHalf::Natural,
                "second decision",
                2_000,
                2_000,
            )
            .await
            .expect_err("one recorded decision per account");
        match refused {
            CohortError::Refused(SampleRefusal::AlreadySampled { half, .. }) => {
                assert_eq!(
                    half,
                    CohortHalf::Armed,
                    "the refusal names the half of the row it found, not the \
                     half that was asked for"
                );
            }
            // The already-sampled check runs before the natural half's fact
            // checks, so an account with no identity row still refuses as
            // `AlreadySampled` rather than `AccountUnknown`. That ordering is
            // the operator-legible one: "it is already in the armed half"
            // answers their question; "no account row" does not.
            other => panic!("expected AlreadySampled, got {other:?}"),
        }
        let row = store.read(account(id)).await.expect("read").expect("row");
        assert_eq!(row.reason, "first decision", "the first decision stands");

        store.remove(account(id)).await.expect("cleanup");
    }

    /// The natural half's facts are checked against the account row: an
    /// account identity never issued cannot have a verifiable probation.
    #[tokio::test]
    async fn an_unverifiable_account_is_refused_rather_than_assumed() {
        let store = store();
        // An id identity has never issued: no `da` row can exist for it.
        let id = u64::from_be_bytes(*b"\0\0\0\0n0pe");
        store.remove(account(id)).await.expect("clean slate");

        let refused = store
            .sample(
                account(id),
                CohortHalf::Natural,
                "no fact behind this",
                1_000,
                1_000,
            )
            .await
            .expect_err("the probation fact must be verifiable");
        assert!(
            matches!(
                refused,
                CohortError::Refused(SampleRefusal::AccountUnknown { .. })
            ),
            "expected AccountUnknown, got {refused:?}"
        );
        assert!(
            store.read(account(id)).await.expect("read").is_none(),
            "a refused sample writes nothing"
        );
    }

    /// An account with an upheld adverse finding — shadow-stamped, as every
    /// row is during the ramp — is not natural-honest. The mode stamp governs
    /// enforcement, not the finding.
    #[tokio::test]
    async fn a_strike_in_the_span_is_still_an_upheld_finding() {
        let store = store();
        let id = u64::from_be_bytes(*b"cohrt003");
        let member = account(id);
        store.remove(member).await.expect("clean slate");
        clear_strike_span(member).await;

        // An aged identity row, so probation passes and the check that fires
        // is the finding's, not the account row's absence.
        let db = db();
        let key = crate::keyspace::account_key(member).to_vec();
        let value = postcard::to_allocvec(&crate::keyspace::AccountRow {
            created_ms: 0,
            ..crate::keyspace::AccountRow::default()
        })
        .expect("encode");
        db.run(move |trx, _| {
            let (key, value) = (key.clone(), value.clone());
            async move {
                trx.set(&key, &value);
                Ok(())
            }
        })
        .await
        .expect("seed account row");
        seed_strike_probe(member).await;

        let refused = store
            .sample(
                member,
                CohortHalf::Natural,
                "clean per the dashboard",
                2_000,
                store.probation_ms() + 1,
            )
            .await
            .expect_err("an upheld finding bars the natural half");
        assert!(
            matches!(
                refused,
                CohortError::Refused(SampleRefusal::AdverseFinding { .. })
            ),
            "expected AdverseFinding, got {refused:?}"
        );

        clear_strike_span(member).await;
        db.run(move |trx, _| async move {
            trx.clear(crate::keyspace::account_key(member).as_ref());
            Ok(())
        })
        .await
        .expect("cleanup account row");
        store.remove(member).await.expect("cleanup");
    }

    /// A natural sample past probation succeeds against a real account row,
    /// and one inside the window is refused with the numbers attached —
    /// the durable fact doing exactly its job.
    #[tokio::test]
    async fn probation_is_checked_from_the_account_row() {
        let store = store();
        let id = u64::from_be_bytes(*b"cohrt004");
        let member = account(id);

        // The identity service owns account rows; this store only reads them.
        // Write one directly so the fact being checked is the row's own.
        let db = db();
        let created_ms = 1_000;
        let row = crate::keyspace::AccountRow {
            created_ms,
            ..crate::keyspace::AccountRow::default()
        };
        let key = crate::keyspace::account_key(member).to_vec();
        let value = postcard::to_allocvec(&row).expect("encode");
        db.run(move |trx, _| {
            let (key, value) = (key.clone(), value.clone());
            async move {
                trx.set(&key, &value);
                Ok(())
            }
        })
        .await
        .expect("seed account row");

        // One millisecond inside the window: refused, with the numbers.
        let refused = store
            .sample(
                member,
                CohortHalf::Natural,
                "too fresh",
                3_000,
                created_ms + store.probation_ms() - 1,
            )
            .await
            .expect_err("inside the window");
        match refused {
            CohortError::Refused(SampleRefusal::OnProbation {
                created_ms: found,
                probation_ms,
                ..
            }) => {
                assert_eq!(found, created_ms);
                assert_eq!(probation_ms, DEFAULT_PROBATION_MS);
            }
            other => panic!("expected OnProbation, got {other:?}"),
        }

        // One millisecond past it: recorded.
        store
            .sample(
                member,
                CohortHalf::Natural,
                "aged out of probation",
                3_000,
                created_ms + store.probation_ms(),
            )
            .await
            .expect("past the window the fact holds");
        let recorded = store.read(member).await.expect("read").expect("row");
        assert_eq!(recorded.half, CohortHalf::Natural);

        db.run(move |trx, _| async move {
            trx.clear(crate::keyspace::account_key(member).as_ref());
            Ok(())
        })
        .await
        .expect("cleanup account row");
        store.remove(member).await.expect("cleanup");
    }

    /// The armed half records the operator's decision without fact checks:
    /// the harness attestation *is* the fact, and a store that re-derived it
    /// would be scoring its own homework.
    #[tokio::test]
    async fn the_armed_half_needs_no_verifiable_fact_beyond_the_decision() {
        let store = store();
        let id = u64::from_be_bytes(*b"cohrt005");
        let member = account(id);
        store.remove(member).await.expect("clean slate");

        // No account row exists and no probation clock is consulted; the
        // decision records anyway.
        store
            .sample(member, CohortHalf::Armed, "harness-driven", 1_000, 1_000)
            .await
            .expect("armed sampling is the operator attesting");
        let recorded = store.read(member).await.expect("read").expect("row");
        assert_eq!(recorded.half, CohortHalf::Armed);

        store.remove(member).await.expect("cleanup");
    }

    /// An over-long reason is refused before any transaction runs: the
    /// free-text field is bounded at the writer, like the posture row's.
    #[tokio::test]
    async fn an_over_long_reason_is_refused_at_the_writer() {
        let store = store();
        let refused = store
            .sample(
                account(1),
                CohortHalf::Armed,
                &"x".repeat(MAX_COHORT_REASON_BYTES + 1),
                1_000,
                1_000,
            )
            .await
            .expect_err("the writer bound is enforced");
        assert!(matches!(
            refused,
            CohortError::Refused(SampleRefusal::ReasonTooLong(_))
        ));
    }
}
