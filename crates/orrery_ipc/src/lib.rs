//! Engine-neutral IPC schema for a headless Orrery prediction sidecar.
//!
//! This crate defines messages and their byte encoding, not a transport. A
//! later integration may put the bytes on a socket, pipe, or shared-memory
//! queue without changing the extraction contract here. Its only dependency
//! is `orrery_protocol`, keeping Bevy, Lightyear, and engine-native handles on
//! the sidecar side of the boundary.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use orrery_protocol::{InterpBasis, LatticePoint, PersistId, QuantizedDir, Tick, UNorm16};

const MAGIC: [u8; 4] = *b"ORIP";
const ENGINE_TO_SIDECAR: u8 = 0;
const SIDECAR_TO_ENGINE: u8 = 1;

/// Current version of the encoded IPC schema.
///
/// This is independent of `orrery_protocol::PROTOCOL_VERSION`, which versions
/// peer traffic rather than the local engine boundary.
pub const IPC_SCHEMA_VERSION: u16 = 1;

/// Result of encoding one IPC message.
pub type EncodeResult = Result<Vec<u8>, EncodeError>;

/// Failure to encode an IPC message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EncodeError {
    /// A repeated or opaque field cannot fit the schema's `u32` length.
    LengthOverflow {
        /// Field whose length exceeded the wire representation.
        field: &'static str,
    },
}

impl core::fmt::Display for EncodeError {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::LengthOverflow { field } => {
                write!(formatter, "IPC field {field} is too long to encode")
            }
        }
    }
}

impl core::error::Error for EncodeError {}

/// Failure to decode an IPC message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecodeError {
    /// The bytes are not one complete value of the expected direction.
    Malformed {
        /// Structural problem found by the decoder.
        reason: &'static str,
    },
    /// The message uses an IPC schema this build does not understand.
    UnsupportedVersion {
        /// Version carried by the message.
        received: u16,
    },
    /// The bytes carry a message flowing in the other direction.
    UnexpectedDirection {
        /// Direction byte carried by the message.
        received: u8,
    },
}

impl core::fmt::Display for DecodeError {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Malformed { reason } => write!(formatter, "malformed IPC message: {reason}"),
            Self::UnsupportedVersion { received } => write!(
                formatter,
                "unsupported IPC schema version {received}; expected {IPC_SCHEMA_VERSION}"
            ),
            Self::UnexpectedDirection { received } => {
                write!(formatter, "unexpected IPC direction {received}")
            }
        }
    }
}

impl core::error::Error for DecodeError {}

struct Writer(Vec<u8>);

impl Writer {
    fn message(direction: u8, kind: u8) -> Self {
        let mut writer = Self(Vec::new());
        writer.0.extend_from_slice(&MAGIC);
        writer.u16(IPC_SCHEMA_VERSION);
        writer.u8(direction);
        writer.u8(kind);
        writer
    }

    fn u8(&mut self, value: u8) {
        self.0.push(value);
    }

    fn u16(&mut self, value: u16) {
        self.0.extend_from_slice(&value.to_le_bytes());
    }

    fn i16(&mut self, value: i16) {
        self.0.extend_from_slice(&value.to_le_bytes());
    }

    fn u32(&mut self, value: u32) {
        self.0.extend_from_slice(&value.to_le_bytes());
    }

    fn u64(&mut self, value: u64) {
        self.0.extend_from_slice(&value.to_le_bytes());
    }

    fn i64(&mut self, value: i64) {
        self.0.extend_from_slice(&value.to_le_bytes());
    }

    fn len(&mut self, field: &'static str, len: usize) -> Result<(), EncodeError> {
        let len = u32::try_from(len).map_err(|_| EncodeError::LengthOverflow { field })?;
        self.u32(len);
        Ok(())
    }

    fn persist_id(&mut self, value: PersistId) {
        self.u64(value.0);
    }

    fn tick(&mut self, value: Tick) {
        self.u64(value.0);
    }

    fn direction(&mut self, value: QuantizedDir) {
        self.i16(value.x);
        self.i16(value.y);
        self.i16(value.z);
    }

    fn transform(&mut self, value: QuantizedTransform) {
        self.i64(value.translation.x);
        self.i64(value.translation.y);
        self.i64(value.translation.z);
        self.direction(value.forward);
        self.direction(value.up);
    }

