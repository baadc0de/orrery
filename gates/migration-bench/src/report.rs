//! The differential report: ratios, and nothing but ratios.
//!
//! Benches observe; gates refuse. The refusal half lives in the entry point
//! (no committed baseline, or a mismatched environment); this module is the
//! observe half, and it contains no threshold, no pass, no fail — those are
//! evaluated by a human at phase exit, against A10 §8.4's proposed values,
//! never asserted by a machine that was not asked to hold them.

use serde::{Deserialize, Serialize};

use crate::environment::EnvironmentManifest;

/// One metric's baseline value, candidate value, and ratio.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MetricComparison {
    /// The metric, by its dotted path (`b1.corpus.combat-island.tick_us_p99`).
    pub metric: String,
    /// What the committed baseline recorded. `None` when the candidate
    /// measured something the baseline does not carry.
    pub baseline: Option<f64>,
    /// What this run measured. `None` when the baseline carries something the
    /// candidate no longer measures.
    pub candidate: Option<f64>,
    /// candidate / baseline, where both exist and the baseline is non-zero.
    pub ratio: Option<f64>,
}

/// The full comparison artifact.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CompareReport {
    /// The report format version.
    pub schema: u32,
    /// Where the baseline came from.
    pub baseline_path: String,
    /// The baseline's environment, verbatim.
    pub baseline_environment: EnvironmentManifest,
    /// This run's environment, verbatim.
    pub candidate_environment: EnvironmentManifest,
    /// The standing statement of what this report does not do. It is a field
    /// so that a reader who never opens the README still cannot mistake a
    /// ratio for a gate.
    pub thresholds: String,
    /// Metrics present in both baseline and candidate, sorted by name.
    pub comparisons: Vec<MetricComparison>,
    /// Metrics the candidate measures that the baseline does not carry.
    pub not_in_baseline: Vec<String>,
    /// Metrics the baseline carries that the candidate no longer measures.
    pub not_in_candidate: Vec<String>,
}

/// The sentence every report carries.
pub const THRESHOLD_STATEMENT: &str = "none — benches observe; gates refuse. Ratios are for human evaluation at phase exit \
     (A10 §8.1, §8.4); nothing here asserts a threshold.";

/// Build the report from the two metric maps. Baseline metrics the candidate
/// no longer measures are listed, not dropped, and vice versa.
pub fn build(
    baseline_path: &str,
    baseline_environment: EnvironmentManifest,
    candidate_environment: EnvironmentManifest,
    baseline: &std::collections::BTreeMap<String, f64>,
    candidate: &std::collections::BTreeMap<String, f64>,
) -> CompareReport {
    let mut comparisons = Vec::new();
    let mut not_in_baseline = Vec::new();
    let mut not_in_candidate = Vec::new();
    for (metric, candidate_value) in candidate {
        match baseline.get(metric) {
            Some(baseline_value) => {
                let ratio = if *baseline_value != 0.0 {
                    Some(candidate_value / baseline_value)
                } else {
                    None
                };
                comparisons.push(MetricComparison {
                    metric: metric.clone(),
                    baseline: Some(*baseline_value),
                    candidate: Some(*candidate_value),
                    ratio,
                });
            }
            None => not_in_baseline.push(metric.clone()),
        }
    }
    for metric in baseline.keys() {
        if !candidate.contains_key(metric) {
            not_in_candidate.push(metric.clone());
        }
    }
    comparisons.sort_by(|a, b| a.metric.cmp(&b.metric));
    CompareReport {
        schema: 1,
        baseline_path: baseline_path.to_string(),
        baseline_environment,
        candidate_environment,
        thresholds: THRESHOLD_STATEMENT.to_string(),
        comparisons,
        not_in_baseline,
        not_in_candidate,
    }
}
