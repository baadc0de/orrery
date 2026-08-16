//! The honest pilot: what a legitimate player of this game asks for.
//!
//! This is **harness code, not core rules** — it never runs inside `step` and
//! nothing adjudicates it. It is still fully deterministic, because the whole
//! measurement rests on being able to say "same inputs, different execution"
//! about an honest and a tampered run.
//!
//! Two properties are deliberate:
//!
//! - **It is a pure function of `(seed, entity, slot, tick)`** — it never reads
//!   game state. So an honest build and a cheating one receive byte-identical
//!   input streams, and every difference between the two runs is attributable
//!   to the rules.
//! - **It holds the trigger down.** One fire order per tick, which is what a
//!   held button produces at a 60 Hz input rate, and the *rules* — not the
//!   client — decide which of them becomes a shot. That is what makes
//!   [`Tamper::NoCooldown`](crate::Tamper::NoCooldown) visible at all: the
//!   cheat does not send more packets, it honours fewer of them.
//!
//! The flight profile is a lazy orbit rather than a straight line, and that is
//! load-bearing too. Craft accelerating away from each other stop being in
//! weapon range within a few seconds, and a combat scenario that quietly
//! becomes a coasting scenario measures the wrong thing. Turning at roughly
//! 0.7–1.0 rad/s while thrusting at the ceiling holds an interceptor on a
//! ~150 m circle and a cruiser on a ~75 m one, so the population stays inside
//! its own weapon reach for the whole window.

use orrery_core::TickRng;
use orrery_protocol::{PersistId, Tick};
use rand_core::RngCore;

use super::archetype::Archetype;
use super::order::Order;

/// Base turn rate, micro-radians per tick — about 0.72 rad/s.
const BASE_TURN_URAD: i32 = 12_000;

/// Per-slot spread on the turn rate, so craft hold different radii and drift
/// in and out of each other's reach instead of flying in formation.
const TURN_SPREAD_URAD: i32 = 1_500;

/// Yaw jitter, ± this many micro-radians per tick.
const YAW_JITTER_URAD: i32 = 2_000;

/// Pitch jitter, ± this many micro-radians per tick.
const PITCH_JITTER_URAD: i32 = 1_500;

/// Append one tick of honest input for `entity`.
///
/// `tick` is unused: `rng` is already seeded per tick, so time enters this
/// pilot through the stream rather than through the arithmetic. A pilot with a
/// scripted routine — patrol here, then there — would read it.
pub fn honest_orders(
    slot: u64,
    _tick: Tick,
    peers: &[PersistId],
    rng: &mut TickRng,
    out: &mut Vec<Order>,
) {
    let limits = Archetype::for_slot(slot).limits();

    let turn = BASE_TURN_URAD + (slot % 4) as i32 * TURN_SPREAD_URAD;
    let yaw_jitter = signed(rng.next_u32(), YAW_JITTER_URAD);
    let pitch_jitter = signed(rng.next_u32(), PITCH_JITTER_URAD);

    out.push(Order::Thrust {
        // The ceiling itself: an honest client asks for everything the rules
        // allow and not one quantum more, which is what leaves the clamp inert
        // on this path and makes a raised ceiling the only way to go faster.
        accel_mmss: i32::try_from(limits.max_accel_mmss).unwrap_or(i32::MAX),
        yaw_urad: turn + yaw_jitter,
        pitch_urad: pitch_jitter,
    });

    // The draw happens whether or not there is anyone to shoot at, so the
    // pilot's stream does not depend on the population size.
    let choice = rng.next_u32() as usize;
    if !peers.is_empty() {
        out.push(Order::Fire {
            target: peers[choice % peers.len()],
        });
    }
}

/// A draw mapped into `[-magnitude, magnitude]`.
fn signed(draw: u32, magnitude: i32) -> i32 {
    let span = magnitude.saturating_mul(2).saturating_add(1);
    #[allow(clippy::cast_possible_wrap)]
    let value = (draw % span as u32) as i32;
    value - magnitude
}
