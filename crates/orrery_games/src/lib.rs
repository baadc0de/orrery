//! Orrery's reference games (docs/11-roadmap.md §P4).
//!
//! P4 is not a construction phase. Detection, evidence and transport are
//! landed; what the phase exists to produce is a *number* — the false-positive
//! rate — and shadow mode stays on until ≥ 500 honest player-hours under
//! injected impairment produce zero reports (D17 risk 3). A number needs
//! something to measure, and until this crate there was nothing to measure but
//! `orrery_conformance`'s corpus kernel, which is deliberately not a game: no
//! caps, no cooldowns, no reach, no way to be refused. A kernel that accepts
//! every input can be replayed but never disagreed with, and a false-positive
//! rate measured over it would be a statement about arithmetic rather than
//! about play.
//!
//! So this crate ships **games**: rulesets complete enough to be played,
//! measured, and cheated at.
//!
//! # What is here
//!
//! - [`game`] — the [`Game`] contract on top of
//!   [`Ruleset`](orrery_core::Ruleset): populate a world, produce honest
//!   inputs, route cross-entity events, expose the continuous state the
//!   tolerance comparator needs. Plus [`Tamper`], because P4's demo criterion
//!   is a modified client and a game that can only be played honestly cannot
//!   prove that half.
//! - [`skirmish`] — the first game: kinematic movement over `libm`, integer
//!   combat with cooldowns, reach and a death state.
//! - [`scenario`] — the harness that plays a game, records what an authority
//!   would have logged, runs the stage-1 checks the way a peer does, and
//!   re-executes the log the way a witness does.
//! - [`golden`] — committed chain digests, checked on all four determinism
//!   targets.
//!
//! # Why the catalogue is plural
//!
//! A false-positive rate is a property of the rules being played, not of the
//! witness alone: craft that coast in straight lines and craft that brawl at a
//! cell boundary stress the comparator and the caps very differently, and one
//! kernel cannot tell you which of those a number came from. The harness is
//! therefore generic over [`Game`] and the battery in `tests/` runs over
//! [`CATALOGUE`](game::CATALOGUE) rather than over a type — adding the second
//! game is two lines in [`game`], and everything measured about the first is
//! measured about it for free.
//!
//! # What this crate is not
//!
//! Not a Bevy plugin, not networked, and not a replacement for `p1-swarm`.
//! The swarm runs the real witness over an impaired link and answers whether
//! the *pipeline* holds up; this answers whether the *rules* are honest-safe
//! and the cheats are adjudicable, in milliseconds, on every commit. The two
//! measure different halves and the swarm is the one that accumulates hours.
//!
//! # A seam this crate needed, and got
//!
//! Writing Skirmish surfaced a gap in the `Ruleset` contract: a rule could not
//! attribute an event to itself. [`StateView`](orrery_core::StateView) handed
//! a step its own state and its neighbours' but never its own
//! [`PersistId`](orrery_protocol::PersistId), so damage could be resolved but
//! not signed, and a kill could not name its killer — leaving a log of
//! anonymous effects that a P5 kill-credit intent would have nothing to attach
//! to. [`StateView::entity`](orrery_core::StateView::entity) closes it. The
//! identity comes from the executor, which already knew it, rather than from
//! the state, so a rule cannot claim to be an entity it is not.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod game;
pub mod golden;
pub mod regolith;
pub mod scenario;
pub mod skirmish;

pub use game::{for_each_game, Game, GameMeta, GameVisitor, Tamper, CATALOGUE};
pub use regolith::Regolith;
pub use scenario::{
    adjudicate, adjudicate_isolated, play, Divergence, Flag, Play, Scenario, SCENARIOS,
};
pub use skirmish::Skirmish;
