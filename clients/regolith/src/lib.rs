//! Bevy rendering and keyboard input over Regolith's headless rules pipeline.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod assets;
pub mod combat;
pub mod craft;
pub mod hud;
pub mod intent;
pub mod session;
pub mod telemetry;

/// Commit revision embedded in this client binary at build time.
pub const BUILD_REV: &str = env!("ORRERY_BUILD_REV");

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use assets::VisualAssetPaths;
use bevy::prelude::*;
use bevy::time::common_conditions::on_timer;
use combat::{CombatView, LockBreak, ProjectileTracks};
use intent::{decode_packet, Controls, IntentPipeline};
use orrery_core::Executor;
use orrery_games::{
    regolith::archetype::Archetype,
    regolith::order::{Order, Outcome},
    regolith::state::RegolithState,
    Game, Regolith,
};
use orrery_protocol::{PersistId, Tick, UniverseSeed};
use telemetry::{JsonlTelemetry, OverlayMetrics};

const PLAYER: PersistId = PersistId::new(1);
const OPPONENT: PersistId = PersistId::new(2);
const SEED: UniverseSeed = UniverseSeed([0x61; 32]);

/// Ties a rendered body back to the core entity whose state it draws.
#[derive(Component)]
pub struct CoreEntity(
    /// The core entity this body mirrors.
    pub PersistId,
);
#[derive(Component)]
struct AlwaysOnStrip;
#[derive(Component)]
struct F3Pane;

#[derive(Debug, Default, Resource)]
struct OverlayState {
    expanded: bool,
}

#[derive(Debug, Default, Resource)]
struct MetricWindow {
    intents: u64,
    idle_ticks: u64,
}

/// A playable local authority using only the shared headless executor.
#[derive(Resource)]
struct LocalSession {
    executor: Executor<Regolith>,
    human: IntentPipeline,
    bot: IntentPipeline,
    pending: BTreeMap<PersistId, Vec<Order>>,
    tick: Tick,
}

impl Default for LocalSession {
    fn default() -> Self {
        let game = Regolith::honest();
        let mut executor = Executor::new(game, SEED);
        executor.insert(PLAYER, game.spawn(PLAYER, 0));
        executor.insert(OPPONENT, game.spawn(OPPONENT, 1));
        Self {
            executor,
            human: IntentPipeline::new(SEED, PLAYER, 0, vec![OPPONENT]),
            bot: IntentPipeline::new(SEED, OPPONENT, 1, vec![PLAYER]),
            pending: BTreeMap::new(),
            tick: Tick::new(1_000_000),
        }
    }
}

/// Installs the thin skin after Bevy's [`DefaultPlugins`].
pub struct RegolithSkinPlugin {
    telemetry_path: PathBuf,
}

impl RegolithSkinPlugin {
    /// Configure the append-only overlay stream.
    #[must_use]
    pub fn new(telemetry_path: PathBuf) -> Self {
        Self { telemetry_path }
    }
}

impl Plugin for RegolithSkinPlugin {
    fn build(&self, app: &mut App) {
        let sink = JsonlTelemetry::open(&self.telemetry_path).unwrap_or_else(|error| {
            panic!(
                "cannot open Regolith telemetry {}: {error}",
                self.telemetry_path.display()
            )
        });
        app.insert_resource(OverlayMetrics::new(self.telemetry_path.clone()))
            .insert_resource(sink)
            .init_resource::<VisualAssetPaths>()
            .init_resource::<OverlayState>()
            .init_resource::<MetricWindow>()
            .init_resource::<LocalSession>()
            .init_resource::<CombatView>()
            .init_resource::<ProjectileTracks>()
            .init_resource::<LockBreak>()
            .add_systems(Startup, setup_scene)
            .add_systems(FixedUpdate, drive_core)
            .add_systems(
                Update,
                (
                    toggle_overlay,
                    sync_rendered_state,
                    // After `sync_rendered_state`: it frames the positions
                    // that system just wrote, not last frame's.
                    frame_camera.after(sync_rendered_state),
                    refresh_strip,
                    refresh_f3_pane,
                ),
            )
            .add_systems(
                Update,
                (
                    // Everything downstream reads one snapshot of core state,
                    // and every overlay body is placed against the transforms
                    // `sync_rendered_state` wrote this frame.
                    read_combat_state,
                    hud::sync_lock_reticle,
                    hud::sync_range_rings,
                    hud::sync_tracers,
                    hud::sync_gauges,
                    hud::refresh_combat_hud,
                )
                    .chain()
                    .after(sync_rendered_state),
            )
            .add_systems(
                Update,
                stream_metrics.run_if(on_timer(Duration::from_secs(1))),
            );
    }
}

