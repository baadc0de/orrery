//! Orrery prediction and rollback (P1 initial config, docs/11-roadmap.md §P1).
//!
//! The lightyear 0.29 configuration layer for per-entity authority (D8): fixed
//! 60 Hz tick, 20 Hz send, 9-tick rollback window, 100 ms interpolation buffer,
//! 200 ms hit-rewind cap. This crate is **the only one whose internals name
//! lightyear types** — the plan-B seam (docs/10-crates.md §7).
//!
//! P1 scope: the `PredictConfig` (D16 defaults), the `OrreryPredictPlugin`
//! skeleton, and the two guard resources — the [`ReconciliationMonitor`] (the
//! witness signal, D10) and the [`RollbackBudget`] (resim amortized over ≤ 2
//! render frames). The actual lightyear wiring lands with the full P1
//! integration.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod config;
pub mod monitor;
pub mod plugin;

pub use config::PredictConfig;
pub use monitor::{ReconciliationMonitor, ViolationWindow};
pub use plugin::{OrreryPredictPlugin, RollbackBudget};
