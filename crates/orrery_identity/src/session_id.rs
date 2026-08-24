//! Coordinator-style session identities: UUIDv7, minted offline.
//!
//! The P4 ledger (`scripts/p4-ledger.sh`) refuses a human hour without
//! `identity.human_session_id`, a coordinator-issued UUIDv7 allocated once.
//! With no identity service running (#375 decision record: invites are minted
//! offline), the invite mints that identity at allocation time — "pre-minted"
//! — so one invite code binds exactly one bankable session identity before
//! anyone dials anything. The operator passes it to the host
//! (`p1-swarm --expected-session-id`); the volunteer's client presents the
//! same value at join, where the host validates the pair.

use rand::Rng;

use orrery_protocol::UnixMillis;

/// Mint one session identity: a UUIDv7 stamped with `now_ms`.
///
/// Layout follows RFC 9562: 48 bits of Unix millisecond timestamp, version 7,
/// RFC 4122 variant, then 62 random bits. Uniqueness across a campaign comes
/// from those bits plus the monotone timestamp; the ledger still refuses a
/// duplicate explicitly, because hoping is not an allocation discipline.
#[must_use]
pub fn session_uuid_v7(now_ms: UnixMillis, rng: &mut impl Rng) -> String {
    let mut bytes = [0u8; 16];
    // A u64 millisecond stamp is wider than UUIDv7's 48-bit field; the
    // big-endian form's *low* six bytes are the field.
    bytes[..6].copy_from_slice(&now_ms.0.to_be_bytes()[2..8]);
    bytes[6] = 0x70 | (rng.random::<u8>() & 0x0f);
    bytes[7] = rng.random();
    // Variant `10xxxxxx`: exactly what the ledger's `[89ab]` class accepts.
    bytes[8] = 0x80 | (rng.random::<u8>() & 0x3f);
    rng.fill(&mut bytes[9..]);
    format_uuid(&bytes)
}

/// Render the canonical hyphenated lowercase form.
fn format_uuid(bytes: &[u8; 16]) -> String {
    let hex = |slice: &[u8]| -> String {
        slice.iter().map(|byte| format!("{byte:02x}")).collect()
    };
    format!(
        "{}-{}-{}-{}-{}",
        hex(&bytes[..4]),
        hex(&bytes[4..6]),
        hex(&bytes[6..8]),
        hex(&bytes[8..10]),
        hex(&bytes[10..])
    )
}

/// Whether `text` is the exact shape the ledger demands of
/// `identity.human_session_id`: lowercase hexadecimal, version nibble 7,
/// RFC 4122 variant. Mirrors the ledger's own pattern character for character,
/// so a value accepted here cannot be refused there.
#[must_use]
pub fn is_uuid_v7(text: &str) -> bool {
    let parts: Vec<&str> = text.split('-').collect();
    let lengths = [8usize, 4, 4, 4, 12];
    if parts.len() != 5 || !parts.iter().zip(lengths).all(|(part, len)| {
        part.len() == len && part.bytes().all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    }) {
        return false;
    }
    let version = parts[2].as_bytes()[0];
    let variant = parts[3].as_bytes()[0];
    version == b'7' && matches!(variant, b'8' | b'9' | b'a' | b'b')
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::SeedableRng;
    use rand_chacha::ChaCha8Rng;

    /// The ledger's own acceptance pattern, restated from
    /// `scripts/p4-ledger.sh`. Keeping the two side by side is the drift alarm:
    /// this test fails the moment one copy moves without the other.
    fn ledger_accepts(text: &str) -> bool {
        let pattern = regex::Regex::new(
            r"^[0-9a-f]{8}-[0-9a-f]{4}-7[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$",
        )
        .expect("static pattern");
        pattern.is_match(text)
    }

    #[test]
    fn minted_ids_satisfy_the_ledger_pattern_and_the_local_check() {
        let mut rng = ChaCha8Rng::seed_from_u64(7);
        for now_ms in [0u64, 1, 1_756_000_000_000, u64::from(u32::MAX)] {
            let id = session_uuid_v7(UnixMillis(now_ms), &mut rng);
            assert!(is_uuid_v7(&id), "{id} failed the local check");
            assert!(ledger_accepts(&id), "{id} failed the ledger pattern");
            let version = id.split('-').nth(2).expect("three groups").as_bytes()[0];
            assert_eq!(version, b'7', "{id} is not a v7");
        }
    }

    #[test]
    fn timestamps_land_in_the_leading_bits() {
        let mut rng = ChaCha8Rng::seed_from_u64(9);
        let now_ms = UnixMillis(1_756_000_000_000);
        let id = session_uuid_v7(now_ms, &mut rng);
        let first_eight = &id[..8];
        let expected = format!("{:08x}", (now_ms.0 >> 16) as u32);
        assert_eq!(first_eight, expected, "the top 32 timestamp bits lead");
    }

    #[test]
    fn the_checker_refuses_near_misses() {
        for rejected in [
            "",
            "018f8f4e5c907abc8123000000000001",
            "018F8F4E-5C90-7ABC-8123-00000000ABCD", // uppercase
            "018f8f4e-5c90-4abc-8123-00000000abcd", // v4, not v7
            "018f8f4e-5c90-7abc-c123-00000000abcd", // bad variant
            "018f8f4e-5c90-7abc-8123-00000000abcd0", // long
            "018f8f4e_5c90_7abc_8123_00000000abcd", // wrong separators
        ] {
            assert!(!is_uuid_v7(rejected), "{rejected} must be refused");
            assert!(
                !ledger_accepts(rejected),
                "the ledger would accept what the local check refuses: {rejected}"
            );
        }
    }
}
