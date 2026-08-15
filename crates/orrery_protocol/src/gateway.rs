//! Client ↔ gateway wire surface (D11, docs/10-crates.md §9).
//!
//! The gateway is the client's single point of contact with the persistence
//! cluster: it routes bulk diffs to cell actors, serves area loads from actor
//! memory + FDB range scans, and executes attested intents. This module defines
//! the messages that cross that boundary, engine-agnostic and postcard-
//! serializable, so `orrery_persist_client` (Bevy) and `orrery_persistd`
//! (Bevy-free) share one wire surface — exactly the dependency-spine rule
//! (docs/10-crates.md §Dependency spine).
//!
//! The two write classes (docs/08-persistence.md §2) map onto two message
//! families:
//!
//! - **Bulk** ([`DiffUplink`] → [`GatewayReply::BulkAck`]): unreliable
//!   datagrams, app-level acks, idempotent `(entity, tick)` last-writer-wins
//!   records. The ack is the client-observed durability contract (p99 < 5 ms
//!   in-region, D16).
//! - **Critical** ([`SubmitIntent`] → [`GatewayReply::IntentAck`]): reliable
//!   stream, signed + witness-attested, idempotency-keyed.
//!
//! Area load ([`Subscribe`] → [`GatewayReply::AreaPage`]) rides reliable
//! streams, one page per cell, streamed nearest-first.

use serde::{Deserialize, Serialize};

use crate::{
    CellId, Epoch, GridId, Intent, IntentOutcome, Lease, LeaseId, LeaseMsg, Lsn, NodeId, PersistId,
    SeqPair, Tick,
};

/// A client → gateway message (docs/10-crates.md §9).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum GatewayMsg {
    /// Session bootstrap: authenticate and negotiate the protocol version.
    Hello {
        /// The session token from `orrery_identity` login.
        token: Vec<u8>,
        /// The client's NodeId (transport identity, D3).
        node: NodeId,
    },
    /// Authority-lease control traffic. This always uses the reliable control
    /// lane, unlike bulk diffs.
    Lease {
        /// Authority control message for the registrar.
        message: LeaseMsg,
    },
    /// Present the coordinator's signed grant for this peer's active interest
    /// (D7 §5).
    ///
    /// The peer forwards bytes it cannot forge: the gateway verifies the
    /// coordinator's signature and that the grant names this peer. This is the
    /// same handout shape as the identity token in [`GatewayMsg::Hello`] —
    /// carried by the peer, authored by someone else.
    InterestGrant {
        /// A postcard-encoded `InterestGrantV1`.
        grant: Vec<u8>,
    },
    /// Bulk uplink: one change-detection diff for an entity (D11 §2.1).
    Diff {
        /// The diff to journal.
        diff: DiffUplink,
    },
    /// Area load: subscribe to a 27-cell neighborhood (D11 §9).
    Subscribe {
        /// The grid the cells live in (root universe grid is 0). The load is
        /// one `grid/{grid_id}` frame read plus the 27-cell scans in that
        /// grid's `CellId` space (docs/08-persistence.md §9, P-7).
        grid: GridId,
        /// The cells to load, ordered nearest-first by the client.
        cells: Vec<CellId>,
    },
    /// Critical op: submit an attested intent (D11 §2.2).
    SubmitIntent {
        /// The signed, witness-attested intent.
        intent: Intent,
    },
}

