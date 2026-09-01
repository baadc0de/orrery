//! The daily full conservation sweep over archived economic receipts (#615).
//!
//! This is D32 clause (g)'s archive-consuming half. Unlike the hourly
//! [`super::HotLedgerSweeper`], it trusts no incremental cursor: every pass
//! captures the receipt Parquet objects visible at its start and rechecks that
//! complete archive window. Objects published concurrently are atomic and are
//! picked up by the next pass.
//!
//! Shape C (#832, #841) makes this possible without an identity join. Receipt
//! effects already name [`AccountId`] and [`ItemUid`], and their FDB commit
//! versionstamp is the history order. The price is the layout mismatch: objects
//! are ordered by commit, while ownership continuity is grouped by item. The
//! balance fold streams, but ownership transitions must be retained and
//! re-sorted by `(ItemUid, receipt key, transition ordinal)`. Every report
//! carries the exact bytes scanned and the sort vector's memory bound.

use std::collections::BTreeMap;
use std::mem::size_of;
use std::path::{Component, Path, PathBuf};

use orrery_protocol::{AccountId, AssetId, ItemUid};
use serde::{Deserialize, Serialize};

use crate::archive::decode_receipt_object;
use crate::intent::{LEDGER_CREDIT_OP, LEDGER_ITEM_TRANSFER_OP};
use crate::keyspace::{ReceiptBalanceDelta, ReceiptRow};

use super::{hex, AuditFinding, FindingKind};

/// Report schema for the archive-consuming sweep.
pub const FULL_SWEEP_REPORT_SCHEMA: &str = "orrery.audit.full-conservation/1";

/// The clause-(g) starting cadence: one complete archive pass per day.
pub const DEFAULT_FULL_SWEEP_INTERVAL_MS: u64 = 86_400_000;

/// Exact population and I/O denominators for one full pass.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FullSweepPopulation {
    /// Parquet objects captured when the pass began.
    pub objects: u64,
    /// Exact on-disk Parquet bytes read.
    pub object_bytes: u64,
    /// Decoded receipt rows, including legacy v0 rows.
    pub receipts: u64,
    /// Legacy rows whose effect vectors are necessarily empty.
    pub legacy_receipts: u64,
    /// Signed balance effects folded.
    pub balance_deltas: u64,
    /// Ownership transitions retained for the item re-sort.
    pub ownership_transitions: u64,
}

/// The measured layout cost of re-grouping commit-ordered ownership history.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OwnershipResortCost {
    /// Events sorted by item and commit order.
    pub entries: u64,
    /// Inline bytes per retained event in this build.
    pub bytes_per_entry: u64,
    /// Exact in-place sort-vector bound after `shrink_to_fit`.
    pub memory_bytes_bound: u64,
}

/// One side-by-side per-asset conservation sum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssetConservation {
    /// Asset being reconciled.
    pub asset: AssetId,
    /// Sum of every archived signed balance delta in the window.
    pub observed_delta: i128,
    /// Sum attributable to receipted source/sink ops in the same window.
    /// Transfers contribute zero; credits and compensating annulments carry
    /// their explicit external delta.
    pub receipted_ops_delta: i128,
}

/// What one complete archive pass observed and concluded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FullSweepReport {
    /// Stable schema identifier.
    pub schema: String,
    /// Caller-supplied wall clock at pass start; receipt keys carry ordering.
    pub started_at_ms: u64,
    /// Caller-supplied wall clock at pass completion.
    pub finished_at_ms: u64,
    /// First receipt key in the captured window, as lowercase hex.
    pub first_receipt_key: String,
    /// Last receipt key in the captured window, as lowercase hex.
    pub last_receipt_key: String,
    /// Scan denominators. A pass with zero objects or rows is an error instead.
    pub population: FullSweepPopulation,
    /// The in-memory item-history regrouping cost.
    pub ownership_resort: OwnershipResortCost,
    /// Deterministic per-asset sums, ordered by asset id.
    pub assets: Vec<AssetConservation>,
    /// Every conservation, continuity, or receipt-shape finding.
    pub findings: Vec<AuditFinding>,
}

