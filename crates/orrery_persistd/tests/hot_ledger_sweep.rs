//! The hot-ledger incremental sweep (#330) against a live cluster.
//!
//! Two layers:
//!
//! - **The invariants, end to end** (`fdb` feature, live cluster): a sweep
//!   over a clean seeded ledger reports nothing attributable to it; a planted
//!   duplicate ownership row is found and names the item; a negative balance
//!   and an unbacked receipt party are found. The unit tier proves the same
//!   predicates over collected data — this tier proves the FDB adapter feeds
//!   them what it claims to.
//! - **The cursor** (`fdb` feature): receipts banked after pass 1 are judged
//!   by pass 2 and not re-judged, across a sweeper rebuilt from scratch —
//!   which is "restart", because the only state that survives is the durable
//!   cursor row.
//!
//! `emit_time_to_detection` (ignored; run explicitly) measures time-to-
//! detection over a stated population and prints it, optionally writing
//! `docs/data/hot-ledger-sweep-ttd-<date>.json`. It is the *measurement*
//! that lets #224 settle cadence and a target — it settles neither.
//!
//! **These tests share one cluster and one durable cursor row**, so they hold
//! [`CLUSTER_LOCK`] for their whole body: two sweeps interleaving through the
//! same cursor would each consume the other's tail, which is interference,
//! not coverage. Findings asserted here are scoped to rows these tests
//! seeded, because a development cluster pointed at by hand may carry
//! unrelated history a correct sweep *should* still report; CI's per-run
//! throwaway cluster makes the scoped assertion the whole report.

#![cfg(feature = "fdb")]

mod support;

use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use orrery_persistd::audit::{AuditFinding, FindingKind, HotLedgerSweeper, SWEEP_REPORT_SCHEMA};
use orrery_persistd::keyspace;
use orrery_persistd::FdbContext;
use orrery_protocol::{AccountId, AssetId, ItemUid};

/// Serializes every gated test in this binary: one cluster, one cursor row.
static CLUSTER_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Unix milliseconds now.
fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock after epoch")
        .as_millis() as u64
}

/// An item uid unique to this process and instant.
///
/// Test-seeded rows share whatever cluster the suite points at, so uids are
/// minted wide rather than counted from zero — a uid colliding with a foreign
/// row would make this suite report on someone else's data.
fn unique_item(round: u32) -> ItemUid {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock after epoch");
    ItemUid::new(
        elapsed.as_secs().rotate_left(32)
            ^ u64::from(elapsed.subsec_nanos())
            ^ (u64::from(std::process::id()) << 40)
            ^ (u64::from(round) << 24),
    )
}

fn item_row(owner: u64) -> keyspace::ItemRow {
    keyspace::ItemRow {
        owner: AccountId::new(owner),
        state: vec![1, 2, 3],
    }
}

const GHOST_LOW_BITS: u64 = 0x0A;

/// This process run's identity slice for every row it writes.
///
/// The suite may point at a **persistent** development cluster that already
/// holds rows planted by earlier runs of this very suite (AGENTS.md: whatever
/// you write stays). Assertions therefore name only what *this* run seeded:
/// accounts and the asset carry a run tag in their high bits, and every item
/// uid minted here is recorded in [`SEEDED_ITEMS`] so a finding can be
/// attributed. Leftovers from earlier runs keep being swept and reported —
/// that is the sweeper working — but they no longer decide these tests.
fn run_tag() -> u64 {
    static TAG: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *TAG.get_or_init(|| {
        let elapsed = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock after epoch");
        ((u64::from(elapsed.as_millis() as u32) << 20)
            ^ u64::from(elapsed.subsec_nanos())
            ^ (u64::from(std::process::id()) << 44))
            & 0x000F_FFFF_FFFF_FFFF
    })
}

fn tagged(low: u64) -> u64 {
    (run_tag() << 8) | low
}

fn buyer() -> AccountId {
    AccountId::new(tagged(0x01))
}

fn seller() -> AccountId {
    AccountId::new(tagged(0x02))
}

/// The account the negative-balance arm plants an overdraft on.
fn debtor() -> AccountId {
    AccountId::new(tagged(0x03))
}

