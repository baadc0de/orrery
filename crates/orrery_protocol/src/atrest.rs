//! At-rest schema versioning: the bootstrap rule and the version trailer.
//!
//! This module is the **single place** the `absent == v0` rule is written down
//! (D38 clause (d)(1)). Every long-lived at-rest family in this workspace —
//! `world/` component bags, journal logical records, the permanent `ledger/`
//! rows — is either self-describing or, where it is not yet, carries a
//! recorded reason beside its constructor.
//!
//! Normative source: [ADR-0038](https://github.com/baadc0de/orrery/blob/main/docs/adr/0038-at-rest-schema-versioning.md)
//! clause (d), [ADR-0011](https://github.com/baadc0de/orrery/blob/main/docs/adr/0011-persistence.md)
//! and docs/08-persistence.md §16.
//!
//! # The bootstrap rule
//!
//! **A value written without a version field is version 0.** Not unknown, not
//! rejected, not guessed from its shape — *zero*. Rows predating a family's
//! versioning are the oldest readable era of that family, and a migration
//! chain that starts at 0 walks them forward like any other row. The rule is
//! stated once, here, because the alternative — each family inventing its own
//! answer as it grows a version field — is how "what schema is this?" becomes
//! unanswerable across families.
//!
//! Version domains are **orthogonal to
//! [`RulesetId::version`](https://docs.rs/orrery_core)** (D38 clause (d)(3)):
//! a rules hotfix bumps no schema, a schema bump ships without a rules change,
//! and neither number is ever derived from the other.
//!
//! # Two version widths, two questions
//!
//! - [`SchemaVersion`] (u32) answers *"what shape are these game bytes?"*. It
//!   is allocated by the game per `ComponentTypeId`, monotone, never reused
//!   and never gapped within a type (D38 clause (d)(3)).
//! - [`EncodingVersion`] (u8) answers *"what framing does this record use?"*.
//!   It is allocated by this workspace, for the server-owned envelopes below.
//!
//! # The version trailer
//!
//! [`encode_versioned`] appends one [`EncodingVersion`] byte **after** a
//! positionally-encoded postcard body, and [`decode_versioned`] recovers it:
//!
//! ```text
//!     versioned    postcard(T) ‖ version:u8
//!     bootstrap    postcard(T)                 -> version 0
//! ```
//!
//! A trailer rather than a header, and rather than a field on `T`, because
//! only the trailer keeps the bootstrap rule *decidable*:
//!
//! - **A field on `T`** shifts every byte after it. postcard is positional and
//!   `postcard::from_bytes` refuses trailing bytes, so an unversioned row does
//!   not decode-and-default — it fails outright (the `attest/`/`enforced`
//!   episode, `keyspace.rs`). That rejects old bytes rather than reading them
//!   as v0, which is precisely what the bootstrap rule forbids.
//! - **A header byte** is ambiguous: the first byte of a postcard body is a
//!   varint that can take any value, so no header value distinguishes a
//!   versioned row from an unversioned one. Deciding by trial decode is the
//!   guessing this module exists to eliminate.
//! - **A trailer** is decidable by construction. postcard consumes an exact
//!   number of bytes for a given type, so what remains after
//!   [`postcard::take_from_bytes`] is framing, not payload: exactly one byte
//!   means a version, zero bytes means v0, anything else is corruption.
//!
//! D38 clause (d)(5) offered two mechanisms for the journal half — a header
//! field on the record, or a per-`RecordKind` payload prefix — and deferred
//! the choice. This is a third, chosen for the reason above; what the record
//! does *not* defer is satisfied unchanged: the version travels with the
//! record, and no physical envelope is asked to answer for it.

use serde::de::DeserializeOwned;
use serde::Serialize;

/// A game-allocated schema version for one component type (D38 clause (d)(3)).
///
/// Per `ComponentTypeId`, monotone, never reused or gapped within a type.
/// Orthogonal to `RulesetId::version`, which answers a different question.
pub type SchemaVersion = u32;

/// The schema version of bytes written before their family carried one.
///
/// The bootstrap rule of this module, as a constant so a reader of a call site
/// sees the rule rather than a bare `0`.
pub const SCHEMA_V0: SchemaVersion = 0;

