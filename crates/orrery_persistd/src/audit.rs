//! The hourly incremental hot-ledger sweep (#330) — the half of D32 clause
//! (g)'s economy-wide invariant auditor that needs no archive.
//!
//! D32 clause (g) defines the auditor's liveness as "deployed, sweeping on its
//! start cadence (daily full conservation sweep, hourly incremental over hot
//! ledgers), and emitting findings into the audit pipeline", and gates C3's
//! promotion review on it. That liveness has two halves with very different
//! readiness. The **daily full conservation sweep** reads history from Shape
//! C's receipt archive and now lives in [`conservation`]. The body below is the
//! **hourly incremental**: it reads *current ledger state*. It does not settle
//! cadence, ownership or the
//! time-to-detection target — ADR-0032 leaves those to #224; it produces the
//! measured time-to-detection figure #224 needs to settle them from evidence.
//!
//! # What one pass checks
//!
//! A pass walks the hot tier and evaluates every invariant that is a property
//! of *current state* rather than of history:
//!
//! 1. **Single ownership** ([`FindingKind::DuplicateOwnershipRow`]). Every row
//!    in `ledger/item`'s sub-span is an ownership claim on the item its key
//!    names, and the anti-dupe invariant (docs/08-persistence.md §6,
//!    [`crate::keyspace::ItemRow`]'s own doc comment) is that an item has
//!    **exactly one** of them. Claims are grouped by decoded item id, never by
//!    exact key: two distinct keys that both name the same item are two
//!    ownership rows for one item, whatever wrote the second one. Exact-key
//!    uniqueness is FoundationDB's business and holds trivially; the invariant
//!    worth sweeping is the one FDB does not enforce for us.
//! 2. **Balance/receipt agreement**, in the two directions current state
//!    actually supports:
//!    - [`FindingKind::NegativeBalance`] — a `ledger/bal` row below zero.
//!      Every debit is checked against a read of the same balance inside its
//!      committing transaction, so under correct operation no balance can go
//!      negative; a negative row means a sufficiency check was bypassed or an
//!      unchecked write path exists, which is value created by overdraft.
//!    - [`FindingKind::UnbackedReceiptParty`] — a receipt naming a party with
//!      no remaining presence anywhere in the ledger (no balance row, no owned
//!      item). Every trade moves price between its parties, so both parties
//!      held value at commit; a party from whom no trace remains either burned
//!      value through a path that does not exist today or was named by a
//!      receipt nothing backed. Each receipt is judged once, in the first pass
//!      after it lands — which is exactly what makes the sweep *incremental*.
//!
//! Structural findings ride along with any arm: [`FindingKind::StrayLedgerRow`]
//! for a row inside the ledger family outside every registered economic
//! sub-span (the D35 collision class), and
//! [`FindingKind::UndecodableLedgerRow`] for a row whose value will not decode.
//!
//! # What one pass cannot see, stated plainly
//!
//! - **Wealth created by sanctioned-looking credits.** `LEDGER_CREDIT_OP`
//!   mints from nothing *by design* (docs/08-persistence.md §6), and its only
//!   durable trace after the hour-old intent row is swept is the balance it
//!   created. "Nonzero balance with no provenance" is therefore not a finding
//!   any incremental reader may raise; separating sanctioned from unsanctioned
//!   creation needs history, which [`conservation`] now reads from the receipt
//!   archive.
//! - **Magnitude.** A finding says *that* current state contradicts an
//!   invariant; quantifying the leak against sanctioned flows is the daily
//!   full sweep's job.
//!
//! # The cursor, and why it adds no ordering
//!
//! Receipts are keyed by commit versionstamp, so receipt order *is* commit
//! order and nothing else in the ledger has an order at all. The sweep's
//! resumable position ([`crate::keyspace::ledger_audit_cursor_key`]) records
//! the last receipt key processed; the next pass reads receipts strictly after
//! it and advances it to the newest key it saw. Balances and items are walked
//! whole every pass — they are the *hot* tier, bounded by active population —
//! so the cursor bounds exactly the one family that grows without bound
//! (receipts are permanent by design) and invents no second ordering to do it.
//!
//! # How findings reach the audit pipeline
//!
//! The same two surfaces the enforcement ramp's shadow arm already uses
//! ([`crate::intent::shadow`]): a structured `tracing` event (the
//! out-of-process surface a deployment's OTel sink already consumes, docs/09
//! §9), and an in-process bounded log plus typed report for whatever
//! aggregates in-process. Reports carry their denominators — row counts walked
//! per family, sample counts per measurement — because a number without its
//! population is not a measurement.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use orrery_protocol::{AccountId, AssetId, ItemUid};
use serde::{Deserialize, Serialize};

