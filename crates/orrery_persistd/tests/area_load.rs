//! Area load end to end (docs/11-roadmap.md §P2, docs/08-persistence.md §9).
//!
//! Proves the area-load contract against the real gateway wire:
//!
//! - **First page first**: pages stream as their cells resolve — the centre
//!   cell's page is observable before the last requested cell's read
//!   completes (`first_page_precedes_last_cell_read`).
//! - **Every cell answered**: a 27-cell subscribe yields 27 pages, empty
//!   cells included (`every_requested_cell_gets_a_page`).
//! - **Bounded frames**: a cell whose entities exceed one datagram arrives
//!   intact, chunked into sequenced `AreaPage` frames under the
//!   [`MAX_AREA_PAGE_FRAME_BYTES`] budget (`oversized_cell_arrives_intact`),
//!   and nothing is silently dropped on send (`oversize_send_is_counted_not_silent`).
//! - **No head-of-line blocking**: a subscribe whose reads block does not
//!   delay a diff's ack on the same connection (`subscribe_does_not_block_diffs`).
//! - **Client wiring**: the AOI drives one subscribe per crossing and evicts
//!   departed pages (`aoi_change_drives_one_subscribe_and_evicts_departed_pages`).
//! - **Full neighborhood**: one subscribe over a 27-cell set of live, empty,
//!   and cold cells yields 27 pages in face-then-edge-then-corner order, the
//!   centre first, the cold cell served from the durable tier
//!   (`area_load_end_to_end`, FDB-gated).

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use iroh::RelayMode;
use orrery_persistd::actor::{EntityRecord, Reject, SnapshotPage};
use orrery_persistd::checkpoint::{CheckpointStore, MemCheckpointStore};
use orrery_persistd::journal::{AdaptiveCommitMode, GroupCommitConfig};
use orrery_persistd::{
    payload_crc, CellRuntime, GatewayConfig, GatewayServer, JournalConfig, MemFenceStore, Router,
    RuntimeConfig, GATEWAY_ALPN,
};
#[cfg(feature = "fdb")]
use orrery_persistd::{FenceOutcome, FenceRow, FenceStatus, FenceStore};
use orrery_protocol::channels::{decode_stream_frame, encode_datagram, encode_stream_frame};
use orrery_protocol::{
    CellId, DiffUplink, GatewayMsg, GatewayReply, GridId, JournalRecord, Lsn, PersistId,
    RecordKind, Tick, MAX_AREA_PAGE_FRAME_BYTES,
};
use tokio::sync::{Barrier, Mutex};

fn node(n: u8) -> orrery_protocol::NodeId {
    let mut seed = [0u8; 32];
    seed[0] = n;
    iroh_base::SecretKey::from_bytes(&seed).public()
}

#[allow(clippy::needless_pass_by_value)]
fn cell(x: i32, y: i32, z: i32) -> CellId {
    CellId::from_coords(glam::IVec3::new(x, y, z), CellId::MAX_LEVEL).unwrap()
}

fn manhattan(cell: CellId) -> i32 {
    let (c, _) = cell.coords();
    c.abs().element_sum()
}

/// Order the neighborhood nearest-first (centre, then face/edge/corner
/// tiers), mirroring the client's `order_nearest_first` — the ordering the
/// client requests and the gateway streams (D16).
fn order_nearest_first(center: CellId, cells: Vec<CellId>) -> Vec<CellId> {
    let (c, _) = center.coords();
    let mut ranked: Vec<(CellId, i32)> = cells
        .into_iter()
        .map(|cell| {
            let (coords, _) = cell.coords();
            let dist = (coords - c).abs().element_sum();
            (cell, dist)
        })
        .collect();
    ranked.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| a.0.cmp(&b.0)));
    ranked.into_iter().map(|(cell, _)| cell).collect()
}

