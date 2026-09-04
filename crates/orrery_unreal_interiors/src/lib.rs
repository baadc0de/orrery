//! Spike #1045 — moving interiors: the Rust side.
//!
//! **Research spike, not shipped code.** A throwaway ruleset in which a
//! *frame* (a mothership fixture, a ship, a mech) carries its velocity and
//! rotation at its root and its contents step in the frame's own integer
//! millimetre lattice — `docs/01-spatial-model.md` §13.1 ("the carrier is a
//! nested grid, and its velocity lives at the grid root, never in its
//! contents") — with two frame migrations, the teleport-class and the
//! continuous-class crossings of §13.3, both written as one exact transform.
//!
//! None of the ruleset-side machinery this models exists in the tree:
//! `FrameChange` is deferred (`crates/orrery_core/src/lib.rs:68-73`), and no
//! `Ruleset` in `orrery_games` carries a `GridId`. So this crate stubs it, as
//! #1045 says it must, and it **cannot claim replay closure** — there is no
//! `FrameChange` record binding the log to the new basis. What it can do is
//! be hosted on `orrery_sim_host` unchanged, so a frame change is stepped,
//! hashed, snapshotted and restored by the machinery D47 names as the
//! rollback unit, and the question "does a correction spanning the frame
//! change stay hash-exact" has a number.
//!
//! - [`rules`] — the ruleset: [`rules::Body`] state, [`rules::Intent`]
//!   inputs, [`rules::Happening`] events, the frame transform.
//! - [`host`] — the one C factory (`orrery_interiors_host_create`) and the
//!   scene-population helper, beside the generic `orrery_host_*` ABI.
//!
//! The C consumer (`examples/c/interiors_consumer.c`) and the Unreal plugin
//! (`docs/spikes/1045-moving-interiors/unreal`) both include
//! `examples/c/interiors_shared.h`: the byte codec, the scripted scenes and
//! spike #1052's rollback driver, so the Unreal run and the C run drive the
//! same rules with the same inputs through the same entry points.

#![warn(missing_docs)]
// The codec reinterprets bytes (an id is a u64 in a field an i64 reader
// produced); the frame transform reads lattice tuples into vectors; and the
// transform's multiply-then-add stays unfused on purpose — a fused
// multiply-add rounds differently per platform, which is the drift VC-6's
// bands exist to absorb, not to invite. All deliberate and local to this
// throwaway crate.
#![allow(
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap,
    clippy::cast_possible_truncation,
    clippy::tuple_array_conversions,
    clippy::suboptimal_flops
)]

pub mod host;
pub mod rules;
