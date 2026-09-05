//! Read-time scoring for D33's executor-written `ya` strike ledger.
//!
//! This module is deliberately read-only. The row type and key layout come
//! from [`orrery_persistd::adjudication`], whose executor is the only writer;
//! identity receives rows, applies continuous decay at the read instant, and
//! turns the live-only score into the existing token/admission result.

use crate::store::IdentityError;
use async_trait::async_trait;
use orrery_persistd::adjudication::{
    StrikeKind, StrikeMode, StrikeRow, STRIKE_HALF_LIFE_MS, STRIKE_WEIGHT_TABLE_MILLI,
};
use orrery_protocol::AccountId;
use std::collections::HashMap;
use std::fmt;

/// The configured account-policy package: the quarantine/cooldown/ban
/// boundaries in milli-points, intended major-finding count, minimum cooldown,
/// and probation window.
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
    /// Number of major findings by which the operator intends ban to be
    /// reachable, before decay.
    pub intended_major_findings: u32,
    /// Minimum time an account remains in cooldown, in milliseconds.
    pub cooldown_min_ms: u64,
    /// How long after `AccountRow::created_ms` an account remains on
    /// probation, in milliseconds.
    ///
    /// Probation is not a score band, so it is not classified with the other
    /// three: it is a fact about the account row, evaluated at mint time by
    /// [`StandingThresholds::on_probation`] and stamped into the token.
    pub probation_ms: u64,
}

/// D33 clause (d)'s recommended package: 3/5/7, ban intended to be reachable
/// by three major findings, a 14-day minimum cooldown, and 7-day probation.
/// Owner selection can replace this value.
pub const DEFAULT_STANDING_THRESHOLDS: StandingThresholds = StandingThresholds {
    quarantine_milli: 3_000,
    cooldown_milli: 5_000,
    ban_milli: 7_000,
    intended_major_findings: 3,
    cooldown_min_ms: 14 * 24 * 60 * 60 * 1_000,
    probation_ms: 7 * 24 * 60 * 60 * 1_000,
};

/// An incoherent D33 standing-policy package.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StandingThresholdError {
    /// Invariant (i): one maximum-weight finding cannot reach quarantine.
    QuarantineAboveMaximumWeight {
        /// Configured quarantine boundary.
        quarantine_milli: i64,
        /// Maximum entry in D33 clause (a)'s weight table.
        maximum_weight_milli: i64,
    },
    /// Invariant (ii): the three standing boundaries are not strictly ordered.
    BoundariesNotStrictlyOrdered {
        /// Configured quarantine boundary.
        quarantine_milli: i64,
        /// Configured cooldown boundary.
        cooldown_milli: i64,
        /// Configured ban boundary.
        ban_milli: i64,
    },
    /// Invariant (iii): the intended number of maximum-weight findings cannot
    /// reach ban even before decay.
    BanUnreachableByIntendedFindings {
        /// Configured ban boundary.
        ban_milli: i64,
        /// Configured intended number of major findings.
        intended_major_findings: u32,
        /// Maximum entry in D33 clause (a)'s weight table.
        maximum_weight_milli: i64,
        /// Largest score those findings can produce before decay.
        reachable_milli: i64,
    },
    /// Invariant (iv): cooldown has no positive minimum duration.
    CooldownMinimumNotPositive {
        /// Configured minimum cooldown duration.
        cooldown_min_ms: u64,
    },
}

impl fmt::Display for StandingThresholdError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::QuarantineAboveMaximumWeight {
                quarantine_milli,
                maximum_weight_milli,
            } => write!(
                formatter,
                "D33 standing threshold invariant (i) failed: Q={quarantine_milli} milli-points must be <= w_max={maximum_weight_milli} milli-points so one proved major violation quarantines"
            ),
            Self::BoundariesNotStrictlyOrdered {
                quarantine_milli,
                cooldown_milli,
                ban_milli,
            } => write!(
                formatter,
                "D33 standing threshold invariant (ii) failed: Q={quarantine_milli}, C={cooldown_milli}, B={ban_milli} milli-points must satisfy Q < C < B so every standing state is reachable"
            ),
            Self::BanUnreachableByIntendedFindings {
                ban_milli,
                intended_major_findings,
                maximum_weight_milli,
                reachable_milli,
            } => write!(
                formatter,
                "D33 standing threshold invariant (iii) failed: B={ban_milli} milli-points must be <= n_intended={intended_major_findings} * w_max={maximum_weight_milli} milli-points = {reachable_milli} milli-points so ban is reachable by the intended findings"
            ),
            Self::CooldownMinimumNotPositive { cooldown_min_ms } => write!(
                formatter,
                "D33 standing threshold invariant (iv) failed: cooldown_min={cooldown_min_ms} ms must be > 0 so cooldown cannot be left instantly"
            ),
        }
    }
}

