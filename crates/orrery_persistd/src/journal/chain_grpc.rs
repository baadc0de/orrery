//! Ownership-fenced tonic/gRPC transport for journal chain replication.
//!
//! Durable state is keyed by all of [`DurableChainId`]. Connection nonces are
//! deliberately absent from storage keys. Each mirrored record is atomically
//! indexed by `(chain, origin_lsn)` with batch provenance, so restart recovery
//! scans only that chain and can distinguish a complete contiguous prefix from
//! an append whose cursor update was interrupted.
//!
//! One bounded bidirectional gRPC stream carries the open handshake, ordered
//! batches, and durable acknowledgements for a connection session. Closure,
//! cancellation, any send/receive error, or a mismatched response permanently
//! invalidates that stream. Reconnect drops both halves, chooses a fresh nonce,
//! and obtains a reconstructed remote watermark before any replay. No sender
//! task is detached, so dropping a transport also cancels its request stream.

use std::collections::BTreeMap;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use bytes::{Buf, BufMut};
use futures::{SinkExt, Stream};
use hyper_util::client::legacy::{connect::HttpConnector, Client};
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto;
use prost::encoding::{self as pb, DecodeContext, WireType};
use prost::{DecodeError, Message};
use tokio::sync::{broadcast, Mutex};
use tonic::codec::{BufferSettings, Codec, DecodeBuf, Decoder, EncodeBuf, Encoder};
use tonic::codegen::{http, Body, BoxFuture, Service, StdError};
use tonic::{Request, Response, Status};

use orrery_protocol::{JournalRecord, Lsn, NodeId};

use crate::journal::{ChainTransport, Journal, JournalError};

const SERVICE_NAME: &str = "orrery.persistd.chain.v1.ChainReplication";
const STREAM_PATH: &str = "/orrery.persistd.chain.v1.ChainReplication/Replicate";
type GrpcClient = Client<HttpConnector, tonic::body::Body>;

#[derive(Debug, Clone, Default)]
struct ProstCodec<T, U>(core::marker::PhantomData<(T, U)>);

impl<T, U> Codec for ProstCodec<T, U>
where
    T: Message + Send + 'static,
    U: Message + Default + Send + 'static,
{
    type Encode = T;
    type Decode = U;
    type Encoder = ProstEncoder<T>;
    type Decoder = ProstDecoder<U>;

    fn encoder(&mut self) -> Self::Encoder {
        ProstEncoder(core::marker::PhantomData)
    }

    fn decoder(&mut self) -> Self::Decoder {
        ProstDecoder(core::marker::PhantomData)
    }
}

#[derive(Debug, Clone, Default)]
struct ProstEncoder<T>(core::marker::PhantomData<T>);

impl<T: Message> Encoder for ProstEncoder<T> {
    type Item = T;
    type Error = Status;

    fn encode(&mut self, item: T, buf: &mut EncodeBuf<'_>) -> Result<(), Status> {
        item.encode(buf)
            .expect("tonic allocated the encoded length");
        Ok(())
    }

    fn buffer_settings(&self) -> BufferSettings {
        BufferSettings::default()
    }
}

#[derive(Debug, Clone, Default)]
struct ProstDecoder<T>(core::marker::PhantomData<T>);

impl<T: Message + Default> Decoder for ProstDecoder<T> {
    type Item = T;
    type Error = Status;

    fn decode(&mut self, buf: &mut DecodeBuf<'_>) -> Result<Option<T>, Status> {
        T::decode(buf)
            .map(Some)
            .map_err(|e| Status::internal(e.to_string()))
    }

    fn buffer_settings(&self) -> BufferSettings {
        BufferSettings::default()
    }
}