fn setup_scene(
    mut commands: Commands,
    asset_server: Res<AssetServer>,
    paths: Res<VisualAssetPaths>,
    session: Res<LocalSession>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
) {
    commands.spawn((
        Camera3d::default(),
        ChaseCamera,
        Transform::from_xyz(0.0, 500.0, 0.0).looking_at(Vec3::ZERO, Vec3::NEG_Z),
    ));
    commands.spawn((
        DirectionalLight::default(),
        Transform::from_rotation(Quat::from_rotation_x(-0.8)),
    ));
    for (entity, accent) in [(PLAYER, hud::ACCENT_BRIGHT), (OPPONENT, hud::MUTED)] {
        let mut spawned = commands.spawn((
            CoreEntity(entity),
            Transform::from_scale(Vec3::splat(craft::CRAFT_DISPLAY_SCALE)),
            Visibility::Inherited,
        ));
        if let Some(scene) = paths.craft_scene(&asset_server) {
            // The optional glTF still wins when it is on disk: this work
            // improves the fallback, it does not replace the asset path.
            spawned.insert(WorldAssetRoot(scene));
            continue;
        }
        // Otherwise the chassis is composed from Bevy primitives, per
        // archetype, out of the craft's *own hashed state*. The skin reads the
        // archetype; it never tells the ruleset what shape anything is.
        let archetype = archetype_of(&session, entity).unwrap_or(Archetype::Interceptor);
        spawned.with_children(|craft_root| {
            for part in craft::parts(archetype) {
                craft_root.spawn((
                    Name::new(part.name),
                    Mesh3d(meshes.add(craft::mesh_for(part.shape))),
                    MeshMaterial3d(materials.add(craft::finish_material(part.finish, accent))),
                    Transform {
                        translation: part.translation,
                        rotation: part.rotation,
                        scale: part.scale,
                    },
                ));
            }
        });
    }
    hud::spawn_hud(&mut commands);
    hud::spawn_world_overlay(&mut commands, &mut meshes, &mut materials);
    commands.spawn((
        Text::new("intents/s 0 | rollbacks/min 0 | discrepancies 0"),
        Node {
            position_type: PositionType::Absolute,
            top: Val::Px(8.0),
            left: Val::Px(8.0),
            ..Default::default()
        },
        AlwaysOnStrip,
    ));
    commands.spawn((
        Text::new(""),
        Node {
            display: Display::None,
            position_type: PositionType::Absolute,
            top: Val::Px(36.0),
            left: Val::Px(8.0),
            ..Default::default()
        },
        F3Pane,
    ));
}

fn controls(keys: &ButtonInput<KeyCode>) -> Controls {
    Controls {
        left: keys.pressed(KeyCode::ArrowLeft),
        right: keys.pressed(KeyCode::ArrowRight),
        thrust: keys.pressed(KeyCode::ArrowUp),
        fire: keys.pressed(KeyCode::Space),
    }
}

fn drive_core(
    keys: Res<ButtonInput<KeyCode>>,
    mut session: ResMut<LocalSession>,
    mut window: ResMut<MetricWindow>,
    mut sink: ResMut<JsonlTelemetry>,
    mut tracks: ResMut<ProjectileTracks>,
    mut broken: ResMut<LockBreak>,
) {
    let tick = session.tick;
    let controls = controls(&keys);
    let packet = session.human.human_packet(tick, controls);
    let mut human = session.pending.remove(&PLAYER).unwrap_or_default();
    human.extend(decode_packet(&packet).expect("the local codec produced valid orders"));
    window.intents = window.intents.saturating_add(packet.orders.len() as u64);
    if let Err(error) = sink.append_orders(&packet) {
        error!("cannot append Regolith order packet: {error}");
    }
    if controls == Controls::default() {
        window.idle_ticks = window.idle_ticks.saturating_add(1);
    } else {
        window.idle_ticks = 0;
    }
    let mut bot = session.pending.remove(&OPPONENT).unwrap_or_default();
    bot.extend(session.bot.bot_orders(tick));
    let mut delivered = BTreeMap::<PersistId, Vec<Order>>::new();
    let mut emitted = Vec::<Outcome>::new();
    for (entity, orders) in [(PLAYER, human), (OPPONENT, bot)] {
        let outcome = session
            .executor
            .step_entity(entity, tick, &orders)
            .expect("both craft installed");
        for event in &outcome.events {
            if let Some((target, input)) = session.executor.ruleset().deliver(event) {
                delivered.entry(target).or_default().push(input);
            }
        }
        emitted.extend(outcome.events.iter().cloned());
    }
    session.pending = delivered;
    session.tick = Tick::new(tick.0.saturating_add(1));
    // The skin's only source of truth for a shot in the air. `observe` is a
    // copy, not a simulation: it keeps the events this tick produced and
    // discards everything else, so a resolved shot loses its tracer on the
    // same tick the ruleset resolves it.
    tracks.observe(&emitted);
    broken.age();
    broken.observe(&emitted, PLAYER);
}

