//! D33's durable cooldown dwell policy around the read-only strike scorer.
//!
//! [`crate::standing`] scores executor-written rows and never writes them (or
//! anything else). This module is the identity-owned mutation boundary: it
//! turns one score observation into an admission result while recording the
//! derived `dc` cooldown entry in [`crate::AccountStore`].

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
            StandingLevel::Banned => Err(IdentityError::Banned(account)),
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

    // Score has fallen below C. It is safe to lift only after the durable
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

    match observation.level {
        StandingLevel::Good => Ok(SessionStanding::Good),
        StandingLevel::Quarantined => Ok(SessionStanding::Quarantined),
        StandingLevel::Cooldown | StandingLevel::Banned => unreachable!(),
    }
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

    #[tokio::test]
    async fn score_decay_behavior_is_unchanged_after_dwell_passes() {
        let rows = MutableStrikeRows::new(vec![major(0), major(0)]);
        let (standing, store, now) = fixture(rows).await;
        assert_eq!(
            standing.standing(ACCOUNT, store.as_ref()).await,
            Err(IdentityError::Cooldown(ACCOUNT))
        );

        now.store(14 * DAY_MS, Ordering::SeqCst);
        assert_eq!(
            standing.standing(ACCOUNT, store.as_ref()).await,
            Ok(SessionStanding::Quarantined),
            "once dwell has passed, the existing decayed score classification is unchanged"
        );
    }
}
