//! Channel policy + wire framing shared by the gateway client and server (D3,
//! docs/10-crates.md §Dependency spine).
//!
//! The gateway boundary is split into two logical channels: **state** (bulk
//! diffs) over unreliable datagrams and **control** (area load, intents, hello)
//! over reliable QUIC streams (D3: datagrams = state, streams = control/bulk).
//! This module owns the one-byte tag that prefixes every payload so the
//! receiver can route it without a separate framing layer, plus the
//! encode/decode helpers both sides use — so `orrery_persistd` (Bevy-free) and
//! `orrery_persist_client` (Bevy) share **one** wire surface, not two drifted
//! copies.
//!
//! Payload layouts:
//! - **state**: `[TAG_STATE (0)] [ postcard ]`, one iroh datagram each
//! - **control**: `[TAG_CONTROL (1)] [ u32 LE length ] [ postcard ]`, one
//!   stream-lane message each
//!
//! # Why the control payload keeps its own length prefix
//!
//! The stream lane already delimits messages — the transport writes
//! `[u32 LE length][payload]` and hands the reader whole payloads — so the
//! inner prefix is, on that lane, redundant. It stays for two reasons. One
//! decoder then serves both lanes, which matters because the *receiving* side
//! of both the gateway and the client still accepts a control payload that
//! arrives as a datagram; and the tag is what tells a receiver which of the two
//! kinds it is holding regardless of how it arrived. Five bytes on a lane whose
//! messages are pages and intents is not a trade worth making twice.

use std::io::{Read, Write};

use flate2::{read::DeflateDecoder, write::DeflateEncoder, Compression};
use serde::de::DeserializeOwned;
use serde::Serialize;

use crate::{CellId, HitMsg, PersistId};

/// The two logical channels the design defines (docs/02-networking.md §7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Channel {
    /// Unreliable state replication: per-tick entity state, bulk diffs. Carried
    /// by unhealthy datagrams.
    State,
    /// Reliable control and bulk transfer: connection handshakes, area load,
    /// intents. Carried by reliable streams.
    Control,
}

impl Channel {
    /// Whether this channel is carried by unreliable datagrams.
    #[must_use]
    pub const fn is_datagram(self) -> bool {
        matches!(self, Self::State)
    }

    /// Whether this channel is carried by reliable streams.
    #[must_use]
    pub const fn is_stream(self) -> bool {
        matches!(self, Self::Control)
    }
}

/// The one-byte tag prefixing every payload so the receiver can route it to
/// the right channel. `State` is the default (most traffic).
pub const TAG_STATE: u8 = 0;
/// Tag for control payloads: area load, intents, hello, lease control.
pub const TAG_CONTROL: u8 = 1;

/// The largest control message either side will write to — or accept from —
/// the reliable stream lane, in bytes.
///
/// A stream message is length-prefixed, and the length is chosen by the
/// *sender*, which on the receiving side means it is attacker-chosen. Both
/// readers therefore compare the prefix against this cap **before** reserving
/// a buffer for it, so a peer cannot name a gigabyte and have one allocated.
///
/// This must equal the transport's own cap — `aeronet_iroh`'s
/// `MAX_STREAM_MESSAGE_LEN`, re-exported as
/// `orrery_net::peer_link::MAX_STREAM_MESSAGE_LEN` — because the Bevy client
/// rides that implementation while `orrery_persistd` (Bevy-free, D15) speaks
/// raw iroh and cannot link it. A drift between the two would not fail loudly:
/// the larger side would emit messages the smaller side refuses, and the loss
/// would surface as a missing reply. `orrery_persist_client` links both and
/// asserts they agree.
pub const MAX_RELIABLE_MESSAGE_BYTES: usize = 1024 * 1024;

/// Tag a datagram payload with its channel. Returns a new `Vec` with the tag
/// prepended.
#[must_use]
pub fn tag(channel: Channel, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len() + 1);
    out.push(match channel {
        Channel::State => TAG_STATE,
        Channel::Control => TAG_CONTROL,
    });
    out.extend_from_slice(payload);
    out
}

/// Strip a channel tag from a received datagram, returning the channel and the
/// payload (excluding the tag byte). Returns `None` if the payload is empty.
pub fn untag(payload: &[u8]) -> Option<(Channel, &[u8])> {
    let (&tag, rest) = payload.split_first()?;
    let channel = match tag {
        TAG_STATE => Channel::State,
        TAG_CONTROL => Channel::Control,
        _ => return None,
    };
    Some((channel, rest))
}

