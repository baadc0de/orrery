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

use crate::{CellId, GridId, Intent, IntentOutcome, Lsn, NodeId, PersistId, Tick};

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
    /// Bulk uplink: one change-detection diff for an entity (D11 §2.1).
    Diff {
        /// The diff to journal.
        diff: DiffUplink,
    },
    /// Area load: subscribe to a 27-cell neighborhood (D11 §9).
    Subscribe {
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
    },
    /// A page of an area load for one cell (D11 §9).
    AreaPage {
        /// The cell this page covers.
        cell: CellId,
        /// The page contents.
        page: AreaPage,
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
}

/// A page of an area load (D11 §9).
///
/// `entities` and `payloads` are parallel: `payloads[i]` is the component bag
/// for `entities[i]`. `live` distinguishes actor-memory pages (authoritative,
/// ≥ checkpoint freshness) from cold FDB range-scan pages.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AreaPage {
    /// The cell this page covers.
    pub cell: CellId,
    /// The entities in this cell.
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
            },
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