use crate::keyspace::{self, ReceiptRow};

pub mod conservation;

/// The schema string every report this module writes carries.
///
/// Versioned from the first write, for the reason
/// [`crate::intent::ramp::RAMP_ARTIFACT_SCHEMA`] is: a reader guessing at a
/// shape it was not written for reports numbers that are wrong rather than
/// absent.
pub const SWEEP_REPORT_SCHEMA: &str = "orrery.audit.sweep/1";

/// What a finding found, one variant per incremental or full-sweep invariant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FindingKind {
    /// Two or more rows in `ledger/item`'s sub-span name the same item: the
    /// anti-dupe invariant broken. Names the item.
    DuplicateOwnershipRow,
    /// A `ledger/bal` row whose value is below zero: value created by
    /// overdraft. Names the account and asset.
    NegativeBalance,
    /// A receipt naming a party with no balance row and no owned item:
    /// value the receipt says moved has left no trace anywhere in the ledger.
    /// Names the receipt's intent and the account.
    UnbackedReceiptParty,
    /// A row inside the ledger family outside every registered economic
    /// sub-span — the D35 collision class. Carries the raw key.
    StrayLedgerRow,
    /// A row inside an economic sub-span whose key or value will not decode.
    /// Carries the raw key; the family decides what "will not decode" means.
    UndecodableLedgerRow,
    /// The full archive sweep found that an asset's observed signed deltas do
    /// not equal its receipted sanctioned-source deltas over the window.
    GlobalConservationBreak,
    /// A new item-owner interval began before the preceding owner interval
    /// ended.
    OverlappingItemOwnership,
    /// An item was already owned after a recorded burn, without an intervening
    /// `None -> Some` mint transition.
    UnreceiptedItemOwnership,
    /// A receipt's interpreted op ids and effect-vector lengths disagree, so
    /// the sweep cannot derive the sanctioned side of its conservation sum.
    ReceiptEffectShapeMismatch,
}

impl FindingKind {
    /// The stable machine-readable label, used in tracing fields and reports.
    ///
    /// Stable because findings cross process boundaries into the audit
    /// pipeline; renaming one is a schema change, not an edit.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::DuplicateOwnershipRow => "duplicate_ownership_row",
            Self::NegativeBalance => "negative_balance",
            Self::UnbackedReceiptParty => "unbacked_receipt_party",
            Self::StrayLedgerRow => "stray_ledger_row",
            Self::UndecodableLedgerRow => "undecodable_ledger_row",
            Self::GlobalConservationBreak => "global_conservation_break",
            Self::OverlappingItemOwnership => "overlapping_item_ownership",
            Self::UnreceiptedItemOwnership => "unreceipted_item_ownership",
            Self::ReceiptEffectShapeMismatch => "receipt_effect_shape_mismatch",
        }
    }
}

/// One contradiction between ledger state/history and a swept invariant.
///
/// Findings are **reports, never actions**: the auditor does not quarantine,
/// strike, annul or roll anything back. C3 is the control, and whether a
/// finding acts is the ramp's business under D32.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditFinding {
    /// Which invariant was contradicted.
    pub kind: FindingKind,
    /// The item named by an ownership finding, when there is one.
    pub item: Option<ItemUid>,
    /// The account named by a balance, receipt, or ownership finding.
    pub account: Option<AccountId>,
    /// The asset named by a balance or conservation finding.
    pub asset: Option<AssetId>,
    /// The intent id of a receipt finding, when there is one.
    pub receipt_intent_id: Option<u128>,
    /// The offending row's raw key, hex-encoded — structural findings point at
    /// bytes, because the bytes are the evidence.
    pub key_hex: String,
    /// Human-readable detail. Carries the numbers a report renders.
    pub detail: String,
}

impl AuditFinding {
    /// Emit a finding to the audit pipeline's out-of-process surface.
    ///
    /// `info`, not `warn`: volume discipline follows the shadow arm's split —
    /// findings are rare by construction (a clean ledger emits none), but the
    /// level decision belongs to whoever aggregates, and a duplicate-ownership
    /// hit is exactly what the pipeline exists to surface loudly. The
    /// structured fields are the contract; `detail` is decoration.
    pub fn emit(&self) {
        tracing::info!(
            target: "orrery_audit",
            kind = self.kind.as_str(),
            item = self.item.map(|i| i.0),
            account = self.account.map(|a| a.0),
            asset = self.asset.map(|a| a.0),
            receipt_intent_id = self.receipt_intent_id.map(|i| i.to_string()),
            key_hex = %self.key_hex,
            detail = %self.detail,
            "ledger sweep finding"
        );
    }
}

