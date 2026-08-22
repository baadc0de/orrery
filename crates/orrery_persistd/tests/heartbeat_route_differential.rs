//! The batched renewal route answers exactly what routing by `locate` did.
//!
//! `<CellRuntime as Router>::heartbeat_leases` stopped resolving each entry's
//! location out of the lease store and asks the actor that owns the
//! *presented* cell instead, falling back to a locate only for entries that
//! actor has no row for. What has to be preserved is not "the cached cell
//! equals a fresh one" but the **answer**: the same `Option<Lease>` in the
//! same position, over an enumerated state matrix.
//!
//! `CellRuntime::heartbeat_leases_via_locate` is the pre-change body, retained
//! verbatim as the oracle. Both are compared over every combination of where
//! the row actually lives and what token the holder presents, each scenario
//! run twice: alone in a batch of one, and mixed into one batch with all the
//! others — because positional alignment across a batch that mixes fast-path
//! and fallback entries is exactly what a batched route can get wrong.
//!
//! The matrix asserts it is not decorative: every arm the route can produce —
//! a renewed row, a row returned without renewing it, and no row at all —
//! must appear in it. And it asserts the one accepted divergence both *is*
//! the only one and does fire; see [`DIVERGES`].

use std::sync::Arc;

use orrery_persistd::checkpoint::{CheckpointStore, MemCheckpointStore};
use orrery_persistd::cluster::LeaseRenewal;
use orrery_persistd::journal::{AdaptiveCommitMode, GroupCommitConfig};
use orrery_persistd::{
    payload_crc, CellRuntime, ClaimResult, JournalConfig, LeaseStore, MemLeaseStore, Router,
    RuntimeConfig,
};
use orrery_protocol::{
    CellId, ClaimKind, Epoch, GridId, JournalRecord, Lease, LeaseId, Lsn, NodeId, PersistId,
    RecordKind, Tick, ENTITY_REKEY_VERSION,
};

const CLAIM_MS: u64 = 0;
/// Inside every claimed lease's TTL.
const LIVE_MS: u64 = 100;
/// Past every claimed lease's TTL (`LEASE_TTL_MS` is 10 s).
const STALE_MS: u64 = 50_000;

fn test_node(n: u8) -> NodeId {
    let mut seed = [0u8; 32];
    seed[0] = n;
    iroh_base::SecretKey::from_bytes(&seed).public()
}

/// Where the entity's registrar row actually is, relative to the cell the
/// holder presents in its heartbeat.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Home {
    /// Claimed at the presented cell. The overwhelmingly common case.
    Presented,
    /// Claimed at a different leaf cell of the *same* shard. Routing is a
    /// shard-level decision, and this is what proves it.
    SiblingCell,
    /// A committed rekey moved the row to another shard; the holder still
    /// presents the cell it claimed at.
    Rekeyed,
    /// The presented cell belongs to no shard this runtime hosts, while the
    /// row does.
    Unhosted,
    /// Never claimed.
    Missing,
    /// Invariant J violated: an actor holds the row while the durable location
    /// names a cell in another shard. Reachable when a rekey's migration
    /// commits and a later step of the same rekey fails, so the source keeps
    /// both the reservation and the row.
    LocationDiverged,
}

const HOMES: [Home; 6] = [
    Home::Presented,
    Home::SiblingCell,
    Home::Rekeyed,
    Home::Unhosted,
    Home::Missing,
    Home::LocationDiverged,
];

/// What the holder puts in the renewal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Token {
    Current,
    WrongHolder,
    WrongLeaseId,
}

const TOKENS: [Token; 3] = [Token::Current, Token::WrongHolder, Token::WrongLeaseId];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Scenario {
    home: Home,
    token: Token,
    /// The batch's clock. `STALE_MS` puts every claimed row past its expiry,
    /// which is a property of the whole batch rather than of one entry.
    now_ms: u64,
}

fn scenarios() -> Vec<Scenario> {
    let mut out = Vec::new();
    for home in HOMES {
        for token in TOKENS {
            out.push(Scenario {
                home,
                token,
                now_ms: LIVE_MS,
            });
        }
        out.push(Scenario {
            home,
            token: Token::Current,
            now_ms: STALE_MS,
        });
    }
    out
}

struct Fixture {
    runtime: Arc<CellRuntime>,
    store: Arc<MemLeaseStore>,
    holder: NodeId,
    /// One renewal entry per scenario, in `scenarios()` order.
    batch: Vec<LeaseRenewal>,
}

fn runtime_config(dir: &std::path::Path, shards: Vec<CellId>) -> RuntimeConfig {
    RuntimeConfig {
        shards,
        grid: GridId::ROOT,
        journal: JournalConfig {
            dir: dir.to_path_buf(),
            commit: GroupCommitConfig {
                mode: AdaptiveCommitMode::Adaptive,
                ..GroupCommitConfig::default()
            },
        },
        node_id: 1,
        epoch: Epoch::new(1),
        ..RuntimeConfig::default()
    }
}

