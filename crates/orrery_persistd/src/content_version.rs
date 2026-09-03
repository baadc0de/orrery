//! The `content/version` row (docs/12-world-seeding.md §9.3), and the D38
//! trailer that lets it grow a field without breaking the rows already
//! written.
//!
//! # Why the record lives here and not in `orrery_seed`
//!
//! [`crate::keyspace::content_version_key`] is this crate's, and so is every
//! other statement about what a durable key holds. The seeder writes the row
//! and re-exports the type; `persistd` reads it at startup to check that the
//! seed it was handed opens the universe the cluster actually holds. Two
//! independently maintained mirrors of a positional postcard struct is exactly
//! the drift that silent misdecodes are made of.
//!
//! # Why a trailer and not a first-byte tag
//!
//! This row has an **untagged predecessor**: bytes written before this change
//! are a bare `postcard` body with no version anywhere in them. A leading tag
//! byte — the idiom `ramp/{control}` uses, where the family was tagged from
//! its first write — would be read by an old binary as the length varint of
//! `content_build`, so the old reader would half-decode a v1 row into a
//! plausible-looking record instead of failing. D38 clause (d)(1) names the
//! mechanism for precisely this case, and `orrery_protocol::atrest` states the
//! reason in as many words: "a header byte is ambiguous … a trailer is
//! decidable by construction."
//!
//! So the shape is
//!
//! ```text
//! postcard(ContentVersion) ‖ 0x01     a v1 row
//! postcard(ContentVersionV0)          a v0 row — absent trailer *is* the version
//! ```
//!
//! # What an old binary actually does with a v1 row
//!
//! Measured, not assumed — the spike expected a loud failure and that is not
//! what happens. `postcard::from_bytes` discards whatever follows the body it
//! decoded, so an old binary decodes a v1 row **successfully and correctly**:
//! the six fields it knows are byte-for-byte where they always were, and the
//! appended `Option` plus trailer are ignored. It loses the seal, which it has
//! no concept of, and corrupts nothing.
//!
//! The loud reading is available to anyone who wants it: a reader that
//! accounts for every byte (`decode_versioned`, which [`decode`] is built
//! from) refuses a v1 row as v0 rather than truncating it, and that refusal is
//! what makes the dispatch below decidable instead of a guess.
//!
//! A leading tag has neither property: it shifts every field, so the same old
//! reader either errors or produces a record that never existed. Both
//! behaviours are pinned by tests in this module.

use orrery_protocol::atrest::{
    decode_versioned, encode_versioned, EncodingVersion, VersionedError,
};
use orrery_protocol::UniverseSeedFingerprint;

/// The encoding version of a `content/version` row carrying
/// [`ContentVersion::universe_seed_fingerprint`].
pub const CONTENT_VERSION_ENCODING_V1: EncodingVersion = 1;

/// A content-version row.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ContentVersion {
    /// The content build id.
    pub content_build: String,
    /// The manifest digest.
    pub manifest_digest: String,
    /// The scenario seed used to derive the world.
    pub scenario_seed: String,
    /// A digest of the resolved scenario config.
    pub config_digest: String,
    /// The rustc version.
    pub toolchain: String,
    /// Wall-clock time the seeder wrote the world.
    pub seeded_at_ms: u64,
    /// Which universe this world belongs to — the one-way, domain-separated
    /// fingerprint of its VC-3 [`orrery_protocol::UniverseSeed`], never the
    /// seed itself (docs/08-persistence.md §6).
    ///
    /// `None` on a row written before the fingerprint existed, or by a seeding
    /// run that was not given one. An absent fingerprint is "unsealed", and a
    /// reader must warn and proceed rather than refuse: refusing on absent
    /// would brick every cluster seeded before this field.
    pub universe_seed_fingerprint: Option<UniverseSeedFingerprint>,
}