/// A protocol-owned family identified by the inner frame's sub-tag.
///
/// This is the authoritative list of non-opaque P2P wire families.  Keep the
/// enum exhaustive at consumers: adding a sub-tag starts by adding one variant
/// here, so `orrery_net` must make a delivery decision rather than inheriting
/// its transport's default lane.  The enum describes bytes on the wire, not a
/// caller-provided quality-of-service field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WireFamily {
    /// Absolute replicated state, also the keyframe for delta patches.
    Replication,
    /// Compressed absolute replicated state.
    ReplicationCompressed,
    /// A replication delta referring to a previous absolute keyframe.
    ReplicationDelta,
    /// An absolute keyframe sent to a witness-set link.
    WitnessKeyframe,
    /// A verifiable-core frame or state claim.
    Witness,
    /// Compressed verifiable-core traffic.
    WitnessCompressed,
    /// An addressed cross-authority canonical input.
    DeliveredInput,
    /// A hit claim or its verdict.
    Hit,
}

impl WireFamily {
    /// The stable one-byte discriminator for this family.
    #[must_use]
    pub const fn tag(self) -> u8 {
        match self {
            Self::Replication => 0xE6,
            Self::Witness => 0xE7,
            Self::DeliveredInput => 0xE8,
            Self::ReplicationCompressed => 0xE9,
            Self::WitnessCompressed => 0xEA,
            Self::ReplicationDelta => 0xEB,
            Self::Hit => 0xEC,
            Self::WitnessKeyframe => 0xED,
        }
    }

    /// Recover a known family from its inner logical channel and sub-tag.
    ///
    /// This deliberately reads the inner frame rather than the transport that
    /// happened to carry it.  In particular, a delivered input remains a
    /// delivered input when a future transport moves it to a datagram.
    #[must_use]
    pub const fn from_frame(channel: Channel, tag: u8) -> Option<Self> {
        match (channel, tag) {
            (Channel::State, 0xE6) => Some(Self::Replication),
            (Channel::State, 0xE9) => Some(Self::ReplicationCompressed),
            (Channel::State, 0xEB) => Some(Self::ReplicationDelta),
            (Channel::State, 0xED) => Some(Self::WitnessKeyframe),
            (Channel::State, 0xE7) => Some(Self::Witness),
            (Channel::State, 0xEA) => Some(Self::WitnessCompressed),
            (Channel::Control, 0xE8) => Some(Self::DeliveredInput),
            (Channel::State, 0xEC) => Some(Self::Hit),
            _ => None,
        }
    }
}

/// Sub-tag marking a state datagram as replication traffic.
///
/// Replication and witness traffic share `Channel::State`, so both have to be
/// positively identified: tagging only one leaves the other as "everything
/// else", and a receiver still hands foreign bytes to a decoder that reads
/// length prefixes out of them.
pub const TAG_REPLICATION: u8 = WireFamily::Replication.tag();

/// Compressed sibling of [`TAG_REPLICATION`].
///
/// The body is a DEFLATE stream with its uncompressed `u32` length prepended. A
/// separate tag keeps the existing uncompressed wire readable during a
/// rolling upgrade and lets receivers reject an oversized declaration before
/// allocating its output buffer.
pub const TAG_REPLICATION_COMPRESSED: u8 = WireFamily::ReplicationCompressed.tag();

/// Sub-tag marking a keyframe-referenced replication delta.
///
/// The postcard envelope is `(entity, tick, keyframe_age, cell, patch)`. The
/// patch is a skip/write program over the canonical bytes of the referenced
/// keyframe; see [`encode_delta_patch`] and [`apply_delta_patch`].
pub const TAG_REPLICATION_DELTA: u8 = WireFamily::ReplicationDelta.tag();

/// Sub-tag for an absolute replication keyframe on a witness-set link.
///
/// A witness needs the anchor that makes subsequent deltas meaningful.  A20
/// measured 89 false positives at 500 kbps when those anchors could be shed,
/// so this is a distinct wire family rather than caller-declared priority.
/// Receivers decode its body exactly like [`TAG_REPLICATION`].
pub const TAG_WITNESS_KEYFRAME: u8 = WireFamily::WitnessKeyframe.tag();

/// The decoded envelope of one keyframe-referenced replication delta.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplicationDelta {
    /// Entity whose canonical state is patched.
    pub entity: PersistId,
    /// Universe tick of the current state.
    pub tick: u64,
    /// Number of ticks back to the referenced keyframe.
    pub keyframe_age: u16,
    /// New committed cell, present only when it changed since the keyframe.
    pub cell: Option<CellId>,
    /// Skip/write patch over the keyframe's canonical bytes.
    pub patch: Vec<u8>,
}

/// Encode replication traffic as a state datagram.
pub fn encode_replication<T: Serialize>(msg: &T) -> Vec<u8> {
    let mut payload = Vec::with_capacity(64);
    payload.push(TAG_REPLICATION);
    payload.extend_from_slice(&postcard::to_stdvec(msg).expect("wire message is serializable"));
    tag(Channel::State, &payload)
}

