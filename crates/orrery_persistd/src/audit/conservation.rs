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
//! balance fold streams, while ownership continuity is an external merge sort
//! by `(ItemUid, receipt key, transition ordinal)`. The sorter writes bounded
//! sorted runs to a private work directory and merges at most a fixed fan-in,
//! so the ownership *heap* working set is independent of retained history.
//! The spill is not: every transition in the window costs
//! [`OWNERSHIP_RUN_RECORD_BYTES`] on the work directory's filesystem, and the
//! default work directory is the OS temporary directory, which on systemd
//! hosts is RAM-backed tmpfs. `--audit-work-dir` moves it (#912).
//!
//! Findings are established in two independent halves and each half is
//! emitted the moment it is final. The balance, effect-shape, and global
//! conservation findings are complete once the captured window has been
//! folded; the ownership-continuity findings need the external merge, which
//! is the only step between the fold and the report that performs fallible
//! file I/O. A spill failure therefore returns a [`FullSweepFailure`] that
//! carries and has already emitted the first half. It cannot silence a
//! computed conservation break (#912).

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Write};
use std::mem::size_of;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

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

/// Ownership events retained in one sorted external-sort run.
///
/// This is deliberately one receipt-object page from the archive tailer. The
/// page decoder is already that bound; allowing an ownership re-sort run to
/// outgrow it would reintroduce a history-sized allocation through this path.
const OWNERSHIP_RUN_EVENTS: usize = 4_096;

/// Simultaneous sorted-run heads during the external merge.
const OWNERSHIP_MERGE_FAN_IN: usize = 16;

/// Fixed-size on-disk representation of one [`OwnershipEvent`] in a run.
///
/// This is the spill cost per ownership transition in the captured window.
/// It lives on the work directory's filesystem for the duration of the pass,
/// which is why it is reported beside the heap bound rather than folded into
/// it.
pub const OWNERSHIP_RUN_RECORD_BYTES: usize = 42;

/// Maximum ownership-event working set for one full conservation pass.
///
/// It includes one page-sized sorted run and one head per merge input. Object
/// decoding remains page-bounded too; this number specifically reports the
/// ownership re-sort that used to scale with every transition in retention.
pub const OWNERSHIP_RESORT_MEMORY_CEILING_BYTES: u64 =
    ((OWNERSHIP_RUN_EVENTS + OWNERSHIP_MERGE_FAN_IN) * size_of::<OwnershipEvent>()) as u64;

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
    /// Ownership transitions read for the item-history external merge.
    pub ownership_transitions: u64,
}

/// The measured layout cost of re-grouping commit-ordered ownership history.
///
/// Two numbers, deliberately not one. `memory_bytes_bound` is the heap the
/// external merge holds and is independent of retention; `spill_bytes` is
/// what the same merge writes to the work directory and scales with every
/// transition in the window. Reporting only the first is what #912 corrected.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OwnershipResortCost {
    /// Events sorted by item and commit order across all external runs.
    pub entries: u64,
    /// Inline bytes per ownership-event working-set slot in this build.
    pub bytes_per_entry: u64,
    /// Bounded ownership-sort heap working set: one run plus merge heads.
    pub memory_bytes_bound: u64,
    /// Fixed on-disk bytes per spilled event.
    pub spill_record_bytes: u64,
    /// Bytes the sorted runs occupy on the work directory's filesystem for
    /// the pass: `entries * spill_record_bytes`. A consolidation stage
    /// transiently holds one merged output batch beside its unconsumed
    /// inputs, so the work directory needs headroom of under twice this.
    pub spill_bytes: u64,
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
    /// The item-history regrouping cost: bounded heap, window-sized spill.
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

/// A full pass that did not complete, together with every finding it had
/// already established and emitted before it stopped.
///
/// The operator's question when a pass fails is not only "why" but "what did
/// it learn first". When the ledger is unbalanced *and* the spill disk is
/// full, both facts must be visible: the conservation break has already gone
/// out on `orrery_audit` by the time the spill fails, and this value carries
/// it a second time so the failure log line can count it. The pre-image
/// reported only the failure, every day, until the disk was fixed (#912).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FullSweepFailure {
    /// What stopped the pass.
    pub error: FullSweepError,
    /// Findings computed from the complete captured window and emitted
    /// before the failure. Empty when the failure preceded the window fold,
    /// in which case no whole-window verdict existed to lose.
    pub emitted_findings: Vec<AuditFinding>,
}

