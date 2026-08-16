//! Area load/subscribe: the 27-cell neighborhood, streamed nearest-first (D11 §9).
//!
//! When the client enters an area, it requests the 27-cell AOI (D5) over a
//! reliable stream. The gateway partitions the cells: live cells (an actor
//! holds them) are served from actor memory, cold cells by FDB range scans.
//! Pages stream **nearest-first** (center cell, then face/edge/corner neighbors
//! by distance) so the client can spawn-in against page one (D16: < 50 ms to
//! first page-in).

use std::collections::{HashMap, HashSet};

use bevy_ecs::prelude::*;
use bevy_platform::time::Instant;
use orrery_protocol::{AreaPage, CellId, GridId, PersistId};

use crate::config::PersistClientConfig;
use crate::gateway::GatewaySession;

/// A loaded area page (D11 §9).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedPage {
    /// The cell this page covers.
    pub cell: CellId,
    /// The entities in this cell.
    pub entities: Vec<PersistId>,
    /// The component payload for each entity, parallel to `entities`.
    pub payloads: Vec<bytes::Bytes>,
    /// Whether this page came from a live cell actor (vs a cold FDB scan).
    pub live: bool,
}

/// An incomplete multi-chunk page for one cell, held until every chunk of its
/// sequence arrives.
///
/// The reliable lane delivers a page's chunks in order and without loss, so
/// within one connection a partial set is only ever a page still arriving.
/// Across a reconnect it can be a page that will never finish, which is why a
/// partial is **held, never surfaced as a complete [`LoadedPage`]** — a client
/// holding chunks 0 and 2 of a 3-chunk page must not present a partial page.
/// The re-subscribe backstop re-requests the cell set, and the re-sent page (a
/// new `page_seq`) supersedes the stale partial.
#[derive(Debug, Default)]
struct PartialPage {
    /// The page sequence these chunks belong to.
    seq: u32,
    /// The page's chunk count (every chunk carries it).
    total: u32,
    /// Chunks received so far, keyed by `chunk_index`.
    chunks: HashMap<u32, AreaPage>,
}

/// The area loader: tracks the subscribed neighborhood and the pages received.
///
/// A [`Resource`] holding the current AOI subscription and the pages loaded so
/// far. The plugin's system requests the neighborhood when it changes and
/// records pages as they arrive.
#[derive(Debug, Resource)]
pub struct AreaLoader {
    /// The grid the subscription is in (P-7). Nested-grid clients set this to
    /// the frame's `GridId`; the default is the root universe grid.
    pub grid: GridId,
    /// The cells currently subscribed, nearest-first.
    pub cells: Vec<CellId>,
    /// Pages received so far, keyed by cell.
    pub pages: Vec<LoadedPage>,
    /// The time the last subscribe round was issued (the backstop clock, not
    /// the subscribe trigger — see [`drive_area_loader`]).
    pub last_subscribe: Option<Instant>,
    /// The cell set of the last issued subscribe round. A subscribe is issued
    /// when the current set differs from this, or when the backstop fires on
    /// an incomplete round.
    pub last_sent: Vec<CellId>,
    /// The time the first page of the current area arrived (for the < 50 ms
    /// first-page-in target, D16). Cleared when the subscribed cell set
    /// changes, so each area round measures its own request-to-first-page
    /// duration.
    pub first_page_at: Option<Instant>,
    /// Incomplete multi-frame pages (see [`PartialPage`]).
    partials: HashMap<CellId, PartialPage>,
}

impl AreaLoader {
    /// A new, empty loader in the root universe grid.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A new, empty loader for a nested grid (`GridId::ROOT` for the universe).
    #[must_use]
    pub fn in_grid(grid: GridId) -> Self {
        Self {
            grid,
            ..Self::default()
        }
    }

    /// Whether `cell` is in the current subscription.
    #[must_use]
    pub fn contains(&self, cell: CellId) -> bool {
        self.cells.contains(&cell)
    }

    /// The number of pages loaded so far.
    #[must_use]
    pub fn page_count(&self) -> usize {
        self.pages.len()
    }

