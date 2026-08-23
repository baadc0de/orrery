//! Read-only census of `world/` component-bag framing.
//!
//! This is deliberately a diagnostic, not a migration: it classifies a live
//! row by calling [`ComponentBag::decode`] — the same decoder mapped to
//! [`crate::MigrationError::Decode`] by the migration path — and never encodes
//! or rewrites a value.

use std::collections::BTreeMap;

use orrery_protocol::atrest::SchemaVersion;

use crate::keyspace;
use crate::schema::ComponentBag;

/// Counts for one grid's live `world/` rows.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GridWorldCensus {
    /// Live bags accepted by the W1 `ComponentBag` decoder.
    pub framed: u64,
    /// Live bags rejected by that decoder (the opaque-v0 bootstrap case).
    pub legacy: u64,
    /// Framed bags, grouped by their derived schema floor.
    pub schema_floors: BTreeMap<SchemaVersion, u64>,
}

/// The complete read-only framing census, grouped by grid id.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorldCensus {
    /// Counts for each grid represented by a well-formed `world/` key.
    pub grids: BTreeMap<u32, GridWorldCensus>,
    /// `world/` keys whose value is a tombstone, unknown envelope, or
    /// truncated envelope; none carries a component bag to classify.
    pub non_live: u64,
    /// Keys in the scanned `w` range that are not well-formed `world/` keys.
    pub malformed_keys: u64,
}

impl WorldCensus {
    /// Incorporate one raw key/value pair from the `world/` range.
    ///
    /// The classifier deliberately derives the floor from the decoded bag,
    /// rather than trusting the versioned envelope's copied summary.
    pub fn observe(&mut self, key: &[u8], value: &[u8]) {
        let Some((grid, _, _)) = keyspace::decode_world_key(key) else {
            self.malformed_keys += 1;
            return;
        };
        let Some(components) = keyspace::world_value_components(value) else {
            self.non_live += 1;
            return;
        };
        let counts = self.grids.entry(grid.0).or_default();
        match ComponentBag::decode(components) {
            Ok(bag) => {
                counts.framed += 1;
                *counts.schema_floors.entry(bag.schema_floor()).or_default() += 1;
            }
            Err(_) => counts.legacy += 1,
        }
    }

    /// Incorporate a complete page, returning its last key for the next page.
    ///
    /// Keeping cursor advancement beside observation makes a caller that skips
    /// a fetched range observable in the completeness regression test below.
    #[must_use]
    pub fn observe_page<'a, I>(&mut self, page: I) -> Option<Vec<u8>>
    where
        I: IntoIterator<Item = (&'a [u8], &'a [u8])>,
    {
        let mut last = None;
        for (key, value) in page {
            self.observe(key, value);
            last = Some(key.to_vec());
        }
        last
    }
}

#[cfg(feature = "fdb")]
mod fdb_scan {
    use std::sync::Arc;

    use foundationdb::{Database, KeySelector, RangeOption};
    use futures::TryStreamExt;

    use super::WorldCensus;
    use crate::FdbContext;

    /// The maximum rows held by one read transaction.
    pub const DEFAULT_PAGE_ROWS: usize = 1_000;

    /// Scan every `world/` row using short, read-only-by-construction pages.
    ///
    /// Each page owns an FDB transaction only inside [`read_page`]. Its caller
    /// receives copied bytes, and `read_page` drops the transaction without
    /// calling `commit`; FoundationDB persists no mutation from an uncommitted
    /// transaction. Consequently the aggregation path cannot obtain a
    /// transaction or any `set`, `clear`, or `commit` capability.
    pub async fn scan(context: &FdbContext, page_rows: usize) -> Result<WorldCensus, String> {
        if page_rows == 0 {
            return Err("page_rows must be greater than zero".into());
        }
        let db = context.database();
        let mut census = WorldCensus::default();
        let mut cursor = None;
        loop {
            let page = read_page(&db, cursor.as_deref(), page_rows).await?;
            if page.is_empty() {
                return Ok(census);
            }
            cursor = census.observe_page(
                page.iter()
                    .map(|(key, value)| (key.as_slice(), value.as_slice())),
            );
        }
    }