impl FullSweepFailure {
    /// How many emitted findings are whole-window conservation breaks.
    #[must_use]
    pub fn conservation_breaks(&self) -> usize {
        self.emitted_findings
            .iter()
            .filter(|finding| finding.kind == FindingKind::GlobalConservationBreak)
            .count()
    }
}

impl From<FullSweepError> for FullSweepFailure {
    fn from(error: FullSweepError) -> Self {
        Self {
            error,
            emitted_findings: Vec::new(),
        }
    }
}

impl std::fmt::Display for FullSweepFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}", self.error)?;
        if !self.emitted_findings.is_empty() {
            write!(
                formatter,
                "; {} finding(s) were established and emitted before the failure, {} of them \
                 global_conservation_break; ownership continuity is unverified for this window",
                self.emitted_findings.len(),
                self.conservation_breaks()
            )?;
        }
        Ok(())
    }
}

impl std::error::Error for FullSweepFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.error)
    }
}

/// Read-only full sweep over the filesystem receipt-archive backend.
#[derive(Debug, Clone)]
pub struct ReceiptArchiveSweeper {
    archive_root: PathBuf,
    archive_prefix: String,
    /// Where the ownership external sort spills. `None` is the OS temporary
    /// directory, kept as the default so the flag is opt-in, but see
    /// [`Self::with_work_dir`] for why a production host should set it.
    work_dir: Option<PathBuf>,
    /// Test-only fault injected into the first spill of the pass.
    #[cfg(test)]
    spill_fault: Option<String>,
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
            work_dir: None,
            #[cfg(test)]
            spill_fault: None,
        })
    }

    /// Spill the ownership external sort under `work_dir` instead of the OS
    /// temporary directory.
    ///
    /// The sort writes [`OWNERSHIP_RUN_RECORD_BYTES`] per ownership transition
    /// in the captured window and holds it until the pass ends. On a systemd
    /// host `std::env::temp_dir()` is tmpfs, so the default turns that spill
    /// into resident memory the reported heap bound does not include. Point
    /// this at disk with room for twice the reported `spill_bytes`.
    ///
    /// The directory is created if missing. Each pass creates and removes a
    /// private unique subdirectory beneath it.
    #[must_use]
    pub fn with_work_dir(mut self, work_dir: impl Into<PathBuf>) -> Self {
        self.work_dir = Some(work_dir.into());
        self
    }

    /// The configured spill root, when one is set.
    #[must_use]
    pub fn work_dir(&self) -> Option<&Path> {
        self.work_dir.as_deref()
    }

    /// Fail the first spill of the next pass with `message`, after the window
    /// has been folded. This stands in for ENOSPC on the work directory.
    #[cfg(test)]
    fn with_spill_fault(mut self, message: &str) -> Self {
        self.spill_fault = Some(message.to_owned());
        self
    }

    /// Run one full pass over the receipt objects visible when this call begins.
    ///
    /// An empty object set or an object set decoding to zero receipts is an
    /// error. That is deliberate: `0 rows; 0 findings` is not conservation
    /// evidence and must never be reported as a healthy pass.
    ///
    /// Findings are returned and emitted on the shared `orrery_audit` target.
    /// Emission happens in two halves, each as soon as it is final: the
    /// whole-window balance, effect-shape, and conservation findings right
    /// after the fold, and the ownership-continuity findings after the
    /// external merge. A failure between the two returns the first half in
    /// [`FullSweepFailure::emitted_findings`], already emitted.
    ///
    /// # Errors
    ///
    /// Returns [`FullSweepFailure`] for directory reads, object reads, Parquet
    /// decoding, non-monotone/duplicate receipt keys, sum overflow, or a
    /// work-directory spill failure. Only the last can carry emitted findings.
    pub fn run_pass(
        &self,
        started_at_ms: u64,
        finished_at_ms: u64,
    ) -> Result<FullSweepReport, FullSweepFailure> {
        let objects = self.capture_objects()?;
        if objects.is_empty() {
            return Err(FullSweepFailure::from(FullSweepError(
                "full conservation sweep captured zero receipt archive objects".to_owned(),
            )));
        }

        let mut population = FullSweepPopulation {
            objects: objects.len() as u64,
            ..FullSweepPopulation::default()
        };
        let mut observed = BTreeMap::<AssetId, i128>::new();
        let mut receipted = BTreeMap::<AssetId, i128>::new();
        let mut ownership = OwnershipExternalSorter::create(self.work_dir.as_deref())?;
        #[cfg(test)]
        {
            ownership.spill_fault = self.spill_fault.clone();
        }
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
                    return Err(FullSweepFailure::from(FullSweepError(format!(
                        "receipt archive keys are not strictly increasing: {} after {}",
                        hex(&row.key),
                        previous_key.map_or_else(String::new, |key| hex(&key))
                    ))));
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
            return Err(FullSweepFailure::from(FullSweepError(
                "full conservation sweep decoded zero receipt rows".to_owned(),
            )));
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

        // The whole-window verdict is final here: every captured object has
        // been folded, and nothing below can add to or retract these findings.
        // Emit them before the external merge, whose spill is the one step
        // left that can fail on the host rather than on the archive. A
        // conservation break must not wait behind a full disk (#912).
        for finding in &findings {
            finding.emit();
        }

        let ownership_findings = ownership
            .cost(population.ownership_transitions)
            .and_then(|cost| ownership.finish().map(|found| (cost, found)));
        let (ownership_resort, ownership_findings) = match ownership_findings {
            Ok(established) => established,
            Err(error) => {
                return Err(FullSweepFailure {
                    error,
                    emitted_findings: findings,
                });
            }
        };
        for finding in &ownership_findings {
            finding.emit();
        }
        findings.extend(ownership_findings);

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

/// Private scratch directory for one external ownership merge.
///
/// `tempfile` is intentionally only a test dependency in this crate. The
/// daemon needs no new runtime dependency to obtain a best-effort-cleaned
/// unique directory under the configured work root, or under the operating
/// system temporary directory when none is configured.
#[derive(Debug)]
struct SweepWorkDir(PathBuf);

impl SweepWorkDir {
    fn create(root: Option<&Path>) -> Result<Self, FullSweepError> {
        static NEXT_WORK_DIR: AtomicU64 = AtomicU64::new(0);

        let root = match root {
            Some(root) => {
                std::fs::create_dir_all(root).map_err(|error| {
                    FullSweepError(format!(
                        "create audit work directory {}: {error}",
                        root.display()
                    ))
                })?;
                root.to_path_buf()
            }
            None => std::env::temp_dir(),
        };
        let epoch_nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |duration| duration.as_nanos());
        for attempt in 0..64_u64 {
            let serial = NEXT_WORK_DIR.fetch_add(1, Ordering::Relaxed);
            let path = root.join(format!(
                "orrery-conservation-{}-{epoch_nanos}-{serial}-{attempt}",
                std::process::id()
            ));
            match std::fs::create_dir(&path) {
                Ok(()) => return Ok(Self(path)),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => {
                    return Err(FullSweepError(format!(
                        "create ownership-sort work directory {}: {error}",
                        path.display()
                    )));
                }
            }
        }
        Err(FullSweepError(
            "create unique ownership-sort work directory: exhausted 64 attempts".to_owned(),
        ))
    }

    fn stage(&self, stage: u32) -> PathBuf {
        self.0.join(format!("stage-{stage}"))
    }
}

