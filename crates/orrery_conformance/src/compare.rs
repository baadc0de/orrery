//! Comparing reports from different platforms.
//!
//! A bare "these hashes differ" is a useless CI failure: it tells you the
//! matrix diverged but not whether you are looking at a one-quantum `libm`
//! difference on a `sin` call or at a rules change that moved a hit point. So
//! a mismatch is localized to the first diverging `(tick, entity)` and, where
//! the state is continuous, quantified against the §5 tolerance bands.
//!
//! The verdict is still failure either way. Tolerance bands are what the
//! *witness* uses to avoid striking an honest player on platform drift; they
//! are not a licence for the corpus to drift. Reporting the magnitude tells
//! whoever reads the log which of the two problems they have.

use orrery_core::tolerance::Tolerance;

use crate::corpus::Report;

/// How two reports differ.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Divergence {
    /// The reports were produced by different corpus or ruleset versions, so
    /// their hashes were never going to match and comparing them says nothing.
    Incomparable {
        /// What went wrong.
        detail: String,
    },
    /// A case's chain hash differs.
    Case {
        /// The case.
        name: String,
        /// First diverging tick, when per-tick detail was available.
        first_tick: Option<u64>,
        /// The entity at that tick.
        entity: Option<u64>,
        /// Largest per-axis position difference in millimetres, from the final
        /// states.
        max_pos_mm: i64,
        /// Largest per-axis velocity difference in millimetres per second.
        max_vel_mms: i64,
        /// Whether any discrete field (hp, shield, heading, RNG fold) differs.
        /// Discrete state has no tolerance band — this is unambiguously a bug.
        discrete_differs: bool,
    },
}

impl std::fmt::Display for Divergence {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Divergence::Incomparable { detail } => write!(f, "incomparable: {detail}"),
            Divergence::Case {
                name,
                first_tick,
                entity,
                max_pos_mm,
                max_vel_mms,
                discrete_differs,
            } => {
                write!(f, "case {name}: chain hashes differ")?;
                match (first_tick, entity) {
                    (Some(t), Some(e)) => write!(f, "; first at tick {t}, entity {e}")?,
                    _ => write!(f, "; no per-tick detail available to localize")?,
                }
                let band = Tolerance::default();
                write!(
                    f,
                    "; final-state delta pos {max_pos_mm} mm (band {} mm), vel {max_vel_mms} mm/s (band {} mm/s)",
                    band.eps_pos_mm, band.eps_vel_mms
                )?;
                if *discrete_differs {
                    write!(
                        f,
                        "; DISCRETE state differs — VC-5 requires bit-exact, this is not platform drift"
                    )?;
                }
                Ok(())
            }
        }
    }
}

/// Compare a report against a baseline.
///
/// Returns every case that diverged. An empty vector is the pass condition.
pub fn compare(baseline: &Report, other: &Report) -> Vec<Divergence> {
    if baseline.schema != other.schema {
        return vec![Divergence::Incomparable {
            detail: format!(
                "schema {} vs {} — regenerate both sides",
                baseline.schema, other.schema
            ),
        }];
    }
    if baseline.ruleset_version != other.ruleset_version
        || baseline.ruleset_digest != other.ruleset_digest
    {
        return vec![Divergence::Incomparable {
            detail: format!(
                "ruleset {}/{} vs {}/{} — different rules cannot be compared",
                baseline.ruleset_version,
                baseline.ruleset_digest,
                other.ruleset_version,
                other.ruleset_digest
            ),
        }];
    }
    if baseline.cases.len() != other.cases.len() {
        return vec![Divergence::Incomparable {
            detail: format!(
                "{} cases vs {} — corpus differs between builds",
                baseline.cases.len(),
                other.cases.len()
            ),
        }];
    }

    let mut out = Vec::new();
    for (a, b) in baseline.cases.iter().zip(&other.cases) {
        if a.name != b.name {
            out.push(Divergence::Incomparable {
                detail: format!("case order differs: {} vs {}", a.name, b.name),
            });
            continue;
        }
        if a.chain == b.chain {
            continue;
        }

        // Localize, when both sides carried per-tick detail.
        let (first_tick, entity) = a
            .tick_hashes
            .iter()
            .zip(&b.tick_hashes)
            .find(|(x, y)| x.hash != y.hash)
            .map(|(x, _)| (Some(x.tick), Some(x.entity)))
            .unwrap_or((None, None));

        // Quantify, from the final states.
        let mut max_pos_mm = 0i64;
        let mut max_vel_mms = 0i64;
        let mut discrete_differs = false;
        for (fa, fb) in a.final_states.iter().zip(&b.final_states) {
            for axis in 0..3 {
                max_pos_mm = max_pos_mm.max((fa.pos_mm[axis] - fb.pos_mm[axis]).abs());
                max_vel_mms = max_vel_mms.max((fa.vel_mms[axis] - fb.vel_mms[axis]).abs());
            }
            if fa.hp != fb.hp
                || fa.shield != fb.shield
                || fa.heading_urad != fb.heading_urad
                || fa.roll_fold != fb.roll_fold
            {
                discrete_differs = true;
            }
        }

        out.push(Divergence::Case {
            name: a.name.clone(),
            first_tick,
            entity,
            max_pos_mm,
            max_vel_mms,
            discrete_differs,
        });
    }
    out
}
