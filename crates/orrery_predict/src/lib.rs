//! Orrery prediction and rollback (D8, docs/05-prediction-rollback.md).
//!
//! The lightyear 0.29 configuration layer for per-entity authority: fixed
//! 60 Hz tick, 20 Hz send, 9-tick rollback window, 100 ms interpolation
//! buffer, 200 ms hit-rewind cap. This crate is **the only one whose internals
//! name lightyear types** (docs/10-crates.md layering rule 3) — the plan-B
//! seam. If lightyear's abstractions ever have to be replaced, this crate's
//! internals are rewritten and nothing else is.
//!
//! What the crate contains, and why each piece is here rather than upstream:
//!
//! - [`config`] — the five D16 numbers, plus the coupling invariants
//!   docs/05 §12 states between them. lightyear will accept any of them; only
//!   some combinations are a working game.
//! - [`tick`] — the bridge between lightyear's internal tick and Orrery's
//!   universe-global `Tick` (D8, docs/05 §6). lightyear's tick is narrow and
//!   session-relative; Orrery's is a u64 anchored to a coordinator-issued
//!   epoch, and every signed log, RNG seed and journal record references the
//!   latter.
//! - [`budget`] — the resimulation budget guard (docs/05 §3). Rollback cost is
//!   unbounded by construction; the guard turns "we cannot afford this replay"
//!   into a plan that always fits.
//! - [`monitor`] — the reconciliation-error monitor (docs/05 §10). The
//!   residuals prediction already computes are D10's witness signal, kept
//!   instead of discarded.
//! - [`wiring`] — the seam itself: every lightyear type this workspace names
//!   appears in that module and nowhere else. Read its docs for the
//!   D16-to-lightyear knob map and for what lightyear 0.29 does not supply
//!   (per-entity authority, and any rollback signal at all).

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod budget;
pub mod config;
pub mod correction;
pub mod monitor;
pub mod plugin;
pub mod tick;
pub mod wiring;

pub use budget::{PredictPriority, ResimPlan, RollbackBudget};
pub use config::{ConfigDefect, PredictConfig, HIGH_RATE_SET};
pub use correction::{
    authority_correction_plan, reconcile_authority_corrections, AuthorityCorrectionInbox,
    AuthorityCorrectionPlan, AuthorityCorrectionReconciler, SharedAuthorityCorrectionReconciler,
};
pub use monitor::{
    DegradedReason, ErrorTrack, MonitorBands, MonitorSignal, ReconciliationMonitor, TrackKey,
    WitnessConfidence,
};
pub use plugin::OrreryPredictPlugin;
pub use tick::TickBridge;
pub use wiring::{AppReconciliationExt, PredictSystems, PredictedBy, ReconciliationResidual};