impl Drop for SweepWorkDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Page-bounded external sorter for commit-ordered ownership transitions.
#[derive(Debug)]
struct OwnershipExternalSorter {
    work_dir: SweepWorkDir,
    buffer: Vec<OwnershipEvent>,
    runs: u64,
    #[cfg(test)]
    spill_fault: Option<String>,
}

impl OwnershipExternalSorter {
    fn create(root: Option<&Path>) -> Result<Self, FullSweepError> {
        let work_dir = SweepWorkDir::create(root)?;
        std::fs::create_dir(work_dir.stage(0)).map_err(|error| {
            FullSweepError(format!(
                "create ownership-sort initial run directory: {error}"
            ))
        })?;
        Ok(Self {
            work_dir,
            buffer: Vec::with_capacity(OWNERSHIP_RUN_EVENTS),
            runs: 0,
            #[cfg(test)]
            spill_fault: None,
        })
    }

    fn push(&mut self, event: OwnershipEvent) -> Result<(), FullSweepError> {
        self.buffer.push(event);
        if self.buffer.len() == OWNERSHIP_RUN_EVENTS {
            self.flush_run()?;
        }
        Ok(())
    }

    fn cost(&self, entries: u64) -> Result<OwnershipResortCost, FullSweepError> {
        let slots = self
            .buffer
            .capacity()
            .checked_add(OWNERSHIP_MERGE_FAN_IN)
            .ok_or_else(|| FullSweepError("ownership re-sort slot bound overflow".to_owned()))?;
        let memory_bytes_bound = (slots as u64)
            .checked_mul(size_of::<OwnershipEvent>() as u64)
            .ok_or_else(|| FullSweepError("ownership re-sort byte bound overflow".to_owned()))?;
        let spill_bytes = entries
            .checked_mul(OWNERSHIP_RUN_RECORD_BYTES as u64)
            .ok_or_else(|| FullSweepError("ownership re-sort spill bound overflow".to_owned()))?;
        Ok(OwnershipResortCost {
            entries,
            bytes_per_entry: size_of::<OwnershipEvent>() as u64,
            memory_bytes_bound,
            spill_record_bytes: OWNERSHIP_RUN_RECORD_BYTES as u64,
            spill_bytes,
        })
    }

