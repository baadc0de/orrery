//! The domain-separated seed tree (docs/12-world-seeding.md §8, item 2; D-D).
//!
//! Every random draw the seeder makes is addressed by `(layer, cell, index)`
//! down a four-level blake3 keyed-hash tree rooted at the *scenario* seed —
//! the public root, deliberately distinct from the secret `universe_seed`
//! (D-D: content must not become an inversion oracle against the verifiable
//! core's RNG):
//!
//! ```text
//! K_root  = blake3::derive_key(seed.context, seed_material)   // "orrery.seeder.v1"
//! K_layer = blake3::keyed_hash(K_root,  b"L" ‖ layer_name)
//! K_cell  = blake3::keyed_hash(K_layer, b"C" ‖ cell.to_bits().to_be_bytes())
//! K_slot  = blake3::keyed_hash(K_cell,  b"E" ‖ index.to_le_bytes())
//! rng     = ChaCha8Rng::from_seed(K_slot)
//! ```
//!
//! The domain tags (`L`/`C`/`E`) stop a layer name's byte pattern from ever
//! colliding with a cell id's. Because no generator may consume a global
//! sequential RNG (§8 rule 1), inserting one entity cannot shift any draw
//! anywhere else in the world.

use orrery_protocol::CellId;
use rand::SeedableRng;
use rand_chacha::ChaCha8Rng;

/// The default derivation context (D-D, docs/12 §8): a domain tag for
/// `blake3::derive_key`, never a seed itself. `[seed] context` may override
/// it, but the shipped scenarios and every cross-implementation vector use
/// this value.
pub const DEFAULT_CONTEXT: &str = "orrery.seeder.v1";

/// The root of one scenario's seed tree.
///
/// Not `Copy`: the root key is the one value in the tree that is cheap to
/// keep but pointless to duplicate by accident.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeedRoot([u8; 32]);

impl SeedRoot {
    /// Derive the root key from arbitrary seed material under `context`
    /// (§8 item 2: `K_root = blake3::derive_key(seed.context, seed_material)`).
    #[must_use]
    pub fn derive(context: &str, seed_material: &[u8]) -> Self {
        Self(blake3::derive_key(context, seed_material))
    }

    /// The raw root key bytes. Exposed for the manifest/report fingerprint;
    /// derivation goes through the typed methods below.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// The layer subkey: `K_layer = keyed_hash(K_root, b"L" ‖ layer)`.
    ///
    /// Layer names are UTF-8 strings in the scenario file, hashed as bytes.
    #[must_use]
    pub fn layer_key(&self, layer_name: &str) -> [u8; 32] {
        let mut input = Vec::with_capacity(1 + layer_name.len());
        input.push(b'L');
        input.extend_from_slice(layer_name.as_bytes());
        *blake3::keyed_hash(&self.0, &input).as_bytes()
    }

    /// The cell subkey under `layer_key`:
    /// `K_cell = keyed_hash(K_layer, b"C" ‖ cell.to_bits().to_be_bytes())`.
    ///
    /// Big-endian bits: the same byte order the storage keyspace uses, so a
    /// hex dump lines up with a key dump.
    #[must_use]
    pub fn cell_key(layer_key: &[u8; 32], cell: CellId) -> [u8; 32] {
        let mut input = [0u8; 9];
        input[0] = b'C';
        input[1..].copy_from_slice(&cell.to_bits().to_be_bytes());
        *blake3::keyed_hash(layer_key, &input).as_bytes()
    }

    /// The entity slot subkey under `cell_key`:
    /// `K_slot = keyed_hash(K_cell, b"E" ‖ index.to_le_bytes())`.
    ///
    /// Little-endian index, per §8: the slot index is a local counter, not a
    /// key-ordered byte string.
    #[must_use]
    pub fn slot_key(cell_key: &[u8; 32], index: u64) -> [u8; 32] {
        let mut input = [0u8; 9];
        input[0] = b'E';
        input[1..].copy_from_slice(&index.to_le_bytes());
        *blake3::keyed_hash(cell_key, &input).as_bytes()
    }

    /// The slot key for the full path `(layer, cell, index)`.
    #[must_use]
    pub fn slot_key_for(&self, layer_name: &str, cell: CellId, index: u64) -> [u8; 32] {
        let layer_key = self.layer_key(layer_name);
        let cell_key = Self::cell_key(&layer_key, cell);
        Self::slot_key(&cell_key, index)
    }

    /// The slot RNG: `ChaCha8Rng::from_seed(K_slot)` (§8 item 2).
    #[must_use]
    pub fn slot_rng(slot_key: [u8; 32]) -> ChaCha8Rng {
        ChaCha8Rng::from_seed(slot_key)
    }

