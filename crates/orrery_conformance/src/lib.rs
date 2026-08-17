//! Orrery's cross-platform determinism conformance corpus (docs/06 §8, P4).
//!
//! `orrery_core` is the crate whose entire value proposition is that it
//! produces identical bytes on every platform. Until this crate existed that
//! property was verified on exactly one: the core's own determinism tests run
//! an identical tick twice *in-process* and compare state hashes, which catches
//! VC-4 and VC-8 violations — hash iteration order, address hashing — but says
//! nothing about whether Linux and Windows agree with each other. Each platform
//! was proving only that it agreed with itself.
//!
//! This crate closes that gap by producing a *comparable artifact*: a reference
//! ruleset, a fixed corpus, and a digest of per-tick state hashes that CI runs
//! on every target in the matrix and then compares. The comparison is the test;
//! running the corpus is only how the evidence is made.
//!
//! # Layout
//!
//! - [`ruleset`] — the reference kernel, exercising the discrete path (VC-5,
//!   bit-exact) and the continuous one (VC-6, `libm`) that actually drifts.
//! - [`corpus`] — the cases, and the [`Report`](corpus::Report) a platform emits.
//! - [`compare`] — how two reports are diffed, and how a mismatch is localized.
//!
//! # Why a chain hash rather than a state comparison
//!
//! The digest folds *every* per-tick state hash, not just the final state. A
//! window can diverge at tick 40 and reconverge by tick 180 — quantization
//! snaps both back onto the same lattice point — and a final-state comparison
//! would call that a pass. The chain cannot: any tick that differed is in it
//! permanently.

// The last of the fourteen first-party crates to adopt it. CI runs
// `clippy --workspace --all-targets -- -D warnings`, so this makes an
// undocumented public item a build failure rather than a warning nobody reads.
#![warn(missing_docs)]

pub mod compare;
pub mod corpus;
pub mod ruleset;

pub use compare::{compare, Divergence};
pub use corpus::{run_all, run_case, CaseDigest, Report, CASES, SCHEMA};
pub use ruleset::{Body, Command, Outcome, Reference, REFERENCE_RULESET};