    fn flush_run(&mut self) -> Result<(), FullSweepError> {
        if self.buffer.is_empty() {
            return Ok(());
        }
        #[cfg(test)]
        if let Some(message) = self.spill_fault.take() {
            return Err(FullSweepError(format!(
                "create ownership-sort run {}: {message}",
                run_path(&self.work_dir, 0, self.runs).display()
            )));
        }
        self.buffer.sort_unstable_by(event_order);
        let path = run_path(&self.work_dir, 0, self.runs);
        write_run(&path, &self.buffer)?;
        self.buffer.clear();
        self.runs = checked_u64_add(self.runs, 1, "ownership external-sort run count")?;
        Ok(())
    }

    fn finish(mut self) -> Result<Vec<AuditFinding>, FullSweepError> {
        self.flush_run()?;
        if self.runs == 0 {
            return Ok(Vec::new());
        }

        let mut stage = 0_u32;
        while self.runs > OWNERSHIP_MERGE_FAN_IN as u64 {
            self.runs = consolidate_stage(&self.work_dir, stage, self.runs)?;
            stage = stage.checked_add(1).ok_or_else(|| {
                FullSweepError("ownership external-sort stage counter overflow".to_owned())
            })?;
        }
        ownership_findings_from_stage(&self.work_dir.stage(stage), self.runs)
    }
}

fn event_order(left: &OwnershipEvent, right: &OwnershipEvent) -> std::cmp::Ordering {
    (left.item.0, left.receipt_key, left.ordinal).cmp(&(
        right.item.0,
        right.receipt_key,
        right.ordinal,
    ))
}

fn write_run(path: &Path, events: &[OwnershipEvent]) -> Result<(), FullSweepError> {
    let file = File::create(path).map_err(|error| {
        FullSweepError(format!(
            "create ownership-sort run {}: {error}",
            path.display()
        ))
    })?;
    let mut writer = BufWriter::with_capacity(OWNERSHIP_RUN_RECORD_BYTES, file);
    for event in events {
        write_event(&mut writer, event)?;
    }
    writer.flush().map_err(|error| {
        FullSweepError(format!(
            "flush ownership-sort run {}: {error}",
            path.display()
        ))
    })
}

fn write_event(writer: &mut impl Write, event: &OwnershipEvent) -> Result<(), FullSweepError> {
    writer
        .write_all(&event.item.0.to_le_bytes())
        .and_then(|()| write_account(writer, event.before))
        .and_then(|()| write_account(writer, event.after))
        .and_then(|()| writer.write_all(&event.receipt_key))
        .and_then(|()| writer.write_all(&event.ordinal.to_le_bytes()))
        .map_err(|error| FullSweepError(format!("write ownership-sort event: {error}")))
}

fn write_account(writer: &mut impl Write, account: Option<AccountId>) -> std::io::Result<()> {
    match account {
        Some(account) => {
            writer.write_all(&[1])?;
            writer.write_all(&account.0.to_le_bytes())
        }
        None => {
            writer.write_all(&[0])?;
            writer.write_all(&0_u64.to_le_bytes())
        }
    }
}

#[derive(Debug)]
struct RunCursor {
    reader: BufReader<File>,
    current: Option<OwnershipEvent>,
}

impl RunCursor {
    fn open(path: &Path) -> Result<Self, FullSweepError> {
        let file = File::open(path).map_err(|error| {
            FullSweepError(format!(
                "open ownership-sort run {}: {error}",
                path.display()
            ))
        })?;
        let mut cursor = Self {
            reader: BufReader::with_capacity(OWNERSHIP_RUN_RECORD_BYTES, file),
            current: None,
        };
        cursor.advance()?;
        Ok(cursor)
    }

    fn advance(&mut self) -> Result<(), FullSweepError> {
        self.current = read_event(&mut self.reader)?;
        Ok(())
    }
}