fn runtime_config(dir: &std::path::Path) -> RuntimeConfig {
    RuntimeConfig {
        shards: vec![CellId::ROOT],
        grid: GridId::ROOT,
        journal: JournalConfig {
            dir: dir.to_path_buf(),
            commit: GroupCommitConfig {
                mode: AdaptiveCommitMode::AlwaysBatch,
                batch_window: Duration::from_millis(100),
                batch_max_records: 100_000,
                batch_max_bytes: 1 << 20,
            },
        },
        node_id: 0,
        epoch: orrery_protocol::Epoch::new(0),
        fence: Arc::new(MemFenceStore::new()),
    }
}

/// A dialed client: the endpoint (kept alive — dropping it closes the
/// connection) and the admitted connection.
struct Client {
    _endpoint: iroh::Endpoint,
    conn: iroh::endpoint::Connection,
}

/// Dial `server` and complete the admission handshake, returning the raw iroh
/// connection (the same wire surface the aeronet client speaks).
async fn dial(server: &GatewayServer) -> Client {
    let endpoint = iroh::endpoint::Builder::new(iroh::endpoint::presets::N0)
        .alpns(vec![GATEWAY_ALPN.to_vec()])
        .relay_mode(RelayMode::Disabled)
        .bind()
        .await
        .unwrap();
    let conn = endpoint.connect(server.addr(), GATEWAY_ALPN).await.unwrap();
    // Admission: the gateway streams [ACCEPTED] (byte 0) on a uni stream.
    let mut admission = conn.accept_uni().await.unwrap();
    let msg = admission.read_to_end(16).await.unwrap();
    assert_eq!(msg, vec![0u8]);
    Client {
        _endpoint: endpoint,
        conn,
    }
}

/// Send a [`GatewayMsg`] as a stream frame.
fn send_stream(conn: &iroh::endpoint::Connection, msg: &GatewayMsg) {
    conn.send_datagram(Bytes::from(encode_stream_frame(msg)))
        .unwrap();
}

/// Read the next decodable stream-frame reply within `secs` seconds.
async fn recv_reply(conn: &iroh::endpoint::Connection, secs: u64) -> GatewayReply {
    let pkt = tokio::time::timeout(Duration::from_secs(secs), conn.read_datagram())
        .await
        .expect("reply within timeout")
        .expect("datagram readable");
    decode_stream_frame(&pkt).expect("stream-frame reply")
}

/// A scripted router: each cell's read resolves on demand, with every read
/// start recorded. `gated` cells block on `barrier` before resolving, on
/// whichever path serves them (live `read` when `all_live`, else cold).
struct ScriptedRouter {
    pages: HashMap<CellId, SnapshotPage>,
    /// Cells whose read blocks on `barrier` before resolving.
    gated: Vec<CellId>,
    barrier: Arc<Barrier>,
    /// The order in which cell reads *started*.
    started: Mutex<Vec<CellId>>,
    /// Serve every cell from the live path (`read`), not just the seeded ones
    /// — the first-page test needs its gated last cell on the live path.
    all_live: bool,
}

#[async_trait::async_trait]
impl Router for ScriptedRouter {
    async fn apply(&self, _record: JournalRecord) -> Result<Lsn, Reject> {
        Ok(Lsn::new(1, 0))
    }

    async fn read(&self, _grid: GridId, cell: CellId) -> Result<SnapshotPage, Reject> {
        self.started.lock().await.push(cell);
        if self.gated.contains(&cell) {
            self.barrier.wait().await;
        }
        Ok(self.pages.get(&cell).cloned().unwrap_or_default())
    }

    async fn read_cold(&self, _grid: GridId, cell: CellId) -> Result<Option<SnapshotPage>, Reject> {
        self.started.lock().await.push(cell);
        if self.gated.contains(&cell) {
            self.barrier.wait().await;
        }
        Ok(self.pages.get(&cell).cloned())
    }

    async fn has_actor(&self, _grid: GridId, cell: CellId) -> bool {
        self.pages.contains_key(&cell) || self.all_live
    }
}

/// A router whose `apply` never resolves and whose reads complete instantly.
struct BlockingApplyRouter {
    applied: AtomicUsize,
}

#[async_trait::async_trait]
impl Router for BlockingApplyRouter {
    async fn apply(&self, _record: JournalRecord) -> Result<Lsn, Reject> {
        self.applied.fetch_add(1, Ordering::SeqCst);
        std::future::pending::<()>().await;
        unreachable!("pending never resolves")
    }

