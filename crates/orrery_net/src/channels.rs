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
    decode_datagram, decode_hit, decode_replication, decode_stream_frame, decode_witness,
    encode_datagram, encode_hit, encode_replication, encode_replication_compressed,
    encode_stream_frame, encode_witness, encode_witness_compressed, tag, untag, Channel,
    TAG_CONTROL, TAG_HIT, TAG_REPLICATION, TAG_REPLICATION_COMPRESSED, TAG_STATE, TAG_WITNESS,
    TAG_WITNESS_COMPRESSED,
};