fn read_event(reader: &mut impl Read) -> Result<Option<OwnershipEvent>, FullSweepError> {
    let mut item = [0_u8; 8];
    let read = reader
        .read(&mut item)
        .map_err(|error| FullSweepError(format!("read ownership-sort event: {error}")))?;
    if read == 0 {
        return Ok(None);
    }
    reader
        .read_exact(&mut item[read..])
        .map_err(|error| FullSweepError(format!("truncated ownership-sort event item: {error}")))?;
    let before = read_account(reader)?;
    let after = read_account(reader)?;
    let mut receipt_key = [0_u8; 12];
    reader.read_exact(&mut receipt_key).map_err(|error| {
        FullSweepError(format!("truncated ownership-sort receipt key: {error}"))
    })?;
    let mut ordinal = [0_u8; 4];
    reader
        .read_exact(&mut ordinal)
        .map_err(|error| FullSweepError(format!("truncated ownership-sort ordinal: {error}")))?;
    Ok(Some(OwnershipEvent {
        item: ItemUid::new(u64::from_le_bytes(item)),
        before,
        after,
        receipt_key,
        ordinal: u32::from_le_bytes(ordinal),
    }))
}

fn read_account(reader: &mut impl Read) -> Result<Option<AccountId>, FullSweepError> {
    let mut present = [0_u8; 1];
    reader.read_exact(&mut present).map_err(|error| {
        FullSweepError(format!("truncated ownership-sort account tag: {error}"))
    })?;
    let mut account = [0_u8; 8];
    reader
        .read_exact(&mut account)
        .map_err(|error| FullSweepError(format!("truncated ownership-sort account: {error}")))?;
    match present[0] {
        0 => Ok(None),
        1 => Ok(Some(AccountId::new(u64::from_le_bytes(account)))),
        tag => Err(FullSweepError(format!(
            "invalid ownership-sort account presence tag {tag}"
        ))),
    }
}

/// Name run `serial` of `stage`. Runs are written under their serial, so a
/// stage's catalogue is the counter that produced it and never a directory
/// listing.
fn run_path(work_dir: &SweepWorkDir, stage: u32, serial: u64) -> PathBuf {
    work_dir.stage(stage).join(format!("{serial:020}.run"))
}

/// Merge each bounded batch in one stage into the next, deleting inputs as
/// they are consumed. The input catalogue is the run counter, `runs`: every
/// run in a stage is named by its serial, so the batch to merge is derived
/// arithmetically and no directory is listed while files are being removed
/// from it. A `read_dir` iterator live across `remove_file` on the same
/// directory is unspecified under POSIX and observably skips or repeats
/// entries on ext4 with `dir_index` at around eleven thousand files, which is
/// one day of runs (#912). A batch of at most sixteen paths keeps the
/// catalogue out of the process heap.
fn consolidate_stage(
    work_dir: &SweepWorkDir,
    stage: u32,
    runs: u64,
) -> Result<u64, FullSweepError> {
    let input = work_dir.stage(stage);
    let next_stage = stage
        .checked_add(1)
        .ok_or_else(|| FullSweepError("ownership external-sort stage overflow".to_owned()))?;
    let output = work_dir.stage(next_stage);
    std::fs::create_dir(&output).map_err(|error| {
        FullSweepError(format!(
            "create ownership-sort stage {}: {error}",
            output.display()
        ))
    })?;
    let mut first = 0_u64;
    let mut output_runs = 0_u64;
    while first < runs {
        let last = first
            .saturating_add(OWNERSHIP_MERGE_FAN_IN as u64)
            .min(runs);
        let batch = (first..last)
            .map(|serial| run_path(work_dir, stage, serial))
            .collect::<Vec<_>>();
        let output_path = run_path(work_dir, next_stage, output_runs);
        merge_runs(&batch, &output_path)?;
        for path in batch {
            std::fs::remove_file(&path).map_err(|error| {
                FullSweepError(format!(
                    "remove consumed ownership-sort run {}: {error}",
                    path.display()
                ))
            })?;
        }
        output_runs = checked_u64_add(output_runs, 1, "ownership external-sort output run count")?;
        first = last;
    }
    std::fs::remove_dir(&input).map_err(|error| {
        FullSweepError(format!(
            "remove consumed ownership-sort stage {}: {error}",
            input.display()
        ))
    })?;
    Ok(output_runs)
}