/// A receipt party nothing backs: seeded nowhere, owned by nobody.
fn ghost() -> AccountId {
    AccountId::new(tagged(GHOST_LOW_BITS))
}

fn asset() -> AssetId {
    AssetId::new(tagged(0xB1))
}

/// Seed clean rows: balances for two accounts and four items owned by them.
///
/// Everything goes through the same constructors and encodings the intent
/// path writes with, so a finding here means the invariant, not an encoding
/// the sweep misread. Returns the four item uids minted, so the caller can
/// scope its assertions to exactly what it wrote.
async fn seed_clean_ledger(db: &foundationdb::Database) -> Vec<ItemUid> {
    let asset = asset();

    let minted: Vec<ItemUid> = db
        .run(|trx, _| async move {
            for (account, value) in [(buyer(), 5_000_i64), (seller(), 9_000_i64)] {
                let key = keyspace::ledger_bal_key(account, asset);
                trx.atomic_op(
                    &key,
                    &i128::from(value).to_le_bytes(),
                    foundationdb::options::MutationType::Add,
                );
            }
            let minted: Vec<ItemUid> = (0..4_u32)
                .map(|n| {
                    let item = unique_item(n);
                    let key = keyspace::ledger_item_key(item);
                    trx.set(
                        &key,
                        &postcard::to_stdvec(&item_row(if n % 2 == 0 {
                            buyer().0
                        } else {
                            seller().0
                        }))
                        .unwrap(),
                    );
                    item
                })
                .collect();
            Ok(minted)
        })
        .await
        .expect("seed ledger");
    minted
}

/// Bank one trade receipt in its own transaction.
///
/// One per transaction is not a style choice: every versionstamped write in a
/// transaction gets the same commit versionstamp, so N receipts banked
/// together are one key written N times — one row. The intent path banks one
/// receipt per committed intent for exactly this reason.
async fn bank_receipt(db: &foundationdb::Database, parties: &[AccountId]) {
    db.run(|trx, _| async move {
        let param = keyspace::ledger_receipt_versionstamped_key();
        let row = keyspace::ReceiptRow {
            intent_id: u128::from(unix_ms()),
            parties: parties.to_vec(),
            ops: vec![],
        };
        trx.atomic_op(
            &param,
            &postcard::to_stdvec(&row).unwrap(),
            foundationdb::options::MutationType::SetVersionstampedKey,
        );
        Ok(())
    })
    .await
    .expect("bank receipt");
}

/// Plant a duplicate ownership row for `item`: a second row in the item
/// sub-span whose key names the same uid at a different spelling.
///
/// This is how two ownership rows for one item can physically coexist beside
/// FoundationDB's exact-key uniqueness — the canonical ten-byte row plus a
/// drifted-spelling twin — and it is the shape of defect a migration or a
/// buggy second writer would leave behind.
async fn plant_duplicate_ownership(db: &foundationdb::Database, item: ItemUid, owner: u64) {
    db.run(|trx, _| async move {
        let mut drifted = keyspace::ledger_item_key(item).to_vec();
        drifted.push(b'#');
        trx.set(&drifted, &postcard::to_stdvec(&item_row(owner)).unwrap());
        Ok(())
    })
    .await
    .expect("plant duplicate");
}

/// Open the process's FDB context and a sweeper over it.
///
/// `None` means no cluster is configured: the caller skips, loudly.
fn open_sweeper() -> Option<(HotLedgerSweeper, Arc<foundationdb::Database>)> {
    let cluster = support::fdb_cluster_file()?;
    eprintln!("fdb cluster: {cluster}");
    let context = FdbContext::connect(&cluster).expect("configured FDB cluster file must open");
    let db = context.database();
    Some((HotLedgerSweeper::from_context(&context), db))
}

/// Whether a finding names only what one test seeded.
///
/// Each test passes the accounts and items it wrote; findings about anything
/// else belong to whatever history the cluster already had — including rows
/// planted by a sibling test moments ago — and reporting them is the sweeper
/// working, not the test failing.
fn names_only(finding: &AuditFinding, accounts: &[AccountId], items: &[ItemUid]) -> bool {
    let account_is_ours = finding
        .account
        .is_some_and(|account| accounts.contains(&account));
    let item_is_ours = finding.item.is_some_and(|item| items.contains(&item));
    item_is_ours || account_is_ours
}

