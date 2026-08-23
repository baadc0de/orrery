//! Read-time scoring for D33's executor-written `ya` strike ledger.
//!
//! This module is deliberately read-only. The row type and key layout come
//! from [`orrery_persistd::adjudication`], whose executor is the only writer;
//! identity receives rows, applies continuous decay at the read instant, and
//! turns the live-only score into the existing token/admission result.

use crate::service::StandingSource;
use crate::store::IdentityError;
use async_trait::async_trait;
use orrery_persistd::adjudication::{StrikeMode, StrikeRow, STRIKE_HALF_LIFE_MS};
use orrery_protocol::{AccountId, SessionStanding};
use std::collections::HashMap;

/// The configured account-policy package: the quarantine/cooldown/ban
/// boundaries in milli-points, and the probation window in milliseconds.
///
/// One value rather than two, because D33 clause (d) names them as one dial
/// set — "`Q`, `C`, `B`, the minimum cooldown and the probation window are
/// deployment configuration, not constants of this record" — and a second
/// standalone probation constant would be a second place to configure a number
/// the record says has one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StandingThresholds {
    /// `Good` becomes `Quarantined` at this score.
    pub quarantine_milli: i64,
    /// `Quarantined` becomes `Cooldown` at this score.
    pub cooldown_milli: i64,
    /// `Cooldown` becomes `Banned` at this score.
    pub ban_milli: i64,
    /// How long after `AccountRow::created_ms` an account remains on
    /// probation, in milliseconds.
    ///
    /// Probation is not a score band, so it is not classified with the other
    /// three: it is a fact about the account row, evaluated at mint time by
    /// [`StandingThresholds::on_probation`] and stamped into the token.
    pub probation_ms: u64,
}

/// D33 clause (d)'s recommended package: 3/5/7 with a 7-day probation. Owner
/// selection can replace this value.
pub const DEFAULT_STANDING_THRESHOLDS: StandingThresholds = StandingThresholds {
    quarantine_milli: 3_000,
    cooldown_milli: 5_000,
    ban_milli: 7_000,
    probation_ms: 7 * 24 * 60 * 60 * 1_000,
};

impl Default for StandingThresholds {
    fn default() -> Self {
        DEFAULT_STANDING_THRESHOLDS
    }
}

impl StandingThresholds {
    /// Whether an account created at `created_ms` is still inside probation at
    /// `now_ms` (D33 clause (d), `docs/07-witnessing.md` §5).
    ///
    /// The comparison is `elapsed < probation_ms`, so the boundary instant is
    /// the first one past probation and a window of zero means "no probation"
    /// rather than "everyone, forever". A `created_ms` in the future — a clock
    /// that stepped backwards, or a row written by a host whose RTC is wrong —
    /// is *not* read as "very old": the subtraction saturates to zero elapsed
    /// time and the account stays on probation. Subtracting the other way round
    /// would make a skewed clock the cheapest way to skip probation entirely.
    #[must_use]
    pub const fn on_probation(self, created_ms: u64, now_ms: u64) -> bool {
        now_ms.saturating_sub(created_ms) < self.probation_ms
    }

    /// Classify a conservatively rounded live score.
    #[must_use]
    pub const fn classify(self, score_milli: i64) -> StandingLevel {
        if score_milli >= self.ban_milli {
            StandingLevel::Banned
        } else if score_milli >= self.cooldown_milli {
            StandingLevel::Cooldown
        } else if score_milli >= self.quarantine_milli {
            StandingLevel::Quarantined
        } else {
            StandingLevel::Good
        }
    }
}

/// Instantaneous D33 score band at one read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StandingLevel {
    /// Below quarantine.
    Good,
    /// At or above Q and below C.
    Quarantined,
    /// At or above C and below B; identity refuses a token.
    Cooldown,
    /// At or above B; identity refuses a token.
    Banned,
}

/// Live-only and calibration scores at the same read instant.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StandingScores {
    /// Sum over `mode = live`; this is the only enforcement score.
    pub live_milli: i64,
    /// Sum over all rows; telemetry only.
    pub shadow_milli: i64,
}

/// Evaluate D33's decay at `now_ms`, without mutating any row.
///
/// Each contribution is rounded toward positive infinity. At a threshold this
/// is conservative for both positive findings and negative appeal facts: a
/// platform approximation cannot make the account appear safer.
#[must_use]
pub fn score_rows(rows: &[StrikeRow], now_ms: u64) -> StandingScores {
    let mut scores = StandingScores::default();
    for row in rows {
        if row.issued_at_ms > now_ms || row.expires_at_ms <= now_ms {
            continue;
        }
        let age_ms = now_ms - row.issued_at_ms;
        let exponent = -(age_ms as f64) / (STRIKE_HALF_LIFE_MS as f64);
        let contribution = (f64::from(row.weight_milli) * exponent.exp2()).ceil() as i64;
        scores.shadow_milli = scores.shadow_milli.saturating_add(contribution);
        if row.mode == StrikeMode::Live {
            scores.live_milli = scores.live_milli.saturating_add(contribution);
        }
    }
    scores
}

/// Read-only source of one account's `ya` values.
#[async_trait]
pub trait StrikeRowSource: Send + Sync {
    /// Read the account-contiguous row span.
    async fn rows(&self, account: AccountId) -> Result<Vec<StrikeRow>, IdentityError>;
}

/// Fixed rows for tests and harnesses.
#[derive(Debug, Clone, Default)]
pub struct StaticStrikeRows {
    rows: HashMap<AccountId, Vec<StrikeRow>>,
}

