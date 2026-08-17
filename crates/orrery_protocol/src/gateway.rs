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
    CellId, DiscrepancyReport, Epoch, GridId, Intent, IntentOutcome, Lease, LeaseId, LeaseMsg, Lsn,
    NodeId, PersistId, SeqPair, Tick, Verdict,
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
    /// Session bootstrap that names the protocol version the client speaks.
    ///
    /// A separate variant rather than a third field on [`GatewayMsg::Hello`]:
    /// postcard keys variants positionally and fields by order within one, so
    /// growing `Hello` would silently mis-decode every peer still sending the
    /// two-field form. Appending leaves that encoding untouched.
    ///
    /// **Both bootstraps stay live.** A gateway accepts the unversioned
    /// [`GatewayMsg::Hello`] without checking anything, so version enforcement
    /// is opt-in: it binds the clients that send this variant, and becomes
    /// universal only when `Hello` is removed.
    VersionedHello {
        /// The session token from `orrery_identity` login.
        token: Vec<u8>,
        /// The client's NodeId (transport identity, D3).
        node: NodeId,
        /// The [`crate::PROTOCOL_VERSION`] this client was built against.
        version: u16,
    },
    /// Escalate a signed discrepancy report to the cluster's adjudicator
    /// (docs/07-witnessing.md §3, stage 3 → stage 4).
    ///
    /// This is the only message a *witness* sends about somebody else, and the
    /// cluster believes none of it: the bundle inside is self-verifying and is
    /// re-run against the rules build it pins. The signature binds the
    /// accusation to an account, which is what makes the per-account rate
    /// limit (§7, "observer is the liar") mean anything.
    ///
    /// Appended rather than folded into an existing variant for the reason
    /// [`GatewayMsg::VersionedHello`] gives: postcard keys variants
    /// positionally, so growing one mis-decodes every deployed peer.
    Report {
        /// The self-verifying accusation. Boxed because an
        /// [`crate::EvidenceBundle`] is up to
        /// [`crate::MAX_ADJUDICATION_TICKS`] of frames and every other variant
        /// here is tens of bytes — an unboxed one would set the size of the
        /// whole enum, and therefore of every diff uplink's stack copy. The
        /// encoded form still fits the reliable lane's
        /// [`crate::channels::MAX_RELIABLE_MESSAGE_BYTES`], which is what it
        /// rides.
        report: Box<DiscrepancyReport>,
    },
}

impl GatewayMsg {
    /// Whether a service speaking `current` accepts a peer offering `offered`:
    /// the rolling-upgrade window of `{V, V−1}`
    /// ([`crate::PROTOCOL_VERSION`]), so a cluster always deploys ahead of its
    /// clients.
    ///
    /// `current` is a parameter rather than the constant because a gateway
    /// carries its own version per instance, which is what lets a test drive
    /// both ends of the window without touching the constant.
    #[must_use]
    pub const fn protocol_accepted(current: u16, offered: u16) -> bool {
        offered == current || offered == current.saturating_sub(1)
    }
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
    /// The gateway refused the session.
    ///
    /// A refused [`GatewayMsg::VersionedHello`] would otherwise be silent, and
    /// silence here is indistinguishable from a slow gateway: the client would
    /// re-dial and re-offer the same unacceptable version until it gave up,
    /// with nothing to report but a timeout.
    HelloRefused {
        /// The gateway's NodeId.
        gateway: NodeId,
        /// The version the gateway itself speaks, so the client can report the
        /// skew rather than only the failure.
        protocol: u16,
        /// Why the session was refused. See
        /// [`GatewayReply::HELLO_REFUSED_PROTOCOL`].
        reason: u8,
    },
    /// A [`GatewayMsg::Report`] was adjudicated, or refused.
    ///
    /// **Never silence.** A reporter that files and hears nothing cannot tell
    /// a cluster with no adjudicator configured from one that judged the
    /// evidence and exonerated the subject, and the two call for opposite
    /// responses: the first is an operator's gap, the second is the witness
    /// being wrong. The refusal codes are the
    /// [`REPORT_ADJUDICATED`](crate::REPORT_ADJUDICATED) family.
    ReportVerdict {
        /// The accused peer the report named, echoed so a reporter with
        /// several escalations open can match the answer to the accusation.
        subject: NodeId,
        /// The entity the bundle covered.
        entity: PersistId,
        /// The bundle's `window_end` — the disputed claim tick. With
        /// `subject` and `entity` this identifies the window, and a witness
        /// escalates a given window once.
        window_end: Tick,
        /// The verdict, when the report was adjudicated at all. `None`
        /// whenever `reason` is nonzero.
        verdict: Option<Verdict>,
        /// [`REPORT_ADJUDICATED`](crate::REPORT_ADJUDICATED) when `verdict` is
        /// present, otherwise why the cluster would not judge it.
        reason: u16,
    },
}