    async fn read(&self, _grid: GridId, _cell: CellId) -> Result<SnapshotPage, Reject> {
        Ok(SnapshotPage::default())
    }

    async fn has_actor(&self, _grid: GridId, _cell: CellId) -> bool {
        true
    }
}

/// A router that fails every cold read.
struct FailingColdRouter;

#[async_trait::async_trait]
impl Router for FailingColdRouter {
    async fn apply(&self, _record: JournalRecord) -> Result<Lsn, Reject> {
        Ok(Lsn::new(1, 0))
    }

    async fn read(&self, _grid: GridId, _cell: CellId) -> Result<SnapshotPage, Reject> {
        Ok(SnapshotPage::default())
    }

    async fn has_actor(&self, _grid: GridId, _cell: CellId) -> bool {
        false
    }

    async fn read_cold(
        &self,
        _grid: GridId,
        _cell: CellId,
    ) -> Result<Option<SnapshotPage>, Reject> {
        Err(Reject::JournalClosed)
    }
}

#[tokio::test]
async fn first_page_precedes_last_cell_read() {
    // The last requested cell's read blocks on a barrier shared with the test:
    // the gateway's route task waits on one party, the test holds the other
    // until the first page has been observed.
    let barrier = Arc::new(Barrier::new(2));
    let centre = cell(0, 0, 0);
    let neighbourhood = order_nearest_first(centre, centre.neighbors27());
    let last = *neighbourhood.last().unwrap();

    let mut pages = HashMap::new();
    let mut entities = HashMap::new();
    entities.insert(
        PersistId::new(1),
        EntityRecord {
            components: Bytes::from_static(b"centre"),
            dirty: false,
        },
    );
    pages.insert(centre, SnapshotPage { entities });

    let router = Arc::new(ScriptedRouter {
        pages,
        gated: vec![last],
        barrier: Arc::clone(&barrier),
        started: Mutex::new(Vec::new()),
        all_live: true,
    });
    let server = GatewayServer::spawn(GatewayConfig::default(), router.clone())
        .await
        .unwrap();
    let client = dial(&server).await;
    let conn = &client.conn;

    send_stream(
        conn,
        &GatewayMsg::Subscribe {
            grid: GridId::ROOT,
            cells: neighbourhood.clone(),
        },
    );

    // The first page must be observable *before* the gated read is released —
    // a buffered trailing flush could never produce it here.
    let reply = recv_reply(conn, 10).await;
    let GatewayReply::AreaPage {
        cell: page_cell,
        page,
    } = reply
    else {
        panic!("expected an AreaPage, got {reply:?}");
    };
    assert_eq!(page_cell, centre, "the first page is the centre cell");
    assert_eq!(page.entities, vec![PersistId::new(1)]);

    // Prove the barrier was actually armed: the gated read started and is
    // still blocked (the route task holds one party; two would deadlock).
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if router.started.lock().await.contains(&last) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the gated read started");
    assert!(
        !router.started.lock().await.is_empty(),
        "reads are in flight while the first page is out"
    );

    // Release the gated read; the subscribe completes.
    barrier.wait().await;
    server.shutdown().await;
}

