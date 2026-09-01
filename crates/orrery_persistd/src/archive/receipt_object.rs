//! Parquet objects for the FDB-backed economic receipt stream (#832).
//!
//! The journal archive remains bulk-only. This is a separate object family
//! whose rows are ordered by the commit versionstamp already present in each
//! `ledger/receipt/` key. Effect vectors are columns of their own rather than
//! an opaque receipt blob, so a reader can recover balance deltas and item
//! ownership transitions without knowing the intent op vocabulary.

use std::sync::Arc;

use parquet::basic::{Repetition, Type as PhysicalType};
use parquet::data_type::{ByteArray, ByteArrayType, Int32Type};
use parquet::file::properties::{WriterProperties, WriterVersion};
use parquet::file::writer::SerializedFileWriter;
use parquet::schema::types::Type;

use crate::keyspace::{ReceiptBalanceDelta, ReceiptOwnershipTransition, ReceiptRow};

/// One decoded FDB receipt and the complete versionstamped key that ordered it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReceiptArchiveRow {
    /// Complete `lr || commit versionstamp` key.
    pub key: [u8; 12],
    /// Enriched economic receipt.
    pub receipt: ReceiptRow,
    /// At-rest generation decoded from the receipt value.
    pub encoding: u8,
}

/// A receipt archive object could not be encoded or decoded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReceiptObjectError(pub String);

impl core::fmt::Display for ReceiptObjectError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(&self.0)
    }
}

impl core::error::Error for ReceiptObjectError {}

/// Parquet schema name for economic receipts.
pub const RECEIPT_ARCHIVE_SCHEMA_NAME: &str = "orrery_ledger_receipt";

/// Receipt object columns, in stable order.
pub const RECEIPT_ARCHIVE_COLUMNS: [&str; 7] = [
    "receipt_key",
    "intent_id",
    "parties",
    "ops",
    "balance_deltas",
    "ownership_transitions",
    "encoding_version",
];

fn schema() -> Result<Arc<Type>, ReceiptObjectError> {
    let err = |e: parquet::errors::ParquetError| ReceiptObjectError(format!("receipt schema: {e}"));
    let primitive = |name: &str, physical: PhysicalType| {
        Type::primitive_type_builder(name, physical)
            .with_repetition(Repetition::REQUIRED)
            .build()
            .map(Arc::new)
            .map_err(err)
    };
    let fields = vec![
        primitive("receipt_key", PhysicalType::BYTE_ARRAY)?,
        primitive("intent_id", PhysicalType::BYTE_ARRAY)?,
        primitive("parties", PhysicalType::BYTE_ARRAY)?,
        primitive("ops", PhysicalType::BYTE_ARRAY)?,
        primitive("balance_deltas", PhysicalType::BYTE_ARRAY)?,
        primitive("ownership_transitions", PhysicalType::BYTE_ARRAY)?,
        primitive("encoding_version", PhysicalType::INT32)?,
    ];
    Type::group_type_builder(RECEIPT_ARCHIVE_SCHEMA_NAME)
        .with_fields(fields)
        .build()
        .map(Arc::new)
        .map_err(err)
}

fn properties() -> Arc<WriterProperties> {
    Arc::new(
        WriterProperties::builder()
            .set_writer_version(WriterVersion::PARQUET_1_0)
            .set_created_by("orrery-receipt-archive".to_owned())
            .build(),
    )
}

fn write_column<T: parquet::data_type::DataType>(
    group: &mut parquet::file::writer::SerializedRowGroupWriter<'_, &mut Vec<u8>>,
    values: &[T::T],
) -> Result<(), ReceiptObjectError> {
    let err = |e: parquet::errors::ParquetError| ReceiptObjectError(format!("receipt column: {e}"));
    let mut column = group
        .next_column()
        .map_err(err)?
        .ok_or_else(|| ReceiptObjectError("receipt schema ran out of columns".to_owned()))?;
    column
        .typed::<T>()
        .write_batch(values, None, None)
        .map_err(err)?;
    column.close().map_err(err)?;
    Ok(())
}

