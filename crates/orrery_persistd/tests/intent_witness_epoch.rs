//! The durable half of D27's K-of-N enforcement: the `epoch/` record and the
//! recorded eligible vector (D27 clauses (d) and (f), D28 clause (f)).
//!
//! The admission predicate itself is unit-tested against the validator — it
//! needs no cluster, which is the point of keeping it off the FoundationDB
//! round trip. What only a live cluster can show is the part the audit
//! depends on:
//!
//! - **The draw commitment is durable, and it opens under the key admission
//!   actually drew with.** Without this an audit of the draw is theatre: a
//!   gateway could pick the key after seeing which attestations arrived.
//! - **The commitment lands in the transaction the first intent already
//!   runs.** D27 requires the commitment to precede any admission in the
//!   cell-epoch and D28 requires the record to cost the intent p99 no extra
//!   round trip, and those two are reconciled by *atomicity* rather than by
//!   ordering: no intent's effects become durable before the commitment does,
//!   because they are the same commit.
//! - **`E(I)` is recorded per committed intent** and reads back in announced
//!   order. This is D27 clause (f) item 5 — the item that is easy to skip and
//!   fatal to omit, because party exclusion depends on bindings that move and
//!   an audit recomputing `E(I)` later could convict an honest gateway.
//! - **The steady state pays nothing.** Every intent after the first in a
//!   cell-epoch writes no epoch row and reads no index.
//!
//! Every test here is `fdb`-gated and self-skips without a cluster, which is
//! why `scripts/fdb-tests.sh` — not `cargo test` — is what makes them mean
//! anything.

#![cfg(feature = "fdb")]

use std::sync::Arc;

use bytes::Bytes;
use orrery_persistd::witness_epoch::WitnessEpochAuthority;
use orrery_persistd::{keyspace, FdbIntentExecutor, IntentExecutor};
use orrery_protocol::{
    CellEpoch, CellId, CoordinatorInterestSnapshot, GridId, Intent, IntentOp, IntentOutcome,
    IssuerKey, IssuerKeyId, NodeId, WitnessEpochClaimsV1, WitnessEpochV1,
};

/// This file's own grid, so its `epoch/` and `pid/next` rows never touch
/// another test's namespace on the shared dev cluster.
const GRID: u32 = 9601;

/// The cell every announcement here names. `CellId::ROOT` is fine because the
/// grid discriminator is what separates this file's rows from everyone's —
/// which is the D22 property `epoch_key` exists to restore.
fn cell() -> CellId {
    CellId::ROOT
}

fn secret(n: u8) -> iroh_base::SecretKey {
    let mut seed = [0u8; 32];
    seed[0] = n;
    iroh_base::SecretKey::from_bytes(&seed)
}

/// The coordinator whose signature the gateway is configured to trust.
fn coordinator() -> iroh_base::SecretKey {
    secret(200)
}

/// An [`orrery_persistd::gateway::InterestAuthority`] that covers everything.
///
/// D28 clause (d) step 6 is exercised in the cache's own unit tests; here it
/// would only be a way for these tests to fail for an unrelated reason.
#[derive(Debug, Default)]
struct CoverAllInterest;

impl orrery_persistd::gateway::InterestAuthority for CoverAllInterest {
    fn allows(&self, _peer: NodeId, _grid: GridId, _cell: CellId, _now_ms: u64) -> bool {
        true
    }

    fn snapshot_for(&self, _peer: NodeId) -> Option<CoordinatorInterestSnapshot> {
        None
    }
}

/// A signed announcement selecting `selected` for `(GRID, cell(), epoch)`.
fn announcement(epoch: u32, handle: u64, selected: &[NodeId]) -> Vec<u8> {
    let grid = GridId::new(GRID);
    let mut candidates = selected.to_vec();
    candidates.sort_by_key(|node| *node.as_bytes());
    let claims = WitnessEpochClaimsV1::new(
        grid,
        cell(),
        epoch,
        handle,
        30_000,
        30_000,
        candidates,
        selected.to_vec(),
        orrery_protocol::witness_epoch_commitment(grid, cell(), epoch, &[7u8; 32]),
        None,
        IssuerKeyId::new(1),
    );
    WitnessEpochV1::sign(claims, &coordinator())
        .expect("claims encode")
        .encode()
        .expect("envelope encodes")
}

