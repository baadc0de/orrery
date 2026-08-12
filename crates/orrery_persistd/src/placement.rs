//! Rendezvous (highest-random-weight) placement of shard cells onto nodes
//! (docs/08-persistence.md §3.2).
//!
//! `owner(shard) = argmax_n weight_n · h(shard_id, node_id)`. Properties we need
//! and get: no central assignment table for the common case, minimal disruption
//! when nodes join/leave (only shards whose argmax changed move), and capacity
//! weighting.

use orrery_protocol::CellId;

/// The weight of a node in the rendezvous hash.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RendezvousWeight(u64);

impl RendezvousWeight {
    /// A node weight from a raw u64.
    #[must_use]
    pub const fn new(weight: u64) -> Self {
        Self(weight)
    }
}

impl Default for RendezvousWeight {
    fn default() -> Self {
        Self(1)
    }
}

/// A node participating in rendezvous placement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RendezvousNode {
    /// The node's raw identifier.
    pub id: u64,
    /// Its capacity weight (default 1).
    pub weight: RendezvousWeight,
}

impl RendezvousNode {
    /// A node with the default weight of 1.
    #[must_use]
    pub const fn new(id: u64) -> Self {
        Self {
            id,
            weight: RendezvousWeight(1),
        }
    }
}

/// Highest-random-weight (rendezvous) hashing over a node set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RendezvousHasher {
    nodes: Vec<RendezvousNode>,
}

impl RendezvousHasher {
    /// Construct a hasher over the given nodes.
    #[must_use]
    pub fn new(nodes: Vec<RendezvousNode>) -> Self {
        Self { nodes }
    }

    /// The owner of `cell`: the node maximizing `weight · h(cell, node)`.
    ///
    /// Returns `None` if there are no nodes.
    #[must_use]
    pub fn owner(&self, cell: CellId) -> Option<u64> {
        let mut best: Option<(u64, u128)> = None;
        for node in &self.nodes {
            let score = (node.weight.0 as u128) * (hash2(cell.to_bits(), node.id) as u128);
            let cur = best.map(|(_, s)| s).unwrap_or(0);
            if score > cur {
                best = Some((node.id, score));
            }
        }
        best.map(|(id, _)| id)
    }

    /// The node set.
    #[must_use]
    pub fn nodes(&self) -> &[RendezvousNode] {
        &self.nodes
    }
}

/// A 64-bit mixing hash of two 64-bit inputs (a poor-man's splitmix64 over the
/// pair). Used only for placement scoring; not cryptographic.
fn hash2(a: u64, b: u64) -> u64 {
    let mut z = a.wrapping_mul(0x9e37_79b9_7f4a_7c15).wrapping_add(b);
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cell(n: u64) -> CellId {
        // Reuse the coords-based constructor (dev-deps include glam via the
        // workspace); values are only used as distinct hash inputs.
        CellId::from_cell_coords(glam::IVec3::new(n as i32 % 1000, n as i32 % 1000, 0), 21).unwrap()
    }

    #[test]
    fn owner_is_deterministic() {
        let h = RendezvousHasher::new(vec![RendezvousNode::new(1), RendezvousNode::new(2)]);
        for i in 0..100u64 {
            let c = cell(i);
            assert_eq!(h.owner(c), h.owner(c));
        }
    }

    #[test]
    fn weight_biases_ownership() {
        // A heavily-weighted node should own far more cells.
        let heavy = RendezvousNode {
            id: 1,
            weight: RendezvousWeight::new(100),
        };
        let light = RendezvousNode {
            id: 2,
            weight: RendezvousWeight::new(1),
        };
        let h = RendezvousHasher::new(vec![heavy, light]);
        let mut heavy_owns = 0usize;
        for i in 0..1000u64 {
            if h.owner(cell(i)) == Some(1) {
                heavy_owns += 1;
            }
        }
        assert!(heavy_owns > 900, "heavy node owns {heavy_owns}/1000");
    }

    #[test]
    fn empty_set_yields_none() {
        let h = RendezvousHasher::new(vec![]);
        assert_eq!(h.owner(CellId::ROOT), None);
    }
}
