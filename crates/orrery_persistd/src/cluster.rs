//! The multi-node persistence cluster (docs/08-persistence.md §3.2, P2 gaps #2/#7).
//!
//! A [`Cluster`] owns the node set and routes each shard cell to the node that
//! rendezvous (HRW) placement assigns it to — replacing the single
//! `Arc<Mutex<CellRuntime>>` the gateway used for a one-node deployment. The
//! gateway routes diffs and area loads by placement, so a multi-node `persistd`
//! serves shards across nodes instead of one process holding everything.
//!
//! Each node is a [`CellRuntime`] with its own journal and actors. The cluster
//! also wires chain replication between nodes: each node's journal streams to
//! its follower (the next node in HRW order), so a node loss is covered by the
//! follower's copy (RPO ≤ ~100 ms, D11 §4).

use std::collections::HashMap;
use std::sync::Arc;

use orrery_protocol::CellId;

use orrery_protocol::GridId;

use orrery_protocol::JournalRecord;

use crate::actor::{Reject, SnapshotPage};
use crate::journal::{ChainConfig, ChainReplicator, ChainTransport, JournalChainSink};
use crate::placement::{RendezvousHasher, RendezvousNode};
use crate::runtime::CellRuntime;

/// The routing surface the gateway uses to reach cell actors.
///
/// A single-node deployment routes everything to its one runtime; a multi-node
/// [`Cluster`] routes each cell to the node rendezvous placement assigns it to
/// (docs/08-persistence.md §3.2). The gateway depends only on this trait, so
/// the routing topology is swappable.
#[async_trait::async_trait]
pub trait Router: Send + Sync {
    /// Apply a journal record to the actor owning its cell, returning the
    /// durable LSN.
    async fn apply(&self, record: JournalRecord) -> Result<orrery_protocol::Lsn, Reject>;

    /// Read a snapshot of `cell` in `grid` from its owning actor (P-7:
    /// storage cell ids are grid-relative, so the grid scopes which universe
    /// the cell id names).
    async fn read(&self, grid: GridId, cell: CellId) -> Result<SnapshotPage, Reject>;

    /// Whether a live actor holds `cell` in `grid` (vs a cold FDB scan).
    async fn has_actor(&self, grid: GridId, cell: CellId) -> bool;

    /// Read a cold cell from the durable tier (an FDB range scan), if this
    /// router has a cold-store fallback. Returns `None` when there is no cold
    /// store or the cell has no durable rows.
    ///
    /// Area load serves **live cells** from actor memory (authoritative, ≥
    /// checkpoint freshness) and **cold cells** from this scan
    /// (docs/08-persistence.md §9). `grid` scopes the scan: storage cell ids
    /// are grid-relative (P-7, D11 §6).
    async fn read_cold(&self, grid: GridId, cell: CellId) -> Result<Option<SnapshotPage>, Reject> {
        let _ = (grid, cell);
        Ok(None)
    }
}

/// A router over a single runtime (one-node deployment).
///
/// The guard is never held across an actor await: the handle is resolved
/// under the lock, the lock is dropped, and the actor mailbox is awaited
/// outside it — so concurrent applies pipeline into the journal's commit
/// queue instead of serializing the whole node behind one fsync (§4).
#[async_trait::async_trait]
impl Router for tokio::sync::Mutex<CellRuntime> {
    async fn apply(&self, record: JournalRecord) -> Result<orrery_protocol::Lsn, Reject> {
        let handle = {
            let rt = self.lock().await;
            rt.actor(record.grid, record.cell).cloned()
        };
        handle
            .ok_or(Reject::JournalClosed)?
            .apply_diff(record)
            .await
    }

    async fn read(&self, grid: GridId, cell: CellId) -> Result<SnapshotPage, Reject> {
        let handle = {
            let rt = self.lock().await;
            rt.actor(grid, cell).cloned()
        };
        handle
            .ok_or(Reject::JournalClosed)?
            .read_snapshot(vec![cell])
            .await
    }

    async fn has_actor(&self, grid: GridId, cell: CellId) -> bool {
        let rt = self.lock().await;
        rt.actor(grid, cell).is_some()
    }
}

/// A router that serves cold cells from a durable [`ColdCellReader`], falling
/// back to a live [`Router`] for hot cells.
///
/// Composes the live routing topology with the FDB-backed cold-store fallback
/// (docs/08-persistence.md §9): live cells come from actor memory, cold cells
/// from the durable range scan.
pub struct ColdFallbackRouter<R> {
    /// The live router (single runtime or cluster).
    live: R,
    /// The durable cold-cell reader.
    cold: Arc<dyn crate::checkpoint::ColdCellReader>,
}

impl<R> ColdFallbackRouter<R> {
    /// A router serving `live` with `cold` as the cold-cell fallback.
    #[must_use]
    pub fn new(live: R, cold: Arc<dyn crate::checkpoint::ColdCellReader>) -> Self {
        Self { live, cold }
    }
}

#[async_trait::async_trait]
impl<R: Router + Send + Sync> Router for ColdFallbackRouter<R> {
    async fn apply(&self, record: JournalRecord) -> Result<orrery_protocol::Lsn, Reject> {
        self.live.apply(record).await
    }

    async fn read(&self, grid: GridId, cell: CellId) -> Result<SnapshotPage, Reject> {
        self.live.read(grid, cell).await
    }

    async fn has_actor(&self, grid: GridId, cell: CellId) -> bool {
        self.live.has_actor(grid, cell).await
    }

    async fn read_cold(&self, grid: GridId, cell: CellId) -> Result<Option<SnapshotPage>, Reject> {
        self.cold
            .read_cold(grid, cell)
            .await
            .map_err(|_| Reject::JournalClosed)
    }
}