/// Encode an absolute replication keyframe for a witness-set link.
///
/// The payload is the ordinary replication postcard body under
/// [`TAG_WITNESS_KEYFRAME`], so a receiver uses the same replication decoder.
/// Its distinct family makes the upload meter preserve this adjudication
/// anchor under pressure without accepting a caller-supplied lane field.
#[must_use]
pub fn encode_witness_keyframe<T: Serialize>(msg: &T) -> Vec<u8> {
    let mut payload = Vec::with_capacity(64);
    payload.push(TAG_WITNESS_KEYFRAME);
    payload.extend_from_slice(&postcard::to_stdvec(msg).expect("wire message is serializable"));
    tag(Channel::State, &payload)
}

/// Encode replication traffic, using bounded block compression when it makes
/// the complete state datagram smaller.
///
/// Small or incompressible messages retain the ordinary
/// [`TAG_REPLICATION`] form, so compression never increases wire use.
#[must_use]
pub fn encode_replication_compressed<T: Serialize>(msg: &T) -> Vec<u8> {
    encode_sub_tagged_compressed(msg, TAG_REPLICATION, TAG_REPLICATION_COMPRESSED)
}

/// Encode a keyframe-referenced replication delta, falling back to the
/// absolute keyframe whenever the delta datagram would not be smaller.
///
/// `absolute` is the ordinary current-state replication message that the
/// delta would replace. Comparing the complete encoded datagrams keeps the
/// same smaller-only convention as [`encode_replication_compressed`], including
/// any DEFLATE win available to the absolute form.
#[must_use]
pub fn encode_replication_delta<T: Serialize>(absolute: &T, delta: &ReplicationDelta) -> Vec<u8> {
    let keyframe = encode_replication_compressed(absolute);
    let candidate = encode_replication_delta_candidate(delta);
    if candidate.len() < keyframe.len() {
        candidate
    } else {
        keyframe
    }
}

/// Decode a keyframe-referenced replication delta from a state datagram.
///
/// Returns `None` for every other state sub-tag and for malformed or oversized
/// envelopes. The peer-declared patch length is checked before allocating the
/// returned [`ReplicationDelta::patch`].
#[must_use]
pub fn decode_replication_delta(payload: &[u8]) -> Option<ReplicationDelta> {
    let (channel, rest) = untag(payload)?;
    if channel != Channel::State || rest.first() != Some(&TAG_REPLICATION_DELTA) {
        return None;
    }

    let body = rest.get(1..)?;
    let (entity, body) = postcard::take_from_bytes::<PersistId>(body).ok()?;
    let (tick, body) = postcard::take_from_bytes::<u64>(body).ok()?;
    let (keyframe_age, body) = postcard::take_from_bytes::<u16>(body).ok()?;
    let (cell, body) = postcard::take_from_bytes::<Option<CellId>>(body).ok()?;
    let (declared, patch) = postcard::take_from_bytes::<usize>(body).ok()?;
    let declared = accept_declared_len(declared)?;
    if patch.len() != declared {
        return None;
    }

    Some(ReplicationDelta {
        entity,
        tick,
        keyframe_age,
        cell,
        patch: patch.to_vec(),
    })
}

/// Encode `current` as a skip/write patch against `keyframe`.
///
/// The body is `new_len:varint` followed by `(skip:varint,
/// write_len:varint, literals...)` operations. Skips copy keyframe bytes at
/// the same offset. Bytes beyond the keyframe length are always literals, so
/// growing canonical states (including v18's filling craft trail) reconstruct
/// exactly. One-byte equal gaps are folded into a literal run because writing
/// one byte is cheaper than another pair of run headers.
#[must_use]
pub fn encode_delta_patch(keyframe: &[u8], current: &[u8]) -> Vec<u8> {
    let new_len = u32::try_from(current.len()).expect("canonical state length fits in u32");
    let mut patch = Vec::with_capacity(current.len().saturating_add(8));
    push_varint(new_len, &mut patch);

    let mut offset = 0usize;
    while offset < current.len() {
        let skip_start = offset;
        while offset < current.len()
            && keyframe
                .get(offset)
                .is_some_and(|byte| *byte == current[offset])
        {
            offset += 1;
        }
        let skip = offset - skip_start;
        let write_start = offset;

        while offset < current.len() {
            if keyframe
                .get(offset)
                .is_none_or(|byte| *byte != current[offset])
            {
                offset += 1;
                continue;
            }

            let equal_start = offset;
            while offset < current.len()
                && keyframe
                    .get(offset)
                    .is_some_and(|byte| *byte == current[offset])
            {
                offset += 1;
            }
            let equal_len = offset - equal_start;
            if offset == current.len() || equal_len > 1 {
                offset = equal_start;
                break;
            }
        }

        let write_len = offset - write_start;
        push_varint(
            u32::try_from(skip).expect("canonical state length fits in u32"),
            &mut patch,
        );
        push_varint(
            u32::try_from(write_len).expect("canonical state length fits in u32"),
            &mut patch,
        );
        patch.extend_from_slice(&current[write_start..offset]);
    }

    patch
}

