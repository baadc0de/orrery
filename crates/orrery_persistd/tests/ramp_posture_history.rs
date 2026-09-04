//! D32 open question 2's durable posture-change history, measured against a
//! live FoundationDB cluster.
//!
//! What is asserted here is what a *cluster's bytes* do across processes:
//! that a posture change recorded through one store handle is still in the
//! history after every handle that saw it is gone — the "durable" of the
//! slice — that the span orders by commit version and not by any writer's
//! clock, and that one control's history scan never leaks a row belonging to
//! a control whose name extends its own. None of that is falsifiable against
//! a memory store, which is why the store methods' unit-testable halves
//! (key layout, value round-trip, rendering) live elsewhere and only the
//! restart property lands here.
//!
//! These tests self-skip without a cluster, like every other FDB-gated test
//! in this crate. They write to dedicated probe control names, never to a
//! real D32 control's row, and clean their own spans before and after.

#![cfg(feature = "fdb")]

use orrery_persistd::intent::posture::{sign_posture, PostureChange, SignedRampPosture};
use orrery_persistd::intent::{FdbRampPostureStore, PostureHistoryEntry, PostureSource};
use orrery_persistd::intent::{RampMode, RampPosture};
use orrery_persistd::{keyspace, FdbContext};

/// The cluster file for the FDB-gated tests, or `None` if not configured.
///
/// Honors `ORRERY_FDB_CLUSTER_FILE`; otherwise walks up to the workspace
/// root's `.fdb-dev/fdb.cluster` (tests run with CWD = the crate dir).
fn fdb_cluster_file() -> Option<String> {
    if let Ok(path) = std::env::var("ORRERY_FDB_CLUSTER_FILE") {
        return Some(path);
    }
    let mut dir = std::env::current_dir().ok()?;
    loop {
        let candidate = dir.join(".fdb-dev/fdb.cluster");
        if candidate.exists() {
            return Some(candidate.display().to_string());
        }
        if !dir.pop() {
            return None;
        }
    }
}

/// The signer every probe row carries.
///
/// A made-up key: the history records who signed, and this test's answer is
/// deterministic rather than real.
fn operator() -> iroh::SecretKey {
    iroh::SecretKey::from_bytes(&[9; 32])
}

/// A signed `set` row for a probe control.
fn signed(control: &str, mode: RampMode, reason: &str) -> SignedRampPosture {
    sign_posture(
        control,
        RampPosture {
            mode,
            source: PostureSource::Operator,
            set_at_ms: 1_000,
            reason: reason.to_owned(),
            incident_id: None,
        },
        None,
        &operator(),
    )
}

/// Delete every row of a probe's history span and its posture row, raw.
///
/// Deliberately *not* [`FdbRampPostureStore::clear`]: that method now appends
/// a `Cleared` history row — it is the behaviour under test — so cleaning
/// through it would grow the very span this helper is trying to empty, and a
/// rerun would see the last run's tombstones.
async fn clean_span(context: &FdbContext, control: &str) {
    let db = context.database();
    let start = keyspace::posture_history_range_start(control);
    let end = keyspace::posture_history_range_end(control);
    let row_key = keyspace::ramp_key(control);
    db.run(move |trx, _| {
        let (start, end, row_key) = (start.clone(), end.clone(), row_key.clone());
        async move {
            use futures::TryStreamExt;
            let mut stream = trx.get_ranges_keyvalues(
                foundationdb::RangeOption {
                    begin: foundationdb::KeySelector::first_greater_or_equal(&start),
                    end: foundationdb::KeySelector::first_greater_or_equal(&end),
                    ..foundationdb::RangeOption::default()
                },
                false,
            );
            let mut doomed = Vec::new();
            while let Some(kv) = stream.try_next().await? {
                doomed.push(kv.key().to_vec());
            }
            for key in doomed {
                trx.clear(&key);
            }
            trx.clear(&row_key);
            Ok(())
        }
    })
    .await
    .expect("clean probe span");
}

