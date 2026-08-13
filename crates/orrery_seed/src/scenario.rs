//! The TOML scenario file (docs/12-world-seeding.md §5).
//!
//! Parses the §5.2 tables with `#[serde(deny_unknown_fields)]` on every
//! struct — V1 (§10) requires unknown keys be **errors**, not warnings: a
//! typo'd generator param must not silently take a default.
//!
//! Three conventions from §5.1 are load-bearing here:
//!
//! - **`CellRef`** has three spellings with one canonical form: the hex
//!   `to_bits` string (what the tool prints, pasting straight back), the
//!   `{level, xyz}` authoring form, and the `{level, m}` metres form
//!   (grid-local, via the non-clamping `cell_id_from_metres`).
//! - **`Bounds`** has five shapes; `box`/`sphere` snap **outward** to whole
//!   cells and the snap is reported.
//! - **The seeder never clamps**: out-of-range is an error naming the
//!   offending value, not a silent clamp (§5.1).

use std::collections::BTreeMap;
use std::ops::RangeInclusive;

use orrery_protocol::cell::CellRangeError;
use orrery_protocol::{cell_id_from_metres, CellId, GridId, DEFAULT_CELL_EDGE_M};
use serde::Deserialize;

use crate::field::DEFAULT_FIELD_CLAMP;

/// The current scenario schema version (docs/12 §5.2: `schema = 1` is the
/// required first key; a breaking surface change bumps it).
pub const CURRENT_SCHEMA: u32 = 1;

/// The default derivation context, re-exported for scenario defaults.
pub use crate::seedtree::DEFAULT_CONTEXT;

/// A scenario file, exactly as parsed (docs/12 §5.2). Everything is present
/// at parse level; *validity* (backward-only accumulator references, uniform
/// only in v1, …) is checked in [`Scenario::resolve`].
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Scenario {
    /// Required first key: the schema version.
    pub schema: u32,
    /// Identity and description.
    pub scenario: ScenarioMeta,
    /// The seed root and RNG choice.
    pub seed: SeedSection,
    /// The payload class gate.
    #[serde(default)]
    pub payload: PayloadSection,
    /// Additional grids (grid 0 is implicit at the D16 default edge).
    #[serde(default)]
    pub grid: Vec<GridDecl>,
    /// The component-payload tables (`[archetype.<name>]`).
    #[serde(default)]
    pub archetype: BTreeMap<String, ArchetypeDecl>,
    /// Density layers, evaluated in declaration order (§5.3).
    #[serde(default)]
    pub layer: Vec<LayerDecl>,
    /// Realizations: accumulator → rows (§5.4).
    #[serde(default)]
    pub emit: Vec<EmitDecl>,
    /// Global targets and tolerance (§7.2).
    #[serde(default)]
    pub target: TargetSection,
    /// Hard guards checked before any write (§10, V10).
    #[serde(default)]
    pub limits: LimitsSection,
    /// Transaction shape for the write path (§11.1). Parsed and carried;
    /// v1's `plan` never writes.
    #[serde(default)]
    pub load: LoadSection,
    /// Named overlays (`[profile.<name>]`, §5.2; C-5).
    #[serde(default)]
    pub profile: BTreeMap<String, toml::Table>,
    /// The rig seam (§12.3): world + load as one artifact. Parsed, carried,
    /// not consumed by `plan`.
    #[serde(default)]
    pub workload: Vec<toml::Table>,
}

/// `[scenario]` (docs/12 §5.2).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScenarioMeta {
    /// The scenario name: a `ContentKey` component (§9.1).
    pub name: String,
    /// The content build string recorded in `content/version` (§9.3).
    pub content_build: Option<String>,
    /// Free text.
    pub description: Option<String>,
    /// A single non-recursive `extends` (§5.2). Path resolution happens at
    /// load time; the merged document is what gets parsed here.
    pub extends: Option<String>,
    /// Absolute-quantity multiplier (§13.4). Parsed; scaling itself is out
    /// of scope in v1.
    pub scale: Option<f64>,
    /// Which ratio `scale` holds fixed (§13.4). Out of scope in v1.
    pub scale_mode: Option<String>,
}

/// `[seed]` (docs/12 §5.2, §8 item 5, D-D).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SeedSection {
    /// The scenario seed — the **public** root (D-D), committed to git,
    /// printed in every report. The literal string `"random"` draws from the
    /// OS and prints the copy-pasteable form as the first line of output
    /// (§8 item 5).
    pub scenario: String,
    /// The blake3 derivation context (§8 item 2). Defaults to
    /// `"orrery.seeder.v1"` ([`DEFAULT_CONTEXT`]).
    pub context: Option<String>,
    /// RNG choice (§5.2). Only `chacha8` exists; the key is reserved so a
    /// future RNG is an explicit surface change.
    pub rng: Option<String>,
}

/// `[payload]` (docs/12 §4.1, §5.2).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PayloadSection {
    /// `opaque` (the shipped binary's filler) or `ruleset` (a linked
    /// `SeedEncoder`; `apply` refuses without `--allow-opaque` otherwise).
    pub class: Option<String>,
    /// Bag schema version recorded in the opaque header (§4.1).
    pub schema_version: Option<u16>,
}

/// `[[grid]]` (docs/12 §5.1–§5.2). Grid 0 is implicit.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GridDecl {
    /// The grid id (`GridId`).
    pub id: u32,
    /// Interest-cell edge in metres; the only thing giving `CellId` a scale
    /// (§5.1). Defaults to the D16 value, 128.0.
    pub cell_edge_m: Option<f64>,
    /// Parent grid for nested frames (§5.2). v1 writes grid 0 only.
    pub parent: Option<u32>,
}

/// `[archetype.<name>]` (docs/12 §5.5).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArchetypeDecl {
    /// Component names, informational for the opaque path (the game's
    /// `SeedEncoder` is what interprets them).
    pub components: Option<Vec<String>>,
    /// Declared bag size with a unit suffix (§5.1: `"256B"`, `"192B"`) — the
    /// byte-estimation input. Suffixed-scalar form only; a bare integer is a
    /// parse error so the unit is never ambiguous.
    pub declared_size: Option<String>,
    /// Per-archetype schema version override (defaults to `[payload]`).
    pub schema_version: Option<u16>,
    /// The hex escape hatch: `bytes = "0x…"`, capped at 4 KiB (§4.1).
    pub bytes: Option<String>,
    /// Passed through to `SeedEncoder::encode` as `ArchetypeFields`; the
    /// seeder does not interpret it (§5.5).
    pub fields: Option<toml::Table>,
}