    /// Convenience: the RNG for `(layer, cell, index)` in one call.
    #[must_use]
    pub fn rng_for(&self, layer_name: &str, cell: CellId, index: u64) -> ChaCha8Rng {
        Self::slot_rng(self.slot_key_for(layer_name, cell, index))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use orrery_protocol::cell::INTEREST_LEVEL;
    use orrery_protocol::CellId;
    use rand::RngCore;

    /// The docs/12 §8 derivation spelled out longhand — the independent
    /// implementation the pinned vectors below are checked against. If this
    /// and `SeedRoot` ever disagree, the bug is in `SeedRoot`.
    fn longhand(context: &str, material: &[u8], layer: &str, cell: u64, index: u64) -> [u8; 32] {
        let k_root = blake3::derive_key(context, material);
        let k_layer = blake3::keyed_hash(&k_root, format!("L{layer}").as_bytes());
        let mut cell_input = vec![b'C'];
        cell_input.extend_from_slice(&cell.to_be_bytes());
        let k_cell = blake3::keyed_hash(k_layer.as_bytes(), &cell_input);
        let mut slot_input = vec![b'E'];
        slot_input.extend_from_slice(&index.to_le_bytes());
        *blake3::keyed_hash(k_cell.as_bytes(), &slot_input).as_bytes()
    }

    #[test]
    fn derivation_matches_fixed_vectors() {
        // Pinned vectors for a fixed input. Computed once against the
        // longhand derivation above (which is the spec transcription) and
        // hardcoded here so a blake3 upgrade or an accidental parameter swap
        // fails loudly instead of silently re-deriving.
        let root = SeedRoot::derive("orrery.seeder.v1", b"seedtree-vector-v1");
        assert_eq!(
            hex(root.as_bytes()),
            "c8b4acf63e496f0f1ddab0135a5688b0567fa97100c8f3213bebac0ec06bbe9b"
        );

        let layer = root.layer_key("flat");
        assert_eq!(
            hex(&layer),
            "2dd3b81601a2ead8c1594db13bb321d58f6a71d569aaa1fc5b6feef9f2f08d20"
        );

        // The docs/01 §3.3 worked-example cell: coords (2, −1, 8) at level 21.
        let cell = CellId::from_bits(0xA924_9249_2492_4D65).expect("nonzero");
        let cell_key = SeedRoot::cell_key(&layer, cell);
        assert_eq!(
            hex(&cell_key),
            "f55f98df54abb94e69661ff2dabea009db57872188723eaa19e0138896895901"
        );

        let slot = SeedRoot::slot_key(&cell_key, 42);
        assert_eq!(
            hex(&slot),
            "7b417fa7f33ab2ddc863850d3b7ab45219f4961d500efc9b9cb3125e7ad6f067"
        );

        // The one-call path agrees with the stepwise path.
        assert_eq!(slot, root.slot_key_for("flat", cell, 42));
        // And with the spec transcription.
        assert_eq!(
            slot,
            longhand(
                "orrery.seeder.v1",
                b"seedtree-vector-v1",
                "flat",
                0xA924_9249_2492_4D65,
                42
            )
        );
    }

    #[test]
    fn domain_tags_separate_namespaces() {
        // The domain tags exist so a layer named "\x01…" cannot collide with
        // a cell id's byte pattern (§8 item 2). Assert the tags are load
        // bearing: same inputs, tag stripped, must differ.
        let root = SeedRoot::derive(DEFAULT_CONTEXT, b"tag-check");
        let layer = root.layer_key("x");
        let cell = CellId::from_bits(0xA924_9249_2492_4D65).expect("nonzero");

        let tagged = SeedRoot::cell_key(&layer, cell);
        let untagged = *blake3::keyed_hash(&layer, &cell.to_bits().to_be_bytes()).as_bytes();
        assert_ne!(tagged, untagged, "the b\"C\" tag must be load-bearing");
    }

    #[test]
    fn no_draw_is_shared_between_addresses() {
        // §8 rule 1: every draw is addressed by (layer, cell, index); distinct
        // addresses must give distinct keys (collision would be a blake3
        // break, so this is really checking the addressing composes).
        let root = SeedRoot::derive(DEFAULT_CONTEXT, b"addressing");
        let cell_a = CellId::from_bits(0xA924_9249_2492_4D65).expect("nonzero");
        let cell_b = CellId::from_bits(0xA924_9249_2492_4D66).expect("nonzero");
        let mut keys = std::collections::BTreeSet::new();
        keys.insert(root.slot_key_for("flat", cell_a, 0));
        keys.insert(root.slot_key_for("flat", cell_a, 1));
        keys.insert(root.slot_key_for("flat", cell_b, 0));
        keys.insert(root.slot_key_for("other", cell_a, 0));
        assert_eq!(keys.len(), 4);
    }

    #[test]
    fn slot_rng_seeded_from_slot_key() {
        // ChaCha8Rng::from_seed(K_slot) — assert the RNG stream is a pure
        // function of the slot key by comparing against an explicit
        // construction (the same operation spelled out, not a tautology).
        let root = SeedRoot::derive(DEFAULT_CONTEXT, b"rng-check");
        let cell = CellId::from_bits(0xA924_9249_2492_4D65).expect("nonzero");
        let mut a = root.rng_for("flat", cell, 7);
        let key = root.slot_key_for("flat", cell, 7);
        let mut b = ChaCha8Rng::from_seed(key);
        for _ in 0..8 {
            assert_eq!(a.next_u64(), b.next_u64());
        }
        // INTEREST_LEVEL cells derive fine through the same path.
        let interest = CellId::from_bits(cell.to_bits()).expect("nonzero");
        assert_eq!(interest.level(), INTEREST_LEVEL);
    }

    /// Minimal hex so the vectors above read as strings instead of byte
    /// arrays; not the place for a hex crate.
    fn hex(bytes: &[u8; 32]) -> String {
        let mut s = String::with_capacity(64);
        for b in bytes {
            s.push_str(&format!("{b:02x}"));
        }
        s
    }
}