    /// Record a received page, replacing any prior page for the same cell.
    ///
    /// Sets [`AreaLoader::first_page_at`] when it is `None` — the first page
    /// of a round starts the D16 < 50 ms first-page-in measurement, and later
    /// pages of the same round do not move it. Done here (not in the reply
    /// handler) so every page-arrival path is timed identically.
    pub fn record(&mut self, page: LoadedPage) {
        if self.first_page_at.is_none() {
            self.first_page_at = Some(Instant::now());
        }
        self.pages.retain(|p| p.cell != page.cell);
        self.pages.push(page);
    }

    /// Record one chunk of a (possibly multi-chunk) area page.
    ///
    /// Single-chunk pages (`total_chunks: 1`) record immediately. Multi-chunk
    /// pages accumulate until every chunk of the page's `page_seq` has
    /// arrived; only the complete page is recorded — a partial set is never
    /// presented as complete. Chunks are keyed by
    /// `(page_seq, chunk_index)` and every chunk carries `total_chunks`, so
    /// any arrival order completes the set; a re-sent page (a new `page_seq`)
    /// supersedes the stale partial.
    pub fn record_frame(&mut self, page: AreaPage) {
        let cell = page.cell;
        if page.total_chunks == 1 {
            self.partials.remove(&cell);
            self.record(LoadedPage {
                cell,
                entities: page.entities,
                payloads: page.payloads,
                live: page.live,
            });
            return;
        }
        let partial = self.partials.entry(cell).or_insert_with(|| PartialPage {
            seq: page.page_seq,
            total: page.total_chunks,
            chunks: HashMap::new(),
        });
        if partial.seq != page.page_seq {
            // A re-sent page supersedes the stale partial.
            partial.seq = page.page_seq;
            partial.total = page.total_chunks;
            partial.chunks.clear();
        }
        partial.chunks.insert(page.chunk_index, page);
        #[allow(clippy::cast_possible_truncation)]
        let total = partial.total as usize;
        if partial.chunks.len() < total {
            // Missing chunks: hold the partial (the retry floor re-requests).
            return;
        }
        // Complete: assemble in chunk-index order and record.
        let mut entities = Vec::new();
        let mut payloads = Vec::new();
        let mut live = false;
        for index in 0..partial.total {
            let chunk = partial
                .chunks
                .remove(&index)
                .expect("count matches: chunk present");
            live = chunk.live;
            entities.extend(chunk.entities);
            payloads.extend(chunk.payloads);
        }
        self.partials.remove(&cell);
        self.record(LoadedPage {
            cell,
            entities,
            payloads,
            live,
        });
    }

    /// Whether an unchanged subscription is due to be re-issued: the round is
    /// still missing pages *and* the backstop interval has elapsed.
    ///
    /// Both halves matter, and the first is the one that changed with the
    /// lane. An unconditional periodic re-issue re-asks for cells the gateway
    /// already answered, which is the load amplification PR #15/#16 found; a
    /// re-issue gated on a gap asks only when there is something outstanding
    /// to ask about, so a healthy subscription costs exactly one subscribe.
    #[must_use]
    pub fn round_is_overdue(&self, now: Instant, cells_per_round: usize) -> bool {
        let requested = self.cells.len().min(cells_per_round);
        let answered = self
            .cells
            .iter()
            .take(cells_per_round)
            .filter(|cell| self.pages.iter().any(|page| page.cell == **cell))
            .count();
        if answered >= requested {
            return false;
        }
        self.last_subscribe.is_none_or(|last| {
            now.saturating_duration_since(last)
                >= std::time::Duration::from_millis(RESUBSCRIBE_BACKSTOP_MS)
        })
    }

    /// Begin a new subscription round: replace the subscribed cell set, drop
    /// every page and partial whose cell left the subscription, and clear
    /// [`AreaLoader::first_page_at`] so the new round measures its own
    /// request-to-first-page duration (D16).
    pub fn begin_round(&mut self, cells: Vec<CellId>) {
        let keep: HashSet<CellId> = cells.iter().copied().collect();
        self.pages.retain(|p| keep.contains(&p.cell));
        self.partials.retain(|cell, _| keep.contains(cell));
        self.cells = cells;
        self.first_page_at = None;
    }
}

impl Default for AreaLoader {
    fn default() -> Self {
        Self {
            grid: GridId::ROOT,
            cells: Vec::new(),
            pages: Vec::new(),
            last_subscribe: None,
            last_sent: Vec::new(),
            first_page_at: None,
            partials: HashMap::new(),
        }
    }
}