/// A server-allocated framing version for one at-rest envelope.
pub type EncodingVersion = u8;

/// The encoding version of bytes written before their envelope carried one.
pub const ENCODING_V0: EncodingVersion = 0;

/// A version trailer that is neither present nor absent — more bytes follow
/// the body than a version could occupy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VersionedError {
    /// The body itself did not decode.
    Body(postcard::Error),
    /// The body decoded, but `trailing` bytes followed it. A version trailer
    /// is exactly one byte; anything longer is a corrupt or foreign value.
    Trailing(usize),
}

impl core::fmt::Display for VersionedError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Body(e) => write!(f, "decode versioned body: {e}"),
            Self::Trailing(n) => write!(
                f,
                "{n} bytes follow the decoded body; a version trailer is one byte"
            ),
        }
    }
}

impl core::error::Error for VersionedError {}

/// Encode `body` with its `version` trailer.
///
/// # Errors
///
/// Returns the postcard error if `body` does not serialize.
pub fn encode_versioned<T: Serialize>(
    body: &T,
    version: EncodingVersion,
) -> Result<Vec<u8>, postcard::Error> {
    let mut bytes = postcard::to_stdvec(body)?;
    bytes.push(version);
    Ok(bytes)
}

/// Decode a body and its version, applying the bootstrap rule.
///
/// Bytes with no trailer decode as [`ENCODING_V0`] — the rule this module
/// exists to state — rather than being rejected.
///
/// # Errors
///
/// [`VersionedError::Body`] if the body does not decode, or
/// [`VersionedError::Trailing`] if more than one byte follows it.
pub fn decode_versioned<T: DeserializeOwned>(
    bytes: &[u8],
) -> Result<(T, EncodingVersion), VersionedError> {
    let (body, rest) = postcard::take_from_bytes::<T>(bytes).map_err(VersionedError::Body)?;
    match rest {
        [] => Ok((body, ENCODING_V0)),
        [version] => Ok((body, *version)),
        more => Err(VersionedError::Trailing(more.len())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, PartialEq, Eq, Serialize, serde::Deserialize)]
    struct Row {
        account: u64,
        state: Vec<u8>,
    }

    fn row() -> Row {
        Row {
            account: 7,
            state: vec![1, 2, 3],
        }
    }

    #[test]
    fn genuinely_unversioned_bytes_read_as_v0() {
        // The bootstrap rule, on bytes written by a writer that predates it:
        // a bare postcard body, with no trailer anywhere in it.
        let legacy = postcard::to_stdvec(&row()).expect("encodes");
        let (back, version) = decode_versioned::<Row>(&legacy).expect("bootstraps");
        assert_eq!(back, row(), "the body survives the bootstrap unchanged");
        assert_eq!(
            version, ENCODING_V0,
            "absent == v0: unversioned bytes are v0, not rejected and not guessed"
        );
    }

    #[test]
    fn a_versioned_row_round_trips_with_its_version() {
        let encoded = encode_versioned(&row(), 3).expect("encodes");
        let (back, version) = decode_versioned::<Row>(&encoded).expect("decodes");
        assert_eq!(back, row());
        assert_eq!(version, 3);
    }

    #[test]
    fn the_trailer_costs_exactly_one_byte() {
        // D38 clause (f)'s arithmetic depends on this being one byte, not a
        // varint that grows with the number.
        let bare = postcard::to_stdvec(&row()).expect("encodes");
        for version in [0u8, 1, 127, 200, 255] {
            let encoded = encode_versioned(&row(), version).expect("encodes");
            assert_eq!(encoded.len(), bare.len() + 1);
            assert_eq!(
                decode_versioned::<Row>(&encoded).expect("decodes").1,
                version
            );
        }
    }

    #[test]
    fn a_longer_tail_is_corruption_not_a_version() {
        let mut encoded = encode_versioned(&row(), 1).expect("encodes");
        encoded.push(0xAB);
        assert_eq!(
            decode_versioned::<Row>(&encoded),
            Err(VersionedError::Trailing(2)),
            "two trailing bytes are not a version; the value is refused"
        );
    }
}