impl std::error::Error for StandingThresholdError {}

impl Default for StandingThresholds {
    fn default() -> Self {
        DEFAULT_STANDING_THRESHOLDS
    }
}

impl StandingThresholds {
    /// Validate D33 clause (d)'s four startup invariants.
    ///
    /// # Errors
    ///
    /// Returns the first failed invariant, naming every value involved. A
    /// configuration loader must propagate this error and refuse startup.
    pub fn validate(self) -> Result<(), StandingThresholdError> {
        let maximum_weight_milli = maximum_strike_weight_milli();
        if self.quarantine_milli > maximum_weight_milli {
            return Err(StandingThresholdError::QuarantineAboveMaximumWeight {
                quarantine_milli: self.quarantine_milli,
                maximum_weight_milli,
            });
        }
        if !(self.quarantine_milli < self.cooldown_milli && self.cooldown_milli < self.ban_milli) {
            return Err(StandingThresholdError::BoundariesNotStrictlyOrdered {
                quarantine_milli: self.quarantine_milli,
                cooldown_milli: self.cooldown_milli,
                ban_milli: self.ban_milli,
            });
        }
        let reachable_milli = i64::from(self.intended_major_findings) * maximum_weight_milli;
        if self.ban_milli > reachable_milli {
            return Err(StandingThresholdError::BanUnreachableByIntendedFindings {
                ban_milli: self.ban_milli,
                intended_major_findings: self.intended_major_findings,
                maximum_weight_milli,
                reachable_milli,
            });
        }
        if self.cooldown_min_ms == 0 {
            return Err(StandingThresholdError::CooldownMinimumNotPositive {
                cooldown_min_ms: self.cooldown_min_ms,
            });
        }
        Ok(())
    }

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
/// Positive findings round upward, while negative appeal facts round downward.
///
/// A positive fraction must not make a player safer at a threshold. The same
/// `ceil` rule on a negative fraction would make an upheld appeal *smaller in
/// magnitude* and therefore round against the appellant, so its direction is
/// intentionally the opposite. Keep this sign-aware rule: changing both arms
/// to `ceil` would reintroduce that ledger bias.
#[must_use]
pub fn score_rows(rows: &[StrikeRow], now_ms: u64) -> StandingScores {
    let mut scores = StandingScores::default();
    for row in rows {
        if row.issued_at_ms > now_ms || row.expires_at_ms <= now_ms {
            continue;
        }
        let age_ms = now_ms - row.issued_at_ms;
        let exponent = -(age_ms as f64) / (STRIKE_HALF_LIFE_MS as f64);
        let raw = f64::from(row.weight_milli) * exponent.exp2();
        let contribution = if raw.is_sign_negative() {
            raw.floor() as i64
        } else {
            raw.ceil() as i64
        };
        scores.shadow_milli = scores.shadow_milli.saturating_add(contribution);
        if row.mode == StrikeMode::Live {
            scores.live_milli = scores.live_milli.saturating_add(contribution);
        }
    }
    scores
}

/// The account's ledger with every reversed finding, and every appeal fact,
/// removed — the rows that would have been there had the reversed findings
/// never been filed.
///
/// This is the input to D33 clause (e)'s wrongful-cooldown test (#1083). The
/// dwell floor introduced by #884 is blind to *why* a score fell:
/// [`crate::cooldown`] compares `now` against the durable entry instant and
/// nothing else, so an upheld appeal and fourteen days of decay are
/// indistinguishable at that comparison. Telling them apart needs a second
/// score taken **at the entry instant** over the rows an exoneration leaves
/// standing. If those alone were already at or above `C`, the cooldown was
/// earned by findings nobody reversed and its floor holds; if they were below
/// `C`, the crossing was a consequence of the reversed finding and the floor
/// is void.
///
/// Deliberately read-only, like the rest of this module: it derives a second
/// view of the immutable `ya` rows and writes nothing.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExonerationRescore {
    remaining: Vec<StrikeRow>,
    live_reversals: usize,
}

