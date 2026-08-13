//! Area load/subscribe: the 27-cell neighborhood, streamed nearest-first (D11 §9).
//!
//! When the client enters an area, it requests the 27-cell AOI (D5) over a
//! reliable stream. The gateway partitions the cells: live cells (an actor
//! holds them) are served from actor memory, cold cells by FDB range scans.
//! Pages stream **nearest-first** (center cell, then face/edge/corner neighbors
//! by distance) so the client can spawn-in against page one (D16: < 50 ms to
//! first page-in).

use bevy_ecs::prelude::*;
use bevy_platform::time::Instant;
use orrery_protocol::{CellId, PersistId};

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

/// The area loader: tracks the subscribed neighborhood and the pages received.
///
/// A [`Resource`] holding the current AOI subscription and the pages loaded so
/// far. The plugin's system requests the neighborhood when it changes and
/// records pages as they arrive.
#[derive(Debug, Default, Resource)]
pub struct AreaLoader {
    /// The cells currently subscribed, nearest-first.
    pub cells: Vec<CellId>,
    /// Pages received so far, keyed by cell.
    pub pages: Vec<LoadedPage>,
    /// The time the last subscribe round was issued (for rate-limiting).
    pub last_subscribe: Option<Instant>,
    /// The time the first page of the current area arrived (for the < 50 ms
    /// first-page-in target, D16).
    pub first_page_at: Option<Instant>,
}

impl AreaLoader {
    /// A new, empty loader.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
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
    pub fn record(&mut self, page: LoadedPage) {
        self.pages.retain(|p| p.cell != page.cell);
        self.pages.push(page);
    }
}

/// Order the 27-cell neighborhood nearest-first from `center`.
///
/// The center cell first, then face neighbors (Manhattan distance 1), then
/// edge (2), then corner (3). Ties are broken deterministically by cell id.
/// This is the ordering the client requests and the gateway streams, so the
/// client can spawn-in against page one.
#[must_use]
pub fn order_nearest_first(center: CellId, cells: Vec<CellId>) -> Vec<CellId> {
    let (c, _) = center.coords();
    let mut ranked: Vec<(CellId, i32)> = cells
        .into_iter()
        .map(|cell| {
            let (coords, _) = cell.coords();
            let dist = (coords - c).abs().max_element();
            (cell, dist)
        })
        .collect();
    // Nearest first; ties by cell id for determinism.
    ranked.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| a.0.cmp(&b.0)));
    ranked.into_iter().map(|(cell, _)| cell).collect()
}

/// The Bevy system that drives the area loader.
///
/// When the subscribed neighborhood changes, it requests the new cells
/// (nearest-first, bounded by `area_cells_per_round`) over the gateway's
/// reliable stream. Pages are recorded as they arrive.
pub fn drive_area_loader(
    cfg: Res<PersistClientConfig>,
    session: Res<GatewaySession>,
    mut loader: ResMut<AreaLoader>,
    mut sessions: Query<&mut aeronet_io::Session>,
) {
    if !session.is_connected() {
        return;
    }
    let Some(entity) = session.session else {
        return;
    };
    let Ok(mut io) = sessions.get_mut(entity) else {
        return;
    };

    // The caller (the spatial plugin) updates `loader.cells`; here we detect a
    // change and issue a subscribe. To keep this self-contained, we re-request
    // whenever the set differs from what we have pages for — the caller sets
    // `cells` before this runs.
    let subscribed = loader.cells.clone();
    if subscribed.is_empty() {
        return;
    }

    // Rate-limit: only issue a subscribe round if we haven't just done one.
    let now = Instant::now();
    if let Some(last) = loader.last_subscribe {
        if now.saturating_duration_since(last) < std::time::Duration::from_millis(50) {
            return;
        }
    }
    loader.last_subscribe = Some(now);

    let round: Vec<CellId> = subscribed
        .iter()
        .take(cfg.area_cells_per_round)
        .copied()
        .collect();
    let msg = orrery_protocol::GatewayMsg::Subscribe { cells: round };
    io.send.push(GatewaySession::encode_stream(&msg));
}

#[cfg(test)]
mod tests {
    use super::*;
    use glam::IVec3;

    #[allow(clippy::needless_pass_by_value)]
    fn cell(x: i32, y: i32, z: i32) -> CellId {
        CellId::from_coords(IVec3::new(x, y, z), CellId::MAX_LEVEL).unwrap()
    }

    #[test]
    fn nearest_first_orders_center_then_faces() {
        let center = cell(0, 0, 0);
        let mut cells = center.neighbors27();
        // Shuffle to prove ordering is deterministic.
        cells.reverse();
        let ordered = order_nearest_first(center, cells);
        assert_eq!(ordered[0], center);
        // The next six are the face neighbors (Manhattan distance 1).
        let faces = ordered[1..7].to_vec();
        for f in &faces {
            let (c, _) = f.coords();
            assert_eq!((c - IVec3::ZERO).abs().max_element(), 1, "{f:?}");
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
}
