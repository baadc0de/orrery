//! The Parquet object: encoding the twelve columns §11.1 specifies, in the
//! `(grid, cell, lsn)` order it specifies, plus the reader the round-trip
//! tests read them back with.
//!
//! **The dependency, and why this one.** `parquet = "59"` with
//! `default-features = false`, used through its *low-level* writer
//! (`SerializedFileWriter` + typed column writers) rather than through its
//! Arrow bridge. Turning the default features off is what makes this a small
//! dependency instead of a large one: the `arrow` feature is what pulls
//! `arrow-array`/`arrow-buffer`/`arrow-data`/`arrow-schema` in, and the
//! `async` feature is what would pull a runtime. Without them the whole
//! addition is 20 crates — `ahash`, `bytes` (already a dependency), `chrono`,
//! `half`, `hashbrown`, `num-bigint`/`num-integer`/`num-traits`, `seq-macro`,
//! `twox-hash` and their leaves — no Arrow, no tokio, no compression codecs,
//! and nothing that violates `scripts/core-gates.sh`'s async ban (which binds
//! the Tier-H host crates rather than this one, but is the standard the
//! workspace holds a new dependency to anyway).
//!
//! Writing Parquet by hand was the alternative and is not a real one: the
//! format is Thrift-framed metadata plus per-column-chunk page encodings, and
//! a hand-rolled writer that no other tool could read would defeat the point
//! of the format choice, which is D12's "Parquet is directly queryable by the
//! telemetry stack".
//!
//! **Objects are byte-deterministic in their input.** This is load-bearing for
//! #808's idempotence argument, not an incidental property: a crash between
//! the upload and the `jarchive/` row commit is recovered by re-encoding the
//! same segment and re-uploading it to the same key, and that is a no-op only
//! if the bytes are the same. Three things make them so — the sort key
//! `(grid, cell, lsn)` is *total* (an `Lsn` is unique within a journal), the
//! writer properties are fixed here rather than defaulted per build, and
//! nothing timestamped or environment-derived is written. The round-trip test
//! at the bottom of this module pins it.

use std::sync::Arc;

use orrery_protocol::{
    atrest::EncodingVersion, CellId, Epoch, GridId, JournalRecord, Lsn, NodeId, PersistId,
    RecordKind, Tick,
};
use parquet::basic::{Repetition, Type as PhysicalType};
use parquet::data_type::{ByteArray, ByteArrayType, Int32Type, Int64Type};
use parquet::file::properties::{WriterProperties, WriterVersion};
use parquet::file::writer::SerializedFileWriter;
use parquet::schema::types::Type;

use crate::journal::StoredRecord;

/// Why an archive object could not be encoded or decoded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchiveObjectError(pub String);

impl core::fmt::Display for ArchiveObjectError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(&self.0)
    }
}

impl core::error::Error for ArchiveObjectError {}

/// The Parquet message name for one archived journal record.
pub const ARCHIVE_SCHEMA_NAME: &str = "orrery_journal_record";

/// The column names, in §11.1's order. Public so a reader can assert the
/// layout it was written against rather than positional-index into it.
pub const ARCHIVE_COLUMNS: [&str; 12] = [
    "lsn_segment",
    "lsn_offset",
    "grid",
    "cell",
    "entity",
    "tick",
    "epoch",
    "author",
    "kind",
    "payload",
    "crc",
    "encoding_version",
];

/// The `kind` discriminant §11.1's `u8` column stores.
///
/// Pinned here rather than taken from `#[derive]` ordering: the column is a
/// durable at-rest value, and a `RecordKind` variant inserted in the middle
/// would silently re-number every object ever written if the discriminant were
/// derived. Adding a kind means adding an arm here with a *new* number. Kind
/// 1 is retired with v1 terrain and remains intentionally unassigned, so an
/// archive written before that decision is refused rather than reinterpreted.
#[must_use]
pub const fn kind_discriminant(kind: RecordKind) -> u8 {
    match kind {
        RecordKind::ComponentDiff => 0,
        RecordKind::Spawn => 2,
        RecordKind::Despawn => 3,
        RecordKind::Rekey => 4,
        RecordKind::CheckpointMark => 5,
        RecordKind::Restore => 6,
    }
}

