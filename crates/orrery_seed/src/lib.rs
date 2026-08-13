//! Orrery offline world seeder (docs/12-world-seeding.md).
//!
//! A **TOML-configured scenario runner**: it reads a scenario file describing
//! density *fields* over `CellId` space, realizes them into entities with an
//! exact declared count, and — in this first slice — reports the analytic dry
//! run (`plan`) without any cluster. The write path (`apply`/`verify`/`wipe`)
//! sits behind the `fdb` feature and is out of scope for v1 of this crate's
//! generation side.
//!
//! The load-bearing properties, all from docs/12 §8 (the determinism
//! contract):
//!
//! - **No global sequential RNG.** Every draw is addressed by
//!   `(layer, cell, index)` down the domain-separated seed tree
//!   ([`seedtree`]), so generation is order-independent, parallel, resumable,
//!   and repairable per cell.
//! - **Quantization before anything decides.** Generator fields are computed
//!   in `f64` but rounded to [`field::Q16_16`] before any comparison,
//!   threshold, accumulation or split (§8.3: "the quantization boundary is
//!   the contract"). Every count-determining path is then `u128` integer.
//! - **`BTreeMap` accumulators, never `HashMap` iteration** (§8.4), so the
//!   same binary run at different thread counts agrees bit-for-bit.
//!
//! v1 scope (docs/11-roadmap.md §P2): the `uniform` generator, the `union`
//! fold with the implicit `"main"` accumulator, hash placement, the opaque
//! payload encoder, and the analytic `plan` verb. Every other generator,
//! fold op, the `where` predicate grammar, `spread`, the sampled/probe dry-run
//! tiers, the solver and `scale_mode` are out of scope and are rejected with
//! typed "unsupported in v1" errors rather than stubbed.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

/// Start the FoundationDB client network thread.
///
/// Delegates to [`orrery_persistd::checkpoint::fdb::fdb_network`], which owns
/// the **one** boot this process is allowed: `foundationdb::boot()` selects the
/// client API version and the C client permits that exactly once per process,
/// so a second `Once` in this crate would panic the moment a binary or test
/// links both crates — and then poison the guard for every later caller.
///
/// Every entry point here that opens a `Database` calls this first; without it
/// the C client rejects the connection with `network_not_setup`
/// (`error_code: 2008`).
#[cfg(feature = "fdb")]
pub fn fdb_network() {
    let _ = orrery_persistd::checkpoint::fdb::fdb_network();
}

pub mod apply;
pub mod content;
pub mod encode;
pub mod field;
pub mod idmap;
pub mod manifest;
pub mod place;
pub mod plan;
pub mod scenario;
pub mod seedtree;
pub mod split;
pub mod validate;
pub mod verify;
pub mod wipe;
pub mod write;