impl ExonerationRescore {
    /// Pair each [`StrikeKind::Appeal`] row with the positive finding it
    /// compensates, and drop both.
    ///
    /// The pairing identity is `(evidence digest, negated weight, mode)`,
    /// which is exactly what `uphold_appeal` copies onto the compensating row
    /// (`crates/orrery_persistd/src/adjudication.rs:821-830`), and the same
    /// `(digest, Appeal)` identity the ledger deduplicates appeals on
    /// (`adjudication.rs:831-834`). Each appeal consumes at most one original,
    /// so two appeals over one evidence digest — which that deduplication
    /// already refuses — could not reverse a single finding twice here either.
    ///
    /// Unmatched appeals are dropped too: an appeal is a compensating fact
    /// about a finding, never a finding of its own, and leaving one in would
    /// let its negative weight lower the reconstructed entry score.
    #[must_use]
    pub fn from_rows(rows: &[StrikeRow]) -> Self {
        let mut reversed = vec![false; rows.len()];
        let mut live_reversals = 0;
        for appeal in rows.iter().filter(|row| row.kind == StrikeKind::Appeal) {
            let mut matched = None;
            for (index, original) in rows.iter().enumerate() {
                if reversed[index] || original.kind == StrikeKind::Appeal {
                    continue;
                }
                if original.weight_milli > 0
                    && original.mode == appeal.mode
                    && original.evidence_ref.digest == appeal.evidence_ref.digest
                    && appeal.weight_milli.checked_neg() == Some(original.weight_milli)
                {
                    matched = Some(index);
                    break;
                }
            }
            if let Some(index) = matched {
                reversed[index] = true;
                if rows[index].mode == StrikeMode::Live {
                    live_reversals += 1;
                }
            }
        }
        let remaining = rows
            .iter()
            .enumerate()
            .filter(|(index, row)| !reversed[*index] && row.kind != StrikeKind::Appeal)
            .map(|(_, row)| row.clone())
            .collect();
        Self {
            remaining,
            live_reversals,
        }
    }

    /// How many *live* findings upheld appeals reversed.
    ///
    /// A shadow pair never entered the enforcement score, so reversing one
    /// cannot have caused a cooldown and does not count here.
    #[must_use]
    pub const fn live_reversals(&self) -> usize {
        self.live_reversals
    }

    /// Score the surviving rows at an arbitrary instant.
    ///
    /// [`score_rows`] already skips a row whose `issued_at_ms` is after the
    /// read instant, so scoring at a past instant reconstructs the ledger as
    /// it stood then rather than as it stands now.
    #[must_use]
    pub fn score_at(&self, at_ms: u64) -> StandingScores {
        score_rows(&self.remaining, at_ms)
    }

    /// Whether a cooldown entered at `entered_at_ms` was a consequence of a
    /// reversed finding, and therefore carries no clause (d) minimum-cooldown
    /// floor.
    ///
    /// Requires at least one reversed *live* finding, not merely a
    /// reconstructed score below `C`. Without that guard a ledger holding no
    /// appeals at all — or one whose rows have aged out of their 90-day
    /// retention — would reconstruct an entry score of zero and void every
    /// dwell, which is precisely the decay hole #884 closed.
    #[must_use]
    pub fn voids_dwell(&self, thresholds: StandingThresholds, entered_at_ms: u64) -> bool {
        self.live_reversals > 0
            && self.score_at(entered_at_ms).live_milli < thresholds.cooldown_milli
    }
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

/// One read of the executor-owned strike ledger.
///
/// This is deliberately only the score and facts derived from the immutable
/// rows. Applying the cooldown dwell policy, including its durable mutation,
/// belongs to [`crate::CooldownStanding`], not this read-only module.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StandingObservation {
    /// The wall-clock instant at which the rows were scored.
    pub now_ms: u64,
    /// The live and shadow scores at [`Self::now_ms`].
    pub scores: StandingScores,
    /// The instantaneous score band before the cooldown dwell rule.
    pub level: StandingLevel,
    /// The newest active, positive, live strike observed in this read.
    ///
    /// An appeal has a negative weight and cannot restart a cooldown. A row
    /// outside its live scoring interval cannot either.
    pub newest_live_strike_ms: Option<u64>,
    /// The same rows with every reversed finding and every appeal removed,
    /// carried so the dwell rule can re-score them at the durable entry
    /// instant rather than at [`Self::now_ms`].
    ///
    /// It travels with the observation rather than being derived later: the
    /// entry instant is only known after the `dc` row is read, which happens
    /// in [`crate::cooldown`], and re-reading the ledger there would score the
    /// two halves of one admission decision against two different snapshots.
    pub exoneration: ExonerationRescore,
}

