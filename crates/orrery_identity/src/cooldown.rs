//! D33's durable cooldown dwell policy around the read-only strike scorer.
//!
//! [`crate::standing`] scores executor-written rows and never writes them (or
//! anything else). This module is the identity-owned mutation boundary: it
//! turns one score observation into an admission result while recording the
//! derived `dc` cooldown entry and `dn` ban row in [`crate::AccountStore`].
//!
//! # Two rules the score alone cannot express
//!
//! [`crate::standing::StandingThresholds::classify`] answers "which band is the
//! live score in *now*", and the score decays. Two of D33 clause (e)'s
//! sentences are therefore not derivable from it, and both live here (#1059):
//!
//! - **"ban never reverses by decay."** The durable `dn` row is read before the
//!   bands, so an account banned at 9.0 stays refused past the 5.08 days its
//!   score needs to fall back under `B`. Only an administrative lift —
//!   [`crate::AccountStore::lift_ban_if`], which no admission path calls —
//!   removes it.
//! - **"cooldown reverses only after both `S < Q` and fourteen consecutive
//!   days."** Release requires [`StandingLevel::Good`], not merely falling out
//!   of the cooldown band.

use crate::service::StandingSource;
use crate::standing::{
    ComputedStanding, StandingLevel, StandingObservation, StandingThresholds, StrikeRowSource,
};
use crate::store::{AccountStore, IdentityError};
use async_trait::async_trait;
use orrery_protocol::{AccountId, SessionStanding};

/// A standing source that enforces D33's minimum cooldown dwell.
///
/// The scorer remains read-only; the account store owns the derived entry
/// timestamp. Keeping the two roles separate prevents a future scoring change
/// from accidentally becoming a second writer of the executor ledger.
pub struct CooldownStanding<S, R, C> {
    store: S,
    scorer: ComputedStanding<R, C>,
}

impl<S, R, C> CooldownStanding<S, R, C> {
    /// Combine an identity store with a read-only configured scorer.
    #[must_use]
    pub const fn new(store: S, scorer: ComputedStanding<R, C>) -> Self {
        Self { store, scorer }
    }
}

#[async_trait]
impl<S, R, C> StandingSource for CooldownStanding<S, R, C>
where
    S: AccountStore,
    R: StrikeRowSource + Send + Sync,
    C: Fn() -> u64 + Send + Sync,
{
    async fn standing(
        &self,
        account: AccountId,
        _store: &dyn AccountStore,
    ) -> Result<SessionStanding, IdentityError> {
        let observation = self.scorer.observe(account).await?;
        apply_dwell(&self.store, account, self.scorer.thresholds(), observation).await
    }
}

/// Apply D33's durable dwell rule to one read-only score observation.
///
/// Kept outside [`crate::standing`]: it mutates derived `d`-family state, so
/// it must remain visibly separate from the executor-ledger scorer.
async fn apply_dwell(
    store: &dyn AccountStore,
    account: AccountId,
    thresholds: StandingThresholds,
    observation: StandingObservation,
) -> Result<SessionStanding, IdentityError> {
    // The durable ban is consulted **before** the score bands, because that
    // ordering is the whole of "ban never reverses by decay" (D33 clause (e)).
    // `StandingLevel::Banned` is the band the live score is in at this instant
    // and the score decays: at the clause (d) defaults a 9.0 ban re-enters the
    // cooldown band after 5.08 days, and before #1059 that account went on to
    // release through the ordinary dwell. Reading the row first makes the
    // arithmetic unable to reach the decision at all.
    if store.ban_entry(account).await?.is_some() {
        return Err(IdentityError::Banned(account));
    }

    // A ban is a separate admission refusal, but it is still at or above C.
    // Recording its entry preserves the original cooldown start if an upheld
    // appeal later drops the score below B but not below C.
    if matches!(
        observation.level,
        StandingLevel::Cooldown | StandingLevel::Banned
    ) {
        store
            .observe_cooldown(
                account,
                observation.now_ms,
                observation.newest_live_strike_ms,
            )
            .await?;
        return match observation.level {
            StandingLevel::Cooldown => Err(IdentityError::Cooldown(account)),
            StandingLevel::Banned => {
                // First observation of `S >= B` is what makes the ban durable.
                // It is idempotent and first-write-wins, so a later observation
                // — including one at a decayed score that never reaches this
                // arm again — cannot move the instant the sanction began.
                store.record_ban(account, observation.now_ms).await?;
                Err(IdentityError::Banned(account))
            }
            StandingLevel::Good | StandingLevel::Quarantined => unreachable!(),
        };
    }

    // An account that has never entered cooldown has no `dc` row. Do not
    // manufacture one merely because its present score is below C.
    let Some(entry) = store.cooldown_entry(account).await? else {
        return match observation.level {
            StandingLevel::Good => Ok(SessionStanding::Good),
            StandingLevel::Quarantined => Ok(SessionStanding::Quarantined),
            StandingLevel::Cooldown | StandingLevel::Banned => unreachable!(),
        };
    };

    // Clause (e) makes cooldown's reversal conditional on **both** `S < Q` and
    // the fourteen days, not on `S < C`: "cooldown reverses only after both
    // `S < Q` and fourteen consecutive days since its most recent entry"
    // (`docs/adr/0033-strike-ledger-standing.md:238`). Falling out of the
    // cooldown *band* only stops the score escalating; it is `S < Q` that ends
    // the sanction. Until #1059 an account with a `dc` row released the moment
    // the dwell elapsed with `Q <= S < C`, i.e. into `Quarantined`, which is
    // one whole band early — and early by an unbounded margin, since a score
    // sitting just under `C` can hold that band for many further days.
    if !matches!(observation.level, StandingLevel::Good) {
        return Err(IdentityError::Cooldown(account));
    }

    // Score has fallen below Q. It is safe to lift only after the durable
    // entry's full dwell. `saturating_sub` makes a backward clock step a zero
    // elapsed interval rather than a route around the floor.
    if observation.now_ms.saturating_sub(entry.entered_at_ms) < thresholds.cooldown_min_ms {
        return Err(IdentityError::Cooldown(account));
    }

    // Compare-and-clear rather than an unconditional delete: a concurrent
    // score observation that saw a new live strike must win and keep its
    // restarted entry.
    if !store.clear_cooldown_if(account, entry).await? {
        return Err(IdentityError::Cooldown(account));
    }

    Ok(SessionStanding::Good)
}

