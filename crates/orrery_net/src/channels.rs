//! Channel policy (D3): datagrams = state, streams = control/bulk.
//!
//! The design routes state replication over unreliable datagrams and control/
//! bulk transfers over reliable streams, with no head-of-line blocking between
//! them. The tag/framing implementation lives in `orrery_protocol::channels` —
//! the shared, engine-agnostic wire surface — so both the Bevy client and the
//! Bevy-free `orrery_persistd` gateway reuse one source of truth. This module
//! re-exports it under the `orrery_net` name the rest of the client code
//! already uses.

pub use orrery_protocol::channels::{
    decode_datagram, decode_stream_frame, encode_datagram, encode_stream_frame, tag, untag,
    Channel, TAG_CONTROL, TAG_STATE,
};
