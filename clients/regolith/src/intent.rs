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
    /// acceleration, yaw magnitude, elevation, target choice, order shape and
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
                    pitch_urad: _,
                } => {
                    let yaw_urad = match (controls.left, controls.right) {
                        (true, false) => -yaw_urad.abs(),
                        (false, true) => yaw_urad.abs(),
                        _ => 0,
                    };
                    Some(Order::Thrust {
                        accel_mmss: if controls.thrust { accel_mmss } else { 0 },
                        yaw_urad,
                        // Elevation is gated for the same reason `Grab` is,
                        // below: the headless pilot's `pitch_urad` is drawn
                        // from its own RNG on a function of *tick*, so
                        // passing it through has the player's craft climb and
                        // dive on a walk no input of theirs authored (#940).
                        //
                        // It is not cosmetic. `craft.rs` integrates this into
                        // `Craft::pitch_urad` on every `Thrust` order — even
                        // one whose acceleration the line above gated to
                        // zero — and thrust resolves as
                        // `delta_vy = accel * sin(phi)`, so the walk moves the
                        // craft vertically. `Controls` has no pitch field and
                        // the legend lists no elevation key, so there is no
                        // input that could have produced a nonzero value here
                        // and none that could correct one. The renderer builds
                        // craft attitude with `Quat::from_rotation_y` alone,
                        // so the drift is invisible as well as uncommandable.
                        //
                        // The bot path (`bot_orders`) is untouched, which is
                        // what keeps `PITCH_JITTER_URAD` doing its job: the
                        // determinism matrix still evaluates `sin`/`cos` away
                        // from their two exact points.
                        pitch_urad: 0,
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

    /// A fully-held seat rides the bot's own order shape and encoder, order
    /// for order and byte for byte — with the two axes the skin gates
    /// normalised out. Elevation is zero for a human craft (#940) and the
    /// pilot's scheduled `Grab` never reaches one (#568); everything else,
    /// including acceleration, yaw magnitude, target choice, order sequence
    /// and encoding, must still come from the headless path unchanged.
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
        let bot_orders = pipeline.bot_orders(tick);
        assert!(
            bot_orders
                .iter()
                .any(|order| matches!(order, Order::Thrust { pitch_urad, .. } if *pitch_urad != 0)),
            "fixture is inert: the pilot scheduled no elevation on this tick, \
             so this comparison would not exercise the gate at all"
        );
        let gated: Vec<_> = bot_orders
            .into_iter()
            .filter_map(|order| match order {
                Order::Thrust {
                    accel_mmss,
                    yaw_urad,
                    ..
                } => Some(Order::Thrust {
                    accel_mmss,
                    yaw_urad,
                    pitch_urad: 0,
                }),
                Order::Grab { .. } => None,
                other => Some(other),
            })
            .collect();
        assert_eq!(
            human.orders,
            encode_orders(&gated),
            "human and bot orders diverged on the wire beyond the gated axes"
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
        let (bot_yaw, bot_pitch) = match pipeline
            .bot_orders(tick)
            .first()
            .expect("pilot always thrusts")
        {
            Order::Thrust {
                yaw_urad,
                pitch_urad,
                ..
            } => (*yaw_urad, *pitch_urad),
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
        // Elevation is gated to zero: the pilot's scheduled jitter is not a
        // human-authored elevation, and the keyboard has no pitch control
        // that could have produced one (#940).
        assert!(
            bot_pitch != 0,
            "fixture is wrong: the pilot scheduled no elevation on this tick"
        );
        assert!(matches!(
            left.first(),
            Some(Order::Thrust { accel_mmss: 60_000, yaw_urad, pitch_urad })
                if *yaw_urad == -bot_yaw.abs() && *pitch_urad == 0
        ));
    }

    /// #940: the honest pilot draws a fresh `pitch_urad` every tick and the
    /// craft step integrates it into `Craft::pitch_urad` on *every* `Thrust`
    /// order -- including one whose acceleration the skin gated to zero.
    /// Passing that through gave a human craft an elevation random walk it
    /// could not command (no pitch key), could not see (the renderer builds
    /// its attitude with `Quat::from_rotation_y` alone) and could not stop.
    /// Elevation moves the craft: `delta_vy = accel * sin(phi)`.
    ///
    /// This is the same hazard `Order::Grab` was already gated for, and the
    /// same remedy: a human craft acts only on axes its pilot authored.
    #[test]
    fn a_human_craft_is_never_given_an_elevation_it_did_not_author() {
        let pipeline = pipeline();
        // Every distinct control shape a seat can hold, over more than one
        // full turn of the pilot's four-scenario schedule.
        let shapes = [
            Controls::default(),
            Controls {
                thrust: true,
                ..Controls::default()
            },
            Controls {
                left: true,
                thrust: true,
                fire: true,
                ..Controls::default()
            },
            Controls {
                right: true,
                thrust: true,
                fire: true,
                lock_target: Some(PersistId::new(2)),
                ..Controls::default()
            },
        ];
        let mut scheduled_nonzero = 0u32;
        for raw_tick in 0..(4 * 180 + 37) {
            let tick = Tick::new(raw_tick);
            if pipeline
                .bot_orders(tick)
                .iter()
                .any(|order| matches!(order, Order::Thrust { pitch_urad, .. } if *pitch_urad != 0))
            {
                scheduled_nonzero += 1;
            }
            for controls in shapes {
                for order in pipeline.human_orders(tick, controls) {
                    if let Order::Thrust { pitch_urad, .. } = order {
                        assert_eq!(
                            pitch_urad, 0,
                            "tick {raw_tick}: the craft was handed elevation \
                             {pitch_urad} urad its pilot never authored"
                        );
                    }
                }
            }
        }
        assert!(
            scheduled_nonzero > 0,
            "fixture is inert: the pilot scheduled no elevation on any tick"
        );
    }

    /// The bot path keeps its elevation. `PITCH_JITTER_URAD` exists so the
    /// four-platform determinism matrix evaluates `sin`/`cos` away from their
    /// two exact points; gating the *human* adapter must not disarm that.
    #[test]
    fn the_bot_pilot_keeps_flying_all_three_axes() {
        let pipeline = pipeline();
        let mut nonzero = 0u32;
        for raw_tick in 0..600 {
            for order in pipeline.bot_orders(Tick::new(raw_tick)) {
                if let Order::Thrust { pitch_urad, .. } = order {
                    if pitch_urad != 0 {
                        nonzero += 1;
                    }
                }
            }
        }
        assert!(
            nonzero > 400,
            "the headless pilot stopped exercising elevation ({nonzero}/600)"
        );
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

/// The end-to-end consequence of the elevation gate, flown against the real
/// ruleset for the length of a real attempt.
#[cfg(test)]
mod attempt_length_flight {
    use super::*;
    use orrery_core::Executor;
    use orrery_games::regolith::state::RegolithState;
    use orrery_games::regolith::CAMPAIGN_CELL_EDGE_M;
    use orrery_games::Game;

    /// One witnessed attempt, as flown on 2026-09-02.
    const ATTEMPT_TICKS: u64 = 900 * orrery_core::TICK_HZ as u64;

    /// Fly one seat for `ATTEMPT_TICKS` and return its final craft state.
    ///
    /// `reinject_pilot_pitch` restores the pre-#940 behaviour, where the
    /// headless pilot's scheduled `pitch_urad` reached a human craft.
    fn fly(reinject_pilot_pitch: bool) -> (i32, i64, i64) {
        let seed = UniverseSeed([0x61; 32]);
        let me = PersistId::new(5);
        let pipeline = IntentPipeline::new(seed, me, 5, vec![PersistId::new(6)]);
        let game = Regolith::honest();
        let mut executor = Executor::new(game, seed);
        executor.insert(me, game.spawn(me, 5));
        // A seat under power and turning: the ordinary way a craft is flown.
        let controls = Controls {
            thrust: true,
            right: true,
            ..Controls::default()
        };
        for raw in 0..ATTEMPT_TICKS {
            let tick = Tick::new(raw);
            let mut orders = pipeline.human_orders(tick, controls);
            if reinject_pilot_pitch {
                let scheduled =
                    pipeline
                        .bot_orders(tick)
                        .into_iter()
                        .find_map(|order| match order {
                            Order::Thrust { pitch_urad, .. } => Some(pitch_urad),
                            _ => None,
                        });
                for order in &mut orders {
                    if let (Order::Thrust { pitch_urad, .. }, Some(scheduled)) =
                        (&mut *order, scheduled)
                    {
                        *pitch_urad = scheduled;
                    }
                }
            }
            let _ = executor.step_entity(me, tick, &orders);
        }
        match executor.state(me) {
            Some(RegolithState::Craft(craft)) => (craft.pitch_urad, craft.pos.y, craft.vel.y),
            _ => panic!("the craft is installed"),
        }
    }

    /// #940, the visible half: over one 900-second attempt the pilot's
    /// per-tick elevation jitter integrated into a craft the player could not
    /// command and the renderer never drew, carrying it tens of kilometres
    /// off the plane every other craft was flying on.
    ///
    /// The control here is the interest cell edge, 512 m. Two seats drifting
    /// independently by many multiples of that are not in each other's
    /// interest set at all — no craft, no replication — and every shot
    /// between them is far outside the resolver's ~373 m reach, which is
    /// exactly the pair of symptoms the attempt reported.
    #[test]
    fn a_seat_flying_a_full_attempt_holds_the_plane_it_was_spawned_on() {
        let (pitch, y_mm, vy_mms) = fly(false);
        assert_eq!(
            pitch, 0,
            "a human craft accumulated an elevation it never authored"
        );
        assert_eq!(y_mm, 0, "a human craft left the plane it was spawned on");
        assert_eq!(vy_mms, 0, "a human craft acquired a vertical velocity");

        // And the hazard this is guarding is real, not hypothetical: the same
        // flight with the pilot's scheduled elevation restored ends an
        // attempt's worth of cells away from where it started.
        let (drift_pitch, drift_y_mm, _) = fly(true);
        assert_ne!(drift_pitch, 0);
        let drift_cells = (drift_y_mm.abs() / 1_000) as f64 / CAMPAIGN_CELL_EDGE_M;
        assert!(
            drift_cells > 10.0,
            "this test is inert: the un-gated flight drifted only {drift_cells:.1} \
             interest cells, so it would not have hidden anything"
        );
    }
}