impl<R, C> ComputedStanding<R, C> {
    /// Assemble a scorer with an explicit policy package.
    ///
    /// # Errors
    ///
    /// Refuses an incoherent package under D33 clause (d)'s four startup
    /// invariants.
    pub fn new(
        rows: R,
        clock: C,
        thresholds: StandingThresholds,
    ) -> Result<Self, StandingThresholdError> {
        thresholds.validate()?;
        Ok(Self {
            rows,
            clock,
            thresholds,
        })
    }

    /// Read and score one account's immutable ledger rows.
    ///
    /// No durable state changes here. In particular, the result does not
    /// itself apply the minimum-cooldown dwell rule; use
    /// [`crate::CooldownStanding`] for the admission decision.
    pub async fn observe(&self, account: AccountId) -> Result<StandingObservation, IdentityError>
    where
        R: StrikeRowSource,
        C: Fn() -> u64,
    {
        let rows = self.rows.rows(account).await?;
        let now_ms = (self.clock)();
        let scores = score_rows(&rows, now_ms);
        let newest_live_strike_ms = rows
            .iter()
            .filter(|row| {
                row.mode == StrikeMode::Live
                    && row.weight_milli > 0
                    && row.issued_at_ms <= now_ms
                    && row.expires_at_ms > now_ms
            })
            .map(|row| row.issued_at_ms)
            .max();
        Ok(StandingObservation {
            now_ms,
            scores,
            level: self.thresholds.classify(scores.live_milli),
            newest_live_strike_ms,
            exoneration: ExonerationRescore::from_rows(&rows),
        })
    }

    /// The policy package used to classify observations.
    #[must_use]
    pub const fn thresholds(&self) -> StandingThresholds {
        self.thresholds
    }
}