/// A gateway → client message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum GatewayReply {
    /// Session established; the gateway accepts this protocol version.
    HelloAck {
        /// The gateway's NodeId.
        gateway: NodeId,
        /// The negotiated protocol version.
        protocol: u16,
    },
    /// Authority-lease control reply.
    Lease {
        /// Authority control reply from the registrar.
        message: LeaseMsg,
    },
    /// Outcome of presenting a coordinator interest grant.
    ///
    /// A peer needs this: without it, a rejected grant is indistinguishable
    /// from a working one until claims start failing as `NotEligible` for no
    /// visible reason.
    InterestAck {
        /// The coordinator epoch now in force for this peer, when accepted.
        epoch: Option<Epoch>,
        /// Why the grant was refused, as a stable numeric code. `0` on
        /// acceptance. See `INTEREST_ACK_*`.
        reason: u8,
    },
    /// A bulk diff was durably journaled at `lsn` (D11 §2.1).
    ///
    /// `provisional` marks an epoch-fenced downgrade: the actor could not
    /// confirm its shard-ownership epoch, so the ack is provisional and the
    /// client must keep the diff buffered and resend to the new owner
    /// (docs/08-persistence.md §2.1).
    BulkAck {
        /// The entity the diff was for.
        entity: PersistId,
        /// The tick the diff was keyed by.
        tick: Tick,
        /// The durable journal position.
        lsn: Lsn,
        /// Whether this is a provisional (epoch-unconfirmed) ack.
        provisional: bool,
    },
    /// A bulk diff was rejected (invariant violation, stale epoch, or
    /// malformed record).
    BulkNack {
        /// The entity the diff was for.
        entity: PersistId,
        /// The tick the diff was keyed by.
        tick: Tick,
        /// A `Ruleset`-defined rejection reason code.
        reason: u16,
        /// Current registrar row when fencing rejected this persistent write.
        /// `None` is retained for non-authority failures such as a closed
        /// journal.
        #[serde(default)]
        lease: Option<Lease>,
    },
    /// A page of an area load for one cell (D11 §9).
    AreaPage {
        /// The cell this page covers.
        cell: CellId,
        /// The page contents.
        page: AreaPage,
    },
    /// A cell's area-load read failed on the gateway — distinct from an empty
    /// cell (which is an [`AreaPage`] with no entities) so a failed FDB scan is
    /// diagnosable rather than indistinguishable from "nothing there"
    /// (docs/08-persistence.md §9).
    AreaLoadError {
        /// The cell whose read failed.
        cell: CellId,
        /// The failure class ([`AREA_LOAD_ERR_LIVE`]: the owning actor is
        /// gone; [`AREA_LOAD_ERR_COLD`]: the durable-tier scan errored). A
        /// `u8`, not a `&str`, so `GatewayReply` stays borrow-free and
        /// deserializable from any buffer lifetime.
        kind: u8,
    },
    /// An intent committed or was rejected (D11 §2.2).
    IntentAck {
        /// The intent's idempotency key.
        intent_id: u128,
        /// The outcome.
        outcome: IntentOutcome,
    },
}

/// A single bulk diff uplink (D11 §2.1).
///
/// The gateway fills in the server-assigned `epoch`/`lsn`/`author`/`crc` and
/// journals it as a [`crate::JournalRecord`]. Records are idempotent — keyed by
/// `(entity, tick)` with last-writer-wins per component within an entity's
/// single-writer stream — so unacked diffs can be resent on reconnect.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffUplink {
    /// The Morton `CellId` — the routing and storage key.
    pub cell: CellId,
    /// The grid this cell belongs to (root universe grid is 0).
    pub grid: GridId,
    /// The stable persistent entity id.
    pub entity: PersistId,
    /// The universe tick at append (D8).
    pub tick: Tick,
    /// The kind of record (spawn / component diff / despawn / terrain).
    pub kind: crate::RecordKind,
    /// The postcard-encoded component payload.
    pub payload: bytes::Bytes,
    /// A client-side monotonic sequence per entity, for ordering/idempotency.
    pub seq: u64,
    /// Registrar fencing token. `None` remains valid only for legacy/P2
    /// sessions; a P3 registrar requires a current token for persistent rows.
    #[serde(default)]
    pub lease_id: Option<LeaseId>,
    /// Sequence pair observed by the holder when generating this diff.
    #[serde(default)]
    pub authority_seq: Option<SeqPair>,
}