/// A bounded in-process log of findings, mirroring
/// [`crate::intent::shadow::ShadowObservationLog`].
///
/// Bounded because it is reachable from a periodic task and an unbounded one
/// is a memory leak with a bad week behind it. [`Self::total`] keeps counting
/// after the ring stops keeping, so a reader can always tell a quiet sweep
/// from a truncated log.
#[derive(Debug)]
pub struct FindingsLog {
    capacity: usize,
    total: AtomicU64,
    findings: Mutex<Vec<AuditFinding>>,
}

impl Default for FindingsLog {
    fn default() -> Self {
        Self::with_capacity(1024)
    }
}

impl FindingsLog {
    /// A log keeping at most `capacity` of the most recent findings.
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            total: AtomicU64::new(0),
            findings: Mutex::new(Vec::new()),
        }
    }

    /// Record one finding, emitting it as it lands.
    pub fn record(&self, finding: AuditFinding) {
        finding.emit();
        self.total.fetch_add(1, Ordering::Relaxed);
        let mut held = self.findings.lock().expect("findings log poisoned");
        if held.len() == self.capacity {
            held.remove(0);
        }
        held.push(finding);
    }

    /// Every finding still held, oldest first.
    #[must_use]
    pub fn findings(&self) -> Vec<AuditFinding> {
        self.findings.lock().expect("findings log poisoned").clone()
    }

    /// How many findings were recorded in total, including any the ring has
    /// since dropped.
    #[must_use]
    pub fn total(&self) -> u64 {
        self.total.load(Ordering::Relaxed)
    }
}

// ---------------------------------------------------------------------------
// The pass's inputs, collected shape
// ---------------------------------------------------------------------------

/// One `ledger/bal` row, decoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BalanceRow {
    /// The account the balance belongs to.
    pub account: AccountId,
    /// The asset held.
    pub asset: AssetId,
    /// The integer balance. Negative is itself a finding.
    pub value: i128,
}

/// One ownership claim: a row in `ledger/item`'s sub-span naming `item`.
///
/// `key_len` is kept because the claim's *key spelling* may vary while its
/// item does not — two spellings for one item are precisely the duplicate the
/// single-ownership arm exists to catch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ItemClaim {
    /// The item this row claims.
    pub item: ItemUid,
    /// The owner the row records. Present only so a finding can say who
    /// claims what; the invariant counts rows, not owners.
    pub owner: AccountId,
    /// The raw key's length in bytes.
    pub key_len: usize,
}

/// One receipt the sweep judged this pass, decoded, with its raw key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JudgedReceipt {
    /// The complete raw key (twelve bytes, versionstamp included).
    pub key: Vec<u8>,
    /// The decoded receipt row.
    pub row: ReceiptRow,
}

/// Everything one family walk collected, before any invariant is evaluated.
///
/// Structural findings (stray rows, undecodable rows) are produced at
/// collection time because they are properties of individual rows; the
/// invariants below are properties of the whole set and are evaluated by
/// [`evaluate_pass`].
#[derive(Debug, Clone, Default)]
pub struct LedgerWalk {
    /// Every well-formed balance row in the hot tier.
    pub balances: Vec<BalanceRow>,
    /// Every ownership claim in the hot tier.
    pub items: Vec<ItemClaim>,
    /// Every receipt strictly after the cursor, decoded and ready to judge.
    pub new_receipts: Vec<JudgedReceipt>,
    /// Findings already raised while collecting (undecodable rows).
    pub structural: Vec<AuditFinding>,
}

impl LedgerWalk {
    /// Whether an account has any presence in the walked hot tier: a balance
    /// row or an owned item.
    ///
    /// Presence is what [`FindingKind::UnbackedReceiptParty`] tests against,
    /// so it is defined once here rather than inline at the call site — the
    /// definition is the check.
    #[must_use]
    pub fn has_presence(&self, account: AccountId) -> bool {
        self.balances.iter().any(|row| row.account == account)
            || self.items.iter().any(|claim| claim.owner == account)
    }
}

// ---------------------------------------------------------------------------
// The invariants — the guarded stage
// ---------------------------------------------------------------------------

/// Evaluate the sweep's invariants over one walk.
///
/// Split from collection so the invariants are pure functions over data: the
/// unit tier proves them without a cluster, and the FDB adapter cannot drift
/// from them because all it does is feed them.
#[must_use]
pub fn evaluate_pass(walk: &LedgerWalk) -> Vec<AuditFinding> {
    let mut findings = Vec::new();
    findings.extend(single_ownership_findings(walk));
    findings.extend(negative_balance_findings(walk));
    findings.extend(unbacked_receipt_findings(walk));
    findings
}

