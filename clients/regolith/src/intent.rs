//! Keyboard state adapted onto Regolith's headless pilot and core codec.

use orrery_core::{tick_rng, CoreCodec};
use orrery_games::{regolith::order::Order, Game, Regolith};
use orrery_protocol::{PersistId, Tick, UniverseSeed};
use serde::{Deserialize, Serialize};

/// The complete keyboard vocabulary of the v1 skin.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Controls {
    /// Turn counter-clockwise.
    pub left: bool,
    /// Turn clockwise.
    pub right: bool,
    /// Apply the pilot's full acceleration.
    pub thrust: bool,
    /// Emit one trigger order this tick.
    pub fire: bool,
}

/// One tick of canonical core input bytes, suitable for a session JSONL log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrderPacket {
    /// Absolute universe tick.
    pub tick: u64,
    /// Persistent entity id.
    pub entity: u64,
    /// Each order's [`CoreCodec`] bytes, in VC-2 order.
    pub orders: Vec<Vec<u8>>,
}

/// Adapts input sources onto the one Regolith pilot and codec path.
#[derive(Debug, Clone)]
pub struct IntentPipeline {
    game: Regolith,
    seed: UniverseSeed,
    entity: PersistId,
    slot: u64,
    peers: Vec<PersistId>,
}

impl IntentPipeline {
    /// Build a pipeline for one controlled craft.
    #[must_use]
    pub fn new(seed: UniverseSeed, entity: PersistId, slot: u64, peers: Vec<PersistId>) -> Self {
        Self {
            game: Regolith::honest(),
            seed,
            entity,
            slot,
            peers,
        }
    }

    /// Produce the bot pilot's orders for a tick.
    #[must_use]
    pub fn bot_orders(&self, tick: Tick) -> Vec<Order> {
        let mut orders = Vec::new();
        let mut rng = tick_rng(self.seed, self.entity, tick);
        self.game.honest_inputs(
            self.entity,
            self.slot,
            tick,
            &self.peers,
            &mut rng,
            &mut orders,
        );
        orders
    }

    /// Produce keyboard-selected orders from the bot pilot's exact profile.
    ///
    /// This deliberately starts with [`Game::honest_inputs`]. The skin only
    /// gates acceleration/trigger and chooses the sign of the pilot's yaw;
    /// acceleration, yaw magnitude, pitch lock, target choice, order shape and
    /// encoding remain owned by the headless game path.
    #[must_use]
    pub fn human_orders(&self, tick: Tick, controls: Controls) -> Vec<Order> {
        self.bot_orders(tick)
            .into_iter()
            .filter_map(|order| match order {
                Order::Thrust {
                    accel_mmss,
                    yaw_urad,
                    pitch_urad,
                } => {
                    let yaw_urad = match (controls.left, controls.right) {
                        (true, false) => -yaw_urad.abs(),
                        (false, true) => yaw_urad.abs(),
                        _ => 0,
                    };
                    Some(Order::Thrust {
                        accel_mmss: if controls.thrust { accel_mmss } else { 0 },
                        yaw_urad,
                        pitch_urad,
                    })
                }
                Order::Fire { .. } if !controls.fire => None,
                other => Some(other),
            })
            .collect()
    }

    /// Encode a human tick with the same [`CoreCodec`] used by bot logs.
    #[must_use]
    pub fn human_packet(&self, tick: Tick, controls: Controls) -> OrderPacket {
        let orders = self.human_orders(tick, controls);
        OrderPacket {
            tick: tick.0,
            entity: self.entity.0,
            orders: encode_orders(&orders),
        }
    }
}

/// Encode core orders exactly once, independent of input source.
#[must_use]
pub fn encode_orders(orders: &[Order]) -> Vec<Vec<u8>> {
    orders
        .iter()
        .map(|order| {
            let mut bytes = Vec::new();
            order.encode(&mut bytes);
            bytes
        })
        .collect()
}

/// Decode a recorded packet for the ordinary executor/replay path.
pub fn decode_packet(packet: &OrderPacket) -> Result<Vec<Order>, orrery_core::CodecError> {
    packet
        .orders
        .iter()
        .map(|bytes| Order::decode(bytes))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pipeline() -> IntentPipeline {
        IntentPipeline::new(
            UniverseSeed([0x61; 32]),
            PersistId::new(1),
            0,
            vec![PersistId::new(2)],
        )
    }

    #[test]
    fn human_full_controls_match_bot_order_bytes() {
        let pipeline = pipeline();
        let tick = Tick::new(1_000_123);
        let human = pipeline.human_packet(
            tick,
            Controls {
                right: true,
                thrust: true,
                fire: true,
                ..Controls::default()
            },
        );
        let bot = encode_orders(&pipeline.bot_orders(tick));
        assert_eq!(
            human.orders, bot,
            "human and bot orders diverged on the wire"
        );
    }

    #[test]
    fn held_trigger_is_exactly_one_fire_per_tick() {
        let pipeline = pipeline();
        for raw_tick in 10..20 {
            let orders = pipeline.human_orders(
                Tick::new(raw_tick),
                Controls {
                    fire: true,
                    ..Controls::default()
                },
            );
            assert_eq!(
                orders
                    .iter()
                    .filter(|order| matches!(order, Order::Fire { .. }))
                    .count(),
                1
            );
        }
    }

    #[test]
    fn arrows_only_select_pilot_values() {
        let pipeline = pipeline();
        let tick = Tick::new(91);
        let bot_yaw = match pipeline
            .bot_orders(tick)
            .first()
            .expect("pilot always thrusts")
        {
            Order::Thrust { yaw_urad, .. } => *yaw_urad,
            _ => panic!("pilot's first order is thrust"),
        };
        let left = pipeline.human_orders(
            tick,
            Controls {
                left: true,
                thrust: true,
                ..Controls::default()
            },
        );
        assert!(matches!(
            left.first(),
            Some(Order::Thrust { accel_mmss: 60_000, yaw_urad, pitch_urad: 0 })
                if *yaw_urad == -bot_yaw.abs()
        ));
    }
}
