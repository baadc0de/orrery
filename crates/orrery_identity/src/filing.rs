//! D33 clause (e)'s *filing-driven* half: evaluate standing after every live
//! filing, not only when the account next mints.
//!
//! # The gap this closes
//!
//! Clause (e) says standing is evaluated "after every live filing and whenever
//! identity mints or refreshes a token". [`crate::cooldown`] delivers the
//! second half: `apply_dwell` writes the durable `dc` entry that
//! [`crate::invalidation`] publishes, and it runs on the mint path. So an
//! account that crossed `C` and then simply stopped logging in produced no
//! `dc` row, and therefore no [`orrery_protocol::AccountInvalidation`] — its
//! outstanding tokens ran to their signed TTLs with nobody refusing them. The
//! first half of the clause had no mechanism at all.
//!
//! This module is that mechanism. The executor writes a `yd` filing notice in
//! the same FoundationDB transaction as the `ya` strike row
//! ([`orrery_persistd::keyspace::filing_notice_key`]); this reactor drains that
//! queue, re-scores each named account through the ordinary scorer, and records
//! the `dc` entry when the account is at or above `C`.
//!
//! # Why a durable queue rather than a call
//!
//! `orrery_identity` depends on `orrery_persistd` and never the reverse, so the
//! executor cannot notify identity in-process without closing a dependency
//! cycle. A row the executor writes and identity reads is the only direction
//! the graph allows. It is also the more honest shape for a fleet: the notice
//! survives an identity replica being down, and any replica may drain it.
//!
//! # What this reactor may and may not do to a watermark
//!
//! [`orrery_protocol::AccountInvalidation::effective_from_ms`] is the instant
//! the refusal began, and the whole point of reading `dc` rather than
//! re-scoring `ya` at poll time is that a watermark must never drift later than
//! that instant — a late watermark kills tokens the account legitimately held.
//!
//! This reactor therefore calls exactly one store mutation,
//! [`AccountStore::observe_cooldown`], with exactly the arguments
//! `apply_dwell` passes it. That gives it three properties by construction:
//!
//! - it can **create** an entry, stamped at the reactor's own read instant;
//! - it can **restart** one only under the identical rule the mint path uses,
//!   namely a live strike newer than the standing entry;
//! - it can **never retract** one. It does not call
//!   [`AccountStore::clear_cooldown_if`] and holds no release path at all.
//!   Release stays where #884 put it: on the mint path, behind the dwell floor.
//!
//! One consequence is deliberate and worth stating plainly. With this reactor
//! running, an account's `dc` entry is stamped at the *crossing* rather than at
//! its next login, so the [`crate::standing::StandingThresholds::cooldown_min_ms`]
//! dwell now runs from the crossing. That is what `entered_at_ms` has always
//! been documented to mean ("the instant at which identity entered cooldown"),
//! and it strictly increases enforcement: over the absence window the account
//! is now refused by the invalidation feed, where before it was refused by
//! nothing at all.
//!
//! # Posture
//!
//! This is a second enforcement point, so it reads D32 control C5's posture on
//! **every** account, not once at startup — the bug #934 found in the
//! coordinator's session-termination arm. The posture cell and its durable
//! poller are `orrery_persistd`'s
//! ([`orrery_persistd::gateway::StrikesPosture`],
//! [`orrery_persistd::gateway::spawn_strikes_posture_poller`]) rather than a
//! fourth copy of the same three-state enum, because this crate already depends
//! on that one.
//!
//! - `Off` — the queue is not read and not drained. Notices accumulate, so
//!   promoting the control later still acts on everything filed while it was
//!   off. D32 clause (b): "Off observes nothing".
//! - `Shadow` — the full predicate runs on every notice and the would-be entry
//!   is recorded on [`STRIKES_SHADOW_TARGET`], but no `dc` row is written and
//!   **the notice is not cleared**, so a promotion to `Live` still acts on it.
//! - `Live` — the `dc` entry is written and the notice is cleared.
//!
//! Shadow-stamped `ya` rows change no standing on this path either, and not by
//! a branch here: the executor writes a notice for every filing because the
//! ledger has no mode branch, and the scorer at [`crate::standing`] is what
//! ignores a shadow row. A shadow filing queues an evaluation that finds
//! nothing.

use crate::standing::{ComputedStanding, StandingLevel, StrikeRowSource};
use crate::store::{AccountStore, IdentityError};
use async_trait::async_trait;
use orrery_persistd::gateway::{StrikesEnforcement, StrikesPosture};
use orrery_protocol::AccountId;

