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

pub mod executor;
pub mod invariants;
pub mod log;
pub mod migration;
pub mod quantize;
pub mod replay;
pub mod rng;
pub mod ruleset;
pub mod store;
pub mod tolerance;

pub use executor::{Executor, TickOutcome, TICK_HZ, TICK_NANOS};
pub use invariants::{evaluate, Invariant, InvariantKind, InvariantSample, InvariantViolation};
pub use migration::ComponentMigrator;
pub use quantize::{QPos, QVel, Quantized};
pub use replay::{verify_bundle, ReplayError, ReplayHarness, ReplayTrace};
pub use rng::{tick_rng, tick_seed, TickRng};
pub use ruleset::{
    state_hash, CodecError, ComponentTypeId, CoreClass, CoreCodec, EntityMaterialization,
    OrderedInputs, Ruleset, StateView, StepOutput,
};
pub use store::{AuthorityLog, BundleError, ClaimRecord, Retention};
pub use tolerance::{Tolerance, ToleranceOutcome, TrajectorySample};
