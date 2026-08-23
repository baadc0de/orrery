//! Deterministic honest Regolith pilot; pitch remains exactly zero.
//!
//! This is the input-source-independent half of both the swarm bot and the
//! human client. It produces [`Order`] values; both callers use that type's
//! one [`CoreCodec`](orrery_core::CoreCodec) implementation for the wire.
//! The function is pure in the tick RNG (seeded from universe, entity and
//! tick), entity, slot and tick. It never reads simulated state.

use super::{archetype::Archetype, order::Order};
use orrery_core::TickRng;
use orrery_protocol::{PersistId, Tick};
use rand_core::RngCore;

const BASE_TURN_URAD: i32 = 12_000;
const TURN_SPREAD_URAD: i32 = 1_500;
const YAW_JITTER_URAD: i32 = 2_000;
/// Duration of one input-diversity scenario before the table advances.
pub const SCENARIO_TICKS: u64 = 180;

/// One durable pilot surface.
///
/// Combat is deliberately one isolated row: #352 may replace how a hit
/// resolves without changing the scenario schedule, the non-combat target
/// derivations or anything that accounts hours.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PilotScenario {
    /// Hold the trigger on another craft.
    Combat,
    /// Hold the trigger on a deterministic rock lineage.
    Mining,
    /// Paired slots repeatedly ask for the same pickup.
    ContestedGrab,
    /// The island turns toward and mines one announced bloom lineage.
    BloomConvergence,
}

impl PilotScenario {
    /// Stable report/table name.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Combat => "combat",
            Self::Mining => "mining",
            Self::ContestedGrab => "contested-grab",
            Self::BloomConvergence => "bloom-convergence",
        }
    }
}

/// The Regolith input-diversity table, in deterministic schedule order.
pub const PILOT_SCENARIOS: [PilotScenario; 4] = [
    PilotScenario::Combat,
    PilotScenario::Mining,
    PilotScenario::ContestedGrab,
    PilotScenario::BloomConvergence,
];

/// Scenario active at `tick`.
#[must_use]
pub const fn scenario_at(tick: Tick) -> PilotScenario {
    let index = (tick.0 / SCENARIO_TICKS) as usize % PILOT_SCENARIOS.len();
    PILOT_SCENARIOS[index]
}

/// Appends one tick of honest input.
pub fn honest_orders(
    entity: PersistId,
    slot: u64,
    tick: Tick,
    rng: &mut TickRng,
    out: &mut Vec<Order>,
) {
    let scenario = scenario_at(tick);
    let limits = Archetype::for_slot(slot).limits();
    let jitter = (rng.next_u32() % (YAW_JITTER_URAD as u32 * 2 + 1)) as i32 - YAW_JITTER_URAD;
    let direction = if slot.is_multiple_of(2) { 1 } else { -1 };
    let scenario_turn = match scenario {
        PilotScenario::Combat => direction * (BASE_TURN_URAD + jitter),
        PilotScenario::Mining => direction * (BASE_TURN_URAD / 2 + jitter),
        PilotScenario::ContestedGrab => direction * (BASE_TURN_URAD / 3 + jitter),
        PilotScenario::BloomConvergence => -direction * (BASE_TURN_URAD + jitter.abs()),
    };
    out.push(Order::Thrust {
        accel_mmss: i32::try_from(limits.max_accel_mmss).unwrap_or(i32::MAX),
        yaw_urad: scenario_turn + (slot % 4) as i32 * TURN_SPREAD_URAD,
        pitch_urad: 0,
    });

    // Held trigger: exactly one Fire order is emitted every tick. Only this
    // target selector is combat-shaped, so tracking/time-of-flight can replace
    // hit resolution without reopening the table or the non-combat ports.
    let target = match scenario {
        PilotScenario::Combat => combat_target(entity),
        PilotScenario::Mining => mining_target(slot, tick),
        PilotScenario::ContestedGrab => mining_target(slot / 2, tick),
        PilotScenario::BloomConvergence => bloom_target(tick),
    };
    out.push(Order::Fire { target });

    if scenario == PilotScenario::ContestedGrab {
        out.push(Order::Grab {
            pickup: contested_pickup(slot, tick),
        });
    }
}

fn combat_target(entity: PersistId) -> PersistId {
    // Adjacent player ids dogfight. Target choice is deliberately independent
    // of live population/state so the entire pilot stays a pure four-tuple.
    if entity.0.is_multiple_of(2) {
        PersistId::new(entity.0.saturating_sub(1))
    } else {
        PersistId::new(entity.0.saturating_add(1))
    }
}

fn scenario_epoch(tick: Tick) -> u64 {
    tick.0 / SCENARIO_TICKS
}

fn mining_target(slot: u64, tick: Tick) -> PersistId {
    PersistId::new(0xA1_0000_0000_0000 | ((scenario_epoch(tick) & 0x00ff_ffff) << 16) | slot)
}

fn bloom_target(tick: Tick) -> PersistId {
    PersistId::new(0xB1_0000_0000_0000 | (scenario_epoch(tick) & 0x00ff_ffff))
}

fn contested_pickup(slot: u64, tick: Tick) -> PersistId {
    // Adjacent peer slots deliberately collide on one target.
    PersistId::new(
        0xD1_0000_0000_0000 | ((scenario_epoch(tick) & 0x00ff_ffff) << 16) | ((slot / 2) & 0xffff),
    )
}