/// A cache holding one accepted epoch over seven announced witnesses.
///
/// Each test takes its **own** epoch counter, not merely its own handle: the
/// durable row is keyed by `(grid, cell, epoch)`, so two tests sharing a
/// counter would share a row and the second would find the first's draw key
/// already there. Handles are per-test too, because the index is global.
fn accepted_epoch(
    epoch: u32,
    handle: u64,
) -> (Arc<WitnessEpochAuthority>, Vec<iroh_base::SecretKey>) {
    let witnesses: Vec<iroh_base::SecretKey> = (0..7).map(|i| secret(100 + i)).collect();
    let announced: Vec<NodeId> = witnesses.iter().map(iroh_base::SecretKey::public).collect();
    let epochs = Arc::new(WitnessEpochAuthority::new([IssuerKey::new(
        IssuerKeyId::new(1),
        coordinator().public(),
    )]));
    epochs
        .apply_announcement(
            &announcement(epoch, handle, &announced),
            secret(1).public(),
            &CoverAllInterest,
            1_000,
        )
        .expect("the fixture announcement is accepted");
    (epochs, witnesses)
}

/// Clear every row a test is about to write.
///
/// The `fdb` tier runs against a real cluster and a re-run must start from the
/// same state as the first run. That is not automatic here and the failure is
/// silent in the worst direction: a leftover `epoch/` row from the previous
/// run carries the previous run's draw key, and the executor would correctly
/// refuse the intent as a key mismatch — a green implementation failing a
/// stale fixture.
async fn reset(db: &foundationdb::Database, epoch: u32, handle: u64, intents: &[u128]) {
    let intents = intents.to_vec();
    db.run(|trx, _| {
        let intents = intents.clone();
        async move {
            trx.clear(&keyspace::epoch_key(GridId::new(GRID), cell(), epoch));
            trx.clear(&keyspace::epoch_handle_key(handle));
            for id in &intents {
                trx.clear(&keyspace::intent_key(*id));
                trx.clear(&keyspace::attest_key(*id));
            }
            Ok(())
        }
    })
    .await
    .expect("reset");
}

/// A signed, co-signed intent naming `handle`.
fn attested_intent(
    id: u128,
    key: &iroh_base::SecretKey,
    handle: u64,
    epochs: &WitnessEpochAuthority,
    witnesses: &[iroh_base::SecretKey],
) -> Intent {
    let mut intent = Intent {
        evidence: None,
        intent_id: id,
        issuer: key.public(),
        cell_epoch: CellEpoch::new(handle),
        ops: vec![IntentOp {
            op: 100,
            args: Bytes::new(),
        }],
        attestations: Vec::new(),
        signature: key.sign(b"placeholder"),
    };
    intent.sign(key);

    let epoch = epochs.resolve(handle).expect("cached");
    let eligible = orrery_protocol::eligible_witnesses(&epoch.snapshot.selected, intent.issuer);
    for node in epoch.required_witnesses(intent.intent_id, &eligible) {
        let witness = witnesses
            .iter()
            .find(|w| w.public() == node)
            .expect("the draw names only announced witnesses");
        let attestation = intent.attest(witness);
        intent.attestations.push(attestation);
    }
    intent
}

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

async fn read_key(db: &foundationdb::Database, key: Vec<u8>) -> Option<Vec<u8>> {
    db.run(|trx, _| {
        let key = key.clone();
        async move { Ok(trx.get(&key, false).await?.map(|v| v.to_vec())) }
    })
    .await
    .expect("read")
}

