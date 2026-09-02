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
use orrery_persistd::gateway::{SharedBindingAuthority, SnapshotBindingAuthority};
use orrery_persistd::intent::AttestationEnforcement;
use orrery_persistd::witness_epoch::WitnessEpochAuthority;
use orrery_persistd::{keyspace, FdbIntentExecutor, IntentExecutor};
use orrery_protocol::{
    AccountId, CellEpoch, CellId, CoordinatorInterestSnapshot, GridId, Intent, IntentOp,
    IntentOutcome, IssuerKey, IssuerKeyId, NodeId, WitnessEpochClaimsV1, WitnessEpochV1,
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
///
/// Calling this **twice for one handle** models a second gateway over the same
/// announcement, and a test that then expects the second one's intents to be
/// refused wants [`stale_successor`], not this: a fresh key is not a diverging
/// draw, and the difference is issue #288.
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

/// D31 clause (e)'s `owner(n)`, answered from a table: one distinct account
/// per announced witness.
///
/// Every fixture intent here carries one `Ruleset`-opaque op and names no
/// ledger account, so `P(I)` is empty and no announced witness is a party.
/// What the resolver has to supply is the other half of D31 clause (f): a
/// candidate whose binding does not resolve is **excluded**, so a fixture with
/// no bindings at all would record an empty `E(I)` and these tests would be
/// asserting against a vacuum.
fn fixture_bindings(witnesses: &[iroh_base::SecretKey]) -> SharedBindingAuthority {
    Arc::new(SnapshotBindingAuthority::from_bindings(
        witnesses
            .iter()
            .enumerate()
            .map(|(index, key)| (key.public(), AccountId::new(1_000 + index as u64))),
    ))
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

/// A second gateway's cache over the same announcement, minted so that its
/// draw **really** diverges from the durable one — for every intent this test
/// will attest under it.
///
/// # Why a loop and not a second `accepted_epoch` call
///
/// Every stale-draw-key test in this file asserts a *refusal*, and the thing
/// that earns the refusal is not that the two draw keys differ. It is that
/// `required_witnesses(durable, id, E)` names witnesses the intent's
/// co-signatures — solicited under the successor's own key — do not cover.
/// `keyed_hash` over two independent keys is two independent orderings of `E`,
/// so with `K = 3` of `|E| = 7` the two draws name the *same three witnesses*
/// with probability `1 / C(7,3) = 1/35`, measured at 2.8% and matching to
/// three digits. When that happens the intent genuinely carries the subset the
/// durable key requires, the executor's re-proof passes because it *should*,
/// and the intent commits — a green production path failing a fixture that
/// asserted an accident. Five such assertions per run put the file's failure
/// rate at `1 - (34/35)^5 ≈ 13%`, which is issue #288's "1 in 5".
///
/// So the divergence is a property the fixture has to *establish*, not one it
/// may assume. Re-minting until it holds costs ~35/34 accepts on average and
/// makes every refusal below deterministic.
///
/// # Why the intent ids are declared up front
///
/// The divergence is per intent id — it is `id` that goes into the hash — so a
/// cache proven to diverge for one id says nothing about the next. Handing
/// back a bare `WitnessEpochAuthority` would let the next test attest a new id
/// under a cache nobody checked it against, and the flake would come back
/// looking exactly like a fresh bug. [`StaleSuccessor::intent`] is therefore
/// the only way to attest under this cache, and it refuses an id that was not
/// in the set the constructor proved.
struct StaleSuccessor {
    epochs: Arc<WitnessEpochAuthority>,
    handle: u64,
    diverging: Vec<u128>,
}

impl StaleSuccessor {
    /// The cache itself, for the assertions about adoption and commitment.
    fn epochs(&self) -> &Arc<WitnessEpochAuthority> {
        &self.epochs
    }

    /// An intent co-signed under this successor's *stale* key, for one of the
    /// ids the constructor proved the draw diverges on.
    fn intent(
        &self,
        id: u128,
        key: &iroh_base::SecretKey,
        witnesses: &[iroh_base::SecretKey],
    ) -> Intent {
        assert!(
            self.diverging.contains(&id),
            "intent {id:#x} was not among the ids `stale_successor` proved this \
             cache's draw diverges on — declare it there, or its refusal is a \
             1-in-35 coin flip"
        );
        attested_intent(id, key, self.handle, &self.epochs, witnesses)
    }
}

/// Mint successor caches over the announcement for `(epoch, handle)` until one
/// draws a different required subset from `durable` for **every** id in
/// `intents`.
fn stale_successor(
    epoch: u32,
    handle: u64,
    durable: &[u8; 32],
    issuer: NodeId,
    witnesses: &[iroh_base::SecretKey],
    intents: &[u128],
) -> StaleSuccessor {
    assert!(
        !intents.is_empty(),
        "a successor with nothing to attest proves nothing"
    );
    let announced: Vec<NodeId> = witnesses.iter().map(iroh_base::SecretKey::public).collect();
    let eligible = orrery_protocol::eligible_witnesses(&announced, issuer);
    assert!(
        eligible.len() > orrery_protocol::WITNESS_QUORUM_K,
        "with |E| == K every draw names the whole of E and no key can diverge"
    );
    let required_under = |draw_key: &[u8; 32], id: u128| -> Vec<[u8; 32]> {
        // The executor's re-proof asks whether every member of the durable
        // draw is *among* the attestations, so what has to differ is the set,
        // not the order it came back in. Comparing the ordered vector would
        // accept a same-set/different-order pair as "diverging" and let the
        // flake through at 1/35 minus 1/210.
        let mut names: Vec<[u8; 32]> = orrery_protocol::required_witnesses(draw_key, id, &eligible)
            .into_iter()
            .map(|node| *node.as_bytes())
            .collect();
        names.sort_unstable();
        names
    };
    let durable_required: Vec<Vec<[u8; 32]>> = intents
        .iter()
        .map(|id| required_under(durable, *id))
        .collect();

    // 400 tries is `(34/35)^400 < 1e-5` for a single id — a cap that reports a
    // broken draw rather than hanging, not a retry budget.
    for _ in 0..400 {
        let (candidate, _) = accepted_epoch(epoch, handle);
        let minted = *candidate.resolve(handle).expect("cached").draw_key();
        assert_ne!(
            &minted, durable,
            "two independent gateways must not mint the same key, or these \
             tests prove nothing"
        );
        if intents
            .iter()
            .zip(&durable_required)
            .all(|(id, under_durable)| &required_under(&minted, *id) != under_durable)
        {
            return StaleSuccessor {
                epochs: candidate,
                handle,
                diverging: intents.to_vec(),
            };
        }
    }
    panic!(
        "no minted draw key diverged from the durable one for {intents:x?} in \
         400 tries — the draw is not behaving like a keyed hash"
    );
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
        .recording_epochs(Arc::clone(&epochs), fixture_bindings(&witnesses));
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
        .recording_epochs(Arc::clone(&epochs), fixture_bindings(&witnesses));
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

/// The same "off records nothing" property, for the gateway shape #863
/// introduced — an executor that *has* the epoch cache and is merely posted
/// `off`.
///
/// [`an_executor_with_no_epoch_cache_records_nothing`] proves the old
/// structural reason: `off` was handed no cache, so `record_witness_epoch`'s
/// `let else` caught it. A durable gateway now retains its C1 wiring at `off`
/// so a later posture row arms admission and the commit-time re-proof
/// together, which means that `let else` no longer fires and the property has
/// to be carried by the posture itself. Without the guard this test's first
/// half writes `epoch/`, the handle index and an `AttestRow { enforced:
/// false }` — bytes an auditor cannot tell apart from a shadow commit, in the
/// one mode D32 clause (d) says writes nothing.
///
/// The second half is the other side of the same seam: flipping the cell —
/// which is exactly what the C1 poller does when a durable row arrives —
/// starts the recording without rebuilding the executor.
#[tokio::test]
async fn a_durable_off_executor_that_holds_an_epoch_cache_still_records_nothing() {
    let Some(cluster) = fdb_cluster_file() else {
        eprintln!("skipping: ORRERY_FDB_CLUSTER_FILE not set and no .fdb-dev/fdb.cluster");
        return;
    };
    let handle = 0x9601_0000_0000_000B;
    let (epochs, witnesses) = accepted_epoch(11, handle);
    let posture = orrery_persistd::intent::AttestationPosture::new(AttestationEnforcement::Off);
    let exec = FdbIntentExecutor::connect(&cluster, GridId::new(GRID))
        .unwrap()
        .tracking_posture(
            Arc::clone(&epochs),
            fixture_bindings(&witnesses),
            posture.clone(),
        );
    let db = Arc::clone(exec.database());
    reset(&db, 11, handle, &[0x9601_00B1, 0x9601_00B2]).await;

    let key = secret(19);
    let intent = attested_intent(0x9601_00B1, &key, handle, &epochs, &witnesses);
    assert!(matches!(
        exec.execute(&intent).await.unwrap(),
        IntentOutcome::Committed { .. }
    ));
    assert_eq!(
        read_key(&db, keyspace::attest_key(intent.intent_id).to_vec()).await,
        None,
        "`off` must write no AttestRow even when the epoch cache is wired"
    );
    assert_eq!(
        read_key(&db, keyspace::epoch_handle_key(handle).to_vec()).await,
        None,
        "`off` must not make the draw commitment durable"
    );

    // The poller's promotion, on the cell the executor already holds.
    posture.set(AttestationEnforcement::Shadow);
    let intent = attested_intent(0x9601_00B2, &key, handle, &epochs, &witnesses);
    assert!(matches!(
        exec.execute(&intent).await.unwrap(),
        IntentOutcome::Committed { .. }
    ));
    assert!(
        read_key(&db, keyspace::attest_key(intent.intent_id).to_vec())
            .await
            .is_some(),
        "promoting the cell to shadow must start recording without a restart"
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
        .recording_epochs(Arc::clone(&sibling_epochs), fixture_bindings(&witnesses));
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

    let durable = *sibling_epochs.resolve(handle).expect("cached").draw_key();
    let successor = stale_successor(
        5,
        handle,
        &durable,
        key.public(),
        &witnesses,
        &[0x9601_0042, 0x9601_0043],
    );
    let exec_b = FdbIntentExecutor::connect(&cluster, GridId::new(GRID))
        .unwrap()
        .recording_epochs(Arc::clone(successor.epochs()), fixture_bindings(&witnesses));

    // Two intents attested under the successor's own (stale) key, both
    // admitted before it learns anything.
    let in_flight = successor.intent(0x9601_0042, &key, &witnesses);
    let discovers = successor.intent(0x9601_0043, &key, &witnesses);

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
    // This one goes through `attested_intent` rather than `StaleSuccessor`
    // because by now the cache has adopted the durable key, so its draw is
    // meant to *agree*: it is the control, not a stale-key case.
    assert_eq!(
        successor
            .epochs()
            .resolve(handle)
            .expect("cached")
            .draw_key(),
        &durable,
        "the adoption is what makes the next intent a control rather than a \
         third stale-key case"
    );
    assert!(matches!(
        exec_b
            .execute(&attested_intent(
                0x9601_0044,
                &key,
                handle,
                successor.epochs(),
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
        .recording_epochs(Arc::clone(&before), fixture_bindings(&witnesses));
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

    // Restart: a new cache over the same announcement, a new draw key — and
    // one whose draw for the intent below really names a different subset,
    // which is what the refusal at the end of this test rests on.
    let after = stale_successor(
        6,
        handle,
        before.resolve(handle).expect("cached").draw_key(),
        key_for_replay().public(),
        &witnesses,
        &[0x9601_0052],
    );
    let restarted = FdbIntentExecutor::connect(&cluster, GridId::new(GRID))
        .unwrap()
        .recording_epochs(Arc::clone(after.epochs()), fixture_bindings(&witnesses));
    assert_ne!(
        after.epochs().resolve(handle).expect("cached").draw_key(),
        before.resolve(handle).expect("cached").draw_key()
    );

    // The replay returns the recorded outcome without touching step 2c.
    assert!(matches!(
        restarted.execute(&intent).await.unwrap(),
        IntentOutcome::Committed { .. }
    ));
    assert!(
        !after
            .epochs()
            .resolve(handle)
            .expect("cached")
            .is_committed(),
        "a replay certifies nothing about the epoch row"
    );

    // So the very next real intent still reaches the row — and is refused,
    // because its subset was drawn under the freshly minted key rather than
    // the durable one.
    let fresh = after.intent(0x9601_0052, &key_for_replay(), &witnesses);
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
        .recording_epochs(Arc::clone(&sibling_epochs), fixture_bindings(&witnesses));
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

    // A different key is necessary but not sufficient: what refuses the
    // contested intent is a different *required subset*, and two independent
    // keys name the same three witnesses once in 35. `stale_successor` mints
    // until they do not.
    let successor = stale_successor(
        4,
        handle,
        &durable,
        key.public(),
        &witnesses,
        &[0x9601_0032],
    );

    let exec_b = FdbIntentExecutor::connect(&cluster, GridId::new(GRID))
        .unwrap()
        .recording_epochs(Arc::clone(successor.epochs()), fixture_bindings(&witnesses));
    let contested = successor.intent(0x9601_0032, &key, &witnesses);
    assert_eq!(
        exec_b.execute(&contested).await.unwrap(),
        IntentOutcome::Rejected {
            reason: orrery_protocol::REASON_ATTESTATION_QUORUM
        },
        "its required subset was drawn under a key that was never this \
         cell-epoch's, so it is refused rather than committed on a bad draw"
    );
    assert_eq!(
        successor
            .epochs()
            .resolve(handle)
            .expect("cached")
            .draw_key(),
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

    // Drawn under the adopted key, so this one is a control: `attested_intent`
    // deliberately, not `StaleSuccessor::intent`.
    let resubmitted = attested_intent(0x9601_0033, &key, handle, successor.epochs(), &witnesses);
    assert!(matches!(
        exec_b.execute(&resubmitted).await.unwrap(),
        IntentOutcome::Committed { .. }
    ));
}

/// D10 item 4's account half reaches the **recorded** vector, not only
/// admission: a party account's NodeId is absent from `AttestRow.eligible`,
/// and the survivors are still in announced order.
///
/// The reason this needs a cluster is the reason D27 clause (f) exists. `E(I)`
/// is derived twice — once by the validator to decide, once by this executor
/// to record — and only the durable row can show that the second derivation
/// applied the same exclusion as the first. A recorded vector wider than the
/// admitted one would let an auditor redraw `required(I)` over members the
/// gateway never drew from, which is the audit convicting an honest gateway.
#[tokio::test]
async fn a_party_accounts_node_id_is_absent_from_the_recorded_eligible_vector() {
    let Some(cluster) = fdb_cluster_file() else {
        eprintln!("skipping: ORRERY_FDB_CLUSTER_FILE not set and no .fdb-dev/fdb.cluster");
        return;
    };
    let handle = 0x9601_0000_0000_0007;
    let (epochs, witnesses) = accepted_epoch(7, handle);
    let announced: Vec<NodeId> = witnesses.iter().map(iroh_base::SecretKey::public).collect();

    // The submitter, and one announced witness that is its own second device:
    // a different NodeId, the same account. Index 2 rather than 0 so the
    // survivors have to keep their order across a hole in the middle.
    let key = secret(17);
    const PARTY: AccountId = AccountId(151_700);
    const PARTY_INDEX: usize = 2;
    let mut pairs: Vec<(NodeId, AccountId)> = announced
        .iter()
        .enumerate()
        .map(|(index, node)| (*node, AccountId::new(1_000 + index as u64)))
        .collect();
    pairs[PARTY_INDEX].1 = PARTY;
    pairs.push((key.public(), PARTY));
    let bindings: SharedBindingAuthority = Arc::new(SnapshotBindingAuthority::from_bindings(pairs));

    let exec = FdbIntentExecutor::connect(&cluster, GridId::new(GRID))
        .unwrap()
        .recording_epochs(Arc::clone(&epochs), Arc::clone(&bindings));
    let db = Arc::clone(exec.database());
    reset(&db, 7, handle, &[0x9601_0061]).await;

    // What the gateway would have admitted over: the announced set minus the
    // party's device, in announced order.
    let expected: Vec<NodeId> = announced
        .iter()
        .enumerate()
        .filter(|(index, _)| *index != PARTY_INDEX)
        .map(|(_, node)| *node)
        .collect();

    let mut intent = Intent {
        evidence: None,
        intent_id: 0x9601_0061,
        issuer: key.public(),
        cell_epoch: CellEpoch::new(handle),
        ops: vec![IntentOp {
            op: 100,
            args: Bytes::new(),
        }],
        attestations: Vec::new(),
        signature: key.sign(b"placeholder"),
    };
    intent.sign(&key);
    let cached = epochs.resolve(handle).expect("cached");
    for node in cached.required_witnesses(intent.intent_id, &expected) {
        let witness = witnesses
            .iter()
            .find(|w| w.public() == node)
            .expect("the draw names only announced witnesses");
        let attestation = intent.attest(witness);
        intent.attestations.push(attestation);
    }
    assert!(matches!(
        exec.execute(&intent).await.unwrap(),
        IntentOutcome::Committed { .. }
    ));

    let attest: keyspace::AttestRow = postcard::from_bytes(
        &read_key(&db, keyspace::attest_key(intent.intent_id).to_vec())
            .await
            .expect("attest row"),
    )
    .expect("attest row decodes");
    assert!(
        !attest.eligible.contains(&announced[PARTY_INDEX]),
        "a NodeId bound to a party account is not in the audited vector"
    );
    assert_eq!(
        attest.eligible, expected,
        "and the survivors keep announced order — the recorded vector is the \
         object an auditor draws over, not a normalization of it"
    );
    assert_ne!(
        attest.eligible,
        orrery_protocol::eligible_witnesses(&cached.snapshot.selected, intent.issuer),
        "the NodeId-only derivation would have recorded the party's device, \
         so this assertion is what makes the previous two mean something"
    );
}

// ── D32 clause (d): the shadow arm, at the executor ──────────────────────
//
// The admission half is unit-tested against the validator. What only a
// cluster can show is the pair of durable consequences the record decides:
// what a shadow commit's `attest/` row says about itself, and that the
// commit-time re-proof — the second place a below-quorum intent can die — is
// disarmed under shadow. An arm that admitted at admission and refused at
// commit would be acting after saying it would not, which is the failure
// this file's fixtures are already shaped to catch.

/// A signed intent naming `handle`, carrying **no** co-signatures.
///
/// The below-quorum case: `required` refuses it at admission and, if it ever
/// reached the executor, at commit. Shadow commits it, which is the whole
/// question these two tests ask of the durable rows.
fn unattested_intent(id: u128, key: &iroh_base::SecretKey, handle: u64) -> Intent {
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
    intent
}

#[tokio::test]
async fn a_shadow_commit_records_an_attest_row_an_auditor_can_tell_apart() {
    let Some(cluster) = fdb_cluster_file() else {
        eprintln!("skipping: ORRERY_FDB_CLUSTER_FILE not set and no .fdb-dev/fdb.cluster");
        return;
    };
    // D32 clause (d)'s decision, both halves in one test because each is only
    // meaningful against the other. Omitting the row would leave a whole
    // shadow period unauditable against D27 clause (f); writing it unmarked
    // would fabricate an audit trail claiming the cluster stood behind a
    // quorum it deliberately waived. The marker is what makes the story
    // coherent: insufficient co-signatures, admitted by policy, observed and
    // not trusted.
    let handle = 0x9601_0000_0000_0009;
    let (epochs, witnesses) = accepted_epoch(9, handle);
    let exec = FdbIntentExecutor::connect(&cluster, GridId::new(GRID))
        .unwrap()
        .shadowing_epochs(Arc::clone(&epochs), fixture_bindings(&witnesses));
    let db = Arc::clone(exec.database());
    reset(&db, 9, handle, &[0x9601_0071, 0x9601_0072]).await;

    let key = secret(17);
    let bare = unattested_intent(0x9601_0071, &key, handle);
    assert!(
        bare.attestations.is_empty(),
        "the fixture must really be below quorum"
    );
    assert!(
        matches!(
            exec.execute(&bare).await.unwrap(),
            IntentOutcome::Committed { .. }
        ),
        "shadow commits an intent `required` would refuse"
    );

    let shadow_row: keyspace::AttestRow = postcard::from_bytes(
        &read_key(&db, keyspace::attest_key(bare.intent_id).to_vec())
            .await
            .expect("a shadow commit still records its eligible vector"),
    )
    .expect("attest row decodes");
    assert!(
        !shadow_row.enforced,
        "the marker is what stops the row reading as an enforced commit"
    );
    assert_eq!(shadow_row.epoch_handle, handle);
    assert_eq!(
        shadow_row.eligible,
        orrery_protocol::eligible_witnesses(
            &epochs.resolve(handle).expect("cached").snapshot.selected,
            bare.issuer
        ),
        "and it records the same `E(I)` an enforced commit would, so the \
         audit reads one shape of row in both postures"
    );

    // The control, on the same cell-epoch and the same durable draw key: an
    // enforcing executor's row is marked `true`. Without this the assertion
    // above is satisfied by a field that is always `false`.
    let enforcing = FdbIntentExecutor::connect(&cluster, GridId::new(GRID))
        .unwrap()
        .recording_epochs(Arc::clone(&epochs), fixture_bindings(&witnesses));
    let attested = attested_intent(0x9601_0072, &key, handle, &epochs, &witnesses);
    assert!(matches!(
        enforcing.execute(&attested).await.unwrap(),
        IntentOutcome::Committed { .. }
    ));
    let enforced_row: keyspace::AttestRow = postcard::from_bytes(
        &read_key(&db, keyspace::attest_key(attested.intent_id).to_vec())
            .await
            .expect("attest row"),
    )
    .expect("attest row decodes");
    assert!(enforced_row.enforced);
}

#[tokio::test]
async fn a_shadow_executor_does_not_refuse_at_commit_what_admission_admitted() {
    let Some(cluster) = fdb_cluster_file() else {
        eprintln!("skipping: ORRERY_FDB_CLUSTER_FILE not set and no .fdb-dev/fdb.cluster");
        return;
    };
    // The stale-draw-key scenario, run twice: once with the re-proof armed
    // and once in shadow. The armed run is the control — it is the landed
    // behaviour, and it is what proves the fixture really does trip the
    // re-proof rather than passing it. The shadow run is the assertion: the
    // same intent, against the same durable row and the same adopted key,
    // commits.
    //
    // This is the "must not act" half at the executor. A shadow arm that
    // admitted at admission and refused here would be the worst of both
    // modes, and it is the failure mode D32 clause (d) names from the far
    // side of clause (b).
    let handle = 0x9601_0000_0000_000A;
    let (sibling_epochs, witnesses) = accepted_epoch(10, handle);
    let owner = FdbIntentExecutor::connect(&cluster, GridId::new(GRID))
        .unwrap()
        .recording_epochs(Arc::clone(&sibling_epochs), fixture_bindings(&witnesses));
    let db = Arc::clone(owner.database());
    reset(&db, 10, handle, &[0x9601_0081, 0x9601_0082, 0x9601_0083]).await;

    // The sibling mints the durable key.
    let key = secret(18);
    assert!(matches!(
        owner
            .execute(&attested_intent(
                0x9601_0081,
                &key,
                handle,
                &sibling_epochs,
                &witnesses
            ))
            .await
            .unwrap(),
        IntentOutcome::Committed { .. }
    ));

    // A successor with its own, different key, and two intents attested under
    // it — both below quorum against the *durable* draw. "Below quorum" is the
    // property that has to be established rather than assumed: a successor
    // whose draw happened to name the durable three would put both intents
    // *at* quorum, and the armed control below would commit.
    let durable_key = *sibling_epochs.resolve(handle).expect("cached").draw_key();
    let successor = stale_successor(
        10,
        handle,
        &durable_key,
        key.public(),
        &witnesses,
        &[0x9601_0082, 0x9601_0083],
    );
    let stale_for_armed = successor.intent(0x9601_0082, &key, &witnesses);
    let stale_for_shadow = successor.intent(0x9601_0083, &key, &witnesses);

    let armed = FdbIntentExecutor::connect(&cluster, GridId::new(GRID))
        .unwrap()
        .recording_epochs(Arc::clone(successor.epochs()), fixture_bindings(&witnesses));
    assert_eq!(
        armed.execute(&stale_for_armed).await.unwrap(),
        IntentOutcome::Rejected {
            reason: orrery_protocol::REASON_ATTESTATION_QUORUM
        },
        "the control: armed, the re-proof refuses this exact intent"
    );

    // A fresh cache again, so the shadow executor starts from the same stale
    // key the armed one did rather than from the adoption it just made.
    let (shadow_epochs, _) = accepted_epoch(10, handle);
    let shadow = FdbIntentExecutor::connect(&cluster, GridId::new(GRID))
        .unwrap()
        .shadowing_epochs(Arc::clone(&shadow_epochs), fixture_bindings(&witnesses));
    assert!(
        matches!(
            shadow.execute(&stale_for_shadow).await.unwrap(),
            IntentOutcome::Committed { .. }
        ),
        "shadow commits it: the re-proof refuses below-quorum commits, and \
         shadow commits below quorum on purpose"
    );

    // The adoption still happened, in both modes. The cache must converge on
    // the durable key regardless of posture, or a later promotion to
    // `required` would begin against a key this gateway never adopted.
    let durable: keyspace::EpochRow = postcard::from_bytes(
        &read_key(
            &db,
            keyspace::epoch_key(GridId::new(GRID), cell(), 10).to_vec(),
        )
        .await
        .expect("epoch row"),
    )
    .expect("epoch row decodes");
    assert_eq!(
        *shadow_epochs.resolve(handle).expect("cached").draw_key(),
        durable.draw_key,
        "shadow adopts the durable draw key; only the refusal is suppressed"
    );

    let row: keyspace::AttestRow = postcard::from_bytes(
        &read_key(
            &db,
            keyspace::attest_key(stale_for_shadow.intent_id).to_vec(),
        )
        .await
        .expect("attest row"),
    )
    .expect("attest row decodes");
    assert!(!row.enforced);
}
