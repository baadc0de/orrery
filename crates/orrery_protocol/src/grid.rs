//! Nested-grid identity (docs/01-spatial-model.md §13).
//!
//! Each moving reference frame (ship, planet, station) is its own `CellId`
//! space. A [`GridId`] is carried alongside a [`CellId`] wherever a cell
//! reference can cross frames — wire messages, journal records, storage keys,
//! log records. The root universe grid is 0.

use serde::{Deserialize, Serialize};

/// Identifies one nested grid (one `CellId` space). The root universe grid is
/// `0`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct GridId(pub u32);

impl GridId {
    /// The root universe grid.
    pub const ROOT: Self = Self(0);

    /// A grid id for a nested reference frame.
    pub const fn new(id: u32) -> Self {
        Self(id)
    }
}

impl core::fmt::Display for GridId {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "grid:{}", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_is_zero() {
        assert_eq!(GridId::ROOT, GridId(0));
        assert_eq!(GridId::ROOT.0, 0);
    }

    #[test]
    fn new_wraps_the_raw_id() {
        assert_eq!(GridId::new(7), GridId(7));
        assert_eq!(GridId::new(0), GridId::ROOT);
        // `new` is const, so grid ids can be named constants.
        const NESTED: GridId = GridId::new(9);
        assert_eq!(NESTED, GridId(9));
    }

    #[test]
    fn display_is_grid_prefixed_decimal() {
        assert_eq!(GridId::ROOT.to_string(), "grid:0");
        assert_eq!(GridId::new(42).to_string(), "grid:42");
        // Plain decimal: no padding, no hex.
        assert_eq!(GridId::new(255).to_string(), "grid:255");
    }

    #[test]
    fn ordering_is_by_inner_id() {
        let mut ids = [GridId::new(3), GridId::ROOT, GridId::new(7), GridId::new(1)];
        ids.sort();
        assert_eq!(
            ids,
            [GridId::ROOT, GridId::new(1), GridId::new(3), GridId::new(7)]
        );
    }

    #[test]
    fn postcard_roundtrips() {
        for id in [
            GridId::ROOT,
            GridId::new(1),
            GridId::new(300),
            GridId::new(u32::MAX),
        ] {
            let bytes = postcard::to_stdvec(&id).unwrap();
            let back: GridId = postcard::from_bytes(&bytes).unwrap();
            assert_eq!(back, id);
        }
    }

    #[test]
    fn postcard_encoding_is_unsigned_varint() {
        // Postcard (D15) encodes a newtype-wrapped u32 as a bare unsigned
        // LEB128 varint: no tag, no length prefix. Values 0..=127 fit in one
        // byte; larger values take 7 bits per byte, least-significant group
        // first, with the high bit of each byte as the continuation flag.
        assert_eq!(postcard::to_stdvec(&GridId::ROOT).unwrap(), [0x00]);
        assert_eq!(postcard::to_stdvec(&GridId::new(127)).unwrap(), [0x7F]);
        // 128 = 0b1000_0000 → low 7 bits (0) with continuation set, then 1.
        assert_eq!(
            postcard::to_stdvec(&GridId::new(128)).unwrap(),
            [0x80, 0x01]
        );
        // 300 = 0b1_0101_100 → 0b0101_100 with continuation, then 0b10.
        assert_eq!(
            postcard::to_stdvec(&GridId::new(300)).unwrap(),
            [0xAC, 0x02]
        );
        // u32::MAX needs all five varint bytes (32 bits / 7 = 5 groups; the
        // last carries the remaining 4 bits).
        assert_eq!(
            postcard::to_stdvec(&GridId::new(u32::MAX)).unwrap(),
            [0xFF, 0xFF, 0xFF, 0xFF, 0x0F]
        );
    }
}