/// Apply a skip/write `patch` to canonical `keyframe` bytes.
///
/// Malformed programs, non-canonical varints, skips past the keyframe, trailing
/// bytes, and output sizes above [`MAX_RELIABLE_MESSAGE_BYTES`] are refused.
/// The declared output bound is checked before allocating its buffer.
#[must_use]
pub fn apply_delta_patch(keyframe: &[u8], patch: &[u8]) -> Option<Vec<u8>> {
    let (declared, mut operations) = take_varint(patch)?;
    let new_len = accept_declared_len(usize::try_from(declared).ok()?)?;
    let mut output = Vec::with_capacity(new_len);

    while output.len() < new_len {
        let (skip, rest) = take_varint(operations)?;
        let (write_len, rest) = take_varint(rest)?;
        let skip = usize::try_from(skip).ok()?;
        let write_len = usize::try_from(write_len).ok()?;
        if skip == 0 && write_len == 0 {
            return None;
        }

        let after_skip = output.len().checked_add(skip)?;
        if after_skip > new_len || after_skip > keyframe.len() {
            return None;
        }
        output.extend_from_slice(keyframe.get(output.len()..after_skip)?);

        let after_write = output.len().checked_add(write_len)?;
        if after_write > new_len {
            return None;
        }
        let (literals, rest) = rest.split_at_checked(write_len)?;
        output.extend_from_slice(literals);
        operations = rest;
    }

    operations.is_empty().then_some(output)
}

fn encode_replication_delta_candidate(delta: &ReplicationDelta) -> Vec<u8> {
    let body = postcard::to_stdvec(&(
        delta.entity,
        delta.tick,
        delta.keyframe_age,
        delta.cell,
        delta.patch.as_slice(),
    ))
    .expect("wire message is serializable");
    let mut payload = Vec::with_capacity(body.len().saturating_add(1));
    payload.push(TAG_REPLICATION_DELTA);
    payload.extend_from_slice(&body);
    tag(Channel::State, &payload)
}

fn push_varint(mut value: u32, out: &mut Vec<u8>) {
    loop {
        let low = u8::try_from(value & 0x7f).expect("seven bits fit in u8");
        value >>= 7;
        if value == 0 {
            out.push(low);
            return;
        }
        out.push(low | 0x80);
    }
}

fn take_varint(bytes: &[u8]) -> Option<(u32, &[u8])> {
    let mut value = 0u32;
    for (index, byte) in bytes.iter().copied().take(5).enumerate() {
        let shift = u32::try_from(index).ok()?.checked_mul(7)?;
        let low = u32::from(byte & 0x7f);
        if low > (u32::MAX >> shift) {
            return None;
        }
        let chunk = low << shift;
        value = value.checked_add(chunk)?;
        if byte & 0x80 == 0 {
            let consumed = index.checked_add(1)?;
            let canonical_len = if value == 0 {
                1
            } else {
                usize::try_from((32 - value.leading_zeros()).div_ceil(7)).ok()?
            };
            return (canonical_len == consumed).then_some((value, bytes.get(consumed..)?));
        }
    }
    None
}

/// Decode replication traffic from a state datagram.
pub fn decode_replication<T: DeserializeOwned>(payload: &[u8]) -> Option<T> {
    let (channel, rest) = untag(payload)?;
    if channel != Channel::State {
        return None;
    }
    let (marker, body) = rest.split_first()?;
    if *marker == TAG_REPLICATION || *marker == TAG_WITNESS_KEYFRAME {
        return postcard::from_bytes(body).ok();
    }
    decode_sub_tagged(payload, TAG_REPLICATION, TAG_REPLICATION_COMPRESSED)
}

/// Shared body of the sub-tagged state decoders.
fn decode_sub_tagged<T: DeserializeOwned>(
    payload: &[u8],
    plain_tag: u8,
    compressed_tag: u8,
) -> Option<T> {
    let (channel, rest) = untag(payload)?;
    if channel != Channel::State {
        return None;
    }
    let (marker, body) = rest.split_first()?;
    if *marker == plain_tag {
        return postcard::from_bytes(body).ok();
    }
    if *marker != compressed_tag {
        return None;
    }
    let declared = accept_declared_len(
        usize::try_from(u32::from_le_bytes(body.get(..4)?.try_into().ok()?)).ok()?,
    )?;
    let compressed = body.get(4..)?;
    let mut decoder = DeflateDecoder::new(compressed);
    let mut decoded = Vec::with_capacity(declared);
    decoder
        .by_ref()
        .take(u64::try_from(declared).ok()?.saturating_add(1))
        .read_to_end(&mut decoded)
        .ok()?;
    if decoded.len() != declared {
        return None;
    }
    postcard::from_bytes(&decoded).ok()
}