/// The inverse of [`kind_discriminant`].
#[must_use]
pub const fn kind_from_discriminant(value: u8) -> Option<RecordKind> {
    match value {
        0 => Some(RecordKind::ComponentDiff),
        2 => Some(RecordKind::Spawn),
        3 => Some(RecordKind::Despawn),
        4 => Some(RecordKind::Rekey),
        5 => Some(RecordKind::CheckpointMark),
        6 => Some(RecordKind::Restore),
        _ => None,
    }
}

fn schema() -> Result<Arc<Type>, ArchiveObjectError> {
    let err = |e: parquet::errors::ParquetError| ArchiveObjectError(format!("archive schema: {e}"));
    let primitive = |name: &str, physical: PhysicalType| {
        Type::primitive_type_builder(name, physical)
            .with_repetition(Repetition::REQUIRED)
            .build()
            .map(Arc::new)
            .map_err(err)
    };
    // Parquet has no unsigned 64-bit physical type: `u64` columns are stored
    // as INT64 with the same bit pattern, and read back with `as u64`. That is
    // lossless — it is a reinterpretation, not a conversion — and it is why
    // `cell`, `entity`, `tick`, `epoch` and both `lsn` halves are INT64 here
    // while §11.1 calls them `u64`. `grid` and `crc` are `u32` and fit INT32
    // the same way; `kind` and `encoding_version` are `u8` and also ride INT32,
    // because Parquet has no narrower integer physical type.
    let fields = vec![
        primitive("lsn_segment", PhysicalType::INT64)?,
        primitive("lsn_offset", PhysicalType::INT64)?,
        primitive("grid", PhysicalType::INT32)?,
        primitive("cell", PhysicalType::INT64)?,
        primitive("entity", PhysicalType::INT64)?,
        primitive("tick", PhysicalType::INT64)?,
        primitive("epoch", PhysicalType::INT64)?,
        primitive("author", PhysicalType::BYTE_ARRAY)?,
        primitive("kind", PhysicalType::INT32)?,
        primitive("payload", PhysicalType::BYTE_ARRAY)?,
        primitive("crc", PhysicalType::INT32)?,
        primitive("encoding_version", PhysicalType::INT32)?,
    ];
    Type::group_type_builder(ARCHIVE_SCHEMA_NAME)
        .with_fields(fields)
        .build()
        .map(Arc::new)
        .map_err(err)
}

/// The writer properties, fixed rather than defaulted.
///
/// `WriterVersion::PARQUET_1_0` and an explicit "created by" string: both are
/// inputs to the file's own metadata, and a default that tracked the crate
/// version would make an object's bytes depend on which build wrote it, which
/// is precisely what the idempotent-retry argument cannot afford. No
/// compression, because none of the codec features are enabled — a deliberate
/// consequence of the minimal dependency set, and the cost is object size
/// rather than correctness.
fn properties() -> Arc<WriterProperties> {
    Arc::new(
        WriterProperties::builder()
            .set_writer_version(WriterVersion::PARQUET_1_0)
            .set_created_by("orrery-archive-tailer".to_owned())
            .build(),
    )
}

/// The `(grid, cell, lsn)` sort key §11.1 specifies, as a comparable tuple.
///
/// A newtype rather than a bare tuple returned from a closure: this is the
/// archive's clustering, the thing a reader's `cell_ranges` pruning depends
/// on, and it deserves a name that a future secondary clustering (#615's
/// `PersistId` question) has to be written against rather than beside.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct ArchiveSortKey {
    /// Primary: the grid the cell belongs to.
    pub grid: u32,
    /// Secondary: the Morton cell id.
    pub cell: u64,
    /// Tertiary, and the tiebreak that makes the order total: the record's
    /// journal position, unique within one journal.
    pub lsn_segment: u64,
    /// See [`ArchiveSortKey::lsn_segment`].
    pub lsn_offset: u64,
}

impl ArchiveSortKey {
    /// The sort key of one stored record.
    #[must_use]
    pub fn of(stored: &StoredRecord) -> Self {
        Self {
            grid: stored.record.grid.0,
            cell: stored.record.cell.to_bits(),
            lsn_segment: stored.lsn.segment,
            lsn_offset: stored.lsn.offset,
        }
    }
}