/// A running multi-node cluster.
pub struct Cluster {
    /// The node set for rendezvous placement.
    nodes: Vec<RendezvousNode>,
    /// Each node's runtime, keyed by its `u64` node id.
    runtimes: HashMap<u64, Arc<tokio::sync::Mutex<CellRuntime>>>,
    /// Chain-replication tasks (primary → follower), one per node.
    chains: Vec<ChainReplicator>,
}

impl Cluster {
    /// Build a cluster from one runtime per node id.
    ///
    /// `runtimes` maps each node's `u64` id to its runtime. Chain replication is
    /// wired between consecutive nodes in sorted id order (each node's follower
    /// is the next node; the last wraps to the first), so every node's journal
    /// has a follower. Pass `None` for `chain` to disable replication.
    pub fn new(
        runtimes: HashMap<u64, Arc<tokio::sync::Mutex<CellRuntime>>>,
        chain: Option<&ChainConfig>,
    ) -> Self {
        let mut ids: Vec<u64> = runtimes.keys().copied().collect();
        ids.sort_unstable();
        let nodes: Vec<RendezvousNode> = ids.iter().map(|&id| RendezvousNode::new(id)).collect();

        let mut chains = Vec::new();
        if let Some(chain) = chain {
            if ids.len() > 1 {
                for (i, &id) in ids.iter().enumerate() {
                    let follower_id = ids[(i + 1) % ids.len()];
                    let source = runtimes.get(&id).expect("source present");
                    let follower = runtimes.get(&follower_id).expect("follower present");
                    let source_journal = journal_of(source);
                    let follower_journal = journal_of(follower);
                    let sink = Arc::new(JournalChainSink::new(follower_journal));
                    let transport: Arc<dyn ChainTransport> =
                        Arc::new(crate::journal::MemChainTransport::new(sink));
                    let cfg = ChainConfig {
                        follower: follower_id,
                        ..chain.clone()
                    };
                    let replicator = crate::journal::spawn_chain(source_journal, transport, &cfg);
                    chains.push(replicator);
                }
            }
        }

        Self {
            nodes,
            runtimes,
            chains,
        }
    }

    /// The node id that owns `cell` under rendezvous placement.
    #[must_use]
    pub fn owner(&self, cell: CellId) -> Option<u64> {
        RendezvousHasher::new(self.nodes.clone()).owner(cell)
    }

    /// The runtime owning `(grid, cell)`, if this cluster hosts it.
    ///
    /// Placement is keyed by `(grid, cell)` (P-7: storage cell ids are
    /// grid-relative). Today each cluster serves exactly one grid — a nested
    /// deployment runs one cluster per grid — so a mismatched grid has no
    /// runtime to route to and this returns `None`. The grid check is the
    /// routing half of the P-7 guard; the runtime's own `actor()` guard is
    /// the enforcement half.
    #[must_use]
    pub fn runtime_for(
        &self,
        grid: GridId,
        cell: CellId,
    ) -> Option<&Arc<tokio::sync::Mutex<CellRuntime>>> {
        let owner = self.owner(cell)?;
        let rt = self.runtimes.get(&owner)?;
        if rt.try_lock().ok()?.grid() != grid {
            return None;
        }
        Some(rt)
    }

    /// The node set (for diagnostics).
    #[must_use]
    pub fn nodes(&self) -> &[RendezvousNode] {
        &self.nodes
    }

    /// The number of nodes in the cluster.
    #[must_use]
    pub fn len(&self) -> usize {
        self.runtimes.len()
    }

    /// Whether the cluster has no nodes.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.runtimes.is_empty()
    }

    /// Stop chain replication and close every node's journal.
    pub async fn close(self) {
        for chain in self.chains {
            chain.shutdown().await;
        }
        for (_, rt) in self.runtimes {
            let rt = Arc::try_unwrap(rt)
                .unwrap_or_else(|_| panic!("cluster runtime still referenced"))
                .into_inner();
            let _ = rt.close().await;
        }
    }
}

/// Clone the journal `Arc` out of a runtime. Safe at cluster-build time: no
/// other task holds the runtime's lock yet.
fn journal_of(rt: &Arc<tokio::sync::Mutex<CellRuntime>>) -> Arc<crate::journal::Journal> {
    let guard = rt.try_lock().expect("cluster build holds no runtime lock");
    Arc::clone(guard.journal())
}

/// A router over a multi-node cluster: each cell routes to the node rendezvous
/// placement assigns it to (docs/08-persistence.md §3.2).
#[async_trait::async_trait]
impl Router for Cluster {
    async fn apply(&self, record: JournalRecord) -> Result<orrery_protocol::Lsn, Reject> {
        let rt = self
            .runtime_for(record.grid, record.cell)
            .ok_or(Reject::JournalClosed)?;
        let handle = {
            let rt = rt.lock().await;
            rt.actor(record.grid, record.cell).cloned()
        };
        handle
            .ok_or(Reject::JournalClosed)?
            .apply_diff(record)
            .await
    }

    async fn read(&self, grid: GridId, cell: CellId) -> Result<SnapshotPage, Reject> {
        let rt = self.runtime_for(grid, cell).ok_or(Reject::JournalClosed)?;
        let handle = {
            let rt = rt.lock().await;
            rt.actor(grid, cell).cloned()
        };
        handle
            .ok_or(Reject::JournalClosed)?
            .read_snapshot(vec![cell])
            .await
    }

    async fn has_actor(&self, grid: GridId, cell: CellId) -> bool {
        let Some(rt) = self.runtime_for(grid, cell) else {
            return false;
        };
        let rt = rt.lock().await;
        rt.actor(grid, cell).is_some()
    }
}
