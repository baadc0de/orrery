//! Opt-in measurement of replica scope when a target resolves a shot.
//!
//! The accumulator receives decisions made from the coordinator roster which
//! was current at the resolution tick. It never reads or changes the send path:
//! its only inputs are a target-authored verdict, the already-rendered replica
//! scope, and the two authorities' quantized velocities.

use std::collections::BTreeMap;

use serde::Serialize;

use orrery_games::regolith::order::ShotResult;

/// Replica-scope findings for every craft shot resolved during one run.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ShotInterestReport {
    /// The roster cadence whose most recent snapshot supplied each decision.
    pub roster_refresh_hz: u64,
    /// Target-authored shot verdicts observed on player craft.
    pub resolved_shots: u64,
    /// Resolved shots whose attacker was in the victim's replicated scope.
    pub attacker_in_interest: u64,
    /// Resolved shots whose attacker was outside the victim's replicated scope.
    pub attacker_out_of_interest: u64,
    /// Resolved shots which could not be mapped to a directed roster pair.
    ///
    /// Zero means every resolved shot received the requested yes/no answer.
    pub scope_unknown: u64,
    /// `attacker_out_of_interest / resolved_shots`, in 0.0–1.0.
    pub out_of_interest_rate: f64,
    /// Resolved shots where the victim was moving slower than the attacker.
    pub resolved_against_slower_victim: u64,
    /// Out-of-interest shots where the victim was moving slower than the attacker.
    pub out_of_interest_against_slower_victim: u64,
    /// Out-of-interest rate restricted to slower-victim resolutions.
    pub slower_victim_out_of_interest_rate: f64,
    /// Every resolution, grouped by target-authored verdict.
    pub resolved_by_result: BTreeMap<&'static str, u64>,
    /// Out-of-interest resolutions, grouped by target-authored verdict.
    pub out_of_interest_by_result: BTreeMap<&'static str, u64>,
    /// Out-of-interest resolutions, grouped by attacker behaviour.
    pub out_of_interest_by_attacker_profile: BTreeMap<&'static str, u64>,
    /// Authoritative attacker speeds for every out-of-interest resolution.
    pub out_of_interest_attacker_speed: SpeedDistribution,
    /// Out-of-interest resolutions whose attacker velocity was unavailable.
    pub out_of_interest_speed_unknown: u64,
}

/// A distribution over quantized authoritative speeds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SpeedDistribution {
    /// Speeds included in this distribution.
    pub samples: u64,
    /// Width of each histogram bucket.
    pub histogram_bin_mps: u64,
    /// Whole-metre-per-second bucket floor to observations in that bucket.
    pub histogram_mps: BTreeMap<u64, u64>,
    /// Smallest exact quantized speed, in millimetres per second.
    pub min_mms: u64,
    /// Nearest-rank median exact quantized speed, in millimetres per second.
    pub p50_mms: u64,
    /// Nearest-rank 95th percentile exact quantized speed, in millimetres per second.
    pub p95_mms: u64,
    /// Largest exact quantized speed, in millimetres per second.
    pub max_mms: u64,
}

/// Run-local accumulator. Completed output is built once, after simulation.
#[derive(Debug, Default)]
pub(crate) struct ShotInterestStats {
    resolved_shots: u64,
    attacker_in_interest: u64,
    attacker_out_of_interest: u64,
    scope_unknown: u64,
    resolved_against_slower_victim: u64,
    out_of_interest_against_slower_victim: u64,
    resolved_by_result: BTreeMap<&'static str, u64>,
    out_of_interest_by_result: BTreeMap<&'static str, u64>,
    out_of_interest_by_attacker_profile: BTreeMap<&'static str, u64>,
    out_of_interest_speeds_mms: Vec<u64>,
    out_of_interest_speed_unknown: u64,
}

impl ShotInterestStats {
    /// Record one target-authored verdict at its resolution tick.
    pub(crate) fn observe(
        &mut self,
        in_interest: Option<bool>,
        result: ShotResult,
        attacker_profile: Option<&'static str>,
        attacker_speed_mms: Option<u64>,
        victim_speed_mms: u64,
    ) {
        self.resolved_shots += 1;
        increment(&mut self.resolved_by_result, result_name(result));

        let slower_victim = attacker_speed_mms.is_some_and(|speed| victim_speed_mms < speed);
        if slower_victim {
            self.resolved_against_slower_victim += 1;
        }

        match in_interest {
            Some(true) => self.attacker_in_interest += 1,
            Some(false) => {
                self.attacker_out_of_interest += 1;
                increment(&mut self.out_of_interest_by_result, result_name(result));
                if let Some(profile) = attacker_profile {
                    increment(&mut self.out_of_interest_by_attacker_profile, profile);
                }
                if slower_victim {
                    self.out_of_interest_against_slower_victim += 1;
                }
                if let Some(speed) = attacker_speed_mms {
                    self.out_of_interest_speeds_mms.push(speed);
                } else {
                    self.out_of_interest_speed_unknown += 1;
                }
            }
            None => self.scope_unknown += 1,
        }
    }