/// The **single-ownership invariant**: every item has exactly one ownership
/// row.
///
/// This function is #330's guarded stage — the comparison whose removal the
/// acceptance evidence requires to be observable. It groups claims by decoded
/// item id and reports every group larger than one, naming the item.
fn single_ownership_findings(walk: &LedgerWalk) -> Vec<AuditFinding> {
    let mut by_item: BTreeMap<u64, Vec<&ItemClaim>> = BTreeMap::new();
    for claim in &walk.items {
        by_item.entry(claim.item.0).or_default().push(claim);
    }

    let mut findings = Vec::new();
    for (item, claims) in by_item {
        if claims.len() <= 1 {
            continue;
        }
        let owners: Vec<String> = claims.iter().map(|c| c.owner.0.to_string()).collect();
        findings.push(AuditFinding {
            kind: FindingKind::DuplicateOwnershipRow,
            item: Some(ItemUid::new(item)),
            account: None,
            asset: None,
            receipt_intent_id: None,
            key_hex: String::new(),
            detail: format!(
                "item {} has {} ownership rows (owners: {}); the \
                 single-ownership row is the anti-dupe invariant",
                item,
                claims.len(),
                owners.join(", ")
            ),
        });
    }
    findings
}

/// **Balance/receipt agreement, overdraft half**: no balance is negative.
fn negative_balance_findings(walk: &LedgerWalk) -> Vec<AuditFinding> {
    walk.balances
        .iter()
        .filter(|row| row.value < 0)
        .map(|row| AuditFinding {
            kind: FindingKind::NegativeBalance,
            item: None,
            account: Some(row.account),
            asset: Some(row.asset),
            receipt_intent_id: None,
            key_hex: hex(&keyspace::ledger_bal_key(row.account, row.asset)),
            detail: format!(
                "balance of account {} in asset {} is {value} — every debit is \
                 checked against a same-transaction read, so a negative balance \
                 means a sufficiency check was bypassed",
                row.account.0,
                row.asset.0,
                value = row.value
            ),
        })
        .collect()
}

/// **Balance/receipt agreement, backing half**: every party a newly-landed
/// receipt names still has some presence in the ledger.
fn unbacked_receipt_findings(walk: &LedgerWalk) -> Vec<AuditFinding> {
    let mut findings = Vec::new();
    for receipt in &walk.new_receipts {
        for party in &receipt.row.parties {
            if !walk.has_presence(*party) {
                findings.push(AuditFinding {
                    kind: FindingKind::UnbackedReceiptParty,
                    item: None,
                    account: Some(*party),
                    asset: None,
                    receipt_intent_id: Some(receipt.row.intent_id),
                    key_hex: hex(&receipt.key),
                    detail: format!(
                        "receipt for intent {} names account {} as a party, but \
                         the account holds no balance row and owns no item — \
                         value the receipt says moved has left no trace",
                        receipt.row.intent_id, party.0
                    ),
                });
            }
        }
    }
    findings
}

/// Lowercase hex for evidence keys.
fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

// ---------------------------------------------------------------------------
// The FDB adapter: collection, cursor, report
// ---------------------------------------------------------------------------

/// The row population one pass walked, reported with every figure derived from
/// it — a finding rate without its denominator is not a measurement.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SweepPopulation {
    /// `ledger/bal` rows decoded this pass.
    pub balance_rows: u64,
    /// Ownership claims in `ledger/item`'s sub-span this pass.
    pub item_claims: u64,
    /// Receipts judged this pass — the incremental tail, bounded by the
    /// cursor.
    pub new_receipts: u64,
}

/// What one sweep pass did.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SweepPassReport {
    /// The caller-supplied clock reading at pass start. Diagnostic ordering
    /// only; the receipt keys carry the real order.
    pub started_at_ms: u64,
    /// Same clock, pass end.
    pub finished_at_ms: u64,
    /// Raw key of the last receipt processed *before* this pass; empty when
    /// the sweep had never run.
    pub cursor_before: Vec<u8>,
    /// Raw key of the last receipt processed *after* this pass; empty when no
    /// receipt exists yet.
    pub cursor_after: Vec<u8>,
    /// The denominators.
    pub population: SweepPopulation,
    /// Every finding this pass raised, structural ones included.
    pub findings: Vec<AuditFinding>,
}

impl SweepPassReport {
    /// The artifact as pretty JSON, newline-terminated.
    ///
    /// # Errors
    ///
    /// Propagates any `serde_json` failure.
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        let mut rendered = serde_json::to_string_pretty(self)?;
        rendered.push('\n');
        Ok(rendered)
    }
}

/// A collection or cursor failure inside a pass.
#[derive(Debug)]
pub struct SweepError(pub String);

impl std::fmt::Display for SweepError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for SweepError {}

