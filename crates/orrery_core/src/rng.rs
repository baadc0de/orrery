//! Seeded randomness (VC-3).
//!
//! Every draw a core rule makes comes from here, and every seed is a pure
//! function of `(universe_seed, entity, tick)`. That is what lets a witness on
//! another continent, running another OS, reproduce an authority's loot roll
//! exactly — and it is why `step` is handed an RNG rather than being allowed
//! to construct one.
//!
//! Ticks are **absolute universe ticks** (D8), never island-relative, so the
//! derivation survives island merges without re-basing anything.

use rand_chacha::rand_core::SeedableRng;
use rand_chacha::ChaCha8Rng;

use orrery_protocol::{PersistId, Tick, UniverseSeed};

/// The RNG handed to one entity's `step` for one tick.
///
/// A fresh instance per entity per tick, so draw order inside `step` is code
/// order and nothing leaks between entities or across ticks. There is
/// deliberately no way to reseed it mid-tick.
pub type TickRng = ChaCha8Rng;

/// Derive the per-entity, per-tick RNG (VC-3).
///
/// `blake3::keyed_hash(universe_seed, persist_id ‖ tick)`, both operands
/// little-endian, feeding ChaCha8. Keyed hashing rather than plain hashing of
/// a concatenation: the universe seed is a key, not a prefix, so no amount of
/// chosen entity/tick input lets anyone probe it.
#[must_use]
pub fn tick_rng(seed: UniverseSeed, entity: PersistId, tick: Tick) -> TickRng {
    ChaCha8Rng::from_seed(tick_seed(seed, entity, tick))
}

/// The 32-byte seed [`tick_rng`] expands. Exposed for golden-vector tests,
/// which pin the derivation itself rather than only its consequences.
#[must_use]
pub fn tick_seed(seed: UniverseSeed, entity: PersistId, tick: Tick) -> [u8; 32] {
    let mut preimage = [0u8; 16];
    preimage[..8].copy_from_slice(&entity.0.to_le_bytes());
    preimage[8..].copy_from_slice(&tick.0.to_le_bytes());
    *blake3::keyed_hash(&seed.0, &preimage).as_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand_chacha::rand_core::RngCore;

    fn draws(seed: UniverseSeed, entity: PersistId, tick: Tick) -> [u64; 4] {
        let mut rng = tick_rng(seed, entity, tick);
        [
            rng.next_u64(),
            rng.next_u64(),
            rng.next_u64(),
            rng.next_u64(),
        ]
    }

    #[test]
    fn the_same_triple_always_produces_the_same_stream() {
        // The whole adjudication story rests on this: a witness replaying a
        // window must draw exactly what the authority drew.
        let seed = UniverseSeed([1; 32]);
        assert_eq!(
            draws(seed, PersistId::new(7), Tick::new(1_000)),
            draws(seed, PersistId::new(7), Tick::new(1_000))
        );
    }

    #[test]
    fn every_component_of_the_triple_changes_the_stream() {
        // If any of the three were ignored, entities would share loot rolls,
        // or a roll would repeat every tick — both silently exploitable.
        let base = draws(UniverseSeed([1; 32]), PersistId::new(7), Tick::new(1_000));
        assert_ne!(
            base,
            draws(UniverseSeed([2; 32]), PersistId::new(7), Tick::new(1_000))
        );
        assert_ne!(
            base,
            draws(UniverseSeed([1; 32]), PersistId::new(8), Tick::new(1_000))
        );
        assert_ne!(
            base,
            draws(UniverseSeed([1; 32]), PersistId::new(7), Tick::new(1_001))
        );
    }

    #[test]
    fn entity_and_tick_do_not_alias_each_other() {
        // A naive derivation that summed or concatenated without fixed widths
        // would let (entity 1, tick 2) collide with (entity 2, tick 1).
        let seed = UniverseSeed([1; 32]);
        assert_ne!(
            draws(seed, PersistId::new(1), Tick::new(2)),
            draws(seed, PersistId::new(2), Tick::new(1))
        );
    }

    #[test]
    fn the_seed_derivation_is_pinned() {
        // A golden vector: changing the derivation changes every historical
        // replay verdict, so it must never drift silently. If this fails, the
        // change was either deliberate — and needs a rules-version bump — or a
        // bug.
        let seed = tick_seed(UniverseSeed([0; 32]), PersistId::new(0), Tick::new(0));
        assert_eq!(
            seed,
            [
                0xbc, 0xfd, 0xa2, 0xfe, 0xee, 0x26, 0x26, 0xa3, 0x1f, 0xe2, 0xce, 0x58, 0x33, 0xbc,
                0xe9, 0x6a, 0x47, 0xe4, 0xab, 0x46, 0x68, 0xe2, 0x7b, 0x2a, 0xc6, 0x86, 0x98, 0x0b,
                0x67, 0x48, 0xf5, 0x1c,
            ],
            "the VC-3 seed derivation changed; this invalidates every stored replay"
        );
    }
}