    fn basis(&mut self, value: InterpBasis) {
        self.tick(value.from);
        self.tick(value.to);
        self.u16(value.alpha.0);
    }

    fn frame(&mut self, value: EntityFrame) {
        self.persist_id(value.persist_id);
        self.transform(value.transform);
        self.basis(value.basis);
    }
}

struct Reader<'a> {
    bytes: &'a [u8],
    cursor: usize,
}

impl<'a> Reader<'a> {
    fn message(bytes: &'a [u8], expected_direction: u8) -> Result<(Self, u8), DecodeError> {
        let mut reader = Self { bytes, cursor: 0 };
        if reader.take(4)? != MAGIC {
            return Err(DecodeError::Malformed {
                reason: "bad IPC magic",
            });
        }
        let version = reader.u16()?;
        if version != IPC_SCHEMA_VERSION {
            return Err(DecodeError::UnsupportedVersion { received: version });
        }
        let direction = reader.u8()?;
        if direction != expected_direction {
            return Err(DecodeError::UnexpectedDirection {
                received: direction,
            });
        }
        let kind = reader.u8()?;
        Ok((reader, kind))
    }

    const fn finish(self) -> Result<(), DecodeError> {
        if self.cursor == self.bytes.len() {
            Ok(())
        } else {
            Err(DecodeError::Malformed {
                reason: "trailing bytes",
            })
        }
    }

    fn take(&mut self, len: usize) -> Result<&'a [u8], DecodeError> {
        let end = self
            .cursor
            .checked_add(len)
            .filter(|end| *end <= self.bytes.len())
            .ok_or(DecodeError::Malformed {
                reason: "truncated field",
            })?;
        let value = &self.bytes[self.cursor..end];
        self.cursor = end;
        Ok(value)
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N], DecodeError> {
        self.take(N)?
            .try_into()
            .map_err(|_| DecodeError::Malformed {
                reason: "invalid fixed-width field",
            })
    }

    fn u8(&mut self) -> Result<u8, DecodeError> {
        Ok(self.array::<1>()?[0])
    }

    fn u16(&mut self) -> Result<u16, DecodeError> {
        Ok(u16::from_le_bytes(self.array()?))
    }

    fn i16(&mut self) -> Result<i16, DecodeError> {
        Ok(i16::from_le_bytes(self.array()?))
    }

    fn u32(&mut self) -> Result<u32, DecodeError> {
        Ok(u32::from_le_bytes(self.array()?))
    }

    fn u64(&mut self) -> Result<u64, DecodeError> {
        Ok(u64::from_le_bytes(self.array()?))
    }

    fn i64(&mut self) -> Result<i64, DecodeError> {
        Ok(i64::from_le_bytes(self.array()?))
    }

    fn count(&mut self) -> Result<usize, DecodeError> {
        usize::try_from(self.u32()?).map_err(|_| DecodeError::Malformed {
            reason: "length does not fit this platform",
        })
    }

    fn persist_id(&mut self) -> Result<PersistId, DecodeError> {
        Ok(PersistId::new(self.u64()?))
    }

    fn tick(&mut self) -> Result<Tick, DecodeError> {
        Ok(Tick::new(self.u64()?))
    }

    fn direction(&mut self) -> Result<QuantizedDir, DecodeError> {
        Ok(QuantizedDir::new(self.i16()?, self.i16()?, self.i16()?))
    }

    fn transform(&mut self) -> Result<QuantizedTransform, DecodeError> {
        Ok(QuantizedTransform {
            translation: LatticePoint::new(self.i64()?, self.i64()?, self.i64()?),
            forward: self.direction()?,
            up: self.direction()?,
        })
    }

    fn basis(&mut self) -> Result<InterpBasis, DecodeError> {
        Ok(InterpBasis {
            from: self.tick()?,
            to: self.tick()?,
            alpha: UNorm16(self.u16()?),
        })
    }

    fn frame(&mut self) -> Result<EntityFrame, DecodeError> {
        Ok(EntityFrame {
            persist_id: self.persist_id()?,
            transform: self.transform()?,
            basis: self.basis()?,
        })
    }
}

