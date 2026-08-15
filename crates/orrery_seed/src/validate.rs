//! Scenario validation and human-readable diagnostics.
//!
//! The parser already catches unknown keys and shape mismatches. This module
//! handles the remaining demo-reachable semantic checks before any write:
//! over-constraint on targets, projected FDB limit violations, and the wipe
//! production-guard.

use crate::plan::plan;
use crate::scenario::ResolvedScenario;

/// The verb currently being validated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValidationMode {
    /// `plan`.
    Plan,
    /// `apply`.
    Apply,
    /// `verify`.
    Verify,
    /// `wipe`.
    Wipe,
}

/// A validation failure with a source snippet.
#[derive(Debug, Clone)]
pub struct ValidationError {
    /// The human-readable message.
    pub message: String,
    /// Optional source snippet with a caret.
    pub snippet: Option<String>,
}

impl core::fmt::Display for ValidationError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        writeln!(f, "{}", self.message)?;
        if let Some(snippet) = &self.snippet {
            writeln!(f, "{snippet}")?;
        }
        Ok(())
    }
}

impl core::error::Error for ValidationError {}

/// Validate a resolved scenario against the demo-reachable rules.
pub fn validate(
    source: &str,
    scenario: &ResolvedScenario,
    mode: ValidationMode,
) -> Result<(), ValidationError> {
    let seed_display = String::from_utf8_lossy(&scenario.seed_material).to_string();

    if scenario.raw.target.count.is_some()
        && scenario.raw.target.gini.is_some()
        && scenario.raw.target.occupied_fraction.is_some()
        && scenario.raw.target.solve.is_some()
    {
        return Err(snippet_error(
            source,
            "[target]",
            "target over-constrained: count + gini + occupied_fraction need more than one knob; remove one or add another solve knob (V8)".to_string(),
        ));
    }

    for (name, decl) in &scenario.raw.archetype {
        if let Some(size) = decl.declared_size.as_deref() {
            if let Ok(bytes) = crate::scenario::parse_byte_size(size) {
                if bytes > 100 * 1024 {
                    return Err(snippet_error(
                        source,
                        size,
                        format!(
                            "archetype {name:?} projects a world value of {bytes} B, exceeding the 100 KB hard limit (V9)"
                        ),
                    ));
                }
            }
        }
        if let Some(hex) = decl.bytes.as_deref() {
            let body = hex.strip_prefix("0x").unwrap_or(hex);
            if body.len() / 2 > 100 * 1024 {
                return Err(snippet_error(
                    source,
                    "bytes",
                    format!(
                        "archetype {name:?} hex escape projects a world value above the 100 KB hard limit (V9)"
                    ),
                ));
            }
        }
    }

    let report = plan(scenario, &seed_display);

    if let Some(max) = scenario.raw.limits.max_entities {
        if report.total_entities > max {
            return Err(snippet_error(
                source,
                "max_entities",
                format!(
                    "limits.max_entities = {max} is below the projected {} entities (V10)",
                    report.total_entities
                ),
            ));
        }
    }

    if let Some(max_bytes) = &scenario.raw.limits.max_bytes {
        if let Ok(limit) = crate::scenario::parse_byte_size(max_bytes) {
            if report.total_logical_bytes > limit as u64 {
                return Err(snippet_error(
                    source,
                    "max_bytes",
                    format!(
                        "limits.max_bytes = {max_bytes:?} is below the projected {} logical bytes (V10)",
                        report.total_logical_bytes
                    ),
                ));
            }
        }
    }

    if mode == ValidationMode::Wipe && scenario.raw.limits.protect == Some(true) {
        return Err(snippet_error(
            source,
            "protect",
            "[limits] protect = true refuses wipe until the operator clears the guard (V10)"
                .to_string(),
        ));
    }

    Ok(())
}

fn snippet_error(source: &str, needle: &str, message: String) -> ValidationError {
    ValidationError {
        message,
        snippet: find_snippet(source, needle),
    }
}

fn find_snippet(source: &str, needle: &str) -> Option<String> {
    for (line_no, line) in source.lines().enumerate() {
        let Some(col) = line.find(needle) else {
            continue;
        };
        let width = (line_no + 1).to_string().len();
        let mut snippet = String::new();
        snippet.push_str(&format!(
            "{:>width$} | {}\n",
            line_no + 1,
            line,
            width = width
        ));
        snippet.push_str(&format!(
            "{:>width$} | {:>caret$}^\n",
            "",
            "",
            width = width,
            caret = col + 1
        ));
        return Some(snippet);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scenario::Scenario;

    #[test]
    fn oversize_declared_size_is_rejected_before_plan() {
        let source = r#"
schema = 1

[scenario]
name = "t"

[seed]
scenario = "t"

[payload]
class = "opaque"

[archetype.big]
declared_size = "200KiB"

[[layer]]
name = "l"
kind = "uniform"
bounds = { kind = "box", center = { level = 21, xyz = [0, 0, 0] }, extent_cells = [1, 1, 1] }

[[emit]]
name = "e"
count = 1
archetypes = { big = 1.0 }
"#;
        let scenario = Scenario::parse(source)
            .expect("parses")
            .resolve(b"t".to_vec())
            .expect("resolves");
        let err =
            validate(source, &scenario, ValidationMode::Plan).expect_err("oversize is rejected");
        let msg = err.to_string();
        assert!(msg.contains("200KiB") || msg.contains("204800"), "{msg}");
        assert!(msg.contains("100 KB"), "{msg}");
    }
}