/// Copies this tick's combat state out of the executor for the overlay.
fn read_combat_state(session: Res<LocalSession>, mut view: ResMut<CombatView>) {
    *view = CombatView::read(&session.executor, PLAYER);
}

fn archetype_of(session: &LocalSession, entity: PersistId) -> Option<Archetype> {
    match session.executor.state(entity)? {
        RegolithState::Craft(craft) => Some(craft.archetype),
        _ => None,
    }
}

/// Marks the one camera the framing system drives.
#[derive(Component)]
struct ChaseCamera;

/// Keeps every rendered body on screen.
///
/// The camera was fixed above the origin, so anything that drifted left the
/// frame and never came back — a craft that thrusts away is simply gone, with
/// no way to tell where. This frames the bodies that exist: centre on their
/// midpoint, and raise the camera until the furthest one fits, with a floor so
/// a duel at close quarters does not slam the view into the deck.
///
/// Framing only. It reads rendered `Transform`s, which are already a pure
/// function of core state, and writes nothing back — constraint 3 forbids the
/// skin deciding anything the ruleset should.
fn frame_camera(
    bodies: Query<&Transform, (With<CoreEntity>, Without<ChaseCamera>)>,
    mut camera: Query<&mut Transform, With<ChaseCamera>>,
) {
    let Ok(mut view) = camera.single_mut() else {
        return;
    };
    let mut count = 0.0f32;
    let mut centre = Vec3::ZERO;
    for body in &bodies {
        centre += body.translation;
        count += 1.0;
    }
    if count == 0.0 {
        return;
    }
    centre /= count;
    let mut spread: f32 = 0.0;
    for body in &bodies {
        spread = spread.max(body.translation.distance(centre));
    }
    // 2.6 is empirical headroom: enough that a body at the spread radius sits
    // inside the frame rather than on its edge.
    let height = (spread * 2.6).max(500.0);
    view.translation = Vec3::new(centre.x, height, centre.z);
    *view = view.looking_at(centre, Vec3::NEG_Z);
}

fn sync_rendered_state(
    session: Res<LocalSession>,
    mut rendered: Query<(&CoreEntity, &mut Transform)>,
) {
    for (entity, mut transform) in &mut rendered {
        let Some(state) = session.executor.state(entity.0) else {
            continue;
        };
        // `RegolithState` became a sum when #323 added rocks: craft and rock
        // windows share one ruleset. Both carry a lattice position; only a
        // craft has a facing, so a rock or a pickup keeps whatever rotation it
        // was spawned with rather than being forced to zero every frame.
        //
        // A claimed or expired pickup is still rendered at its position; the
        // skin shows what the ruleset says exists and makes no judgement about
        // it. Hiding it here would be gameplay logic in the skin, which
        // constraint 3 forbids.
        let (pos, yaw_urad) = match state {
            RegolithState::Craft(craft) => (craft.pos, Some(craft.yaw_urad)),
            RegolithState::Rock(rock) => (rock.pos, None),
            RegolithState::Pickup(pickup) => (pickup.pos, None),
            // A bloom director is a scheduler, not a body: it occupies no
            // point in the lattice. Its `site_pos` announces where a bloom
            // seeds, which is not the same thing as where the director is —
            // drawing it there would put a visible object in the world that
            // the ruleset never spawned. Skip it and let the rocks it seeds
            // be the only thing the skin shows.
            RegolithState::BloomDirector(_) => continue,
        };
        let (x, _, z) = pos.to_metres();
        transform.translation = Vec3::new(x as f32, 0.0, z as f32);
        if let Some(yaw) = yaw_urad {
            // The ruleset thrusts along `(cos yaw, 0, sin yaw)` — yaw zero is
            // +X — so `from_rotation_y(-yaw)` is the correct world rotation
            // *for a mesh whose nose already points +X*.
            //
            // #377 needed a `NOSE_TO_PLUS_X` correction because the fallback
            // was a bare `Cone`, which Bevy builds pointing +Y: straight at a
            // top-down camera, so yaw spun it on its own symmetry axis and
            // nothing visibly changed. Both remaining models — the optional
            // glTF and `craft::parts` — are authored nose-along-+X, so the
            // correction is gone rather than silently doubled.
            // `heading_matches_the_rulesets_thrust_direction` pins this.
            transform.rotation = heading_rotation(yaw);
        }
    }
}