impl FullSweepReport {
    /// Render the report as pretty newline-terminated JSON.
    ///
    /// # Errors
    ///
    /// Propagates any JSON serialization failure.
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        let mut rendered = serde_json::to_string_pretty(self)?;
        rendered.push('\n');
        Ok(rendered)
    }
}

/// A read, decode, ordering, or arithmetic failure in a full pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FullSweepError(pub String);

impl std::fmt::Display for FullSweepError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for FullSweepError {}

/// Read-only full sweep over the filesystem receipt-archive backend.
#[derive(Debug, Clone)]
pub struct ReceiptArchiveSweeper {
    archive_root: PathBuf,
    archive_prefix: String,
}

impl ReceiptArchiveSweeper {
    /// Point a sweeper at the same root/prefix as the receipt archive tailer.
    ///
    /// # Errors
    ///
    /// Returns [`FullSweepError`] when `archive_prefix` is absolute or contains
    /// a component that can escape `archive_root`.
    pub fn open(
        archive_root: impl Into<PathBuf>,
        archive_prefix: impl Into<String>,
    ) -> Result<Self, FullSweepError> {
        let archive_prefix = archive_prefix.into();
        validate_prefix(&archive_prefix)?;
        Ok(Self {
            archive_root: archive_root.into(),
            archive_prefix,
        })
    }

