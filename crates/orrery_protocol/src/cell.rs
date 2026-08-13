//! The 64-bit [`CellId`]: one ID, three jobs (D5).
//!
//! A `CellId` is simultaneously the replication interest group (27-cell AOI),
//! the storage shard-key prefix, and the authority/handoff unit. The encoding
//! is normative from docs/01-spatial-model.md §3:
//!
//! - Levels run 0 (root) to 21 (finest). Coordinates at level *L* are signed
//!   integers in `[−2^(L−1), 2^(L−1))` per axis.
//! - **Offset-binary:** each signed coord is biased to unsigned:
//!   `u = c + 2^(L−1)`, an *L*-bit value. Arithmetic right-shift of the biased
//!   form equals floor-division of the signed form, so coarsening is a shift.
//! - **Morton interleave:** the three *L*-bit values are interleaved MSB-first
//!   in axis order x, y, z — bit *i* of each axis produces the triplet
//!   `x_i y_i z_i` — yielding a `3·L`-bit Morton prefix (63 bits at level 21).
//! - **Sentinel:** the prefix is placed in the top `3·L` bits of the u64,
//!   followed by a single `1` bit, then zeros.
//!
//! Two properties carry the whole design:
//!
//! - **Sorted order = spatial locality.** Numerically adjacent IDs are (mostly)
//!   spatially adjacent cells, so `[cell_id][entity_id]` range scans read
//!   neighborhoods contiguously (D11).
//! - **Parent is a prefix.** A cell's entire subtree — all descendants at all
//!   levels, plus itself — is exactly the contiguous u64 range
//!   `[id − lsb + 1, id + lsb − 1]`. "Everything stored under this shard cell"
//!   is one range scan (the S2 trick).

use core::ops::RangeInclusive;

use glam::IVec3;

/// The interest level (finest, default) — grid cells with 128 m edge at the
/// default config. The [`CellId`] encoding supports levels 0..=21; the interest
/// level is 21.
///
/// Moved from `orrery_spatial` (engine-agnostic so the world seeder and
/// persistence tools can use it without linking Bevy).
pub const INTEREST_LEVEL: u8 = CellId::MAX_LEVEL;

/// The shard level = interest − 3 (one shard cell = 8×8×8 interest cells, D5).
///
/// Moved from `orrery_spatial` for the same reason as [`INTEREST_LEVEL`].
pub const SHARD_LEVEL: u8 = INTEREST_LEVEL - 3;

/// The default interest-cell edge in metres, 128.0 (D16).
///
/// `f64` because the metre conversions must maintain precision up to the
/// level-21 extent — ±4.3×10¹² m at the solar-grid edge of 4 096 000 m/edge
/// (docs/12-world-seeding.md §5.1). `f32`'s 24-bit mantissa silently discards
/// the low bits of coordinates near that range, which the no-clamping rule
/// must prevent.
///
/// Moved from `orrery_spatial::config::SpatialConfig::default().cell_edge_m` so
/// the world seeder and persistence tools have access to the canonical default
/// without linking Bevy. The engine-side `SpatialConfig` keeps its `f32` field
/// (a running client is always near its own origin and never needs the
/// precision).
pub const DEFAULT_CELL_EDGE_M: f64 = 128.0;

/// The shard cell containing an interest cell: its level-18 ancestor.
///
/// Moved from `orrery_spatial` (engine-agnostic).
#[must_use]
pub fn shard_of(cell: CellId) -> CellId {
    cell.ancestor_at(SHARD_LEVEL)
}