/// Accept an untrusted peer's declared allocation size, or refuse it.
///
/// Checked **before** either the compressed decoder or delta patch applier's
/// `Vec::with_capacity`, and that ordering is the whole point: the declaration
/// is chosen by an untrusted peer, so an unbounded one would have us reserve up
/// to 4 GiB before a single byte of output is examined. A later length mismatch
/// rejects the same envelope, but only after the allocation has happened --
/// which is why that check cannot stand in for this one.
fn accept_declared_len(declared: usize) -> Option<usize> {
    (declared <= MAX_RELIABLE_MESSAGE_BYTES).then_some(declared)
}

/// Serialize once and keep the compressed form only when it is smaller.
fn encode_sub_tagged_compressed<T: Serialize>(
    msg: &T,
    plain_tag: u8,
    compressed_tag: u8,
) -> Vec<u8> {
    let plain = postcard::to_stdvec(msg).expect("wire message is serializable");
    let mut encoder = DeflateEncoder::new(Vec::new(), Compression::new(6));
    encoder.write_all(&plain).expect("Vec writes cannot fail");
    let deflated = encoder.finish().expect("Vec writes cannot fail");
    let mut compressed = Vec::with_capacity(deflated.len() + 4);
    compressed.extend_from_slice(
        &u32::try_from(plain.len())
            .expect("wire message is bounded to u32")
            .to_le_bytes(),
    );
    compressed.extend_from_slice(&deflated);
    let (marker, body) = if compressed.len() < plain.len() {
        (compressed_tag, compressed)
    } else {
        (plain_tag, plain)
    };
    let mut payload = Vec::with_capacity(body.len() + 1);
    payload.push(marker);
    payload.extend_from_slice(&body);
    tag(Channel::State, &payload)
}

/// Sub-tag marking a state datagram as verifiable-core traffic.
///
/// Replication payloads and witness records share `Channel::State` — docs/03
/// §5.3 has log records riding *in the same datagrams* at low priority — so the
/// channel tag alone cannot say which is which. Without a discriminator every
/// receiver attempts to parse every replication datagram as a `LogFrame`, and
/// postcard reads a length prefix out of unrelated bytes before it can fail:
/// slow at best, and an allocation the sender chooses at worst.
///
/// Its state-lane sibling is [`TAG_REPLICATION`]: both kinds are tagged, so a
/// receiver never hands foreign bytes to a decoder that would read a length
/// prefix out of them. Reliable delivered inputs have their own control-lane
/// [`TAG_DELIVERED_INPUT`] discriminator.
pub const TAG_WITNESS: u8 = WireFamily::Witness.tag();

/// Compressed sibling of [`TAG_WITNESS`], with the same bounded DEFLATE envelope
/// as [`TAG_REPLICATION_COMPRESSED`].
pub const TAG_WITNESS_COMPRESSED: u8 = WireFamily::WitnessCompressed.tag();

/// Sub-tag marking a reliable, addressed core input produced by
/// `Game::deliver` from another authority's outcome.
///
/// The payload is `[TAG_DELIVERED_INPUT][from u64 LE][recipient u64 LE]
/// [canonical input]`. The input bytes belong to the negotiated ruleset; this
/// envelope owns only routing/provenance and deliberately does not invent a
/// second command schema.
pub const TAG_DELIVERED_INPUT: u8 = WireFamily::DeliveredInput.tag();

/// One delivered core input addressed to the authority of `recipient`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeliveredInput {
    /// Entity whose authoritative outcome produced the input.
    pub from: PersistId,
    /// Entity whose authority may apply the input.
    pub recipient: PersistId,
    /// The ruleset's canonical `CoreInput` bytes.
    pub input: Vec<u8>,
}

/// Encode one addressed delivered input on the reliable control channel.
///
/// Cross-entity effects are canonical inputs, not replication snapshots. They
/// therefore use the reliable shared stream and retain emission/arrival order.
#[must_use]
pub fn encode_delivered_input(from: PersistId, recipient: PersistId, input: &[u8]) -> Vec<u8> {
    let mut payload = Vec::with_capacity(17 + input.len());
    payload.push(TAG_DELIVERED_INPUT);
    payload.extend_from_slice(&from.0.to_le_bytes());
    payload.extend_from_slice(&recipient.0.to_le_bytes());
    payload.extend_from_slice(input);
    tag(Channel::Control, &payload)
}

/// Decode one addressed delivered input, refusing every other channel member.
#[must_use]
pub fn decode_delivered_input(payload: &[u8]) -> Option<DeliveredInput> {
    let (channel, rest) = untag(payload)?;
    if channel != Channel::Control || rest.first() != Some(&TAG_DELIVERED_INPUT) {
        return None;
    }
    let from = rest.get(1..9)?;
    let recipient = rest.get(9..17)?;
    let input = rest.get(17..)?;
    Some(DeliveredInput {
        from: PersistId::new(u64::from_le_bytes(from.try_into().ok()?)),
        recipient: PersistId::new(u64::from_le_bytes(recipient.try_into().ok()?)),
        input: input.to_vec(),
    })
}