#[tokio::test]
async fn every_requested_cell_gets_a_page() {
    // 27 cells of which 20 are empty: 27 pages must arrive — an empty cell is
    // an empty page, not silence (docs/08-persistence.md §9).
    let centre = cell(0, 0, 0);
    let neighbourhood = centre.neighbors27();
    assert_eq!(neighbourhood.len(), 27);

    let mut pages = HashMap::new();
    for (i, c) in neighbourhood.iter().take(7).enumerate() {
        let mut entities = HashMap::new();
        entities.insert(
            PersistId::new(i as u64 + 1),
            EntityRecord {
                components: Bytes::from_static(b"live"),
                dirty: false,
            },
        );
        pages.insert(*c, SnapshotPage { entities });
    }

    let router = Arc::new(ScriptedRouter {
        pages,
        gated: Vec::new(),
        barrier: Arc::new(Barrier::new(1)),
        started: Mutex::new(Vec::new()),
        all_live: true,
    });
    let server = GatewayServer::spawn(GatewayConfig::default(), router)
        .await
        .unwrap();
    let client = dial(&server).await;
    let conn = &client.conn;

    send_stream(
        conn,
        &GatewayMsg::Subscribe {
            grid: GridId::ROOT,
            cells: neighbourhood.clone(),
        },
    );

    let mut seen = Vec::new();
    for _ in 0..27 {
        match recv_reply(conn, 10).await {
            GatewayReply::AreaPage { cell, page } => {
                assert_eq!(
                    page.total_chunks, 1,
                    "single-frame page for an empty/small cell"
                );
                seen.push(cell);
            }
            other => panic!("expected AreaPage, got {other:?}"),
        }
    }
    let expected: std::collections::HashSet<CellId> = neighbourhood.iter().copied().collect();
    let got: std::collections::HashSet<CellId> = seen.iter().copied().collect();
    assert_eq!(got, expected, "every requested cell got exactly one page");
    server.shutdown().await;
}

#[tokio::test]
async fn failed_scan_is_a_distinct_reply_not_an_empty_page() {
    // A cold scan that errors is an AreaLoadError, diagnosable — never an
    // empty page (docs/08-persistence.md §9).
    let server = GatewayServer::spawn(GatewayConfig::default(), Arc::new(FailingColdRouter))
        .await
        .unwrap();
    let client = dial(&server).await;
    let conn = &client.conn;
    let c = cell(3, 0, 0);
    send_stream(
        conn,
        &GatewayMsg::Subscribe {
            grid: GridId::ROOT,
            cells: vec![c],
        },
    );
    match recv_reply(conn, 10).await {
        GatewayReply::AreaLoadError { cell, kind } => {
            assert_eq!(cell, c);
            assert_eq!(kind, orrery_protocol::AREA_LOAD_ERR_COLD);
        }
        other => panic!("expected AreaLoadError, got {other:?}"),
    }
    server.shutdown().await;
}

/// Seed one cell with 200 entities × 256-byte bags and collect the raw frames
/// the gateway sends for it, plus the server (for the send-failure counter).
async fn collect_chunked_frames() -> (Vec<Bytes>, GatewayServer) {
    let c = cell(0, 0, 0);
    let mut entities = HashMap::new();
    for i in 0..200u64 {
        entities.insert(
            PersistId::new(i),
            EntityRecord {
                components: Bytes::from(vec![0xAB; 256]),
                dirty: false,
            },
        );
    }
    let mut pages = HashMap::new();
    pages.insert(c, SnapshotPage { entities });
    let router = Arc::new(ScriptedRouter {
        pages,
        gated: Vec::new(),
        barrier: Arc::new(Barrier::new(1)),
        started: Mutex::new(Vec::new()),
        all_live: true,
    });
    let server = GatewayServer::spawn(GatewayConfig::default(), router)
        .await
        .unwrap();
    let client = dial(&server).await;
    let conn = &client.conn;
    send_stream(
        conn,
        &GatewayMsg::Subscribe {
            grid: GridId::ROOT,
            cells: vec![c],
        },
    );

    // 200 × 256 B of bags cannot fit one 1100-byte frame: read until the
    // `last` frame of the cell's sequence arrives.
    let mut frames = Vec::new();
    for _ in 0..64 {
        let pkt = tokio::time::timeout(Duration::from_secs(10), conn.read_datagram())
            .await
            .expect("frames arrive")
            .expect("datagram readable");
        let last = matches!(
            decode_stream_frame(&pkt),
            Some(GatewayReply::AreaPage { page, .. })
                if page.chunk_index + 1 == page.total_chunks
        );
        frames.push(pkt);
        if last {
            break;
        }
    }
    (frames, server)
}

