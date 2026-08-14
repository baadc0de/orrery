//! Durable-evidence contract for the P2 kill-9 gate.
//!
//! This module is deliberately storage-agnostic. The future FDB/gateway
//! reader supplies [`RecoveredEvidence`]; the comparison below then proves
//! that every final, durably acknowledged bulk update and every intent
//! outcome survived recovery.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use orrery_protocol::{CellId, GridId, Lsn, PersistId, Tick};

/// An append-only record written only after a gateway acknowledgement.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AckRecord {
    /// A non-provisional bulk acknowledgement: eligible durable evidence.
    Diff(DiffEvidence),
    /// An intent acknowledgement, including the exact protocol outcome.
    Intent {
        /// Decimal u128 idempotency key, lossless in JSON.
        intent_id: String,
        /// Exact known reply outcome. This is intentionally not inferred.
        outcome: IntentOutcomeEvidence,
    },
}

/// The complete client-known identity of a durably acknowledged bulk write.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffEvidence {
    pub grid: GridId,
    pub cell: CellId,
    pub entity: PersistId,
    pub tick: Tick,
    pub lsn: Lsn,
    /// Lowercase BLAKE3 digest of the exact diff payload bytes.
    pub payload_digest: String,
}

/// Wire outcome copied without widening or guessing missing server data.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum IntentOutcomeEvidence {
    Committed { tick: Tick, minted: Vec<PersistId> },
    Rejected { reason: u16 },
}

/// The recovery reader's current materialized bulk state for one entity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveredDiff {
    pub grid: GridId,
    pub cell: CellId,
    pub entity: PersistId,
    pub tick: Tick,
    pub payload_digest: String,
}

/// Storage-reader input to the pure comparator.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct RecoveredEvidence {
    pub diffs: Vec<RecoveredDiff>,
    pub intents: BTreeMap<String, IntentOutcomeEvidence>,
}

/// A mismatch reported by the P2 recovery assertion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EvidenceMismatch {
    MissingBulk {
        grid: GridId,
        entity: PersistId,
    },
    DifferentBulk {
        expected: DiffEvidence,
        actual: RecoveredDiff,
    },
    MissingIntent {
        intent_id: String,
    },
    DifferentIntent {
        intent_id: String,
        expected: IntentOutcomeEvidence,
        actual: IntentOutcomeEvidence,
    },
}

/// Result of comparing an ack log with post-recovery durable state.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct EvidenceReport {
    /// Number of final per-entity bulk writes checked (not every superseded diff).
    pub bulk_checked: usize,
    pub intents_checked: usize,
    pub mismatches: Vec<EvidenceMismatch>,
}

impl EvidenceReport {
    #[must_use]
    pub fn passes(&self) -> bool {
        self.mismatches.is_empty()
    }
}

/// Compare durable acknowledgements to recovered state.
///
/// Bulk state is last-writer-wins, so only the largest acknowledged tick per
/// `(grid, entity)` is asserted. Intent outcomes are idempotency rows and all
/// acknowledged outcomes are asserted exactly. Provisional bulk acks never
/// reach [`AckRecord::Diff`] and therefore cannot enter this proof.
#[must_use]
pub fn compare_recovery(records: &[AckRecord], recovered: &RecoveredEvidence) -> EvidenceReport {
    let mut expected_diffs = BTreeMap::<(GridId, PersistId), DiffEvidence>::new();
    let mut expected_intents = BTreeMap::<String, IntentOutcomeEvidence>::new();
    for record in records {
        match record {
            AckRecord::Diff(diff) => {
                let key = (diff.grid, diff.entity);
                if expected_diffs
                    .get(&key)
                    .is_none_or(|current| diff.tick >= current.tick)
                {
                    expected_diffs.insert(key, diff.clone());
                }
            }
            AckRecord::Intent { intent_id, outcome } => {
                expected_intents.insert(intent_id.clone(), outcome.clone());
            }
        }
    }

    let actual_diffs: BTreeMap<_, _> = recovered
        .diffs
        .iter()
        .cloned()
        .map(|diff| ((diff.grid, diff.entity), diff))
        .collect();
    let mut report = EvidenceReport {
        bulk_checked: expected_diffs.len(),
        intents_checked: expected_intents.len(),
        mismatches: Vec::new(),
    };
    for (key, expected) in expected_diffs {
        match actual_diffs.get(&key) {
            None => report.mismatches.push(EvidenceMismatch::MissingBulk {
                grid: expected.grid,
                entity: expected.entity,
            }),
            Some(actual)
                if actual.cell != expected.cell
                    || actual.tick != expected.tick
                    || actual.payload_digest != expected.payload_digest =>
            {
                report.mismatches.push(EvidenceMismatch::DifferentBulk {
                    expected,
                    actual: actual.clone(),
                });
            }
            Some(_) => {}
        }
    }
    for (intent_id, expected) in expected_intents {
        match recovered.intents.get(&intent_id) {
            None => report
                .mismatches
                .push(EvidenceMismatch::MissingIntent { intent_id }),
            Some(actual) if actual != &expected => {
                report.mismatches.push(EvidenceMismatch::DifferentIntent {
                    intent_id,
                    expected,
                    actual: actual.clone(),
                });
            }
            Some(_) => {}
        }
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;

    fn diff(tick: u64, digest: &str) -> DiffEvidence {
        DiffEvidence {
            grid: GridId::ROOT,
            cell: CellId::ROOT,
            entity: PersistId::new(7),
            tick: Tick::new(tick),
            lsn: Lsn::new(1, tick),
            payload_digest: digest.into(),
        }
    }

    #[test]
    fn checks_final_bulk_write_and_exact_intent_outcome() {
        let records = vec![
            AckRecord::Diff(diff(10, "old")),
            AckRecord::Diff(diff(11, "new")),
            AckRecord::Intent {
                intent_id: "42".into(),
                outcome: IntentOutcomeEvidence::Committed {
                    tick: Tick::new(12),
                    minted: vec![PersistId::new(99)],
                },
            },
        ];
        let recovered = RecoveredEvidence {
            diffs: vec![RecoveredDiff {
                grid: GridId::ROOT,
                cell: CellId::ROOT,
                entity: PersistId::new(7),
                tick: Tick::new(11),
                payload_digest: "new".into(),
            }],
            intents: BTreeMap::from([(
                "42".into(),
                IntentOutcomeEvidence::Committed {
                    tick: Tick::new(12),
                    minted: vec![PersistId::new(99)],
                },
            )]),
        };
        let report = compare_recovery(&records, &recovered);
        assert!(report.passes());
        assert_eq!(report.bulk_checked, 1);
        assert_eq!(report.intents_checked, 1);
    }

    #[test]
    fn reports_digest_and_outcome_regressions() {
        let records = vec![
            AckRecord::Diff(diff(11, "expected")),
            AckRecord::Intent {
                intent_id: "42".into(),
                outcome: IntentOutcomeEvidence::Rejected { reason: 3 },
            },
        ];
        let recovered = RecoveredEvidence {
            diffs: vec![RecoveredDiff {
                grid: GridId::ROOT,
                cell: CellId::ROOT,
                entity: PersistId::new(7),
                tick: Tick::new(11),
                payload_digest: "wrong".into(),
            }],
            intents: BTreeMap::from([("42".into(), IntentOutcomeEvidence::Rejected { reason: 4 })]),
        };
        assert_eq!(compare_recovery(&records, &recovered).mismatches.len(), 2);
    }
}
