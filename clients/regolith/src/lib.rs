//! Bevy rendering and keyboard input over Regolith's headless rules pipeline.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod assets;
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
use intent::{decode_packet, Controls, IntentPipeline};
use orrery_core::Executor;
use orrery_games::{regolith::order::Order, regolith::state::RegolithState, Game, Regolith};
use orrery_protocol::{PersistId, Tick, UniverseSeed};
use telemetry::{JsonlTelemetry, OverlayMetrics};

/// Turns the primitive cone's +Y nose into the +X the ruleset treats as yaw
/// zero. Baked here rather than into the mesh so a real craft scene, which is
/// authored nose-forward, needs no correction.
const NOSE_TO_PLUS_X: Quat = Quat::from_xyzw(
    0.0,
    0.0,
    -core::f32::consts::FRAC_1_SQRT_2,
    core::f32::consts::FRAC_1_SQRT_2,
);

const PLAYER: PersistId = PersistId::new(1);
const OPPONENT: PersistId = PersistId::new(2);
const SEED: UniverseSeed = UniverseSeed([0x61; 32]);

#[derive(Component)]
struct CoreEntity(PersistId);
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
                stream_metrics.run_if(on_timer(Duration::from_secs(1))),
            );
    }
}

fn setup_scene(
    mut commands: Commands,
    asset_server: Res<AssetServer>,
    paths: Res<VisualAssetPaths>,
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
    let primitive_mesh = meshes.add(Cone::new(4.0, 12.0));
    for (entity, color) in [
        (PLAYER, Color::srgb(0.2, 0.8, 1.0)),
        (OPPONENT, Color::srgb(1.0, 0.35, 0.2)),
    ] {
        let mut spawned = commands.spawn((CoreEntity(entity), Transform::default()));
        if let Some(scene) = paths.craft_scene(&asset_server) {
            spawned.insert(WorldAssetRoot(scene));
        } else {
            spawned.insert((
                Mesh3d(primitive_mesh.clone()),
                MeshMaterial3d(materials.add(color)),
            ));
        }
    }
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
    }
    session.pending = delivered;
    session.tick = Tick::new(tick.0.saturating_add(1));
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
            // *for a mesh whose nose already points +X*. The primitive is a
            // `Cone`, which Bevy builds pointing +Y, i.e. straight at a
            // top-down camera: rotating it about Y spins it on its own
            // symmetry axis and nothing visibly changes. Compose the
            // nose-to-+X correction first so heading is legible.
            transform.rotation =
                Quat::from_rotation_y(-(yaw as f32 / 1_000_000.0)) * NOSE_TO_PLUS_X;
        }
    }
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