/// One game-defined input addressed to a predicted entity.
///
/// `payload` is the game's canonical input encoding. The schema keeps it
/// opaque because only the game rules know its concrete type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntityInput {
    /// Entity whose predicted timeline consumes the input.
    pub target: PersistId,
    /// Stable order among inputs for the same entity and tick.
    pub sequence: u16,
    /// Canonical bytes of the game-defined input.
    pub payload: Vec<u8>,
}

/// All game inputs sampled for one universe tick.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputBatch {
    /// Universe tick at which the inputs were sampled.
    pub tick: Tick,
    /// Inputs in the order the engine submitted them.
    pub inputs: Vec<EntityInput>,
}

/// Messages flowing from the engine into the sidecar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EngineToSidecar {
    /// Game input for a fixed simulation tick.
    Input(InputBatch),
}

impl EngineToSidecar {
    /// Encode one engine-to-sidecar message with its IPC schema version.
    ///
    /// # Errors
    ///
    /// Returns [`EncodeError::LengthOverflow`] when a repeated or opaque field
    /// is too large for the schema's `u32` length.
    pub fn encode(self) -> EncodeResult {
        let Self::Input(batch) = self;
        let mut writer = Writer::message(ENGINE_TO_SIDECAR, 0);
        writer.tick(batch.tick);
        writer.len("inputs", batch.inputs.len())?;
        for input in batch.inputs {
            writer.persist_id(input.target);
            writer.u16(input.sequence);
            writer.len("input payload", input.payload.len())?;
            writer.0.extend_from_slice(&input.payload);
        }
        Ok(writer.0)
    }

    /// Decode and version-check one engine-to-sidecar message.
    ///
    /// # Errors
    ///
    /// Returns [`DecodeError`] when the bytes are malformed, versioned for a
    /// different schema, or flow in the other direction.
    pub fn decode(bytes: &[u8]) -> Result<Self, DecodeError> {
        let (mut reader, kind) = Reader::message(bytes, ENGINE_TO_SIDECAR)?;
        if kind != 0 {
            return Err(DecodeError::Malformed {
                reason: "unknown engine-to-sidecar message kind",
            });
        }
        let tick = reader.tick()?;
        let input_count = reader.count()?;
        let mut inputs = Vec::new();
        for _ in 0..input_count {
            let target = reader.persist_id()?;
            let sequence = reader.u16()?;
            let payload_len = reader.count()?;
            let payload = reader.take(payload_len)?.to_vec();
            inputs.push(EntityInput {
                target,
                sequence,
                payload,
            });
        }
        reader.finish()?;
        Ok(Self::Input(InputBatch { tick, inputs }))
    }
}

/// Renderer-ready transform with no floating-point wire fields.
///
/// Translation reuses the protocol's millimetre lattice. Orientation reuses
/// its signed-i16 direction quantization as forward and up vectors; their
/// magnitude is irrelevant, but a producer must provide non-zero,
/// non-collinear directions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QuantizedTransform {
    /// Grid-relative translation in millimetres.
    pub translation: LatticePoint,
    /// Quantized forward direction.
    pub forward: QuantizedDir,
    /// Quantized up direction.
    pub up: QuantizedDir,
}

/// One predicted or interpolated entity as it was actually presented.
///
/// `basis` is mandatory even for exact samples. A predicted entity normally
/// uses `InterpBasis::exact`; an interpolated entity must carry the two source
/// ticks and blend factor used to render it. Consumers use this exact value
/// when constructing a later hit claim.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EntityFrame {
    /// Stable identity, never a Bevy or engine-native entity handle.
    pub persist_id: PersistId,
    /// Quantized transform rendered for this entity.
    pub transform: QuantizedTransform,
    /// Snapshot interpolation basis used to produce `transform`.
    pub basis: InterpBasis,
}

/// One renderer extraction containing both timeline classes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameBatch {
    /// Universe tick at which the sidecar extracted the frame.
    pub extracted_at: Tick,
    /// Locally predicted entity frames.
    pub predicted: Vec<EntityFrame>,
    /// Remotely interpolated entity frames.
    pub interpolated: Vec<EntityFrame>,
}

/// Stable ids newly entering the engine's presentation set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpawnBatch {
    /// Spawned entities, keyed solely by stable identity.
    pub entities: Vec<PersistId>,
}

/// Stable ids leaving the engine's presentation set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DespawnBatch {
    /// Despawned entities, keyed solely by stable identity.
    pub entities: Vec<PersistId>,
}