/// [`GatewayReply::InterestAck`] reason: the grant was accepted.
pub const INTEREST_ACK_OK: u8 = 0;
/// [`GatewayReply::InterestAck`] reason: oversized, undecodable, or a version
/// this build does not accept.
pub const INTEREST_ACK_MALFORMED: u8 = 1;
/// [`GatewayReply::InterestAck`] reason: no configured coordinator key carries
/// the claimed identifier, or the signature did not verify under it.
pub const INTEREST_ACK_UNTRUSTED: u8 = 2;
/// [`GatewayReply::InterestAck`] reason: the grant authorizes a different peer.
pub const INTEREST_ACK_WRONG_PEER: u8 = 3;
/// [`GatewayReply::InterestAck`] reason: unusable coverage or lifetime.
pub const INTEREST_ACK_BOUNDS: u8 = 4;
/// [`GatewayReply::InterestAck`] reason: an epoch this gateway has moved past.
pub const INTEREST_ACK_SUPERSEDED: u8 = 5;
/// [`GatewayReply::InterestAck`] reason: this gateway accepts no grants.
pub const INTEREST_ACK_UNSUPPORTED: u8 = 6;

/// [`GatewayReply::AreaLoadError`] kind: the live read failed (the owning
/// actor is gone — e.g. it crashed between the liveness check and the read).
pub const AREA_LOAD_ERR_LIVE: u8 = 1;
/// [`GatewayReply::AreaLoadError`] kind: the cold durable-tier scan errored
/// (e.g. an FDB transaction failure).
pub const AREA_LOAD_ERR_COLD: u8 = 2;

/// The maximum size of one encoded area-page frame on the wire, in bytes.
///
/// The lane is packet-only (D3 datagrams; the reliable-stream class of
/// docs/08-persistence.md §2.1 does not exist in this build), so every frame
/// must fit one datagram. The budget derives from the 1280-byte IPv6 minimum
/// MTU minus QUIC/UDP/IP headers and the channel tag — deliberately
/// conservative so it holds on any path QUIC will run over. A cell whose
/// entities do not fit is split across as many sequenced [`AreaPage`] frames
/// as needed (`page_seq`/`chunk_index`/`total_chunks`); tune the budget here,
/// in one place.
pub const MAX_AREA_PAGE_FRAME_BYTES: usize = 1100;