/// World rotation for a nose-along-+X model at the ruleset's yaw.
#[must_use]
pub fn heading_rotation(yaw_urad: i32) -> Quat {
    Quat::from_rotation_y(-(yaw_urad as f32 / 1_000_000.0))
}

fn toggle_overlay(keys: Res<ButtonInput<KeyCode>>, mut state: ResMut<OverlayState>) {
    if keys.just_pressed(KeyCode::F3) {
        state.expanded = !state.expanded;
    }
}

fn refresh_strip(metrics: Res<OverlayMetrics>, mut strip: Query<&mut Text, With<AlwaysOnStrip>>) {
    if let Ok(mut text) = strip.single_mut() {
        **text = format!(
            "intents/s {} | rollbacks/min {} | discrepancies {}",
            metrics.intents_per_second, metrics.rollbacks_per_minute, metrics.live_discrepancies
        );
    }
}

fn refresh_f3_pane(
    state: Res<OverlayState>,
    metrics: Res<OverlayMetrics>,
    view: Res<CombatView>,
    broken: Res<LockBreak>,
    tracks: Res<ProjectileTracks>,
    mut pane: Query<(&mut Text, &mut Node), With<F3Pane>>,
) {
    if let Ok((mut text, mut node)) = pane.single_mut() {
        node.display = if state.expanded {
            Display::Block
        } else {
            Display::None
        };
        **text = format!(
            "adjudications {} | latency p50/p99 {}/{} ms\nprediction set {} | loss observed/configured {:.2}/{:.2}%\njitter observed p50/p99 {}/{} ms | configured {} ms\nisland {:?} | cell {:?}\nbuild {}\nsession {}\nbanked {:.1} min | idle {:.1} min",
            metrics.adjudications_completed, metrics.adjudication_latency_p50_ms,
            metrics.adjudication_latency_p99_ms, metrics.prediction_set_size,
            metrics.observed_loss_pct, metrics.configured_loss_pct,
            metrics.observed_jitter_p50_ms, metrics.observed_jitter_p99_ms,
            metrics.configured_jitter_ms, metrics.island_id, metrics.cell_id, BUILD_REV,
            metrics.session_record_path.display(), metrics.banked_minutes, metrics.idle_minutes,
        );
        text.push('\n');
        text.push_str(&hud::lock_debug_lines(&view, &broken));
        text.push_str(&format!(
            "\nshots in flight {} | {}",
            tracks.tracks().len(),
            hud::target_relation(&view, &tracks)
        ));
    }
}

