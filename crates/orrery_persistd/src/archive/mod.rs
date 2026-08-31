//! The journal-to-archive path (#808, docs/08-persistence.md §11).
//!
//! D20 bounded the journal by a retention floor and #806 made a verified
//! archive watermark one of that floor's terms. This module is the thing that
//! moves the watermark: it consumes sealed journal segments, re-sorts them into
//! §11.1's `(grid, cell, lsn)` order, writes one Parquet object per
//! `(node_id, segment_seq)`, verifies it by reading it back, records it under
//! `jarchive/{node_id}/{segment_seq}`, and only then tells the journal it may
//! release.
//!
//! Four seams, one per file:
//!
//! - [`store`] — the object store, as a trait, with a filesystem backend.
//! - [`object`] — the Parquet encoding of §11.1's twelve columns.
//! - [`index`] — the `jarchive/` metadata rows, and watermark recovery.
//! - [`tailer`] — the loop, the ordering discipline, and the retry.
//!
//! **The operational coupling this introduces, named up front.** With the
//! clamp registered (`persistd --archive-retention`) an unreachable object
//! store stops the retention floor, and the journal then grows at the arrival
//! rate — ~26 MB/s at the P2 gate's load (D20). That terminates in
//! docs/08-persistence.md §15's "journal disk full → bulk acks shed", which is
//! the correct shed order (bulk is the shed-able class; history is not
//! re-creatable) but is a countdown, and a countdown nobody is told about is
//! the thing this module must not ship. Two places say it: the tailer's own
//! `warn` after [`ArchiveTailerConfig::alarm_after_failures`] consecutive
//! failures, and the checkpoint scheduler's escalation of a blocked release
//! once the floor-to-watermark gap passes
//! [`ARCHIVE_LAG_ALARM_SEGMENTS`](crate::journal::ARCHIVE_LAG_ALARM_SEGMENTS).
//! The runbook entry is docs/09-services-and-ops.md §10, "Archive unreachable".

pub mod index;
pub mod object;
pub mod store;
pub mod tailer;

#[cfg(feature = "fdb")]
pub use index::FdbJarchiveIndex;
pub use index::{
    recover_watermark, JarchiveIndex, JarchiveRow, MemJarchiveIndex, RecoveredWatermark,
};
pub use object::{
    decode_object, encode_object, sort_for_archive, ArchiveObjectError, ArchiveSortKey,
    ARCHIVE_COLUMNS,
};
pub use store::{ArchiveStore, ArchiveStoreError, FsArchiveStore};
pub use tailer::{
    object_key, spawn_archive_tailer, ArchiveStall, ArchiveTailer, ArchiveTailerConfig,
    ArchiveTailerHandle, ArchiveTailerStatus, TailerPass,
};