/// The hourly incremental hot-ledger sweeper over a live cluster.
///
/// One instance per process; it holds no mutable state of its own because the
/// durable cursor is the only state that must survive it. Dropping and
/// rebuilding an instance between passes is therefore safe — that is what
/// "resumable" means, and [`crate::keyspace::AuditCursorRow`] is where it
/// resumes from.
#[cfg(feature = "fdb")]
pub struct HotLedgerSweeper {
    db: std::sync::Arc<foundationdb::Database>,
}

#[cfg(feature = "fdb")]
use crate::keyspace::AuditCursorRow;

#[cfg(feature = "fdb")]
fn encode_err(what: &str, error: postcard::Error) -> SweepError {
    SweepError(format!("{what}: {error}"))
}

#[cfg(feature = "fdb")]
impl HotLedgerSweeper {
    /// Collect every `(key, value)` pair in `begin..end`, inclusive/exclusive.
    ///
    /// One shape for the whole sweeper, so a streaming-mode or retry subtlety
    /// cannot differ between arms: they all walk the same way.
    async fn scan_range(
        trx: &foundationdb::Transaction,
        begin: Vec<u8>,
        end: Vec<u8>,
    ) -> Result<Vec<(Vec<u8>, Vec<u8>)>, foundationdb::FdbBindingError> {
        use foundationdb::{KeySelector, RangeOption};
        use futures::TryStreamExt;

        let range = RangeOption {
            begin: KeySelector::first_greater_or_equal(begin.as_slice()),
            end: KeySelector::first_greater_or_equal(end.as_slice()),
            ..RangeOption::default()
        };
        let mut stream = trx.get_ranges_keyvalues(range, false);
        let mut pairs = Vec::new();
        while let Some(kv) = stream.try_next().await? {
            pairs.push((kv.key().to_vec(), kv.value().to_vec()));
        }
        Ok(pairs)
    }

    /// Construct from the process-scoped FDB context.
    #[must_use]
    pub fn from_context(context: &crate::FdbContext) -> Self {
        Self {
            db: context.database(),
        }
    }

    /// Read the durable cursor without sweeping. Exposed so a harness can
    /// show a restart resumed rather than restarted.
    pub async fn read_cursor(&self) -> Result<AuditCursorRow, SweepError> {
        let db = std::sync::Arc::clone(&self.db);
        db.run(|trx, _| async move { Self::read_cursor_in_trx(&trx).await })
            .await
            .map_err(|error: foundationdb::FdbBindingError| {
                SweepError(format!("read audit cursor: {error}"))
            })
    }

    async fn read_cursor_in_trx(
        trx: &foundationdb::Transaction,
    ) -> Result<AuditCursorRow, foundationdb::FdbBindingError> {
        let Some(bytes) = trx.get(&keyspace::ledger_audit_cursor_key(), false).await? else {
            return Ok(AuditCursorRow {
                last_receipt_key: Vec::new(),
                updated_at_ms: 0,
            });
        };
        let row: AuditCursorRow = postcard::from_bytes(bytes.as_ref()).map_err(|error| {
            foundationdb::FdbBindingError::new_custom_error(Box::new(SweepError(format!(
                "decode audit cursor: {error}"
            ))))
        })?;
        // A corrupt-length cursor cannot be honored without guessing at an
        // ordering nobody wrote down; restart from the head instead, where
        // "the head" costs one re-read of receipts already judged and loses
        // nothing but work.
        if !(row.last_receipt_key.is_empty()
            || row.last_receipt_key.len() == keyspace::ledger_receipt_key().len())
        {
            return Ok(AuditCursorRow {
                last_receipt_key: Vec::new(),
                updated_at_ms: row.updated_at_ms,
            });
        }
        Ok(row)
    }