    /// Run one full pass over the receipt objects visible when this call begins.
    ///
    /// An empty object set or an object set decoding to zero receipts is an
    /// error. That is deliberate: `0 rows; 0 findings` is not conservation
    /// evidence and must never be reported as a healthy pass.
    ///
    /// Findings are returned and emitted on the shared `orrery_audit` target.
    ///
    /// # Errors
    ///
    /// Returns [`FullSweepError`] for directory reads, object reads, Parquet
    /// decoding, non-monotone/duplicate receipt keys, or sum overflow.
    pub fn run_pass(
        &self,
        started_at_ms: u64,
        finished_at_ms: u64,
    ) -> Result<FullSweepReport, FullSweepError> {
        let objects = self.capture_objects()?;
        if objects.is_empty() {
            return Err(FullSweepError(
                "full conservation sweep captured zero receipt archive objects".to_owned(),
            ));
        }

        let mut population = FullSweepPopulation {
            objects: objects.len() as u64,
            ..FullSweepPopulation::default()
        };
        let mut observed = BTreeMap::<AssetId, i128>::new();
        let mut receipted = BTreeMap::<AssetId, i128>::new();
        let mut ownership = Vec::<OwnershipEvent>::new();
        let mut findings = Vec::new();
        let mut first_key = None;
        let mut previous_key = None;

        for path in objects {
            let bytes = std::fs::read(&path).map_err(|error| {
                FullSweepError(format!("read receipt object {}: {error}", path.display()))
            })?;
            population.object_bytes = checked_u64_add(
                population.object_bytes,
                bytes.len() as u64,
                "object byte count",
            )?;
            let rows = decode_receipt_object(&bytes).map_err(|error| {
                FullSweepError(format!("decode receipt object {}: {error}", path.display()))
            })?;
            for row in rows {
                if previous_key.is_some_and(|previous| row.key <= previous) {
                    return Err(FullSweepError(format!(
                        "receipt archive keys are not strictly increasing: {} after {}",
                        hex(&row.key),
                        previous_key.map_or_else(String::new, |key| hex(&key))
                    )));
                }
                first_key.get_or_insert(row.key);
                previous_key = Some(row.key);
                population.receipts = checked_u64_add(population.receipts, 1, "receipt count")?;
                if row.encoding == orrery_protocol::atrest::ENCODING_V0 {
                    population.legacy_receipts =
                        checked_u64_add(population.legacy_receipts, 1, "legacy receipt count")?;
                }
                population.balance_deltas = checked_u64_add(
                    population.balance_deltas,
                    row.receipt.balance_deltas.len() as u64,
                    "balance delta count",
                )?;
                population.ownership_transitions = checked_u64_add(
                    population.ownership_transitions,
                    row.receipt.ownership.len() as u64,
                    "ownership transition count",
                )?;
                fold_receipt(
                    row.key,
                    &row.receipt,
                    &mut observed,
                    &mut receipted,
                    &mut ownership,
                    &mut findings,
                )?;
            }
        }

        let Some(last_key) = previous_key else {
            return Err(FullSweepError(
                "full conservation sweep decoded zero receipt rows".to_owned(),
            ));
        };
        let first_key = first_key.expect("last receipt implies first receipt");

        let mut all_assets = observed
            .keys()
            .chain(receipted.keys())
            .copied()
            .collect::<Vec<_>>();
        all_assets.sort_unstable_by_key(|asset| asset.0);
        all_assets.dedup();
        let assets = all_assets
            .into_iter()
            .map(|asset| AssetConservation {
                asset,
                observed_delta: observed.get(&asset).copied().unwrap_or(0),
                receipted_ops_delta: receipted.get(&asset).copied().unwrap_or(0),
            })
            .collect::<Vec<_>>();
        for asset in &assets {
            if asset.observed_delta != asset.receipted_ops_delta {
                findings.push(AuditFinding {
                    kind: FindingKind::GlobalConservationBreak,
                    item: None,
                    account: None,
                    asset: Some(asset.asset),
                    receipt_intent_id: None,
                    key_hex: hex(&last_key),
                    detail: format!(
                        "asset {} changed by {} across the full archive window, but receipted \
                         source/sink ops account for {}; unexplained delta {}",
                        asset.asset.0,
                        asset.observed_delta,
                        asset.receipted_ops_delta,
                        asset.observed_delta - asset.receipted_ops_delta
                    ),
                });
            }
        }

        ownership.shrink_to_fit();
        let ownership_resort = OwnershipResortCost {
            entries: ownership.len() as u64,
            bytes_per_entry: size_of::<OwnershipEvent>() as u64,
            memory_bytes_bound: (ownership.capacity() as u64)
                .checked_mul(size_of::<OwnershipEvent>() as u64)
                .ok_or_else(|| {
                    FullSweepError("ownership re-sort byte bound overflow".to_owned())
                })?,
        };
        ownership.sort_unstable_by(|left, right| {
            (left.item.0, left.receipt_key, left.ordinal).cmp(&(
                right.item.0,
                right.receipt_key,
                right.ordinal,
            ))
        });
        findings.extend(ownership_findings(&ownership));

        for finding in &findings {
            finding.emit();
        }

        Ok(FullSweepReport {
            schema: FULL_SWEEP_REPORT_SCHEMA.to_owned(),
            started_at_ms,
            finished_at_ms,
            first_receipt_key: hex(&first_key),
            last_receipt_key: hex(&last_key),
            population,
            ownership_resort,
            assets,
            findings,
        })
    }

    fn capture_objects(&self) -> Result<Vec<PathBuf>, FullSweepError> {
        let mut directory = self.archive_root.clone();
        for component in Path::new(self.archive_prefix.trim_end_matches('/')).components() {
            if let Component::Normal(part) = component {
                directory.push(part);
            }
        }
        directory.push("rarchive");
        let entries = std::fs::read_dir(&directory).map_err(|error| {
            FullSweepError(format!(
                "read receipt archive directory {}: {error}",
                directory.display()
            ))
        })?;
        let mut objects = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|error| {
                FullSweepError(format!(
                    "read receipt archive entry in {}: {error}",
                    directory.display()
                ))
            })?;
            let path = entry.path();
            if path
                .extension()
                .is_some_and(|extension| extension == "parquet")
            {
                objects.push(path);
            }
        }
        objects.sort();
        Ok(objects)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct OwnershipEvent {
    item: ItemUid,
    before: Option<AccountId>,
    after: Option<AccountId>,
    receipt_key: [u8; 12],
    ordinal: u32,
}

fn validate_prefix(prefix: &str) -> Result<(), FullSweepError> {
    for component in Path::new(prefix).components() {
        if !matches!(component, Component::Normal(_)) {
            return Err(FullSweepError(format!(
                "archive prefix {prefix:?} is not a plain relative path"
            )));
        }
    }
    Ok(())
}