fn merge_runs(paths: &[PathBuf], output: &Path) -> Result<(), FullSweepError> {
    let mut cursors = paths
        .iter()
        .map(|path| RunCursor::open(path))
        .collect::<Result<Vec<_>, _>>()?;
    let file = File::create(output).map_err(|error| {
        FullSweepError(format!(
            "create merged ownership-sort run {}: {error}",
            output.display()
        ))
    })?;
    let mut writer = BufWriter::with_capacity(OWNERSHIP_RUN_RECORD_BYTES, file);
    while let Some(index) = smallest_cursor(&cursors) {
        let event = cursors[index]
            .current
            .expect("selected cursor always has a current ownership event");
        write_event(&mut writer, &event)?;
        cursors[index].advance()?;
    }
    writer.flush().map_err(|error| {
        FullSweepError(format!(
            "flush merged ownership-sort run {}: {error}",
            output.display()
        ))
    })
}

fn smallest_cursor(cursors: &[RunCursor]) -> Option<usize> {
    cursors
        .iter()
        .enumerate()
        .filter_map(|(index, cursor)| cursor.current.map(|event| (index, event)))
        .min_by(|(_, left), (_, right)| event_order(left, right))
        .map(|(index, _)| index)
}

fn ownership_findings_from_stage(
    stage: &Path,
    runs: u64,
) -> Result<Vec<AuditFinding>, FullSweepError> {
    if runs > OWNERSHIP_MERGE_FAN_IN as u64 {
        return Err(FullSweepError(
            "final ownership-sort stage exceeds merge fan-in".to_owned(),
        ));
    }
    let paths = (0..runs)
        .map(|serial| stage.join(format!("{serial:020}.run")))
        .collect::<Vec<_>>();
    let mut cursors = paths
        .iter()
        .map(|path| RunCursor::open(path))
        .collect::<Result<Vec<_>, _>>()?;
    let mut findings = Vec::new();
    let mut previous: Option<(ItemUid, Option<AccountId>)> = None;
    while let Some(index) = smallest_cursor(&cursors) {
        let event = cursors[index]
            .current
            .expect("selected cursor always has a current ownership event");
        match previous {
            Some((item, current)) if item == event.item => {
                append_ownership_finding(&mut findings, event.item, current, &event);
                previous = Some((item, event.after));
            }
            _ => previous = Some((event.item, event.after)),
        }
        cursors[index].advance()?;
    }
    Ok(findings)
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
    ownership: &mut OwnershipExternalSorter,
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
        })?;
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

