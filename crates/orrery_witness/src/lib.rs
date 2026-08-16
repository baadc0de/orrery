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
//!    mismatch, arms an audit over a window bounded at 180 ticks.
//! 4. **Stage 3 — report.** The window is assembled into a
//!    [`DiscrepancyReport`] whose evidence stands on its own.
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
//! # Not yet here
//!
//! Transport. Nothing in this crate sends or receives: frames and claims are
//! handed to [`Witness::ingest_frame`] and [`Witness::ingest_claim`], and gap
//! repair surfaces as a [`LogRangeRequest`] the caller is expected to send.
//! Streaming lives in `orrery_net` and the Bevy plugin adapter is a thin drain
//! over this engine; both land together, since one without the other has
//! nothing to carry.
//!
//! Attestation co-signing (docs/07 §4) is P5, not P4 — this phase is passive
//! by design: logs, replay, telemetry, no enforcement.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod report;
pub mod witness;

pub use report::{sign_report, verify_report, ReportError};
pub use witness::{
    Observation, Watch, Witness, WitnessConfig, WitnessCounters, WitnessError, WitnessSignal,
};
