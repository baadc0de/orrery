//! Passive witnessing (D10, docs/07-witnessing.md).
//!
//! A witness watches an authority it does not trust and, without any extra
//! traffic, decides whether that authority is executing the rules it claims to.
//! It never decides guilt: it produces a *self-verifying* report that anyone
//! holding the same rules build can check for themselves, and the cluster
//! re-runs the evidence rather than believing the reporter.
//!
//! # The pipeline this implements
//!
//! Stages 1–3 of docs/07 §3, client side:
//!
//! 1. **Stage 1a — stateless invariants.** The game's `Ruleset::invariants()`
//!    run on every received sample. Cheap, no history beyond the previous
//!    sample, and the only validation most bulk-class state ever gets.
//! 2. **Stage 1c — continuous log re-execution.** Witness-set members
//!    re-execute the streamed signed input log for their watched entities, tick
//!    by tick, and compare against the authority's 2 Hz `StateClaim` hashes.
//!    This is *the* signal for entities nobody is interacting with, which
//!    prediction error cannot provide.
//! 3. **Stage 2 — escalation.** A hard invariant breach, or a re-execution
//!    mismatch, arms an audit over a window bounded by
//!    [`WitnessConfig::window_ticks`]. [`Witness::audit_window`] computes that
//!    window: it opens at the last claim the witness and the subject
//!    demonstrably agreed on and closes at the disputed claim.
//! 4. **Stage 3 — report.** The window is assembled into a
//!    [`DiscrepancyReport`] whose evidence stands on its own, by
//!    [`Witness::raise`].
//!
//! **Stages 2 and 3 are engine-driven, and filing is opt-in.** The engine
//! decides nothing about escalation on its own; the `bevy` adapter arms the
//! window and calls `raise` when a [`WitnessSignal::ClaimMismatch`] comes back,
//! but only if the host inserted a [`WitnessIdentity`] to sign with — and even
//! then, shadow mode files nothing. A host that inserts no identity gets
//! stages 1a and 1c and the counters, which is what the `p1-swarm` harness
//! does, deliberately. A host with no Bevy at all drives `audit_window` and
//! `raise` itself, as `orrery_persistd`'s tests do.
//!
//! # Shadow mode is the default, and that is the point
//!
//! [`WitnessConfig::shadow_mode`] starts `true`. In shadow mode the witness
//! does every check and files nothing — it only counts. D17 risk 3 names
//! false-positive strikes on honest players as the failure that kills
//! witness-based trust, and P4 exists to measure the real cross-platform drift
//! distribution *before* anything can strike. A witness that shipped
//! enforcement-on by default would be asserting a false-positive rate nobody
//! has measured yet.
//!
//! # The engine and the adapter
//!
//! [`witness`] and [`report`] are Bevy-free and decide everything. The `bevy`
//! feature adds [`plugin`], a thin drain that moves bytes between
//! `orrery_net`'s peer lane and the engine and turns its return values into ECS
//! messages — no detection logic of its own. `orrery_persistd` takes this crate
//! with `default-features = false` and gets only the engine, which is what lets
//! the cluster and a headless bot harness run the same witness a game client
//! does. The P4 exit criterion is measured over bot *and* human play, so that
//! has to be one implementation.
//!
//! # Not yet here
//!
//! Attestation co-signing (docs/07 §4) is P5, not P4 — this phase is passive
//! by design: logs, replay, telemetry, no enforcement.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

#[cfg(feature = "bevy")]
pub mod plugin;
pub mod report;
pub mod witness;

#[cfg(feature = "bevy")]
pub use plugin::{
    AuthoredLog, PendingRepairs, PublishClaim, PublishFrame, RepairBudget, ReportFiled,
    WitnessClock, WitnessIdentity, WitnessLinkCounters, WitnessPlugin, WitnessSet, WitnessState,
    Witnessed,
};
pub use report::{sign_report, verify_report, ReportError};
pub use witness::{
    Catchup, Observation, Watch, Witness, WitnessConfig, WitnessCounters, WitnessError,
    WitnessSignal,
};