fn stream_metrics(
    mut metrics: ResMut<OverlayMetrics>,
    mut window: ResMut<MetricWindow>,
    mut sink: ResMut<JsonlTelemetry>,
) {
    metrics.intents_per_second = std::mem::take(&mut window.intents);
    metrics.idle_minutes = window.idle_ticks as f64 / (orrery_core::TICK_HZ as f64 * 60.0);
    if let Err(error) = sink.append(&metrics) {
        error!("cannot append Regolith telemetry: {error}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use orrery_core::state_hash;
    use orrery_games::scenario::{Entry, Play, Scenario, TickRecord};

    #[test]
    fn build_revision_tracks_the_checkout_commit() {
        let output = std::process::Command::new("git")
            .args(["rev-parse", "--verify", "HEAD"])
            .output()
            .expect("git is available in this source checkout");
        assert!(output.status.success(), "git must resolve HEAD");
        let expected = String::from_utf8(output.stdout)
            .expect("git emits UTF-8 commit ids")
            .trim()
            .to_owned();
        assert_eq!(BUILD_REV, expected, "the binary stamp must be this commit");
    }

    /// #377's fix, restated for the models that replaced its `Cone`.
    ///
    /// The ruleset thrusts along `(cos yaw, 0, sin yaw)`. A model authored
    /// nose-along-+X is only correct if the world rotation carries `+X` onto
    /// exactly that vector — if it does not, every craft flies sideways and
    /// heading, transversal and therefore the whole #352 tracking model become
    /// unreadable again.
    #[test]
    fn heading_matches_the_rulesets_thrust_direction() {
        for yaw_urad in [0, 500_000, 1_570_796, 3_141_592, 4_712_388, 6_000_000] {
            let yaw = yaw_urad as f32 / 1_000_000.0;
            let nose = heading_rotation(yaw_urad) * Vec3::X;
            let thrust = Vec3::new(yaw.cos(), 0.0, yaw.sin());
            assert!(
                nose.distance(thrust) < 1e-5,
                "yaw {yaw_urad}: the nose points {nose}, the ruleset thrusts {thrust}"
            );
        }
    }

    /// Both spawn slots must reach a *different* chassis, or per-archetype
    /// models are dead code in the only session this client runs.
    #[test]
    fn the_two_seats_fly_different_chassis() {
        let game = Regolith::honest();
        let mut executor = Executor::new(game, SEED);
        executor.insert(PLAYER, game.spawn(PLAYER, 0));
        executor.insert(OPPONENT, game.spawn(OPPONENT, 1));
        let chassis: Vec<_> = [PLAYER, OPPONENT]
            .into_iter()
            .map(|entity| match executor.state(entity).expect("installed") {
                RegolithState::Craft(craft) => craft.archetype,
                other => panic!("a seat holds a craft, not {other:?}"),
            })
            .collect();
        assert_eq!(chassis, vec![Archetype::Interceptor, Archetype::Cruiser]);
        assert_ne!(
            craft::parts(chassis[0]),
            craft::parts(chassis[1]),
            "the two seats must not draw the same model"
        );
    }

    /// The view is a copy, and a copy is only useful if it copies.
    #[test]
    fn the_combat_view_copies_the_rulesets_lock_fields() {
        let game = Regolith::honest();
        for progress in [0u16, 3, 29, 30] {
            let mut executor = Executor::new(game, SEED);
            let RegolithState::Craft(mut craft) = game.spawn(PLAYER, 0) else {
                panic!("a seat holds a craft");
            };
            craft.lock_target = Some(OPPONENT);
            craft.lock_progress = progress;
            craft.locks_acquired = 4;
            executor.insert(PLAYER, RegolithState::Craft(craft));
            executor.insert(OPPONENT, game.spawn(OPPONENT, 1));
            let view = CombatView::read(&executor, PLAYER);
            assert_eq!(view.lock.progress, progress);
            assert_eq!(view.lock.target, Some(OPPONENT));
            assert_eq!(view.lock.acquired, 4);
            assert_eq!(
                view.target.map(|target| target.archetype),
                Some(Archetype::Cruiser),
                "the locked target's own window feeds the target panel"
            );
        }
    }

    #[test]
    fn recorded_human_order_log_replays_through_game_harness() {
        let scenario = Scenario {
            name: "human-recording",
            entities: 1,
            ticks: 30,
            seed_byte: 0x61,
            sample_loss_pct: 0,
        };
        let game = Regolith::honest();
        let mut executor = Executor::new(game, SEED);
        executor.insert(PLAYER, game.spawn(PLAYER, 0));
        let pipeline = IntentPipeline::new(SEED, PLAYER, 0, Vec::new());
        let mut log = Vec::new();
        for offset in 0..scenario.ticks {
            let tick = Tick::new(orrery_games::scenario::T0 + offset);
            let packet = pipeline.human_packet(
                tick,
                Controls {
                    right: true,
                    thrust: true,
                    fire: true,
                    ..Controls::default()
                },
            );
            let inputs = decode_packet(&packet).expect("recorded core orders decode");
            let outcome = executor
                .step_entity(PLAYER, tick, &inputs)
                .expect("player installed");
            let state = executor.state(PLAYER).expect("player state").clone();
            assert_eq!(outcome.state_hash, state_hash(&state));
            log.push(TickRecord {
                tick,
                entries: vec![Entry {
                    entity: PLAYER,
                    inputs,
                    hash: outcome.state_hash,
                    state,
                }],
            });
        }
        let play = Play {
            chain: [0; 32],
            log,
            flags: Vec::new(),
            events: 0,
        };
        assert!(
            orrery_games::adjudicate(Regolith::honest(), &scenario, &play).is_none(),
            "the shared adjudicator rejected a human recording"
        );
    }
}