    async fn read_page(
        db: &Arc<Database>,
        cursor: Option<&[u8]>,
        page_rows: usize,
    ) -> Result<Vec<(Vec<u8>, Vec<u8>)>, String> {
        // Do not use Database::run: it commits the transaction after its
        // closure. This direct transaction is consumed solely by reads then
        // dropped at this function boundary, before any aggregate is exposed.
        let trx = db
            .create_trx()
            .map_err(|error| format!("create census read transaction: {error}"))?;
        let range = RangeOption {
            begin: cursor.map_or_else(
                || KeySelector::first_greater_or_equal(b"w"),
                KeySelector::first_greater_than,
            ),
            end: KeySelector::first_greater_or_equal(b"x"),
            limit: Some(page_rows),
            ..RangeOption::default()
        };
        let mut stream = trx.get_ranges_keyvalues(range, true);
        let mut page = Vec::new();
        while let Some(kv) = stream
            .try_next()
            .await
            .map_err(|error| format!("read census page: {error}"))?
        {
            page.push((kv.key().to_vec(), kv.value().to_vec()));
        }
        // `trx` is intentionally dropped here. There is no `commit` call in
        // this function or any function it invokes.
        drop(stream);
        drop(trx);
        Ok(page)
    }
}

#[cfg(feature = "fdb")]
pub use fdb_scan::{scan as scan_fdb, DEFAULT_PAGE_ROWS};

#[cfg(test)]
mod tests {
    use bytes::Bytes;
    use orrery_core::ComponentTypeId;
    use orrery_protocol::{CellId, GridId, PersistId};

    use super::*;
    use crate::schema::ComponentSlot;

    fn key(grid: u32, entity: u64) -> [u8; 21] {
        keyspace::world_key(GridId::new(grid), CellId::ROOT, PersistId::new(entity))
    }

    fn framed_value(floor: SchemaVersion) -> Vec<u8> {
        let bag = ComponentBag {
            slots: vec![ComponentSlot {
                component: ComponentTypeId(7),
                schema_version: floor,
                payload: Bytes::from_static(b"component"),
            }],
        };
        keyspace::encode_live_value(&bag.encode().expect("test bag encodes"))
    }

    #[test]
    fn classifier_counts_an_opaque_v0_bag_as_legacy() {
        let mut census = WorldCensus::default();
        census.observe(
            &key(4, 1),
            &keyspace::encode_live_value(b"not postcard slots"),
        );
        census.observe(&key(4, 2), &framed_value(3));

        assert_eq!(census.grids[&4].legacy, 1, "decode failure is legacy");
        assert_eq!(census.grids[&4].framed, 1);
        assert_eq!(census.grids[&4].schema_floors[&3], 1);
    }

    #[test]
    fn paging_observes_every_range_before_advancing_the_cursor() {
        let first = vec![(key(1, 1).to_vec(), framed_value(1))];
        let second = vec![
            (key(1, 2).to_vec(), framed_value(2)),
            (key(2, 3).to_vec(), keyspace::encode_live_value(b"opaque")),
        ];
        let mut census = WorldCensus::default();
        let first_cursor = census.observe_page(
            first
                .iter()
                .map(|(key, value)| (key.as_slice(), value.as_slice())),
        );
        let second_cursor = census.observe_page(
            second
                .iter()
                .map(|(key, value)| (key.as_slice(), value.as_slice())),
        );

        assert_eq!(first_cursor, Some(key(1, 1).to_vec()));
        assert_eq!(second_cursor, Some(key(2, 3).to_vec()));
        assert_eq!(census.grids[&1].framed, 2, "both fetched pages counted");
        assert_eq!(census.grids[&2].legacy, 1, "last page was not skipped");
    }
}
