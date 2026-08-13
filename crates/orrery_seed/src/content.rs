//! `ContentKey`: identity is the derivation path (docs/12 §9.1, D-C).
//!
//! An entity's content identity is a hash of **how it was derived** — layer
//! name, emit name, cell, per-cell index, archetype — never of its position,
//! its minted `PersistId`, or a global ordinal:
//!
//! ```text
//! ContentKey = blake3(b"orrery.ck.v1" ‖ scenario_name ‖ emit_name ‖ layer_name
//!                     ‖ grid ‖ cell.to_bits() ‖ index ‖ archetype)[..16]
//! ```
//!
//! This is the whole patch story (D-C): under a global scheme, changing
//! `count` from 10 000 to 10 001 shifts every downstream draw and rewrites
//! the world; under cell-local derivation, a count change perturbs only the
//! cells whose split changed. Position-independence is what lets a `Rekey`
//! (same content, moved cell) show up as a location diff with an unchanged
//! digest rather than as a delete-plus-create.

use core::str::FromStr;

use orrery_protocol::{CellId, GridId};

/// The domain tag prefixing every content-key preimage (docs/12 §9.1).
pub const CONTENT_KEY_DOMAIN: &[u8] = b"orrery.ck.v1";

/// Content identity of one seeded entity: the first 16 bytes of the blake3
/// digest of its derivation path (docs/12 §9.1).
///
/// `Ord` so manifest entries stream in `(grid, cell, ContentKey)` order and
/// `BTreeMap`/`BTreeSet` membership is total (§8.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ContentKey(pub [u8; 16]);

/// Everything the key commits to (docs/12 §9.1). All fields are part of the
/// derivation path; nothing else about the entity is.
#[derive(Debug, Clone, Copy)]
pub struct ContentKeyPreimage<'a> {
    /// `[scenario] name`.
    pub scenario: &'a str,
    /// `[[emit]] name`.
    pub emit: &'a str,
    /// The name of the layer that produced the field the emit realized
    /// (§9.1 lists it as a preimage component).
    pub layer: &'a str,
    /// The grid the entity lives in.
    pub grid: GridId,
    /// The entity's own interest cell (P-2: not the shard).
    pub cell: CellId,
    /// The per-cell slot index, `0..cell_count`.
    pub index: u64,
    /// The archetype name from `[archetype.<name>]`.
    pub archetype: &'a str,
}

impl ContentKey {
    /// Derive the key from its preimage (docs/12 §9.1). Field widths are
    /// fixed at the storage width — `grid` big-endian u32, `cell` big-endian
    /// u64 bits, `index` little-endian u64 (matching the seed tree's slot
    /// index, §8) — so the preimage is unambiguous without separators.
    #[must_use]
    pub fn derive(pre: &ContentKeyPreimage<'_>) -> Self {
        let mut hasher = blake3::Hasher::new();
        hasher.update(CONTENT_KEY_DOMAIN);
        hasher.update(pre.scenario.as_bytes());
        hasher.update(pre.emit.as_bytes());
        hasher.update(pre.layer.as_bytes());
        hasher.update(&pre.grid.0.to_be_bytes());
        hasher.update(&pre.cell.to_bits().to_be_bytes());
        hasher.update(&pre.index.to_le_bytes());
        hasher.update(pre.archetype.as_bytes());
        let digest = hasher.finalize();
        let mut key = [0u8; 16];
        key.copy_from_slice(&digest.as_bytes()[..16]);
        Self(key)
    }
}

impl core::fmt::Display for ContentKey {
    /// Lowercase hex, no prefix — the form that pastes into a manifest diff.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        for b in &self.0 {
            write!(f, "{b:02x}")?;
        }
        Ok(())
    }
}

/// Parse error for a malformed [`ContentKey`] hex string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContentKeyParseError(pub String);

impl core::fmt::Display for ContentKeyParseError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "invalid content key: {}", self.0)
    }
}

impl core::error::Error for ContentKeyParseError {}

impl FromStr for ContentKey {
    type Err = ContentKeyParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.strip_prefix("0x").unwrap_or(s);
        if s.len() != 32 {
            return Err(ContentKeyParseError(format!(
                "expected 32 hex digits, got {}",
                s.len()
            )));
        }
        let mut key = [0u8; 16];
        let mut i = 0;
        while i < 16 {
            key[i] = u8::from_str_radix(&s[2 * i..2 * i + 2], 16)
                .map_err(|e| ContentKeyParseError(e.to_string()))?;
            i += 1;
        }
        Ok(Self(key))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use orrery_protocol::PersistId;

    fn preimage<'a>(scenario: &'a str, archetype: &'a str) -> ContentKeyPreimage<'a> {
        ContentKeyPreimage {
            scenario,
            emit: "props",
            layer: "world",
            grid: GridId::ROOT,
            cell: CellId::from_bits(0xA924_9249_2492_4D65).expect("nonzero"),
            index: 42,
            archetype,
        }
    }