impl GatewayReply {
    /// [`GatewayReply::HelloRefused`] reason: the offered protocol version is
    /// outside the `{V, V−1}` window this gateway accepts
    /// ([`GatewayMsg::protocol_accepted`]).
    pub const HELLO_REFUSED_PROTOCOL: u8 = 1;
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
/// Area pages ride the reliable stream lane (C-1), so this is no longer an MTU
/// figure: QUIC re-segments a stream write across as many packets as the path
/// needs and retransmits what is lost. What remains bounded is the *message*,
/// because both readers refuse a length prefix larger than
/// [`MAX_RELIABLE_MESSAGE_BYTES`](crate::channels::MAX_RELIABLE_MESSAGE_BYTES)
/// before allocating for it, and because a receiver holding partial chunks for
/// 27 cells wants each chunk's footprint knowable in advance.
///
/// 64 KiB is that bound, with an order of magnitude of headroom under the
/// message cap. Against the old 1100-byte MTU budget it cuts a large cell's
/// chunk count — and with it the reassembly bookkeeping and the per-chunk
/// postcard header tax — by ~60×, which is most of what leaving the packet
/// lane buys. A cell whose entities still do not fit is split across as many
/// sequenced [`AreaPage`] frames as needed
/// (`page_seq`/`chunk_index`/`total_chunks`); tune the budget here, in one
/// place.
pub const MAX_AREA_PAGE_FRAME_BYTES: usize = 64 * 1024;

/// The chunker bounds the payload it packs; the frame it emits adds postcard
/// and channel headers on top. Raise the budget to the message cap itself and
/// a maximal chunk encodes just past it, at which point the peer's reader
/// refuses the whole message — a failure that looks like a silently missing
/// cell, not an overflow. Checked at compile time, where a bad edit cannot
/// reach a test run.
const _: () = assert!(
    MAX_AREA_PAGE_FRAME_BYTES < crate::channels::MAX_RELIABLE_MESSAGE_BYTES,
    "area-page budget must leave headroom under the reliable message cap"
);

/// A page of an area load (D11 §9).
///
/// `entities` and `payloads` are parallel: `payloads[i]` is the component bag
/// for `entities[i]`. `live` distinguishes actor-memory pages (authoritative,
/// ≥ checkpoint freshness) from cold FDB range-scan pages.
///
/// A cell whose entities exceed [`MAX_AREA_PAGE_FRAME_BYTES`] is split across
/// several sequenced chunks. The chunk coordinates outlived the datagram lane
/// they were designed for, and deliberately: the reliable lane delivers each
/// chunk exactly once and in order *within one connection*, but a page split
/// across a reconnect is re-sent from the start, so a chunk still carries the
/// full identity of its page rather than just an intra-page index.
///
/// - `page_seq` is the gateway's per-send counter for the page (a re-sent page
///   has a different `page_seq`, so chunks of an interrupted send never mix
///   with the re-send);
/// - `chunk_index` is this chunk's index within the page;
/// - `total_chunks` is the page's chunk count (every chunk carries it, so any
///   arrival order completes the set — a page is complete when all
///   `0..total_chunks` chunk indices for one `page_seq` are held).
///
/// A single-chunk page is `chunk_index: 0, total_chunks: 1`. The client
/// reassembles all chunks of a page before presenting it; a partial set is
/// held (never surfaced as complete) until a re-subscribe supersedes it.
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
    fn single_frame_page_fits_the_frame_budget() {
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

    /// A minimal but structurally complete report, for the wire tests below.
    fn report() -> Box<DiscrepancyReport> {
        use crate::{ChainHash, EvidenceBundle, RulesetId, StateClaim};
        let subject = iroh_base::SecretKey::from_bytes(&[3; 32]);
        let ruleset = RulesetId {
            version: 2,
            digest: [9; 32],
        };
        Box::new(DiscrepancyReport {
            subject: subject.public(),
            bundle: EvidenceBundle {
                ruleset,
                entity: PersistId::new(11),
                window_start: Tick::new(100),
                window_end: Tick::new(130),
                t0_claim: StateClaim {
                    entity: PersistId::new(11),
                    chain_epoch: 0,
                    tick: Tick::new(100),
                    input_head: ChainHash::EMPTY,
                    state_hash: [1; 32],
                    prev_claim: [0; 32],
                    ruleset,
                    sig: subject.sign(b"claim"),
                },
                t0_snapshot: bytes::Bytes::from_static(b"state"),
                frames: Vec::new(),
                sibling_heads: Vec::new(),
                disputed_claims: Vec::new(),
                claimed_hashes: vec![[3; 32]; 30],
                computed_hashes: vec![[4; 32]; 30],
            },
            reporter: node(5),
            reporter_sig: subject.sign(b"report"),
        })
    }

    #[test]
    fn report_and_verdict_roundtrip() {
        // Both halves of the escalation exchange cross the reliable lane and
        // feed a strike ledger, so both have to survive the trip intact.
        let msg = GatewayMsg::Report { report: report() };
        let bytes = postcard::to_stdvec(&msg).unwrap();
        assert_eq!(postcard::from_bytes::<GatewayMsg>(&bytes).unwrap(), msg);
        // And it rides the reliable lane, which is what makes a bundle-sized
        // message admissible at all.
        assert!(
            crate::channels::encode_stream_frame(&msg).len()
                <= crate::channels::MAX_RELIABLE_MESSAGE_BYTES
        );

        for reply in [
            GatewayReply::ReportVerdict {
                subject: node(3),
                entity: PersistId::new(11),
                window_end: Tick::new(130),
                verdict: Some(Verdict::Confirms {
                    at: Tick::new(117),
                    kind: crate::DeviationKind::DiscreteMismatch,
                }),
                reason: crate::REPORT_ADJUDICATED,
            },
            GatewayReply::ReportVerdict {
                subject: node(3),
                entity: PersistId::new(11),
                window_end: Tick::new(130),
                verdict: None,
                reason: crate::REPORT_REFUSED_NO_ADJUDICATOR,
            },
        ] {
            let bytes = postcard::to_stdvec(&reply).unwrap();
            assert_eq!(postcard::from_bytes::<GatewayReply>(&bytes).unwrap(), reply);
        }
    }

    #[test]
    fn appending_the_report_variants_moved_no_existing_discriminant() {
        // postcard keys enum variants positionally, so a variant inserted
        // anywhere but the end silently re-points every deployed peer's
        // messages at the wrong arm. Round-trips inside one build cannot catch
        // that — both ends shift together — so the discriminants are pinned
        // here as literals.
        let release = LeaseMsg::Heartbeat {
            renew: vec![(PersistId::new(1), LeaseId(1))],
            tick: Tick::new(0),
        };
        let msgs: [(GatewayMsg, u8); 8] = [
            (
                GatewayMsg::Hello {
                    token: Vec::new(),
                    node: node(1),
                },
                0,
            ),
            (
                GatewayMsg::Lease {
                    message: release.clone(),
                },
                1,
            ),
            (GatewayMsg::InterestGrant { grant: Vec::new() }, 2),
            (
                GatewayMsg::Diff {
                    diff: DiffUplink {
                        cell: CellId::ROOT,
                        grid: GridId::ROOT,
                        entity: PersistId::new(1),
                        tick: Tick::new(0),
                        kind: RecordKind::ComponentDiff,
                        payload: bytes::Bytes::new(),
                        seq: 0,
                        lease_id: None,
                        authority_seq: None,
                    },
                },
                3,
            ),
            (
                GatewayMsg::Subscribe {
                    grid: GridId::ROOT,
                    cells: Vec::new(),
                },
                4,
            ),
            (
                GatewayMsg::SubmitIntent {
                    intent: Intent {
                        intent_id: 1,
                        issuer: node(1),
                        cell_epoch: crate::CellEpoch::new(0),
                        ops: Vec::new(),
                        attestations: Vec::new(),
                        signature: iroh_base::SecretKey::from_bytes(&[1; 32]).sign(b"x"),
                    },
                },
                5,
            ),
            (
                GatewayMsg::VersionedHello {
                    token: Vec::new(),
                    node: node(1),
                    version: 1,
                },
                6,
            ),
            (GatewayMsg::Report { report: report() }, 7),
        ];
        for (msg, discriminant) in msgs {
            assert_eq!(
                postcard::to_stdvec(&msg).unwrap()[0],
                discriminant,
                "GatewayMsg discriminant moved: {msg:?}"
            );
        }

        let replies: [(GatewayReply, u8); 10] = [
            (
                GatewayReply::HelloAck {
                    gateway: node(1),
                    protocol: 1,
                },
                0,
            ),
            (GatewayReply::Lease { message: release }, 1),
            (
                GatewayReply::InterestAck {
                    epoch: None,
                    reason: INTEREST_ACK_OK,
                },
                2,
            ),
            (
                GatewayReply::BulkAck {
                    entity: PersistId::new(1),
                    tick: Tick::new(0),
                    lsn: Lsn::new(0, 0),
                    provisional: false,
                },
                3,
            ),
            (
                GatewayReply::BulkNack {
                    entity: PersistId::new(1),
                    tick: Tick::new(0),
                    reason: 0,
                    lease: None,
                },
                4,
            ),
            (
                GatewayReply::AreaPage {
                    cell: CellId::ROOT,
                    page: AreaPage {
                        cell: CellId::ROOT,
                        page_seq: 0,
                        chunk_index: 0,
                        total_chunks: 1,
                        entities: Vec::new(),
                        payloads: Vec::new(),
                        live: true,
                    },
                },
                5,
            ),
            (
                GatewayReply::AreaLoadError {
                    cell: CellId::ROOT,
                    kind: AREA_LOAD_ERR_LIVE,
                },
                6,
            ),
            (
                GatewayReply::IntentAck {
                    intent_id: 1,
                    outcome: IntentOutcome::Rejected { reason: 1 },
                },
                7,
            ),
            (
                GatewayReply::HelloRefused {
                    gateway: node(1),
                    protocol: 1,
                    reason: GatewayReply::HELLO_REFUSED_PROTOCOL,
                },
                8,
            ),
            (
                GatewayReply::ReportVerdict {
                    subject: node(1),
                    entity: PersistId::new(1),
                    window_end: Tick::new(0),
                    verdict: Some(Verdict::Exonerates),
                    reason: crate::REPORT_ADJUDICATED,
                },
                9,
            ),
        ];
        for (reply, discriminant) in replies {
            assert_eq!(
                postcard::to_stdvec(&reply).unwrap()[0],
                discriminant,
                "GatewayReply discriminant moved: {reply:?}"
            );
        }
    }

    #[test]
    fn versioned_hello_roundtrips_and_leaves_the_unversioned_form_alone() {
        let versioned = GatewayMsg::VersionedHello {
            token: b"session-token".to_vec(),
            node: node(1),
            version: crate::PROTOCOL_VERSION,
        };
        let bytes = postcard::to_stdvec(&versioned).unwrap();
        let back: GatewayMsg = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(back, versioned);

        // The point of appending rather than growing `Hello`: a peer still
        // sending the two-field form encodes exactly what it always did.
        let unversioned = GatewayMsg::Hello {
            token: b"session-token".to_vec(),
            node: node(1),
        };
        let bytes = postcard::to_stdvec(&unversioned).unwrap();
        assert_eq!(
            postcard::from_bytes::<GatewayMsg>(&bytes).unwrap(),
            unversioned
        );
    }

    #[test]
    fn hello_refused_roundtrips() {
        let reply = GatewayReply::HelloRefused {
            gateway: node(1),
            protocol: 4,
            reason: GatewayReply::HELLO_REFUSED_PROTOCOL,
        };
        let bytes = postcard::to_stdvec(&reply).unwrap();
        let back: GatewayReply = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(back, reply);
    }

    #[test]
    fn the_accepted_version_window_is_v_and_v_minus_one() {
        assert!(GatewayMsg::protocol_accepted(3, 3));
        assert!(GatewayMsg::protocol_accepted(3, 2));
        assert!(!GatewayMsg::protocol_accepted(3, 1));
        assert!(!GatewayMsg::protocol_accepted(3, 4));
        // A service at the floor accepts only itself, rather than wrapping
        // into the top of the u16 range.
        assert!(GatewayMsg::protocol_accepted(0, 0));
        assert!(!GatewayMsg::protocol_accepted(0, u16::MAX));
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
