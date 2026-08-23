//! The [`Game`] contract, and the catalogue of games this crate ships.
//!
//! [`Ruleset`] is what the *engine* needs from a game: step one entity, one
//! tick, deterministically. That is not enough to **measure** a game, and
//! measurement is what P4 is for. A harness that wants to run 500 honest
//! player-hours has to know three more things no `Ruleset` method exposes:
//!
//! 1. how to populate a world (`spawn`),
//! 2. what an honest player *does* (`honest_inputs`),
//! 3. where a cross-entity event lands (`deliver`) — the executor deliberately
//!    does not route events, because routing is a property of the game's rules
//!    rather than of the tick.
//!
//! [`Game`] adds exactly those, plus [`Game::trajectory`], which is what lets
//! `orrery_core`'s tolerance comparator judge a game it has never heard of.
//!
//! # Why the catalogue is plural
//!
//! One reference game measures one shape of play. The false-positive rate that
//! gates P4 is a property of the *rules being played*, not of the witness
//! alone: a game whose craft coast in straight lines and a game whose craft
//! brawl at cell boundaries stress the comparator and the stage-1 caps very
//! differently, and a single kernel cannot tell you which of those the number
//! came from. So games are a set, the harness is generic over it, and adding
//! the second one is two lines in this file — [`CATALOGUE`] and
//! [`for_each_game`], which a test holds to agreeing.
//!
//! # Tampering is part of the contract
//!
//! P4's demo criterion is a *modified client*: "a 1.5× speed multiplier joins
//! an 8-peer island: detected, escalated, replay-adjudicated". A game that can
//! only be played honestly cannot prove that half. So every game also builds
//! its own cheats ([`Game::tampered`]), and a tampered build keeps the honest
//! [`RulesetId`](orrery_protocol::RulesetId) — which is the whole point. A
//! cheater claims to be running the rules; the claim is what the witness holds
//! it to.

use orrery_core::{QPos, QVel, Ruleset, TickRng};
use orrery_protocol::{PersistId, RulesetId, Tick};

use crate::regolith::Regolith;
use crate::skirmish::Skirmish;

/// What a harness can learn about a game without instantiating it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GameMeta {
    /// Stable short name. Appears in scenario names, digests and reports.
    pub name: &'static str,
    /// One line, for a `--list` and for the reader of a failing gate.
    pub summary: &'static str,
    /// The honest build's identity. A tampered build reports this one too.
    pub ruleset: RulesetId,
}

/// A named deviation from the rules, for proving detection works.
///
/// The set is chosen so that each member is caught by a *different* stage of
/// the trust apparatus. That is the point of having three rather than one: a
/// suite where every cheat trips the same check proves one check works and
/// says nothing about the rest of the pipeline.
///
/// | Tamper | Stage 1 (invariants) | Replay adjudication |
/// |---|---|---|
/// | [`Tamper::SpeedMultiplier`] | speed and acceleration caps | continuous, outside the band |
/// | [`Tamper::DamageInflation`] | **silent** — every field stays legal | discrete, bit-exact |
/// | [`Tamper::NoCooldown`] | fire-rate limit | discrete, bit-exact |
///
/// `DamageInflation` is the one worth staring at. Nothing about an inflated
/// damage roll is *impossible* — the victim's hull drops by a legal amount,
/// the attacker's counters advance by legal steps — so no cheap check can see
/// it, and only re-execution of the attacker's own window can. It is the
/// reason stage 1 is a filter and not a verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Tamper {
    /// Movement caps raised by 1.5× — P4's demo criterion, literally.
    SpeedMultiplier,
    /// Damage rolls doubled.
    DamageInflation,
    /// The weapon cooldown is not honoured.
    NoCooldown,
}

impl Tamper {
    /// Every tamper, in a fixed order.
    pub const ALL: &'static [Tamper] = &[
        Tamper::SpeedMultiplier,
        Tamper::DamageInflation,
        Tamper::NoCooldown,
    ];

    /// A short name for test output and reports.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Tamper::SpeedMultiplier => "speed-multiplier",
            Tamper::DamageInflation => "damage-inflation",
            Tamper::NoCooldown => "no-cooldown",
        }
    }
}

/// A reference game: a `Ruleset` complete enough to be played, measured and
/// cheated at.
///
/// Every method here is deterministic, and the two the harness drives —
/// [`spawn`](Game::spawn) and [`honest_inputs`](Game::honest_inputs) — are
/// deliberately **not** functions of game state.
///
/// That last constraint is load-bearing rather than a simplification. It means
/// an honest build and a tampered build, run from the same seed, receive
/// *byte-identical input streams*, so every difference between the two runs is
/// attributable to the rules and nothing else. It is also the honest model of
/// a cheat: a modified client sends the same packets a real one would — it
/// mashes the trigger whether or not the weapon is ready — and lies in how it
/// executes them, not in what it asks for.
pub trait Game: Ruleset + Sized {
    /// This game's card in the [`CATALOGUE`].
    const META: GameMeta;

    /// The committed chain digest per scenario name (`docs/06` §8's golden,
    /// per game). Empty is legal for a game still being brought up; the
    /// battery skips what is absent rather than inventing a baseline.
    const GOLDEN_CHAINS: &'static [(&'static str, [u8; 32])] = &[];

    /// The rules as shipped.
    fn honest() -> Self;

    /// A build that breaks `tamper`, or `None` if this game has no way to
    /// express it.
    fn tampered(tamper: Tamper) -> Option<Self>;

    /// One entity's starting state. `slot` is its index in the scenario, so a
    /// game can vary loadouts across a population deterministically.
    fn spawn(&self, entity: PersistId, slot: u64) -> Self::CoreState;

    /// What an honest player of this game asks for at `tick`.
    ///
    /// Appends to `out` rather than returning, so a harness can prepend the
    /// events delivered from the previous tick without allocating twice.
    /// `peers` is every other entity in the scenario, in `PersistId` order.
    fn honest_inputs(
        &self,
        entity: PersistId,
        slot: u64,
        tick: Tick,
        peers: &[PersistId],
        rng: &mut TickRng,
        out: &mut Vec<Self::CoreInput>,
    );

    /// Where a cross-entity event lands, as the target's input for the *next*
    /// tick. `None` for an event that concerns only its emitter.
    fn deliver(&self, event: &Self::CoreEvent) -> Option<(PersistId, Self::CoreInput)>;

    /// The continuous half of a state, for
    /// [`Tolerance`](orrery_core::Tolerance). Every game has one; the
    /// comparator lives in the core and cannot reach into a game's fields
    /// without this.
    fn trajectory(state: &Self::CoreState) -> (QPos, QVel);
}

/// Every game this crate ships.
///
/// Kept in step with [`for_each_game`] by `catalogue_and_visitor_agree` in the
/// battery — a game added to one and not the other is a test failure, not a
/// silently unmeasured game.
pub const CATALOGUE: &[GameMeta] = &[Skirmish::META, Regolith::META];

/// A visitor over the catalogue, static-dispatched.
///
/// `Ruleset` has associated types, so a `Vec<Box<dyn Game>>` cannot exist and
/// the usual registry shape is unavailable. A visitor is the way to write
/// something once and have it run against every game concretely — which is
/// exactly what the battery in `tests/` is.
pub trait GameVisitor {
    /// Called once per game in the catalogue.
    fn visit<G: Game>(&mut self);
}

/// Run `visitor` over every game in the catalogue.
pub fn for_each_game<V: GameVisitor>(visitor: &mut V) {
    visitor.visit::<Skirmish>();
    visitor.visit::<Regolith>();
}