    /// The spec transcription (docs/12 §9.1) spelled out longhand: one
    /// contiguous preimage, one digest, truncate to 16 bytes.
    fn longhand(pre: &ContentKeyPreimage<'_>) -> [u8; 16] {
        let mut buf = Vec::new();
        buf.extend_from_slice(b"orrery.ck.v1");
        buf.extend_from_slice(pre.scenario.as_bytes());
        buf.extend_from_slice(pre.emit.as_bytes());
        buf.extend_from_slice(pre.layer.as_bytes());
        buf.extend_from_slice(&pre.grid.0.to_be_bytes());
        buf.extend_from_slice(&pre.cell.to_bits().to_be_bytes());
        buf.extend_from_slice(&pre.index.to_le_bytes());
        buf.extend_from_slice(pre.archetype.as_bytes());
        let digest = blake3::hash(&buf);
        let mut out = [0u8; 16];
        out.copy_from_slice(&digest.as_bytes()[..16]);
        out
    }

    #[test]
    fn content_key_matches_pinned_vector() {
        // One full preimage pinned against a hardcoded 16-byte vector, so a
        // preimage-layout change (field order, width, domain tag) fails
        // loudly. The vector is independently reproduced by `longhand`.
        let pre = preimage("p2demo", "crate");
        let key = ContentKey::derive(&pre);
        assert_eq!(
            key.0,
            [
                0x2b, 0xf7, 0xd5, 0x10, 0xa0, 0x35, 0xb3, 0xfb, 0xbd, 0x70, 0xaf, 0x5a, 0x12, 0x8f,
                0x67, 0x3b
            ],
            "pinned blake3[..16] of the docs/12 §9.1 preimage"
        );
        assert_eq!(
            key.0,
            longhand(&pre),
            "production derivation matches the spec transcription"
        );
    }

    #[test]
    fn content_key_is_position_and_pid_independent() {
        // D-C: the key commits to the derivation path only. Neither
        // `local_pos` nor the minted `PersistId` appears in the preimage
        // type at all, so this test varies them at the call site and asserts
        // the key derivation has no input to consume: the preimage built for
        // two different (position, PersistId) pairs is byte-identical.
        let pre = preimage("p2demo", "crate");
        let key_a = ContentKey::derive(&pre);

        let local_pos_a = [12.5f32, -3.25, 100.0];
        let local_pos_b = [127.9f32, 0.0, 1.0];
        let pid_a = PersistId::new(1);
        let pid_b = PersistId::new(9_999_999);

        // Rebuild the preimage "after" a position/PersistId assignment: the
        // struct has no field to change, which is the point — assert the
        // derived key is unchanged and that the two (pos, pid) pairs are
        // genuinely different inputs at the call site.
        let pre_b = ContentKeyPreimage {
            scenario: pre.scenario,
            emit: pre.emit,
            layer: pre.layer,
            grid: pre.grid,
            cell: pre.cell,
            index: pre.index,
            archetype: pre.archetype,
        };
        let key_b = ContentKey::derive(&pre_b);
        assert_ne!(local_pos_a, local_pos_b);
        assert_ne!(pid_a, pid_b);
        assert_eq!(key_a, key_b, "position and PersistId never enter the key");
    }

    #[test]
    fn derivation_path_components_all_matter() {
        // Every documented preimage component is load-bearing: change one,
        // the key changes. (A collision here would be a blake3 break; the
        // point is that no component is accidentally dropped.)
        let base = preimage("p2demo", "crate");
        let base_key = ContentKey::derive(&base);

        let mut p = base;
        p.scenario = "other";
        assert_ne!(ContentKey::derive(&p), base_key, "scenario is committed");

        let mut p = base;
        p.emit = "other";
        assert_ne!(ContentKey::derive(&p), base_key, "emit is committed");

        let mut p = base;
        p.layer = "other";
        assert_ne!(ContentKey::derive(&p), base_key, "layer is committed");

        let mut p = base;
        p.grid = GridId::new(7);
        assert_ne!(ContentKey::derive(&p), base_key, "grid is committed");

        let mut p = base;
        p.cell = CellId::from_bits(0xA924_9249_2492_4D66).expect("nonzero");
        assert_ne!(ContentKey::derive(&p), base_key, "cell is committed");

        let mut p = base;
        p.index = 43;
        assert_ne!(ContentKey::derive(&p), base_key, "index is committed");

        let p = preimage("p2demo", "barrel");
        assert_ne!(ContentKey::derive(&p), base_key, "archetype is committed");
    }

    #[test]
    fn display_fromstr_hex_roundtrip() {
        let key = ContentKey::derive(&preimage("p2demo", "crate"));
        let text = key.to_string();
        assert_eq!(text.len(), 32);
        let parsed: ContentKey = text.parse().expect("parses");
        assert_eq!(parsed, key);
        // 0x-prefixed form parses too (tool output may carry it).
        let prefixed: ContentKey = format!("0x{text}").parse().expect("parses");
        assert_eq!(prefixed, key);
        // Garbage is an error, not a truncation.
        assert!("abcd".parse::<ContentKey>().is_err());
        assert!("zz".repeat(16).parse::<ContentKey>().is_err());
    }

    #[test]
    fn ordering_is_total() {
        // The manifest streams in (grid, cell, ContentKey) order and uses
        // BTree structures (§8.4): ordering must be plain byte order.
        let a = ContentKey([0x00; 16]);
        let b = ContentKey([0x00, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]);
        assert!(a < b);
    }
}