/// IdentityService's concrete scorer path.
///
/// The implementation lives here rather than in [`crate::standing`] because
/// it applies the durable dwell mutation. That keeps scoring over `ya` rows
/// read-only even though every real admission uses this source.
#[async_trait]
impl<R, C> StandingSource for ComputedStanding<R, C>
where
    R: StrikeRowSource + Send + Sync,
    C: Fn() -> u64 + Send + Sync,
{
    async fn standing(
        &self,
        account: AccountId,
        store: &dyn AccountStore,
    ) -> Result<SessionStanding, IdentityError> {
        let observation = self.observe(account).await?;
        apply_dwell(store, account, self.thresholds(), observation).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::standing::StandingThresholds;
    use crate::{AccountStore, ComputedStanding, MemAccountStore};
    use async_trait::async_trait;
    use orrery_persistd::adjudication::{
        StrikeEvidenceRef, StrikeKind, StrikeMode, StrikeRow, MAJOR_STRIKE_WEIGHT_MILLI,
        STRIKE_RETENTION_MS,
    };
    use orrery_protocol::{PersistId, RulesetId, Tick};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

    const DAY_MS: u64 = 24 * 60 * 60 * 1_000;
    const ACCOUNT: AccountId = AccountId(0x0862_0000_0000_0003);

    #[derive(Clone, Default)]
    struct MutableStrikeRows(Arc<Mutex<Vec<StrikeRow>>>);

    impl MutableStrikeRows {
        fn new(rows: Vec<StrikeRow>) -> Self {
            Self(Arc::new(Mutex::new(rows)))
        }

        fn replace(&self, rows: Vec<StrikeRow>) {
            *Self::lock(&self.0) = rows;
        }

        fn lock(rows: &Mutex<Vec<StrikeRow>>) -> MutexGuard<'_, Vec<StrikeRow>> {
            rows.lock().unwrap_or_else(PoisonError::into_inner)
        }
    }

    #[async_trait]
    impl StrikeRowSource for MutableStrikeRows {
        async fn rows(&self, account: AccountId) -> Result<Vec<StrikeRow>, IdentityError> {
            assert_eq!(account, ACCOUNT);
            Ok(Self::lock(&self.0).clone())
        }
    }

    fn major(issued_at_ms: u64) -> StrikeRow {
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
            mode: StrikeMode::Live,
            expires_at_ms: issued_at_ms + STRIKE_RETENTION_MS,
        }
    }

    async fn fixture(
        rows: MutableStrikeRows,
    ) -> (
        ComputedStanding<MutableStrikeRows, impl Fn() -> u64 + Send + Sync>,
        Arc<MemAccountStore>,
        Arc<AtomicU64>,
    ) {
        let store = Arc::new(MemAccountStore::new());
        store
            .create_account(ACCOUNT, 0)
            .await
            .expect("create account");
        let now = Arc::new(AtomicU64::new(0));
        let clock_now = Arc::clone(&now);
        let scorer = ComputedStanding::new(
            rows,
            move || clock_now.load(Ordering::SeqCst),
            StandingThresholds::default(),
        )
        .expect("default policy is coherent");
        (scorer, store, now)
    }

    #[tokio::test]
    async fn dwell_floor_refuses_after_decay_falls_below_cooldown() {
        let rows = MutableStrikeRows::new(vec![major(0), major(0)]);
        let (standing, store, now) = fixture(rows).await;

        assert_eq!(
            standing.standing(ACCOUNT, store.as_ref()).await,
            Err(IdentityError::Cooldown(ACCOUNT)),
            "two current major findings enter cooldown"
        );
        now.store(4 * DAY_MS, Ordering::SeqCst);
        assert!(
            crate::score_rows(&[major(0), major(0)], 4 * DAY_MS).live_milli
                < StandingThresholds::default().cooldown_milli,
            "the test reaches the score-cleared side before its 14-day dwell"
        );
        assert_eq!(
            standing.standing(ACCOUNT, store.as_ref()).await,
            Err(IdentityError::Cooldown(ACCOUNT)),
            "dwell_floor_refuses_after_decay_falls_below_cooldown"
        );
    }

    #[tokio::test]
    async fn reentry_restarts_the_cooldown_dwell_clock() {
        let rows = MutableStrikeRows::new(vec![major(0), major(0)]);
        let (standing, store, now) = fixture(rows.clone()).await;
        assert_eq!(
            standing.standing(ACCOUNT, store.as_ref()).await,
            Err(IdentityError::Cooldown(ACCOUNT))
        );

        now.store(10 * DAY_MS, Ordering::SeqCst);
        rows.replace(vec![major(0), major(0), major(10 * DAY_MS)]);
        assert_eq!(
            standing.standing(ACCOUNT, store.as_ref()).await,
            Err(IdentityError::Cooldown(ACCOUNT)),
            "the new live strike leaves the resulting score above C and restarts dwell"
        );

        now.store(20 * DAY_MS, Ordering::SeqCst);
        assert!(
            crate::score_rows(&[major(0), major(0), major(10 * DAY_MS)], 20 * DAY_MS).live_milli
                < StandingThresholds::default().cooldown_milli
        );
        assert_eq!(
            standing.standing(ACCOUNT, store.as_ref()).await,
            Err(IdentityError::Cooldown(ACCOUNT)),
            "reentry_restarts_the_cooldown_dwell_clock"
        );
    }

    /// D33 clause (e) (`docs/adr/0033-strike-ledger-standing.md:238`):
    /// "cooldown reverses only after **both** `S < Q` and fourteen consecutive
    /// days since its most recent entry". Both conditions, not the dwell alone.
    ///
    /// This test used to assert the opposite — that the dwell elapsing released
    /// the account into `Quarantined` — which is one whole band early and is
    /// the divergence #1059 names. The clause is not stale: its own worked
    /// rationale immediately below it (`:242-246`) computes
    /// `14·log2(5/3) = 10.32 d` as "the time decay needs to fall below 3", so
    /// the number the record reasons about is `Q`, not `C`.
    #[tokio::test]
    async fn release_needs_the_score_below_quarantine_not_merely_below_cooldown() {
        let rows = MutableStrikeRows::new(vec![major(0), major(0)]);
        let (standing, store, now) = fixture(rows).await;
        assert_eq!(
            standing.standing(ACCOUNT, store.as_ref()).await,
            Err(IdentityError::Cooldown(ACCOUNT))
        );

        // One half-life on: 6.0 has decayed to exactly Q = 3.0, and the
        // fourteen-day floor has elapsed. The dwell is satisfied and `S < Q`
        // is not, so the account is still refused.
        now.store(14 * DAY_MS, Ordering::SeqCst);
        let thresholds = StandingThresholds::default();
        let at_fourteen = crate::score_rows(&[major(0), major(0)], 14 * DAY_MS).live_milli;
        assert_eq!(at_fourteen, thresholds.quarantine_milli);
        assert!(at_fourteen < thresholds.cooldown_milli);
        assert_eq!(
            standing.standing(ACCOUNT, store.as_ref()).await,
            Err(IdentityError::Cooldown(ACCOUNT)),
            "out of the cooldown band but not yet below Q: clause (e) keeps it in cooldown"
        );

        // A day later the score is genuinely below Q and both conditions hold.
        now.store(15 * DAY_MS, Ordering::SeqCst);
        assert!(
            crate::score_rows(&[major(0), major(0)], 15 * DAY_MS).live_milli
                < thresholds.quarantine_milli
        );
        assert_eq!(
            standing.standing(ACCOUNT, store.as_ref()).await,
            Ok(SessionStanding::Good),
            "S < Q and the dwell elapsed: the entry is cleared and the account releases"
        );
        assert_eq!(
            store.cooldown_entry(ACCOUNT).await.expect("read"),
            None,
            "release clears the durable entry"
        );
    }

    /// The defect #1059 filed: `classify` reports the band the live score is in
    /// *now*, so without a durable row a ban is a fourteen-day cooldown wearing
    /// a different name. D33 clause (e): "ban never reverses by decay".
    #[tokio::test]
    async fn a_ban_outlives_the_decay_of_the_score_that_produced_it() {
        let rows = MutableStrikeRows::new(vec![major(0), major(0), major(0)]);
        let (standing, store, now) = fixture(rows).await;
        let thresholds = StandingThresholds::default();

        assert_eq!(
            standing.standing(ACCOUNT, store.as_ref()).await,
            Err(IdentityError::Banned(ACCOUNT)),
            "three current major findings reach B = 7.0 at a score of 9.0"
        );
        assert_eq!(
            store.ban_entry(ACCOUNT).await.expect("read"),
            Some(crate::BanEntry { banned_at_ms: 0 }),
            "the crossing is what makes the ban durable"
        );

        // 9.0 falls back under B = 7.0 at 14·log2(9/7) = 5.08 days. Before
        // #1059 the account was `Cooldown` from here and released at day 14.
        now.store(6 * DAY_MS, Ordering::SeqCst);
        let at_six = crate::score_rows(&[major(0), major(0), major(0)], 6 * DAY_MS).live_milli;
        assert!(at_six < thresholds.ban_milli && at_six >= thresholds.cooldown_milli);
        assert_eq!(
            standing.standing(ACCOUNT, store.as_ref()).await,
            Err(IdentityError::Banned(ACCOUNT)),
            "the score is in the cooldown band; the durable row still says banned"
        );

        // And far past both the dwell floor and `S < Q`, where an account that
        // had only ever been in cooldown would be `Good`.
        now.store(40 * DAY_MS, Ordering::SeqCst);
        assert!(
            crate::score_rows(&[major(0), major(0), major(0)], 40 * DAY_MS).live_milli
                < thresholds.quarantine_milli
        );
        assert_eq!(
            standing.standing(ACCOUNT, store.as_ref()).await,
            Err(IdentityError::Banned(ACCOUNT)),
            "no amount of elapsed time reverses a ban"
        );
        assert_eq!(
            store.ban_entry(ACCOUNT).await.expect("read"),
            Some(crate::BanEntry { banned_at_ms: 0 }),
            "and no later observation re-stamps the instant the sanction began"
        );
    }

    /// The other half of the same rule: the row is not a black hole. An
    /// administrative lift — the only thing D33 clause (e) leaves — ends the
    /// ban, and the account then falls back under the ordinary dwell.
    #[tokio::test]
    async fn an_administrative_lift_is_what_ends_a_ban() {
        let rows = MutableStrikeRows::new(vec![major(0), major(0), major(0)]);
        let (standing, store, now) = fixture(rows).await;
        assert_eq!(
            standing.standing(ACCOUNT, store.as_ref()).await,
            Err(IdentityError::Banned(ACCOUNT))
        );
        let ban = store
            .ban_entry(ACCOUNT)
            .await
            .expect("read")
            .expect("banned");

        // Compare-and-clear: a lift authorised against a ban that is no longer
        // the stored one changes nothing.
        assert!(
            !store
                .lift_ban_if(ACCOUNT, crate::BanEntry { banned_at_ms: 1 })
                .await
                .expect("lift"),
            "a stale expectation does not clear the row"
        );
        assert_eq!(
            standing.standing(ACCOUNT, store.as_ref()).await,
            Err(IdentityError::Banned(ACCOUNT))
        );

        assert!(store.lift_ban_if(ACCOUNT, ban).await.expect("lift"));

        // Lifted, but not admitted: the `dc` entry the ban also wrote is still
        // there, so the account serves the ordinary cooldown on the score it
        // actually has.
        now.store(6 * DAY_MS, Ordering::SeqCst);
        assert_eq!(
            standing.standing(ACCOUNT, store.as_ref()).await,
            Err(IdentityError::Cooldown(ACCOUNT)),
            "a lifted ban leaves the account where its live score puts it"
        );

        now.store(40 * DAY_MS, Ordering::SeqCst);
        assert_eq!(
            standing.standing(ACCOUNT, store.as_ref()).await,
            Ok(SessionStanding::Good)
        );
    }
}