    /// Run one full pass: walk the hot tier, judge the incremental tail,
    /// advance the durable cursor, return the report.
    ///
    /// Findings are returned in the report *and* emitted to tracing as they
    /// are raised, so a deployment with only a log sink still has them.
    ///
    /// # Errors
    ///
    /// Propagates transaction failures. A failed pass advances nothing: the
    /// next pass redoes the whole walk from the old cursor, which is correct
    /// because every check is a pure read of current state.
    pub async fn run_pass(&self, now_ms: u64) -> Result<SweepPassReport, SweepError> {
        let db = std::sync::Arc::clone(&self.db);
        let report = db
            .run(move |trx, _| async move {
                let cursor_before = Self::read_cursor_in_trx(&trx).await?;

                let mut walk = LedgerWalk::default();
                let mut structural = Vec::new();
                let balance_rows = Self::collect_balances(&trx, &mut structural).await?;
                let item_claims = Self::collect_items(&trx, &mut structural).await?;
                let new_receipts =
                    Self::collect_new_receipts(&trx, &cursor_before, &mut structural).await?;
                let strays = Self::collect_strays(&trx).await?;
                structural.extend(strays);

                walk.balances = balance_rows;
                walk.items = item_claims;
                walk.new_receipts = new_receipts.clone();
                walk.structural = structural;

                let mut findings = evaluate_pass(&walk);
                let raised = std::mem::take(&mut walk.structural);
                findings.extend(raised);
                for finding in &findings {
                    finding.emit();
                }

                let cursor_after = new_receipts
                    .last()
                    .map_or(cursor_before.last_receipt_key.clone(), |receipt| {
                        receipt.key.clone()
                    });

                // Advance the cursor in the same transaction as the reads:
                // serializable isolation makes the walk-and-advance atomic,
                // so a concurrent commit either lands before this pass's read
                // version (judged next pass) or after (judged by whoever runs
                // after this one).
                let row = AuditCursorRow {
                    last_receipt_key: cursor_after.clone(),
                    updated_at_ms: now_ms,
                };
                let encoded = postcard::to_stdvec(&row).map_err(|error| {
                    foundationdb::FdbBindingError::new_custom_error(Box::new(encode_err(
                        "encode audit cursor",
                        error,
                    )))
                })?;
                trx.set(&keyspace::ledger_audit_cursor_key(), &encoded);

                let population = SweepPopulation {
                    balance_rows: walk.balances.len() as u64,
                    item_claims: walk.items.len() as u64,
                    new_receipts: new_receipts.len() as u64,
                };
                Ok(SweepPassReport {
                    started_at_ms: now_ms,
                    finished_at_ms: now_ms,
                    cursor_before: cursor_before.last_receipt_key,
                    cursor_after,
                    population,
                    findings,
                })
            })
            .await
            .map_err(|error: foundationdb::FdbBindingError| {
                SweepError(format!("sweep pass: {error}"))
            })?;

        Ok(SweepPassReport {
            finished_at_ms: now_ms,
            ..report
        })
    }

    async fn collect_balances(
        trx: &foundationdb::Transaction,
        structural: &mut Vec<AuditFinding>,
    ) -> Result<Vec<BalanceRow>, foundationdb::FdbBindingError> {
        let mut rows = Vec::new();
        let pairs = Self::scan_range(
            trx,
            keyspace::ledger_bal_range_start(),
            keyspace::ledger_bal_range_end(),
        )
        .await?;
        for (key, value) in pairs {
            if key.len() != keyspace::ledger_bal_key(AccountId::new(0), AssetId::new(0)).len()
                || value.len() != 16
            {
                structural.push(AuditFinding {
                    kind: FindingKind::UndecodableLedgerRow,
                    item: None,
                    account: None,
                    asset: None,
                    receipt_intent_id: None,
                    key_hex: hex(&key),
                    detail: format!(
                        "balance row with key of {} bytes and value of {} bytes; \
                         a well-formed row is 18 and 16",
                        key.len(),
                        value.len()
                    ),
                });
                continue;
            }
            let account = AccountId::new(u64::from_be_bytes(
                key[2..10].try_into().expect("length checked"),
            ));
            let asset = AssetId::new(u64::from_be_bytes(
                key[10..18].try_into().expect("length checked"),
            ));
            rows.push(BalanceRow {
                account,
                asset,
                value: i128::from_le_bytes(value.try_into().expect("length checked")),
            });
        }
        Ok(rows)
    }

    async fn collect_items(
        trx: &foundationdb::Transaction,
        structural: &mut Vec<AuditFinding>,
    ) -> Result<Vec<ItemClaim>, foundationdb::FdbBindingError> {
        let mut claims = Vec::new();
        let pairs = Self::scan_range(
            trx,
            keyspace::ledger_item_range_start(),
            keyspace::ledger_item_range_end(),
        )
        .await?;
        for (key, value) in pairs {
            // The uid is whatever the key names, however long the key grew:
            // key-spelling drift is exactly how one item comes to have two
            // rows, and refusing to decode long keys would blind the
            // single-ownership arm to the duplicates worth catching.
            if key.len() < 10 || value.is_empty() {
                structural.push(AuditFinding {
                    kind: FindingKind::UndecodableLedgerRow,
                    item: None,
                    account: None,
                    asset: None,
                    receipt_intent_id: None,
                    key_hex: hex(&key),
                    detail: format!(
                        "ownership row with key of {} bytes; a claim needs at \
                         least the two discriminator bytes plus eight uid bytes",
                        key.len()
                    ),
                });
                continue;
            }
            match postcard::from_bytes::<crate::keyspace::ItemRow>(&value) {
                Ok(row) => claims.push(ItemClaim {
                    item: ItemUid::new(u64::from_be_bytes(
                        key[2..10].try_into().expect("length checked"),
                    )),
                    owner: row.owner,
                    key_len: key.len(),
                }),
                Err(error) => structural.push(AuditFinding {
                    kind: FindingKind::UndecodableLedgerRow,
                    item: None,
                    account: None,
                    asset: None,
                    receipt_intent_id: None,
                    key_hex: hex(&key),
                    detail: format!("ownership row does not decode as ItemRow: {error}"),
                }),
            }
        }
        Ok(claims)
    }