fn fold_receipt(
    key: [u8; 12],
    receipt: &ReceiptRow,
    observed: &mut BTreeMap<AssetId, i128>,
    receipted: &mut BTreeMap<AssetId, i128>,
    ownership: &mut Vec<OwnershipEvent>,
    findings: &mut Vec<AuditFinding>,
) -> Result<(), FullSweepError> {
    for delta in &receipt.balance_deltas {
        add_delta(observed, delta)?;
    }

    if receipt.ops.is_empty() {
        // The only landed mutation with no op ids is a compensating annulment.
        // Its explicit inverse effects are a sanctioned sink/source reversal.
        for delta in &receipt.balance_deltas {
            add_delta(receipted, delta)?;
        }
    } else {
        let mut next_delta = 0_usize;
        for op in &receipt.ops {
            match *op {
                LEDGER_CREDIT_OP => {
                    let Some(delta) = receipt.balance_deltas.get(next_delta) else {
                        findings.push(effect_shape_finding(
                            key,
                            receipt,
                            "credit op has no corresponding balance delta",
                        ));
                        break;
                    };
                    add_delta(receipted, delta)?;
                    next_delta += 1;
                }
                LEDGER_ITEM_TRANSFER_OP => {
                    if receipt.balance_deltas.len().saturating_sub(next_delta) < 2 {
                        findings.push(effect_shape_finding(
                            key,
                            receipt,
                            "item transfer op has fewer than two balance deltas",
                        ));
                        next_delta = receipt.balance_deltas.len();
                        break;
                    }
                    next_delta += 2;
                }
                _ => {}
            }
        }
        if next_delta != receipt.balance_deltas.len() {
            findings.push(effect_shape_finding(
                key,
                receipt,
                "interpreted op ids do not consume the complete balance effect vector",
            ));
        }
    }

    for (ordinal, transition) in receipt.ownership.iter().enumerate() {
        ownership.push(OwnershipEvent {
            item: transition.item,
            before: transition.before,
            after: transition.after,
            receipt_key: key,
            ordinal: u32::try_from(ordinal)
                .map_err(|_| FullSweepError("receipt ownership ordinal exceeds u32".to_owned()))?,
        });
    }
    Ok(())
}

fn add_delta(
    totals: &mut BTreeMap<AssetId, i128>,
    delta: &ReceiptBalanceDelta,
) -> Result<(), FullSweepError> {
    let total = totals.entry(delta.asset).or_insert(0);
    *total = total
        .checked_add(i128::from(delta.delta))
        .ok_or_else(|| FullSweepError(format!("asset {} delta sum overflow", delta.asset.0)))?;
    Ok(())
}

fn effect_shape_finding(key: [u8; 12], receipt: &ReceiptRow, reason: &str) -> AuditFinding {
    AuditFinding {
        kind: FindingKind::ReceiptEffectShapeMismatch,
        item: None,
        account: None,
        asset: None,
        receipt_intent_id: Some(receipt.intent_id),
        key_hex: hex(&key),
        detail: format!(
            "receipt for intent {} has {} op ids and {} balance deltas: {reason}",
            receipt.intent_id,
            receipt.ops.len(),
            receipt.balance_deltas.len()
        ),
    }
}