impl StaticStrikeRows {
    /// Build a fixed account-to-row table.
    #[must_use]
    pub fn new(rows: impl IntoIterator<Item = (AccountId, Vec<StrikeRow>)>) -> Self {
        Self {
            rows: rows.into_iter().collect(),
        }
    }
}

#[async_trait]
impl StrikeRowSource for StaticStrikeRows {
    async fn rows(&self, account: AccountId) -> Result<Vec<StrikeRow>, IdentityError> {
        Ok(self.rows.get(&account).cloned().unwrap_or_default())
    }
}

/// A read-time scorer suitable for [`crate::IdentityService`].
pub struct ComputedStanding<R, C> {
    rows: R,
    clock: C,
    thresholds: StandingThresholds,
}

impl<R, C> ComputedStanding<R, C> {
    /// Assemble a scorer with an explicit policy package.
    pub const fn new(rows: R, clock: C, thresholds: StandingThresholds) -> Self {
        Self {
            rows,
            clock,
            thresholds,
        }
    }
}

#[async_trait]
impl<R, C> StandingSource for ComputedStanding<R, C>
where
    R: StrikeRowSource,
    C: Fn() -> u64 + Send + Sync,
{
    async fn standing(&self, account: AccountId) -> Result<SessionStanding, IdentityError> {
        let rows = self.rows.rows(account).await?;
        let scores = score_rows(&rows, (self.clock)());
        match self.thresholds.classify(scores.live_milli) {
            StandingLevel::Good => Ok(SessionStanding::Good),
            StandingLevel::Quarantined => Ok(SessionStanding::Quarantined),
            StandingLevel::Cooldown => Err(IdentityError::Cooldown(account)),
            StandingLevel::Banned => Err(IdentityError::Banned(account)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use orrery_persistd::adjudication::{
        StrikeEvidenceRef, StrikeKind, MAJOR_STRIKE_WEIGHT_MILLI, STRIKE_RETENTION_MS,
    };
    use orrery_protocol::{PersistId, RulesetId, Tick};

    const DAY_MS: u64 = 24 * 60 * 60 * 1000;

    fn row(issued_at_ms: u64, mode: StrikeMode) -> StrikeRow {
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

    #[test]
    fn probation_is_a_configured_window_measured_from_the_account_row() {
        let week = DEFAULT_STANDING_THRESHOLDS;
        // Open at both ends of the window, and in the direction that excludes
        // at every ambiguity.
        assert!(
            week.on_probation(0, 0),
            "a brand-new account is on probation"
        );
        assert!(week.on_probation(0, 7 * DAY_MS - 1));
        assert!(
            !week.on_probation(0, 7 * DAY_MS),
            "the boundary instant is the first one past probation"
        );
        assert!(!week.on_probation(0, 30 * DAY_MS));
        assert!(
            week.on_probation(DAY_MS, 0),
            "a row from the future is not an ancient account"
        );

        // The window is a dial, so the same account answers differently under
        // a different configuration — which is the whole of D33 clause (d).
        let day = StandingThresholds {
            probation_ms: DAY_MS,
            ..DEFAULT_STANDING_THRESHOLDS
        };
        assert!(!day.on_probation(0, 3 * DAY_MS));
        assert!(
            !StandingThresholds {
                probation_ms: 0,
                ..DEFAULT_STANDING_THRESHOLDS
            }
            .on_probation(0, 0),
            "a zero window is no probation, not an unbounded one"
        );
    }

    #[test]
    fn decay_is_read_time_with_d33_worked_anchors() {
        let strike = row(0, StrikeMode::Live);
        assert_eq!(
            score_rows(std::slice::from_ref(&strike), 0).live_milli,
            3_000
        );
        assert_eq!(
            score_rows(std::slice::from_ref(&strike), 7 * DAY_MS).live_milli,
            2_122
        );
        assert_eq!(
            score_rows(std::slice::from_ref(&strike), 14 * DAY_MS).live_milli,
            1_500
        );
        assert_eq!(
            score_rows(std::slice::from_ref(&strike), 28 * DAY_MS).live_milli,
            750
        );
    }

    #[test]
    fn two_major_findings_cross_cooldown_within_8_19_days() {
        let now = 20 * DAY_MS;
        // 3.000 now + 3.000 * 2^(-8.19/14) = 5.0004..., rounded up.
        let inside = vec![
            row(now, StrikeMode::Live),
            row(now - 819 * DAY_MS / 100, StrikeMode::Live),
        ];
        let inside_score = score_rows(&inside, now).live_milli;
        assert_eq!(inside_score, 5_000);
        assert_eq!(
            DEFAULT_STANDING_THRESHOLDS.classify(inside_score),
            StandingLevel::Cooldown
        );

        // At 8.20 days the older finding contributes 1.999 points after the
        // conservative ceil, so the sum is 4.999 and remains quarantined.
        let outside = vec![
            row(now, StrikeMode::Live),
            row(now - 820 * DAY_MS / 100, StrikeMode::Live),
        ];
        let outside_score = score_rows(&outside, now).live_milli;
        assert_eq!(outside_score, 4_999);
        assert_eq!(
            DEFAULT_STANDING_THRESHOLDS.classify(outside_score),
            StandingLevel::Quarantined
        );
    }

    #[test]
    fn shadow_rows_are_visible_to_telemetry_and_inert_for_standing() {
        let scores = score_rows(&[row(0, StrikeMode::Shadow)], 0);
        assert_eq!(scores.shadow_milli, 3_000);
        assert_eq!(scores.live_milli, 0);
        assert_eq!(
            DEFAULT_STANDING_THRESHOLDS.classify(scores.live_milli),
            StandingLevel::Good
        );
    }
}