fn maximum_strike_weight_milli() -> i64 {
    STRIKE_WEIGHT_TABLE_MILLI
        .iter()
        .copied()
        .max()
        .map_or(0, i64::from)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::service::StandingSource;
    use crate::AccountStore;
    use orrery_persistd::adjudication::{
        StrikeEvidenceRef, StrikeKind, MAJOR_STRIKE_WEIGHT_MILLI, STRIKE_RETENTION_MS,
    };
    use orrery_protocol::SessionStanding;
    use orrery_protocol::{PersistId, RulesetId, Tick};
    use std::sync::Arc;

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
    fn default_thresholds_satisfy_all_four_startup_invariants() {
        DEFAULT_STANDING_THRESHOLDS
            .validate()
            .expect("D33's accepted default package must start");
    }

    #[test]
    fn invariant_i_names_quarantine_and_weight_table_maximum() {
        let thresholds = StandingThresholds {
            quarantine_milli: 3_001,
            ..DEFAULT_STANDING_THRESHOLDS
        };
        let error = thresholds.validate().expect_err("Q above w_max must fail");
        assert_eq!(
            error,
            StandingThresholdError::QuarantineAboveMaximumWeight {
                quarantine_milli: 3_001,
                maximum_weight_milli: 3_000,
            }
        );
        assert_eq!(
            error.to_string(),
            "D33 standing threshold invariant (i) failed: Q=3001 milli-points must be <= w_max=3000 milli-points so one proved major violation quarantines"
        );
    }

    #[test]
    fn invariant_ii_names_all_three_unordered_boundaries() {
        let thresholds = StandingThresholds {
            cooldown_milli: 3_000,
            ..DEFAULT_STANDING_THRESHOLDS
        };
        let error = thresholds
            .validate()
            .expect_err("equal Q and C must fail strict ordering");
        assert_eq!(
            error,
            StandingThresholdError::BoundariesNotStrictlyOrdered {
                quarantine_milli: 3_000,
                cooldown_milli: 3_000,
                ban_milli: 7_000,
            }
        );
        assert!(error.to_string().contains("Q=3000, C=3000, B=7000"));
    }

    #[test]
    fn legacy_three_six_ten_is_rejected_by_invariant_iii_at_three_findings() {
        let legacy = StandingThresholds {
            quarantine_milli: 3_000,
            cooldown_milli: 6_000,
            ban_milli: 10_000,
            intended_major_findings: 3,
            ..DEFAULT_STANDING_THRESHOLDS
        };
        let error = ComputedStanding::new(StaticStrikeRows::default(), || 0, legacy)
            .err()
            .expect("startup must refuse a ban unreachable by intended findings");
        assert_eq!(
            error,
            StandingThresholdError::BanUnreachableByIntendedFindings {
                ban_milli: 10_000,
                intended_major_findings: 3,
                maximum_weight_milli: 3_000,
                reachable_milli: 9_000,
            }
        );
        assert!(error.to_string().contains(
            "invariant (iii) failed: B=10000 milli-points must be <= n_intended=3 * w_max=3000 milli-points = 9000 milli-points"
        ));
    }

    #[test]
    fn invariant_iv_names_the_zero_cooldown() {
        let thresholds = StandingThresholds {
            cooldown_min_ms: 0,
            ..DEFAULT_STANDING_THRESHOLDS
        };
        let error = thresholds
            .validate()
            .expect_err("an instantaneous cooldown must fail");
        assert_eq!(
            error,
            StandingThresholdError::CooldownMinimumNotPositive { cooldown_min_ms: 0 }
        );
        assert!(error
            .to_string()
            .contains("invariant (iv) failed: cooldown_min=0 ms must be > 0"));
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

    #[test]
    fn upheld_appeal_moves_an_account_out_of_quarantine_on_recomputation() {
        let account = AccountId::new(77);
        let strike = row(0, StrikeMode::Live);
        let mut appeal = strike.clone();
        appeal.kind = StrikeKind::Appeal;
        appeal.weight_milli = -strike.weight_milli;
        appeal.issued_at_ms = 1;
        appeal.expires_at_ms = STRIKE_RETENTION_MS + 1;

        assert_eq!(
            DEFAULT_STANDING_THRESHOLDS
                .classify(score_rows(std::slice::from_ref(&strike), 1).live_milli),
            StandingLevel::Quarantined,
            "without the compensating fact the original finding still quarantines"
        );
        let scores = score_rows(&[strike, appeal], 1);
        assert_eq!(scores.live_milli, 0);
        assert_eq!(
            DEFAULT_STANDING_THRESHOLDS.classify(scores.live_milli),
            StandingLevel::Good,
            "the compensating fact changes the derived classification"
        );

        // And the same reversal is observable through the scorer the identity
        // service actually mints from, not only through `score_rows`.
        let store = Arc::new(crate::MemAccountStore::new());
        futures::executor::block_on(store.create_account(account, 0)).expect("create account");
        let convicted = ComputedStanding::new(
            StaticStrikeRows::new([(account, vec![row(0, StrikeMode::Live)])]),
            || 1,
            DEFAULT_STANDING_THRESHOLDS,
        )
        .expect("default thresholds are coherent");
        assert_ne!(
            futures::executor::block_on(convicted.standing(account, store.as_ref())),
            Ok(SessionStanding::Good),
            "the unappealed conviction is not Good"
        );

        let appealed = ComputedStanding::new(
            StaticStrikeRows::new([(
                account,
                vec![row(0, StrikeMode::Live), {
                    let mut appeal = row(0, StrikeMode::Live);
                    appeal.kind = StrikeKind::Appeal;
                    appeal.weight_milli = -MAJOR_STRIKE_WEIGHT_MILLI;
                    appeal.issued_at_ms = 1;
                    appeal.expires_at_ms = STRIKE_RETENTION_MS + 1;
                    appeal
                }],
            )]),
            || 1,
            DEFAULT_STANDING_THRESHOLDS,
        )
        .expect("default thresholds are coherent");
        assert_eq!(
            futures::executor::block_on(appealed.standing(account, store.as_ref())),
            Ok(SessionStanding::Good),
            "recomputation after the appeal restores the account"
        );
    }

    #[test]
    fn negative_appeal_contributions_round_away_from_zero_not_against_the_appellant() {
        let mut appeal = row(0, StrikeMode::Live);
        appeal.kind = StrikeKind::Appeal;
        appeal.weight_milli = -1_001;
        // At one half-life this is -500.5, so ceil would produce -500 and
        // silently leave 1 milli-point of the reversed finding behind.
        assert_eq!(score_rows(&[appeal], 14 * DAY_MS).live_milli, -501);

        let mut fractional = row(0, StrikeMode::Live);
        fractional.kind = StrikeKind::Appeal;
        fractional.weight_milli = -1_001;
        assert_eq!(score_rows(&[fractional], 7 * DAY_MS).live_milli, -708);
    }
}