fn seed_record(cell: CellId, entity: PersistId) -> JournalRecord {
    let image = b"pre-rekey".as_slice();
    JournalRecord {
        lsn: Lsn::new(0, 0),
        cell,
        grid: GridId::ROOT,
        entity,
        tick: Tick::new(1),
        epoch: Epoch::new(1),
        author: test_node(1),
        kind: RecordKind::Spawn,
        crc: payload_crc(image),
        payload: bytes::Bytes::copy_from_slice(image),
    }
}

/// Build the whole matrix's state in one runtime: every scenario gets its own
/// entity, so they cannot interact, and the mixed batch is then just all of
/// them in one call.
async fn fixture(dir: &std::path::Path) -> Fixture {
    let shards = CellId::ROOT.children();
    let (home_shard, away_shard, unhosted) = (shards[0], shards[1], shards[2]);
    let store = Arc::new(MemLeaseStore::new());
    let checkpoints: Arc<dyn CheckpointStore> = Arc::new(MemCheckpointStore::new());
    let runtime = Arc::new(
        CellRuntime::open_with_lease_store(
            &runtime_config(dir, vec![home_shard, away_shard]),
            &checkpoints,
            Arc::clone(&store) as Arc<dyn LeaseStore>,
        )
        .await
        .expect("runtime opens"),
    );
    let holder = test_node(11);
    let usurper = test_node(12);

    let mut batch = Vec::new();
    for (index, scenario) in scenarios().into_iter().enumerate() {
        let entity = PersistId::new(70_000 + index as u64);
        // A distinct leaf per entity in each shard, so no two scenarios share
        // a cell and `LeaseStore::put`'s location check never conflicts.
        let home_cell = home_shard.children()[index % 8].children()[index / 8 % 8];
        let sibling_cell = home_shard.children()[(index + 1) % 8].children()[index / 8 % 8];
        let away_cell = away_shard.children()[index % 8].children()[index / 8 % 8];
        let unhosted_cell = unhosted.children()[index % 8].children()[index / 8 % 8];

        let (presented, granted) = match scenario.home {
            Home::Missing => (home_cell, None),
            Home::Rekeyed => {
                runtime
                    .apply(seed_record(home_cell, entity))
                    .await
                    .expect("seed the source image");
                let grant = claim(&runtime, home_cell, entity, holder, ClaimKind::Strong).await;
                rekey(&runtime, entity, home_cell, away_cell, grant.lease_id).await;
                (home_cell, Some(grant))
            }
            Home::LocationDiverged => {
                let grant = claim(&runtime, home_cell, entity, holder, ClaimKind::Weak).await;
                // Move only the durable location. The actor keeps its row,
                // which is exactly the state a rekey leaves behind when its
                // migration commits and a later step fails.
                store
                    .migrate(GridId::ROOT, entity, home_cell, away_cell, grant.lease_id)
                    .await
                    .expect("the store migrates");
                (home_cell, Some(grant))
            }
            Home::Presented => {
                let grant = claim(&runtime, home_cell, entity, holder, ClaimKind::Weak).await;
                (home_cell, Some(grant))
            }
            Home::SiblingCell => {
                let grant = claim(&runtime, home_cell, entity, holder, ClaimKind::Weak).await;
                (sibling_cell, Some(grant))
            }
            Home::Unhosted => {
                let grant = claim(&runtime, home_cell, entity, holder, ClaimKind::Weak).await;
                (unhosted_cell, Some(grant))
            }
        };
        let lease_id = granted.map_or(LeaseId(1), |row| row.lease_id);
        batch.push(LeaseRenewal {
            cell: presented,
            entity,
            lease_id: match scenario.token {
                Token::Current | Token::WrongHolder => lease_id,
                Token::WrongLeaseId => LeaseId(lease_id.0.wrapping_add(1)),
            },
        });
        let _ = usurper;
    }

    Fixture {
        runtime,
        store,
        holder,
        batch,
    }
}

async fn claim(
    runtime: &Arc<CellRuntime>,
    cell: CellId,
    entity: PersistId,
    holder: NodeId,
    kind: ClaimKind,
) -> Lease {
    let ClaimResult::Granted(row) = Router::claim_lease(
        runtime.as_ref(),
        GridId::ROOT,
        cell,
        entity,
        holder,
        kind,
        CLAIM_MS,
    )
    .await
    .expect("claim routes") else {
        panic!("claim of {entity:?} should be granted");
    };
    row
}

