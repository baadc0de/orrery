//! Ownership-fenced tonic/gRPC transport for journal chain replication.
//!
//! Durable state is keyed by all of [`DurableChainId`]. Connection nonces are
//! deliberately absent from storage keys. Each mirrored record is atomically
//! indexed by `(chain, origin_lsn)` with batch provenance, so restart recovery
//! scans only that chain and can distinguish a complete contiguous prefix from
//! an append whose cursor update was interrupted.

use std::collections::BTreeMap;
use std::sync::Arc;

use bytes::{Buf, BufMut};
use hyper_util::client::legacy::{connect::HttpConnector, Client};
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto;
use prost::encoding::{self as pb, DecodeContext, WireType};
use prost::{DecodeError, Message};
use tokio::sync::Mutex;
use tonic::codec::{BufferSettings, Codec, DecodeBuf, Decoder, EncodeBuf, Encoder};
use tonic::codegen::{http, Body, BoxFuture, Service, StdError};
use tonic::{Request, Response, Status};

use orrery_protocol::{JournalRecord, Lsn, NodeId};

use crate::journal::{ChainTransport, Journal, JournalError};

const SERVICE_NAME: &str = "orrery.persistd.chain.v1.ChainReplication";
const APPEND_PATH: &str = "/orrery.persistd.chain.v1.ChainReplication/AppendBatch";
const RECONNECT_PATH: &str = "/orrery.persistd.chain.v1.ChainReplication/Reconnect";
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
}

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct ReconnectRequest {
    chain: Option<WireChainId>,
    session_nonce: Vec<u8>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct ProgressReply {
    known: bool,
    watermark: Option<WireLsn>,
    has_batch: bool,
    batch_seq: u64,
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

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct RecordProvenance {
    batch_seq: u64,
    ordinal: u32,
    batch_len: u32,
    predecessor: Option<Lsn>,
    first_lsn: Lsn,
    last_lsn: Lsn,
    next_lsn: Lsn,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct Cursor {
    batch_seq: Option<u64>,
    watermark: Option<Lsn>,
    next_lsn: Option<Lsn>,
}

fn chain_key(chain: &DurableChainId) -> Result<Vec<u8>, JournalError> {
    postcard::to_stdvec(chain)
        .map_err(|e| JournalError::Store(format!("encode durable chain identity: {e}")))
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

    let mut cursor = Cursor::default();
    for (seq, mut records) in batches {
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

struct FollowerState {
    session_nonce: Option<[u8; 16]>,
    cursor: Cursor,
}

struct FollowerReplica {
    chain: DurableChainId,
    key: Vec<u8>,
    journal: Arc<Journal>,
    state: Mutex<FollowerState>,
}

impl FollowerReplica {
    fn load(chain: DurableChainId, journal: Arc<Journal>) -> Result<Self, JournalError> {
        let key = chain_key(&chain)?;
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

    fn nonce(bytes: &[u8]) -> Result<[u8; 16], Status> {
        bytes
            .try_into()
            .map_err(|_| Status::invalid_argument("session nonce must be 16 bytes"))
    }

    async fn reconnect(&self, request: ReconnectRequest) -> Result<ProgressReply, Status> {
        self.validate_chain(request.chain)?;
        let nonce = Self::nonce(&request.session_nonce)?;
        let mut state = self.state.lock().await;
        // Always reconstruct from chain-scoped provenance. The cached cursor is
        // never used as the recovery probe result.
        state.cursor = rebuild_cursor(&self.journal, &self.key)
            .map_err(|e| Status::internal(format!("rebuild chain cursor: {e}")))?;
        state.session_nonce = Some(nonce);
        let encoded = postcard::to_stdvec(&state.cursor)
            .map_err(|e| Status::internal(format!("encode chain cursor: {e}")))?;
        self.journal
            .set_chain_grpc_state(&self.key, &encoded)
            .map_err(|e| Status::internal(format!("persist chain cursor: {e}")))?;
        Ok(progress(state.cursor))
    }

    async fn append(&self, request: AppendBatchRequest) -> Result<ProgressReply, Status> {
        self.validate_chain(request.chain)?;
        let nonce = Self::nonce(&request.session_nonce)?;
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
                handle
                    .committed()
                    .await
                    .map_err(|e| Status::internal(format!("commit mirrored record: {e}")))?;
            }
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

#[async_trait::async_trait]
trait ChainReplication: Send + Sync + 'static {
    async fn append_batch(
        &self,
        request: Request<AppendBatchRequest>,
    ) -> Result<Response<ProgressReply>, Status>;
    async fn reconnect(
        &self,
        request: Request<ReconnectRequest>,
    ) -> Result<Response<ProgressReply>, Status>;
}

#[async_trait::async_trait]
impl ChainReplication for FollowerReplica {
    async fn append_batch(
        &self,
        request: Request<AppendBatchRequest>,
    ) -> Result<Response<ProgressReply>, Status> {
        self.append(request.into_inner()).await.map(Response::new)
    }

    async fn reconnect(
        &self,
        request: Request<ReconnectRequest>,
    ) -> Result<Response<ProgressReply>, Status> {
        self.reconnect(request.into_inner())
            .await
            .map(Response::new)
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
            APPEND_PATH => {
                struct Method<T>(Arc<T>);
                impl<T: ChainReplication> tonic::server::UnaryService<AppendBatchRequest> for Method<T> {
                    type Response = ProgressReply;
                    type Future = BoxFuture<Response<Self::Response>, Status>;
                    fn call(&mut self, request: Request<AppendBatchRequest>) -> Self::Future {
                        let inner = Arc::clone(&self.0);
                        Box::pin(async move { inner.append_batch(request).await })
                    }
                }
                Box::pin(async move {
                    let mut grpc = tonic::server::Grpc::new(ProstCodec::default());
                    Ok(grpc.unary(Method(inner), request).await)
                })
            }
            RECONNECT_PATH => {
                struct Method<T>(Arc<T>);
                impl<T: ChainReplication> tonic::server::UnaryService<ReconnectRequest> for Method<T> {
                    type Response = ProgressReply;
                    type Future = BoxFuture<Response<Self::Response>, Status>;
                    fn call(&mut self, request: Request<ReconnectRequest>) -> Self::Future {
                        let inner = Arc::clone(&self.0);
                        Box::pin(async move { inner.reconnect(request).await })
                    }
                }
                Box::pin(async move {
                    let mut grpc = tonic::server::Grpc::new(ProstCodec::default());
                    Ok(grpc.unary(Method(inner), request).await)
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
#[derive(Debug)]
pub struct ChainGrpcServer {
    addr: std::net::SocketAddr,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    join: tokio::task::JoinHandle<()>,
}

impl ChainGrpcServer {
    /// Bound local address.
    #[must_use]
    pub fn addr(&self) -> std::net::SocketAddr {
        self.addr
    }

    /// Stop accepting connections.
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
    let service = ChainService(Arc::new(replica));
    let (shutdown, mut stopped) = tokio::sync::oneshot::channel();
    let join = tokio::spawn(async move {
        let builder = auto::Builder::new(TokioExecutor::new()).http2_only();
        loop {
            tokio::select! {
                _ = &mut stopped => break,
                accepted = listener.accept() => {
                    let Ok((stream, _)) = accepted else { break };
                    let service = service.clone();
                    let builder = builder.clone();
                    tokio::spawn(async move {
                        let _ = builder.serve_connection(TokioIo::new(stream), service).await;
                    });
                }
            }
        }
    });
    Ok(ChainGrpcServer {
        addr,
        shutdown: Some(shutdown),
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
}

#[derive(Debug)]
struct ClientState {
    client: GrpcClient,
    nonce: [u8; 16],
    next_batch: u64,
    watermark: Option<Lsn>,
}

fn fresh_nonce() -> [u8; 16] {
    use std::sync::atomic::{AtomicU64, Ordering};
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

async fn unary<Req, Resp>(
    client: GrpcClient,
    addr: std::net::SocketAddr,
    path: &'static str,
    request: Req,
) -> Result<Resp, Status>
where
    Req: Message + Default + Send + 'static,
    Resp: Message + Default + Send + 'static,
{
    let origin = format!("http://{addr}")
        .parse::<http::Uri>()
        .map_err(|e| Status::invalid_argument(format!("gRPC origin: {e}")))?;
    let mut grpc = tonic::client::Grpc::with_origin(client, origin);
    grpc.ready()
        .await
        .map_err(|e| Status::unavailable(format!("gRPC transport: {e}")))?;
    grpc.unary(
        Request::new(request),
        http::uri::PathAndQuery::from_static(path),
        ProstCodec::default(),
    )
    .await
    .map(Response::into_inner)
}

impl GrpcChainTransport {
    /// Connect and perform a fresh remote recovery probe.
    pub async fn connect(
        addr: std::net::SocketAddr,
        chain: DurableChainId,
    ) -> Result<Self, JournalError> {
        let transport = Self {
            addr,
            chain,
            inner: Mutex::new(ClientState {
                client: build_client(),
                nonce: fresh_nonce(),
                next_batch: 0,
                watermark: None,
            }),
        };
        transport.reconnect().await?;
        Ok(transport)
    }

    /// Start a fresh session and return the follower's reconstructed progress.
    pub async fn reconnect(&self) -> Result<Option<Lsn>, JournalError> {
        let mut state = self.inner.lock().await;
        self.probe_locked(&mut state).await
    }

    async fn probe_locked(&self, state: &mut ClientState) -> Result<Option<Lsn>, JournalError> {
        state.nonce = fresh_nonce();
        let reply: ProgressReply = unary(
            state.client.clone(),
            self.addr,
            RECONNECT_PATH,
            ReconnectRequest {
                chain: Some(wire_chain(&self.chain)),
                session_nonce: state.nonce.to_vec(),
            },
        )
        .await
        .map_err(|e| JournalError::Store(format!("gRPC reconnect: {e}")))?;
        let (batch, watermark) = decode_progress(reply);
        state.next_batch = batch.map_or(0, |value| value + 1);
        state.watermark = watermark;
        Ok(watermark)
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
        let attempted_batch = state.next_batch;
        let make_request = |state: &ClientState| AppendBatchRequest {
            chain: Some(wire_chain(&self.chain)),
            session_nonce: state.nonce.to_vec(),
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
        };
        let first: Result<ProgressReply, Status> = unary(
            state.client.clone(),
            self.addr,
            APPEND_PATH,
            make_request(&state),
        )
        .await;
        let reply = match first {
            Ok(reply) => reply,
            Err(error) => {
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
                unary(
                    state.client.clone(),
                    self.addr,
                    APPEND_PATH,
                    make_request(&state),
                )
                .await
                .map_err(|e| JournalError::Store(format!("gRPC append retry: {e}")))?
            }
        };
        let (batch, watermark) = decode_progress(reply);
        if batch != Some(state.next_batch) || watermark != Some(last) {
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
        }
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
}