/// Encode verifiable-core traffic as a state datagram.
pub fn encode_witness<T: Serialize>(msg: &T) -> Vec<u8> {
    let mut payload = Vec::with_capacity(64);
    payload.push(TAG_WITNESS);
    payload.extend_from_slice(&postcard::to_stdvec(msg).expect("wire message is serializable"));
    tag(Channel::State, &payload)
}

/// Encode verifiable-core traffic, using bounded block compression when it
/// makes the complete message smaller.
///
/// This is intended for multi-tick log frames. Claims and other small messages
/// normally retain [`TAG_WITNESS`] because the compressed form would not win.
#[must_use]
pub fn encode_witness_compressed<T: Serialize>(msg: &T) -> Vec<u8> {
    encode_sub_tagged_compressed(msg, TAG_WITNESS, TAG_WITNESS_COMPRESSED)
}

/// Decode verifiable-core traffic from a state datagram.
///
/// Returns `None` for anything carrying neither [`TAG_WITNESS`] nor
/// [`TAG_WITNESS_COMPRESSED`], *before* handing bytes to postcard — which is
/// the point.
pub fn decode_witness<T: DeserializeOwned>(payload: &[u8]) -> Option<T> {
    decode_sub_tagged(payload, TAG_WITNESS, TAG_WITNESS_COMPRESSED)
}

/// Sub-tag marking hit-registration traffic (docs/05 §7): a [`HitMsg`]
/// claim from a shooter, or the verdict back. Rides the unreliable state
/// channel; the shooter resends a claim until a verdict names its key.
pub const TAG_HIT: u8 = WireFamily::Hit.tag();

/// Encode one hit claim or verdict as a state datagram:
/// `[TAG_STATE][TAG_HIT][postcard]`.
#[must_use]
pub fn encode_hit(msg: &HitMsg) -> Vec<u8> {
    let mut payload = Vec::with_capacity(64);
    payload.push(TAG_HIT);
    payload.extend_from_slice(&postcard::to_stdvec(msg).expect("wire message is serializable"));
    tag(Channel::State, &payload)
}

/// Decode one hit claim or verdict, refusing every other state-channel member
/// before handing bytes to postcard.
#[must_use]
pub fn decode_hit(payload: &[u8]) -> Option<HitMsg> {
    let (channel, rest) = untag(payload)?;
    if channel != Channel::State || rest.first() != Some(&TAG_HIT) {
        return None;
    }
    postcard::from_bytes(&rest[1..]).ok()
}

/// Encode a message as a **state** datagram: `[TAG_STATE][postcard]`.
///
/// Used for bulk diffs and their acks (D11 §2.1). Both directions share this
/// encoding.
///
/// # Panics
///
/// Panics if `msg` is not postcard-serializable.
#[must_use]
pub fn encode_datagram<T: Serialize>(msg: &T) -> Vec<u8> {
    let payload = postcard::to_stdvec(msg).expect("wire message is serializable");
    tag(Channel::State, &payload)
}

/// Decode a **state** datagram into `T`.
///
/// Returns `None` if the tag is not the state channel or the payload does not
/// decode.
pub fn decode_datagram<T: DeserializeOwned>(payload: &[u8]) -> Option<T> {
    let (channel, rest) = untag(payload)?;
    if channel != Channel::State {
        return None;
    }
    postcard::from_bytes(rest).ok()
}

/// Encode a message as a **stream/control** frame: `[TAG_CONTROL][u32 LE
/// length][postcard]`.
///
/// Reliable-stream traffic is length-prefixed so one channel can carry many
/// messages, and tagged so the receiver can route it without a separate
/// framing layer. Both directions share this encoding. See the [module
/// docs](self#why-the-control-payload-keeps-its-own-length-prefix) for why the
/// prefix survives the move onto a lane that already frames.
///
/// # Panics
///
/// Panics if `msg` is not postcard-serializable.
#[must_use]
pub fn encode_stream_frame<T: Serialize>(msg: &T) -> Vec<u8> {
    let payload = postcard::to_stdvec(msg).expect("wire message is serializable");
    let len = u32::try_from(payload.len()).expect("stream frame fits in u32");
    let mut out = Vec::with_capacity(payload.len() + 5);
    out.push(TAG_CONTROL);
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(&payload);
    out
}