async fn rekey(
    runtime: &Arc<CellRuntime>,
    entity: PersistId,
    source: CellId,
    destination: CellId,
    expected_lease_id: LeaseId,
) {
    let rekey = orrery_protocol::EntityRekey {
        source_schema_floor: 0,
        version: ENTITY_REKEY_VERSION,
        entity,
        source_grid: GridId::ROOT,
        source_cell: source,
        destination_grid: GridId::ROOT,
        destination_cell: destination,
        expected_lease_id,
        source_record: bytes::Bytes::from_static(b"pre-rekey"),
    };
    let payload = bytes::Bytes::from(postcard::to_allocvec(&rekey).expect("encode"));
    Router::commit_rekey(
        runtime.as_ref(),
        JournalRecord {
            lsn: Lsn::new(0, 0),
            cell: source,
            grid: GridId::ROOT,
            entity,
            tick: Tick::new(7),
            epoch: Epoch::new(1),
            author: test_node(1),
            kind: RecordKind::Rekey,
            crc: payload_crc(&payload),
            payload,
        },
    )
    .await
    .expect("the rekey commits");
}

/// The holder each entry presents. `WrongHolder` entries are renewed by
/// somebody else, which the registrar must refuse while still returning the
/// live row.
fn holder_for(scenario: Scenario, holder: NodeId) -> NodeId {
    match scenario.token {
        Token::WrongHolder => test_node(12),
        _ => holder,
    }
}

/// Which arm of the answer this is, for the coverage assertion below.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
enum Arm {
    Renewed,
    RowWithoutRenewal,
    NoRow,
}

/// Renewal is visible in exactly one place: `LeaseRegistrar::heartbeat` sets
/// `expires_at` to `now_ms + LEASE_TTL_MS` and otherwise leaves the row alone.
/// Comparing against that is what separates "renewed" from "refused, and here
/// is the live row" — a refused renewal still returns a row whose holder is
/// the caller and whose expiry is in the future, so neither of those
/// distinguishes the two.
fn arm(row: Option<&Lease>, now_ms: u64) -> Arm {
    match row {
        None => Arm::NoRow,
        Some(row) if row.expires_at == now_ms + orrery_persistd::LEASE_TTL_MS => Arm::Renewed,
        Some(_) => Arm::RowWithoutRenewal,
    }
}

/// The one state where the two routes answer differently, decided rather than
/// discovered.
///
/// Invariant J is what makes asking the presented cell's owner the same choice
/// as asking the locate's, so the routes can only differ where J is violated:
/// an actor holding a row for an entity whose durable location sits in another
/// shard. That is reachable — a rekey whose `LeaseStore::migrate` committed
/// and whose later steps then failed leaves the source holding both the
/// reservation and the row — and it is the same state §2.1.2 accepted a
/// divergence in for the fenced bulk path.
///
/// Routing by `locate` answers `None`: the destination has no row yet, so the
/// holder is told its lease is invalid and re-claims. Asking the owner answers
/// the source's live row, and renews it. The new payload is the more useful
/// one, and it is not a hole in single-writer safety: a write is admitted by
/// `LeaseRegistrar::admits_write` against the actor's own row, never by a
/// renewal acknowledgement, and `install_rekey` restores the prepare-time
/// snapshot, so the renewed expiry does not outlive the rekey under either
/// route. What differs is what the holder is told, not who may write.
///
/// It is also not silent: this is exactly what the sampled J audit's
/// `location_mismatches` counts, and the renewal accept path now feeds that
/// same sample.
const DIVERGES: Home = Home::LocationDiverged;

/// What routing by `locate` answers for each state. Pinned so the matrix
/// asserts the states it *built* are the states it meant: a scenario that
/// silently stopped constructing its condition would otherwise still compare
/// equal to an oracle looking at the same nothing.
fn expected_oracle_arm(scenario: Scenario) -> Arm {
    match scenario.home {
        // Nothing to renew, and nothing that could be renewed.
        Home::Missing => Arm::NoRow,
        // The durable location names a shard whose actor holds no row, so
        // routing by it finds nothing — even though the source actor still
        // has the row.
        Home::LocationDiverged => Arm::NoRow,
        _ if scenario.token == Token::Current && scenario.now_ms == LIVE_MS => Arm::Renewed,
        // A live row, returned so the holder can see who has it, but not
        // renewed: wrong holder, wrong token, or past its expiry.
        _ => Arm::RowWithoutRenewal,
    }
}

