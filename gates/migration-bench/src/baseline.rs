//! The baseline document: what gets captured, committed, and refused against.
//!
//! A baseline is one suite run plus the environment it ran in, committed to
//! `docs/plans/baselines/` by a human after review. [`BaselineDocument`] is
//! that artifact; [`BaselineDocument::load`] is deliberately strict, because
//! a baseline that parses loosely is a baseline that can be compared against
//! loosely.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::environment::EnvironmentManifest;
use crate::suite::{AbsentLeg, SuiteMetrics, metric_map};

/// The document format version. A baseline written by a different layout
/// fails to load rather than comparing across a schema drift.
pub const SCHEMA: u32 = 1;

/// The programme this baseline belongs to.
pub const PROGRAMME: &str = "a18";

/// One captured baseline: metrics, environment, and the legs recorded absent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BaselineDocument {
    /// The document format version.
    pub schema: u32,
    /// The programme the baseline serves.
    pub programme: String,
    /// When the suite ran, RFC 3339 UTC.
    pub captured_at: String,
    /// The environment the numbers came from.
    pub environment: EnvironmentManifest,
    /// What the suite measured.
    pub metrics: SuiteMetrics,
    /// What the suite could not measure, and why. Presence with a reason is
    /// the honest alternative to an empty number.
    pub absent: Vec<AbsentLeg>,
}

/// Why a baseline document could not be used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BaselineError {
    /// The file could not be read. This is the no-baseline refusal.
    Io(String),
    /// The file exists but is not a usable baseline. Also a refusal — a
    /// comparison against unparseable bytes is still a comparison against
    /// nothing.
    Unusable(String),
}

impl core::fmt::Display for BaselineError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            BaselineError::Io(what) => write!(f, "could not read baseline: {what}"),
            BaselineError::Unusable(what) => write!(f, "baseline is unusable: {what}"),
        }
    }
}

impl BaselineDocument {
    /// Assemble a candidate baseline from a fresh suite run.
    pub fn capture(metrics: SuiteMetrics, absent: Vec<AbsentLeg>) -> Self {
        Self {
            schema: SCHEMA,
            programme: PROGRAMME.to_string(),
            captured_at: rfc3339_now(),
            environment: EnvironmentManifest::capture(),
            metrics,
            absent,
        }
    }

    /// Load and validate a committed baseline. Every failure is a refusal:
    /// the caller must not produce a comparison from a document that did not
    /// fully validate.
    pub fn load(path: &Path) -> Result<Self, BaselineError> {
        let bytes = std::fs::read(path).map_err(|err| BaselineError::Io(err.to_string()))?;
        let doc: Self = serde_json::from_slice(&bytes).map_err(|err| {
            BaselineError::Unusable(format!("not valid JSON for this schema: {err}"))
        })?;
        doc.validate()?;
        Ok(doc)
    }

    /// The structural checks a document must pass before anything compares
    /// against it.
    fn validate(&self) -> Result<(), BaselineError> {
        if self.schema != SCHEMA {
            return Err(BaselineError::Unusable(format!(
                "schema {} != expected {SCHEMA}",
                self.schema
            )));
        }
        if self.programme != PROGRAMME {
            return Err(BaselineError::Unusable(format!(
                "programme '{}' != expected '{PROGRAMME}'",
                self.programme
            )));
        }
        let env = &self.environment;
        for (field, value) in [
            ("rustc_version", &env.rustc_version),
            ("target_triple", &env.target_triple),
            ("build_profile", &env.build_profile),
            ("cpu.model", &env.cpu.model),
        ] {
            if value.is_empty() || value == "unknown" {
                return Err(BaselineError::Unusable(format!(
                    "environment field '{field}' is not recorded ('{value}')"
                )));
            }
        }
        if metric_map(&self.metrics).is_empty() {
            return Err(BaselineError::Unusable(
                "the document carries no metrics".to_string(),
            ));
        }
        Ok(())
    }

    /// The named metrics this baseline carries, in comparison form.
    pub fn metrics(&self) -> std::collections::BTreeMap<String, f64> {
        metric_map(&self.metrics)
    }
}

/// The current time, RFC 3339 UTC, hand-rolled: the harness stays free of a
/// date dependency, and a measurement tool may not need sub-second stamps.
fn rfc3339_now() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rem / 3_600,
        (rem % 3_600) / 60,
        rem % 60
    )
}

/// Days since the Unix epoch to a civil UTC date (Howard Hinnant's algorithm).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}