fn ownership_findings(events: &[OwnershipEvent]) -> Vec<AuditFinding> {
    let mut findings = Vec::new();
    let mut index = 0;
    while index < events.len() {
        let item = events[index].item;
        let mut end = index + 1;
        while end < events.len() && events[end].item == item {
            end += 1;
        }
        let history = &events[index..end];
        let mut current = history[0].after;
        for event in &history[1..] {
            match (current, event.before) {
                (Some(expected), Some(stated)) if expected == stated => {}
                (None, None) => {}
                (None, Some(stated)) => findings.push(AuditFinding {
                    kind: FindingKind::UnreceiptedItemOwnership,
                    item: Some(item),
                    account: Some(stated),
                    asset: None,
                    receipt_intent_id: None,
                    key_hex: hex(&event.receipt_key),
                    detail: format!(
                        "item {} was burned, then receipt {} claimed prior owner {} without an \
                         intervening None -> Some mint",
                        item.0,
                        hex(&event.receipt_key),
                        stated.0
                    ),
                }),
                (Some(expected), stated) => findings.push(AuditFinding {
                    kind: FindingKind::OverlappingItemOwnership,
                    item: Some(item),
                    account: Some(expected),
                    asset: None,
                    receipt_intent_id: None,
                    key_hex: hex(&event.receipt_key),
                    detail: format!(
                        "item {} remained owned by account {}, but the next transition named \
                         prior owner {}; the preceding ownership interval never closed",
                        item.0,
                        expected.0,
                        stated.map_or_else(|| "None".to_owned(), |owner| owner.0.to_string())
                    ),
                }),
            }
            current = event.after;
        }
        index = end;
    }
    findings
}

