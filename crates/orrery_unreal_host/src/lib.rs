//! Spike #1043, the engine-independent half: the object an Unreal plugin
//! would wrap, built and driven from C on Linux.
//!
//! Two handles cross one C ABI, side by side:
//!
//! - the existing `orrery_host_*` handle from [`orrery_sim_host::abi`], here
//!   created by [`skirmish::orrery_skirmish_host_create`] over a real ruleset
//!   (`orrery_games::Skirmish`, not the synthetic one), and
//! - an `orrery_app_*` handle ([`app`]) around a real headless
//!   `bevy_app::App` carrying `MinimalPlugins`, `OrreryNetPlugin` and
//!   `OrreryPredictPlugin`, whose `App::update()` the foreign main loop calls
//!   once per fixed tick.
//!
//! This is the `App` prong of D53's fork (D53 §Options, "A fork H1 and H2
//! share"): a full `bevy_app::App` *beside* the ABI handle, not a non-`App`
//! driver behind it. It is evidence on that prong, not the decision, and it
//! settles neither G10.2 nor D52/D53 (both Proposed, #1022). The deciding
//! in-process number is the one taken inside a UE 5.8 process on Windows,
//! which this host cannot produce.
//!
//! Nothing here connects the prediction plugin to the host seam: D53 §5
//! records that the rollback driver does not exist, and this spike measures
//! what the `App` prong costs on the game thread rather than building that
//! driver. What it can answer from C is the coexistence question — what a
//! headless `App` spawns into a foreign process, what `App::update()` costs
//! per frame, whether panics stay behind the boundary, whether lightyear's
//! tick tracks a foreign accumulator — and the predicted-tick latency
//! measured the way #920 measures `ipc_added`.

#![deny(unsafe_op_in_unsafe_fn)]
#![warn(missing_docs)]

pub mod app;
pub mod skirmish;
