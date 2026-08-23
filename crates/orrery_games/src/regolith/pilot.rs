//! Deterministic honest Regolith pilot; pitch remains exactly zero.
use super::{archetype::Archetype, order::Order};
use orrery_core::TickRng;
use orrery_protocol::{PersistId, Tick};
use rand_core::RngCore;

const BASE_TURN_URAD: i32 = 12_000;
const TURN_SPREAD_URAD: i32 = 1_500;
const YAW_JITTER_URAD: i32 = 2_000;

/// Appends one tick of honest input.
pub fn honest_orders(
    slot: u64,
    _tick: Tick,
    peers: &[PersistId],
    rng: &mut TickRng,
    out: &mut Vec<Order>,
) {
    let limits = Archetype::for_slot(slot).limits();
    let jitter = (rng.next_u32() % (YAW_JITTER_URAD as u32 * 2 + 1)) as i32 - YAW_JITTER_URAD;
    out.push(Order::Thrust {
        accel_mmss: i32::try_from(limits.max_accel_mmss).unwrap_or(i32::MAX),
        yaw_urad: BASE_TURN_URAD + (slot % 4) as i32 * TURN_SPREAD_URAD + jitter,
        pitch_urad: 0,
    });
    let choice = rng.next_u32() as usize;
    if !peers.is_empty() {
        out.push(Order::Fire {
            target: peers[choice % peers.len()],
        });
    }
}