fn checked_u64_add(left: u64, right: u64, label: &str) -> Result<u64, FullSweepError> {
    left.checked_add(right)
        .ok_or_else(|| FullSweepError(format!("{label} overflow")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::archive::{encode_receipt_object, ReceiptArchiveRow};
    use crate::audit::{evaluate_pass, BalanceRow, LedgerWalk};
    use crate::keyspace::{ReceiptOwnershipTransition, RECEIPT_ENCODING_V1};

    fn receipt_key(n: u64) -> [u8; 12] {
        let mut key = crate::keyspace::ledger_receipt_key();
        key[4..].copy_from_slice(&n.to_be_bytes());
        key
    }

    fn delta(account: u64, asset: u64, value: i64) -> ReceiptBalanceDelta {
        ReceiptBalanceDelta {
            account: AccountId::new(account),
            asset: AssetId::new(asset),
            delta: value,
        }
    }

    fn archived(
        key: u64,
        intent: u128,
        ops: Vec<u16>,
        balance_deltas: Vec<ReceiptBalanceDelta>,
        ownership: Vec<ReceiptOwnershipTransition>,
    ) -> ReceiptArchiveRow {
        ReceiptArchiveRow {
            key: receipt_key(key),
            receipt: ReceiptRow {
                intent_id: intent,
                parties: balance_deltas.iter().map(|effect| effect.account).collect(),
                ops,
                balance_deltas,
                ownership,
            },
            encoding: RECEIPT_ENCODING_V1,
        }
    }

    fn write_window_pages(
        rows: &[ReceiptArchiveRow],
        page_rows: usize,
    ) -> (tempfile::TempDir, ReceiptArchiveSweeper) {
        let directory = tempfile::tempdir().expect("archive directory");
        let object_dir = directory.path().join("rarchive");
        std::fs::create_dir_all(&object_dir).expect("receipt object directory");
        for page in rows.chunks(page_rows) {
            let bytes = encode_receipt_object(page).expect("encode receipt object");
            let last = page.last().expect("nonempty object page").key;
            std::fs::write(
                object_dir.join(format!("{}.parquet", hex(&last[2..]))),
                bytes,
            )
            .expect("write receipt object");
        }
        let sweeper = ReceiptArchiveSweeper::open(directory.path(), "").expect("open sweeper");
        (directory, sweeper)
    }

    fn write_window(rows: &[ReceiptArchiveRow]) -> (tempfile::TempDir, ReceiptArchiveSweeper) {
        write_window_pages(rows, rows.len().max(1))
    }

    /// THE guarded stage: a one-unit leak composed across several otherwise
    /// plausible ledger moves is invisible after the hot cursor advanced, but
    /// the archive sum names `global_conservation_break` for its asset.
    #[test]
    fn full_window_slow_leak_is_named_when_the_incremental_pass_is_clean() {
        let item = ItemUid::new(55);
        let rows = vec![
            archived(
                1,
                1,
                vec![LEDGER_CREDIT_OP],
                vec![delta(1, 7, 1_000)],
                Vec::new(),
            ),
            archived(
                2,
                2,
                vec![LEDGER_ITEM_TRANSFER_OP],
                vec![delta(1, 7, -100), delta(2, 7, 100)],
                vec![ReceiptOwnershipTransition {
                    item,
                    before: Some(AccountId::new(1)),
                    after: Some(AccountId::new(2)),
                }],
            ),
            archived(
                3,
                3,
                vec![LEDGER_ITEM_TRANSFER_OP],
                vec![delta(2, 7, -100), delta(3, 7, 99)],
                vec![ReceiptOwnershipTransition {
                    item,
                    before: Some(AccountId::new(2)),
                    after: Some(AccountId::new(3)),
                }],
            ),
        ];
        let (_directory, sweeper) = write_window_pages(&rows, 1);

        let hot = LedgerWalk {
            balances: vec![
                BalanceRow {
                    account: AccountId::new(1),
                    asset: AssetId::new(7),
                    value: 900,
                },
                BalanceRow {
                    account: AccountId::new(2),
                    asset: AssetId::new(7),
                    value: 0,
                },
                BalanceRow {
                    account: AccountId::new(3),
                    asset: AssetId::new(7),
                    value: 99,
                },
            ],
            items: Vec::new(),
            new_receipts: Vec::new(),
            structural: Vec::new(),
        };
        assert!(
            evaluate_pass(&hot).is_empty(),
            "the hot cursor has passed the leak and every remaining balance is healthy"
        );

        let report = sweeper.run_pass(10, 11).expect("non-vacuous full pass");
        assert_eq!(report.population.receipts, 3, "the planted window was read");
        assert_eq!(
            report.population.objects, 3,
            "the leak spans archive objects rather than one degenerate page"
        );
        assert!(
            report.population.object_bytes > 0,
            "Parquet bytes were scanned"
        );
        let finding = report
            .findings
            .iter()
            .find(|finding| finding.kind == FindingKind::GlobalConservationBreak)
            .expect("the full-window conservation comparison must fire");
        assert_eq!(finding.kind.as_str(), "global_conservation_break");
        assert_eq!(finding.asset, Some(AssetId::new(7)));
        assert!(finding.detail.contains("unexplained delta -1"));
    }

    /// The reverse mutation direction: the same archive with both sides of
    /// every transfer balanced raises no finding, while still proving rows and
    /// bytes were consumed.
    #[test]
    fn clean_full_window_reads_receipts_and_raises_nothing() {
        let item = ItemUid::new(55);
        let rows = vec![
            archived(
                1,
                1,
                vec![LEDGER_CREDIT_OP],
                vec![delta(1, 7, 1_000)],
                Vec::new(),
            ),
            archived(
                2,
                2,
                vec![LEDGER_ITEM_TRANSFER_OP],
                vec![delta(1, 7, -100), delta(2, 7, 100)],
                vec![ReceiptOwnershipTransition {
                    item,
                    before: Some(AccountId::new(1)),
                    after: Some(AccountId::new(2)),
                }],
            ),
            archived(
                3,
                3,
                vec![LEDGER_ITEM_TRANSFER_OP],
                vec![delta(2, 7, -100), delta(3, 7, 100)],
                vec![ReceiptOwnershipTransition {
                    item,
                    before: Some(AccountId::new(2)),
                    after: Some(AccountId::new(3)),
                }],
            ),
        ];
        let (_directory, sweeper) = write_window_pages(&rows, 2);

        let report = sweeper.run_pass(10, 11).expect("non-vacuous clean pass");
        assert_eq!(report.population.receipts, 3, "the healthy rows were read");
        assert_eq!(report.population.objects, 2);
        assert!(report.population.object_bytes > 0, "the object was scanned");
        assert_eq!(report.population.balance_deltas, 5);
        assert_eq!(report.population.ownership_transitions, 2);
        assert!(
            report.findings.is_empty(),
            "healthy archived effects must not be flagged: {:?}",
            report.findings
        );
    }

    #[test]
    fn item_history_names_overlap_and_unreceipted_reappearance() {
        let item = ItemUid::new(9);
        let rows = vec![
            archived(
                1,
                1,
                Vec::new(),
                Vec::new(),
                vec![ReceiptOwnershipTransition {
                    item,
                    before: None,
                    after: Some(AccountId::new(1)),
                }],
            ),
            archived(
                2,
                2,
                Vec::new(),
                Vec::new(),
                vec![ReceiptOwnershipTransition {
                    item,
                    before: None,
                    after: Some(AccountId::new(2)),
                }],
            ),
            archived(
                3,
                3,
                Vec::new(),
                Vec::new(),
                vec![ReceiptOwnershipTransition {
                    item,
                    before: Some(AccountId::new(2)),
                    after: None,
                }],
            ),
            archived(
                4,
                4,
                Vec::new(),
                Vec::new(),
                vec![ReceiptOwnershipTransition {
                    item,
                    before: Some(AccountId::new(3)),
                    after: Some(AccountId::new(4)),
                }],
            ),
        ];
        let (_directory, sweeper) = write_window_pages(&rows, 1);

        let report = sweeper.run_pass(0, 1).expect("full pass");
        assert!(report
            .findings
            .iter()
            .any(|finding| finding.kind == FindingKind::OverlappingItemOwnership));
        assert!(report
            .findings
            .iter()
            .any(|finding| finding.kind == FindingKind::UnreceiptedItemOwnership));
        assert_eq!(report.ownership_resort.entries, 4);
        assert_eq!(
            report.ownership_resort.memory_bytes_bound,
            4 * report.ownership_resort.bytes_per_entry
        );
    }

    #[test]
    fn zero_object_window_is_not_a_green_pass() {
        let directory = tempfile::tempdir().expect("archive directory");
        std::fs::create_dir_all(directory.path().join("rarchive"))
            .expect("receipt object directory");
        let sweeper = ReceiptArchiveSweeper::open(directory.path(), "").expect("open sweeper");
        let error = sweeper
            .run_pass(0, 1)
            .expect_err("zero objects are vacuous");
        assert!(error.0.contains("zero receipt archive objects"));
    }

    /// Reproducible inputs for #615's layout-cost artifact. The source rate is
    /// the P2-shaped 530 intents/s used by the Shape-C spike; the all-trade arm
    /// is the ownership-heavy bound the secondary-clustering decision needs.
    #[test]
    fn realistic_window_cost_inputs_are_machine_derived() {
        const PAGE_ROWS: u64 = 4_096;
        const INTENTS_PER_SECOND: u64 = 530;
        const SECONDS_PER_DAY: u64 = 86_400;

        let rows = (1..=PAGE_ROWS)
            .map(|key| {
                archived(
                    key,
                    u128::from(key),
                    vec![LEDGER_ITEM_TRANSFER_OP],
                    vec![delta(key, 7, -1), delta(key + 1, 7, 1)],
                    vec![ReceiptOwnershipTransition {
                        item: ItemUid::new(key),
                        before: Some(AccountId::new(key)),
                        after: Some(AccountId::new(key + 1)),
                    }],
                )
            })
            .collect::<Vec<_>>();
        let (_directory, sweeper) = write_window(&rows);
        let report = sweeper.run_pass(0, 1).expect("measurement pass");
        assert_eq!(report.population.receipts, PAGE_ROWS);
        assert!(report.findings.is_empty());

        let receipts_per_day = INTENTS_PER_SECOND * SECONDS_PER_DAY;
        let objects_per_day = receipts_per_day.div_ceil(PAGE_ROWS);
        let scan_bytes_per_day_bound = report.population.object_bytes * objects_per_day;
        let resort_bytes_per_day = receipts_per_day * report.ownership_resort.bytes_per_entry;
        eprintln!(
            "measurement page_rows={PAGE_ROWS} page_bytes={} ownership_event_bytes={} \
             receipts_per_day={receipts_per_day} objects_per_day={objects_per_day} \
             scan_bytes_per_day_bound={scan_bytes_per_day_bound} \
             resort_bytes_per_day={resort_bytes_per_day}",
            report.population.object_bytes, report.ownership_resort.bytes_per_entry,
        );
    }
}
