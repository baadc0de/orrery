//! Orrery's verifiable core (D9, docs/06-verifiable-core.md).
//!
//! Orrery does not need determinism to keep peers in sync — live sync is state
//! replication, and a misprediction is corrected by the next authoritative
//! snapshot. It needs determinism to prove, *after the fact*, that an authority
//! executed the rules it claimed to execute. That inverts the usual failure
//! economics: a determinism bug here degrades witnessing and adjudication
//! quality, but it cannot desync a session.
//!
//! Determinism is therefore **scoped** on two axes: only rules whose outcomes
//! touch persistent value, and only re-executed out of band — witness
//! re-execution, replay adjudication, parked-entity catch-up. Never as the live
//! sync mechanism.
//!
//! # What this crate is
//!
//! - [`ruleset`] — the [`Ruleset`](ruleset::Ruleset) contract a game implements
//!   once and links identically into peers, field hosts and `persistd`.
//! - [`sched`] — the optional declared per-entity system schedule a ruleset may
//!   write its tick as, instead of one long `step` body (D43 clause (b)/(g)).
//! - [`executor`] — the fixed 60 Hz tick (VC-1), which owns the guarantees a
//!   ruleset must not have to remember: seeded RNG, post-step quantization,
//!   neighbour-read recording.
//! - [`rng`] — randomness derived from `(universe_seed, entity, tick)` (VC-3).
//! - [`quantize`] — the tick-boundary lattice (VC-7).
//! - [`tolerance`] — the band comparator that separates platform drift from
//!   cheating (docs/06 §5).
//! - [`log`] — the tamper-evident hash chain and its frame/claim signatures
//!   (docs/06 §6).
//! - [`store`] — the authority's retained history, and the bundle assembly that
//!   makes a window servable (docs/06 §6, "Retention").
//! - [`invariants`] — the stage-1 checks every interested peer runs on received
//!   state, regardless of witness-set membership (docs/06 §3, D10 stage 1).
//! - [`replay`] — the headless harness and `verify_bundle` (docs/06 §7).
//!
//! # Engine-agnostic, and mechanically so
//!
//! No Bevy, no tokio, no OS services. The same build links into three very
//! different processes, so anything platform-specific in here would make them
//! disagree — which is the exact failure this crate exists to detect. The
//! dependency spine enforces it: `orrery_core` has no Bevy dependency to leak.
//!
//! # The determinism rules (docs/06 §4)
//!
//! Contractual for any code reachable from a `Ruleset` method. CI enforces what
//! it can; the rest is review discipline.
//!
//! | | Rule |
//! |---|---|
//! | VC-1 | Fixed tick. State advances only in `step`, at exactly 60 Hz. `dt` is a constant, never a measurement. |
//! | VC-2 | Total input order. The authority's log *is* the normative order; replay never re-sorts. |
//! | VC-3 | Seeded RNG from `(universe_seed, entity, absolute tick)`. No `thread_rng`, ever. |
//! | VC-4 | No unordered iteration. `BTreeMap`/`BTreeSet` or sorted `Vec`s — std hash iteration order is randomized per process. |
//! | VC-5 | Integer math for discrete outcomes, compared bit-exact. This is where the persistent value lives. |
//! | VC-6 | `libm` for continuous math, compared within tolerance bands. |
//! | VC-7 | Quantization at tick boundaries; the quantized value is what the next tick reads. |
//! | VC-8 | No ambient inputs — no clocks, no environment, no address hashing, no allocation-order dependence. |
//!
//! VC-4 and VC-8 are the two that fail silently, so the test strategy targets
//! them directly: running an identical tick twice in-process and comparing
//! state hashes catches hash-iteration order and address hashing immediately,
//! which is why those tests exist in every module here rather than only at the
//! edges.
//!
//! # Not yet here
//!
//! The `GeometryFrame`, `FieldFrame`, `FrameChange` and `TerrainPromotion`
//! record sources, and the `validate_intent` / `park_tick` / `catch_up` half of
//! the `Ruleset` sketch. Each closes replay over a
//! subsystem that does not exist yet, or serves a consumer that does not
//! (`orrery_witness`, the field host, the intent path). Each is additive.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod author;
pub mod executor;
pub mod invariants;
pub mod log;
pub mod migration;
pub mod quantize;
pub mod replay;
pub mod rng;
pub mod ruleset;
pub mod sched;
pub mod store;
pub mod tolerance;

pub use executor::{
    canonical_step, CanonicalOutcome, CanonicalStep, Executor, NeighborFrame, NeighborSnapshot,
    SealedTickInputs, SteppedEntity, TickBackend, TickOutcome, TICK_HZ, TICK_NANOS,
};
pub mod geometry;
pub use author::{AuthoredFrame, InputLogProducer};
pub use invariants::{evaluate, Invariant, InvariantKind, InvariantSample, InvariantViolation};
pub use migration::ComponentMigrator;
pub use quantize::{QPos, QVel, Quantized};
pub use replay::{verify_bundle, verify_bundle_on, ReplayError, ReplayHarness, ReplayTrace};
pub use rng::{tick_rng, tick_seed, TickRng};
pub use ruleset::{
    assert_section_is_exact, state_hash, CodecError, ComponentTypeId, CoreClass, CoreCodec,
    EntityMaterialization, OrderedInputs, Ruleset, Section, Sectioned, StateSection, StateView,
    StepOutput,
};
pub use sched::{
    run_schedule, run_system, run_system_as, Observation, Schedule, Scheduled, Stage, StageName,
    StepCtx, System, SystemName,
};
pub use store::{AuthorityLog, BundleError, ClaimRecord, Retention};
pub use tolerance::{Tolerance, ToleranceOutcome, TrajectorySample};

#[cfg(test)]
mod overflow_profile_pin {
    /// D43 (f)(2): the canonical crates' build pins `overflow-checks = false`
    /// uniformly across profiles.
    ///
    /// This asserts the *behaviour* rather than parsing the workspace
    /// manifest or reading a cfg flag, so it cannot be satisfied by a
    /// `[profile]` entry that some other configuration overrides -- it fails
    /// whenever this build would actually panic on overflow. It runs under
    /// the `test` profile, which inherits `dev`: the half Cargo defaults to
    /// `true`, and therefore the half that splits dev from release.
    ///
    /// The clause exists because *profile-dependence* is the hazard. The same
    /// stray operation must not panic in a test and wrap in a shipped client.
    /// The pin was absent from the root manifest entirely until this test
    /// arrived with it, which is the argument for the test existing: an
    /// Accepted clause with nothing asserting it drifts back out in silence.
    #[test]
    fn canonical_arithmetic_wraps_instead_of_panicking() {
        let at_max = std::hint::black_box(i32::MAX);
        let wrapped = std::panic::catch_unwind(|| std::hint::black_box(at_max + 1));

        assert_eq!(
            wrapped.ok(),
            Some(i32::MIN),
            "overflow-checks is ON in this profile: D43 (f)(1) bars resolving \
             overflow by aborting the tick, and (f)(2) requires one behaviour \
             across all profiles. Restore the `[profile.dev]` and \
             `[profile.release]` pin in the workspace root Cargo.toml."
        );
    }
}