/// Convert grid-local metres to an interest-level [`CellId`] for a given
/// `cell_edge_m`.
///
/// The conversion is `(m ÷ cell_edge_m).floor()` for each axis, then passed to
/// [`CellId::from_coords`] at [`INTEREST_LEVEL`].
///
/// # Errors
///
/// Returns [`CellRangeError`] if any coordinate (in cell units) is outside the
/// representable range `[−2^(L−1), 2^(L−1))` at [`INTEREST_LEVEL`], naming the
/// offending value and the grid's half-extent in metres
/// (`2^20 × cell_edge_m`). Unlike [`clamp_coord`] in `orrery_spatial`, this
/// never silently clamps — the seeder must know it is out of range (see
/// docs/12-world-seeding.md §5.1).
pub fn cell_id_from_metres(
    metres: glam::DVec3,
    cell_edge_m: f64,
) -> Result<CellId, CellRangeError> {
    let cell_coords = glam::IVec3::new(
        (metres.x / cell_edge_m).floor() as i32,
        (metres.y / cell_edge_m).floor() as i32,
        (metres.z / cell_edge_m).floor() as i32,
    );
    CellId::from_coords(cell_coords, INTEREST_LEVEL)
}

/// Convert an interest-level [`CellId`] back to the **minimum corner** of that
/// cell in grid-local metres.
///
/// Returns the metre-space corner as `cell_coord × cell_edge_m` for each axis.
/// This is the *min* corner (the lowest metre value in the cell's volume);
/// the cell's centre is `min_corner + (cell_edge_m / 2, cell_edge_m / 2, cell_edge_m / 2)`.
///
/// The conversion uses `f64` for the same precision reason as
/// [`cell_id_from_metres`].
pub fn metres_from_cell_id(cell: CellId, cell_edge_m: f64) -> glam::DVec3 {
    let (coords, _level) = cell.coords();
    glam::DVec3::new(
        coords.x as f64 * cell_edge_m,
        coords.y as f64 * cell_edge_m,
        coords.z as f64 * cell_edge_m,
    )
}

/// Error returned when a cell coordinate or level is out of range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CellRangeError {
    /// The level is not in `0..=MAX_LEVEL`.
    LevelOutOfRange {
        /// The offending level.
        level: u8,
    },
    /// A coordinate is outside `[−2^(L−1), 2^(L−1))` for the given level.
    CoordOutOfRange {
        /// The offending coordinate value.
        coord: i32,
        /// The level at which it is out of range.
        level: u8,
    },
}

impl core::fmt::Display for CellRangeError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::LevelOutOfRange { level } => {
                write!(
                    f,
                    "cell level {level} out of range 0..={}",
                    CellId::MAX_LEVEL
                )
            }
            Self::CoordOutOfRange { coord, level } => {
                let half = 1i64 << (level - 1);
                write!(
                    f,
                    "cell coordinate {coord} out of range [−{half}, {half}) at level {level}"
                )
            }
        }
    }
}

impl core::error::Error for CellRangeError {}

/// A hierarchical uniform grid cell (D5).
///
/// `NonZeroU64` newtype: 0 is invalid, giving a free `Option<CellId>` niche.
/// Totally ordered, hash- and sort-stable, identical on wire, in memory, and
/// (by default) in the storage keyspace.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CellId(core::num::NonZeroU64);

impl CellId {
    /// The finest (default interest) level. ±2²⁰ cells per axis.
    pub const MAX_LEVEL: u8 = 21;

    /// The root cell: level 0, the entire addressable volume of one grid.
    pub const ROOT: Self = Self::from_raw(1 << 63);

    /// The raw u64 encoding (offset-binary + Morton interleave + sentinel).
    ///
    /// Never zero. Useful for storage keys and range scans; prefer the typed
    /// accessors for everything else.
    #[must_use]
    pub const fn to_bits(self) -> u64 {
        self.0.get()
    }

    /// Reconstruct a `CellId` from its raw u64 encoding — the inverse of
    /// [`CellId::to_bits`], used to decode cell ids out of storage keys.
    ///
    /// Returns `None` for `0` (the invalid id); mirrors the `serde` guard. The
    /// bits are trusted to be a well-formed offset-binary/Morton/sentinel
    /// encoding, since every key is written by `to_bits`.
    #[must_use]
    pub const fn from_bits(raw: u64) -> Option<Self> {
        match core::num::NonZeroU64::new(raw) {
            Some(v) => Some(Self(v)),
            None => None,
        }
    }

    /// Construct a `CellId` from signed cell coordinates at the given level.
    ///
    /// Returns [`CellRangeError`] if `level > MAX_LEVEL` or any coordinate is
    /// outside `[−2^(L−1), 2^(L−1))`.
    pub fn from_cell_coords(xyz: IVec3, level: u8) -> Result<Self, CellRangeError> {
        Self::from_coords(xyz, level)
    }