#[tokio::test]
async fn the_first_intent_of_an_epoch_makes_the_draw_commitment_durable() {
    let Some(cluster) = fdb_cluster_file() else {
        eprintln!("skipping: ORRERY_FDB_CLUSTER_FILE not set and no .fdb-dev/fdb.cluster");
        return;
    };
    let handle = 0x9601_0000_0000_0001;
    let (epochs, witnesses) = accepted_epoch(1, handle);
    let exec = FdbIntentExecutor::connect(&cluster, GridId::new(GRID))
        .unwrap()
        .recording_epochs(Arc::clone(&epochs));
    let db = Arc::clone(exec.database());
    reset(&db, 1, handle, &[0x9601_0001]).await;

    let cached = epochs.resolve(handle).expect("cached");
    assert!(
        !cached.is_committed(),
        "nothing is durable before the first intent runs"
    );
    let minted_key = *cached.draw_key();

    let key = secret(11);
    let intent = attested_intent(0x9601_0001, &key, handle, &epochs, &witnesses);
    assert!(matches!(
        exec.execute(&intent).await.unwrap(),
        IntentOutcome::Committed { .. }
    ));

    // The handle index resolves to the cell-scoped row, which is the whole
    // reason there are two families: an intent carries a handle and nothing
    // else, and an auditor scans by cell.
    let row_key = keyspace::epoch_key(GridId::new(GRID), cell(), 1);
    assert_eq!(
        read_key(&db, keyspace::epoch_handle_key(handle).to_vec()).await,
        Some(row_key.to_vec()),
        "epoch-handle/{{handle}} must name the epoch row"
    );

    let row: keyspace::EpochRow =
        postcard::from_bytes(&read_key(&db, row_key.to_vec()).await.expect("epoch row"))
            .expect("epoch row decodes");

    // The envelope is stored verbatim, so a reader recomputes the coordinator
    // signature from the bytes and trusts neither this gateway nor FDB.
    assert_eq!(row.announcement, cached.announcement);
    assert!(
        orrery_protocol::verify_witness_epoch(
            &row.announcement,
            &[IssuerKey::new(IssuerKeyId::new(1), coordinator().public())],
        )
        .is_ok(),
        "the stored envelope must still verify on its own"
    );

    // The commitment opens under the key the required subset was drawn with.
    // This is the assertion that makes a retrospective audit of the draw
    // non-vacuous.
    assert_eq!(row.draw_key, minted_key);
    assert_eq!(
        row.draw_commit,
        orrery_protocol::attestation_draw_commitment(GridId::new(GRID), cell(), 1, &row.draw_key)
    );
    assert!(row.revealed_key.is_none(), "k_epoch is the coordinator's");
    assert_eq!(row.first_seen_ms, cached.snapshot.first_seen_ms);

    // D27 clause (f) item 5: the eligible vector, in announced order.
    let attest: keyspace::AttestRow = postcard::from_bytes(
        &read_key(&db, keyspace::attest_key(intent.intent_id).to_vec())
            .await
            .expect("attest row"),
    )
    .expect("attest row decodes");
    assert_eq!(attest.epoch_handle, handle);
    assert_eq!(
        attest.eligible,
        orrery_protocol::eligible_witnesses(&cached.snapshot.selected, intent.issuer),
        "the recorded vector is what the gateway drew over, in announced order"
    );
    assert_eq!(
        orrery_protocol::required_witnesses(&row.draw_key, intent.intent_id, &attest.eligible)
            .len(),
        orrery_protocol::WITNESS_QUORUM_K,
        "and an auditor with the row and the record can redraw the subset"
    );
    for attestation in &intent.attestations {
        assert!(
            attest.eligible.contains(&attestation.witness),
            "every counted co-signature came from the recorded eligible vector"
        );
    }

    // The cache now knows the row is durable, which is what removes the index
    // read from every later intent in this cell-epoch.
    assert!(epochs.resolve(handle).expect("cached").is_committed());
}