/// The six-field body written before the fingerprint existed.
///
/// Kept as its own type rather than reconstructed by hand at the decode site:
/// the bootstrap rule is "these exact fields, in this order, with nothing
/// after them", and a struct is the only way to say that to `postcard`.
#[derive(Debug, serde::Deserialize)]
struct ContentVersionV0 {
    content_build: String,
    manifest_digest: String,
    scenario_seed: String,
    config_digest: String,
    toolchain: String,
    seeded_at_ms: u64,
}

impl From<ContentVersionV0> for ContentVersion {
    fn from(v0: ContentVersionV0) -> Self {
        Self {
            content_build: v0.content_build,
            manifest_digest: v0.manifest_digest,
            scenario_seed: v0.scenario_seed,
            config_digest: v0.config_digest,
            toolchain: v0.toolchain,
            seeded_at_ms: v0.seeded_at_ms,
            universe_seed_fingerprint: None,
        }
    }
}

/// Encode a `content/version` row: the body, then its one-byte version.
///
/// # Errors
///
/// Returns the postcard error if the record does not serialize.
pub fn encode(record: &ContentVersion) -> Result<Vec<u8>, postcard::Error> {
    encode_versioned(record, CONTENT_VERSION_ENCODING_V1)
}

/// Decode a `content/version` row of either shape.
///
/// The two are told apart by the trailer, not guessed at: a v0 body that
/// consumes the whole value is v0 (the bootstrap rule), and a value with bytes
/// left over after a v0 body is a longer body, which is decoded as v1 and must
/// carry [`CONTENT_VERSION_ENCODING_V1`].
///
/// # Errors
///
/// Returns a human-readable message when the value decodes as neither shape,
/// or carries an encoding version this build does not know.
pub fn decode(bytes: &[u8]) -> Result<ContentVersion, String> {
    match decode_versioned::<ContentVersionV0>(bytes) {
        Ok((v0, orrery_protocol::atrest::ENCODING_V0)) => Ok(v0.into()),
        Ok((_, version)) => Err(format!(
            "content/version: a six-field body carries encoding version {version}, \
             which this build does not know"
        )),
        // The body ran longer than v0's six fields, so this is a later
        // encoding. Which one is a question only its trailer can answer.
        Err(VersionedError::Trailing(_)) => match decode_versioned::<ContentVersion>(bytes) {
            Ok((record, CONTENT_VERSION_ENCODING_V1)) => Ok(record),
            Ok((_, version)) => Err(format!(
                "content/version: unknown encoding version {version}; this build reads \
                 v0 (no trailer) and v{CONTENT_VERSION_ENCODING_V1}"
            )),
            Err(e) => Err(format!("decode content/version: {e}")),
        },
        Err(e) => Err(format!("decode content/version: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v1() -> ContentVersion {
        ContentVersion {
            content_build: "demo-2026-09-03".to_string(),
            manifest_digest: "d41a8b02".to_string(),
            scenario_seed: "smoke".to_string(),
            config_digest: "c0ffee".to_string(),
            toolchain: "rustc 1.96.0".to_string(),
            seeded_at_ms: 1_757_000_000_000,
            universe_seed_fingerprint: Some(UniverseSeedFingerprint([0x5a; 16])),
        }
    }

    /// The bytes an old binary wrote: six fields, nothing after them.
    fn legacy_bytes(record: &ContentVersion) -> Vec<u8> {
        #[derive(serde::Serialize)]
        struct Legacy<'a> {
            content_build: &'a str,
            manifest_digest: &'a str,
            scenario_seed: &'a str,
            config_digest: &'a str,
            toolchain: &'a str,
            seeded_at_ms: u64,
        }
        postcard::to_stdvec(&Legacy {
            content_build: &record.content_build,
            manifest_digest: &record.manifest_digest,
            scenario_seed: &record.scenario_seed,
            config_digest: &record.config_digest,
            toolchain: &record.toolchain,
            seeded_at_ms: record.seeded_at_ms,
        })
        .expect("legacy encodes")
    }

    #[test]
    fn a_v1_row_round_trips_through_a_v1_reader() {
        let encoded = encode(&v1()).expect("encodes");
        assert_eq!(
            encoded.last(),
            Some(&CONTENT_VERSION_ENCODING_V1),
            "the version is the last byte, not the first"
        );
        assert_eq!(decode(&encoded).expect("decodes"), v1());
    }

    #[test]
    fn a_v1_row_without_a_fingerprint_still_round_trips() {
        // The seeder was given no fingerprint: the field is `None`, and the
        // row is still v1 — "absent fingerprint" and "absent trailer" are
        // different facts and must not collapse into one.
        let mut record = v1();
        record.universe_seed_fingerprint = None;
        let encoded = encode(&record).expect("encodes");
        assert_eq!(decode(&encoded).expect("decodes"), record);
    }

    #[test]
    fn an_untagged_v0_row_reads_as_an_absent_fingerprint() {
        let mut expected = v1();
        expected.universe_seed_fingerprint = None;
        let decoded = decode(&legacy_bytes(&v1())).expect("bootstraps");
        assert_eq!(
            decoded, expected,
            "absent trailer == v0, and v0 has no fingerprint — not a guessed one"
        );
    }

    #[test]
    fn an_old_reader_reads_a_v1_row_as_the_v0_view_it_understands() {
        // The migration's actual cost, measured rather than assumed. The
        // spike predicted an old binary would fail loudly at the trailer;
        // `postcard::from_bytes` in fact *discards* whatever follows the body
        // it decoded, so an old reader succeeds — and, because the trailer
        // appends rather than shifts, it succeeds with exactly the six fields
        // it always read. Silent, but silently *correct*: it drops a seal it
        // has no concept of and corrupts nothing.
        let encoded = encode(&v1()).expect("encodes");
        let old = postcard::from_bytes::<ContentVersionV0>(&encoded).expect("old reader decodes");
        let expected = v1();
        assert_eq!(old.content_build, expected.content_build);
        assert_eq!(old.manifest_digest, expected.manifest_digest);
        assert_eq!(old.scenario_seed, expected.scenario_seed);
        assert_eq!(old.config_digest, expected.config_digest);
        assert_eq!(old.toolchain, expected.toolchain);
        assert_eq!(
            old.seeded_at_ms, expected.seeded_at_ms,
            "no field may shift: that is the entire difference between a trailer and a tag"
        );
    }

    #[test]
    fn a_strict_reader_sees_the_extra_bytes_rather_than_ignoring_them() {
        // The loud half of the same fact, and the one this module relies on:
        // a reader that accounts for every byte — `decode_versioned`, which
        // `decode` above is built from — refuses a v1 row as v0 instead of
        // silently truncating it. That refusal is what makes `decode`'s
        // dispatch decidable rather than a guess.
        let encoded = encode(&v1()).expect("encodes");
        let error = decode_versioned::<ContentVersionV0>(&encoded)
            .expect_err("a strict v0 reader must not accept a v1 row");
        assert!(
            matches!(error, VersionedError::Trailing(n) if n > 1),
            "the refusal must name the leftover bytes: {error}"
        );
    }

    #[test]
    fn a_leading_tag_would_have_corrupted_the_same_read() {
        // Why the version is a trailer, demonstrated rather than asserted.
        // With the version byte *first*, an old reader takes it as the length
        // varint of `content_build` and every field after it shifts. The read
        // either fails or yields a record that never existed — never the row
        // that was written.
        let mut tagged = vec![CONTENT_VERSION_ENCODING_V1];
        tagged.extend_from_slice(&legacy_bytes(&v1()));
        let misread = postcard::from_bytes::<ContentVersionV0>(&tagged);
        assert!(
            !matches!(&misread, Ok(record) if record.content_build == v1().content_build),
            "a leading tag must not be able to reproduce the written row: {misread:?}"
        );
    }
}