/// Decode a **stream/control** frame into `T`.
///
/// Returns `None` if the tag is not the control channel, the frame is
/// malformed, or the payload does not decode.
pub fn decode_stream_frame<T: DeserializeOwned>(payload: &[u8]) -> Option<T> {
    let (channel, rest) = untag(payload)?;
    if channel != Channel::Control {
        return None;
    }
    let len = usize::try_from(u32::from_le_bytes(rest.get(..4)?.try_into().ok()?)).ok()?;
    let frame = rest.get(4..4 + len)?;
    postcard::from_bytes(frame).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tag_untag_roundtrip() {
        let payload = b"state-bytes";
        let tagged = tag(Channel::State, payload);
        assert_eq!(untag(&tagged), Some((Channel::State, payload.as_slice())));
        let tagged = tag(Channel::Control, payload);
        assert_eq!(untag(&tagged), Some((Channel::Control, payload.as_slice())));
        assert_eq!(untag(&[]), None);
    }

    #[test]
    fn datagram_frame_roundtrips() {
        let msg = crate::GatewayMsg::Hello {
            token: b"tok".to_vec(),
            node: crate::NodeId::from_bytes(&[3u8; 32]).unwrap(),
        };
        let bytes = encode_datagram(&msg);
        assert_eq!(bytes[0], TAG_STATE);
        let back: crate::GatewayMsg = decode_datagram(&bytes).unwrap();
        assert_eq!(back, msg);
        // A control frame does not decode as a datagram.
        assert!(decode_datagram::<crate::GatewayMsg>(&encode_stream_frame(&msg)).is_none());
    }

    #[test]
    fn stream_frame_is_length_prefixed_and_tagged() {
        let msg = crate::GatewayReply::HelloAck {
            gateway: crate::NodeId::from_bytes(&[3u8; 32]).unwrap(),
            protocol: 1,
        };
        let frame = encode_stream_frame(&msg);
        assert_eq!(frame[0], TAG_CONTROL);
        let len = u32::from_le_bytes(frame[1..5].try_into().unwrap()) as usize;
        assert_eq!(len, frame.len() - 5);
        let back: crate::GatewayReply = decode_stream_frame(&frame).unwrap();
        assert_eq!(back, msg);
        // A datagram does not decode as a stream frame.
        assert!(decode_stream_frame::<crate::GatewayReply>(&encode_datagram(&msg)).is_none());
    }

    #[test]
    fn delivered_input_roundtrips_and_is_not_replication() {
        let delivered =
            encode_delivered_input(PersistId::new(7), PersistId::new(42), b"canonical order");
        assert_eq!(
            decode_delivered_input(&delivered),
            Some(DeliveredInput {
                from: PersistId::new(7),
                recipient: PersistId::new(42),
                input: b"canonical order".to_vec(),
            })
        );
        assert!(decode_replication::<Vec<u8>>(&delivered).is_none());
        assert!(decode_delivered_input(&encode_datagram(&42u64)).is_none());
    }

    #[test]
    fn compressed_state_subtags_roundtrip_and_never_expand_small_messages() {
        let repetitive = vec![0x5a; 4_096];
        let replication = encode_replication_compressed(&repetitive);
        let (_, replication_body) = untag(&replication).expect("state tag");
        assert_eq!(replication_body[0], TAG_REPLICATION_COMPRESSED);
        assert_eq!(
            decode_replication::<Vec<u8>>(&replication),
            Some(repetitive.clone())
        );

        let witness = encode_witness_compressed(&repetitive);
        let (_, witness_body) = untag(&witness).expect("state tag");
        assert_eq!(witness_body[0], TAG_WITNESS_COMPRESSED);
        assert_eq!(decode_witness::<Vec<u8>>(&witness), Some(repetitive));

        let small = encode_witness_compressed(&7u8);
        let (_, small_body) = untag(&small).expect("state tag");
        assert_eq!(small_body[0], TAG_WITNESS);
        assert_eq!(decode_witness::<u8>(&small), Some(7));
    }

    #[test]
    fn a_witness_keyframe_is_replication_with_a_distinct_wire_family() {
        let keyframe = encode_witness_keyframe(&vec![0x5au8; 64]);
        let (channel, body) = untag(&keyframe).expect("state tag");
        assert_eq!(channel, Channel::State);
        assert_eq!(body.first(), Some(&TAG_WITNESS_KEYFRAME));
        assert_eq!(
            WireFamily::from_frame(channel, TAG_WITNESS_KEYFRAME),
            Some(WireFamily::WitnessKeyframe)
        );
        assert_eq!(
            decode_replication::<Vec<u8>>(&keyframe),
            Some(vec![0x5a; 64])
        );
    }

    /// The declared-size bound, pinned on its own.
    ///
    /// `compressed_state_decoder_rejects_oversized_or_truncated_envelopes`
    /// does not cover it: with the bound deleted that test still passes,
    /// because the trailing `decoded.len() != declared` mismatch rejects the
    /// same envelope -- after the oversized allocation the bound exists to
    /// `apply_delta_patch` refuses an oversized declared output length.
    ///
    /// The bound is in the code, but nothing pinned it: removing
    /// `accept_declared_len` from the patch decoder left all 133 tests green.
    /// That is the identical gap #649 shipped on the DEFLATE decoder -- a
    /// later structural check rejects the same message, but only after
    /// `Vec::with_capacity` has reserved on a stranger's varint. The
    /// allocation is the hazard, so the predicate is asserted where it is made.
    #[test]
    fn a_delta_patch_declaring_an_oversized_output_is_refused_before_allocating() {
        /// Little-endian base-128, canonical length, matching `take_varint`.
        fn varint(mut value: u32) -> Vec<u8> {
            let mut out = Vec::new();
            loop {
                let byte = u8::try_from(value & 0x7f).expect("seven bits");
                value >>= 7;
                if value == 0 {
                    out.push(byte);
                    return out;
                }
                out.push(byte | 0x80);
            }
        }

        let keyframe = vec![0u8; 8];
        let over = u32::try_from(MAX_RELIABLE_MESSAGE_BYTES).expect("cap fits u32") + 1;

        // A patch that would SUCCEED if the bound were absent: skip nothing,
        // write `over` literals. Refusing it therefore proves the cap ran,
        // not merely that some later structural check rejected the message.
        // An envelope that fails either way proves nothing -- which is how
        // #649's oversize test came to pass with its bound deleted.
        let mut hostile = varint(over);
        hostile.extend_from_slice(&varint(0));
        hostile.extend_from_slice(&varint(over));
        hostile.extend(std::iter::repeat_n(0xAB, over as usize));

        let well_formed_if_unbounded = {
            let (declared, ops) = take_varint(&hostile).expect("varint header");
            let (skip, rest) = take_varint(ops).expect("skip");
            let (write, rest) = take_varint(rest).expect("write");
            declared == over && skip == 0 && write == over && rest.len() == over as usize
        };
        assert!(
            well_formed_if_unbounded,
            "the hostile patch must be structurally valid, or the refusal \
             below would prove nothing about the cap"
        );

        assert_eq!(
            apply_delta_patch(&keyframe, &hostile),
            None,
            "a peer declaring {over} output bytes must be refused before that \
             capacity is reserved, even though the patch is otherwise well formed"
        );
    }

    /// prevent. Asserting the predicate directly is the only way to hold it.
    #[test]
    fn a_declared_plaintext_size_above_the_cap_is_refused_before_allocating() {
        assert_eq!(
            accept_declared_len(MAX_RELIABLE_MESSAGE_BYTES),
            Some(MAX_RELIABLE_MESSAGE_BYTES),
            "the cap itself is admissible"
        );
        assert_eq!(
            accept_declared_len(MAX_RELIABLE_MESSAGE_BYTES + 1),
            None,
            "one byte over the cap is refused"
        );
        assert_eq!(
            accept_declared_len(usize::try_from(u32::MAX).expect("u32 fits usize")),
            None,
            "a peer declaring the largest encodable size must not have it \
             reserved on its say-so"
        );
    }

    #[test]
    fn compressed_state_decoder_rejects_oversized_or_truncated_envelopes() {
        let mut oversized = vec![TAG_WITNESS_COMPRESSED];
        oversized.extend_from_slice(
            &u32::try_from(MAX_RELIABLE_MESSAGE_BYTES + 1)
                .unwrap()
                .to_le_bytes(),
        );
        oversized.extend_from_slice(&[0x03, 0x00]);
        assert!(decode_witness::<Vec<u8>>(&tag(Channel::State, &oversized)).is_none());

        let mut truncated = vec![TAG_REPLICATION_COMPRESSED];
        truncated.extend_from_slice(&128u32.to_le_bytes());
        truncated.extend_from_slice(&[0x03, 0x00]);
        assert!(decode_replication::<Vec<u8>>(&tag(Channel::State, &truncated)).is_none());
    }

    #[test]
    fn hit_frame_roundtrips_and_refuses_other_sub_tags() {
        use crate::{
            HitClaim, HitSurface, InterpBasis, LatticePoint, QuantizedDir, QuantizedRay, Tick,
            WeaponRef,
        };
        let msg = HitMsg::Claim(HitClaim {
            shooter: PersistId::new(1),
            target: PersistId::new(2),
            weapon: WeaponRef(1),
            fire_tick: Tick::new(100),
            basis: InterpBasis::exact(Tick::new(95)),
            ray: QuantizedRay {
                origin: LatticePoint::new(0, 0, 0),
                direction: QuantizedDir::new(1, 0, 0),
            },
            claimed: HitSurface(0),
            input_seq: 1,
        });
        let bytes = encode_hit(&msg);
        assert_eq!(bytes[0], TAG_STATE);
        assert_eq!(bytes[1], TAG_HIT);
        assert_eq!(decode_hit(&bytes), Some(msg));

        // The same postcard body under the witness sub-tag is not a hit.
        let mut witness = vec![TAG_WITNESS];
        witness.extend_from_slice(&bytes[2..]);
        assert_eq!(decode_hit(&tag(Channel::State, &witness)), None);
        // And a control frame is never one either.
        assert_eq!(decode_hit(&tag(Channel::Control, &bytes[1..])), None);
    }
}