/// The tracing target this reactor's shadow observations are emitted on.
///
/// The same literal the coordinator and the gateway use, so one filter catches
/// every control's would-be actions — and, for the same reason those two are
/// copies of each other, a copied literal: no crate these three share owns
/// enforcement vocabulary yet.
pub const STRIKES_SHADOW_TARGET: &str = "orrery::ramp::shadow";

/// One pending "this account was filed against" notice.
///
/// A named pair rather than a tuple: it crosses a public trait boundary, and
/// which of the two fields is the account must not be positional.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FilingNotice {
    /// The account the executor resolved the strike to.
    pub account: AccountId,
    /// The filing instant the notice was stamped with.
    ///
    /// Carried only so the notice can be cleared compare-and-swap: a filing
    /// that lands while this reactor is evaluating must survive the clear.
    /// It is **not** a watermark and is never written into a `dc` entry.
    pub filed_at_ms: u64,
}

/// The executor-written filing queue, as identity drains it.
#[async_trait]
pub trait FilingNoticeQueue: Send + Sync {
    /// Every notice awaiting evaluation.
    ///
    /// One range read for the family, not one per account: the queue is keyed
    /// by account precisely so its cardinality is the accounts awaiting
    /// evaluation rather than the filings ever made.
    ///
    /// # Errors
    ///
    /// Propagates the store failure unchanged. An empty successful read and a
    /// failed read are different facts and only the first means "nothing to do".
    async fn pending(&self) -> Result<Vec<FilingNotice>, IdentityError>;

    /// Remove a notice, but only while it is still the one that was read.
    ///
    /// Returns `false` when a filing landed in the meantime and overwrote it;
    /// the caller leaves it for the next sweep rather than dropping a filing
    /// nothing has evaluated.
    ///
    /// # Errors
    ///
    /// Propagates the store failure unchanged.
    async fn clear_if(&self, notice: FilingNotice) -> Result<bool, IdentityError>;
}

/// What one sweep of the filing queue did.
///
/// Reported rather than logged-and-forgotten so a deployment can tell an idle
/// reactor from a reactor whose posture is `Off`, and a shadow rollout can
/// count the refusals it would have caused before anyone arms it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FilingSweep {
    /// The posture the sweep ran under.
    pub mode: StrikesEnforcement,
    /// Notices read from the queue.
    pub seen: usize,
    /// Accounts successfully re-scored.
    pub evaluated: usize,
    /// Accounts whose `dc` entry this sweep wrote (`Live` only).
    pub published: usize,
    /// Accounts a `Live` sweep would have published (`Shadow` only).
    pub would_publish: usize,
    /// Notices cleared from the queue (`Live` only).
    pub cleared: usize,
    /// Accounts whose evaluation failed and were left queued.
    pub failed: usize,
}

/// D33 clause (e)'s filing-driven evaluator.
///
/// Holds the store, the queue and the scorer by value so a caller may pass
/// owned or shared handles: [`AccountStore`] is implemented for `Arc<T>`, which
/// is how one store backs both the login path and this reactor.
pub struct StandingFilingReactor<S, Q, R, C> {
    store: S,
    queue: Q,
    scorer: ComputedStanding<R, C>,
    posture: StrikesPosture,
}

impl<S, Q, R, C> StandingFilingReactor<S, Q, R, C> {
    /// Assemble a reactor over one store, one queue and one scorer.
    #[must_use]
    pub const fn new(
        store: S,
        queue: Q,
        scorer: ComputedStanding<R, C>,
        posture: StrikesPosture,
    ) -> Self {
        Self {
            store,
            queue,
            scorer,
            posture,
        }
    }

    /// The posture cell this reactor reads. The operator's lever.
    #[must_use]
    pub const fn posture(&self) -> &StrikesPosture {
        &self.posture
    }
}

