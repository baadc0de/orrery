//! Committed chain digests, per game and scenario.
//!
//! A golden is the same instrument `orrery_conformance` uses, pointed at a
//! game instead of at a corpus kernel: a blake3 chain over every per-tick
//! state hash of a fixed scenario. Two things make it worth committing.
//!
//! **It runs on four platforms.** `orrery_games` is in the determinism
//! matrix's headless spine (`.github/workflows/ci.yml`), so every leg checks
//! its own chain against these bytes. Cross-platform agreement is therefore
//! asserted per commit rather than inferred — a `libm` divergence that
//! survived quantization on one target fails that target's leg by name.
//!
//! **It notices a rules change nobody meant to make.** Any edit to a step, a
//! limit, a spawn or the pilot moves these values. That is the intended
//! friction: regenerate them *and* bump
//! [`SKIRMISH_RULESET`](crate::skirmish::SKIRMISH_RULESET)`.version`, because
//! a golden regenerated without a version bump hides a rules change as a
//! determinism pass — the one failure this whole apparatus exists to prevent.
//!
//! # What the chain does not cover
//!
//! State hashes, and only those. A rules change that alters *events* without
//! altering any state field moves nothing here — adding attribution to
//! `Outcome::DamageDealt` did not shift a single chain. That is not a defect
//! in the golden so much as the same fact `Craft::damage_dealt` exists for: an
//! outcome that leaves no trace in the emitter's own state is invisible to
//! everything downstream that works from state hashes, adjudication included.
//! A game that wants an outcome checked has to write it down.
//!
//! That is also why an event carries an [`Archetype`] rather than a reach in
//! millimetres: nothing regenerates events and compares them against logged
//! records, so a scalar on the wire is uncontested, while an archetype is
//! hashed into the emitter's own state and adjudicated with it.
//!
//! [`Archetype`]: crate::skirmish::archetype::Archetype
//!
//! Regenerate with:
//!
//! ```sh
//! cargo test -p orrery_games --test battery -- --ignored --nocapture emit_goldens
//! cargo fmt -p orrery_games
//! ```

/// Skirmish, by scenario name.
pub const SKIRMISH: [(&str, [u8; 32]); 4] = [
    (
        "solo",
        [
            0xe2, 0xbc, 0x93, 0x46, 0x4e, 0xd0, 0x63, 0x26, 0x41, 0x6e, 0x00, 0xf2, 0xbb, 0x32,
            0x60, 0xdb, 0x9f, 0x92, 0xd9, 0x4b, 0x85, 0x34, 0x27, 0x1e, 0x40, 0x9e, 0x66, 0x8d,
            0xf7, 0x47, 0xc6, 0x15,
        ],
    ),
    (
        "duel",
        [
            0xe0, 0xcc, 0x13, 0xa1, 0xfa, 0xcf, 0x3c, 0xf5, 0xff, 0x90, 0x4d, 0xa5, 0xfe, 0x21,
            0xe1, 0x04, 0x58, 0xdd, 0xa0, 0x0a, 0x78, 0x0b, 0x00, 0xb4, 0x07, 0x40, 0x6a, 0x82,
            0x76, 0x7f, 0x62, 0x91,
        ],
    ),
    (
        "island",
        [
            0xdc, 0xc0, 0x70, 0x8a, 0xdf, 0xe5, 0x66, 0xc7, 0xda, 0x02, 0x03, 0x5f, 0xf8, 0xbb,
            0xb1, 0x94, 0x37, 0x68, 0xbe, 0xa8, 0x7b, 0x0f, 0x6b, 0xef, 0x17, 0xff, 0x1f, 0x7b,
            0x65, 0x65, 0xcf, 0x31,
        ],
    ),
    (
        "island-lossy",
        [
            0x8e, 0x19, 0x5e, 0x95, 0xcc, 0xb2, 0xb6, 0x58, 0xf8, 0xf6, 0x30, 0x55, 0xc7, 0xb9,
            0x4b, 0xb4, 0xae, 0xab, 0x7c, 0x47, 0xb5, 0x6e, 0x76, 0x2a, 0x71, 0x15, 0x06, 0xcd,
            0xe0, 0x14, 0xd4, 0xbf,
        ],
    ),
];