/// Notification that Lightyear added a visual correction for one entity.
///
/// The generic correction error `D` has no honest universal wire shape. This
/// notice therefore reports the event, while the accompanying and subsequent
/// [`FrameBatch`] values carry the regenerated quantized transform to apply by
/// overwrite.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CorrectionNotice {
    /// Stable identity of the corrected predicted entity.
    pub persist_id: PersistId,
    /// Universe tick when `Added<VisualCorrection<D>>` was observed.
    pub observed_at: Tick,
}

/// Visual corrections observed during one extraction pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CorrectionBatch {
    /// One notice per newly added visual-correction component.
    pub corrections: Vec<CorrectionNotice>,
}

/// Messages flowing from the sidecar out to the engine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SidecarToEngine {
    /// Predicted and interpolated presentation frames.
    Frames(FrameBatch),
    /// Entities newly entering presentation.
    Spawns(SpawnBatch),
    /// Entities leaving presentation.
    Despawns(DespawnBatch),
    /// Newly observed visual corrections.
    Corrections(CorrectionBatch),
}

impl SidecarToEngine {
    /// Encode one sidecar-to-engine message with its IPC schema version.
    ///
    /// # Errors
    ///
    /// Returns [`EncodeError::LengthOverflow`] when a repeated field is too
    /// large for the schema's `u32` length.
    pub fn encode(self) -> EncodeResult {
        let kind = match &self {
            Self::Frames(_) => 0,
            Self::Spawns(_) => 1,
            Self::Despawns(_) => 2,
            Self::Corrections(_) => 3,
        };
        let mut writer = Writer::message(SIDECAR_TO_ENGINE, kind);
        match self {
            Self::Frames(batch) => {
                writer.tick(batch.extracted_at);
                writer.len("predicted frames", batch.predicted.len())?;
                for frame in batch.predicted {
                    writer.frame(frame);
                }
                writer.len("interpolated frames", batch.interpolated.len())?;
                for frame in batch.interpolated {
                    writer.frame(frame);
                }
            }
            Self::Spawns(batch) => encode_ids(&mut writer, "spawn ids", batch.entities)?,
            Self::Despawns(batch) => encode_ids(&mut writer, "despawn ids", batch.entities)?,
            Self::Corrections(batch) => {
                writer.len("correction notices", batch.corrections.len())?;
                for notice in batch.corrections {
                    writer.persist_id(notice.persist_id);
                    writer.tick(notice.observed_at);
                }
            }
        }
        Ok(writer.0)
    }

    /// Decode and version-check one sidecar-to-engine message.
    ///
    /// # Errors
    ///
    /// Returns [`DecodeError`] when the bytes are malformed, versioned for a
    /// different schema, or flow in the other direction.
    pub fn decode(bytes: &[u8]) -> Result<Self, DecodeError> {
        let (mut reader, kind) = Reader::message(bytes, SIDECAR_TO_ENGINE)?;
        let message = match kind {
            0 => Self::Frames(FrameBatch {
                extracted_at: reader.tick()?,
                predicted: decode_frames(&mut reader)?,
                interpolated: decode_frames(&mut reader)?,
            }),
            1 => Self::Spawns(SpawnBatch {
                entities: decode_ids(&mut reader)?,
            }),
            2 => Self::Despawns(DespawnBatch {
                entities: decode_ids(&mut reader)?,
            }),
            3 => {
                let count = reader.count()?;
                let mut corrections = Vec::new();
                for _ in 0..count {
                    corrections.push(CorrectionNotice {
                        persist_id: reader.persist_id()?,
                        observed_at: reader.tick()?,
                    });
                }
                Self::Corrections(CorrectionBatch { corrections })
            }
            _ => {
                return Err(DecodeError::Malformed {
                    reason: "unknown sidecar-to-engine message kind",
                });
            }
        };
        reader.finish()?;
        Ok(message)
    }
}

fn encode_ids(
    writer: &mut Writer,
    field: &'static str,
    ids: Vec<PersistId>,
) -> Result<(), EncodeError> {
    writer.len(field, ids.len())?;
    for id in ids {
        writer.persist_id(id);
    }
    Ok(())
}

