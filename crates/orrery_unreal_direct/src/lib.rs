//! Spike #1052, the non-`App` prong of D53's fork: the object an Unreal
//! plugin would wrap if the process carried **no Bevy `App`**.
//!
//! One handle crosses the C ABI — the existing `orrery_host_*` handle from
//! [`orrery_sim_host::abi`], created by [`skirmish::orrery_skirmish_host_create`]
//! over a real ruleset (`orrery_games::Skirmish`). There is no second handle:
//! no `bevy_app::App`, no schedule runner, no task pool, no lightyear, no
//! iroh endpoint, no tokio runtime. The Rust side of this crate is therefore
//! exactly the seam that already exists (`step`/`snapshot`/`restore`, D53 §5),
//! plus the one factory a game adds and two helpers so the C driver never
//! reimplements the game's spawn table or its honest pilot.
//!
//! Everything the `App` prong got from Bevy — the fixed-step clock, the
//! prediction ring, correction intake, the rollback and replay, the residual
//! for the reconciliation monitor — is on the *other* side of the ABI, in
//! `examples/c/direct_consumer.c`, written the way a `UGameInstanceSubsystem`
//! would write it. That C is the cost of this prong, and it is measured rather
//! than described.
//!
//! This is evidence for a decision the owner has not taken: it settles
//! neither G10.2 nor D52/D53 (both Proposed, #1022). The deciding in-process
//! number is the one taken inside a UE 5.8 process on Windows, which this host
//! cannot produce.

#![deny(unsafe_op_in_unsafe_fn)]
#![warn(missing_docs)]

pub mod skirmish;