/// Encode `records` as one Parquet object.
///
/// The caller owns the sort: this function writes the rows in the order it is
/// given them, and [`sort_for_archive`] is what puts them in §11.1's order.
/// Splitting the two is what lets the tailer sort once, derive the metadata
/// row's `cell_ranges` from the same sorted slice, and encode without a second
/// pass.
///
/// # Errors
///
/// [`ArchiveObjectError`] if the Parquet writer rejects the schema or a batch.
pub fn encode_object(records: &[StoredRecord]) -> Result<Vec<u8>, ArchiveObjectError> {
    let err = |e: parquet::errors::ParquetError| ArchiveObjectError(format!("archive encode: {e}"));
    let mut buffer: Vec<u8> = Vec::new();
    let mut writer =
        SerializedFileWriter::new(&mut buffer, schema()?, properties()).map_err(err)?;
    // One row group for the whole object. The object *is* one sealed segment
    // (§11.6), so a second row group would only ever split one segment's
    // records by an arbitrary count, and the metadata row's `lsn_span` and
    // `cell_ranges` are already the pruning unit a reader uses to skip it.
    if !records.is_empty() {
        let mut group = writer.next_row_group().map_err(err)?;
        let i64s = |f: &dyn Fn(&StoredRecord) -> u64| -> Vec<i64> {
            records.iter().map(|r| f(r) as i64).collect()
        };
        let i32s = |f: &dyn Fn(&StoredRecord) -> u32| -> Vec<i32> {
            records.iter().map(|r| f(r) as i32).collect()
        };
        write_column::<Int64Type>(&mut group, &i64s(&|r| r.lsn.segment))?;
        write_column::<Int64Type>(&mut group, &i64s(&|r| r.lsn.offset))?;
        write_column::<Int32Type>(&mut group, &i32s(&|r| r.record.grid.0))?;
        write_column::<Int64Type>(&mut group, &i64s(&|r| r.record.cell.to_bits()))?;
        write_column::<Int64Type>(&mut group, &i64s(&|r| r.record.entity.0))?;
        write_column::<Int64Type>(&mut group, &i64s(&|r| r.record.tick.0))?;
        write_column::<Int64Type>(&mut group, &i64s(&|r| r.record.epoch.0))?;
        write_column::<ByteArrayType>(
            &mut group,
            &records
                .iter()
                .map(|r| ByteArray::from(r.record.author.as_bytes().to_vec()))
                .collect::<Vec<_>>(),
        )?;
        write_column::<Int32Type>(
            &mut group,
            &records
                .iter()
                .map(|r| i32::from(kind_discriminant(r.record.kind)))
                .collect::<Vec<_>>(),
        )?;
        write_column::<ByteArrayType>(
            &mut group,
            &records
                .iter()
                .map(|r| ByteArray::from(r.record.payload.to_vec()))
                .collect::<Vec<_>>(),
        )?;
        write_column::<Int32Type>(&mut group, &i32s(&|r| r.record.crc))?;
        write_column::<Int32Type>(
            &mut group,
            &records
                .iter()
                .map(|r| i32::from(r.encoding))
                .collect::<Vec<_>>(),
        )?;
        group.close().map_err(err)?;
    }
    writer.close().map_err(err)?;
    Ok(buffer)
}

fn write_column<T: parquet::data_type::DataType>(
    group: &mut parquet::file::writer::SerializedRowGroupWriter<'_, &mut Vec<u8>>,
    values: &[T::T],
) -> Result<(), ArchiveObjectError> {
    let err = |e: parquet::errors::ParquetError| ArchiveObjectError(format!("archive column: {e}"));
    let mut column = group
        .next_column()
        .map_err(err)?
        .ok_or_else(|| ArchiveObjectError("archive schema ran out of columns".into()))?;
    column
        .typed::<T>()
        .write_batch(values, None, None)
        .map_err(err)?;
    column.close().map_err(err)?;
    Ok(())
}

/// Put `records` into §11.1's `(grid, cell, lsn)` object order, in place.
pub fn sort_for_archive(records: &mut [StoredRecord]) {
    records.sort_unstable_by_key(ArchiveSortKey::of);
}