/// A page of an area load (D11 §9).
///
/// `entities` and `payloads` are parallel: `payloads[i]` is the component bag
/// for `entities[i]`. `live` distinguishes actor-memory pages (authoritative,
/// ≥ checkpoint freshness) from cold FDB range-scan pages.
///
/// A cell whose entities exceed [`MAX_AREA_PAGE_FRAME_BYTES`] is split across
/// several sequenced chunks. The wire is unordered datagrams (D3), so a chunk
/// carries the full identity of its page, not just an intra-page index:
///
/// - `page_seq` is the gateway's per-send counter for the page (a retried send
///   has a different `page_seq`, so stale chunks of an old send never mix with
///   the retry);
/// - `chunk_index` is this chunk's index within the page;
/// - `total_chunks` is the page's chunk count (every chunk carries it, so any
///   arrival order completes the set — a page is complete when all
///   `0..total_chunks` chunk indices for one `page_seq` are held).
///
/// A single-chunk page is `chunk_index: 0, total_chunks: 1`. The client
/// reassembles all chunks of a page before presenting it; a partial set is
/// held (never surfaced as complete) until the client's retry floor leads to
/// a re-request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AreaPage {
    /// The cell this page covers.
    pub cell: CellId,
    /// The page sequence number (per gateway send; distinguishes a retry from
    /// the original send's chunks).
    pub page_seq: u32,
    /// This chunk's index within the page (0-based).
    pub chunk_index: u32,
    /// The page's chunk count; the final chunk has `chunk_index == total_chunks - 1`.
    pub total_chunks: u32,
    /// The entities in this chunk of the cell's page.
    pub entities: Vec<PersistId>,
    /// The component payload for each entity, parallel to `entities`.
    pub payloads: Vec<bytes::Bytes>,
    /// Whether this page came from a live cell actor (vs a cold FDB scan).
    pub live: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Epoch, RecordKind};

    fn node(n: u8) -> NodeId {
        let mut seed = [0u8; 32];
        seed[0] = n;
        iroh_base::SecretKey::from_bytes(&seed).public()
    }

    #[test]
    fn diff_uplink_roundtrips() {
        let msg = GatewayMsg::Diff {
            diff: DiffUplink {
                cell: CellId::ROOT,
                grid: GridId::ROOT,
                entity: PersistId::new(7),
                tick: Tick::new(123_456),
                kind: RecordKind::ComponentDiff,
                payload: bytes::Bytes::from_static(b"\x01\x02\x03"),
                seq: 42,
                lease_id: None,
                authority_seq: None,
            },
        };
        let bytes = postcard::to_stdvec(&msg).unwrap();
        let back: GatewayMsg = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(back, msg);
    }

    #[test]
    fn subscribe_carries_grid() {
        // P-7: the area-load request names the grid its cells are relative to,
        // so a nested-grid load never reaches for root-grid rows.
        let msg = GatewayMsg::Subscribe {
            grid: GridId::new(7),
            cells: vec![CellId::ROOT],
        };
        let bytes = postcard::to_stdvec(&msg).unwrap();
        let back: GatewayMsg = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(back, msg);
    }

    #[test]
    fn bulk_ack_roundtrips() {
        let reply = GatewayReply::BulkAck {
            entity: PersistId::new(7),
            tick: Tick::new(123_456),
            lsn: Lsn::new(3, 4096),
            provisional: false,
        };
        let bytes = postcard::to_stdvec(&reply).unwrap();
        let back: GatewayReply = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(back, reply);
    }

    #[test]
    fn area_page_roundtrips() {
        let page = GatewayReply::AreaPage {
            cell: CellId::ROOT,
            page: AreaPage {
                cell: CellId::ROOT,
                page_seq: 0,
                chunk_index: 0,
                total_chunks: 1,
                entities: vec![PersistId::new(1), PersistId::new(2)],
                payloads: vec![
                    bytes::Bytes::from_static(b"a"),
                    bytes::Bytes::from_static(b"b"),
                ],
                live: true,
            },
        };
        let bytes = postcard::to_stdvec(&page).unwrap();
        let back: GatewayReply = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(back, page);
    }

    #[test]
    fn area_load_error_roundtrips() {
        // A failed scan is a distinct reply, not an empty page
        // (docs/08-persistence.md §9).
        let reply = GatewayReply::AreaLoadError {
            cell: CellId::ROOT,
            kind: AREA_LOAD_ERR_COLD,
        };
        let bytes = postcard::to_stdvec(&reply).unwrap();
        let back: GatewayReply = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(back, reply);
    }

    #[test]
    fn single_frame_page_fits_the_datagram_budget() {
        // The budget constant is what the gateway chunks against; a minimal
        // single-frame page must fit well under it (the chunked path is
        // exercised end-to-end in orrery_persistd/tests/area_load.rs).
        let page = GatewayReply::AreaPage {
            cell: CellId::ROOT,
            page: AreaPage {
                cell: CellId::ROOT,
                page_seq: 0,
                chunk_index: 0,
                total_chunks: 1,
                entities: vec![PersistId::new(1)],
                payloads: vec![bytes::Bytes::from_static(b"x")],
                live: true,
            },
        };
        let encoded = crate::channels::encode_stream_frame(&page);
        assert!(encoded.len() <= MAX_AREA_PAGE_FRAME_BYTES);
    }

    #[test]
    fn intent_ack_roundtrips() {
        let reply = GatewayReply::IntentAck {
            intent_id: 0x1122_3344_5566_7788_99aa_bbcc_ddee_ff00,
            outcome: IntentOutcome::Committed {
                tick: Tick::new(9),
                minted: vec![PersistId::new(3)],
            },
        };
        let bytes = postcard::to_stdvec(&reply).unwrap();
        let back: GatewayReply = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(back, reply);
    }

    #[test]
    fn hello_roundtrips() {
        let msg = GatewayMsg::Hello {
            token: b"session-token".to_vec(),
            node: node(1),
        };
        let bytes = postcard::to_stdvec(&msg).unwrap();
        let back: GatewayMsg = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(back, msg);
        // Epoch is not part of the hello; sanity that the enum is exhaustive
        // over the four client messages.
        let _ = Epoch::new(0);
    }
}