fn postcard_column<T: serde::Serialize>(
    values: &[T],
) -> Result<Vec<ByteArray>, ReceiptObjectError> {
    values
        .iter()
        .map(|value| {
            postcard::to_stdvec(value)
                .map(ByteArray::from)
                .map_err(|e| ReceiptObjectError(format!("receipt nested column encode: {e}")))
        })
        .collect()
}

fn decode_nested<T: serde::de::DeserializeOwned>(
    bytes: &[u8],
    column: &str,
) -> Result<T, ReceiptObjectError> {
    postcard::from_bytes(bytes)
        .map_err(|e| ReceiptObjectError(format!("receipt column {column} postcard decode: {e}")))
}

/// Encode one bounded scanner page as a deterministic Parquet object.
///
/// # Errors
///
/// Returns [`ReceiptObjectError`] if Parquet or a nested postcard column
/// rejects the rows.
pub fn encode_receipt_object(rows: &[ReceiptArchiveRow]) -> Result<Vec<u8>, ReceiptObjectError> {
    let err = |e: parquet::errors::ParquetError| ReceiptObjectError(format!("receipt encode: {e}"));
    let mut buffer = Vec::new();
    let mut writer =
        SerializedFileWriter::new(&mut buffer, schema()?, properties()).map_err(err)?;
    if !rows.is_empty() {
        let mut group = writer.next_row_group().map_err(err)?;
        write_column::<ByteArrayType>(
            &mut group,
            &rows
                .iter()
                .map(|row| ByteArray::from(row.key.to_vec()))
                .collect::<Vec<_>>(),
        )?;
        write_column::<ByteArrayType>(
            &mut group,
            &rows
                .iter()
                .map(|row| ByteArray::from(row.receipt.intent_id.to_le_bytes().to_vec()))
                .collect::<Vec<_>>(),
        )?;
        write_column::<ByteArrayType>(
            &mut group,
            &postcard_column(
                &rows
                    .iter()
                    .map(|row| row.receipt.parties.clone())
                    .collect::<Vec<_>>(),
            )?,
        )?;
        write_column::<ByteArrayType>(
            &mut group,
            &postcard_column(
                &rows
                    .iter()
                    .map(|row| row.receipt.ops.clone())
                    .collect::<Vec<_>>(),
            )?,
        )?;
        write_column::<ByteArrayType>(
            &mut group,
            &postcard_column(
                &rows
                    .iter()
                    .map(|row| row.receipt.balance_deltas.clone())
                    .collect::<Vec<_>>(),
            )?,
        )?;
        write_column::<ByteArrayType>(
            &mut group,
            &postcard_column(
                &rows
                    .iter()
                    .map(|row| row.receipt.ownership.clone())
                    .collect::<Vec<_>>(),
            )?,
        )?;
        write_column::<Int32Type>(
            &mut group,
            &rows
                .iter()
                .map(|row| i32::from(row.encoding))
                .collect::<Vec<_>>(),
        )?;
        group.close().map_err(err)?;
    }
    writer.close().map_err(err)?;
    Ok(buffer)
}

