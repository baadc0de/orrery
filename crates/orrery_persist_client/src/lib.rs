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
//! - **Area load/subscribe** — the 27-cell neighborhood (D5) is requested over
//!   a reliable stream and streamed back **nearest-first** (center cell, then
//!   face/edge/corner neighbors by distance), so the client can spawn-in
//!   against page one (D16: < 50 ms to first page-in).
//! - **Intent queue** — signed, witness-attested critical writes (D11 §2.2)
//!   with the netsplit posture (D12): while the gateway is unreachable the
//!   queue persists locally and durable commits pause; on reconnect, queued
//!   intents replay (idempotency keys make this safe).
//!
//! The gateway wire surface lives in `orrery_protocol` (engine-agnostic, D15),
//! so `orrery_persistd` and this crate share one message set. The transport is
//! `aeronet_io` sessions; the iroh dialing and session lifecycle live in
//! `orrery_net`.
//!
//! This slice is the P2 client: the scheduler, the area loader, the intent
//! queue, and the replicon change-detection wiring (feeding [`DiffUplink`]s
//! from replicon diffs) are implemented and unit-tested against an in-memory
//! gateway harness. The iroh stream plumbing lands with the full P2
//! integration.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod area;
pub mod config;
pub mod feed;
pub mod gateway;
pub mod intents;
pub mod plugin;
pub mod queue_store;
pub mod replies;
pub mod uplink;

pub use area::{order_nearest_first, sync_aoi_to_loader, AreaLoader, LoadedPage};
pub use config::PersistClientConfig;
pub use feed::{feed_uplink, LocallyAuthoritative, PersistId, UplinkSeq};
pub use gateway::{GatewayConfig, GatewaySession, GatewayState, SessionEvent};
pub use intents::{IntentQueue, IntentStatus, IntentTicket, PredictedEffects};
pub use plugin::{OrreryPersistClientPlugin, PersistClientSet};
pub use uplink::UplinkScheduler;