    async fn collect_new_receipts(
        trx: &foundationdb::Transaction,
        cursor: &AuditCursorRow,
        structural: &mut Vec<AuditFinding>,
    ) -> Result<Vec<JudgedReceipt>, foundationdb::FdbBindingError> {
        let begin = if cursor.last_receipt_key.is_empty() {
            keyspace::ledger_receipt_range_start()
        } else {
            // Strictly after the cursor: append a zero byte to make the
            // bound exclusive even when the cursor names a real receipt key.
            let mut next = cursor.last_receipt_key.clone();
            next.push(0);
            next
        };
        let mut receipts = Vec::new();
        let pairs = Self::scan_range(trx, begin, keyspace::ledger_receipt_range_end()).await?;
        for (key, value) in pairs {
            match keyspace::decode_receipt_row(&value) {
                Ok((row, _version)) => receipts.push(JudgedReceipt { key, row }),
                Err(error) => structural.push(AuditFinding {
                    kind: FindingKind::UndecodableLedgerRow,
                    item: None,
                    account: None,
                    asset: None,
                    receipt_intent_id: None,
                    key_hex: hex(&key),
                    detail: format!("receipt row does not decode as ReceiptRow: {error}"),
                }),
            }
        }
        Ok(receipts)
    }

    /// Walk every gap between the registered economic sub-spans — the places
    /// nothing legitimate writes — and report anything found there.
    ///
    /// The D35 story is why these gaps exist as a concept: a constructor that
    /// shares the family byte without declaring its sub-span lands its rows
    /// here, and pre-D35 `lease_key` was the live instance. The gaps exclude
    /// the auditor's own cursor span ([`keyspace::
    /// ledger_audit_cursor_range_start`]).
    async fn collect_strays(
        trx: &foundationdb::Transaction,
    ) -> Result<Vec<AuditFinding>, foundationdb::FdbBindingError> {
        // Inclusive-start/exclusive-end pairs covering [l, m) minus the five
        // registered sub-spans: the audit cursor's own [la, lb), balances
        // [lb, lc), lease registrar [le, lf), items [li, lj), receipts
        // [lr, ls).
        let gaps: [(Vec<u8>, Vec<u8>); 5] = [
            (
                vec![b'l', 0x00],
                keyspace::ledger_audit_cursor_range_start(),
            ),
            (keyspace::ledger_bal_range_end(), vec![b'l', b'e']),
            (vec![b'l', b'f'], keyspace::ledger_item_range_start()),
            (
                keyspace::ledger_item_range_end(),
                keyspace::ledger_receipt_range_start(),
            ),
            (
                keyspace::ledger_receipt_range_end(),
                keyspace::ledger_range_end(),
            ),
        ];

        let mut findings = Vec::new();
        for (start, end) in gaps {
            for (key, value) in Self::scan_range(trx, start, end).await? {
                findings.push(AuditFinding {
                    kind: FindingKind::StrayLedgerRow,
                    item: None,
                    account: None,
                    asset: None,
                    receipt_intent_id: None,
                    key_hex: hex(&key),
                    detail: format!(
                        "row of {} value bytes inside the ledger family but \
                         outside every registered economic sub-span",
                        value.len()
                    ),
                });
            }
        }
        Ok(findings)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn claim(item: u64, owner: u64) -> ItemClaim {
        ItemClaim {
            item: ItemUid::new(item),
            owner: AccountId::new(owner),
            key_len: 10,
        }
    }

    fn balance(account: u64, asset: u64, value: i128) -> BalanceRow {
        BalanceRow {
            account: AccountId::new(account),
            asset: AssetId::new(asset),
            value,
        }
    }

    fn receipt(key_tail: u8, intent: u128, parties: &[u64]) -> JudgedReceipt {
        JudgedReceipt {
            key: {
                let mut key = keyspace::ledger_receipt_key().to_vec();
                key[2] = key_tail;
                key
            },
            row: ReceiptRow {
                intent_id: intent,
                parties: parties
                    .iter()
                    .map(|account| AccountId::new(*account))
                    .collect(),
                ops: vec![],
                balance_deltas: vec![],
                ownership: vec![],
            },
        }
    }

    /// A clean ledger raises nothing: every arm is silent when no invariant is
    /// contradicted. This is the not-noisy half of the acceptance evidence.
    #[test]
    fn a_clean_ledger_reports_nothing() {
        let walk = LedgerWalk {
            balances: vec![
                balance(1, 100, 5_000),
                balance(2, 100, 700),
                balance(2, 200, 1),
            ],
            items: vec![claim(7, 1), claim(9, 2)],
            new_receipts: vec![receipt(0x01, 42, &[1, 2])],
            structural: Vec::new(),
        };

        assert!(
            evaluate_pass(&walk).is_empty(),
            "a ledger every invariant holds over must raise no finding"
        );
    }

    /// THE guarded stage: two ownership rows naming one item are one finding,
    /// naming the item — and the finding survives distinct key spellings,
    /// which is how a second row for one uid can exist at all beside FDB's
    /// exact-key uniqueness.
    #[test]
    fn duplicate_ownership_rows_are_found_and_name_the_item() {
        let mut long_spelling = keyspace::ledger_item_key(ItemUid::new(7)).to_vec();
        long_spelling.push(b'x');
        let walk = LedgerWalk {
            balances: Vec::new(),
            items: vec![
                ItemClaim {
                    item: ItemUid::new(7),
                    owner: AccountId::new(1),
                    key_len: long_spelling.len(),
                },
                claim(7, 2),
                claim(8, 3),
            ],
            new_receipts: Vec::new(),
            structural: Vec::new(),
        };

        let findings = evaluate_pass(&walk);
        assert_eq!(findings.len(), 1, "exactly the duplicated item is reported");
        assert_eq!(findings[0].kind, FindingKind::DuplicateOwnershipRow);
        assert_eq!(findings[0].item, Some(ItemUid::new(7)));
        assert!(
            findings[0].detail.contains("item 7"),
            "the finding names the item: {}",
            findings[0].detail
        );
        assert!(findings[0].detail.contains('2'), "it names the row count");
    }

    /// Three rows for one item are still one finding about that item, and an
    /// unrelated item's single row stays out of the report.
    #[test]
    fn triplicated_ownership_is_one_finding_and_singletons_stay_silent() {
        let walk = LedgerWalk {
            balances: Vec::new(),
            items: vec![claim(5, 1), claim(5, 2), claim(5, 3), claim(6, 4)],
            new_receipts: Vec::new(),
            structural: Vec::new(),
        };

        let findings = evaluate_pass(&walk);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].item, Some(ItemUid::new(5)));
    }

    /// A negative balance is impossible under correct operation and is
    /// reported with its account and asset.
    #[test]
    fn negative_balances_are_found() {
        let walk = LedgerWalk {
            balances: vec![balance(11, 77, -500), balance(12, 77, 0)],
            items: Vec::new(),
            new_receipts: Vec::new(),
            structural: Vec::new(),
        };

        let findings = evaluate_pass(&walk);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].kind, FindingKind::NegativeBalance);
        assert_eq!(findings[0].account, Some(AccountId::new(11)));
        assert_eq!(findings[0].asset, Some(AssetId::new(77)));
        assert!(
            !findings[0].key_hex.is_empty(),
            "the evidence key is carried"
        );
    }

    /// A receipt whose party has no balance row and no owned item is
    /// unbacked; a party present either way is not.
    #[test]
    fn unbacked_receipt_parties_are_found_and_backed_ones_are_not() {
        let walk = LedgerWalk {
            balances: vec![balance(1, 100, 10)],
            items: vec![claim(3, 2)],
            new_receipts: vec![receipt(0x01, 100, &[1, 2]), receipt(0x02, 101, &[1, 99])],
            structural: Vec::new(),
        };

        let findings = evaluate_pass(&walk);
        assert_eq!(findings.len(), 1, "party 99 alone lacks presence");
        assert_eq!(findings[0].kind, FindingKind::UnbackedReceiptParty);
        assert_eq!(findings[0].account, Some(AccountId::new(99)));
        assert_eq!(findings[0].receipt_intent_id, Some(101));
    }

    /// Presence is defined once: any balance row counts, even a zero one, as
    /// does owning any item.
    #[test]
    fn presence_is_a_balance_row_or_an_owned_item_even_at_zero() {
        let walk = LedgerWalk {
            balances: vec![balance(21, 100, 0)],
            items: vec![claim(30, 22)],
            new_receipts: vec![receipt(0x01, 300, &[21, 22])],
            structural: Vec::new(),
        };

        assert!(evaluate_pass(&walk).is_empty());
    }
}
