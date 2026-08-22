//! The component bag's at-rest framing: per-component schema versions
//! (docs/08-persistence.md §16, D38 clause (d)).
//!
//! §16 is explicit that versioning is per *component*, not per snapshot: a
//! slot carries its own schema version beside its payload, so unrelated
//! components migrate independently and a single-component change does not
//! rewrite the whole bag. D38 clause (f) prices the alternative — one version
//! per bag saves ~15 B an entity and couples every component's migration to
//! every other's — and rejects it.
//!
//! # What this module does and does not decide
//!
//! It decides the **framing**: how slots are laid out and where the version
//! sits inside one. It decides nothing about the payloads, which stay
//! `Ruleset`-opaque exactly as they are today (`EntityRecord::components`,
//! docs/12 §4.1's encoder seam). A game that adopts this framing gets a bag
//! persistd can take a floor from; a game that does not keeps writing an
//! unframed bag, which is [`SCHEMA_V0`] under the bootstrap rule of
//! [`orrery_protocol::atrest`] and stays readable.
//!
//! **No migration runs here.** W1 writes versions; applying them is W2 (#281),
//! and this module deliberately holds no registry, no `from_version` dispatch
//! and no lazy-apply path.
//!
//! # The floor
//!
//! [`ComponentBag::schema_floor`] is the number persistd stamps into the
//! `world/` value envelope ([`crate::keyspace::LIVE_VERSIONED_TAG`]): the
//! minimum schema version over the bag's slots, i.e. how far behind the
//! furthest-behind component is. It is a *summary* of the bag, derived from
//! it, never an independent counter — so the envelope and the bag cannot drift
//! apart undetectably.
//!
//! An empty bag has no slot to be behind, so its floor is [`SCHEMA_V0`]: an
//! entity with no components is not a row a sweep needs to touch, and the
//! alternative (a floor of `u32::MAX`) would read as "ahead of everything",
//! which is worse than reading as "the oldest era" for a row with nothing in
//! it.

use bytes::Bytes;
use orrery_core::ComponentTypeId;
use orrery_protocol::atrest::{SchemaVersion, SCHEMA_V0};
use serde::{Deserialize, Serialize};

/// One component slot inside a bag: which component, what schema version its
/// bytes are written under, and the bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComponentSlot {
    /// The game-assigned component type this slot holds.
    pub component: ComponentTypeId,
    /// The schema version `payload` is written under (D38 clause (d)(3)):
    /// allocated by the game per component type, monotone, never reused or
    /// gapped, and orthogonal to `RulesetId::version`.
    pub schema_version: SchemaVersion,
    /// The component's `Ruleset`-opaque bytes. The cluster never interprets
    /// them; only the game's own code does.
    pub payload: Bytes,
}

/// A framed component bag: the `world/` value's payload, slot by slot.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ComponentBag {
    /// The slots, in the order the writer emitted them.
    pub slots: Vec<ComponentSlot>,
}

/// The postcard shape of one slot.
///
/// `ComponentTypeId` is an `orrery_core` type without serde derives, and this
/// crate is not the place to add them to somebody else's trait surface (D38
/// clause (c): W1 touches no `Ruleset` surface). So the wire form carries the
/// raw u32 and the public type keeps the id.
#[derive(Serialize, Deserialize)]
struct WireSlot {
    component: u32,
    schema_version: SchemaVersion,
    payload: Bytes,
}

impl ComponentBag {
    /// The bag-level schema floor: the minimum slot version, or [`SCHEMA_V0`]
    /// for an empty bag.
    #[must_use]
    pub fn schema_floor(&self) -> SchemaVersion {
        self.slots
            .iter()
            .map(|slot| slot.schema_version)
            .min()
            .unwrap_or(SCHEMA_V0)
    }

    /// Encode the bag to the bytes a `world/` value carries.
    ///
    /// # Errors
    ///
    /// Returns the postcard error if the bag does not serialize.
    pub fn encode(&self) -> Result<Bytes, postcard::Error> {
        let wire: Vec<WireSlot> = self
            .slots
            .iter()
            .map(|slot| WireSlot {
                component: slot.component.0,
                schema_version: slot.schema_version,
                payload: slot.payload.clone(),
            })
            .collect();
        postcard::to_stdvec(&wire).map(Bytes::from)
    }

    /// Decode a framed bag.
    ///
    /// **Only call this on bytes a framed writer produced.** There is no
    /// probe-and-guess path: an unframed bag is not a malformed framed one,
    /// it is a v0 bag, and the thing that tells the two apart is the `world/`
    /// value tag, not the bag's own bytes.
    ///
    /// # Errors
    ///
    /// Returns the postcard error if the bytes are not a framed bag.
    pub fn decode(bytes: &[u8]) -> Result<Self, postcard::Error> {
        let wire: Vec<WireSlot> = postcard::from_bytes(bytes)?;
        Ok(Self {
            slots: wire
                .into_iter()
                .map(|slot| ComponentSlot {
                    component: ComponentTypeId(slot.component),
                    schema_version: slot.schema_version,
                    payload: slot.payload,
                })
                .collect(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn slot(component: u32, schema_version: SchemaVersion) -> ComponentSlot {
        ComponentSlot {
            component: ComponentTypeId(component),
            schema_version,
            payload: Bytes::from_static(b"opaque"),
        }
    }

    #[test]
    fn a_bag_round_trips_with_every_slot_version() {
        let bag = ComponentBag {
            slots: vec![slot(1, 4), slot(2, 9), slot(3, 4)],
        };
        let bytes = bag.encode().expect("encodes");
        assert_eq!(ComponentBag::decode(&bytes).expect("decodes"), bag);
    }

    #[test]
    fn the_floor_is_the_furthest_behind_slot() {
        let bag = ComponentBag {
            slots: vec![slot(1, 7), slot(2, 3), slot(3, 11)],
        };
        assert_eq!(
            bag.schema_floor(),
            3,
            "the floor answers how far behind the worst slot is, not the best"
        );
    }

    #[test]
    fn an_empty_bag_floors_at_v0() {
        assert_eq!(ComponentBag::default().schema_floor(), SCHEMA_V0);
    }

    #[test]
    fn per_component_versions_are_independent() {
        // §16's reason for per-component rather than per-bag versioning: one
        // component's bump moves that slot and nothing else.
        let before = ComponentBag {
            slots: vec![slot(1, 2), slot(2, 2)],
        };
        let mut after = before.clone();
        after.slots[0].schema_version = 3;
        assert_eq!(after.slots[1], before.slots[1]);
        assert_eq!(
            after.schema_floor(),
            2,
            "the bag is still floored by the slot that did not move"
        );
    }

    #[test]
    fn the_slot_version_costs_one_byte_below_128() {
        // D38 clause (f)'s arithmetic: a postcard varint is one byte for
        // values under 128, which is where the ~160 MB at 10^7 rows comes
        // from. A slot version that cost more would break that estimate.
        let one = ComponentBag {
            slots: vec![slot(1, 1)],
        };
        let big = ComponentBag {
            slots: vec![slot(1, 127)],
        };
        assert_eq!(
            one.encode().expect("encodes").len(),
            big.encode().expect("encodes").len()
        );
    }
}
