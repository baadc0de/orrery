//! Orrery gateway session, diff uplink scheduler, area load/subscribe, and
//! intent queue (P2, docs/10-crates.md §9).
//!
//! This is the client side of the "really really fast" persistence tier (D11).
//! It owns the client's relationship with the gateway:
//!
//! - **Gateway session** — connect, hello, and track the reliable stream +
//!   datagram channels to the gateway (D3: datagrams = state, streams =
//!   control/bulk).
//! - **Diff uplink scheduler** — the 1–4 Hz per-entity change-detection
//!   uplink (D11 §2.1). Replicon change-detection diffs for locally-authoritative
//!   entities are scheduled by a per-entity priority accumulator and sent as
//!   unreliable datagrams; unacked diffs stay buffered and are resent on
//!   reconnect (records are idempotent, keyed by `(entity, tick)`).
//! - **Area load/subscribe** — the 27-cell neighborhood (D5) is requested on
//!   the reliable stream lane and streamed back **nearest-first** (center
//!   cell, then face/edge/corner neighbors by distance), so the client can
//!   spawn-in against page one (D16: < 50 ms to first page-in).
//! - **Intent queue** — signed, witness-attested critical writes (D11 §2.2)
//!   with the netsplit posture (D12): while the gateway is unreachable the
//!   queue persists locally and durable commits pause; on reconnect, queued
//!   intents replay (idempotency keys make this safe). In-flight intents are
//!   requeued on disconnect or in-flight timeout.
//! - **Latency sampling** — bounded-memory histograms (D16) for bulk-ack and
//!   intent-commit latency, readable from the scheduler and queue resources.
//!
//! The gateway wire surface lives in `orrery_protocol` (engine-agnostic, D15),
//! so `orrery_persistd` and this crate share one message set. The transport is
//! `aeronet_io` sessions; the iroh dialing and session lifecycle live in
//! `orrery_net`.
//!
//! Both lanes are the real ones: bulk diffs use `aeronet_io::Session`'s
//! datagram buffers and control traffic uses `aeronet_iroh::stream`'s
//! `IrohStreamIo`. `tests/gateway_live.rs` drives the whole surface — hello,
//! lease control, a multi-chunk area page and an intent commit — against a
//! real `orrery_persistd` gateway over iroh, so the wire contract is checked
//! against the server that implements it rather than against a fake.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod area;
pub mod config;
pub mod corrections;
pub mod feed;
pub mod gateway;
pub mod intents;
pub mod latency;
pub mod plugin;
pub mod queue_store;
pub mod replies;
pub mod reports;
pub mod uplink;

pub use area::{order_nearest_first, sync_aoi_to_loader, AreaLoader, LoadedPage};
pub use config::PersistClientConfig;
pub use corrections::{AuthorityCorrectionQueue, AUTHORITY_CORRECTION_QUEUE_CAPACITY};
pub use feed::{feed_uplink, LocallyAuthoritative, PersistId, UplinkSeq};
pub use gateway::{GatewayConfig, GatewaySession, GatewayState, SessionEvent};
pub use intents::{
    CoSignDisposition, IntentQueue, IntentStatus, IntentTicket, PredictedEffects, COSIGN_BUDGET,
};
pub use latency::LatencyHistogram;
pub use plugin::{OrreryPersistClientPlugin, PersistClientSet};
pub use reports::{drain_reports, ReportOutcome, ReportQueue, DEFAULT_REPORT_QUEUE_CAPACITY};
pub use uplink::UplinkScheduler;

#[cfg(test)]
mod wire_contract_tests {
    /// The reliable-lane message cap must agree with the transport's own.
    ///
    /// `orrery_protocol` defines the cap because `orrery_persistd` is
    /// Bevy-free and cannot link `aeronet_iroh`; this crate links both, so it
    /// is the only place the two can be compared. A drift would not fail
    /// loudly — the larger side would emit messages the smaller side refuses,
    /// and the loss would surface as a reply that never arrives.
    #[test]
    fn reliable_message_cap_matches_the_transport() {
        assert_eq!(
            orrery_protocol::channels::MAX_RELIABLE_MESSAGE_BYTES as u64,
            orrery_net::peer_link::MAX_STREAM_MESSAGE_LEN,
        );
    }
}