/// The one recorded change is the set that was written, envelope included,
/// read back through a handle that never saw the write.
#[tokio::test]
async fn a_posture_change_history_survives_the_process_that_wrote_it() {
    let Some(cluster) = fdb_cluster_file() else {
        eprintln!("skipping: ORRERY_FDB_CLUSTER_FILE not set and no .fdb-dev/fdb.cluster");
        return;
    };
    let context = FdbContext::connect(&cluster).expect("connect");
    let control = "histprb01";
    clean_span(&context, control).await;

    let row = signed(control, RampMode::Shadow, "p1 swarm incident 4471");
    FdbRampPostureStore::from_context(&context)
        .write(control, &row)
        .await
        .expect("the write records");

    // A fresh handle: the history the first handle wrote lives in the
    // cluster, not in any process — which is the whole claim "durable" makes
    // and the thing the in-process posture row never could.
    let reloaded = FdbRampPostureStore::from_context(&context);
    let entries: Vec<PostureHistoryEntry> = reloaded.history(control).await.expect("read back");
    assert_eq!(entries.len(), 1, "exactly the one recorded change");
    assert_ne!(
        entries[0].versionstamp, [0; 10],
        "FDB substituted a real versionstamp"
    );
    assert!(
        entries[0].row.recorded_at_ms > 0,
        "the writer's clock is wired, not left at a default"
    );
    let PostureChange::Set(recorded) = &entries[0].row.change else {
        panic!("expected the recorded set, got {:?}", entries[0].row.change);
    };
    assert_eq!(recorded.posture.mode, RampMode::Shadow);
    assert_eq!(recorded.posture.reason, "p1 swarm incident 4471");
    assert_eq!(recorded.signer, Some(operator().public()));
    assert_eq!(recorded.expires_at_ms, None);

    // The clear is a recorded event too, and it survives the same way: an
    // un-ended incident is what a silent clear would read as.
    reloaded.clear(control).await.expect("the clear records");
    let after = FdbRampPostureStore::from_context(&context)
        .history(control)
        .await
        .expect("read back");
    assert_eq!(after.len(), 2, "set and clear are both history");
    assert!(matches!(after[1].row.change, PostureChange::Cleared));
    assert!(
        after[1].versionstamp > after[0].versionstamp,
        "the clear commits after the set, in key order"
    );

    clean_span(&context, control).await;
}

/// The span orders by commit version, and each row carries the who and the
/// why of the write that appended it.
#[tokio::test]
async fn the_history_orders_by_commit_and_names_who_what_why() {
    let Some(cluster) = fdb_cluster_file() else {
        eprintln!("skipping: ORRERY_FDB_CLUSTER_FILE not set and no .fdb-dev/fdb.cluster");
        return;
    };
    let context = FdbContext::connect(&cluster).expect("connect");
    let control = "histprb02";
    clean_span(&context, control).await;

    let store = FdbRampPostureStore::from_context(&context);
    store
        .write(
            control,
            &signed(control, RampMode::Live, "clause (e) review"),
        )
        .await
        .expect("first change");
    store
        .write(
            control,
            &signed(control, RampMode::Off, "handover to the operator"),
        )
        .await
        .expect("second change");

    let entries = store.history(control).await.expect("read back");
    assert_eq!(entries.len(), 2, "both changes, oldest first");
    assert!(
        entries[0].versionstamp < entries[1].versionstamp,
        "the span orders by commit version, not by any writer's clock"
    );
    for (entry, reason) in entries
        .iter()
        .zip(["clause (e) review", "handover to the operator"])
    {
        let PostureChange::Set(recorded) = &entry.row.change else {
            panic!("expected a recorded set, got {:?}", entry.row.change);
        };
        assert_eq!(recorded.posture.reason, reason, "each row names its why");
        assert_eq!(
            recorded.signer,
            Some(operator().public()),
            "each row names its who"
        );
    }

    clean_span(&context, control).await;
}

/// The `0x00` separator terminates the control name in the key, so a control
/// whose name extends another's never leaks its rows into that other's scan.
#[tokio::test]
async fn one_control_s_history_scan_never_leaks_an_extending_control_s_rows() {
    let Some(cluster) = fdb_cluster_file() else {
        eprintln!("skipping: ORRERY_FDB_CLUSTER_FILE not set and no .fdb-dev/fdb.cluster");
        return;
    };
    let context = FdbContext::connect(&cluster).expect("connect");
    let control = "histprb03";
    let extending = "histprb03x";
    clean_span(&context, control).await;
    clean_span(&context, extending).await;

    let store = FdbRampPostureStore::from_context(&context);
    store
        .write(
            control,
            &signed(control, RampMode::Live, "the real control"),
        )
        .await
        .expect("first control's change");
    store
        .write(
            extending,
            &signed(extending, RampMode::Live, "the impostor"),
        )
        .await
        .expect("extending control's change");

    let own = store.history(control).await.expect("read back");
    assert_eq!(
        own.len(),
        1,
        "the scan ends at the separator, so the extending control's row stays out"
    );
    let PostureChange::Set(recorded) = &own[0].row.change else {
        panic!("expected a recorded set");
    };
    assert_eq!(recorded.posture.reason, "the real control");
    assert_eq!(
        store.history(extending).await.expect("read back").len(),
        1,
        "and the extending control's own history is intact"
    );

    clean_span(&context, control).await;
    clean_span(&context, extending).await;
}