/// Decode a receipt object, recovering every enriched effect column.
///
/// # Errors
///
/// Returns [`ReceiptObjectError`] for a foreign schema, corrupt nested value,
/// or malformed key/id width.
pub fn decode_receipt_object(bytes: &[u8]) -> Result<Vec<ReceiptArchiveRow>, ReceiptObjectError> {
    use parquet::file::reader::{FileReader, SerializedFileReader};
    use parquet::record::Field;

    let err = |e: parquet::errors::ParquetError| ReceiptObjectError(format!("receipt decode: {e}"));
    let reader = SerializedFileReader::new(bytes::Bytes::copy_from_slice(bytes)).map_err(err)?;
    let mut out = Vec::new();
    for row in reader.get_row_iter(None).map_err(err)? {
        let row = row.map_err(err)?;
        let fields: Vec<(&String, &Field)> = row.get_column_iter().collect();
        if fields.len() != RECEIPT_ARCHIVE_COLUMNS.len() {
            return Err(ReceiptObjectError(format!(
                "receipt object has {} columns, expected {}",
                fields.len(),
                RECEIPT_ARCHIVE_COLUMNS.len()
            )));
        }
        for (index, (name, _)) in fields.iter().enumerate() {
            if name.as_str() != RECEIPT_ARCHIVE_COLUMNS[index] {
                return Err(ReceiptObjectError(format!(
                    "receipt column {index} is {name}, expected {}",
                    RECEIPT_ARCHIVE_COLUMNS[index]
                )));
            }
        }
        let blob = |index: usize| -> Result<Vec<u8>, ReceiptObjectError> {
            match fields[index].1 {
                Field::Bytes(value) => Ok(value.data().to_vec()),
                other => Err(ReceiptObjectError(format!(
                    "receipt column {} is {other:?}, expected bytes",
                    RECEIPT_ARCHIVE_COLUMNS[index]
                ))),
            }
        };
        let int = |index: usize| -> Result<i32, ReceiptObjectError> {
            match fields[index].1 {
                Field::Int(value) => Ok(*value),
                other => Err(ReceiptObjectError(format!(
                    "receipt column {} is {other:?}, expected int",
                    RECEIPT_ARCHIVE_COLUMNS[index]
                ))),
            }
        };
        let key: [u8; 12] = blob(0)?
            .try_into()
            .map_err(|_| ReceiptObjectError("receipt_key is not 12 bytes".to_owned()))?;
        if &key[..2] != b"lr" {
            return Err(ReceiptObjectError(
                "receipt_key is outside ledger/receipt".to_owned(),
            ));
        }
        let intent_id = u128::from_le_bytes(
            blob(1)?
                .try_into()
                .map_err(|_| ReceiptObjectError("intent_id is not 16 bytes".to_owned()))?,
        );
        let encoding = u8::try_from(int(6)?)
            .map_err(|_| ReceiptObjectError("encoding_version is out of range".to_owned()))?;
        let parties_bytes = blob(2)?;
        let ops_bytes = blob(3)?;
        let deltas_bytes = blob(4)?;
        let ownership_bytes = blob(5)?;
        out.push(ReceiptArchiveRow {
            key,
            receipt: ReceiptRow {
                intent_id,
                parties: decode_nested(&parties_bytes, RECEIPT_ARCHIVE_COLUMNS[2])?,
                ops: decode_nested(&ops_bytes, RECEIPT_ARCHIVE_COLUMNS[3])?,
                balance_deltas: decode_nested::<Vec<ReceiptBalanceDelta>>(
                    &deltas_bytes,
                    RECEIPT_ARCHIVE_COLUMNS[4],
                )?,
                ownership: decode_nested::<Vec<ReceiptOwnershipTransition>>(
                    &ownership_bytes,
                    RECEIPT_ARCHIVE_COLUMNS[5],
                )?,
            },
            encoding,
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use orrery_protocol::{AccountId, AssetId, ItemUid};

    #[test]
    fn receipt_object_round_trips_deltas_item_and_ownership() {
        let mut key = crate::keyspace::ledger_receipt_key();
        key[2..].copy_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8, 9, 10]);
        let input = vec![ReceiptArchiveRow {
            key,
            receipt: ReceiptRow {
                intent_id: 832,
                parties: vec![AccountId::new(1), AccountId::new(2)],
                ops: vec![0, 1],
                balance_deltas: vec![ReceiptBalanceDelta {
                    account: AccountId::new(1),
                    asset: AssetId::new(9),
                    delta: 70,
                }],
                ownership: vec![ReceiptOwnershipTransition {
                    item: ItemUid::new(44),
                    before: None,
                    after: Some(AccountId::new(2)),
                }],
            },
            encoding: crate::keyspace::RECEIPT_ENCODING_V1,
        }];
        let bytes = encode_receipt_object(&input).expect("encode");
        assert_eq!(&bytes[..4], b"PAR1");
        assert_eq!(decode_receipt_object(&bytes).expect("decode"), input);
    }
}
