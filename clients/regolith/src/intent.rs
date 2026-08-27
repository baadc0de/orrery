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
    /// Mouse-selected target sustained through the ordinary lock order.
    pub lock_target: Option<PersistId>,
    /// The pickup the skin's proximity emitter claims this tick, if any.
    ///
    /// There is no grab key (#568): `crate::grab` reads the ruleset's own
    /// reach against replicated pickup state and latches one order per pickup
    /// per approach. `None` on every other tick.
    pub grab: Option<PersistId>,
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
        let mut saw_lock = false;
        let mut orders: Vec<_> = self
            .bot_orders(tick)
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
                Order::Fire if !controls.fire => None,
                // The pilot's schedule reaches `PilotScenario::ContestedGrab`
                // on a function of *tick*, at a pickup of its own choosing.
                // Passing that through would have the player's craft grab
                // unbidden, at a target no input of theirs selected (#568).
                // A human craft grabs only from `controls.grab`, below.
                Order::Grab { .. } => None,
                Order::Lock { target } => {
                    saw_lock = true;
                    Some(Order::Lock {
                        target: controls.lock_target.unwrap_or(target),
                    })
                }
                other => Some(other),
            })
            .collect();
        if let Some(target) = controls.lock_target.filter(|_| !saw_lock) {
            let before_fire = orders
                .iter()
                .position(|order| matches!(order, Order::Fire))
                .unwrap_or(orders.len());
            orders.insert(before_fire, Order::Lock { target });
        }
        if let Some(pickup) = controls.grab {
            // `Grab` and not `GrabAttempt`: `Grab { pickup }` is the ship-side
            // order, which the craft's own step turns into
            // `Outcome::GrabAttempted` carrying the ruleset's own stamp of the
            // ship position. `GrabAttempt { ship, ship_pos }` is what
            // `Regolith::deliver` then hands the pickup — an order a client
            // has no business authoring, since authoring it would let the skin
            // state a position the ruleset never stamped.
            orders.push(Order::Grab { pickup });
        }
        orders
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
                    .filter(|order| matches!(order, Order::Fire))
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

    /// A tick the pilot's schedule spends in `ContestedGrab`, which is when
    /// it emits `Order::Grab` at a pickup of its own choosing.
    fn contested_grab_tick() -> Tick {
        use orrery_games::regolith::pilot::{scenario_at, PilotScenario, SCENARIO_TICKS};
        let tick = Tick::new(SCENARIO_TICKS * PILOT_CONTESTED_GRAB_INDEX + 7);
        assert_eq!(
            scenario_at(tick),
            PilotScenario::ContestedGrab,
            "the schedule moved; this fixture must follow it"
        );
        tick
    }

    /// Index of `ContestedGrab` in `PILOT_SCENARIOS`.
    const PILOT_CONTESTED_GRAB_INDEX: u64 = 2;

    #[test]
    fn the_pilots_scheduled_grab_never_reaches_a_human_craft() {
        let pipeline = pipeline();
        let tick = contested_grab_tick();
        assert!(
            pipeline
                .bot_orders(tick)
                .iter()
                .any(|order| matches!(order, Order::Grab { .. })),
            "fixture is wrong: the pilot did not schedule a grab on this tick"
        );
        for controls in [
            Controls::default(),
            Controls {
                left: true,
                thrust: true,
                fire: true,
                lock_target: Some(PersistId::new(0x442)),
                ..Controls::default()
            },
        ] {
            let orders = pipeline.human_orders(tick, controls);
            assert!(
                !orders
                    .iter()
                    .any(|order| matches!(order, Order::Grab { .. })),
                "the craft grabbed unbidden, at a pickup the player never chose"
            );
        }
    }

    #[test]
    fn the_proximity_emitter_grabs_the_pickup_the_skin_selected() {
        let pipeline = pipeline();
        let chosen = PersistId::new(0x91cc);
        let orders = pipeline.human_orders(
            contested_grab_tick(),
            Controls {
                grab: Some(chosen),
                ..Controls::default()
            },
        );
        let grabs: Vec<_> = orders
            .iter()
            .filter_map(|order| match order {
                Order::Grab { pickup } => Some(*pickup),
                _ => None,
            })
            .collect();
        assert_eq!(
            grabs,
            vec![chosen],
            "exactly one grab, at the skin's own pickup"
        );
    }

    #[test]
    fn no_grab_is_emitted_without_one_selected() {
        let pipeline = pipeline();
        for raw_tick in 0..(4 * 180) {
            let orders = pipeline.human_orders(Tick::new(raw_tick), Controls::default());
            assert!(
                !orders
                    .iter()
                    .any(|order| matches!(order, Order::Grab { .. })),
                "tick {raw_tick} emitted a grab from no intent"
            );
        }
    }

    #[test]
    fn mouse_target_replaces_the_pilots_lock_without_changing_the_order_path() {
        let pipeline = pipeline();
        let clicked = PersistId::new(0x442);
        let orders = pipeline.human_orders(
            Tick::new(91),
            Controls {
                lock_target: Some(clicked),
                ..Controls::default()
            },
        );
        assert!(orders
            .iter()
            .any(|order| matches!(order, Order::Lock { target } if *target == clicked)));
        assert!(!orders
            .iter()
            .any(|order| matches!(order, Order::Lock { target } if *target != clicked)));
    }
}