/// Complete durable identity of a journal replication chain.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DurableChainId {
    /// Primary transport identity.
    pub primary_node: NodeId,
    /// Designated follower transport identity.
    pub follower_node: NodeId,
    /// Stable identity of the owned shard set.
    pub shard_set: Vec<u8>,
    /// Ownership fencing epoch.
    pub epoch: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct WireChainId {
    primary_node: Vec<u8>,
    follower_node: Vec<u8>,
    shard_set: Vec<u8>,
    epoch: u64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct WireLsn {
    segment: u64,
    offset: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct AppendBatchRequest {
    chain: Option<WireChainId>,
    session_nonce: Vec<u8>,
    batch_seq: u64,
    predecessor: Option<WireLsn>,
    first_lsn: Option<WireLsn>,
    last_lsn: Option<WireLsn>,
    records: Vec<Vec<u8>>,
    /// The retention floor the *primary* has reached in its own journal, which
    /// is what bounds the follower's mirror (D23). Carried on every frame it
    /// can be carried on rather than as a message of its own: it is one
    /// monotone LSN, the follower only ever takes the maximum, and a lost
    /// frame therefore costs one cadence of retention rather than correctness.
    primary_floor: Option<WireLsn>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct ReconnectRequest {
    chain: Option<WireChainId>,
    session_nonce: Vec<u8>,
    /// See [`AppendBatchRequest::primary_floor`]. Carried on the handshake too
    /// so a chain that reconnects and then sits idle still bounds its mirror.
    primary_floor: Option<WireLsn>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct ProgressReply {
    known: bool,
    watermark: Option<WireLsn>,
    has_batch: bool,
    batch_seq: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
enum RequestFrame {
    Open(ReconnectRequest),
    Append(AppendBatchRequest),
}

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct StreamRequest {
    frame: Option<RequestFrame>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct StreamReply {
    chain: Option<WireChainId>,
    session_nonce: Vec<u8>,
    opened: bool,
    progress: Option<ProgressReply>,
}

macro_rules! postcard_message {
    ($ty:ty) => {
        impl Message for $ty {
            fn encode_raw(&self, buf: &mut impl BufMut) {
                let bytes = postcard::to_stdvec(self).expect("wire message is serializable");
                pb::bytes::encode(1, &bytes, buf);
            }

            fn merge_field(
                &mut self,
                tag: u32,
                wire_type: WireType,
                buf: &mut impl Buf,
                ctx: DecodeContext,
            ) -> Result<(), DecodeError> {
                if tag != 1 {
                    return pb::skip_field(wire_type, tag, buf, ctx);
                }
                let mut bytes = Vec::new();
                pb::bytes::merge(wire_type, &mut bytes, buf, ctx)?;
                #[allow(deprecated)]
                let decoded = postcard::from_bytes(&bytes)
                    .map_err(|error| DecodeError::new(error.to_string()))?;
                *self = decoded;
                Ok(())
            }

            fn encoded_len(&self) -> usize {
                let bytes = postcard::to_stdvec(self).expect("wire message is serializable");
                pb::bytes::encoded_len(1, &bytes)
            }

            fn clear(&mut self) {
                *self = Self::default();
            }
        }
    };
}

postcard_message!(AppendBatchRequest);
postcard_message!(ReconnectRequest);
postcard_message!(ProgressReply);
postcard_message!(StreamRequest);
postcard_message!(StreamReply);

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct RecordProvenance {
    pub(crate) batch_seq: u64,
    pub(crate) ordinal: u32,
    pub(crate) batch_len: u32,
    pub(crate) predecessor: Option<Lsn>,
    pub(crate) first_lsn: Lsn,
    pub(crate) last_lsn: Lsn,
    pub(crate) next_lsn: Lsn,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct AdoptedRecord {
    pub(crate) origin: Lsn,
    pub(crate) local: Lsn,
}

/// Fsynced acceptance decision for a complete source-chain history.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct AdoptionMarker {
    pub(crate) source_key: Vec<u8>,
    pub(crate) watermark: Option<Lsn>,
    pub(crate) records: Vec<AdoptedRecord>,
}

/// The follower's durable dedupe cursor for one chain, persisted as keyed
/// journal metadata after every batch it advances (docs/13 §4.1, D23).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct Cursor {
    pub(crate) batch_seq: Option<u64>,
    pub(crate) watermark: Option<Lsn>,
    pub(crate) next_lsn: Option<Lsn>,
}

pub(crate) fn chain_key_for_adoption(chain: &DurableChainId) -> Result<Vec<u8>, JournalError> {
    postcard::to_stdvec(chain)
        .map_err(|e| JournalError::Store(format!("encode durable chain identity: {e}")))
}

fn chain_key(chain: &DurableChainId) -> Result<Vec<u8>, JournalError> {
    chain_key_for_adoption(chain)
}

/// The identity a chain shares with every other epoch of the same ownership:
/// primary, follower and shard set, without the epoch.
///
/// postcard writes a struct as its fields back to back and the epoch is the
/// last field of [`DurableChainId`], so this is exactly the leading bytes of
/// [`chain_key`] for any epoch — which is what makes a sibling epoch findable
/// by a key-range seek instead of a scan.
#[derive(serde::Serialize)]
struct ChainFamily<'a> {
    primary_node: &'a NodeId,
    follower_node: &'a NodeId,
    shard_set: &'a [u8],
}

fn chain_family_key(chain: &DurableChainId) -> Result<Vec<u8>, JournalError> {
    postcard::to_stdvec(&ChainFamily {
        primary_node: &chain.primary_node,
        follower_node: &chain.follower_node,
        shard_set: &chain.shard_set,
    })
    .map_err(|e| JournalError::Store(format!("encode chain family identity: {e}")))
}

fn wire_chain(chain: &DurableChainId) -> WireChainId {
    WireChainId {
        primary_node: chain.primary_node.as_bytes().to_vec(),
        follower_node: chain.follower_node.as_bytes().to_vec(),
        shard_set: chain.shard_set.clone(),
        epoch: chain.epoch,
    }
}

fn decode_node(bytes: &[u8]) -> Result<NodeId, Status> {
    let bytes: [u8; 32] = bytes
        .try_into()
        .map_err(|_| Status::invalid_argument("node id must be 32 bytes"))?;
    NodeId::from_bytes(&bytes).map_err(|_| Status::invalid_argument("invalid node id"))
}

fn decode_chain(wire: Option<WireChainId>) -> Result<DurableChainId, Status> {
    let wire = wire.ok_or_else(|| Status::invalid_argument("missing durable chain identity"))?;
    if wire.shard_set.is_empty() {
        return Err(Status::invalid_argument("empty shard-set identity"));
    }
    Ok(DurableChainId {
        primary_node: decode_node(&wire.primary_node)?,
        follower_node: decode_node(&wire.follower_node)?,
        shard_set: wire.shard_set,
        epoch: wire.epoch,
    })
}

fn lsn(wire: WireLsn) -> Lsn {
    Lsn::new(wire.segment, wire.offset)
}

fn progress(cursor: Cursor) -> ProgressReply {
    ProgressReply {
        known: cursor.watermark.is_some(),
        watermark: cursor.watermark.map(|value| WireLsn {
            segment: value.segment,
            offset: value.offset,
        }),
        has_batch: cursor.batch_seq.is_some(),
        batch_seq: cursor.batch_seq.unwrap_or_default(),
    }
}

fn decode_progress(reply: ProgressReply) -> (Option<u64>, Option<Lsn>) {
    (
        reply.has_batch.then_some(reply.batch_seq),
        reply.watermark.filter(|_| reply.known).map(lsn),
    )
}

/// The durable cursor a rebuild starts from, or `None` to start from batch
/// zero (D23).
///
/// **`None` unless retention has actually removed something.** The provenance
/// index is written atomically with the records themselves, which makes it the
/// stronger of the two sources, and while the journal is at floor `0:0` it
/// still holds every batch — so the persisted cursor is not consulted at all
/// and this path behaves exactly as it did before retention reached followers.
/// Above a floor the index no longer begins at batch zero, and the persisted
/// cursor is what accounts for the prefix that is gone. `release_before`
/// refuses to release a mirror that has no persisted cursor
/// ([`ReleaseBlocked::MirrorCursorAbsent`]), so "released but unseedable" is a
/// state this journal cannot reach.
///
/// [`ReleaseBlocked::MirrorCursorAbsent`]: crate::journal::ReleaseBlocked::MirrorCursorAbsent
pub(crate) fn seed_cursor(journal: &Journal, key: &[u8]) -> Result<Option<Cursor>, JournalError> {
    if journal.released_floor() == Lsn::new(0, 0) {
        return Ok(None);
    }
    let Some(bytes) = journal.chain_grpc_state(key)? else {
        return Ok(None);
    };
    postcard::from_bytes(&bytes)
        .map(Some)
        .map_err(|e| JournalError::Store(format!("decode persisted chain cursor: {e}")))
}

fn rebuild_cursor(journal: &Journal, key: &[u8]) -> Result<Cursor, JournalError> {
    let mut batches: BTreeMap<u64, Vec<(Lsn, RecordProvenance)>> = BTreeMap::new();
    for (origin, bytes) in journal.chain_grpc_records(key)? {
        let provenance: RecordProvenance = postcard::from_bytes(&bytes)
            .map_err(|e| JournalError::Store(format!("decode chain provenance: {e}")))?;
        batches
            .entry(provenance.batch_seq)
            .or_default()
            .push((origin, provenance));
    }

    let seed = seed_cursor(journal, key)?;
    let seeded_through = seed.and_then(|cursor| cursor.batch_seq);
    let mut cursor = seed.unwrap_or_default();
    for (seq, mut records) in batches {
        // Only the *retained suffix* is validated. A batch at or below the
        // seed is a batch the persisted cursor already accounts for, and
        // retention may have cut it in half — the floor is a checkpoint
        // watermark, not a batch boundary — so an incomplete one here is
        // expected rather than a gap. What is still checked is the one thing
        // that would mean the cursor and the index disagree: a batch the seed
        // *ends at* that survived whole must end where the seed says it does.
        if seeded_through.is_some_and(|through| seq <= through) {
            records.sort_by_key(|(_, provenance)| provenance.ordinal);
            if let Some((_, first)) = records.first() {
                if seeded_through == Some(seq)
                    && usize::try_from(first.batch_len).ok() == Some(records.len())
                    && (cursor.watermark != Some(first.last_lsn)
                        || cursor.next_lsn != Some(first.next_lsn))
                {
                    return Err(JournalError::Store(
                        "persisted chain cursor disagrees with the retained provenance index"
                            .into(),
                    ));
                }
            }
            continue;
        }
        let expected_seq = cursor.batch_seq.map_or(0, |value| value + 1);
        if seq != expected_seq {
            break;
        }
        records.sort_by_key(|(_, provenance)| provenance.ordinal);
        let Some((_, first)) = records.first() else {
            break;
        };
        let same_batch = |provenance: &RecordProvenance| {
            provenance.batch_seq == first.batch_seq
                && provenance.batch_len == first.batch_len
                && provenance.predecessor == first.predecessor
                && provenance.first_lsn == first.first_lsn
                && provenance.last_lsn == first.last_lsn
                && provenance.next_lsn == first.next_lsn
        };
        if usize::try_from(first.batch_len).ok() != Some(records.len())
            || first.predecessor != cursor.watermark
            || records
                .iter()
                .enumerate()
                .any(|(ordinal, (origin, provenance))| {
                    provenance.batch_seq != seq
                        || usize::try_from(provenance.ordinal).ok() != Some(ordinal)
                        || !same_batch(provenance)
                        || (ordinal == 0 && *origin != first.first_lsn)
                        || (ordinal + 1 == records.len() && *origin != first.last_lsn)
                })
        {
            break;
        }
        cursor = Cursor {
            batch_seq: Some(seq),
            watermark: Some(first.last_lsn),
            next_lsn: Some(first.next_lsn),
        };
    }
    Ok(cursor)
}

/// Refuse to open a chain whose journal already mirrors a sibling epoch.
///
/// docs/13 §3.1 requires `DurableChainId` to change when the ownership epoch
/// changes, and `fence::activate_shards` bumps that epoch on every activation
/// — an ordinary clean restart of the same owner included. The follower's
/// dedupe index is keyed by `(chain_key, origin_lsn)`, so a bumped epoch is a
/// *fresh* namespace: the cursor rebuilds as `None` and the primary re-streams
/// its whole journal into a second physical copy of every record, which then
/// makes promotion impossible — `adopt_chain_history` nulls any origin LSN
/// with two local rows and refuses the ambiguous identity.
///
/// Re-keying the index to drop the epoch would silently let a superseded
/// primary resume onto a live follower session, which is the thing §3.1 exists
/// to prevent. So the epoch stays in the key and the fork is *detected*: the
/// only sound resolution is the intentional restart handshake that this design
/// round does not have, and a follower started without a fence store cannot
/// verify an epoch claim on its own.
fn refuse_sibling_epoch(
    journal: &Journal,
    chain: &DurableChainId,
    key: &[u8],
) -> Result<(), JournalError> {
    let family = chain_family_key(chain)?;
    // Two durable traces, because one of them is not always left. The record
    // index carries a row per *mirrored record*, so a follower that opened
    // this directory at an earlier epoch and received nothing is invisible
    // there — and that is not a hypothetical: `scripts/p2-kill9-gate.sh` ran
    // its load against a gateway that refused every unleased diff, mirrored
    // zero records, and then walked through this refusal at the bumped epoch.
    // The chain-state row is written by every `FollowerReplica::load`, empty
    // cursor included, so it is the trace that answers "was this directory
    // ever opened under a different epoch of this chain".
    let sibling = match journal.chain_epoch_sibling(&family, key)? {
        Some(sibling) => Some(sibling),
        None => journal.chain_state_epoch_sibling(&family, key)?,
    };
    let Some(sibling) = sibling else {
        return Ok(());
    };
    let epoch = postcard::from_bytes::<DurableChainId>(&sibling)
        .map(|sibling| sibling.epoch.to_string())
        .unwrap_or_else(|_| "unknown".into());
    Err(JournalError::Store(format!(
        "follower journal already mirrors this shard set at chain epoch {epoch}; \
         opening it at epoch {} would fork the mirrored namespace and re-stream \
         every record. An epoch change needs an intentional restart handshake, \
         which this follower cannot perform.",
        chain.epoch
    )))
}

struct FollowerState {
    session_nonce: Option<[u8; 16]>,
    cursor: Cursor,
}

struct FollowerReplica {
    chain: DurableChainId,
    key: Vec<u8>,
    journal: Arc<Journal>,
    state: Mutex<FollowerState>,
    sessions_opened: AtomicU64,
    duplicate_batches: AtomicU64,
    fail_next_ack: AtomicBool,
}

impl FollowerReplica {
    fn load(chain: DurableChainId, journal: Arc<Journal>) -> Result<Self, JournalError> {
        let key = chain_key(&chain)?;
        refuse_sibling_epoch(&journal, &chain, &key)?;
        let cursor = rebuild_cursor(&journal, &key)?;
        let persisted = journal.chain_grpc_state(&key)?;
        let encoded = postcard::to_stdvec(&cursor)
            .map_err(|e| JournalError::Store(format!("encode chain cursor: {e}")))?;
        if persisted.as_deref() != Some(encoded.as_slice()) {
            tracing::debug!("repairing chain cursor from durable provenance");
        }
        journal.set_chain_grpc_state(&key, &encoded)?;
        Ok(Self {
            chain,
            key,
            journal,
            state: Mutex::new(FollowerState {
                session_nonce: None,
                cursor,
            }),
            sessions_opened: AtomicU64::new(0),
            duplicate_batches: AtomicU64::new(0),
            fail_next_ack: AtomicBool::new(false),
        })
    }

    fn validate_chain(&self, wire: Option<WireChainId>) -> Result<(), Status> {
        if decode_chain(wire)? != self.chain {
            return Err(Status::failed_precondition(
                "durable chain identity mismatch",
            ));
        }
        Ok(())
    }

    /// Record what the primary has released, which is what bounds this
    /// mirror's own retention (D23).
    ///
    /// Advisory in both directions: an absent floor leaves the mirror pinned
    /// where it was, and a floor that arrives on a frame the follower then
    /// rejects is still true — the primary released those records either way.
    fn note_primary_floor(&self, floor: Option<WireLsn>) {
        if let Some(floor) = floor {
            self.journal.note_primary_floor(&self.key, lsn(floor));
        }
    }

    fn nonce(bytes: &[u8]) -> Result<[u8; 16], Status> {
        bytes
            .try_into()
            .map_err(|_| Status::invalid_argument("session nonce must be 16 bytes"))
    }

    async fn reconnect(&self, request: ReconnectRequest) -> Result<ProgressReply, Status> {
        self.validate_chain(request.chain)?;
        let nonce = Self::nonce(&request.session_nonce)?;
        self.note_primary_floor(request.primary_floor);
        let mut state = self.state.lock().await;
        // Always reconstruct from chain-scoped provenance, never from the
        // in-memory cursor this replica happens to be holding. Where retention
        // has removed a prefix, the reconstruction is *seeded* by the durable
        // cursor row and validated against the retained suffix (D23) — which
        // is still a reconstruction from what is on disk, and still not the
        // cached value.
        state.cursor = rebuild_cursor(&self.journal, &self.key)
            .map_err(|e| Status::internal(format!("rebuild chain cursor: {e}")))?;
        state.session_nonce = Some(nonce);
        self.sessions_opened.fetch_add(1, Ordering::Relaxed);
        let encoded = postcard::to_stdvec(&state.cursor)
            .map_err(|e| Status::internal(format!("encode chain cursor: {e}")))?;
        self.journal
            .set_chain_grpc_state(&self.key, &encoded)
            .map_err(|e| Status::internal(format!("persist chain cursor: {e}")))?;
        Ok(progress(state.cursor))
    }

    async fn close_session(&self, nonce: [u8; 16]) {
        let mut state = self.state.lock().await;
        if state.session_nonce == Some(nonce) {
            state.session_nonce = None;
        }
    }

    async fn append(&self, request: AppendBatchRequest) -> Result<ProgressReply, Status> {
        self.validate_chain(request.chain)?;
        let nonce = Self::nonce(&request.session_nonce)?;
        self.note_primary_floor(request.primary_floor);
        let first_lsn = request
            .first_lsn
            .map(lsn)
            .ok_or_else(|| Status::invalid_argument("missing first LSN"))?;
        let last_lsn = request
            .last_lsn
            .map(lsn)
            .ok_or_else(|| Status::invalid_argument("missing last LSN"))?;
        if request.records.is_empty() {
            return Err(Status::invalid_argument("empty append batch"));
        }
        let records: Vec<JournalRecord> = request
            .records
            .iter()
            .map(|bytes| {
                postcard::from_bytes(bytes)
                    .map_err(|e| Status::invalid_argument(format!("decode journal record: {e}")))
            })
            .collect::<Result<_, _>>()?;
        if records.first().map(|record| record.lsn) != Some(first_lsn)
            || records.last().map(|record| record.lsn) != Some(last_lsn)
            || records
                .windows(2)
                .any(|pair| self.journal.chain_grpc_successor(&pair[0]) != pair[1].lsn)
        {
            return Err(Status::invalid_argument("invalid primary LSN span"));
        }

        let predecessor = request.predecessor.map(lsn);
        if predecessor.is_some_and(|previous| first_lsn <= previous) {
            return Err(Status::invalid_argument("primary LSN span regresses"));
        }
        let mut state = self.state.lock().await;
        if state.session_nonce != Some(nonce) {
            return Err(Status::failed_precondition(
                "stale or unopened chain session",
            ));
        }
        let expected_seq = state.cursor.batch_seq.map_or(0, |value| value + 1);
        let provenance = RecordProvenance {
            batch_seq: request.batch_seq,
            ordinal: 0,
            batch_len: u32::try_from(records.len())
                .map_err(|_| Status::invalid_argument("batch too large"))?,
            predecessor,
            first_lsn,
            last_lsn,
            next_lsn: self
                .journal
                .chain_grpc_successor(records.last().expect("checked non-empty")),
        };

        if request.batch_seq < expected_seq {
            for (ordinal, record) in records.iter().enumerate() {
                let mut expected = provenance.clone();
                expected.ordinal = ordinal as u32;
                let stored = self
                    .journal
                    .chain_grpc_record(&self.key, record.lsn)
                    .map_err(|e| Status::internal(format!("read dedupe index: {e}")))?
                    .ok_or_else(|| {
                        Status::failed_precondition("retry does not match durable batch")
                    })?;
                let stored: RecordProvenance = postcard::from_bytes(&stored)
                    .map_err(|e| Status::internal(format!("decode dedupe index: {e}")))?;
                if stored != expected {
                    return Err(Status::failed_precondition(
                        "retry does not match durable batch",
                    ));
                }
            }
            // Every row of this batch was already durable and matched its
            // provenance, so the append is a no-op replay — docs/13 §6's
            // duplicate batch count. It is expected during reconnect and
            // alarming above reconnect noise, which is only distinguishable
            // if the two are counted separately.
            self.duplicate_batches.fetch_add(1, Ordering::Relaxed);
            return Ok(progress(state.cursor));
        }
        if request.batch_seq != expected_seq
            || predecessor != state.cursor.watermark
            || state.cursor.next_lsn.is_some_and(|next| first_lsn != next)
        {
            return Err(Status::failed_precondition(
                "gap or out-of-order append batch",
            ));
        }

        // Stage every row before awaiting any commit.  The journal committer
        // can therefore group the complete wire batch into one fsync window;
        // importantly, the cursor remains unpublished until *all* staged rows
        // are durable.  If a process dies between the two, rebuild_cursor sees
        // the indexed prefix and the primary retries the same idempotent batch.
        let mut commits = Vec::with_capacity(records.len());
        for (ordinal, record) in records.into_iter().enumerate() {
            let mut item = provenance.clone();
            item.ordinal = ordinal as u32;
            let encoded = postcard::to_stdvec(&item)
                .map_err(|e| Status::internal(format!("encode provenance: {e}")))?;
            if let Some(handle) = self
                .journal
                .append_replicated_indexed(record, &self.key, &encoded)
                .map_err(|e| Status::internal(format!("append mirrored record: {e}")))?
            {
                commits.push(handle);
            }
        }
        for handle in commits {
            handle
                .committed()
                .await
                .map_err(|e| Status::internal(format!("commit mirrored record: {e}")))?;
        }

        state.cursor = Cursor {
            batch_seq: Some(request.batch_seq),
            watermark: Some(last_lsn),
            next_lsn: Some(provenance.next_lsn),
        };
        let encoded = postcard::to_stdvec(&state.cursor)
            .map_err(|e| Status::internal(format!("encode cursor: {e}")))?;
        self.journal
            .set_chain_grpc_state(&self.key, &encoded)
            .map_err(|e| Status::internal(format!("persist cursor: {e}")))?;
        Ok(progress(state.cursor))
    }
}

type ReplyStream = Pin<Box<dyn Stream<Item = Result<StreamReply, Status>> + Send + 'static>>;

trait ChainReplication: Send + Sync + 'static {
    fn replicate(
        self: Arc<Self>,
        request: Request<tonic::Streaming<StreamRequest>>,
    ) -> Result<Response<ReplyStream>, Status>;
}

impl ChainReplication for FollowerReplica {
    fn replicate(
        self: Arc<Self>,
        request: Request<tonic::Streaming<StreamRequest>>,
    ) -> Result<Response<ReplyStream>, Status> {
        struct SessionStream {
            incoming: tonic::Streaming<StreamRequest>,
            replica: Arc<FollowerReplica>,
            nonce: Option<[u8; 16]>,
            terminal: bool,
        }

        let stream = futures::stream::unfold(
            SessionStream {
                incoming: request.into_inner(),
                replica: self,
                nonce: None,
                terminal: false,
            },
            |mut session| async move {
                if session.terminal {
                    return None;
                }
                let next = session.incoming.message().await;
                let frame = match next {
                    Ok(Some(request)) => request
                        .frame
                        .ok_or_else(|| Status::invalid_argument("missing stream frame")),
                    Ok(None) => {
                        if let Some(nonce) = session.nonce {
                            session.replica.close_session(nonce).await;
                        }
                        return None;
                    }
                    Err(error) => {
                        if let Some(nonce) = session.nonce.take() {
                            session.replica.close_session(nonce).await;
                        }
                        session.terminal = true;
                        return Some((Err(error), session));
                    }
                };

                let reply = match frame {
                    Ok(RequestFrame::Open(open)) if session.nonce.is_none() => {
                        let nonce = FollowerReplica::nonce(&open.session_nonce);
                        match nonce {
                            Ok(nonce) => match session.replica.reconnect(open).await {
                                Ok(progress) => {
                                    session.nonce = Some(nonce);
                                    Ok(StreamReply {
                                        chain: Some(wire_chain(&session.replica.chain)),
                                        session_nonce: nonce.to_vec(),
                                        opened: true,
                                        progress: Some(progress),
                                    })
                                }
                                Err(error) => Err(error),
                            },
                            Err(error) => Err(error),
                        }
                    }
                    Ok(RequestFrame::Open(_)) => {
                        Err(Status::failed_precondition("session is already open"))
                    }
                    Ok(RequestFrame::Append(append)) => {
                        let Some(nonce) = session.nonce else {
                            return Some((
                                Err(Status::failed_precondition(
                                    "first stream frame must open the session",
                                )),
                                session,
                            ));
                        };
                        match session.replica.append(append).await {
                            Ok(progress) => {
                                if session.replica.fail_next_ack.swap(false, Ordering::SeqCst) {
                                    Err(Status::unavailable(
                                        "injected stream loss after durable append",
                                    ))
                                } else {
                                    Ok(StreamReply {
                                        chain: Some(wire_chain(&session.replica.chain)),
                                        session_nonce: nonce.to_vec(),
                                        opened: false,
                                        progress: Some(progress),
                                    })
                                }
                            }
                            Err(error) => Err(error),
                        }
                    }
                    Err(error) => Err(error),
                };
                if reply.is_err() {
                    if let Some(nonce) = session.nonce.take() {
                        session.replica.close_session(nonce).await;
                    }
                    session.terminal = true;
                }
                Some((reply, session))
            },
        );
        Ok(Response::new(Box::pin(stream)))
    }
}

struct ChainService<T>(Arc<T>);

impl<T> Clone for ChainService<T> {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}

impl<T> tonic::server::NamedService for ChainService<T> {
    const NAME: &'static str = SERVICE_NAME;
}

impl<T, B> Service<http::Request<B>> for ChainService<T>
where
    T: ChainReplication,
    B: Body + Send + 'static,
    B::Error: Into<StdError> + Send + 'static,
{
    type Response = http::Response<tonic::body::Body>;
    type Error = std::convert::Infallible;
    type Future = BoxFuture<Self::Response, Self::Error>;

    fn poll_ready(
        &mut self,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        std::task::Poll::Ready(Ok(()))
    }

    fn call(&mut self, request: http::Request<B>) -> Self::Future {
        let inner = Arc::clone(&self.0);
        match request.uri().path() {
            STREAM_PATH => {
                struct Method<T>(Arc<T>);
                impl<T: ChainReplication> tonic::server::StreamingService<StreamRequest> for Method<T> {
                    type Response = StreamReply;
                    type ResponseStream = ReplyStream;
                    type Future = BoxFuture<Response<Self::ResponseStream>, Status>;
                    fn call(
                        &mut self,
                        request: Request<tonic::Streaming<StreamRequest>>,
                    ) -> Self::Future {
                        let inner = Arc::clone(&self.0);
                        Box::pin(async move { inner.replicate(request) })
                    }
                }
                Box::pin(async move {
                    let mut grpc = tonic::server::Grpc::new(ProstCodec::default());
                    Ok(grpc.streaming(Method(inner), request).await)
                })
            }
            _ => Box::pin(async move {
                Ok(http::Response::builder()
                    .status(200)
                    .header("grpc-status", "12")
                    .header(http::header::CONTENT_TYPE, "application/grpc")
                    .body(tonic::body::Body::empty())
                    .expect("valid gRPC not-found response"))
            }),
        }
    }
}

impl<T, B> hyper::service::Service<http::Request<B>> for ChainService<T>
where
    T: ChainReplication,
    B: Body + Send + 'static,
    B::Error: Into<StdError> + Send + 'static,
{
    type Response = http::Response<tonic::body::Body>;
    type Error = std::convert::Infallible;
    type Future = BoxFuture<Self::Response, Self::Error>;

    fn call(&self, request: http::Request<B>) -> Self::Future {
        let mut service = self.clone();
        Service::call(&mut service, request)
    }
}

/// Running follower gRPC endpoint.
pub struct ChainGrpcServer {
    addr: std::net::SocketAddr,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    close_connections: broadcast::Sender<Arc<tokio::sync::Barrier>>,
    replica: Arc<FollowerReplica>,
    join: tokio::task::JoinHandle<()>,
}

impl std::fmt::Debug for ChainGrpcServer {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ChainGrpcServer")
            .field("addr", &self.addr)
            .field("sessions_opened", &self.sessions_opened())
            .field("duplicate_batches", &self.duplicate_batches())
            .finish_non_exhaustive()
    }
}

impl ChainGrpcServer {
    /// Bound local address.
    #[must_use]
    pub fn addr(&self) -> std::net::SocketAddr {
        self.addr
    }

    /// Number of successfully opened stream sessions.
    ///
    /// This diagnostic is useful for asserting that batches reuse one live
    /// stream and that failure recovery establishes exactly one replacement.
    ///
    /// It does **not** measure topology flap. A primary's watermark probe is
    /// [`GrpcChainTransport::follower_watermark`], which delegates to
    /// `reconnect`, and every reconnect opens a session — so a chain that is
    /// merely retrying a failed push drives this counter at the retry rate
    /// (~100/s), with no stream having been lost at all.
    #[must_use]
    pub fn sessions_opened(&self) -> u64 {
        self.replica.sessions_opened.load(Ordering::Relaxed)
    }

    /// Batches the follower deduped against its durable provenance index
    /// instead of storing again — docs/13 §6's duplicate batch count.
    ///
    /// Counted only on the fully-matching idempotent replay: a retry whose
    /// provenance disagrees is a failed precondition, not a duplicate.
    #[must_use]
    pub fn duplicate_batches(&self) -> u64 {
        self.replica.duplicate_batches.load(Ordering::Relaxed)
    }

    /// Deterministically close all current HTTP/2 connections while continuing
    /// to accept reconnects. Every stream on those connections becomes unusable.
    pub async fn close_connections(&self) {
        let barrier = Arc::new(tokio::sync::Barrier::new(
            self.close_connections.receiver_count() + 1,
        ));
        if self.close_connections.send(Arc::clone(&barrier)).is_ok() {
            barrier.wait().await;
        }
    }

    /// Test hook: fail the next response after its batch is durably appended.
    ///
    /// The resulting replay exercises the ambiguous durable-before-ACK case.
    #[doc(hidden)]
    pub fn fail_next_ack(&self) {
        self.replica.fail_next_ack.store(true, Ordering::SeqCst);
    }

    /// Stop accepting connections, cancel all live streams, and await every
    /// connection task so shutdown leaves no detached senders or servers.
    pub async fn shutdown(mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        let _ = self.join.await;
    }
}

/// Bind a follower endpoint for exactly one durable chain.
pub async fn spawn_chain_grpc(
    bind: std::net::SocketAddr,
    journal: Arc<Journal>,
    chain: DurableChainId,
) -> Result<ChainGrpcServer, std::io::Error> {
    let listener = tokio::net::TcpListener::bind(bind).await?;
    let addr = listener.local_addr()?;
    let replica =
        FollowerReplica::load(chain, journal).map_err(|e| std::io::Error::other(e.to_string()))?;
    let replica = Arc::new(replica);
    let service = ChainService(Arc::clone(&replica));
    let (shutdown, mut stopped) = tokio::sync::oneshot::channel();
    let (close_connections, _) = broadcast::channel::<Arc<tokio::sync::Barrier>>(1);
    let close_for_server = close_connections.clone();
    let join = tokio::spawn(async move {
        let builder = auto::Builder::new(TokioExecutor::new()).http2_only();
        let mut connections = tokio::task::JoinSet::new();
        loop {
            tokio::select! {
                _ = &mut stopped => break,
                accepted = listener.accept() => {
                    let Ok((stream, _)) = accepted else { break };
                    let service = service.clone();
                    let builder = builder.clone();
                    let mut close = close_for_server.subscribe();
                    connections.spawn(async move {
                        tokio::select! {
                            result = builder.serve_connection(TokioIo::new(stream), service) => {
                                let _ = result;
                            }
                            result = close.recv() => {
                                if let Ok(barrier) = result {
                                    barrier.wait().await;
                                }
                            }
                        }
                    });
                }
                Some(_) = connections.join_next(), if !connections.is_empty() => {}
            }
        }
        connections.abort_all();
        while connections.join_next().await.is_some() {}
    });
    Ok(ChainGrpcServer {
        addr,
        shutdown: Some(shutdown),
        close_connections,
        replica,
        join,
    })
}

/// Primary-side tonic transport. Operations are serialized so reconnect and
/// append cannot race or regress the acknowledged cursor.
#[derive(Debug)]
pub struct GrpcChainTransport {
    addr: std::net::SocketAddr,
    chain: DurableChainId,
    inner: Mutex<ClientState>,
    /// The primary's own retention floor, as last reported by the replicator
    /// (D23). Kept outside `inner` so noting it never waits on a live stream.
    primary_floor: std::sync::Mutex<Option<Lsn>>,
}

#[derive(Debug)]
struct ClientState {
    client: GrpcClient,
    live: Option<LiveStream>,
    next_batch: u64,
    watermark: Option<Lsn>,
}

#[derive(Debug)]
struct LiveStream {
    nonce: [u8; 16],
    requests: futures::channel::mpsc::Sender<StreamRequest>,
    replies: tonic::Streaming<StreamReply>,
}

fn fresh_nonce() -> [u8; 16] {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let mut nonce = [0; 16];
    nonce[..8].copy_from_slice(&NEXT.fetch_add(1, Ordering::Relaxed).to_be_bytes());
    nonce[8..].copy_from_slice(
        &std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
            .to_be_bytes()[8..],
    );
    nonce
}

fn build_client() -> GrpcClient {
    Client::builder(TokioExecutor::new())
        .http2_only(true)
        .build(HttpConnector::new())
}

impl GrpcChainTransport {
    /// Construct a transport without performing network I/O.
    ///
    /// The first append or explicit [`Self::reconnect`] performs the recovery
    /// probe. This lets a primary start and keep committing locally while its
    /// designated follower is unavailable; [`crate::journal::spawn_chain`]
    /// retries from the follower's durable watermark in the background.
    #[must_use]
    pub fn new(addr: std::net::SocketAddr, chain: DurableChainId) -> Self {
        Self {
            addr,
            chain,
            inner: Mutex::new(ClientState {
                client: build_client(),
                live: None,
                next_batch: 0,
                watermark: None,
            }),
            primary_floor: std::sync::Mutex::new(None),
        }
    }

    fn wire_primary_floor(&self) -> Option<WireLsn> {
        self.primary_floor
            .lock()
            .expect("primary floor lock")
            .map(|floor| WireLsn {
                segment: floor.segment,
                offset: floor.offset,
            })
    }

    /// Connect and perform a fresh remote recovery probe.
    pub async fn connect(
        addr: std::net::SocketAddr,
        chain: DurableChainId,
    ) -> Result<Self, JournalError> {
        let transport = Self::new(addr, chain);
        transport.reconnect().await?;
        Ok(transport)
    }

    /// Drop the current stream, start a fresh session, and return the follower's
    /// remotely reconstructed progress. Cached client progress is never used as
    /// a successful recovery probe.
    pub async fn reconnect(&self) -> Result<Option<Lsn>, JournalError> {
        let mut state = self.inner.lock().await;
        self.probe_locked(&mut state).await
    }

    async fn probe_locked(&self, state: &mut ClientState) -> Result<Option<Lsn>, JournalError> {
        state.live = None;
        let nonce = fresh_nonce();
        let (mut requests, incoming) = futures::channel::mpsc::channel(1);
        requests
            .try_send(StreamRequest {
                frame: Some(RequestFrame::Open(ReconnectRequest {
                    chain: Some(wire_chain(&self.chain)),
                    session_nonce: nonce.to_vec(),
                    primary_floor: self.wire_primary_floor(),
                })),
            })
            .map_err(|e| JournalError::Store(format!("queue gRPC open handshake: {e}")))?;

        let origin = format!("http://{}", self.addr)
            .parse::<http::Uri>()
            .map_err(|e| JournalError::Store(format!("gRPC origin: {e}")))?;
        let mut grpc = tonic::client::Grpc::with_origin(state.client.clone(), origin);
        grpc.ready()
            .await
            .map_err(|e| JournalError::Store(format!("gRPC transport: {e}")))?;
        let replies = grpc
            .streaming(
                Request::new(incoming),
                http::uri::PathAndQuery::from_static(STREAM_PATH),
                ProstCodec::default(),
            )
            .await
            .map(Response::into_inner)
            .map_err(|e| JournalError::Store(format!("gRPC open stream: {e}")))?;
        let mut live = LiveStream {
            nonce,
            requests,
            replies,
        };
        let reply = live
            .replies
            .message()
            .await
            .map_err(|e| JournalError::Store(format!("gRPC recovery probe: {e}")))?
            .ok_or_else(|| {
                JournalError::Store("gRPC stream closed during recovery probe".into())
            })?;
        let (batch, watermark) = decode_progress(self.validate_reply(&live, reply, true)?);
        if state
            .watermark
            .is_some_and(|previous| watermark.is_none_or(|remote| remote < previous))
        {
            return Err(JournalError::Store(
                "follower watermark regressed during reconnect".into(),
            ));
        }
        state.next_batch = batch.map_or(0, |value| value + 1);
        state.watermark = watermark;
        state.live = Some(live);
        Ok(watermark)
    }

    fn validate_reply(
        &self,
        live: &LiveStream,
        reply: StreamReply,
        opened: bool,
    ) -> Result<ProgressReply, JournalError> {
        let chain = decode_chain(reply.chain)
            .map_err(|e| JournalError::Store(format!("gRPC response identity: {e}")))?;
        if chain != self.chain
            || reply.session_nonce.as_slice() != live.nonce
            || reply.opened != opened
        {
            return Err(JournalError::Store(
                "stale or mismatched gRPC stream response".into(),
            ));
        }
        reply
            .progress
            .ok_or_else(|| JournalError::Store("gRPC response omitted progress".into()))
    }

    async fn send_batch_locked(
        &self,
        state: &mut ClientState,
        request: AppendBatchRequest,
    ) -> Result<ProgressReply, JournalError> {
        let live = state
            .live
            .as_mut()
            .ok_or_else(|| JournalError::Store("gRPC chain stream is not open".into()))?;
        live.requests
            .send(StreamRequest {
                frame: Some(RequestFrame::Append(request)),
            })
            .await
            .map_err(|e| JournalError::Store(format!("gRPC append send: {e}")))?;
        let reply = live
            .replies
            .message()
            .await
            .map_err(|e| JournalError::Store(format!("gRPC append receive: {e}")))?
            .ok_or_else(|| JournalError::Store("gRPC stream closed before durable ACK".into()))?;
        self.validate_reply(live, reply, false)
    }

    /// Send one ordered batch and await its durable follower acknowledgement.
    pub async fn append_batch(&self, records: Vec<JournalRecord>) -> Result<Lsn, JournalError> {
        let mut state = self.inner.lock().await;
        let first = records
            .first()
            .ok_or_else(|| JournalError::Store("cannot send empty chain batch".into()))?
            .lsn;
        let last = records.last().expect("checked non-empty").lsn;
        let encoded: Vec<Vec<u8>> = records
            .iter()
            .map(|record| {
                postcard::to_stdvec(record)
                    .map_err(|e| JournalError::Store(format!("encode chain record: {e}")))
            })
            .collect::<Result<_, _>>()?;

        // `new` is deliberately lazy. Establishing the first session here also
        // reconstructs batch sequencing and the durable remote watermark before
        // any record is sent.
        if state.live.is_none() {
            self.probe_locked(&mut state).await?;
        }
        if let Some(remote) = state.watermark {
            if last <= remote {
                return Ok(remote);
            }
            if first <= remote {
                return Err(JournalError::Store(
                    "chain batch overlaps follower watermark; rescan from durable progress".into(),
                ));
            }
        }
        let attempted_batch = state.next_batch;
        let make_request = |state: &ClientState| AppendBatchRequest {
            chain: Some(wire_chain(&self.chain)),
            session_nonce: state
                .live
                .as_ref()
                .expect("connected transport has a live stream")
                .nonce
                .to_vec(),
            batch_seq: attempted_batch,
            predecessor: state.watermark.map(|value| WireLsn {
                segment: value.segment,
                offset: value.offset,
            }),
            first_lsn: Some(WireLsn {
                segment: first.segment,
                offset: first.offset,
            }),
            last_lsn: Some(WireLsn {
                segment: last.segment,
                offset: last.offset,
            }),
            records: encoded.clone(),
            primary_floor: self.wire_primary_floor(),
        };
        let request = make_request(&state);
        let first = self.send_batch_locked(&mut state, request).await;
        let reply = match first {
            Ok(reply) => reply,
            Err(error) => {
                state.live = None;
                tracing::debug!(%error, "chain append failed; probing follower before retry");
                self.probe_locked(&mut state).await?;
                if state.next_batch == attempted_batch + 1 && state.watermark == Some(last) {
                    return Ok(last);
                }
                if state.next_batch != attempted_batch {
                    return Err(JournalError::Store(
                        "follower progress changed incompatibly during retry".into(),
                    ));
                }
                let request = make_request(&state);
                match self.send_batch_locked(&mut state, request).await {
                    Ok(reply) => reply,
                    Err(error) => {
                        state.live = None;
                        return Err(JournalError::Store(format!("gRPC append retry: {error}")));
                    }
                }
            }
        };
        let (batch, watermark) = decode_progress(reply);
        if batch != Some(state.next_batch) || watermark != Some(last) {
            state.live = None;
            return Err(JournalError::Store(
                "follower returned inconsistent progress".into(),
            ));
        }
        state.next_batch += 1;
        state.watermark = watermark;
        Ok(last)
    }
}

#[async_trait::async_trait]
impl ChainTransport for GrpcChainTransport {
    async fn append(&self, record: JournalRecord) -> Result<Lsn, JournalError> {
        self.append_batch(vec![record]).await
    }

    fn note_primary_floor(&self, floor: Lsn) {
        let mut current = self.primary_floor.lock().expect("primary floor lock");
        *current = Some(current.map_or(floor, |previous| previous.max(floor)));
    }

    async fn append_batch(&self, records: Vec<JournalRecord>) -> Result<Lsn, JournalError> {
        GrpcChainTransport::append_batch(self, records).await
    }

    async fn follower_watermark(&self) -> Option<Lsn> {
        self.reconnect().await.ok().flatten()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::time::Duration;

    use crate::journal::{AdaptiveCommitMode, GroupCommitConfig, JournalConfig};
    use orrery_protocol::{CellId, Epoch, GridId, PersistId, RecordKind, Tick};

    #[test]
    fn durable_key_changes_for_every_identity_component() {
        fn node(n: u8) -> NodeId {
            let mut seed = [0; 32];
            seed[0] = n;
            iroh_base::SecretKey::from_bytes(&seed).public()
        }
        let base = DurableChainId {
            primary_node: node(1),
            follower_node: node(2),
            shard_set: b"a".to_vec(),
            epoch: 3,
        };
        for changed in [
            DurableChainId {
                primary_node: node(4),
                ..base.clone()
            },
            DurableChainId {
                follower_node: node(4),
                ..base.clone()
            },
            DurableChainId {
                shard_set: b"b".to_vec(),
                ..base.clone()
            },
            DurableChainId {
                epoch: 4,
                ..base.clone()
            },
        ] {
            assert_ne!(chain_key(&base).unwrap(), chain_key(&changed).unwrap());
        }
    }

    #[test]
    fn the_family_key_is_the_chain_key_without_its_epoch() {
        // The sibling-epoch seek depends on this: postcard writes fields back
        // to back and `epoch` is last, so every epoch of one ownership shares
        // this prefix. Widths deliberately span the varint boundary at 128.
        let base = chain(1, 2, 3, 0);
        let family = chain_family_key(&base).unwrap();
        for epoch in [0, 1, 127, 128, 4, u64::MAX] {
            let id = DurableChainId {
                epoch,
                ..base.clone()
            };
            let key = chain_key(&id).unwrap();
            assert!(key.starts_with(&family), "epoch {epoch} left the family");
            assert!(key.len() > family.len());
        }
        assert_ne!(chain_family_key(&chain(1, 2, 4, 0)).unwrap(), family);
    }

    #[tokio::test]
    async fn a_sibling_epoch_refuses_to_open_a_forked_namespace() {
        let dir = tempfile::tempdir().unwrap();
        let journal = Arc::new(Journal::open(&config(dir.path())).unwrap());
        let first = chain(1, 9, 1, 1);
        let replica = FollowerReplica::load(first.clone(), Arc::clone(&journal)).unwrap();
        let nonce = [21; 16];
        replica
            .reconnect(reconnect_request(&first, nonce))
            .await
            .unwrap();
        replica
            .append(batch_request(&first, nonce, 0, None, &[record(10, 1)]))
            .await
            .unwrap();

        // Same ownership, next epoch: a fresh key, an empty cursor and a full
        // re-stream, which is exactly the fork this refuses.
        for epoch in [2, 200] {
            let Err(error) = FollowerReplica::load(chain(1, 9, 1, epoch), Arc::clone(&journal))
            else {
                panic!("a sibling epoch must not open a second namespace");
            };
            assert!(
                error.to_string().contains("restart handshake"),
                "refusal must name the missing handshake: {error}"
            );
        }
        // A different shard set is a different chain, not a forked epoch.
        FollowerReplica::load(chain(1, 9, 2, 7), Arc::clone(&journal)).unwrap();
        journal.close().await.unwrap();
    }

    fn node(n: u8) -> NodeId {
        let mut seed = [0; 32];
        seed[0] = n;
        iroh_base::SecretKey::from_bytes(&seed).public()
    }

    fn chain(primary: u8, follower: u8, shard: u8, epoch: u64) -> DurableChainId {
        DurableChainId {
            primary_node: node(primary),
            follower_node: node(follower),
            shard_set: vec![shard],
            epoch,
        }
    }

    fn config(path: &std::path::Path) -> JournalConfig {
        JournalConfig {
            dir: path.to_path_buf(),
            commit: GroupCommitConfig {
                mode: AdaptiveCommitMode::AlwaysBatch,
                batch_window: Duration::from_millis(1),
                batch_max_records: 128,
                batch_max_bytes: 1 << 20,
            },
        }
    }

    fn record(origin: u64, entity: u64) -> JournalRecord {
        let payload = entity.to_le_bytes();
        JournalRecord {
            lsn: Lsn::new(0, origin),
            cell: CellId::ROOT,
            grid: GridId::ROOT,
            entity: PersistId::new(entity),
            tick: Tick::new(entity),
            epoch: Epoch::new(0),
            author: node(1),
            kind: RecordKind::Spawn,
            payload: bytes::Bytes::copy_from_slice(&payload),
            crc: crate::payload_crc(&payload),
        }
    }

    fn reconnect_request(chain: &DurableChainId, nonce: [u8; 16]) -> ReconnectRequest {
        ReconnectRequest {
            chain: Some(wire_chain(chain)),
            session_nonce: nonce.to_vec(),
            primary_floor: None,
        }
    }

    fn batch_request(
        chain: &DurableChainId,
        nonce: [u8; 16],
        seq: u64,
        predecessor: Option<Lsn>,
        records: &[JournalRecord],
    ) -> AppendBatchRequest {
        AppendBatchRequest {
            chain: Some(wire_chain(chain)),
            session_nonce: nonce.to_vec(),
            batch_seq: seq,
            predecessor: predecessor.map(|value| WireLsn {
                segment: value.segment,
                offset: value.offset,
            }),
            first_lsn: records.first().map(|record| WireLsn {
                segment: record.lsn.segment,
                offset: record.lsn.offset,
            }),
            last_lsn: records.last().map(|record| WireLsn {
                segment: record.lsn.segment,
                offset: record.lsn.offset,
            }),
            records: records
                .iter()
                .map(|record| postcard::to_stdvec(record).unwrap())
                .collect(),
            primary_floor: None,
        }
    }

    // Retention is a `journal-raw` property: the Fjall fallback answers
    // `release_before` with `Unsupported` by design (D19, D20 §9), so these
    // five cases are about the default backend and are compiled for it.
    #[cfg(feature = "journal-raw")]
    /// Mirror three one-record batches and return their records, so a test can
    /// name the origin LSN it wants the primary's floor to sit at.
    async fn mirror_three(
        replica: &FollowerReplica,
        journal: &Journal,
        id: &DurableChainId,
        nonce: [u8; 16],
    ) -> Vec<JournalRecord> {
        replica
            .reconnect(reconnect_request(id, nonce))
            .await
            .unwrap();
        let mut records = Vec::new();
        let mut previous: Option<Lsn> = None;
        let mut origin = 10;
        for seq in 0..3 {
            let record = record(origin, seq + 1);
            origin = journal.chain_grpc_successor(&record).offset;
            replica
                .append(batch_request(
                    id,
                    nonce,
                    seq,
                    previous,
                    std::slice::from_ref(&record),
                ))
                .await
                .unwrap();
            previous = Some(record.lsn);
            records.push(record);
        }
        records
    }

    // Retention is a `journal-raw` property: the Fjall fallback answers
    // `release_before` with `Unsupported` by design (D19, D20 §9), so these
    // five cases are about the default backend and are compiled for it.
    #[cfg(feature = "journal-raw")]
    /// A released mirror rebuilds its cursor from the persisted row plus the
    /// retained suffix, and neither half alone would do it (D23).
    ///
    /// The provenance index no longer starts at batch zero, so an unseeded walk
    /// stops at the first gap and reports an empty cursor — which is the full
    /// re-stream `refuse_sibling_epoch` exists to catch, arrived at from the
    /// other direction.
    #[tokio::test]
    async fn a_released_mirror_seeds_its_cursor_from_the_persisted_row() {
        let dir = tempfile::tempdir().unwrap();
        let journal = Arc::new(Journal::open(&config(dir.path())).unwrap());
        let id = chain(1, 9, 1, 4);
        let key = chain_key(&id).unwrap();
        let replica = FollowerReplica::load(id.clone(), Arc::clone(&journal)).unwrap();
        let records = mirror_three(&replica, &journal, &id, [21; 16]).await;

        journal.note_primary_floor(&key, records[2].lsn);
        let release = journal.release_before(Lsn::new(9_999, 0)).unwrap();
        assert_eq!(release.blocked, None);
        assert_eq!(
            journal.chain_grpc_records(&key).unwrap().len(),
            1,
            "the released rows go with the records they point at"
        );

        journal.close().await.unwrap();
        drop(replica);
        drop(journal);

        let journal = Arc::new(Journal::open(&config(dir.path())).unwrap());
        let replica = FollowerReplica::load(id, Arc::clone(&journal)).unwrap();
        let cursor = replica.state.lock().await.cursor;
        assert_eq!(cursor.batch_seq, Some(2));
        assert_eq!(cursor.watermark, Some(records[2].lsn));
        journal.close().await.unwrap();
    }

    // Retention is a `journal-raw` property: the Fjall fallback answers
    // `release_before` with `Unsupported` by design (D19, D20 §9), so these
    // five cases are about the default backend and are compiled for it.
    #[cfg(feature = "journal-raw")]
    /// The crash boundary the persisted cursor does *not* close: records are
    /// durable before the row that names them is written, so a cursor can be
    /// one batch behind the index it seeds.
    ///
    /// Seeding must therefore be a starting point and not an answer — the walk
    /// continues over every retained batch above the seed. The opposite bug is
    /// the expensive one: a rebuild that trusted the row and stopped would
    /// report a watermark below what is durable, and the primary would resend
    /// records the follower already holds.
    #[tokio::test]
    async fn a_cursor_behind_its_index_still_advances_over_the_retained_suffix() {
        let dir = tempfile::tempdir().unwrap();
        let journal = Arc::new(Journal::open(&config(dir.path())).unwrap());
        let id = chain(1, 9, 1, 4);
        let key = chain_key(&id).unwrap();
        let replica = FollowerReplica::load(id.clone(), Arc::clone(&journal)).unwrap();
        let records = mirror_three(&replica, &journal, &id, [22; 16]).await;

        // The row a crash between the first batch's durability and its cursor
        // write would have left behind.
        let stale = Cursor {
            batch_seq: Some(0),
            watermark: Some(records[0].lsn),
            next_lsn: Some(journal.chain_grpc_successor(&records[0])),
        };
        journal
            .set_chain_grpc_state(&key, &postcard::to_stdvec(&stale).unwrap())
            .unwrap();
        journal.note_primary_floor(&key, records[1].lsn);
        assert_eq!(
            journal.release_before(Lsn::new(9_999, 0)).unwrap().blocked,
            None
        );

        assert_eq!(
            rebuild_cursor(&journal, &key).unwrap().batch_seq,
            Some(2),
            "the retained suffix above the seed is still the durable truth"
        );
        journal.close().await.unwrap();
    }

    // Retention is a `journal-raw` property: the Fjall fallback answers
    // `release_before` with `Unsupported` by design (D19, D20 §9), so these
    // five cases are about the default backend and are compiled for it.
    #[cfg(feature = "journal-raw")]
    /// A persisted cursor that disagrees with a batch the release *kept* is a
    /// corruption, not a starting point, and it fails the open loudly.
    #[tokio::test]
    async fn a_cursor_that_disagrees_with_the_retained_index_refuses_to_open() {
        let dir = tempfile::tempdir().unwrap();
        let journal = Arc::new(Journal::open(&config(dir.path())).unwrap());
        let id = chain(1, 9, 1, 4);
        let key = chain_key(&id).unwrap();
        let replica = FollowerReplica::load(id.clone(), Arc::clone(&journal)).unwrap();
        let records = mirror_three(&replica, &journal, &id, [23; 16]).await;

        let wrong = Cursor {
            batch_seq: Some(1),
            watermark: Some(Lsn::new(7, 7)),
            next_lsn: Some(Lsn::new(7, 8)),
        };
        journal
            .set_chain_grpc_state(&key, &postcard::to_stdvec(&wrong).unwrap())
            .unwrap();
        journal.note_primary_floor(&key, records[1].lsn);
        assert_eq!(
            journal.release_before(Lsn::new(9_999, 0)).unwrap().blocked,
            None
        );

        let error = rebuild_cursor(&journal, &key).unwrap_err().to_string();
        assert!(error.contains("disagrees"), "unexpected error: {error}");
        journal.close().await.unwrap();
    }

    // Retention is a `journal-raw` property: the Fjall fallback answers
    // `release_before` with `Unsupported` by design (D19, D20 §9), so these
    // five cases are about the default backend and are compiled for it.
    #[cfg(feature = "journal-raw")]
    /// Retention does not cost a follower its two refusals: a replayed batch
    /// below the floor is refused rather than mirrored a second time, and a
    /// bumped chain epoch is still detected on the released directory.
    #[tokio::test]
    async fn a_released_mirror_keeps_its_refusals() {
        let dir = tempfile::tempdir().unwrap();
        let journal = Arc::new(Journal::open(&config(dir.path())).unwrap());
        let id = chain(1, 9, 1, 4);
        let key = chain_key(&id).unwrap();
        let replica = FollowerReplica::load(id.clone(), Arc::clone(&journal)).unwrap();
        let nonce = [24; 16];
        let records = mirror_three(&replica, &journal, &id, nonce).await;

        journal.note_primary_floor(&key, records[2].lsn);
        assert_eq!(
            journal.release_before(Lsn::new(9_999, 0)).unwrap().blocked,
            None
        );
        let retained = journal.scan_from(journal.released_floor()).count();

        // A replay of a released batch cannot be answered as a duplicate — the
        // rows that proved it durable are gone — so it is refused, and the one
        // thing it must never do is append a second physical copy.
        let replay = replica
            .append(batch_request(&id, nonce, 0, None, &[records[0].clone()]))
            .await;
        assert!(replay.is_err(), "a released batch must not be re-mirrored");
        assert_eq!(
            journal.scan_from(journal.released_floor()).count(),
            retained
        );

        journal.close().await.unwrap();
        drop(replica);
        drop(journal);

        let journal = Arc::new(Journal::open(&config(dir.path())).unwrap());
        let sibling = FollowerReplica::load(chain(1, 9, 1, 5), Arc::clone(&journal));
        assert!(
            sibling.is_err(),
            "a released mirror still knows it was opened at another epoch"
        );
        journal.close().await.unwrap();
    }

    #[tokio::test]
    async fn shared_journal_rebuilds_chain_scoped_watermarks() {
        let dir = tempfile::tempdir().unwrap();
        let journal = Arc::new(Journal::open(&config(dir.path())).unwrap());
        let chain_a = chain(1, 9, 1, 4);
        let chain_b = chain(1, 9, 2, 5);
        let a = FollowerReplica::load(chain_a.clone(), Arc::clone(&journal)).unwrap();
        let b = FollowerReplica::load(chain_b.clone(), Arc::clone(&journal)).unwrap();
        let nonce_a = [1; 16];
        let nonce_b = [2; 16];
        a.reconnect(reconnect_request(&chain_a, nonce_a))
            .await
            .unwrap();
        b.reconnect(reconnect_request(&chain_b, nonce_b))
            .await
            .unwrap();
        a.append(batch_request(&chain_a, nonce_a, 0, None, &[record(10, 1)]))
            .await
            .unwrap();
        b.append(batch_request(&chain_b, nonce_b, 0, None, &[record(20, 2)]))
            .await
            .unwrap();
        journal.close().await.unwrap();
        drop(a);
        drop(b);
        drop(journal);

        let journal = Arc::new(Journal::open(&config(dir.path())).unwrap());
        let a = FollowerReplica::load(chain_a, Arc::clone(&journal)).unwrap();
        let b = FollowerReplica::load(chain_b, Arc::clone(&journal)).unwrap();
        assert_eq!(a.state.lock().await.cursor.watermark, Some(Lsn::new(0, 10)));
        assert_eq!(b.state.lock().await.cursor.watermark, Some(Lsn::new(0, 20)));
        journal.close().await.unwrap();
    }

    #[tokio::test]
    async fn complete_wire_batch_persists_every_index_before_advancing_cursor() {
        let dir = tempfile::tempdir().unwrap();
        let journal = Arc::new(Journal::open(&config(dir.path())).unwrap());
        let id = chain(1, 9, 1, 4);
        let replica = FollowerReplica::load(id.clone(), Arc::clone(&journal)).unwrap();
        let nonce = [12; 16];
        replica
            .reconnect(reconnect_request(&id, nonce))
            .await
            .unwrap();

        let first = record(10, 1);
        let second = record(journal.chain_grpc_successor(&first).offset, 2);
        let third = record(journal.chain_grpc_successor(&second).offset, 3);
        let records = [first, second, third];
        let reply = replica
            .append(batch_request(&id, nonce, 0, None, &records))
            .await
            .unwrap();
        assert_eq!(decode_progress(reply).1, Some(records[2].lsn));
        assert_eq!(
            journal
                .scan_from(Lsn::new(0, 0))
                .collect::<Result<Vec<_>, _>>()
                .unwrap()
                .len(),
            records.len()
        );
        let key = chain_key(&id).unwrap();
        for (ordinal, record) in records.iter().enumerate() {
            let provenance = journal
                .chain_grpc_record(&key, record.lsn)
                .unwrap()
                .expect("cursor cannot advance before record provenance is durable");
            let provenance: RecordProvenance = postcard::from_bytes(&provenance).unwrap();
            assert_eq!(provenance.ordinal, ordinal as u32);
            assert_eq!(provenance.batch_len, records.len() as u32);
        }
        journal.close().await.unwrap();
    }

    #[tokio::test]
    async fn gaps_and_regressions_do_not_advance_progress() {
        let dir = tempfile::tempdir().unwrap();
        let journal = Arc::new(Journal::open(&config(dir.path())).unwrap());
        let id = chain(1, 9, 1, 1);
        let replica = FollowerReplica::load(id.clone(), Arc::clone(&journal)).unwrap();
        let nonce = [3; 16];
        replica
            .reconnect(reconnect_request(&id, nonce))
            .await
            .unwrap();
        assert!(replica
            .append(batch_request(&id, nonce, 1, None, &[record(10, 1)]))
            .await
            .is_err());
        assert!(replica
            .append(batch_request(
                &id,
                nonce,
                0,
                None,
                &[record(10, 1), record(20, 2)],
            ))
            .await
            .is_err());
        assert_eq!(replica.state.lock().await.cursor, Cursor::default());
        replica
            .append(batch_request(&id, nonce, 0, None, &[record(10, 1)]))
            .await
            .unwrap();
        assert!(replica
            .append(batch_request(
                &id,
                nonce,
                1,
                Some(Lsn::new(0, 9)),
                &[record(11, 2)],
            ))
            .await
            .is_err());
        assert_eq!(
            replica.state.lock().await.cursor.watermark,
            Some(Lsn::new(0, 10))
        );
        journal.close().await.unwrap();
    }

    #[tokio::test]
    async fn append_durable_before_cursor_is_deduped_and_repaired() {
        let dir = tempfile::tempdir().unwrap();
        let journal = Arc::new(Journal::open(&config(dir.path())).unwrap());
        let id = chain(1, 9, 1, 1);
        let key = chain_key(&id).unwrap();
        let provenance = RecordProvenance {
            batch_seq: 0,
            ordinal: 0,
            batch_len: 1,
            predecessor: None,
            first_lsn: Lsn::new(0, 10),
            last_lsn: Lsn::new(0, 10),
            next_lsn: Lsn::new(0, 82),
        };
        let encoded = postcard::to_stdvec(&provenance).unwrap();
        journal
            .append_replicated_indexed(record(10, 1), &key, &encoded)
            .unwrap()
            .unwrap()
            .committed()
            .await
            .unwrap();
        journal.close().await.unwrap();
        drop(journal);

        let journal = Arc::new(Journal::open(&config(dir.path())).unwrap());
        let replica = FollowerReplica::load(id.clone(), Arc::clone(&journal)).unwrap();
        let nonce = [4; 16];
        let reply = replica
            .reconnect(reconnect_request(&id, nonce))
            .await
            .unwrap();
        assert_eq!(decode_progress(reply).1, Some(Lsn::new(0, 10)));
        assert_eq!(replica.duplicate_batches.load(Ordering::Relaxed), 0);
        let retry = replica
            .append(batch_request(&id, nonce, 0, None, &[record(10, 1)]))
            .await
            .unwrap();
        assert_eq!(decode_progress(retry).1, Some(Lsn::new(0, 10)));
        assert_eq!(
            journal
                .scan_from(Lsn::new(0, 0))
                .collect::<Result<Vec<_>, _>>()
                .unwrap()
                .len(),
            1
        );
        // The dedupe is the docs/13 §6 duplicate batch signal, and it is only
        // separable from reconnect noise if the replay is counted where it
        // happens rather than inferred from the unchanged record count.
        assert_eq!(replica.duplicate_batches.load(Ordering::Relaxed), 1);
        journal.close().await.unwrap();
    }

    #[tokio::test]
    async fn adoption_refuses_a_provenance_batch_gap() {
        let dir = tempfile::tempdir().unwrap();
        let journal = Journal::open(&config(dir.path())).unwrap();
        let id = chain(1, 9, 1, 1);
        let key = chain_key(&id).unwrap();
        let provenance = RecordProvenance {
            batch_seq: 1,
            ordinal: 0,
            batch_len: 1,
            predecessor: None,
            first_lsn: Lsn::new(0, 10),
            last_lsn: Lsn::new(0, 10),
            next_lsn: Lsn::new(0, 82),
        };
        journal
            .append_replicated_indexed(
                record(10, 1),
                &key,
                &postcard::to_stdvec(&provenance).unwrap(),
            )
            .unwrap()
            .unwrap()
            .committed()
            .await
            .unwrap();
        assert!(journal.adopt_chain_history(id).is_err());
        journal.close().await.unwrap();
    }

    // Retention is a `journal-raw` property: the Fjall fallback answers
    // `release_before` with `Unsupported` by design (D19, D20 §9), so these
    // five cases are about the default backend and are compiled for it.
    #[cfg(feature = "journal-raw")]
    /// Promotion adopts a *released* mirror by starting where the persisted
    /// cursor says the released prefix ended (D23).
    ///
    /// Adoption walks the same provenance index a rebuild does and used to
    /// insist it begin at batch zero, so the first follower release would have
    /// turned every later promotion into "cannot adopt chain history with a
    /// batch gap" — a node that refuses to start rather than one that starts
    /// short.
    #[tokio::test]
    async fn adoption_over_a_released_mirror_starts_at_the_persisted_cursor() {
        let dir = tempfile::tempdir().unwrap();
        let journal = Arc::new(Journal::open(&config(dir.path())).unwrap());
        let id = chain(1, 9, 1, 4);
        let key = chain_key(&id).unwrap();
        let replica = FollowerReplica::load(id.clone(), Arc::clone(&journal)).unwrap();
        let records = mirror_three(&replica, &journal, &id, [25; 16]).await;

        journal.note_primary_floor(&key, records[2].lsn);
        assert_eq!(
            journal.release_before(Lsn::new(9_999, 0)).unwrap().blocked,
            None
        );

        let history = journal.adopt_chain_history(id).expect("adopt");
        assert_eq!(
            history.watermark(),
            Some(records[2].lsn),
            "the adopted cutoff is the whole mirrored history, released prefix included"
        );
        journal.close().await.unwrap();
    }

    #[tokio::test]
    async fn every_mismatched_chain_component_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let journal = Arc::new(Journal::open(&config(dir.path())).unwrap());
        let id = chain(1, 9, 1, 1);
        let replica = FollowerReplica::load(id.clone(), Arc::clone(&journal)).unwrap();
        for wrong in [
            chain(2, 9, 1, 1),
            chain(1, 8, 1, 1),
            chain(1, 9, 2, 1),
            chain(1, 9, 1, 2),
        ] {
            assert!(replica
                .reconnect(reconnect_request(&wrong, [5; 16]))
                .await
                .is_err());
        }
        assert_eq!(replica.state.lock().await.cursor, Cursor::default());
        journal.close().await.unwrap();
    }

    #[tokio::test]
    async fn interleaved_sessions_cannot_regress_watermark() {
        let dir = tempfile::tempdir().unwrap();
        let journal = Arc::new(Journal::open(&config(dir.path())).unwrap());
        let id = chain(1, 9, 1, 1);
        let replica = Arc::new(FollowerReplica::load(id.clone(), Arc::clone(&journal)).unwrap());
        let first = [6; 16];
        let second = [7; 16];
        replica
            .reconnect(reconnect_request(&id, first))
            .await
            .unwrap();
        replica
            .reconnect(reconnect_request(&id, second))
            .await
            .unwrap();
        let stale = replica.append(batch_request(&id, first, 0, None, &[record(20, 1)]));
        let current = replica.append(batch_request(&id, second, 0, None, &[record(10, 2)]));
        let (stale, current) = tokio::join!(stale, current);
        assert!(stale.is_err());
        assert!(current.is_ok());
        assert_eq!(
            replica.state.lock().await.cursor.watermark,
            Some(Lsn::new(0, 10))
        );
        journal.close().await.unwrap();
    }

    #[tokio::test]
    async fn reconnect_rejects_stale_session_and_identity_frames_without_progress() {
        let dir = tempfile::tempdir().unwrap();
        let journal = Arc::new(Journal::open(&config(dir.path())).unwrap());
        let id = chain(1, 9, 1, 1);
        let replica = FollowerReplica::load(id.clone(), Arc::clone(&journal)).unwrap();
        let stale = [8; 16];
        let current = [9; 16];
        replica
            .reconnect(reconnect_request(&id, stale))
            .await
            .unwrap();
        replica
            .reconnect(reconnect_request(&id, current))
            .await
            .unwrap();

        assert!(replica
            .append(batch_request(&id, stale, 0, None, &[record(10, 1)]))
            .await
            .is_err());
        assert!(replica
            .append(batch_request(
                &chain(1, 9, 2, 1),
                current,
                0,
                None,
                &[record(10, 1)],
            ))
            .await
            .is_err());
        assert_eq!(replica.state.lock().await.cursor, Cursor::default());

        replica
            .append(batch_request(&id, current, 0, None, &[record(10, 1)]))
            .await
            .unwrap();
        assert_eq!(
            replica.state.lock().await.cursor.watermark,
            Some(Lsn::new(0, 10))
        );
        journal.close().await.unwrap();
    }
}