    /// Finish the deterministic summaries written to JSON.
    pub(crate) fn report(mut self) -> ShotInterestReport {
        self.out_of_interest_speeds_mms.sort_unstable();
        ShotInterestReport {
            roster_refresh_hz: 1,
            resolved_shots: self.resolved_shots,
            attacker_in_interest: self.attacker_in_interest,
            attacker_out_of_interest: self.attacker_out_of_interest,
            scope_unknown: self.scope_unknown,
            out_of_interest_rate: ratio(self.attacker_out_of_interest, self.resolved_shots),
            resolved_against_slower_victim: self.resolved_against_slower_victim,
            out_of_interest_against_slower_victim: self.out_of_interest_against_slower_victim,
            slower_victim_out_of_interest_rate: ratio(
                self.out_of_interest_against_slower_victim,
                self.resolved_against_slower_victim,
            ),
            resolved_by_result: self.resolved_by_result,
            out_of_interest_by_result: self.out_of_interest_by_result,
            out_of_interest_by_attacker_profile: self.out_of_interest_by_attacker_profile,
            out_of_interest_attacker_speed: summarize_speeds(&self.out_of_interest_speeds_mms),
            out_of_interest_speed_unknown: self.out_of_interest_speed_unknown,
        }
    }
}

fn increment(counts: &mut BTreeMap<&'static str, u64>, key: &'static str) {
    *counts.entry(key).or_default() += 1;
}

const fn result_name(result: ShotResult) -> &'static str {
    match result {
        ShotResult::Hit => "hit",
        ShotResult::Miss => "miss",
        ShotResult::OutOfArc => "out_of_arc",
        ShotResult::NoLock => "no_lock",
    }
}

fn ratio(numerator: u64, denominator: u64) -> f64 {
    if denominator == 0 {
        0.0
    } else {
        numerator as f64 / denominator as f64
    }
}

fn summarize_speeds(sorted_mms: &[u64]) -> SpeedDistribution {
    SpeedDistribution {
        samples: sorted_mms.len() as u64,
        histogram_bin_mps: 1,
        histogram_mps: sorted_mms.iter().fold(BTreeMap::new(), |mut bins, speed| {
            *bins.entry(speed / 1_000).or_default() += 1;
            bins
        }),
        min_mms: sorted_mms.first().copied().unwrap_or(0),
        p50_mms: percentile(sorted_mms, 50),
        p95_mms: percentile(sorted_mms, 95),
        max_mms: sorted_mms.last().copied().unwrap_or(0),
    }
}

fn percentile(sorted: &[u64], p: usize) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let index = (sorted.len() * p).div_ceil(100).saturating_sub(1);
    sorted[index.min(sorted.len() - 1)]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_verdict_gets_one_scope_classification() {
        let mut stats = ShotInterestStats::default();
        stats.observe(
            Some(true),
            ShotResult::Hit,
            Some("cruise"),
            Some(32_000),
            20_000,
        );
        stats.observe(
            Some(false),
            ShotResult::Miss,
            Some("burst"),
            Some(480_000),
            0,
        );
        stats.observe(None, ShotResult::OutOfArc, None, None, 0);

        let report = stats.report();
        assert_eq!(report.resolved_shots, 3);
        assert_eq!(report.attacker_in_interest, 1);
        assert_eq!(report.attacker_out_of_interest, 1);
        assert_eq!(report.scope_unknown, 1);
        assert_eq!(
            report.attacker_in_interest + report.attacker_out_of_interest + report.scope_unknown,
            report.resolved_shots
        );
    }

    #[test]
    fn out_of_interest_speed_distribution_keeps_exact_summaries() {
        let mut stats = ShotInterestStats::default();
        for speed in [479_999, 480_000, 480_000, 120_500] {
            stats.observe(Some(false), ShotResult::Hit, Some("burst"), Some(speed), 0);
        }

        let report = stats.report();
        assert_eq!(
            report.out_of_interest_attacker_speed.histogram_mps,
            BTreeMap::from([(120, 1), (479, 1), (480, 2)])
        );
        assert_eq!(report.out_of_interest_attacker_speed.min_mms, 120_500);
        assert_eq!(report.out_of_interest_attacker_speed.p50_mms, 479_999);
        assert_eq!(report.out_of_interest_attacker_speed.p95_mms, 480_000);
        assert_eq!(report.out_of_interest_attacker_speed.max_mms, 480_000);
        assert_eq!(report.out_of_interest_against_slower_victim, 4);
    }
}