/// `[[layer]]` (docs/12 §5.3).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LayerDecl {
    /// Unique layer name: the seed-tree tag and a `ContentKey` component.
    pub name: String,
    /// Generator discriminant (§6). v1 implements `uniform` only.
    pub kind: String,
    /// The fold op (§5.3). Default `union`; v1 accepts only `union`.
    pub op: Option<String>,
    /// The accumulator to fold into (default `"main"`, §5.3).
    pub into: Option<String>,
    /// Where the field is non-zero (§5.1). Default `all`… which v1 rejects
    /// for entity emits (V6-adjacent: a v1 uniform layer needs a bounded
    /// region to have a mass oracle over).
    pub bounds: Option<BoundsSpec>,
    /// The level this layer's field is defined at (§5.3). Default 21.
    pub level: Option<u8>,
    /// How a coarse layer pushes mass down (§5.3). Out of scope in v1.
    pub spread: Option<String>,
    /// Blend coefficient (§5.3).
    pub weight: Option<f64>,
    /// Set false to disable a layer without deleting it.
    pub enabled: Option<bool>,
    /// The post-fold clamp (§5.3, default 64.0).
    pub field_clamp: Option<f64>,
    /// `where` predicate for `conditional` (§5.3). Out of scope in v1.
    #[serde(rename = "where")]
    pub where_predicate: Option<String>,
    /// Generator parameters (`[layer.params]`). v1's `uniform` reads
    /// `intensity` only.
    pub params: Option<toml::Table>,
    /// Secret-content routing (§8). Out of scope in v1.
    pub secret: Option<bool>,
}

/// `[[emit]]` (docs/12 §5.4).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EmitDecl {
    /// Unique emit name: a `ContentKey` component (§9.1).
    pub name: String,
    /// The accumulator to realize (default `"main"`).
    pub from: Option<String>,
    /// `entity` (default) or `terrain`. v1 implements `entity` only.
    pub kind: Option<String>,
    /// The exact count (D-B). Required for an entity emit.
    pub count: Option<u64>,
    /// The emit level (default 21).
    pub level: Option<u8>,
    /// `hash` (default), `stratified`, or `centered` (§5.4). v1 implements
    /// `hash` only.
    pub placement: Option<String>,
    /// The archetype mix: `{ name = weight, … }`, apportioned per cell by
    /// largest remainder (§5.5).
    pub archetypes: Option<BTreeMap<String, f64>>,
    /// Terrain conflict policy (§5.3). Out of scope in v1 (no terrain).
    pub on_conflict: Option<String>,
}

/// `[target]` (docs/12 §7.2).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TargetSection {
    /// Exact count target (D-B).
    pub count: Option<u64>,
    /// Fraction of entities in the hottest shard.
    pub hot_shard_share: Option<f64>,
    /// Number of deliberate hotspots.
    pub hotspots: Option<u64>,
    /// Gini of the per-cell count distribution.
    pub gini: Option<f64>,
    /// Fraction of cells occupied (§A.3.3).
    pub occupied_fraction: Option<f64>,
    /// Storage-cost inversion target (§7.2). Out of scope in v1.
    pub max_bytes: Option<String>,
    /// Tolerance for the non-exact targets.
    pub tolerance: Option<f64>,
    /// Which knob the solver may move (§7.2). Out of scope in v1.
    pub solve: Option<toml::Table>,
    /// Hot-shard placement (§11.3). Out of scope in v1.
    pub hot_shard_placement: Option<String>,
}

/// `[limits]` (docs/12 §10, V10).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LimitsSection {
    /// Hard entity ceiling.
    pub max_entities: Option<u64>,
    /// Hard byte ceiling (suffixed scalar, §5.1).
    pub max_bytes: Option<String>,
    /// Hard wall-clock ceiling (suffixed scalar).
    pub max_wall_clock: Option<String>,
    /// The production-wipe guard (§9.5).
    pub protect: Option<bool>,
}

/// `[load]` (docs/12 §11.1). v1's `plan` never writes; these are validated
/// for shape and carried.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LoadSection {
    /// `offline` (default) | `quiesce` | `online` (§11.4).
    pub mode: Option<String>,
    /// Target bytes per transaction (§11.1: 768 KiB).
    pub txn_bytes: Option<String>,
    /// In-flight transaction concurrency.
    pub concurrency: Option<u32>,
    /// `shuffled` dispatch across pre-split boundaries (§11.2, #11510).
    pub dispatch: Option<String>,
}

/// The resolved `fields` of one archetype, handed to `SeedEncoder::encode`
/// as `ArchetypeFields` (docs/12 §4.1, §5.5).
#[derive(Debug, Clone)]
pub struct ArchetypeFields {
    /// Declared bag size in bytes (parsed from the suffixed scalar).
    pub declared_size_bytes: Option<usize>,
    /// Effective schema version (archetype override else `[payload]`).
    pub schema_version: u16,
    /// The hex escape hatch, verbatim (decoded by the encoder at use).
    pub bytes_hex: Option<String>,
    /// The uninterpreted `[archetype.<name>.fields]` table (§5.5).
    pub table: toml::Table,
}

/// A `CellRef` (docs/12 §5.1): three spellings, one canonical.
#[derive(Debug, Clone, PartialEq)]
pub enum CellRef {
    /// `"0xA92492492493D600"` — canonical raw bits, big-endian `to_bits()`.
    Bits(u64),
    /// `{ level = 18, xyz = [3, -2, 5] }` — the authoring form.
    Xyz {
        /// The cell's level.
        level: u8,
        /// Signed cell coordinates at `level`.
        xyz: [i32; 3],
    },
    /// `{ level = 21, m = [384.0, -256.0, 640.0] }` — metres, grid-local.
    Metres {
        /// Must be the interest level (21): the metre conversion produces an
        /// interest cell (§5.1).
        level: u8,
        /// Grid-local metres.
        m: [f64; 3],
    },
}

impl<'de> Deserialize<'de> for CellRef {
    /// Hand-written because the three forms have disjoint shapes (a string,
    /// or a table with either `xyz` or `m`), and the error must name the
    /// accepted forms (§10: errors name the fix).
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct Visitor;
        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = CellRef;

            fn expecting(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
                f.write_str(
                    "a CellRef: hex bits string, { level, xyz = [x,y,z] }, or { level, m = [x,y,z] } (docs/12 §5.1)",
                )
            }

            fn visit_str<E>(self, v: &str) -> Result<CellRef, E>
            where
                E: serde::de::Error,
            {
                let s = v.strip_prefix("0x").unwrap_or(v);
                u64::from_str_radix(s, 16)
                    .map(CellRef::Bits)
                    .map_err(|e| E::custom(format!("CellRef hex bits: {e}")))
            }

            fn visit_map<A>(self, mut map: A) -> Result<CellRef, A::Error>
            where
                A: serde::de::MapAccess<'de>,
            {
                let mut level: Option<u8> = None;
                let mut xyz: Option<[i32; 3]> = None;
                let mut m: Option<[f64; 3]> = None;
                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "level" => {
                            if level.is_some() {
                                return Err(serde::de::Error::duplicate_field("level"));
                            }
                            level = Some(map.next_value()?);
                        }
                        "xyz" => {
                            if xyz.is_some() {
                                return Err(serde::de::Error::duplicate_field("xyz"));
                            }
                            xyz = Some(map.next_value()?);
                        }
                        "m" => {
                            if m.is_some() {
                                return Err(serde::de::Error::duplicate_field("m"));
                            }
                            m = Some(map.next_value()?);
                        }
                        other => {
                            return Err(serde::de::Error::unknown_field(
                                other,
                                &["level", "xyz", "m"],
                            ));
                        }
                    }
                }
                let level = level.ok_or_else(|| serde::de::Error::missing_field("level"))?;
                match (xyz, m) {
                    (Some(xyz), None) => Ok(CellRef::Xyz { level, xyz }),
                    (None, Some(m)) => Ok(CellRef::Metres { level, m }),
                    (None, None) => Err(serde::de::Error::custom(
                        "CellRef table needs `xyz` or `m` (docs/12 §5.1)",
                    )),
                    (Some(_), Some(_)) => Err(serde::de::Error::custom(
                        "CellRef table takes `xyz` or `m`, not both (docs/12 §5.1)",
                    )),
                }
            }
        }
        deserializer.deserialize_any(Visitor)
    }
}