#[tokio::test]
async fn oversized_cell_arrives_intact() {
    let (frames, server) = collect_chunked_frames().await;
    // More than one frame (200 × 256 B ≫ 1100 B), every frame under the
    // budget.
    assert!(frames.len() > 1, "the page was chunked");
    for frame in &frames {
        assert!(
            frame.len() <= MAX_AREA_PAGE_FRAME_BYTES,
            "frame of {} B exceeds the {MAX_AREA_PAGE_FRAME_BYTES} B budget",
            frame.len()
        );
    }
    // Server-side truth: the chunks are sequenced and reassemble to exactly
    // the seeded entities — the same reassembly the client's
    // `AreaLoader::record_frame` performs (client-side coverage lives in
    // orrery_persist_client::area tests).
    let mut assembled: Vec<PersistId> = Vec::new();
    let mut seen_last = false;
    for (i, frame) in frames.iter().enumerate() {
        let Some(GatewayReply::AreaPage {
            cell: page_cell,
            page,
        }) = decode_stream_frame(frame)
        else {
            panic!("frame {i} is not an AreaPage");
        };
        assert_eq!(page_cell, cell(0, 0, 0));
        assert_eq!(page.chunk_index as usize, i, "frames are sequenced");
        assert_eq!(
            page.total_chunks as usize,
            frames.len(),
            "every chunk carries the total"
        );
        seen_last |= page.chunk_index + 1 == page.total_chunks;
        assembled.extend(page.entities);
        assert!(
            page.payloads.iter().all(|p| p.len() == 256),
            "every 256-byte bag arrived intact"
        );
    }
    assert!(seen_last, "the sequence terminated with a last frame");
    assembled.sort_by_key(|p| p.0);
    assembled.dedup();
    assert_eq!(
        assembled.len(),
        200,
        "all 200 PersistIds arrived intact, no duplicates"
    );
    server.shutdown().await;
}

#[tokio::test]
async fn oversize_send_is_counted_not_silent() {
    // The chunked-page path must never trip the send-failure counter: chunking
    // keeps every frame sendable, so a failure here would mean a real
    // regression (today `send_datagram` returns Err(TooLarge) and the gateway
    // discards it with `let _ =`).
    let (frames, server) = collect_chunked_frames().await;
    assert!(!frames.is_empty());
    assert_eq!(
        server.area_page_send_failures(),
        0,
        "no send was discarded: every frame fit the datagram budget"
    );
    server.shutdown().await;
}

#[tokio::test]
async fn subscribe_does_not_block_diffs() {
    // A subscribe whose reads block (the apply never resolves) must not delay
    // a diff's ack on the same connection: per-message routing is spawned, so
    // the diff routes while the subscribe is still in flight.
    //
    // Here the *diff* is the blocked route and the subscribe is fast: the ack
    // can never arrive, but all 27 pages must — proving the subscribe was not
    // queued behind the in-flight diff.
    let router = Arc::new(BlockingApplyRouter {
        applied: AtomicUsize::new(0),
    });
    let server = GatewayServer::spawn(GatewayConfig::default(), router.clone())
        .await
        .unwrap();
    let client = dial(&server).await;
    let conn = &client.conn;

    // The diff first: its route task pends forever on `apply`.
    conn.send_datagram(Bytes::from(encode_datagram(&GatewayMsg::Diff {
        diff: DiffUplink {
            cell: CellId::ROOT,
            grid: GridId::ROOT,
            entity: PersistId::new(1),
            tick: Tick::new(1),
            kind: RecordKind::Spawn,
            payload: Bytes::from_static(b"hp=100"),
            seq: 1,
        },
    })))
    .unwrap();
    // Then the subscribe on the same connection.
    let centre = cell(0, 0, 0);
    let neighbourhood = centre.neighbors27();
    send_stream(
        conn,
        &GatewayMsg::Subscribe {
            grid: GridId::ROOT,
            cells: neighbourhood,
        },
    );

    // The subscribe must complete even though the diff's route is still
    // pending: what arrives while a read can never resolve — 27 pages, or
    // nothing? Inline routing would deadlock the connection on the diff and
    // the 10 s read timeout would fire.
    for i in 0..27 {
        match recv_reply(conn, 10).await {
            GatewayReply::AreaPage { .. } => {}
            other => panic!("reply {i}: expected AreaPage, got {other:?}"),
        }
    }
    assert_eq!(
        router.applied.load(Ordering::SeqCst),
        1,
        "the diff route started (and is still pending)"
    );
    server.shutdown().await;
}