/// A sweep over a clean ledger raises nothing attributable to it — the check
/// is not merely noisy.
#[tokio::test]
async fn fdb_clean_ledger_reports_no_findings() {
    let _guard = CLUSTER_LOCK.lock().await;
    let Some((sweeper, db)) = open_sweeper() else {
        eprintln!("skipping: ORRERY_FDB_CLUSTER_FILE is absent");
        return;
    };
    let minted = seed_clean_ledger(&db).await;
    bank_receipt(&db, &[buyer(), seller()]).await;
    bank_receipt(&db, &[buyer(), seller()]).await;

    let report = sweeper.run_pass(unix_ms()).await.expect("pass runs");
    let scope = [buyer(), seller()];
    let ours: Vec<_> = report
        .findings
        .iter()
        .filter(|finding| names_only(finding, &scope, &minted))
        .collect();
    assert!(
        ours.is_empty(),
        "a clean ledger must raise no findings, got: {ours:?}"
    );
    assert!(report.population.balance_rows >= 2);
    assert!(report.population.item_claims >= 4);
    assert!(report.population.new_receipts >= 2);
}

/// The acceptance evidence's first line: a planted duplicate ownership row is
/// detected, naming the item.
#[tokio::test]
async fn fdb_planted_duplicate_ownership_row_is_found_and_names_the_item() {
    let _guard = CLUSTER_LOCK.lock().await;
    let Some((sweeper, db)) = open_sweeper() else {
        eprintln!("skipping: ORRERY_FDB_CLUSTER_FILE is absent");
        return;
    };
    seed_clean_ledger(&db).await;

    let duplicated = unique_item(77);
    let canonical = keyspace::ledger_item_key(duplicated);
    let rival = tagged(0x09);
    db.run(|trx, _| async move {
        trx.set(
            &canonical,
            &postcard::to_stdvec(&item_row(buyer().0)).unwrap(),
        );
        Ok(())
    })
    .await
    .expect("seed canonical owner");

    plant_duplicate_ownership(&db, duplicated, rival).await;

    let report = sweeper.run_pass(unix_ms()).await.expect("pass runs");
    let duplicates: Vec<_> = report
        .findings
        .iter()
        .filter(|finding| {
            finding.kind == FindingKind::DuplicateOwnershipRow && finding.item == Some(duplicated)
        })
        .collect();
    assert_eq!(
        duplicates.len(),
        1,
        "exactly the planted duplicate for item {}: {:?}",
        duplicated.0,
        report.findings
    );
    assert!(
        duplicates[0].detail.contains("2 ownership rows"),
        "the finding counts the rows: {}",
        duplicates[0].detail
    );
    assert!(
        duplicates[0].detail.contains(&buyer().0.to_string())
            && duplicates[0].detail.contains(&rival.to_string()),
        "the finding names both owners ({} and {}): {}",
        buyer().0,
        rival,
        duplicates[0].detail
    );
}

/// A negative balance and an unbacked receipt party are found live.
#[tokio::test]
async fn fdb_negative_balance_and_unbacked_receipt_are_found() {
    let _guard = CLUSTER_LOCK.lock().await;
    let Some((sweeper, db)) = open_sweeper() else {
        eprintln!("skipping: ORRERY_FDB_CLUSTER_FILE is absent");
        return;
    };
    seed_clean_ledger(&db).await;

    let ghost = ghost();
    let asset = asset();
    db.run(|trx, _| async move {
        let key = keyspace::ledger_bal_key(debtor(), asset);
        trx.atomic_op(
            &key,
            &(-250_i128).to_le_bytes(),
            foundationdb::options::MutationType::Add,
        );
        Ok(())
    })
    .await
    .expect("plant negative balance");
    bank_receipt(&db, &[ghost]).await;

    let report = sweeper.run_pass(unix_ms()).await.expect("pass runs");
    assert!(
        report
            .findings
            .iter()
            .any(|finding| finding.kind == FindingKind::NegativeBalance
                && finding.account == Some(debtor())),
        "negative balance reported: {:?}",
        report.findings
    );
    assert!(
        report
            .findings
            .iter()
            .any(|finding| finding.kind == FindingKind::UnbackedReceiptParty
                && finding.account == Some(ghost)),
        "unbacked party named: {:?}",
        report.findings
    );
}