fn append_ownership_finding(
    findings: &mut Vec<AuditFinding>,
    item: ItemUid,
    current: Option<AccountId>,
    event: &OwnershipEvent,
) {
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
            OWNERSHIP_RESORT_MEMORY_CEILING_BYTES
        );
    }

    /// Thirty page-sized synthetic days force the multi-stage merge path: the
    /// sweep reads 30 complete archive objects (not a delta) while its reported
    /// ownership working set remains below the stated 256 KiB ceiling.
    #[test]
    fn thirty_day_synthetic_archive_stays_inside_ownership_memory_ceiling() {
        const SYNTHETIC_DAYS: u64 = 30;
        const PAGE_ROWS: u64 = OWNERSHIP_RUN_EVENTS as u64;
        const MEMORY_CEILING_BYTES: u64 = 256 * 1024;

        let directory = tempfile::tempdir().expect("archive directory");
        let object_dir = directory.path().join("rarchive");
        std::fs::create_dir_all(&object_dir).expect("receipt object directory");
        for day in 0..SYNTHETIC_DAYS {
            let first = day * PAGE_ROWS + 1;
            let rows = (first..first + PAGE_ROWS)
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
            let bytes = encode_receipt_object(&rows).expect("encode synthetic receipt object");
            let last = rows.last().expect("synthetic page is nonempty").key;
            std::fs::write(
                object_dir.join(format!("{}.parquet", hex(&last[2..]))),
                bytes,
            )
            .expect("write synthetic receipt object");
        }

        let sweeper = ReceiptArchiveSweeper::open(directory.path(), "").expect("open sweeper");
        let report = sweeper.run_pass(0, 1).expect("synthetic 30-day pass");
        assert_eq!(report.population.objects, SYNTHETIC_DAYS);
        assert_eq!(report.population.receipts, SYNTHETIC_DAYS * PAGE_ROWS);
        assert_eq!(
            report.population.ownership_transitions,
            SYNTHETIC_DAYS * PAGE_ROWS
        );
        assert!(
            report.findings.is_empty(),
            "synthetic transfers are balanced"
        );
        assert_eq!(
            report.ownership_resort.memory_bytes_bound,
            OWNERSHIP_RESORT_MEMORY_CEILING_BYTES
        );
        assert!(
            report.ownership_resort.memory_bytes_bound <= MEMORY_CEILING_BYTES,
            "ownership external-sort working set {} exceeds stated {} byte ceiling",
            report.ownership_resort.memory_bytes_bound,
            MEMORY_CEILING_BYTES
        );
    }

    /// The slow-leak window from the guarded test above, unchanged.
    fn leaking_window_rows() -> Vec<ReceiptArchiveRow> {
        let item = ItemUid::new(55);
        vec![
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
        ]
    }

    /// THE #912 stage: the ledger is unbalanced *and* the spill fails. The
    /// conservation break was computed before the spill and must reach the
    /// audit log anyway; the spill failure must also be reported, carrying
    /// the break, and must not be mistaken for a pass that found nothing.
    #[test]
    fn conservation_break_is_emitted_before_a_spill_failure_and_the_failure_is_reported() {
        let rows = leaking_window_rows();
        let (_directory, sweeper) = write_window_pages(&rows, 1);
        let sweeper = sweeper.with_spill_fault("No space left on device (os error 28)");

        let failure = sweeper
            .run_pass(10, 11)
            .expect_err("the injected spill failure must surface");

        // The louder condition is reported, with the spill path it hit.
        assert!(
            failure.error.0.contains("No space left on device"),
            "the spill failure names its cause: {}",
            failure.error
        );
        assert!(
            failure.error.0.contains("ownership-sort run"),
            "the spill failure names the step that failed: {}",
            failure.error
        );

        // The more serious condition was emitted before the failure, in the
        // same shape a clean pass emits it. `emitted_findings` is the record
        // of exactly that emission, not a parallel one: `run_pass` emits the
        // whole-window verdict the moment the fold is final, and a spill
        // failure returns precisely the findings it had already emitted.
        // Asserting on the returned value pins the ordering without
        // depending on process-global tracing state (#936).
        assert_eq!(failure.conservation_breaks(), 1);
        let carried = failure
            .emitted_findings
            .iter()
            .find(|finding| finding.kind == FindingKind::GlobalConservationBreak)
            .expect("the computed break must be carried as emitted before the spill failed");
        assert_eq!(carried.asset, Some(AssetId::new(7)));
        assert!(
            carried.detail.contains("unexplained delta -1"),
            "the emitted break carries its magnitude: {}",
            carried.detail
        );
        assert!(
            !failure.emitted_findings.iter().any(|finding| matches!(
                finding.kind,
                FindingKind::OverlappingItemOwnership | FindingKind::UnreceiptedItemOwnership
            )),
            "ownership continuity was never established and must not be emitted: {:?}",
            failure.emitted_findings
        );

        // And the failure states what it had already told the operator, so
        // the daemon's failure line can count it.
        let rendered = failure.to_string();
        assert!(
            rendered.contains("1 of them global_conservation_break")
                && rendered.contains("ownership continuity is unverified"),
            "the failure text states both what was found and what was not checked: {rendered}"
        );
    }

    /// A clean window through the same seam: the failure is still a failure
    /// with nothing carried, never a report with zero findings.
    #[test]
    fn spill_failure_on_a_clean_window_is_a_failure_not_a_green_pass() {
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
        ];
        let (_directory, sweeper) = write_window(&rows);
        let sweeper = sweeper.with_spill_fault("No space left on device (os error 28)");
        let failure = sweeper
            .run_pass(10, 11)
            .expect_err("the spill failure must surface");
        assert!(
            failure.emitted_findings.is_empty(),
            "nothing was found, so nothing was emitted: {:?}",
            failure.emitted_findings
        );
        assert!(!failure.to_string().contains("emitted before the failure"));
    }

    /// A work directory that cannot take the spill is a reported failure with
    /// the path in it. ENOTDIR stands in for ENOSPC: both are the host
    /// refusing the spill root before any run is written, and neither must
    /// become a report.
    #[test]
    fn unwritable_work_dir_is_a_reported_failure_not_a_clean_pass() {
        let rows = leaking_window_rows();
        let (_directory, sweeper) = write_window_pages(&rows, 1);
        let blocker = tempfile::tempdir().expect("work root");
        let not_a_directory = blocker.path().join("full-disk");
        std::fs::write(&not_a_directory, b"occupied").expect("place a file where the root goes");
        let work_dir = not_a_directory.join("spill");
        let sweeper = sweeper.with_work_dir(&work_dir);

        let failure = sweeper
            .run_pass(10, 11)
            .expect_err("an unusable work directory cannot produce a report");
        assert!(
            failure.error.0.contains("create audit work directory")
                && failure.error.0.contains(&work_dir.display().to_string()),
            "the failure names the configured work directory: {}",
            failure.error
        );
        assert!(
            failure.emitted_findings.is_empty(),
            "the spill root is prepared before the window is folded, so nothing was established"
        );
    }

    /// `--audit-work-dir` is where the runs go: the configured root is
    /// created, the pass spills beneath it, and the pass cleans up after
    /// itself. The injected fault reports the spill path it would have
    /// written, which proves the location without racing the pass.
    #[test]
    fn audit_work_dir_selects_the_spill_location() {
        let rows = leaking_window_rows();
        let (_directory, sweeper) = write_window_pages(&rows, 1);
        let root = tempfile::tempdir().expect("work root");
        let work_dir = root.path().join("audit-spill");
        assert!(!work_dir.exists());
        let sweeper = sweeper.with_work_dir(&work_dir);
        assert_eq!(sweeper.work_dir(), Some(work_dir.as_path()));

        let failure = sweeper
            .clone()
            .with_spill_fault("probe")
            .run_pass(10, 11)
            .expect_err("the probe fault surfaces the spill path");
        let spill_path = failure
            .error
            .0
            .split("create ownership-sort run ")
            .nth(1)
            .and_then(|rest| rest.split(": probe").next())
            .map(PathBuf::from)
            .expect("the fault names the run it would have created");
        assert!(
            spill_path.starts_with(&work_dir),
            "spill {} is not under the configured work directory {}",
            spill_path.display(),
            work_dir.display()
        );
        assert!(
            spill_path.ends_with("stage-0/00000000000000000000.run"),
            "the first run of the pass: {}",
            spill_path.display()
        );

        let report = sweeper
            .run_pass(10, 11)
            .expect("the same window passes cleanly through disk");
        assert_eq!(report.ownership_resort.entries, 2);
        assert_eq!(
            report.ownership_resort.spill_bytes,
            2 * OWNERSHIP_RUN_RECORD_BYTES as u64
        );
        assert!(work_dir.is_dir(), "the configured root was created");
        assert_eq!(
            std::fs::read_dir(&work_dir)
                .expect("list work root")
                .count(),
            0,
            "each pass removes its private subdirectory"
        );
    }

    #[test]
    fn zero_object_window_is_not_a_green_pass() {
        let directory = tempfile::tempdir().expect("archive directory");
        std::fs::create_dir_all(directory.path().join("rarchive"))
            .expect("receipt object directory");
        let sweeper = ReceiptArchiveSweeper::open(directory.path(), "").expect("open sweeper");
        let failure = sweeper
            .run_pass(0, 1)
            .expect_err("zero objects are vacuous");
        assert!(failure.error.0.contains("zero receipt archive objects"));
        assert!(
            failure.emitted_findings.is_empty(),
            "no window was folded, so no verdict existed to emit"
        );
    }

    /// Reproducible inputs for #890's layout-cost artifact. The source rate is
    /// the P2-shaped 530 intents/s used by the Shape-C spike; the all-trade arm
    /// proves the external sort stays page-bounded at any retention horizon.
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
        let ownership_spill_bytes_per_day =
            receipts_per_day * report.ownership_resort.spill_record_bytes;
        assert_eq!(
            report.ownership_resort.spill_bytes,
            PAGE_ROWS * OWNERSHIP_RUN_RECORD_BYTES as u64,
            "the spill is window-sized: one record per transition"
        );
        assert_eq!(
            report.ownership_resort.memory_bytes_bound,
            OWNERSHIP_RESORT_MEMORY_CEILING_BYTES,
            "the reported ownership re-sort bound must be page/fan-in bounded, not transition bounded"
        );
        eprintln!(
            "measurement page_rows={PAGE_ROWS} page_bytes={} ownership_event_bytes={} \
             receipts_per_day={receipts_per_day} objects_per_day={objects_per_day} \
             scan_bytes_per_day_bound={scan_bytes_per_day_bound} \
             ownership_resort_memory_bytes_bound={} \
             ownership_spill_record_bytes={} \
             ownership_spill_bytes_per_day={ownership_spill_bytes_per_day}",
            report.population.object_bytes,
            report.ownership_resort.bytes_per_entry,
            report.ownership_resort.memory_bytes_bound,
            report.ownership_resort.spill_record_bytes,
        );
    }
}