    /// Construct a `CellId` from signed cell coordinates at the given level.
    ///
    /// Alias of [`CellId::from_cell_coords`], matching the docs/01-spatial-model
    /// §3.4 sketch name.
    pub fn from_coords(xyz: IVec3, level: u8) -> Result<Self, CellRangeError> {
        if level > Self::MAX_LEVEL {
            return Err(CellRangeError::LevelOutOfRange { level });
        }
        // Level 0 is the root: the range [−1, 1) admits only the origin.
        if level == 0 {
            return (xyz == IVec3::ZERO)
                .then_some(Self::ROOT)
                .ok_or(CellRangeError::CoordOutOfRange { coord: 0, level });
        }
        let half = 1i64 << (level - 1);
        for c in [xyz.x, xyz.y, xyz.z] {
            let c = i64::from(c);
            if c < -half || c >= half {
                return Err(CellRangeError::CoordOutOfRange {
                    coord: c as i32,
                    level,
                });
            }
        }
        Ok(Self::from_raw(Self::encode(xyz, level)))
    }

    /// The level of this cell, `0..=MAX_LEVEL`.
    ///
    /// `(63 − id.trailing_zeros()) / 3` — O(1) via `TZCNT`.
    pub fn level(self) -> u8 {
        ((63 - self.0.get().trailing_zeros()) / 3) as u8
    }

    /// The parent cell (drop the bottom three Morton bits, re-sentinel), or
    /// `None` at level 0 (the root has no parent).
    pub fn parent(self) -> Option<Self> {
        if self.level() == 0 {
            return None;
        }
        let id = self.0.get();
        let lsb = id & id.wrapping_neg();
        let nl = lsb << 3;
        let parent = (id & nl.wrapping_neg()) | nl;
        (parent != id).then_some(Self::from_raw(parent))
    }

    /// The ancestor of this cell at the given level (coarser or equal).
    ///
    /// # Panics
    ///
    /// Panics if `level > self.level()`.
    pub fn ancestor_at(self, level: u8) -> Self {
        assert!(
            level <= self.level(),
            "ancestor_at level {level} is finer than cell level {}",
            self.level()
        );
        let mut cell = self;
        while cell.level() > level {
            cell = cell.parent().expect("level > 0");
        }
        cell
    }

    /// The eight child cells (sentinel moves down three bits).
    ///
    /// Each child appends one 3-bit triplet (`x_0 y_0 z_0`) and re-sets the
    /// sentinel three bits lower. Child `i` has triplet value `i`.
    ///
    /// # Panics
    ///
    /// Panics if `self.level() == MAX_LEVEL` (the finest level has no
    /// children).
    pub fn children(self) -> [Self; 8] {
        let (coords, level) = self.coords();
        assert!(
            level < Self::MAX_LEVEL,
            "children of a level {level} cell exceeds MAX_LEVEL"
        );
        let child_level = level + 1;
        let mut out = [Self::ROOT; 8];
        for (i, slot) in out.iter_mut().enumerate() {
            let dx = (i & 1) as i32;
            let dy = ((i >> 1) & 1) as i32;
            let dz = ((i >> 2) & 1) as i32;
            // The root (level 0) spans 2 units per axis while reporting
            // coordinate 0, so its children tile [-1, 0]³ rather than [0, 1]³.
            let child_coords = if level == 0 {
                IVec3::new(dx - 1, dy - 1, dz - 1)
            } else {
                coords * 2 + IVec3::new(dx, dy, dz)
            };
            *slot = Self::from_coords(child_coords, child_level)
                .expect("child of a valid cell is in range");
        }
        out
    }