impl CellRef {
    /// Resolve to a [`CellId`] against a grid's `cell_edge_m` (docs/12 §5.1).
    /// Never clamps: out-of-range is an error naming the offending value.
    ///
    /// # Errors
    ///
    /// Returns [`CellRangeError`] for an out-of-range coordinate or level.
    pub fn resolve(&self, cell_edge_m: f64) -> Result<CellId, CellRangeError> {
        match *self {
            CellRef::Bits(bits) => CellId::from_bits(bits).ok_or(CellRangeError::CoordOutOfRange {
                coord: 0,
                level: 0,
            }),
            CellRef::Xyz { level, xyz } => {
                CellId::from_cell_coords(glam::IVec3::new(xyz[0], xyz[1], xyz[2]), level)
            }
            CellRef::Metres { level, m } => {
                if level != orrery_protocol::INTEREST_LEVEL {
                    return Err(CellRangeError::LevelOutOfRange { level });
                }
                cell_id_from_metres(glam::DVec3::new(m[0], m[1], m[2]), cell_edge_m)
            }
        }
    }
}

/// A `Bounds` shape (docs/12 §5.1).
#[derive(Debug, Clone, PartialEq)]
pub enum BoundsSpec {
    /// `"all"` — the grid extent. Only legal for operators that do not need
    /// a normalization sweep (V6); v1 rejects it for the uniform entity
    /// path because 2^63 cells is not a plannable region.
    All,
    /// `{ kind = "subtree", cell = <CellRef> }` — one cell's subtree.
    Subtree {
        /// The subtree root.
        cell: CellRef,
    },
    /// `{ kind = "cells", level = 21, min = […], max = […] }` — an inclusive
    /// cell-coordinate box at one level.
    Cells {
        /// The level both corners are expressed at.
        level: u8,
        /// Inclusive minimum corner (signed cell coords).
        min: [i32; 3],
        /// Inclusive maximum corner (signed cell coords).
        max: [i32; 3],
    },
    /// `{ kind = "box", center = <CellRef>, extent_cells = […] }` — a
    /// cell-aligned box by half-extent (§5.1: `extent_cells` is a
    /// half-extent, so `[64,8,64]` is 128×16×128 cells).
    Box {
        /// The box centre.
        center: CellRef,
        /// Half-extent in cells per axis.
        extent_cells: [u32; 3],
    },
    /// `{ kind = "sphere", center = <CellRef>, radius_m = 8192.0 }` — a
    /// sphere snapped **outward** to whole cells (§5.1); the snap is
    /// reported in [`ResolvedBounds::snap`].
    Sphere {
        /// The sphere centre.
        center: CellRef,
        /// Radius in grid-local metres.
        radius_m: f64,
    },
}

impl<'de> Deserialize<'de> for BoundsSpec {
    /// Hand-written because `deny_unknown_fields` does not reach through
    /// internally-tagged enums (the known gap this compensates for), and V1
    /// requires unknown keys be errors everywhere.
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct Visitor;
        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = BoundsSpec;

            fn expecting(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
                f.write_str(
                    "a Bounds: \"all\", or { kind = \"subtree\"|\"cells\"|\"box\"|\"sphere\", … } (docs/12 §5.1)",
                )
            }

            fn visit_str<E>(self, v: &str) -> Result<BoundsSpec, E>
            where
                E: serde::de::Error,
            {
                match v {
                    "all" => Ok(BoundsSpec::All),
                    other => Err(E::unknown_variant(other, &["all"])),
                }
            }

            fn visit_map<A>(self, mut map: A) -> Result<BoundsSpec, A::Error>
            where
                A: serde::de::MapAccess<'de>,
            {
                let mut kind: Option<String> = None;
                let mut cell: Option<CellRef> = None;
                let mut center: Option<CellRef> = None;
                let mut level: Option<u8> = None;
                let mut min: Option<[i32; 3]> = None;
                let mut max: Option<[i32; 3]> = None;
                let mut extent_cells: Option<[u32; 3]> = None;
                let mut radius_m: Option<f64> = None;
                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "kind" => kind = Some(map.next_value()?),
                        "cell" => cell = Some(map.next_value()?),
                        "center" => center = Some(map.next_value()?),
                        "level" => level = Some(map.next_value()?),
                        "min" => min = Some(map.next_value()?),
                        "max" => max = Some(map.next_value()?),
                        "extent_cells" => extent_cells = Some(map.next_value()?),
                        "radius_m" => radius_m = Some(map.next_value()?),
                        other => {
                            return Err(serde::de::Error::unknown_field(
                                other,
                                &[
                                    "kind",
                                    "cell",
                                    "center",
                                    "level",
                                    "min",
                                    "max",
                                    "extent_cells",
                                    "radius_m",
                                ],
                            ));
                        }
                    }
                }
                let kind = kind.ok_or_else(|| serde::de::Error::missing_field("kind"))?;
                match kind.as_str() {
                    "subtree" => {
                        let cell = cell.ok_or_else(|| serde::de::Error::missing_field("cell"))?;
                        Ok(BoundsSpec::Subtree { cell })
                    }
                    "cells" => {
                        let level =
                            level.ok_or_else(|| serde::de::Error::missing_field("level"))?;
                        let min = min.ok_or_else(|| serde::de::Error::missing_field("min"))?;
                        let max = max.ok_or_else(|| serde::de::Error::missing_field("max"))?;
                        Ok(BoundsSpec::Cells { level, min, max })
                    }
                    "box" => {
                        let center =
                            center.ok_or_else(|| serde::de::Error::missing_field("center"))?;
                        let extent_cells = extent_cells
                            .ok_or_else(|| serde::de::Error::missing_field("extent_cells"))?;
                        Ok(BoundsSpec::Box {
                            center,
                            extent_cells,
                        })
                    }
                    "sphere" => {
                        let center =
                            center.ok_or_else(|| serde::de::Error::missing_field("center"))?;
                        let radius_m =
                            radius_m.ok_or_else(|| serde::de::Error::missing_field("radius_m"))?;
                        Ok(BoundsSpec::Sphere { center, radius_m })
                    }
                    other => Err(serde::de::Error::unknown_variant(
                        other,
                        &["subtree", "cells", "box", "sphere"],
                    )),
                }
            }
        }
        deserializer.deserialize_any(Visitor)
    }
}

/// A resolved bounds: an axis-aligned box of whole cells at the layer's
/// level (docs/12 §5.1). `box`/`sphere` snap **outward** to this; the snap
/// is reported.
#[derive(Debug, Clone)]
pub struct ResolvedBounds {
    /// The level the box is expressed at.
    pub level: u8,
    /// Inclusive minimum cell coords per axis.
    pub min: [i32; 3],
    /// Inclusive maximum cell coords per axis.
    pub max: [i32; 3],
    /// What the snap did, for the report (§5.1: "the snap is reported").
    /// `None` when the bounds were already cell-aligned.
    pub snap: Option<String>,
}