/// The full neighborhood over a live runtime: 27 cells (live entities in a
/// few, the rest empty) in one subscribe, centre page first, then face, edge,
/// corner tiers.
#[tokio::test]
async fn area_load_end_to_end_live_cells() {
    let dir = tempfile::tempdir().unwrap();
    let runtime = Arc::new(Mutex::new(
        CellRuntime::open(
            &runtime_config(dir.path()),
            &(Arc::new(MemCheckpointStore::new()) as Arc<dyn CheckpointStore>),
        )
        .unwrap(),
    ));
    // Seed live entities into three cells (all under the ROOT shard).
    for (i, c) in [cell(0, 0, 0), cell(1, 0, 0), cell(1, 1, 1)]
        .iter()
        .enumerate()
    {
        let rec = JournalRecord {
            lsn: Lsn::new(0, 0),
            cell: *c,
            grid: GridId::ROOT,
            entity: PersistId::new(i as u64 + 1),
            tick: Tick::new(1),
            epoch: orrery_protocol::Epoch::new(0),
            author: node(1),
            kind: RecordKind::Spawn,
            payload: Bytes::from(format!("entity-{i}").into_bytes()),
            crc: payload_crc(format!("entity-{i}").as_bytes()),
        };
        runtime.lock().await.apply(rec).await.unwrap();
    }
    let router: Arc<dyn Router> = runtime;
    let server = GatewayServer::spawn(GatewayConfig::default(), router)
        .await
        .unwrap();
    let client = dial(&server).await;
    let conn = &client.conn;

    let centre = cell(0, 0, 0);
    let cells = order_nearest_first(centre, centre.neighbors27());
    send_stream(
        conn,
        &GatewayMsg::Subscribe {
            grid: GridId::ROOT,
            cells: cells.clone(),
        },
    );

    let mut order = Vec::new();
    let mut live_entities = HashMap::new();
    for _ in 0..27 {
        match recv_reply(conn, 10).await {
            GatewayReply::AreaPage { cell, page } => {
                order.push(cell);
                if !page.entities.is_empty() {
                    live_entities.insert(cell, page.entities.clone());
                }
            }
            other => panic!("expected AreaPage, got {other:?}"),
        }
    }
    assert_eq!(order.len(), 27, "all 27 pages arrive");
    assert_eq!(order[0], centre, "the first page is the centre");
    for (i, c) in order.iter().enumerate().skip(1) {
        let expected = match i {
            1..=6 => 1,
            7..=18 => 2,
            _ => 3,
        };
        assert_eq!(
            manhattan(*c),
            expected,
            "face pages precede edge precede corner (index {i})"
        );
    }
    assert_eq!(
        live_entities.len(),
        3,
        "the three seeded cells came back live"
    );
    server.shutdown().await;
}

/// The cluster file for the FDB-gated tests, or `None` if not configured.
///
/// Honors `ORRERY_FDB_CLUSTER_FILE`; otherwise walks up from the crate dir to
/// find the workspace-root `.fdb-dev/fdb.cluster` (tests run with CWD = the
/// crate dir, not the workspace root).
#[cfg(feature = "fdb")]
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

/// A runtime pinned to `grid` (the fdb tests share one dev cluster; the grid
/// scopes every row so tests are disjoint by construction, P-7).
#[cfg(feature = "fdb")]
fn runtime_config_in(dir: &std::path::Path, grid: GridId) -> RuntimeConfig {
    RuntimeConfig {
        grid,
        ..runtime_config(dir)
    }
}