    /// The 3×3×3 AOI neighborhood (27 cells, self included), clamped at the
    /// volume edge (D5). Order is unspecified but deterministic.
    ///
    /// Returns a `Vec` because the volume edge can admit fewer than 27 cells;
    /// the interior fast path always yields exactly 27.
    pub fn neighbors27(self) -> Vec<Self> {
        let (coords, level) = self.coords();
        let mut out = Vec::with_capacity(27);
        for dz in -1..=1 {
            for dy in -1..=1 {
                for dx in -1..=1 {
                    if let Ok(n) = Self::from_coords(coords + IVec3::new(dx, dy, dz), level) {
                        out.push(n);
                    }
                }
            }
        }
        out
    }

    /// The contiguous u64 range of every cell in this cell's subtree (itself
    /// included) — the storage range-scan key span (D5, D11).
    ///
    /// Uses wrapping arithmetic so the root's subtree spans the whole valid
    /// space `[1, u64::MAX]`.
    pub fn subtree_range(self) -> RangeInclusive<u64> {
        let id = self.0.get();
        let lsb = id & id.wrapping_neg();
        (id - lsb + 1)..=(id.wrapping_add(lsb).wrapping_sub(1))
    }

    /// Whether `other` is this cell or a descendant (this cell's subtree
    /// contains `other`).
    pub fn is_prefix_of(self, other: Self) -> bool {
        self.subtree_range().contains(&other.0.get())
    }

    /// The signed coordinates and level of this cell.
    pub fn coords(self) -> (IVec3, u8) {
        let level = self.level();
        if level == 0 {
            return (IVec3::ZERO, 0);
        }
        let id = self.0.get();
        // De-interleave the top 3·level bits, MSB-first, axis order x, y, z.
        // Triplet i occupies bits 63−3i (x), 62−3i (y), 61−3i (z); the sentinel
        // is at bit 63−3·level.
        let mut x = 0u64;
        let mut y = 0u64;
        let mut z = 0u64;
        for i in 0..level {
            let xb = (id >> (63 - 3 * i)) & 1;
            let yb = (id >> (62 - 3 * i)) & 1;
            let zb = (id >> (61 - 3 * i)) & 1;
            let shift = level - 1 - i;
            x |= xb << shift;
            y |= yb << shift;
            z |= zb << shift;
        }
        let half = 1i64 << (level - 1);
        let unbias = |v: u64| (v as i64) - half;
        (
            IVec3::new(unbias(x) as i32, unbias(y) as i32, unbias(z) as i32),
            level,
        )
    }

    /// The neighboring cell at `offset` in cell units, or `None` if the result
    /// is outside `[−2^(L−1), 2^(L−1))` (the volume edge).
    ///
    /// Always decode → add → re-encode; never raw key arithmetic (D5).
    pub fn neighbor(self, offset: IVec3) -> Option<Self> {
        let (coords, level) = self.coords();
        Self::from_coords(coords + offset, level).ok()
    }

    /// Construct from a raw u64, panicking if it is not a valid cell id.
    ///
    /// Internal helper; the public constructors enforce validity.
    const fn from_raw(raw: u64) -> Self {
        Self(core::num::NonZeroU64::new(raw).expect("cell id is never zero"))
    }

    /// Encode signed coords at `level` into the raw u64 (offset-binary +
    /// Morton interleave + sentinel). `level` is assumed valid and nonzero.
    fn encode(xyz: IVec3, level: u8) -> u64 {
        let half = 1u64 << (level - 1);
        let bias = |c: i32| (i64::from(c) + half as i64) as u64;
        let x = bias(xyz.x);
        let y = bias(xyz.y);
        let z = bias(xyz.z);

        let mut out = 0u64;
        for i in 0..level {
            let shift = level - 1 - i;
            let xb = (x >> shift) & 1;
            let yb = (y >> shift) & 1;
            let zb = (z >> shift) & 1;
            let bit = 63 - (3 * i);
            out |= xb << bit;
            out |= yb << (bit - 1);
            out |= zb << (bit - 2);
        }
        // Sentinel bit: immediately below the 3·level-bit prefix.
        out | 1u64 << (63 - 3 * level)
    }
}

impl core::fmt::Debug for CellId {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let (coords, level) = self.coords();
        f.debug_struct("CellId")
            .field("raw", &format_args!("0x{:016x}", self.0.get()))
            .field("coords", &coords)
            .field("level", &level)
            .finish()
    }
}