/// Order the 27-cell neighborhood nearest-first from `center`.
///
/// The center cell first, then face neighbors (Manhattan distance 1), then
/// edge (2), then corner (3). Manhattan distance (not Chebyshev) separates the
/// three tiers: Chebyshev collapses all 26 neighbors into one tier, which
/// would let a corner page land before a face page and delay spawn-in (D16).
/// Ties are broken deterministically by cell id. This is the ordering the
/// client requests and the gateway streams, so the client can spawn-in against
/// page one.
#[must_use]
pub fn order_nearest_first(center: CellId, cells: Vec<CellId>) -> Vec<CellId> {
    let (c, _) = center.coords();
    let mut ranked: Vec<(CellId, i32)> = cells
        .into_iter()
        .map(|cell| {
            let (coords, _) = cell.coords();
            let dist = (coords - c).abs().element_sum();
            (cell, dist)
        })
        .collect();
    // Nearest first; ties by cell id for determinism.
    ranked.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| a.0.cmp(&b.0)));
    ranked.into_iter().map(|(cell, _)| cell).collect()
}

/// The minimum interval between two subscribes of the *same* cell set, in
/// milliseconds.
///
/// The old value here was 50 ms, and it was a retry: the request rode the
/// unreliable lane, so a page that never arrived had to be asked for again,
/// and one measurement window (the D16 < 50 ms first-page-in budget) was the
/// natural period. That mechanism is what PR #15/#16 caught amplifying — a
/// gateway slow enough to miss the window gets the whole 27-cell set asked for
/// again, twenty times a second, per client, for as long as it stays slow.
///
/// On the reliable lane the retry has nothing left to recover. A subscribe is
/// delivered or the connection is gone, and a gone connection re-subscribes on
/// reconnect anyway ([`AreaLoader::begin_round`] runs again for the new
/// session). What remains is a backstop against a gateway that accepted a
/// subscribe and answered part of it — 2 s, two orders of magnitude off the
/// page-in budget, and gated on the round actually being incomplete, so a
/// fully-answered subscription is never re-issued at all.
const RESUBSCRIBE_BACKSTOP_MS: u64 = 2_000;

/// The Bevy system that drives the area loader.
///
/// Issues a [`GatewayMsg::Subscribe`] when the subscribed cell set differs
/// from the last-sent set (the AOI system updates `loader.cells` on a
/// crossing), and otherwise only when the round is still missing pages and the
/// backstop interval has elapsed. Pages are recorded as they arrive.
pub fn drive_area_loader(
    cfg: Res<PersistClientConfig>,
    session: Res<GatewaySession>,
    mut loader: ResMut<AreaLoader>,
    mut streams: Query<&mut aeronet_iroh::stream::IrohStreamIo>,
) {
    if !session.is_connected() {
        return;
    }
    let Some(entity) = session.session else {
        return;
    };
    let Ok(mut streams) = streams.get_mut(entity) else {
        return;
    };

    // The AOI system updates `loader.cells` on a crossing; here we detect the
    // change and issue the subscribe.
    let subscribed = loader.cells.clone();
    if subscribed.is_empty() {
        return;
    }
    let now = Instant::now();
    if subscribed == loader.last_sent && !loader.round_is_overdue(now, cfg.area_cells_per_round) {
        return;
    }
    loader.last_subscribe = Some(now);
    loader.last_sent = subscribed.clone();

    let round: Vec<CellId> = subscribed
        .iter()
        .take(cfg.area_cells_per_round)
        .copied()
        .collect();
    let msg = orrery_protocol::GatewayMsg::Subscribe {
        grid: loader.grid,
        cells: round,
    };
    GatewaySession::push_control(&mut streams, &msg);
}