/// The cursor resumes correctly across a restart: receipts judged before the
/// restart are not re-judged, receipts banked after are.
///
/// Counts are relative to the pass that wrote the cursor, never absolute:
/// whatever the cluster held before this test began sits behind the cursor by
/// the time pass one returns, and asserting otherwise would measure the
/// cluster's history rather than the resume.
#[tokio::test]
async fn fdb_cursor_resumes_across_restart() {
    let _guard = CLUSTER_LOCK.lock().await;
    let Some(cluster) = support::fdb_cluster_file() else {
        eprintln!("skipping: ORRERY_FDB_CLUSTER_FILE is absent");
        return;
    };
    let first_context = FdbContext::connect(&cluster).expect("configured cluster opens");
    let sweeper_one = HotLedgerSweeper::from_context(&first_context);
    seed_clean_ledger(&first_context.database()).await;

    // Pass one. `sweeper_one` is dropped afterwards — the durable cursor row
    // is the only thing carried into the next block, which is what a restart
    // keeps too.
    let cursor_after_first = {
        let report = sweeper_one.run_pass(unix_ms()).await.expect("first pass");
        assert!(
            !report.cursor_after.is_empty(),
            "the cursor advanced past at least one receipt"
        );
        report.cursor_after.clone()
    };
    drop(sweeper_one);

    // Bank two more receipts "after the restart", each in its own transaction.
    let second_context = FdbContext::connect(
        support::fdb_cluster_file()
            .as_deref()
            .expect("still configured"),
    )
    .expect("reopen");
    bank_receipt(&second_context.database(), &[buyer(), seller()]).await;
    bank_receipt(&second_context.database(), &[buyer(), seller()]).await;

    // A fresh sweeper — a restarted process — resumes from the durable row.
    let sweeper_two = HotLedgerSweeper::from_context(&second_context);
    let durable = sweeper_two.read_cursor().await.expect("cursor readable");
    assert_eq!(
        durable.last_receipt_key, cursor_after_first,
        "the durable cursor survived the restart"
    );

    let second = sweeper_two.run_pass(unix_ms()).await.expect("second pass");
    assert_eq!(
        second.population.new_receipts, 2,
        "exactly the post-restart receipts were judged"
    );
    assert_eq!(
        second.cursor_before, cursor_after_first,
        "the pass began where the last one ended"
    );
}