#[tokio::test]
async fn the_renewal_route_answers_what_the_locate_oracle_answers() {
    let cases = scenarios();

    // Alone: one runtime per scenario, so a batch of one is genuinely a batch
    // of one and nothing else has touched the state.
    let mut arms: Vec<Arm> = Vec::new();
    let mut diverged: Vec<(Scenario, Option<Lease>, Option<Lease>)> = Vec::new();
    for (index, scenario) in cases.iter().copied().enumerate() {
        let route_dir = tempfile::tempdir().unwrap();
        let oracle_dir = tempfile::tempdir().unwrap();
        let routed = fixture(route_dir.path()).await;
        let oracle = fixture(oracle_dir.path()).await;
        let who = holder_for(scenario, routed.holder);
        let entry = [routed.batch[index]];

        let mine = Router::heartbeat_leases(
            routed.runtime.as_ref(),
            GridId::ROOT,
            who,
            &entry,
            scenario.now_ms,
        )
        .await
        .expect("the route answers");
        let theirs = oracle
            .runtime
            .heartbeat_leases_via_locate(GridId::ROOT, who, &[oracle.batch[index]], scenario.now_ms)
            .await
            .expect("the oracle answers");

        assert_eq!(mine.len(), 1, "one entry in, one answer out: {scenario:?}");
        assert_eq!(theirs.len(), 1);
        assert_eq!(
            arm(theirs[0].as_ref(), scenario.now_ms),
            expected_oracle_arm(scenario),
            "the oracle's answer for {scenario:?} is not the state this scenario meant to build",
        );
        if mine[0] != theirs[0] {
            diverged.push((scenario, mine[0].clone(), theirs[0].clone()));
        }
        arms.push(arm(mine[0].as_ref(), scenario.now_ms));
        if std::env::var("MATRIX_DUMP").is_ok() {
            println!(
                "{:?} {:?} now={} -> {:?}",
                scenario.home,
                scenario.token,
                scenario.now_ms,
                arm(mine[0].as_ref(), scenario.now_ms)
            );
        }

        close(routed).await;
        close(oracle).await;
    }

    // Mixed: every scenario of one clock in a single batch, compared
    // positionally. A batch that mixes fast-path and fallback entries is
    // where a positional reply goes wrong.
    for now_ms in [LIVE_MS, STALE_MS] {
        let route_dir = tempfile::tempdir().unwrap();
        let oracle_dir = tempfile::tempdir().unwrap();
        let routed = fixture(route_dir.path()).await;
        let oracle = fixture(oracle_dir.path()).await;
        // One holder for the whole batch, so the mixed pass covers the
        // `WrongHolder` entries as "somebody else's token in my batch".
        let who = routed.holder;
        let picked: Vec<usize> = cases
            .iter()
            .enumerate()
            .filter(|(_, scenario)| scenario.now_ms == now_ms)
            .map(|(index, _)| index)
            .collect();
        let mine_batch: Vec<LeaseRenewal> =
            picked.iter().map(|index| routed.batch[*index]).collect();
        let oracle_batch: Vec<LeaseRenewal> =
            picked.iter().map(|index| oracle.batch[*index]).collect();

        let mine = Router::heartbeat_leases(
            routed.runtime.as_ref(),
            GridId::ROOT,
            who,
            &mine_batch,
            now_ms,
        )
        .await
        .expect("the route answers");
        let theirs = oracle
            .runtime
            .heartbeat_leases_via_locate(GridId::ROOT, who, &oracle_batch, now_ms)
            .await
            .expect("the oracle answers");
        assert_eq!(mine.len(), picked.len(), "the reply stays positional");
        assert_eq!(theirs.len(), picked.len());
        for (slot, index) in picked.iter().copied().enumerate() {
            assert_eq!(
                mine[slot].as_ref().map(|row| row.entity),
                Some(mine_batch[slot].entity).filter(|_| mine[slot].is_some()),
                "the reply stays positional at {slot} ({:?})",
                cases[index],
            );
            if mine[slot] != theirs[slot] {
                diverged.push((cases[index], mine[slot].clone(), theirs[slot].clone()));
            }
            arms.push(arm(mine[slot].as_ref(), now_ms));
        }
        close(routed).await;
        close(oracle).await;
    }

    arms.sort_unstable();
    arms.dedup();
    let mut every = vec![Arm::Renewed, Arm::RowWithoutRenewal, Arm::NoRow];
    every.sort_unstable();
    assert_eq!(
        arms, every,
        "the matrix must exercise every arm the route can produce",
    );

    let unexpected: Vec<_> = diverged
        .iter()
        .filter(|(scenario, _, _)| scenario.home != DIVERGES)
        .collect();
    assert!(
        unexpected.is_empty(),
        "the renewal route diverged from the locate oracle: {unexpected:#?}",
    );
    // And the accepted divergence is not a dead letter: if it stops firing,
    // either the state stopped being reachable or the route stopped taking
    // the fast path, and both need saying out loud rather than passing.
    assert!(
        diverged
            .iter()
            .any(|(scenario, _, _)| scenario.home == DIVERGES),
        "the accepted divergence must actually occur",
    );
}

async fn close(fixture: Fixture) {
    let Fixture { runtime, store, .. } = fixture;
    drop(store);
    Arc::try_unwrap(runtime)
        .ok()
        .expect("sole owner")
        .close()
        .await
        .expect("close");
}