impl ResolvedBounds {
    /// Number of whole cells per axis.
    #[must_use]
    pub fn extent(&self) -> [u64; 3] {
        let mut out = [0u64; 3];
        for (axis, o) in out.iter_mut().enumerate() {
            *o = (i64::from(self.max[axis]) - i64::from(self.min[axis]) + 1).max(0) as u64;
        }
        out
    }

    /// Total cell count of the box.
    #[must_use]
    pub fn cell_count(&self) -> u64 {
        let e = self.extent();
        e[0] * e[1] * e[2]
    }

    /// Whether `cell`'s coords lie inside the box (levels must match).
    #[must_use]
    pub fn contains(&self, cell: CellId) -> bool {
        let (coords, level) = cell.coords();
        if level != self.level {
            return false;
        }
        let c = [coords.x, coords.y, coords.z];
        (0..3).all(|a| c[a] >= self.min[a] && c[a] <= self.max[a])
    }

    /// The bounds as a [`CellId`] range cover: the smallest set of level-N
    /// ancestors whose union of subtrees covers the box. For the v1 uniform
    /// path the box is walked by the splitter, which needs an
    /// `is_prefix_of`-style test — see [`ResolvedBounds::field_mass_under`].
    #[must_use]
    pub fn subtree_span(&self) -> Option<RangeInclusive<u64>> {
        let min_cell = CellId::from_cell_coords(
            glam::IVec3::new(self.min[0], self.min[1], self.min[2]),
            self.level,
        )
        .ok()?;
        let max_cell = CellId::from_cell_coords(
            glam::IVec3::new(self.max[0], self.max[1], self.max[2]),
            self.level,
        )
        .ok()?;
        // Morton order does not make an arbitrary box one contiguous range,
        // but the box's min-corner cell has the smallest bits and the
        // max-corner cell the largest only for monotone boxes — which a
        // cell-aligned box is, per axis. The *cover* for the field oracle is
        // computed cell-by-cell instead (see `field_mass_under`); this span
        // is informational (used for reporting).
        Some(min_cell.to_bits()..=max_cell.to_bits())
    }

    /// The quantized mass a uniform field of `intensity` per cell
    /// contributes under `cell` (the O(depth) oracle, docs/12 §7.1):
    /// `intensity ×` the number of level-N box cells inside `cell`'s
    /// subtree, all in Q16.16 (§8.3).
    #[must_use]
    pub fn field_mass_under(&self, cell: CellId, intensity: crate::field::Q16_16) -> u64 {
        // The number of level-`self.level` cells of the box inside `cell`'s
        // subtree. If `cell` is finer than the box level, it is either one
        // box cell or none.
        let (coords, level) = cell.coords();
        if level > self.level {
            return u64::from(self.contains(cell) as u32) * u64::from(intensity.0.max(0) as u32);
        }
        // Cell is at or above the box level: intersect per axis.
        let shift = u32::from(self.level - level);
        let mut count = 1u128;
        let c = [coords.x, coords.y, coords.z];
        for axis in 0..3 {
            // The subtree of `cell` spans [c*2^shift, (c+1)*2^shift − 1] in
            // level-N coords… except at level 0, whose subtree is the whole
            // volume. `CellId::coords` reports the root as (0,0,0) at level
            // 0; its subtree covers everything.
            let (lo, hi) = if level == 0 {
                (i64::MIN / 2, i64::MAX / 2) // handled below by clamping to the box
            } else {
                let base = i64::from(c[axis]) << shift;
                (base, base + (1i64 << shift) - 1)
            };
            let lo = lo.max(i64::from(self.min[axis]));
            let hi = hi.min(i64::from(self.max[axis]));
            if lo > hi {
                return 0;
            }
            count *= (hi - lo + 1) as u128;
        }
        let per_cell = u128::from(intensity.0.max(0) as u32);
        let total = count * per_cell;
        u64::try_from(total.min(u128::from(u64::MAX))).unwrap_or(u64::MAX)
    }
}

/// A resolved grid: id plus edge.
#[derive(Debug, Clone, Copy)]
pub struct ResolvedGrid {
    /// The grid id.
    pub id: GridId,
    /// Interest-cell edge in metres (§5.1).
    pub cell_edge_m: f64,
}

/// A fully resolved scenario: parsed, defaults applied, v1-checked.
///
/// This is what `plan` consumes. Resolution is where *validation* lives —
/// the parse types above mirror the file, this type mirrors what the file
/// *means*.
#[derive(Debug, Clone)]
pub struct Scenario_ {
    /// The parse-level scenario (kept for error quoting and the config
    /// digest).
    pub raw: Scenario,
    /// Resolved grids by id.
    pub grids: BTreeMap<u32, ResolvedGrid>,
    /// Resolved archetypes by name.
    pub archetypes: BTreeMap<String, ArchetypeFields>,
    /// Resolved layers (uniform-only in v1), in declaration order.
    pub layers: Vec<ResolvedLayer>,
    /// Resolved emits, in declaration order.
    pub emits: Vec<ResolvedEmit>,
    /// The scenario seed material (bytes of the `scenario` string, or the OS
    /// draw for `"random"` — resolved by the caller, which prints it first).
    pub seed_material: Vec<u8>,
    /// The derivation context (§8 item 2).
    pub seed_context: String,
}

/// A resolved layer (v1: uniform + union only).
#[derive(Debug, Clone)]
pub struct ResolvedLayer {
    /// Layer name.
    pub name: String,
    /// The field level (docs/12 §5.3, default 21).
    pub level: u8,
    /// The resolved bounds.
    pub bounds: ResolvedBounds,
    /// The accumulator the layer folds into (default `"main"`).
    pub into: String,
    /// Uniform per-cell intensity (quantized, §8.3).
    pub intensity: crate::field::Q16_16,
    /// The post-fold clamp (§5.3).
    pub field_clamp: crate::field::Q16_16,
    /// The grid the layer's bounds resolve against.
    pub grid: GridId,
    /// The grid's cell edge (for metre forms).
    pub cell_edge_m: f64,
}

/// A resolved emit (v1: entity + hash placement only).
#[derive(Debug, Clone)]
pub struct ResolvedEmit {
    /// Emit name.
    pub name: String,
    /// Accumulator realized (default `"main"`).
    pub from: String,
    /// The exact count (D-B).
    pub count: u64,
    /// The emit level (default 21).
    pub level: u8,
    /// The archetype mix, sorted by name (§8.4) with non-negative weights.
    pub archetypes: Vec<(String, f64)>,
    /// The grid the emit realizes into (v1: grid 0).
    pub grid: GridId,
}

/// A scenario error: carries the `toml::de::Error` span when the failure is
/// a parse failure (docs/12 §10: errors quote the config span).
#[derive(Debug)]
pub enum ScenarioError {
    /// TOML parse error with span.
    Parse(toml::de::Error),
    /// A semantic error (validation), with an optional byte span into the
    /// source.
    Semantic {
        /// What failed, naming the fix (§10).
        message: String,
        /// Byte span into the source when known.
        span: Option<core::ops::Range<usize>>,
    },
}