/// Measure time-to-detection over a stated population.
///
/// ```sh
/// ORRERY_FDB_CLUSTER_FILE=… cargo test -p orrery_persistd --features fdb \
///     -- --ignored --nocapture emit_time_to_detection
/// ```
///
/// One duplicate ownership row becomes readable at a known wall-clock time in
/// each round, planted at a rotating offset inside the sweep interval so the
/// samples are not phase-locked to the pass boundary; detection is the moment
/// the first pass reports *that* item. The artifact carries every raw sample,
/// the population walked, and the pass durations — the denominators without
/// which none of these numbers is a measurement. It states no target and
/// settles no cadence: both remain #224's.
#[tokio::test]
#[ignore]
async fn emit_time_to_detection() {
    let _guard = CLUSTER_LOCK.lock().await;
    const ROUNDS: usize = 12;
    const SWEEP_INTERVAL_MS: u64 = 150;

    let Some((sweeper, db)) = open_sweeper() else {
        eprintln!("skipping: ORRERY_FDB_CLUSTER_FILE is absent");
        return;
    };
    seed_clean_ledger(&db).await;
    sweeper.run_pass(unix_ms()).await.expect("warm-up pass");

    let mut raw_samples_ms: Vec<u64> = Vec::with_capacity(ROUNDS);
    let mut pass_durations_ms: Vec<u64> = Vec::new();

    for round in 0..ROUNDS {
        // Rotate the plant position through the interval so consecutive
        // rounds do not all sit the same distance from a pass boundary.
        let offset = (u64::from(round as u32) * 37) % SWEEP_INTERVAL_MS;
        tokio::time::sleep(Duration::from_millis(offset)).await;
        let planted_at = unix_ms();
        let violation = unique_item(1_000 + round as u32);
        // A duplicate is TWO ownership rows: the canonical one and the
        // drifted twin that breaks single ownership. Both become readable
        // now.
        let rival = tagged(0x09);
        db.run(|trx, _| async move {
            trx.set(
                &keyspace::ledger_item_key(violation),
                &postcard::to_stdvec(&item_row(buyer().0)).unwrap(),
            );
            Ok(())
        })
        .await
        .expect("seed canonical owner");
        plant_duplicate_ownership(&db, violation, rival).await;

        tokio::time::sleep(Duration::from_millis(SWEEP_INTERVAL_MS - offset)).await;
        let pass_started = Instant::now();
        let report = sweeper.run_pass(unix_ms()).await.expect("measured pass");
        let detected_at = unix_ms();
        pass_durations_ms.push(pass_started.elapsed().as_millis() as u64);

        let caught = report.findings.iter().any(|finding| {
            finding.kind == FindingKind::DuplicateOwnershipRow && finding.item == Some(violation)
        });
        assert!(
            caught,
            "round {round}: the planted violation for item {} was reported",
            violation.0
        );
        raw_samples_ms.push(detected_at.saturating_sub(planted_at));
        tokio::time::sleep(Duration::from_millis(SWEEP_INTERVAL_MS)).await;
    }

    let population = sweeper
        .run_pass(unix_ms())
        .await
        .expect("final population pass")
        .population;

    let mut sorted = raw_samples_ms.clone();
    sorted.sort_unstable();
    let percentile = |fraction: f64| -> u64 {
        debug_assert!(!sorted.is_empty());
        let index = ((sorted.len() as f64 * fraction) as usize).min(sorted.len() - 1);
        sorted[index]
    };

    let artifact = serde_json::json!({
        "schema": SWEEP_REPORT_SCHEMA,
        "provenance": {
            "traffic": "harness",
            "source": format!(
                "emit_time_to_detection on {} (pid {})",
                hostname(),
                std::process::id()
            ),
            "note": "violations are planted duplicate ownership rows; detection is the \
                     first sweep pass reporting them. Cadence and any time-to-detection \
                     target remain #224's; this file supplies the measurement only.",
        },
        "measurement": {
            "definition": "detected_at_ms − planted_at_ms, where planted_at_ms is when the \
                           violating row became readable and detection is the first sweep \
                           pass reporting it",
            "sweep_interval_ms": SWEEP_INTERVAL_MS,
            "samples": raw_samples_ms.len(),
            "missed_within_window": 0_usize,
            "ttd_ms_min": sorted.first().copied().unwrap_or(0),
            "ttd_ms_median": percentile(0.5),
            "ttd_ms_p95": percentile(0.95),
            "ttd_ms_max": sorted.last().copied().unwrap_or(0),
            "raw_ttd_samples_ms": raw_samples_ms,
        },
        "population": {
            "balance_rows": population.balance_rows,
            "item_claims": population.item_claims,
            "receipts_judged_final_pass": population.new_receipts,
            "note": "rows the sweep walks per pass; the population the findings are over",
        },
        "passes": {
            "count": pass_durations_ms.len(),
            "duration_ms_sum": pass_durations_ms.iter().sum::<u64>(),
            "duration_ms_max": pass_durations_ms.iter().max().copied().unwrap_or(0),
        },
    });

    let rendered = serde_json::to_string_pretty(&artifact).expect("artifact serializes") + "\n";
    println!("{rendered}");

    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../docs/data")
        .join(format!("hot-ledger-sweep-ttd-{}.json", today()));
    match std::fs::write(&path, rendered.as_bytes()) {
        Ok(()) => eprintln!("wrote {}", path.display()),
        Err(error) => eprintln!(
            "could not write {}: {error} — the printed figures are the record",
            path.display()
        ),
    }
}

fn hostname() -> String {
    std::fs::read_to_string("/etc/hostname")
        .map(|name| name.trim().to_owned())
        .unwrap_or_else(|_| "unknown".to_owned())
}

/// Days-since-epoch to `YYYY-MM-DD`, civil-from-days (Howard Hinnant).
fn today() -> String {
    let days = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("after epoch")
        .as_secs()
        / 86_400;
    let z = i64::try_from(days + 719_468).expect("date");
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let m = if m <= 2 { m + 12 } else { m };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}