#[cfg(feature = "fdb")]
async fn activate_fdb_checkpoint_fence(cluster: &str, grid: GridId) {
    let store = orrery_persistd::fence::FdbFenceStore::connect(cluster).unwrap();
    let expected = FenceRow {
        owner: 0,
        epoch: orrery_protocol::Epoch::new(0),
        status: FenceStatus::Active,
    };
    match store.read(grid, CellId::ROOT).await.unwrap() {
        Some(row) => assert_eq!(row, expected, "test grid has unexpected fence"),
        None => assert!(matches!(
            store
                .fence(grid, CellId::ROOT, None, &expected)
                .await
                .unwrap(),
            FenceOutcome::Fenced
        )),
    }
}

/// Cold-cell coverage: an entity checkpointed into FDB and then absent from
/// any live actor is served by the durable tier on subscribe
/// (docs/08-persistence.md §9). Grid 9201 is this test's slice of the shared
/// dev cluster.
#[cfg(feature = "fdb")]
#[tokio::test]
async fn area_load_end_to_end_cold_cell_served() {
    let Some(cluster) = fdb_cluster_file() else {
        eprintln!("skipping: ORRERY_FDB_CLUSTER_FILE not set and no .fdb-dev/fdb.cluster");
        return;
    };
    let grid = GridId::new(9201);
    activate_fdb_checkpoint_fence(&cluster, grid).await;
    let store =
        Arc::new(orrery_persistd::checkpoint::FdbCheckpointStore::connect(&cluster).unwrap());

    // Phase 1: write an entity in a cold cell and checkpoint it into FDB.
    let cold_cell = cell(4, 0, 0);
    let dir = tempfile::tempdir().unwrap();
    {
        let rt = CellRuntime::open(
            &runtime_config_in(dir.path(), grid),
            &(store.clone() as Arc<dyn CheckpointStore>),
        )
        .unwrap();
        let rec = JournalRecord {
            lsn: Lsn::new(0, 0),
            cell: cold_cell,
            grid,
            entity: PersistId::new(42),
            tick: Tick::new(1),
            epoch: orrery_protocol::Epoch::new(0),
            author: node(1),
            kind: RecordKind::Spawn,
            payload: Bytes::from_static(b"cold-entity"),
            crc: payload_crc(b"cold-entity"),
        };
        rt.apply(rec).await.unwrap();
        rt.checkpoint(store.as_ref()).await.unwrap();
        rt.close().await.unwrap();
    }

    // Phase 2: no live actors at all — the subscribe is served entirely from
    // the FDB range scans (a `ColdFallbackRouter` over an empty live router,
    // exactly the not-yet-fenced case the cold path exists for).
    struct NoLive;
    #[async_trait::async_trait]
    impl Router for NoLive {
        async fn apply(&self, _record: JournalRecord) -> Result<Lsn, Reject> {
            Err(Reject::JournalClosed)
        }
        async fn read(&self, _grid: GridId, _cell: CellId) -> Result<SnapshotPage, Reject> {
            Err(Reject::JournalClosed)
        }
        async fn has_actor(&self, _grid: GridId, _cell: CellId) -> bool {
            false
        }
    }
    let router: Arc<dyn Router> = Arc::new(orrery_persistd::ColdFallbackRouter::new(NoLive, store));
    let server = GatewayServer::spawn(GatewayConfig::default(), router)
        .await
        .unwrap();
    let client = dial(&server).await;
    let conn = &client.conn;

    let centre = cell(4, 0, 0);
    let cells = order_nearest_first(centre, centre.neighbors27());
    send_stream(
        conn,
        &GatewayMsg::Subscribe {
            grid,
            cells: cells.clone(),
        },
    );

    let mut cold_served = false;
    let mut pages = 0;
    for _ in 0..cells.len() {
        match recv_reply(conn, 15).await {
            GatewayReply::AreaPage { cell, page } => {
                pages += 1;
                if cell == cold_cell {
                    cold_served = page.entities == vec![PersistId::new(42)]
                        && !page.live
                        && page.payloads[0].as_ref() == b"cold-entity";
                }
            }
            other => panic!("expected AreaPage, got {other:?}"),
        }
    }
    assert_eq!(pages, 27, "all 27 pages arrive");
    assert!(
        cold_served,
        "the cold cell was served from the durable tier"
    );
    server.shutdown().await;
}