impl<S, Q, R, C> StandingFilingReactor<S, Q, R, C>
where
    S: AccountStore,
    Q: FilingNoticeQueue,
    R: StrikeRowSource,
    C: Fn() -> u64,
{
    /// Drain and evaluate the filing queue once.
    ///
    /// One account's failure does not abort the sweep: an executor may file
    /// against a binding whose account row identity cannot read, and taking the
    /// whole reactor down over one such row would stop every *other* account's
    /// invalidation from ever being published. A failed account keeps its
    /// notice and is retried on the next sweep.
    ///
    /// # Errors
    ///
    /// Only the queue read itself. A failed read is surfaced rather than
    /// reported as an empty sweep, for the reason [`FilingNoticeQueue::pending`]
    /// gives.
    pub async fn sweep(&self) -> Result<FilingSweep, IdentityError> {
        // Read once at the top *and* again per account below: a posture that
        // demotes mid-sweep must take effect within the sweep, not after it.
        let mut outcome = FilingSweep {
            mode: self.posture.get(),
            ..FilingSweep::default()
        };
        if outcome.mode == StrikesEnforcement::Off {
            return Ok(outcome);
        }

        let notices = self.queue.pending().await?;
        outcome.seen = notices.len();
        for notice in notices {
            // D32 clause (f)'s auto-suspend can demote this control while the
            // sweep runs. Re-reading here is what makes a demotion take effect
            // within one poll interval rather than one whole queue.
            let mode = self.posture.get();
            if mode == StrikesEnforcement::Off {
                outcome.mode = mode;
                break;
            }

            let observation = match self.scorer.observe(notice.account).await {
                Ok(observation) => observation,
                Err(error) => {
                    outcome.failed += 1;
                    tracing::warn!(
                        account = notice.account.0,
                        %error,
                        "could not score a filed account; leaving its notice queued"
                    );
                    continue;
                }
            };
            outcome.evaluated += 1;

            let refused = matches!(
                observation.level,
                StandingLevel::Cooldown | StandingLevel::Banned
            );

            match mode {
                // Handled above and re-checked at the top of the loop.
                StrikesEnforcement::Off => unreachable!("checked before this match"),
                StrikesEnforcement::Shadow => {
                    if refused {
                        outcome.would_publish += 1;
                        tracing::info!(
                            target: STRIKES_SHADOW_TARGET,
                            control = "strikes",
                            account = notice.account.0,
                            level = ?observation.level,
                            "would publish an account invalidation for a filed account"
                        );
                    }
                    // Deliberately no clear: the notice must survive until a
                    // posture that actually acts on it has done so.
                }
                StrikesEnforcement::Live => {
                    if refused {
                        // The only mutation this reactor makes, with exactly
                        // the arguments the mint path passes. It can create or
                        // restart an entry; it has no path that retracts one.
                        if let Err(error) = self
                            .store
                            .observe_cooldown(
                                notice.account,
                                observation.now_ms,
                                observation.newest_live_strike_ms,
                            )
                            .await
                        {
                            outcome.evaluated -= 1;
                            outcome.failed += 1;
                            tracing::warn!(
                                account = notice.account.0,
                                %error,
                                "could not record a cooldown entry; leaving its notice queued"
                            );
                            continue;
                        }
                        outcome.published += 1;
                    }
                    match self.queue.clear_if(notice).await {
                        Ok(true) => outcome.cleared += 1,
                        // A filing landed during evaluation. Leave it queued.
                        Ok(false) => {}
                        Err(error) => tracing::warn!(
                            account = notice.account.0,
                            %error,
                            "could not clear an evaluated filing notice; it will be re-evaluated"
                        ),
                    }
                }
            }
        }
        Ok(outcome)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::standing::StandingThresholds;
    use crate::{ComputedStanding, MemAccountStore};
    use orrery_persistd::adjudication::{
        StrikeEvidenceRef, StrikeKind, StrikeMode, StrikeRow, MAJOR_STRIKE_WEIGHT_MILLI,
        STRIKE_RETENTION_MS,
    };
    use orrery_protocol::{PersistId, RulesetId, Tick};
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

    const ALICE: AccountId = AccountId(0x0862_9000_0000_0001);
    const BOB: AccountId = AccountId(0x0862_9000_0000_0002);
    /// Bound by an executor but never created in the identity store, which is
    /// what an `UnknownAccount` looks like on a real filing path.
    const GHOST: AccountId = AccountId(0x0862_9000_0000_0003);

    /// The instant a filing lands. Deliberately not zero: production stamps a
    /// wall clock, so a fixture that feeds 0 would exercise a value the
    /// deployed path can never emit.
    const FILED_AT_MS: u64 = 1_756_000_000_000;

    fn lock<T>(cell: &Mutex<T>) -> MutexGuard<'_, T> {
        cell.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// An in-memory queue with the durable one's compare-and-clear contract,
    /// so the unit tests drive the production control flow rather than a
    /// simplified one.
    #[derive(Debug, Default)]
    struct MemQueue(Mutex<Vec<FilingNotice>>);

    impl MemQueue {
        fn with(notices: impl IntoIterator<Item = FilingNotice>) -> Arc<Self> {
            Arc::new(Self(Mutex::new(notices.into_iter().collect())))
        }

        fn remaining(&self) -> Vec<FilingNotice> {
            lock(&self.0).clone()
        }
    }

    #[async_trait]
    impl FilingNoticeQueue for Arc<MemQueue> {
        async fn pending(&self) -> Result<Vec<FilingNotice>, IdentityError> {
            Ok(lock(&self.0).clone())
        }

        async fn clear_if(&self, notice: FilingNotice) -> Result<bool, IdentityError> {
            let mut queued = lock(&self.0);
            let before = queued.len();
            queued.retain(|held| *held != notice);
            Ok(queued.len() != before)
        }
    }

    #[derive(Debug, Clone, Default)]
    struct Rows(Arc<HashMap<AccountId, Vec<StrikeRow>>>);

    impl Rows {
        fn new(rows: impl IntoIterator<Item = (AccountId, Vec<StrikeRow>)>) -> Self {
            Self(Arc::new(rows.into_iter().collect()))
        }
    }

    #[async_trait]
    impl StrikeRowSource for Rows {
        async fn rows(&self, account: AccountId) -> Result<Vec<StrikeRow>, IdentityError> {
            Ok(self.0.get(&account).cloned().unwrap_or_default())
        }
    }

    fn strike(mode: StrikeMode, issued_at_ms: u64) -> StrikeRow {
        StrikeRow {
            issued_at_ms,
            weight_milli: MAJOR_STRIKE_WEIGHT_MILLI,
            kind: StrikeKind::Deviation,
            evidence_ref: StrikeEvidenceRef {
                entity: PersistId::new(1),
                window_start: Tick::new(1),
                window_end: Tick::new(2),
                digest: [issued_at_ms as u8; 32],
            },
            ruleset: RulesetId {
                version: 1,
                digest: [1; 32],
            },
            mode,
            expires_at_ms: issued_at_ms + STRIKE_RETENTION_MS,
        }
    }

    /// Two current major findings, which is what puts an account over `C`.
    fn over_c(mode: StrikeMode) -> Vec<StrikeRow> {
        vec![strike(mode, FILED_AT_MS), strike(mode, FILED_AT_MS)]
    }

    async fn store() -> Arc<MemAccountStore> {
        let store = Arc::new(MemAccountStore::new());
        for account in [ALICE, BOB] {
            store.create_account(account, 0).await.expect("create");
        }
        store
    }

    fn reactor(
        store: Arc<MemAccountStore>,
        queue: Arc<MemQueue>,
        rows: Rows,
        now_ms: u64,
        mode: StrikesEnforcement,
    ) -> StandingFilingReactor<
        Arc<MemAccountStore>,
        Arc<MemQueue>,
        Rows,
        impl Fn() -> u64 + Send + Sync,
    > {
        let scorer = ComputedStanding::new(rows, move || now_ms, StandingThresholds::default())
            .expect("the default policy package is coherent");
        StandingFilingReactor::new(store, queue, scorer, StrikesPosture::new(mode))
    }

    fn notice(account: AccountId) -> FilingNotice {
        FilingNotice {
            account,
            filed_at_ms: FILED_AT_MS,
        }
    }

    /// The whole point of the module: an account that crossed `C` and never
    /// came back to identity now has a `dc` entry, so
    /// [`crate::invalidation`] publishes it.
    #[tokio::test]
    async fn a_filing_publishes_without_the_account_ever_minting_again() {
        let store = store().await;
        let queue = MemQueue::with([notice(ALICE)]);
        let reactor = reactor(
            Arc::clone(&store),
            Arc::clone(&queue),
            Rows::new([(ALICE, over_c(StrikeMode::Live))]),
            FILED_AT_MS + 1_000,
            StrikesEnforcement::Live,
        );

        let sweep = reactor.sweep().await.expect("sweep the queue");
        assert_eq!(
            (sweep.seen, sweep.evaluated, sweep.published, sweep.cleared),
            (1, 1, 1, 1),
            "one notice, scored once, published once, drained once"
        );

        let published = crate::invalidation::StandingInvalidationSource::new(Arc::clone(&store))
            .current()
            .await
            .expect("publish");
        assert_eq!(
            published,
            vec![orrery_protocol::AccountInvalidation {
                account: ALICE,
                effective_from_ms: orrery_protocol::UnixMillis(FILED_AT_MS + 1_000),
            }],
            "a_filing_publishes_without_the_account_ever_minting_again"
        );
        assert!(
            queue.remaining().is_empty(),
            "an evaluated notice is drained"
        );
    }

    /// D32 clause (b). `Off` must not read, evaluate, publish or drain — and
    /// must not lose the notice, or promoting the control later would act on
    /// nothing that was filed while it was off.
    #[tokio::test]
    async fn an_off_posture_publishes_nothing_and_keeps_the_notice() {
        let store = store().await;
        let queue = MemQueue::with([notice(ALICE)]);
        let reactor = reactor(
            Arc::clone(&store),
            Arc::clone(&queue),
            Rows::new([(ALICE, over_c(StrikeMode::Live))]),
            FILED_AT_MS + 1_000,
            StrikesEnforcement::Off,
        );

        let sweep = reactor.sweep().await.expect("sweep");
        assert_eq!(
            sweep,
            FilingSweep {
                mode: StrikesEnforcement::Off,
                ..FilingSweep::default()
            },
            "an_off_posture_publishes_nothing_and_keeps_the_notice"
        );
        assert_eq!(
            store.cooldown_entry(ALICE).await.expect("read"),
            None,
            "off observes nothing, so no durable entry appears"
        );
        assert_eq!(
            queue.remaining(),
            vec![notice(ALICE)],
            "the notice survives"
        );
    }

    /// #934's bug in the other direction: a shadow posture must run the full
    /// predicate and change nothing. It must also not drain, or a promotion to
    /// live would find an empty queue.
    #[tokio::test]
    async fn a_shadow_posture_evaluates_but_writes_no_entry_and_drains_nothing() {
        let store = store().await;
        let queue = MemQueue::with([notice(ALICE)]);
        let reactor = reactor(
            Arc::clone(&store),
            Arc::clone(&queue),
            Rows::new([(ALICE, over_c(StrikeMode::Live))]),
            FILED_AT_MS + 1_000,
            StrikesEnforcement::Shadow,
        );

        let sweep = reactor.sweep().await.expect("sweep");
        assert_eq!(
            (
                sweep.evaluated,
                sweep.would_publish,
                sweep.published,
                sweep.cleared
            ),
            (1, 1, 0, 0),
            "shadow counts the would-be action and takes none"
        );
        assert_eq!(
            store.cooldown_entry(ALICE).await.expect("read"),
            None,
            "a_shadow_posture_evaluates_but_writes_no_entry_and_drains_nothing"
        );
        assert_eq!(queue.remaining(), vec![notice(ALICE)]);

        // The promotion the retained notice exists for.
        reactor.posture().set(StrikesEnforcement::Live);
        let sweep = reactor.sweep().await.expect("sweep after promotion");
        assert_eq!(sweep.published, 1, "promotion acts on the retained notice");
    }

    /// Box 4. A shadow-stamped `ya` row changes no standing on this path
    /// either — and not by a branch here: the executor writes a notice for
    /// every filing, and the scorer is what ignores the row.
    #[tokio::test]
    async fn a_shadow_stamped_row_publishes_nothing_even_at_a_live_posture() {
        let store = store().await;
        let queue = MemQueue::with([notice(ALICE)]);
        let reactor = reactor(
            Arc::clone(&store),
            Arc::clone(&queue),
            Rows::new([(ALICE, over_c(StrikeMode::Shadow))]),
            FILED_AT_MS + 1_000,
            StrikesEnforcement::Live,
        );

        let sweep = reactor.sweep().await.expect("sweep");
        assert_eq!(
            (sweep.evaluated, sweep.published, sweep.cleared),
            (1, 0, 1),
            "the account is scored, found clear, and its notice drained"
        );
        assert_eq!(
            store.cooldown_entry(ALICE).await.expect("read"),
            None,
            "a_shadow_stamped_row_publishes_nothing_even_at_a_live_posture"
        );
    }

    /// D33 clause (e)'s monotonicity. A second sweep must republish the same
    /// watermark: this reactor may create an entry, never move one forward to
    /// its own later clock, and never retract one.
    #[tokio::test]
    async fn a_later_sweep_does_not_move_the_watermark() {
        let store = store().await;
        let rows = Rows::new([(ALICE, over_c(StrikeMode::Live))]);
        let first = reactor(
            Arc::clone(&store),
            MemQueue::with([notice(ALICE)]),
            rows.clone(),
            FILED_AT_MS + 1_000,
            StrikesEnforcement::Live,
        );
        first.sweep().await.expect("first sweep");
        let entered = store
            .cooldown_entry(ALICE)
            .await
            .expect("read")
            .expect("the first sweep entered cooldown");

        // A much later sweep, over the same rows, with the notice re-queued.
        let second = reactor(
            Arc::clone(&store),
            MemQueue::with([notice(ALICE)]),
            rows,
            FILED_AT_MS + 9_000_000,
            StrikesEnforcement::Live,
        );
        second.sweep().await.expect("second sweep");

        assert_eq!(
            store.cooldown_entry(ALICE).await.expect("read"),
            Some(entered),
            "a_later_sweep_does_not_move_the_watermark"
        );
    }

    /// One unreadable account must not stop every other account's
    /// invalidation from ever being published — and its own notice stays
    /// queued for the next sweep rather than being silently dropped.
    #[tokio::test]
    async fn one_unresolvable_account_does_not_abort_the_sweep() {
        let store = store().await;
        let queue = MemQueue::with([notice(GHOST), notice(BOB)]);
        let reactor = reactor(
            Arc::clone(&store),
            Arc::clone(&queue),
            Rows::new([
                (GHOST, over_c(StrikeMode::Live)),
                (BOB, over_c(StrikeMode::Live)),
            ]),
            FILED_AT_MS + 1_000,
            StrikesEnforcement::Live,
        );

        let sweep = reactor.sweep().await.expect("sweep");
        assert_eq!(
            (sweep.seen, sweep.failed, sweep.published),
            (2, 1, 1),
            "one_unresolvable_account_does_not_abort_the_sweep"
        );
        assert!(
            store.cooldown_entry(BOB).await.expect("read").is_some(),
            "the readable account is still published"
        );
        assert_eq!(
            queue.remaining(),
            vec![notice(GHOST)],
            "the failed notice is retried, the succeeded one is drained"
        );
    }

    /// The other failure arm: D33 clause (f)'s unreadable ledger. A read the
    /// scorer cannot complete must leave the notice queued rather than be
    /// scored as `Good` and drained — the same fail-closed posture the mint
    /// path takes, expressed as "evaluate it again later".
    #[tokio::test]
    async fn an_unreadable_ledger_leaves_the_notice_queued() {
        #[derive(Debug, Clone, Default)]
        struct Unreadable;

        #[async_trait]
        impl StrikeRowSource for Unreadable {
            async fn rows(&self, _account: AccountId) -> Result<Vec<StrikeRow>, IdentityError> {
                Err(IdentityError::Store("ledger unreadable".into()))
            }
        }

        let store = store().await;
        let queue = MemQueue::with([notice(ALICE)]);
        let scorer = ComputedStanding::new(
            Unreadable,
            || FILED_AT_MS + 1_000,
            StandingThresholds::default(),
        )
        .expect("the default policy package is coherent");
        let reactor = StandingFilingReactor::new(
            Arc::clone(&store),
            Arc::clone(&queue),
            scorer,
            StrikesPosture::new(StrikesEnforcement::Live),
        );

        let sweep = reactor
            .sweep()
            .await
            .expect("an unreadable account is not a failed sweep");
        assert_eq!(
            (sweep.seen, sweep.evaluated, sweep.failed, sweep.cleared),
            (1, 0, 1, 0),
            "an_unreadable_ledger_leaves_the_notice_queued"
        );
        assert_eq!(queue.remaining(), vec![notice(ALICE)]);
    }

    /// A filing that lands while the sweep is evaluating must survive the
    /// clear, or the strike it names is never evaluated by anyone.
    #[tokio::test]
    async fn a_notice_overwritten_during_evaluation_is_not_dropped() {
        let store = store().await;
        let queue = MemQueue::with([FilingNotice {
            account: ALICE,
            filed_at_ms: FILED_AT_MS + 5_000,
        }]);
        let reactor = reactor(
            Arc::clone(&store),
            Arc::clone(&queue),
            Rows::new([(ALICE, over_c(StrikeMode::Live))]),
            FILED_AT_MS + 6_000,
            StrikesEnforcement::Live,
        );
        // The sweep reads the queue, then a later filing overwrites the notice
        // before the clear. Modelled by clearing a notice that no longer
        // matches: the durable `clear_if` compares the stamp.
        assert!(
            !queue
                .clear_if(notice(ALICE))
                .await
                .expect("compare-and-clear"),
            "a_notice_overwritten_during_evaluation_is_not_dropped"
        );
        let sweep = reactor.sweep().await.expect("sweep");
        assert_eq!(sweep.cleared, 1, "the notice actually read is drained");
    }
}