/// Wire the spatial AOI into the area loader (D5 → D11 §9).
///
/// On an [`AoiSubscription`] change, sets `loader.cells` to the neighborhood
/// ordered nearest-first from the local player's committed [`Cell`] (so the
/// gateway streams the centre page first, D16) and drops every page whose
/// cell left the subscription ([`AreaLoader::begin_round`]). Runs before
/// [`drive_area_loader`] in [`PersistClientSet::Flush`](crate::PersistClientSet)
/// so a crossing costs at most one update of latency.
///
/// The loader's `grid` (P-7) is set by the game from the player's reference
/// frame; this system only reorders the cells the spatial plugin computed.
pub fn sync_aoi_to_loader(
    aoi: Option<Res<orrery_spatial::plugin::AoiSubscription>>,
    player: Query<&orrery_spatial::plugin::Cell, With<orrery_spatial::plugin::LocalPlayer>>,
    mut loader: ResMut<AreaLoader>,
) {
    // Optional: a client without the spatial plugin installed (tests, harness
    // tools) drives `loader.cells` directly and this system stays a no-op.
    let Some(aoi) = aoi else { return };
    if !aoi.is_changed() {
        return;
    }
    let Ok(center) = player.single() else {
        return;
    };
    let cells = order_nearest_first(center.0, aoi.cells.clone());
    if cells == loader.cells {
        return;
    }
    loader.begin_round(cells);
}

#[cfg(test)]
mod tests {
    use super::*;
    use glam::IVec3;

    #[allow(clippy::needless_pass_by_value)]
    fn cell(x: i32, y: i32, z: i32) -> CellId {
        CellId::from_coords(IVec3::new(x, y, z), CellId::MAX_LEVEL).unwrap()
    }

    fn manhattan(cell: CellId) -> i32 {
        let (c, _) = cell.coords();
        c.abs().element_sum()
    }

    #[test]
    fn neighbour_tiers_are_contiguous() {
        // Manhattan distance tiers: index 0 is the centre, 1..7 the 6 face
        // cells (distance 1), 7..19 the 12 edge cells (distance 2), 19..27
        // the 8 corner cells (distance 3) — 1 + 6 + 12 + 8 = 27.
        let center = cell(0, 0, 0);
        let mut cells = center.neighbors27();
        // Shuffle to prove ordering is deterministic.
        cells.reverse();
        let ordered = order_nearest_first(center, cells);
        assert_eq!(ordered.len(), 27);
        assert_eq!(ordered[0], center, "index 0 is the centre");
        for (i, c) in ordered.iter().enumerate().skip(1) {
            let expected = match i {
                1..=6 => 1,
                7..=18 => 2,
                _ => 3,
            };
            assert_eq!(manhattan(*c), expected, "index {i} tier {expected}");
        }
        // All 27 present, no duplicates.
        let mut seen = std::collections::HashSet::new();
        for c in &ordered {
            assert!(seen.insert(*c), "duplicate {c:?}");
        }
        assert_eq!(seen.len(), 27);
    }

    #[test]
    fn loader_records_and_replaces_pages() {
        let mut loader = AreaLoader::new();
        let page = LoadedPage {
            cell: cell(0, 0, 0),
            entities: vec![PersistId::new(1)],
            payloads: vec![bytes::Bytes::from_static(b"x")],
            live: true,
        };
        loader.record(page.clone());
        assert_eq!(loader.page_count(), 1);
        // Re-recording the same cell replaces.
        loader.record(LoadedPage {
            cell: cell(0, 0, 0),
            entities: vec![PersistId::new(2)],
            payloads: vec![bytes::Bytes::from_static(b"y")],
            live: true,
        });
        assert_eq!(loader.page_count(), 1);
        assert_eq!(loader.pages[0].entities, vec![PersistId::new(2)]);
    }

    #[test]
    fn contains_reflects_subscription() {
        let mut loader = AreaLoader::new();
        loader.cells = vec![cell(0, 0, 0)];
        assert!(loader.contains(cell(0, 0, 0)));
        assert!(!loader.contains(cell(1, 0, 0)));
    }

    #[test]
    fn first_page_at_is_set_on_the_first_page_of_a_round() {
        let mut loader = AreaLoader::new();
        assert!(loader.first_page_at.is_none(), "None before any page");
        let page = |id: u64| LoadedPage {
            cell: cell(id as i32, 0, 0),
            entities: vec![PersistId::new(id)],
            payloads: vec![bytes::Bytes::from_static(b"x")],
            live: true,
        };
        loader.record(page(0));
        let first = loader.first_page_at.expect("Some after the first page");
        loader.record(page(1));
        assert_eq!(
            loader.first_page_at,
            Some(first),
            "unchanged by the second page"
        );
    }