#[tokio::test]
async fn later_intents_in_one_epoch_add_no_epoch_write_and_keep_the_same_draw_key() {
    let Some(cluster) = fdb_cluster_file() else {
        eprintln!("skipping: ORRERY_FDB_CLUSTER_FILE not set and no .fdb-dev/fdb.cluster");
        return;
    };
    let handle = 0x9601_0000_0000_0002;
    let (epochs, witnesses) = accepted_epoch(2, handle);
    let exec = FdbIntentExecutor::connect(&cluster, GridId::new(GRID))
        .unwrap()
        .recording_epochs(Arc::clone(&epochs));
    let db = Arc::clone(exec.database());
    reset(&db, 2, handle, &[0x9601_0011, 0x9601_0012]).await;
    let key = secret(12);

    let first = attested_intent(0x9601_0011, &key, handle, &epochs, &witnesses);
    exec.execute(&first).await.unwrap();
    let row_key = keyspace::epoch_key(GridId::new(GRID), cell(), 2);
    let after_first = read_key(&db, row_key.to_vec()).await.expect("epoch row");

    let second = attested_intent(0x9601_0012, &key, handle, &epochs, &witnesses);
    assert!(matches!(
        exec.execute(&second).await.unwrap(),
        IntentOutcome::Committed { .. }
    ));

    assert_eq!(
        read_key(&db, row_key.to_vec()).await,
        Some(after_first),
        "the epoch row is written once per cell-epoch, not once per intent — \
         re-minting a draw key would re-roll every outstanding required subset"
    );

    // But each intent records its own eligible vector: the audit is per
    // intent, because `required(I)` is.
    for intent in [&first, &second] {
        assert!(
            read_key(&db, keyspace::attest_key(intent.intent_id).to_vec())
                .await
                .is_some(),
            "every committed intent under an enforced epoch records E(I)"
        );
    }
}

#[tokio::test]
async fn an_executor_with_no_epoch_cache_records_nothing() {
    let Some(cluster) = fdb_cluster_file() else {
        eprintln!("skipping: ORRERY_FDB_CLUSTER_FILE not set and no .fdb-dev/fdb.cluster");
        return;
    };
    // The enforcement switch's off position, all the way down. An executor
    // that banked an eligible vector nobody enforced would be recording
    // evidence about a check that did not happen.
    let handle = 0x9601_0000_0000_0003;
    let (epochs, witnesses) = accepted_epoch(3, handle);
    let exec = FdbIntentExecutor::connect(&cluster, GridId::new(GRID)).unwrap();
    let db = Arc::clone(exec.database());
    reset(&db, 3, handle, &[0x9601_0021]).await;

    let key = secret(13);
    let intent = attested_intent(0x9601_0021, &key, handle, &epochs, &witnesses);
    assert!(matches!(
        exec.execute(&intent).await.unwrap(),
        IntentOutcome::Committed { .. }
    ));

    assert_eq!(
        read_key(&db, keyspace::attest_key(intent.intent_id).to_vec()).await,
        None
    );
    assert_eq!(
        read_key(&db, keyspace::epoch_handle_key(handle).to_vec()).await,
        None
    );
}