impl core::fmt::Display for ScenarioError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Parse(e) => write!(f, "{e}"),
            Self::Semantic { message, .. } => write!(f, "{message}"),
        }
    }
}

impl core::error::Error for ScenarioError {}

impl From<toml::de::Error> for ScenarioError {
    fn from(e: toml::de::Error) -> Self {
        Self::Parse(e)
    }
}

impl Scenario {
    /// Parse a scenario from TOML text. Parse errors carry their
    /// `toml::de::Error` span (docs/12 §10: errors quote the config).
    ///
    /// # Errors
    ///
    /// Returns [`ScenarioError::Parse`] on a TOML or shape error.
    pub fn parse(source: &str) -> Result<Self, ScenarioError> {
        let scenario: Scenario = toml::from_str(source)?;
        Ok(scenario)
    }

    /// Validate the v1 surface and resolve to a [`Scenario_`].
    ///
    /// This is where the "unsupported in v1" errors live — every out-of-scope
    /// feature is rejected by name here rather than stubbed silently
    /// (`noise`/`zipf`/`cluster`/`ca`/…, the other seven fold ops, the
    /// `where` grammar, `spread`, `stratified`, terrain emits, `scale_mode`).
    ///
    /// `seed_material` is supplied by the caller because `"random"` must be
    /// drawn from the OS and printed **before anything else happens**
    /// (docs/12 §8 item 5) — that side effect belongs to the binary, not the
    /// library.
    ///
    /// # Errors
    ///
    /// Returns [`ScenarioError::Semantic`] naming the first violation.
    pub fn resolve(self, seed_material: Vec<u8>) -> Result<Scenario_, ScenarioError> {
        let err = |message: String| ScenarioError::Semantic {
            message,
            span: None,
        };

        if self.schema != CURRENT_SCHEMA {
            return Err(err(format!(
                "schema = {} is not supported (this build knows schema = {CURRENT_SCHEMA}; docs/12 §5.2)",
                self.schema
            )));
        }

        // Grids: grid 0 implicit at the D16 default.
        let mut grids = BTreeMap::new();
        grids.insert(
            0u32,
            ResolvedGrid {
                id: GridId::ROOT,
                cell_edge_m: DEFAULT_CELL_EDGE_M,
            },
        );
        for g in &self.grid {
            if g.id != 0 && self.grid.iter().filter(|gg| gg.id == g.id).count() > 1 {
                return Err(err(format!("duplicate [[grid]] id {}", g.id)));
            }
            grids.insert(
                g.id,
                ResolvedGrid {
                    id: GridId::new(g.id),
                    cell_edge_m: g.cell_edge_m.unwrap_or(DEFAULT_CELL_EDGE_M),
                },
            );
        }

        let payload_class = self.payload.class.as_deref().unwrap_or("opaque");
        if payload_class != "opaque" && payload_class != "ruleset" {
            return Err(err(format!(
                "[payload] class = {payload_class:?} is not \"opaque\" or \"ruleset\" (docs/12 §4.1)"
            )));
        }

        // Archetypes.
        let mut archetypes = BTreeMap::new();
        for (name, decl) in &self.archetype {
            let declared_size_bytes = match &decl.declared_size {
                Some(s) => Some(parse_byte_size(s).map_err(err)?),
                None => None,
            };
            if let Some(hex) = &decl.bytes {
                let body = hex.strip_prefix("0x").unwrap_or(hex);
                if body.len() / 2 > crate::encode::HEX_ESCAPE_CAP {
                    return Err(err(format!(
                        "archetype {name:?} bytes escape exceeds the {}B cap (docs/12 §4.1)",
                        crate::encode::HEX_ESCAPE_CAP
                    )));
                }
            }
            archetypes.insert(
                name.clone(),
                ArchetypeFields {
                    declared_size_bytes,
                    schema_version: decl
                        .schema_version
                        .or(self.payload.schema_version)
                        .unwrap_or(0),
                    bytes_hex: decl.bytes.clone(),
                    table: decl.fields.clone().unwrap_or_default(),
                },
            );
        }

        // Layers.
        let mut layers = Vec::new();
        for layer in &self.layer {
            if layer.enabled == Some(false) {
                continue;
            }
            if layer.kind != "uniform" {
                return Err(err(format!(
                    "layer {:?}: kind = {:?} is unsupported in v1 — only \"uniform\" is implemented (docs/12 §6; the generator bank lands with its dry-run tier)",
                    layer.name, layer.kind
                )));
            }
            let op = layer.op.as_deref().unwrap_or("union");
            if op != "union" {
                return Err(err(format!(
                    "layer {:?}: op = {op:?} is unsupported in v1 — only \"union\" is implemented (docs/12 §5.3)",
                    layer.name
                )));
            }
            if layer.spread.is_some() {
                return Err(err(format!(
                    "layer {:?}: spread is unsupported in v1 (docs/12 §5.3)",
                    layer.name
                )));
            }
            if layer.where_predicate.is_some() {
                return Err(err(format!(
                    "layer {:?}: the `where` predicate grammar is unsupported in v1 (docs/12 §5.3)",
                    layer.name
                )));
            }
            if layer.secret == Some(true) {
                return Err(err(format!(
                    "layer {:?}: secret content routing is unsupported in v1 (docs/12 §8)",
                    layer.name
                )));
            }
            let into = layer.into.clone().unwrap_or_else(|| "main".to_string());
            let level = layer.level.unwrap_or(orrery_protocol::INTEREST_LEVEL);
            let grid = grids
                .get(&0)
                .copied()
                .expect("grid 0 is implicit");
            let bounds = resolve_bounds(
                layer.bounds.as_ref(),
                level,
                grid.cell_edge_m,
                &layer.name,
            )?;
            let intensity_raw = layer
                .params
                .as_ref()
                .and_then(|p| p.get("intensity"))
                .and_then(|v| v.as_float())
                .unwrap_or(1.0);
            if !intensity_raw.is_finite() || intensity_raw < 0.0 {
                return Err(err(format!(
                    "layer {:?}: intensity must be a non-negative finite f64 (docs/12 §6.1)",
                    layer.name
                )));
            }
            let field_clamp = layer.field_clamp.unwrap_or(DEFAULT_FIELD_CLAMP);
            layers.push(ResolvedLayer {
                name: layer.name.clone(),
                level,
                bounds,
                into,
                intensity: crate::field::Q16_16::from_f64(intensity_raw),
                field_clamp: crate::field::Q16_16::from_f64(field_clamp),
                grid: grid.id,
                cell_edge_m: grid.cell_edge_m,
            });
        }

        // Emits.
        let mut emits = Vec::new();
        for emit in &self.emit {
            let kind = emit.kind.as_deref().unwrap_or("entity");
            if kind != "entity" {
                return Err(err(format!(
                    "emit {:?}: kind = {kind:?} is unsupported in v1 — only \"entity\" is implemented (docs/12 §5.4)",
                    emit.name
                )));
            }
            let placement = emit.placement.as_deref().unwrap_or("hash");
            if placement != "hash" {
                return Err(err(format!(
                    "emit {:?}: placement = {placement:?} is unsupported in v1 — only \"hash\" is implemented (docs/12 §5.4)",
                    emit.name
                )));
            }
            let count = emit.count.ok_or_else(|| {
                err(format!(
                    "emit {:?}: count is required for an entity emit (D-B, docs/12 §5.4)",
                    emit.name
                ))
            })?;
            let archetypes = emit.archetypes.clone().ok_or_else(|| {
                err(format!(
                    "emit {:?}: archetypes is required (docs/12 §5.4)",
                    emit.name
                ))
            })?;
            for name in archetypes.keys() {
                if !archetypes.contains_key(name) {
                    return Err(err(format!(
                        "emit {:?} names archetype {name:?} which has no [archetype.{name}] table (V4)",
                        emit.name
                    )));
                }
            }
            let mut mix: Vec<(String, f64)> = archetypes.into_iter().collect();
            mix.sort_by(|a, b| a.0.cmp(&b.0));
            for (name, w) in &mix {
                if !w.is_finite() || *w < 0.0 {
                    return Err(err(format!(
                        "emit {:?}: archetype {name:?} weight must be a non-negative finite f64",
                        emit.name
                    )));
                }
            }
            emits.push(ResolvedEmit {
                name: emit.name.clone(),
                from: emit.from.clone().unwrap_or_else(|| "main".to_string()),
                count,
                level: emit.level.unwrap_or(orrery_protocol::INTEREST_LEVEL),
                archetypes: mix,
                grid: GridId::ROOT,
            });
        }

        let seed_context = self
            .seed
            .context
            .clone()
            .unwrap_or_else(|| DEFAULT_CONTEXT.to_string());

        Ok(Scenario_ {
            raw: self,
            grids,
            archetypes,
            layers,
            emits,
            seed_material,
            seed_context,
        })
    }
}