fn decode_ids(reader: &mut Reader<'_>) -> Result<Vec<PersistId>, DecodeError> {
    let count = reader.count()?;
    let mut ids = Vec::new();
    for _ in 0..count {
        ids.push(reader.persist_id()?);
    }
    Ok(ids)
}

fn decode_frames(reader: &mut Reader<'_>) -> Result<Vec<EntityFrame>, DecodeError> {
    let count = reader.count()?;
    let mut frames = Vec::new();
    for _ in 0..count {
        frames.push(reader.frame()?);
    }
    Ok(frames)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn transform(x: i64) -> QuantizedTransform {
        QuantizedTransform {
            translation: LatticePoint::new(x, -2, 3),
            forward: QuantizedDir::new(32_000, 0, -700),
            up: QuantizedDir::new(0, 32_000, 0),
        }
    }

    fn roundtrip_in(message: EngineToSidecar) {
        let bytes = message.clone().encode().expect("input message encodes");
        assert_eq!(EngineToSidecar::decode(&bytes), Ok(message));
    }

    fn roundtrip_out(message: SidecarToEngine) {
        let bytes = message.clone().encode().expect("output message encodes");
        assert_eq!(SidecarToEngine::decode(&bytes), Ok(message));
    }

    #[test]
    fn input_message_roundtrips() {
        roundtrip_in(EngineToSidecar::Input(InputBatch {
            tick: Tick::new(898),
            inputs: vec![
                EntityInput {
                    target: PersistId::new(7),
                    sequence: 1,
                    payload: vec![0x10, 0x20],
                },
                EntityInput {
                    target: PersistId::new(9),
                    sequence: 2,
                    payload: vec![0x30],
                },
            ],
        }));
    }

    #[test]
    fn frame_message_roundtrips_predicted_and_interpolated_entities() {
        roundtrip_out(SidecarToEngine::Frames(FrameBatch {
            extracted_at: Tick::new(1_004),
            predicted: vec![EntityFrame {
                persist_id: PersistId::new(7),
                transform: transform(10),
                basis: InterpBasis::exact(Tick::new(1_004)),
            }],
            interpolated: vec![EntityFrame {
                persist_id: PersistId::new(9),
                transform: transform(20),
                basis: InterpBasis {
                    from: Tick::new(990),
                    to: Tick::new(993),
                    alpha: UNorm16(16_384),
                },
            }],
        }));
    }

    #[test]
    fn spawn_message_roundtrips() {
        roundtrip_out(SidecarToEngine::Spawns(SpawnBatch {
            entities: vec![PersistId::new(7), PersistId::new(9)],
        }));
    }

    #[test]
    fn despawn_message_roundtrips() {
        roundtrip_out(SidecarToEngine::Despawns(DespawnBatch {
            entities: vec![PersistId::new(9), PersistId::new(7)],
        }));
    }

    #[test]
    fn correction_message_roundtrips() {
        roundtrip_out(SidecarToEngine::Corrections(CorrectionBatch {
            corrections: vec![CorrectionNotice {
                persist_id: PersistId::new(7),
                observed_at: Tick::new(1_004),
            }],
        }));
    }

    #[test]
    fn decoder_rejects_an_unsupported_schema_version() {
        let mut bytes = EngineToSidecar::Input(InputBatch {
            tick: Tick::new(1),
            inputs: Vec::new(),
        })
        .encode()
        .expect("message encodes");
        bytes[4..6].copy_from_slice(&(IPC_SCHEMA_VERSION + 1).to_le_bytes());

        assert_eq!(
            EngineToSidecar::decode(&bytes),
            Err(DecodeError::UnsupportedVersion {
                received: IPC_SCHEMA_VERSION + 1,
            })
        );
    }

    #[test]
    fn decoder_rejects_trailing_bytes_and_the_wrong_direction() {
        let mut bytes = SidecarToEngine::Spawns(SpawnBatch {
            entities: Vec::new(),
        })
        .encode()
        .expect("message encodes");
        assert_eq!(
            EngineToSidecar::decode(&bytes),
            Err(DecodeError::UnexpectedDirection {
                received: SIDECAR_TO_ENGINE,
            })
        );

        bytes.push(0);
        assert_eq!(
            SidecarToEngine::decode(&bytes),
            Err(DecodeError::Malformed {
                reason: "trailing bytes",
            })
        );
    }
}