#[tokio::test]
async fn an_intent_admitted_under_a_stale_draw_key_is_refused_after_the_adoption() {
    let Some(cluster) = fdb_cluster_file() else {
        eprintln!("skipping: ORRERY_FDB_CLUSTER_FILE not set and no .fdb-dev/fdb.cluster");
        return;
    };
    // The narrow window the key comparison alone does not close. A sibling
    // owned the shard and minted the durable key; this gateway minted its own
    // and *admitted* intents under it; then it discovers the mismatch and
    // adopts. Every intent already in flight now resolves a cache entry whose
    // key matches the row — so a key comparison would wave them through, on a
    // required subset drawn under a key that was never this cell-epoch's.
    let handle = 0x9601_0000_0000_0005;
    let (sibling_epochs, witnesses) = accepted_epoch(5, handle);
    let exec_a = FdbIntentExecutor::connect(&cluster, GridId::new(GRID))
        .unwrap()
        .recording_epochs(Arc::clone(&sibling_epochs));
    reset(
        exec_a.database(),
        5,
        handle,
        &[0x9601_0041, 0x9601_0042, 0x9601_0043, 0x9601_0044],
    )
    .await;
    let key = secret(15);
    exec_a
        .execute(&attested_intent(
            0x9601_0041,
            &key,
            handle,
            &sibling_epochs,
            &witnesses,
        ))
        .await
        .unwrap();

    let (successor_epochs, _) = accepted_epoch(5, handle);
    let exec_b = FdbIntentExecutor::connect(&cluster, GridId::new(GRID))
        .unwrap()
        .recording_epochs(Arc::clone(&successor_epochs));

    // Two intents attested under the successor's own (stale) key, both
    // admitted before it learns anything.
    let in_flight = attested_intent(0x9601_0042, &key, handle, &successor_epochs, &witnesses);
    let discovers = attested_intent(0x9601_0043, &key, handle, &successor_epochs, &witnesses);

    assert_eq!(
        exec_b.execute(&discovers).await.unwrap(),
        IntentOutcome::Rejected {
            reason: orrery_protocol::REASON_ATTESTATION_QUORUM
        },
        "the first to reach the row discovers the mismatch"
    );

    assert_eq!(
        exec_b.execute(&in_flight).await.unwrap(),
        IntentOutcome::Rejected {
            reason: orrery_protocol::REASON_ATTESTATION_QUORUM
        },
        "and the one behind it must not be rescued by the adoption: its \
         required subset was drawn under a key that was never this epoch's, \
         and only re-deriving from the durable key catches that"
    );

    // A fresh intent, drawn under the adopted key, commits — otherwise the
    // assertions above are satisfied by a gateway that refuses everything.
    assert!(matches!(
        exec_b
            .execute(&attested_intent(
                0x9601_0044,
                &key,
                handle,
                &successor_epochs,
                &witnesses
            ))
            .await
            .unwrap(),
        IntentOutcome::Committed { .. }
    ));
}

#[tokio::test]
async fn a_replayed_intent_does_not_certify_an_epoch_row_that_was_never_written() {
    let Some(cluster) = fdb_cluster_file() else {
        eprintln!("skipping: ORRERY_FDB_CLUSTER_FILE not set and no .fdb-dev/fdb.cluster");
        return;
    };
    // The restart case. A gateway commits an intent, restarts, re-accepts the
    // announcement (minting a *fresh* draw key, because the cache is memory),
    // and the next thing it sees is a retransmit of the intent it already
    // committed — ordinary, since intents ride the packet lane. The
    // idempotency row answers that replay without ever running the
    // witness-epoch step, so nothing about it may be read as "the epoch row is
    // durable": doing so would leave the draw commitment unwritten while
    // intents commit under the epoch, which is D27 clause (d)'s ordering rule
    // failing silently.
    let handle = 0x9601_0000_0000_0006;
    let (before, witnesses) = accepted_epoch(6, handle);
    let exec = FdbIntentExecutor::connect(&cluster, GridId::new(GRID))
        .unwrap()
        .recording_epochs(Arc::clone(&before));
    reset(exec.database(), 6, handle, &[0x9601_0051, 0x9601_0052]).await;

    let intent = attested_intent(0x9601_0051, &key_for_replay(), handle, &before, &witnesses);
    assert!(matches!(
        exec.execute(&intent).await.unwrap(),
        IntentOutcome::Committed { .. }
    ));
    let durable_row = read_key(
        exec.database(),
        keyspace::epoch_key(GridId::new(GRID), cell(), 6).to_vec(),
    )
    .await
    .expect("the first intent wrote the row");

    // Restart: a new cache over the same announcement, a new draw key.
    let (after, _) = accepted_epoch(6, handle);
    let restarted = FdbIntentExecutor::connect(&cluster, GridId::new(GRID))
        .unwrap()
        .recording_epochs(Arc::clone(&after));
    assert_ne!(
        after.resolve(handle).expect("cached").draw_key(),
        before.resolve(handle).expect("cached").draw_key()
    );

    // The replay returns the recorded outcome without touching step 2c.
    assert!(matches!(
        restarted.execute(&intent).await.unwrap(),
        IntentOutcome::Committed { .. }
    ));
    assert!(
        !after.resolve(handle).expect("cached").is_committed(),
        "a replay certifies nothing about the epoch row"
    );

    // So the very next real intent still reaches the row — and is refused,
    // because its subset was drawn under the freshly minted key rather than
    // the durable one.
    let fresh = attested_intent(0x9601_0052, &key_for_replay(), handle, &after, &witnesses);
    assert_eq!(
        restarted.execute(&fresh).await.unwrap(),
        IntentOutcome::Rejected {
            reason: orrery_protocol::REASON_ATTESTATION_QUORUM
        }
    );
    assert_eq!(
        read_key(
            restarted.database(),
            keyspace::epoch_key(GridId::new(GRID), cell(), 6).to_vec()
        )
        .await,
        Some(durable_row),
        "and the original row — the one the commitment was published for — is \
         untouched"
    );
}