/// The resolved-scenario type alias: `Scenario` is the parse type, this is
/// the resolved one. (Named with an underscore to avoid a stutter while the
/// parse type keeps the natural name.)
pub type ResolvedScenario = Scenario_;

/// Resolve a [`BoundsSpec`] to a [`ResolvedBounds`] at `level` (docs/12
/// §5.1). `box`/`sphere` snap **outward** to whole cells and record the snap.
fn resolve_bounds(
    bounds: Option<&BoundsSpec>,
    level: u8,
    cell_edge_m: f64,
    layer_name: &str,
) -> Result<ResolvedBounds, ScenarioError> {
    let err = |message: String| ScenarioError::Semantic {
        message,
        span: None,
    };
    match bounds {
        None | Some(BoundsSpec::All) => Err(err(format!(
            "layer {layer_name:?}: bounds = \"all\" is unsupported in v1 for an entity-emit field — the uniform mass oracle needs a bounded region (docs/12 §5.1, V6)"
        ))),
        Some(BoundsSpec::Subtree { cell }) => {
            let root = cell.resolve(cell_edge_m).map_err(|e| {
                err(format!("layer {layer_name:?}: subtree cell: {e}"))
            })?;
            if root.level() != level {
                return Err(err(format!(
                    "layer {layer_name:?}: subtree root is level {} but the layer is level {level} — v1 needs the bounds at the layer level (docs/12 §5.1)",
                    root.level()
                )));
            }
            let (coords, _) = root.coords();
            Ok(ResolvedBounds {
                level,
                min: [coords.x, coords.y, coords.z],
                max: [coords.x, coords.y, coords.z],
                snap: None,
            })
        }
        Some(BoundsSpec::Cells { level: bl, min, max }) => {
            if *bl != level {
                return Err(err(format!(
                    "layer {layer_name:?}: cells bounds are level {bl} but the layer is level {level}"
                )));
            }
            // Validate corners are in range at the level.
            for c in [min, max] {
                CellId::from_cell_coords(glam::IVec3::new(c[0], c[1], c[2]), level).map_err(
                    |e| err(format!("layer {layer_name:?}: cells corner: {e}")),
                )?;
            }
            Ok(ResolvedBounds {
                level,
                min: *min,
                max: *max,
                snap: None,
            })
        }
        Some(BoundsSpec::Box {
            center,
            extent_cells,
        }) => {
            let center = center.resolve(cell_edge_m).map_err(|e| {
                err(format!("layer {layer_name:?}: box center: {e}"))
            })?;
            let (cc, cl) = center.coords();
            if cl != level {
                return Err(err(format!(
                    "layer {layer_name:?}: box center is level {cl} but the layer is level {level}"
                )));
            }
            let c = [cc.x, cc.y, cc.z];
            let mut min = [0i32; 3];
            let mut max = [0i32; 3];
            for axis in 0..3 {
                let e = i64::from(extent_cells[axis]);
                // docs/12 §5.1: `extent_cells` is a half-extent and the full
                // extent is exactly 2·e ("[64,8,64] is 128×16×128 = 262 144
                // cells"), so the box spans [c − e, c + e − 1] — NOT
                // [c − e, c + e], which would be 2·e + 1 wide. The centre
                // cell sits at the upper half's base.
                min[axis] = i32::try_from(i64::from(c[axis]) - e).map_err(|_| {
                    err(format!(
                        "layer {layer_name:?}: box underflows level-{level} coords on axis {axis}"
                    ))
                })?;
                max[axis] = i32::try_from(i64::from(c[axis]) + e - 1).map_err(|_| {
                    err(format!(
                        "layer {layer_name:?}: box overflows level-{level} coords on axis {axis}"
                    ))
                })?;
            }
            // A cell-authored box is already cell-aligned: no snap. The snap
            // record exists for the metre forms.
            Ok(ResolvedBounds {
                level,
                min,
                max,
                snap: None,
            })
        }
        Some(BoundsSpec::Sphere { center, radius_m }) => {
            let center = center.resolve(cell_edge_m).map_err(|e| {
                err(format!("layer {layer_name:?}: sphere center: {e}"))
            })?;
            let (cc, cl) = center.coords();
            if cl != level {
                return Err(err(format!(
                    "layer {layer_name:?}: sphere center is level {cl} but the layer is level {level}"
                )));
            }
            if !radius_m.is_finite() || *radius_m < 0.0 {
                return Err(err(format!(
                    "layer {layer_name:?}: sphere radius_m must be a non-negative finite f64"
                )));
            }
            // Snap OUTWARD (§5.1): the cell box is the sphere's bounding box
            // expanded to whole cells on every side. radius/cell_edge in
            // cells, rounded UP per axis.
            let radius_cells = (radius_m / cell_edge_m).ceil();
            let radius_i = i64::try_from(radius_cells as u128).map_err(|_| {
                err(format!(
                    "layer {layer_name:?}: sphere radius {radius_m} m overflows cell coords"
                ))
            })?;
            let c = [cc.x, cc.y, cc.z];
            let mut min = [0i32; 3];
            let mut max = [0i32; 3];
            for axis in 0..3 {
                min[axis] = i32::try_from(i64::from(c[axis]) - radius_i).map_err(|_| {
                    err(format!(
                        "layer {layer_name:?}: sphere snap underflows coords on axis {axis}"
                    ))
                })?;
                max[axis] = i32::try_from(i64::from(c[axis]) + radius_i).map_err(|_| {
                    err(format!(
                        "layer {layer_name:?}: sphere snap overflows coords on axis {axis}"
                    ))
                })?;
            }
            let snapped = ResolvedBounds {
                level,
                min,
                max,
                snap: Some(format!(
                    "sphere radius {radius_m} m snapped outward to ±{radius_i} cells ({:.3} cell edges)",
                    radius_m / cell_edge_m
                )),
            };
            Ok(snapped)
        }
    }
}