impl core::fmt::Display for CellId {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "0x{:016x}", self.0.get())
    }
}

impl serde::Serialize for CellId {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_u64(self.0.get())
    }
}

impl<'de> serde::Deserialize<'de> for CellId {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = u64::deserialize(deserializer)?;
        core::num::NonZeroU64::new(raw)
            .map(Self)
            .ok_or_else(|| serde::de::Error::custom("CellId cannot be zero"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worked_example_matches_doc() {
        // docs/01-spatial-model.md §3.3: world (312.7, −45.2, 1024.0) m at
        // 128 m edge, level 21 → cell coords (2, −1, 8) → 0xA924_9249_2492_4D65.
        let cell = CellId::from_coords(IVec3::new(2, -1, 8), 21).unwrap();
        assert_eq!(cell.0.get(), 0xA924_9249_2492_4D65);
        assert_eq!(cell.level(), 21);

        // Parent (level 20): 0xA924_9249_2492_4D68.
        let parent = cell.parent().unwrap();
        assert_eq!(parent.0.get(), 0xA924_9249_2492_4D68);
        assert_eq!(parent.level(), 20);

        // Three parents → shard cell (level 18): 0xA924_9249_2492_4E00.
        let shard = parent.parent().unwrap().parent().unwrap();
        assert_eq!(shard.0.get(), 0xA924_9249_2492_4E00);
        assert_eq!(shard.level(), 18);

        // Subtree range of the shard.
        assert_eq!(
            shard.subtree_range(),
            0xA924_9249_2492_4C01..=0xA924_9249_2492_4FFF
        );

        // The level-21 cell shares the shard's top 54 bits (parent-is-prefix).
        assert!(shard.is_prefix_of(cell));
        assert_eq!(cell.coords(), (IVec3::new(2, -1, 8), 21));
    }

    #[test]
    fn coords_roundtrip() {
        for level in 0..=CellId::MAX_LEVEL {
            if level == 0 {
                let cell = CellId::from_coords(IVec3::ZERO, 0).unwrap();
                assert_eq!(cell.coords(), (IVec3::ZERO, 0));
                continue;
            }
            let half = 1i64 << (level - 1);
            for c in [-half, -half + 1, 0, half - 2, half - 1] {
                let xyz = IVec3::new(c as i32, 0, c as i32);
                let cell = CellId::from_coords(xyz, level).unwrap();
                assert_eq!(cell.coords(), (xyz, level), "level {level} c {c}");
            }
        }
    }

    #[test]
    fn out_of_range_rejected() {
        assert!(CellId::from_coords(IVec3::ZERO, 22).is_err());
        // At level 0 the range is [−1, 1) → only the origin is valid.
        assert!(CellId::from_coords(IVec3::ZERO, 0).is_ok());
        assert!(CellId::from_coords(IVec3::new(1, 0, 0), 0).is_err());
        // At level 1 the range is [−1, 1).
        assert!(CellId::from_coords(IVec3::new(1, 0, 0), 1).is_err());
        assert!(CellId::from_coords(IVec3::new(-1, 0, 0), 1).is_ok());
    }

    #[test]
    fn parent_chain_reaches_root() {
        let cell = CellId::from_coords(IVec3::new(2, -1, 8), 21).unwrap();
        let mut c = cell;
        let mut levels = Vec::new();
        while let Some(p) = c.parent() {
            levels.push(c.level());
            c = p;
        }
        assert_eq!(c, CellId::ROOT);
        assert_eq!(c.level(), 0);
        assert_eq!(levels, (1..=21).rev().collect::<Vec<_>>());
    }

    #[test]
    fn children_are_under_parent() {
        let cell = CellId::from_coords(IVec3::new(2, -1, 8), 20).unwrap();
        for child in cell.children() {
            assert_eq!(child.level(), 21);
            assert_eq!(child.parent(), Some(cell));
            assert!(cell.is_prefix_of(child));
        }
        // Children are distinct and cover the 8 sub-octants.
        let mut seen = std::collections::HashSet::new();
        for child in cell.children() {
            assert!(seen.insert(child));
        }
        assert_eq!(seen.len(), 8);
    }

    #[test]
    fn neighbors27_include_self_and_are_same_level() {
        let cell = CellId::from_coords(IVec3::new(2, -1, 8), 21).unwrap();
        let n = cell.neighbors27();
        assert_eq!(n.len(), 27);
        assert!(n.contains(&cell));
        for c in n {
            assert_eq!(c.level(), 21);
        }
    }

    #[test]
    fn interest_and_shard_level_constants() {
        assert_eq!(INTEREST_LEVEL, CellId::MAX_LEVEL);
        assert_eq!(INTEREST_LEVEL, 21);
        assert_eq!(SHARD_LEVEL, 18);
        assert_eq!(INTEREST_LEVEL - SHARD_LEVEL, 3);
    }

    #[test]
    fn shard_of_at_interest_level() {
        let cell = CellId::from_coords(IVec3::new(2, -1, 8), INTEREST_LEVEL).unwrap();
        let s = shard_of(cell);
        assert_eq!(s.level(), SHARD_LEVEL);
        assert!(s.is_prefix_of(cell));
    }

    #[test]
    fn default_cell_edge_is_128() {
        assert_eq!(DEFAULT_CELL_EDGE_M, 128.0);
    }

    #[test]
    fn metres_conversion_roundtrip_at_origin() {
        // The origin cell at (0,0,0) should give metre-space min corner (0,0,0)
        // and its centre is at (64, 64, 64).
        let cell = cell_id_from_metres(glam::DVec3::ZERO, DEFAULT_CELL_EDGE_M).unwrap();
        assert_eq!(cell.coords(), (IVec3::ZERO, INTEREST_LEVEL));
        let min_corner = metres_from_cell_id(cell, DEFAULT_CELL_EDGE_M);
        assert_eq!(min_corner, glam::DVec3::ZERO);
    }

    #[test]
    fn metres_conversion_known_cell() {
        // The worked example at (312.7, −45.2, 1024.0) m with 128 m edge
        // → cell coords (2, -1, 8).
        let cell = cell_id_from_metres(glam::DVec3::new(312.7, -45.2, 1024.0), DEFAULT_CELL_EDGE_M)
            .unwrap();
        assert_eq!(cell.coords(), (IVec3::new(2, -1, 8), INTEREST_LEVEL));

        // Min corner: (2*128, -1*128, 8*128) = (256, -128, 1024).
        let min_corner = metres_from_cell_id(cell, DEFAULT_CELL_EDGE_M);
        assert_eq!(min_corner, glam::DVec3::new(256.0, -128.0, 1024.0));
    }

    #[test]
    fn metres_conversion_rejects_too_large() {
        // At level 21, the range is [−2^20, 2^20) per axis = [−1_048_576, 1_048_576).
        // At 128 m/edge: [−134_217_728, 134_217_728) m.
        // A value at the far end should produce an error naming the offending
        // value and the representable extent.
        let too_far = 150_000_000_f64;
        let err = cell_id_from_metres(glam::DVec3::new(too_far, 0.0, 0.0), DEFAULT_CELL_EDGE_M)
            .unwrap_err();
        assert!(
            matches!(
                err,
                CellRangeError::CoordOutOfRange {
                    coord: 1_171_875,
                    level: 21
                }
            ),
            "expected CoordOutOfRange near cell coord 1_171_875, got {err:?}"
        );

        // Negative side.
        let err = cell_id_from_metres(glam::DVec3::new(0.0, -too_far, 0.0), DEFAULT_CELL_EDGE_M)
            .unwrap_err();
        assert!(
            matches!(err, CellRangeError::CoordOutOfRange { .. }),
            "expected CoordOutOfRange for negative value, got {err:?}"
        );
    }

    #[test]
    fn metres_conversion_rejects_negative_extreme() {
        // Just past the negative bound: -134_217_728 m is exactly at
        // -1_048_576 cells which is the half-exclusive lower bound.
        // At 128 m/edge: -134_217_728 / 128 = -1_048_576 which is exclusive.
        // So -134_217_729 m → cell coord -1_048_577 which is out of range.
        let negative_extreme = -134_217_729_f64;
        let err = cell_id_from_metres(
            glam::DVec3::new(0.0, 0.0, negative_extreme),
            DEFAULT_CELL_EDGE_M,
        )
        .unwrap_err();
        // cell coord = floor(-1_048_576.0078) = -1_048_577
        assert!(
            matches!(
                err,
                CellRangeError::CoordOutOfRange {
                    coord: -1_048_577,
                    level: 21
                }
            ),
            "expected CoordOutOfRange(-1_048_577), got {err:?}"
        );
    }

    #[test]
    fn metres_conversion_error_naming_convention() {
        // The error message should name the offending value.
        let err = cell_id_from_metres(
            glam::DVec3::new(999_999_999.0, 0.0, 0.0),
            DEFAULT_CELL_EDGE_M,
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("7812499"),
            "error message should name the offending cell coord, got: {msg}"
        );
    }

    #[test]
    fn shard_level_is_interest_minus_three() {
        assert_eq!(SHARD_LEVEL, 18);
        assert_eq!(INTEREST_LEVEL - SHARD_LEVEL, 3);
    }

    #[test]
    fn metres_from_cell_id_is_min_corner() {
        // Cell at (1, 2, 3) at level 21 with default edge: min corner is
        // (128, 256, 384).
        let cell = CellId::from_coords(IVec3::new(1, 2, 3), 21).unwrap();
        let m = metres_from_cell_id(cell, DEFAULT_CELL_EDGE_M);
        assert_eq!(m, glam::DVec3::new(128.0, 256.0, 384.0));

        // Centre would be (128+64, 256+64, 384+64) = (192, 320, 448).
        // Document that we return the min corner.
        let centre = m + glam::DVec3::splat(DEFAULT_CELL_EDGE_M / 2.0);
        assert_eq!(centre, glam::DVec3::new(192.0, 320.0, 448.0));
    }

    #[test]
    fn metres_reject_at_non_default_edge() {
        // With a larger cell edge (1 km), the extent is much larger.
        let cell_edge = 1_024_000_f64; // ~7.18 AU half-extent
                                      // This should still be in range.
        let in_range = cell_id_from_metres(glam::DVec3::new(1e11, 0.0, 0.0), cell_edge).unwrap();
        assert_eq!(in_range.level(), INTEREST_LEVEL);

        // But ~1 AU extra might still be in range with this edge size.
        // The half-extent is 2^20 * 1_024_000 ≈ 1.07e12 m. 1e11 is fine.
        // Let's try something well beyond: 2e12 m should exceed it.
        let out = cell_id_from_metres(glam::DVec3::new(2e12, 0.0, 0.0), cell_edge);
        assert!(out.is_err(), "2e12 m should exceed even 1 km/edge extent");
    }

    #[test]
    fn f64_precision_roundtrips_near_extent() {
        // At the solar-grid edge (cell_edge_m = 4_096_000, half-extent
        // = 2^20 * 4_096_000 = 4.3e12 m), an f32 Vec3 would lose the
        // low bits of the coordinate — the no-clamping failure mode.
        // With f64 the conversion must round-trip correctly.
        let solar_edge = 4_096_000_f64;
        // A metre value near the extent: 4 trillion metres on x, specific
        // low bits on y and z to test precision.
        let metres = glam::DVec3::new(4_000_000_000_000.0, 123_456.789, -987_654.321);
        let cell = cell_id_from_metres(metres, solar_edge).unwrap();
        let back = metres_from_cell_id(cell, solar_edge);
        // The round-trip lands on the cell's min corner. The original metre
        // value might be anywhere inside the cell, but min corner should be
        // at most one cell edge below the original.
        for axis in 0..3 {
            let orig = metres.to_array()[axis];
            let min_corner = back.to_array()[axis];
            assert!(
                min_corner <= orig,
                "min corner {min_corner} must not exceed original {orig} for axis {axis}"
            );
            assert!(
                orig - min_corner < solar_edge,
                "original {orig} must be less than one cell edge ({solar_edge}) above min corner {min_corner}"
            );
        }
    }
}