fn key_for_replay() -> iroh_base::SecretKey {
    secret(16)
}

#[tokio::test]
async fn a_durable_draw_key_from_a_sibling_is_adopted_and_this_intent_is_refused() {
    let Some(cluster) = fdb_cluster_file() else {
        eprintln!("skipping: ORRERY_FDB_CLUSTER_FILE not set and no .fdb-dev/fdb.cluster");
        return;
    };
    // D26's live shard handover, from the successor's side: this gateway
    // accepted the same announcement and minted its *own* draw key, but a
    // sibling got there first and its key is the one every outstanding
    // co-signature was solicited under.
    let handle = 0x9601_0000_0000_0004;
    let (sibling_epochs, witnesses) = accepted_epoch(4, handle);
    let exec_a = FdbIntentExecutor::connect(&cluster, GridId::new(GRID))
        .unwrap()
        .recording_epochs(Arc::clone(&sibling_epochs));
    reset(
        exec_a.database(),
        4,
        handle,
        &[0x9601_0031, 0x9601_0032, 0x9601_0033],
    )
    .await;
    let key = secret(14);
    let first = attested_intent(0x9601_0031, &key, handle, &sibling_epochs, &witnesses);
    exec_a.execute(&first).await.unwrap();
    let durable = *sibling_epochs.resolve(handle).expect("cached").draw_key();

    let (successor_epochs, _) = accepted_epoch(4, handle);
    let minted = *successor_epochs.resolve(handle).expect("cached").draw_key();
    assert_ne!(
        minted, durable,
        "two independent gateways must not mint the same key, or this test \
         proves nothing"
    );

    let exec_b = FdbIntentExecutor::connect(&cluster, GridId::new(GRID))
        .unwrap()
        .recording_epochs(Arc::clone(&successor_epochs));
    let contested = attested_intent(0x9601_0032, &key, handle, &successor_epochs, &witnesses);
    assert_eq!(
        exec_b.execute(&contested).await.unwrap(),
        IntentOutcome::Rejected {
            reason: orrery_protocol::REASON_ATTESTATION_QUORUM
        },
        "its required subset was drawn under a key that was never this \
         cell-epoch's, so it is refused rather than committed on a bad draw"
    );
    assert_eq!(
        successor_epochs.resolve(handle).expect("cached").draw_key(),
        &durable,
        "and the durable key is adopted, so the resubmission is judged right"
    );

    // A refusal must bank nothing. `db.run` commits whatever the closure
    // staged, so the check that produced this refusal has to run *above*
    // `apply_plan` — and the observable consequence is these two rows being
    // absent, not merely the outcome saying `Rejected`.
    let db = exec_b.database();
    assert_eq!(
        read_key(db, keyspace::intent_key(0x9601_0032).to_vec()).await,
        None,
        "a refused intent burns no idempotency row"
    );
    assert_eq!(
        read_key(db, keyspace::attest_key(0x9601_0032).to_vec()).await,
        None,
        "and records no eligible vector for a judgement it did not make"
    );

    let resubmitted = attested_intent(0x9601_0033, &key, handle, &successor_epochs, &witnesses);
    assert!(matches!(
        exec_b.execute(&resubmitted).await.unwrap(),
        IntentOutcome::Committed { .. }
    ));
}