/// Parse a suffixed byte-size scalar (docs/12 §5.1: `"768KiB"`, `"40GiB"`,
/// `"256B"` — the value carries its unit, never the key).
///
/// # Errors
///
/// Returns a message naming the accepted forms.
pub fn parse_byte_size(s: &str) -> Result<usize, String> {
    let s = s.trim();
    let (digits, mult) = if let Some(n) = s.strip_suffix("KiB") {
        (n, 1024usize)
    } else if let Some(n) = s.strip_suffix("MiB") {
        (n, 1024 * 1024)
    } else if let Some(n) = s.strip_suffix("GiB") {
        (n, 1024 * 1024 * 1024)
    } else if let Some(n) = s.strip_suffix('B') {
        (n, 1)
    } else {
        return Err(format!(
            "byte size {s:?} has no unit suffix (expected e.g. \"256B\", \"768KiB\", \"40GiB\"; docs/12 §5.1)"
        ));
    };
    let n: usize = digits
        .trim()
        .replace('_', "")
        .parse()
        .map_err(|e| format!("byte size {s:?}: {e}"))?;
    n.checked_mul(mult)
        .ok_or_else(|| format!("byte size {s:?} overflows usize"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SMOKE: &str = r#"
schema = 1

[scenario]
name          = "smoke"
content_build = "smoke-2026-08-13"
description   = "1k entities in a 4-shard box."

[seed]
scenario = "smoke-v1"

[payload]
class = "opaque"

[archetype.prop]
declared_size = "256B"

[[layer]]
name   = "flat"
kind   = "uniform"
bounds = { kind = "box", center = { level = 21, xyz = [0, 0, 0] }, extent_cells = [8, 1, 8] }

[[emit]]
name       = "props"
from       = "main"
count      = 1_000
archetypes = { prop = 1.0 }
"#;

    #[test]
    fn unknown_key_is_an_error() {
        // V1 (docs/12 §10): unknown keys are errors, not warnings — a typo'd
        // generator param must not silently take a default.
        let bad = SMOKE.replace("kind   = \"uniform\"", "kind   = \"uniform\"\nintensty = 1.0");
        // The typo sits in [[layer]], which denies unknown fields.
        let err = Scenario::parse(&bad).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("unknown field") || msg.contains("intensty"),
            "the typo'd key is named: {msg}"
        );
        match &err {
            ScenarioError::Parse(e) => {
                assert!(e.span().is_some(), "parse errors carry a span (§10)");
            }
            other => panic!("expected a parse error, got {other:?}"),
        }

        // A top-level typo too.
        let bad2 = SMOKE.replace("schema = 1", "schema = 1\nschemma = 1");
        assert!(Scenario::parse(&bad2).is_err());

        // And inside an archetype table.
        let bad3 = SMOKE.replace("declared_size = \"256B\"", "declared_sise = \"256B\"");
        assert!(Scenario::parse(&bad3).is_err());
    }

    #[test]
    fn cellref_hex_roundtrips_through_config() {
        // docs/12 §5.1: the hex form is `to_bits()` big-endian, so tool
        // output pastes straight back into a config.
        let cell = CellId::from_bits(0xA924_9249_2492_4D65).expect("nonzero");
        let toml_src = format!(
            "schema = 1\n[scenario]\nname=\"t\"\n[seed]\nscenario=\"t\"\n[[layer]]\nname=\"l\"\nkind=\"uniform\"\nbounds={{ kind = \"subtree\", cell = \"{cell}\" }}\n[[emit]]\nname=\"e\"\ncount=1\narchetypes={{ a = 1.0 }}\n[archetype.a]\ndeclared_size=\"256B\"\n"
        );
        let sc = Scenario::parse(&toml_src).expect("parses");
        let layer = &sc.layer[0];
        let bounds = layer.bounds.as_ref().expect("bounds present");
        match bounds {
            BoundsSpec::Subtree { cell: CellRef::Bits(bits) } => {
                assert_eq!(*bits, 0xA924_9249_2492_4D65);
            }
            other => panic!("expected hex subtree cell, got {other:?}"),
        }
        // Round-trip: Display prints the canonical hex, which re-parses to
        // the same bits.
        let printed = cell.to_string();
        let back: u64 = u64::from_str_radix(printed.strip_prefix("0x").expect("0x"), 16)
            .expect("hex parses");
        assert_eq!(back, cell.to_bits());
        // And resolving the ref lands the same cell.
        let resolved = CellRef::Bits(cell.to_bits())
            .resolve(DEFAULT_CELL_EDGE_M)
            .expect("resolves");
        assert_eq!(resolved, cell);
    }

    #[test]
    fn cellref_xyz_and_metres_forms() {
        // { level, xyz } — the docs/01 §3.3 worked example.
        let r = CellRef::Xyz {
            level: 21,
            xyz: [2, -1, 8],
        };
        let cell = r.resolve(DEFAULT_CELL_EDGE_M).expect("resolves");
        assert_eq!(cell.to_bits(), 0xA924_9249_2492_4D65);

        // { level, m } — metres, grid-local: (312.7, −45.2, 1024.0) → the
        // same cell (docs/01 §3.3).
        let r = CellRef::Metres {
            level: 21,
            m: [312.7, -45.2, 1024.0],
        };
        let cell = r.resolve(DEFAULT_CELL_EDGE_M).expect("resolves");
        assert_eq!(cell.to_bits(), 0xA924_9249_2492_4D65);

        // A non-interest level in the metres form is an error, not a clamp.
        let r = CellRef::Metres {
            level: 18,
            m: [0.0, 0.0, 0.0],
        };
        assert!(r.resolve(DEFAULT_CELL_EDGE_M).is_err());

        // Out-of-range metres are an error naming the value (§5.1).
        let r = CellRef::Metres {
            level: 21,
            m: [150_000_000.0, 0.0, 0.0],
        };
        let msg = r.resolve(DEFAULT_CELL_EDGE_M).unwrap_err().to_string();
        assert!(msg.contains("1171875"), "names the cell coord: {msg}");
    }

    #[test]
    fn box_extent_is_a_half_extent() {
        // §5.1: extent_cells [64,8,64] is 128×16×128 cells = 262 144.
        let b = ResolvedBounds {
            level: 21,
            min: [-64, -8, -64],
            max: [63, 7, 63],
            snap: None,
        };
        assert_eq!(b.extent(), [128, 16, 128]);
        assert_eq!(b.cell_count(), 262_144);
        // And via the `box` form at center 0: [c−e, c+e−1].
        let spec = BoundsSpec::Box {
            center: CellRef::Xyz {
                level: 21,
                xyz: [0, 0, 0],
            },
            extent_cells: [64, 8, 64],
        };
        let resolved = resolve_bounds(Some(&spec), 21, DEFAULT_CELL_EDGE_M, "t").expect("ok");
        assert_eq!(resolved.min, [-64, -8, -64]);
        assert_eq!(resolved.max, [63, 7, 63]);
        assert_eq!(resolved.cell_count(), 262_144);
    }

    #[test]
    fn sphere_snaps_outward_and_reports() {
        let sc = Scenario::parse(
            r#"
schema = 1
[scenario]
name = "t"
[seed]
scenario = "t"
[[layer]]
name = "l"
kind = "uniform"
bounds = { kind = "sphere", center = { level = 21, xyz = [0,0,0] }, radius_m = 8192.0 }
[[emit]]
name = "e"
count = 1
archetypes = { a = 1.0 }
[archetype.a]
declared_size = "256B"
"#,
        )
        .expect("parses");
        let resolved = sc.resolve(b"t".to_vec()).expect("resolves");
        let layer = &resolved.layers[0];
        // 8192 m / 128 m = 64 cells exactly → ±64.
        assert_eq!(layer.bounds.min, [-64, -64, -64]);
        assert_eq!(layer.bounds.max, [64, 64, 64]);
        assert!(layer.bounds.snap.is_some(), "the snap is reported (§5.1)");
    }

    #[test]
    fn unsupported_features_are_named_errors() {
        let parse = |layer_toml: &str| {
            let src = format!(
                "schema = 1\n[scenario]\nname=\"t\"\n[seed]\nscenario=\"t\"\n[[layer]]\n{layer_toml}\n[[emit]]\nname=\"e\"\ncount=1\narchetypes={{ a = 1.0 }}\n[archetype.a]\ndeclared_size=\"256B\"\n"
            );
            Scenario::parse(&src)
                .expect("parses")
                .resolve(b"t".to_vec())
        };
        // A non-uniform generator names itself.
        let err = parse("name=\"l\"\nkind=\"noise\"\nbounds=\"all\"").unwrap_err();
        assert!(err.to_string().contains("noise"), "{err}");
        assert!(err.to_string().contains("unsupported in v1"), "{err}");
        // A non-union fold names itself.
        let err =
            parse("name=\"l\"\nkind=\"uniform\"\nop=\"mask\"\nbounds=\"all\"").unwrap_err();
        assert!(err.to_string().contains("mask"), "{err}");
        // Stratified placement names itself.
        let mut src = Scenario::parse(
            "schema = 1\n[scenario]\nname=\"t\"\n[seed]\nscenario=\"t\"\n[[layer]]\nname=\"l\"\nkind=\"uniform\"\nbounds={kind=\"cells\",level=21,min=[0,0,0],max=[1,1,1]}\n[[emit]]\nname=\"e\"\ncount=1\nplacement=\"stratified\"\narchetypes={ a = 1.0 }\n[archetype.a]\ndeclared_size=\"256B\"\n",
        )
        .expect("parses");
        src.emit[0].placement = Some("stratified".to_string());
        let err = src.resolve(b"t".to_vec()).unwrap_err();
        assert!(err.to_string().contains("stratified"), "{err}");
    }

    #[test]
    fn bounds_all_is_rejected_for_the_uniform_path() {
        // V6-adjacent (§5.1): "all" at level 21 is 2^63 cells — not a
        // plannable region for the v1 uniform oracle.
        let src = "schema = 1\n[scenario]\nname=\"t\"\n[seed]\nscenario=\"t\"\n[[layer]]\nname=\"l\"\nkind=\"uniform\"\nbounds=\"all\"\n[[emit]]\nname=\"e\"\ncount=1\narchetypes={ a = 1.0 }\n[archetype.a]\ndeclared_size=\"256B\"\n";
        let err = Scenario::parse(src)
            .expect("parses")
            .resolve(b"t".to_vec())
            .unwrap_err();
        assert!(err.to_string().contains("all"), "{err}");
    }

    #[test]
    fn smoke_resolves_with_defaults() {
        let sc = Scenario::parse(SMOKE).expect("parses");
        let resolved = sc.resolve(b"smoke-v1".to_vec()).expect("resolves");
        assert_eq!(resolved.layers.len(), 1);
        assert_eq!(resolved.layers[0].into, "main");
        assert_eq!(resolved.layers[0].level, 21);
        assert_eq!(resolved.layers[0].intensity, crate::field::Q16_16::ONE);
        assert_eq!(
            resolved.layers[0].field_clamp,
            crate::field::Q16_16::from_f64(DEFAULT_FIELD_CLAMP)
        );
        assert_eq!(resolved.emits.len(), 1);
        assert_eq!(resolved.emits[0].count, 1_000);
        assert_eq!(resolved.emits[0].from, "main");
        assert_eq!(resolved.emits[0].level, 21);
        assert_eq!(
            resolved.emits[0].archetypes,
            vec![("prop".to_string(), 1.0)]
        );
        // The box: [8,1,8] half-extent at center (0,0,0) → 16×2×16 = 512.
        assert_eq!(resolved.layers[0].bounds.cell_count(), 512);
    }

    #[test]
    fn parse_byte_size_suffixed_scalars() {
        assert_eq!(parse_byte_size("256B").expect("ok"), 256);
        assert_eq!(parse_byte_size("768KiB").expect("ok"), 768 * 1024);
        assert_eq!(parse_byte_size("128MiB").expect("ok"), 128 * 1024 * 1024);
        assert_eq!(
            parse_byte_size("40GiB").expect("ok"),
            40 * 1024 * 1024 * 1024
        );
        assert!(parse_byte_size("256").is_err(), "a bare integer has no unit");
        assert!(parse_byte_size("10QB").is_err(), "unknown unit");
    }

    #[test]
    fn schema_version_is_checked() {
        let src = SMOKE.replace("schema = 1", "schema = 2");
        let err = Scenario::parse(&src)
            .expect("parses")
            .resolve(b"x".to_vec())
            .unwrap_err();
        assert!(err.to_string().contains("schema"), "{err}");
    }

    #[test]
    fn field_mass_under_counts_box_cells_in_subtree() {
        // The O(depth) oracle: a uniform intensity-1.0 field over a 2×2×2 box
        // at level 21 reports mass 8 under the box's level-20 parent… wait,
        // level 20 parent covers 2^3 cells of level 21 — the box spans
        // (0,0,0)..(1,1,1), whose parent at level 20 is (0,0,0) covering
        // coords 0..1 per axis: exactly the box.
        let bounds = ResolvedBounds {
            level: 21,
            min: [0, 0, 0],
            max: [1, 1, 1],
            snap: None,
        };
        let parent = CellId::from_cell_coords(glam::IVec3::new(0, 0, 0), 20).expect("ok");
        let mass = bounds.field_mass_under(parent, crate::field::Q16_16::ONE);
        assert_eq!(mass, 8 * 65_536, "8 cells at intensity 1.0 (Q16.16)");
        // A level-21 cell inside: exactly one cell's mass.
        let leaf = CellId::from_cell_coords(glam::IVec3::new(1, 1, 1), 21).expect("ok");
        assert_eq!(bounds.field_mass_under(leaf, crate::field::Q16_16::ONE), 65_536);
        // A level-21 cell outside: zero.
        let outside = CellId::from_cell_coords(glam::IVec3::new(5, 5, 5), 21).expect("ok");
        assert_eq!(bounds.field_mass_under(outside, crate::field::Q16_16::ONE), 0);
        // The root covers the whole box.
        assert_eq!(
            bounds.field_mass_under(CellId::ROOT, crate::field::Q16_16::ONE),
            8 * 65_536
        );
    }
}