/// Decode an archive object back into stored records.
///
/// This exists for the round-trip assertions this crate's own tests make, and
/// as the shape #615's sweep and #809's rollback will read through. It is
/// deliberately whole-object rather than predicate-pushed: pruning happens on
/// the `jarchive/` metadata rows before an object is fetched at all, which is
/// the layout §11.1 describes, and a reader that needs row-group pruning
/// inside an object is #615's measurement to justify rather than this
/// change's to guess at.
///
/// # Errors
///
/// [`ArchiveObjectError`] if the bytes are not a readable object of this
/// schema, or carry a value no `RecordKind` or `NodeId` accepts.
pub fn decode_object(bytes: &[u8]) -> Result<Vec<StoredRecord>, ArchiveObjectError> {
    use parquet::file::reader::{FileReader, SerializedFileReader};
    use parquet::record::Field;

    let err = |e: parquet::errors::ParquetError| ArchiveObjectError(format!("archive decode: {e}"));
    let reader = SerializedFileReader::new(bytes::Bytes::copy_from_slice(bytes)).map_err(err)?;
    let mut out = Vec::new();
    for row in reader.get_row_iter(None).map_err(err)? {
        let row = row.map_err(err)?;
        let fields: Vec<(&String, &Field)> = row.get_column_iter().collect();
        if fields.len() != ARCHIVE_COLUMNS.len() {
            return Err(ArchiveObjectError(format!(
                "archive object has {} columns, expected {}",
                fields.len(),
                ARCHIVE_COLUMNS.len()
            )));
        }
        for (index, (name, _)) in fields.iter().enumerate() {
            if name.as_str() != ARCHIVE_COLUMNS[index] {
                return Err(ArchiveObjectError(format!(
                    "archive column {index} is {name}, expected {}",
                    ARCHIVE_COLUMNS[index]
                )));
            }
        }
        let long = |i: usize| -> Result<u64, ArchiveObjectError> {
            match fields[i].1 {
                Field::Long(v) => Ok(*v as u64),
                other => Err(ArchiveObjectError(format!(
                    "archive column {} is {other:?}, expected a long",
                    ARCHIVE_COLUMNS[i]
                ))),
            }
        };
        let int = |i: usize| -> Result<u32, ArchiveObjectError> {
            match fields[i].1 {
                Field::Int(v) => Ok(*v as u32),
                other => Err(ArchiveObjectError(format!(
                    "archive column {} is {other:?}, expected an int",
                    ARCHIVE_COLUMNS[i]
                ))),
            }
        };
        let blob = |i: usize| -> Result<Vec<u8>, ArchiveObjectError> {
            match fields[i].1 {
                Field::Bytes(v) => Ok(v.data().to_vec()),
                other => Err(ArchiveObjectError(format!(
                    "archive column {} is {other:?}, expected bytes",
                    ARCHIVE_COLUMNS[i]
                ))),
            }
        };

        let lsn = Lsn::new(long(0)?, long(1)?);
        let author_bytes: [u8; 32] = blob(7)?.try_into().map_err(|_| {
            ArchiveObjectError("archive `author` column is not 32 bytes".to_owned())
        })?;
        let author = NodeId::from_bytes(&author_bytes)
            .map_err(|e| ArchiveObjectError(format!("archive `author` is not a NodeId: {e}")))?;
        let kind_value = u8::try_from(int(8)?)
            .map_err(|_| ArchiveObjectError("archive `kind` column out of range".to_owned()))?;
        let kind = kind_from_discriminant(kind_value).ok_or_else(|| {
            ArchiveObjectError(format!("archive `kind` column has no variant {kind_value}"))
        })?;
        let encoding = u8::try_from(int(11)?).map_err(|_| {
            ArchiveObjectError("archive `encoding_version` column out of range".to_owned())
        })?;
        out.push(StoredRecord {
            lsn,
            record: JournalRecord {
                lsn,
                cell: CellId::from_bits(long(3)?).ok_or_else(|| {
                    ArchiveObjectError("archive `cell` column is the invalid id 0".to_owned())
                })?,
                grid: GridId(int(2)?),
                entity: PersistId::new(long(4)?),
                tick: Tick::new(long(5)?),
                epoch: Epoch::new(long(6)?),
                author,
                kind,
                payload: bytes::Bytes::from(blob(9)?),
                crc: int(10)?,
            },
            encoding: encoding as EncodingVersion,
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(n: u8) -> NodeId {
        let mut seed = [0u8; 32];
        seed[0] = n;
        iroh::SecretKey::from_bytes(&seed).public()
    }

    fn stored(lsn: Lsn, grid: u32, cell: u64, entity: u64) -> StoredRecord {
        let payload = vec![u8::try_from(entity % 251).unwrap_or(0); 8];
        StoredRecord {
            lsn,
            record: JournalRecord {
                lsn,
                cell: CellId::from_bits(cell).expect("nonzero cell bits"),
                grid: GridId(grid),
                entity: PersistId::new(entity),
                tick: Tick::new(entity * 3),
                epoch: Epoch::new(7),
                author: node(2),
                kind: RecordKind::ComponentDiff,
                crc: crate::payload_crc(&payload),
                payload: bytes::Bytes::from(payload),
            },
            encoding: 1,
        }
    }

    #[test]
    fn an_object_round_trips_every_column_of_the_schema() {
        let input = vec![
            stored(Lsn::new(3, 0), 0, 9, 1),
            stored(Lsn::new(3, 64), 0, 2, 2),
            stored(Lsn::new(3, 128), 1, 2, 3),
        ];
        let bytes = encode_object(&input).expect("encode");
        assert_eq!(&bytes[..4], b"PAR1", "an archive object is a Parquet file");
        let decoded = decode_object(&bytes).expect("decode");
        assert_eq!(decoded.len(), input.len());
        for (a, b) in input.iter().zip(decoded.iter()) {
            assert_eq!(a.lsn, b.lsn);
            assert_eq!(a.encoding, b.encoding);
            assert_eq!(a.record, b.record, "every §11.1 column survives");
        }
    }

    #[test]
    fn the_sort_is_grid_then_cell_then_lsn_and_is_total() {
        let mut records = vec![
            stored(Lsn::new(3, 128), 1, 2, 3),
            stored(Lsn::new(3, 64), 0, 2, 2),
            stored(Lsn::new(3, 0), 0, 9, 1),
            stored(Lsn::new(3, 192), 0, 2, 4),
        ];
        sort_for_archive(&mut records);
        let keys: Vec<(u32, u64, u64)> = records
            .iter()
            .map(|r| (r.record.grid.0, r.record.cell.to_bits(), r.lsn.offset))
            .collect();
        assert_eq!(
            keys,
            vec![(0, 2, 64), (0, 2, 192), (0, 9, 0), (1, 2, 128)],
            "grid, then cell, then lsn — and the lsn tiebreak makes it total"
        );
    }

    #[test]
    fn encoding_the_same_records_twice_produces_the_same_bytes() {
        // The whole of #808's idempotent-retry argument: a crash between the
        // upload and the metadata commit is repaired by re-encoding and
        // re-uploading to the same key, which is a no-op only if this holds.
        let mut a = vec![
            stored(Lsn::new(5, 0), 0, 4, 1),
            stored(Lsn::new(5, 64), 0, 1, 2),
        ];
        let mut b = a.clone();
        b.reverse();
        sort_for_archive(&mut a);
        sort_for_archive(&mut b);
        assert_eq!(
            encode_object(&a).expect("encode a"),
            encode_object(&b).expect("encode b"),
            "byte-identical objects for the same records in any input order"
        );
    }

    #[test]
    fn an_empty_object_is_still_a_readable_parquet_file() {
        let bytes = encode_object(&[]).expect("encode");
        assert_eq!(decode_object(&bytes).expect("decode").len(), 0);
    }

    #[test]
    fn every_record_kind_has_a_pinned_discriminant_that_round_trips() {
        for kind in [
            RecordKind::ComponentDiff,
            RecordKind::Spawn,
            RecordKind::Despawn,
            RecordKind::Rekey,
            RecordKind::CheckpointMark,
            RecordKind::Restore,
        ] {
            assert_eq!(kind_from_discriminant(kind_discriminant(kind)), Some(kind));
        }
        assert_eq!(kind_from_discriminant(7), None);
    }

    #[test]
    fn retired_terrain_discriminant_is_refused_not_reused() {
        assert_eq!(
            kind_from_discriminant(1),
            None,
            "archive kind 1 was TerrainDelta and is permanently retired"
        );
    }
}