    fn chunk(cell: CellId, seq: u32, index: u32, total: u32, ids: &[u64]) -> AreaPage {
        AreaPage {
            cell,
            page_seq: seq,
            chunk_index: index,
            total_chunks: total,
            entities: ids.iter().map(|&id| PersistId::new(id)).collect(),
            payloads: ids
                .iter()
                .map(|&id| bytes::Bytes::from(id.to_le_bytes().to_vec()))
                .collect(),
            live: true,
        }
    }

    #[test]
    fn partial_sequence_is_never_presented_as_complete() {
        let mut loader = AreaLoader::new();
        let c = cell(0, 0, 0);
        // Chunks 0 and 2 of a 3-chunk page: nothing is recorded.
        loader.record_frame(chunk(c, 1, 0, 3, &[1, 2]));
        loader.record_frame(chunk(c, 1, 2, 3, &[5, 6]));
        assert_eq!(loader.page_count(), 0, "partial page held, not recorded");
        // Chunk 1 completes the sequence; the full page lands.
        loader.record_frame(chunk(c, 1, 1, 3, &[3, 4]));
        assert_eq!(loader.page_count(), 1);
        let ids: Vec<PersistId> = loader.pages[0].entities.clone();
        assert_eq!(
            ids,
            vec![
                PersistId::new(1),
                PersistId::new(2),
                PersistId::new(3),
                PersistId::new(4),
                PersistId::new(5),
                PersistId::new(6),
            ],
            "chunks applied in chunk-index order"
        );
    }

    #[test]
    fn restarted_sequence_supersedes_stale_partial() {
        let mut loader = AreaLoader::new();
        let c = cell(0, 0, 0);
        // A 3-chunk page (seq 1) stalls after chunk 0.
        loader.record_frame(chunk(c, 1, 0, 3, &[1]));
        // The retry re-sends the page under a new seq, superseding the
        // partial — here as a single-chunk page.
        loader.record_frame(chunk(c, 2, 0, 1, &[9]));
        assert_eq!(loader.page_count(), 1);
        assert_eq!(loader.pages[0].entities, vec![PersistId::new(9)]);
    }

    #[test]
    fn stale_seq_never_mixes_with_a_retry() {
        let mut loader = AreaLoader::new();
        let c = cell(0, 0, 0);
        // Seq 1 arrives in part; the retry (seq 2) completes fully; a late
        // chunk of seq 1 afterwards must not corrupt the recorded page.
        loader.record_frame(chunk(c, 1, 0, 2, &[1]));
        loader.record_frame(chunk(c, 2, 0, 2, &[7]));
        loader.record_frame(chunk(c, 2, 1, 2, &[8]));
        assert_eq!(loader.page_count(), 1);
        assert_eq!(
            loader.pages[0].entities,
            vec![PersistId::new(7), PersistId::new(8)]
        );
        loader.record_frame(chunk(c, 1, 1, 2, &[2]));
        assert_eq!(loader.page_count(), 1, "the stale chunk did not replace");
        assert_eq!(
            loader.pages[0].entities,
            vec![PersistId::new(7), PersistId::new(8)]
        );
    }

    #[test]
    fn begin_round_evicts_departed_pages_and_clears_first_page_at() {
        let mut loader = AreaLoader::new();
        let center = cell(0, 0, 0);
        let round_a = order_nearest_first(center, center.neighbors27());
        loader.begin_round(round_a.clone());
        loader.record(LoadedPage {
            cell: center,
            entities: vec![PersistId::new(1)],
            payloads: vec![bytes::Bytes::from_static(b"x")],
            live: true,
        });
        assert!(loader.first_page_at.is_some());

        // Cross one cell east: the new neighborhood keeps 18 of the 27 cells.
        let east = cell(1, 0, 0);
        let round_b = order_nearest_first(east, east.neighbors27());
        loader.begin_round(round_b.clone());
        assert!(
            loader.first_page_at.is_none(),
            "new round re-arms the timer"
        );
        assert!(
            loader.pages.iter().all(|p| round_b.contains(&p.cell)),
            "every kept page is in the new subscription"
        );
        assert!(
            loader.page_count() <= round_b.len(),
            "page count bounded by the subscription"
        );
    }
}
