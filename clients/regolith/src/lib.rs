//! Bevy rendering and keyboard input over Regolith's headless rules pipeline.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod admission;
pub mod assets;
pub mod campaign;
pub mod combat;
pub mod craft;
pub mod hud;
pub mod identity;
pub mod intent;
pub mod join;
pub mod net;
pub mod session;
pub mod telemetry;

/// Commit revision embedded in this client binary at build time.
pub const BUILD_REV: &str = env!("ORRERY_BUILD_REV");

/// Public campaign-admission origin used by a no-argument volunteer launch.
pub const DEFAULT_ADMISSION_URL: &str = "https://campaigns.distopik.com";

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use assets::VisualAssetPaths;
use bevy::prelude::*;
use bevy::time::common_conditions::on_timer;
use bevy::window::PrimaryWindow;
use combat::{CombatView, LockBreak, ProjectileTracks, ShotFeedback};
use intent::{decode_packet, Controls, IntentPipeline};
use orrery_core::{Executor, TICK_NANOS};
use orrery_games::{
    regolith::archetype::Archetype,
    regolith::order::{Order, Outcome},
    regolith::state::RegolithState,
    Game, Regolith,
};
use orrery_protocol::{CellId, PersistId, Tick, UniverseSeed};
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
/// Marks one drawn firing-arc fan and the craft it belongs to.
#[derive(Component)]
pub struct FiringArcFan(
    /// The core entity whose chassis this arc marks.
    pub PersistId,
);

/// The archetype and seat used for a craft body's current visual composition.
///
/// This is presentation state only. It lets the skin recognise an early
/// slot-derived body that must be replaced when replication supplies the
/// craft's authoritative archetype.
#[derive(Component, Debug, Clone, Copy)]
struct CraftBodyComposition {
    archetype: Archetype,
    seat: craft::Seat,
}

#[derive(Component)]
struct RockBody;

#[derive(Debug, Default, Resource)]
struct SelectedLock {
    target: Option<PersistId>,
}

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
///
/// **Offline and smoke use only — never a campaign path.** Nothing this
/// session runs produces campaign evidence: no join happened, so no link was
/// measured, and a banked hour requires the joined-session state machine
/// ([`campaign::CampaignRuntime`]) to have run.
#[derive(Resource)]
pub struct LocalSession {
    /// The in-process executor holding both seats of the offline duel.
    pub executor: Executor<Regolith>,
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

/// Which world the skin renders against.
///
/// Exactly one of these exists per run. [`ActiveSession::Local`] is the
/// offline path; [`ActiveSession::Campaign`] is #386's joined path, whose
/// orders flow through the same [`IntentPipeline::human_packet`] and whose
/// replicated state arrives on slice 1's wire surface. Input source and
/// rendering are the only deltas between them (#320 constraint 3), which is
/// why both arms below call the same pipeline and step the same executor.
#[derive(Resource)]
pub enum ActiveSession {
    /// Offline in-process executor; smoke tests and keyboard play.
    Local(Box<LocalSession>),
    /// Joined to an island host over iroh.
    Campaign(Box<campaign::CampaignRuntime>),
}

impl ActiveSession {
    /// The executor holding rendered state (local authority or replicated).
    #[must_use]
    pub fn executor(&self) -> &Executor<Regolith> {
        match self {
            Self::Local(local) => &local.executor,
            Self::Campaign(runtime) => runtime.executor(),
        }
    }

    /// The craft the keyboard drives.
    #[must_use]
    pub fn local_entity(&self) -> PersistId {
        match self {
            Self::Local(_) => PLAYER,
            Self::Campaign(runtime) => runtime.entity(),
        }
    }

    /// The remote craft the duel view follows, when one is known.
    #[must_use]
    pub fn focus_entity(&self) -> Option<PersistId> {
        match self {
            Self::Local(_) => Some(OPPONENT),
            Self::Campaign(runtime) => runtime.focus(),
        }
    }
}

/// Installs the thin skin after Bevy's [`DefaultPlugins`].
pub struct RegolithSkinPlugin {
    telemetry_path: PathBuf,
    campaign: Option<campaign::CampaignConfig>,
}

impl RegolithSkinPlugin {
    /// Configure the append-only overlay stream.
    #[must_use]
    pub fn new(telemetry_path: PathBuf) -> Self {
        Self {
            telemetry_path,
            campaign: None,
        }
    }

    /// Join an island host instead of running offline (#386). Without this,
    /// the client stays on [`ActiveSession::Local`] and banks nothing.
    #[must_use]
    pub fn with_campaign(mut self, config: campaign::CampaignConfig) -> Self {
        self.campaign = Some(config);
        self
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
        // The joined session starts its dial here, at plugin build, so the
        // handshake overlaps window startup instead of serialising behind it.
        let session = match &self.campaign {
            Some(config) => ActiveSession::Campaign(Box::new(campaign::CampaignRuntime::launch(
                config.clone(),
                SEED,
            ))),
            None => ActiveSession::Local(Box::<LocalSession>::default()),
        };
        app.insert_resource(OverlayMetrics::new(self.telemetry_path.clone()))
            .insert_resource(sink)
            // The harness and the design run 60 Hz; Bevy's FixedUpdate
            // default is 64. A campaign tick that drifts from TICK_HZ would
            // misstate every per-second rate the overlay shows.
            .insert_resource(Time::<Fixed>::from_duration(Duration::from_nanos(
                TICK_NANOS,
            )))
            .insert_resource(session)
            .init_resource::<VisualAssetPaths>()
            .init_resource::<OverlayState>()
            .init_resource::<MetricWindow>()
            .init_resource::<SelectedLock>()
            .init_resource::<CombatView>()
            .init_resource::<ProjectileTracks>()
            .init_resource::<LockBreak>()
            .init_resource::<ShotFeedback>()
            .add_systems(Startup, setup_scene)
            .add_systems(FixedUpdate, drive_core)
            .add_systems(
                Update,
                (
                    toggle_overlay,
                    sync_rendered_state,
                    select_clicked_body.before(sync_rendered_state),
                    recompose_craft_bodies.after(sync_rendered_state),
                    ensure_local_body.after(sync_rendered_state),
                    ensure_focus_body.after(recompose_craft_bodies),
                    ensure_rock_bodies.after(sync_rendered_state),
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
                    hud::sync_impact_flash,
                    hud::sync_gauges,
                    hud::refresh_combat_hud,
                )
                    .chain()
                    .after(sync_rendered_state),
            )
            .add_systems(Update, write_campaign_record_on_exit)
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
    session: Res<ActiveSession>,
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
    // The local seat always exists. In a joined session the remote seat may
    // not be known yet (nothing has replicated); `ensure_focus_body` spawns
    // it the moment the first remote craft lands.
    let mut seats: Vec<(PersistId, craft::Seat)> =
        vec![(session.local_entity(), craft::Seat::Player)];
    if let Some(focus) = session.focus_entity() {
        seats.push((focus, craft::Seat::Bot));
    }
    for (entity, seat) in seats {
        spawn_craft_body(
            &mut commands,
            &asset_server,
            &paths,
            session.executor(),
            entity,
            seat,
            Transform::from_scale(Vec3::splat(craft::CRAFT_DISPLAY_SCALE)),
            &mut meshes,
            &mut materials,
        );
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

/// Spawns one rendered body for a core entity.
///
/// Extracted from `setup_scene` so [`ensure_focus_body`] can build the remote
/// seat on demand with identical composition rules.
#[allow(clippy::too_many_arguments)]
fn spawn_craft_body(
    commands: &mut Commands,
    asset_server: &AssetServer,
    paths: &VisualAssetPaths,
    executor: &Executor<Regolith>,
    entity: PersistId,
    seat: craft::Seat,
    transform: Transform,
    meshes: &mut ResMut<Assets<Mesh>>,
    materials: &mut ResMut<Assets<StandardMaterial>>,
) {
    // The seat's plate tint is #383's allegiance cue; the accent still
    // drives trim and glow per the design ("this one is mine" for the
    // player, neutral ramp for everyone else).
    let accent = match seat {
        craft::Seat::Player => hud::ACCENT_BRIGHT,
        craft::Seat::Bot => hud::MUTED,
    };
    // The arcs are children of the craft root, which is the whole point:
    // `sync_rendered_state` already puts `heading_rotation(yaw)` on that
    // root, so an arc authored in the ruleset's own `(cos θ, 0, sin θ)`
    // craft-local frame lands on the ruleset's bearing without a second,
    // independently-derivable world rotation to get wrong. #377's `Cone`
    // trap was exactly a second derivation.
    //
    // They are spawned before the glTF branch below so the marking survives
    // whichever hull the run draws.
    //
    // The chassis comes from the craft's *own hashed state*. The skin reads
    // the archetype; it never tells the ruleset what shape anything is. A
    // joined session may not hold this entity's state yet, so the slot's own
    // archetype derivation stands in until replication fills it in — the
    // same stand-in the hull below has always used. If replication later
    // contradicts it, `recompose_craft_bodies` replaces this whole body.
    let archetype = archetype_of(executor, entity).unwrap_or_else(|| match seat {
        craft::Seat::Player => Archetype::Interceptor,
        craft::Seat::Bot => archetype_for_remote(entity),
    });
    let mut spawned = commands.spawn((
        CoreEntity(entity),
        CraftBodyComposition { archetype, seat },
        transform,
        Visibility::Inherited,
    ));
    let arc_radius = craft::hull_length(archetype) * craft::ARC_RADIUS_HULL_LENGTHS;
    let arc_material = materials.add(hud::firing_arc_material(seat, accent));
    spawned.with_children(|craft_root| {
        for arc in craft::firing_arcs(archetype) {
            craft_root.spawn((
                Name::new(arc.name),
                FiringArcFan(entity),
                Mesh3d(meshes.add(craft::arc_mesh(*arc, arc_radius))),
                MeshMaterial3d(arc_material.clone()),
                // Just off the deck, so the fan reads over the plan view
                // instead of z-fighting the hull plate it sits under.
                Transform::from_xyz(0.0, hud::FIRING_ARC_LIFT_M, 0.0),
            ));
        }
    });
    if let Some(scene) = paths.craft_scene(asset_server) {
        // The optional glTF still wins when it is on disk: this work
        // improves the fallback, it does not replace the asset path.
        spawned.insert(WorldAssetRoot(scene));
        return;
    }
    // Otherwise the chassis is composed from Bevy primitives, per archetype,
    // from the same reading the arcs above took.
    spawned.with_children(|craft_root| {
        for part in craft::parts(archetype) {
            craft_root.spawn((
                Name::new(part.name),
                Mesh3d(meshes.add(craft::mesh_for(part.shape))),
                MeshMaterial3d(materials.add(craft::finish_material(part.finish, seat, accent))),
                Transform {
                    translation: part.translation,
                    rotation: part.rotation,
                    scale: part.scale,
                },
            ));
        }
    });
}

/// The chassis a remote seat would have, derived the way the harness derives
/// every bot's: from its slot alone (`Archetype::for_slot`).
fn archetype_for_remote(entity: PersistId) -> Archetype {
    Archetype::for_slot(entity.0.saturating_sub(1))
}

fn controls(keys: &ButtonInput<KeyCode>, selected: Option<PersistId>) -> Controls {
    Controls {
        left: keys.pressed(KeyCode::ArrowLeft),
        right: keys.pressed(KeyCode::ArrowRight),
        thrust: keys.pressed(KeyCode::ArrowUp),
        fire: keys.pressed(KeyCode::Space),
        lock_target: selected,
    }
}

#[allow(clippy::too_many_arguments)]
fn drive_core(
    keys: Res<ButtonInput<KeyCode>>,
    mut session: ResMut<ActiveSession>,
    mut window: ResMut<MetricWindow>,
    mut sink: ResMut<JsonlTelemetry>,
    mut tracks: ResMut<ProjectileTracks>,
    mut broken: ResMut<LockBreak>,
    mut shots: ResMut<ShotFeedback>,
    mut selected: ResMut<SelectedLock>,
) {
    let controls = controls(&keys, selected.target);
    match &mut *session {
        ActiveSession::Local(local) => {
            let tick = local.tick;
            // One pipeline, one codec path: what the keyboard ships is what a
            // bot's pilot would have produced with these gates applied
            // (`human_full_controls_match_bot_order_bytes` pins it).
            let packet = local.human.human_packet(tick, controls);
            let mut human = local.pending.remove(&PLAYER).unwrap_or_default();
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
            let mut bot = local.pending.remove(&OPPONENT).unwrap_or_default();
            bot.extend(local.bot.bot_orders(tick));
            let mut delivered = BTreeMap::<PersistId, Vec<Order>>::new();
            let mut emitted = Vec::<Outcome>::new();
            for (entity, orders) in [(PLAYER, human), (OPPONENT, bot)] {
                let outcome = local
                    .executor
                    .step_entity(entity, tick, &orders)
                    .expect("both craft installed");
                for event in &outcome.events {
                    if let Some((target, input)) = local.executor.ruleset().deliver(event) {
                        delivered.entry(target).or_default().push(input);
                    }
                }
                emitted.extend(outcome.events.iter().cloned());
            }
            local.pending = delivered;
            local.tick = Tick::new(tick.0.saturating_add(1));
            observe_skin_effects(&emitted, PLAYER, &mut tracks, &mut broken, &mut shots);
            clear_refused_selection(&emitted, PLAYER, &mut selected);
        }
        ActiveSession::Campaign(runtime) => {
            // The joined tick: same pipeline inside `advance`, plus the
            // wire leg, replicated-state application, link measurement and
            // the accumulator feed.
            let report = runtime.advance(controls, &mut sink);
            window.intents = window.intents.saturating_add(report.intents as u64);
            if !report.events.is_empty() {
                observe_skin_effects(
                    &report.events,
                    runtime.entity(),
                    &mut tracks,
                    &mut broken,
                    &mut shots,
                );
                clear_refused_selection(&report.events, runtime.entity(), &mut selected);
            }
        }
    }
}

fn clear_refused_selection(events: &[Outcome], locker: PersistId, selected: &mut SelectedLock) {
    if events.iter().any(|event| {
        matches!(
            event,
            Outcome::LockRefused { locker: who, target }
                if *who == locker && Some(*target) == selected.target
        )
    }) {
        selected.target = None;
    }
}

/// The skin's per-tick event consumption, shared by both session kinds:
/// tracers, lock breakage and shot feedback read exactly what the ruleset
/// raised this tick.
fn observe_skin_effects(
    emitted: &[Outcome],
    observer: PersistId,
    tracks: &mut ProjectileTracks,
    broken: &mut LockBreak,
    shots: &mut ShotFeedback,
) {
    // The skin's only source of truth for a shot in the air. `observe` is a
    // copy, not a simulation: it keeps the events this tick produced and
    // discards everything else, so a resolved shot loses its tracer on the
    // same tick the ruleset resolves it.
    tracks.observe(emitted);
    broken.age();
    broken.observe(emitted, observer);
    // #383's two feedback layers, in event order: the provisional arrival
    // armed off this tick's last flight leg, then the target's authoritative
    // verdict — which arrives one delivery later and overrides the guess.
    shots.age();
    shots.arm_provisional(tracks, observer);
    shots.observe(emitted, observer);
}

/// Copies this tick's combat state out of the executor for the overlay.
fn read_combat_state(session: Res<ActiveSession>, mut view: ResMut<CombatView>) {
    *view = CombatView::read(session.executor(), session.local_entity());
}

fn archetype_of(executor: &Executor<Regolith>, entity: PersistId) -> Option<Archetype> {
    match executor.state(entity)? {
        RegolithState::Craft(craft) => Some(craft.archetype),
        _ => None,
    }
}

fn select_clicked_body(
    buttons: Res<ButtonInput<MouseButton>>,
    windows: Query<&Window, With<PrimaryWindow>>,
    camera: Query<(&Camera, &GlobalTransform), With<ChaseCamera>>,
    bodies: Query<(&CoreEntity, &GlobalTransform)>,
    session: Res<ActiveSession>,
    mut selected: ResMut<SelectedLock>,
) {
    if !buttons.just_pressed(MouseButton::Left) {
        return;
    }
    let Ok(window) = windows.single() else {
        return;
    };
    let Some(cursor) = window.cursor_position() else {
        return;
    };
    let Ok((camera, camera_transform)) = camera.single() else {
        return;
    };
    let candidates = bodies.iter().filter_map(|(entity, transform)| {
        let lockable = match session.executor().state(entity.0)? {
            RegolithState::Craft(craft) => craft.hull > 0,
            RegolithState::Rock(rock) => rock.hull > 0,
            RegolithState::Pickup(_) | RegolithState::BloomDirector(_) => false,
        };
        if !lockable || entity.0 == session.local_entity() {
            return None;
        }
        camera
            .world_to_viewport(camera_transform, transform.translation())
            .ok()
            .map(|screen| (entity.0, screen))
    });
    selected.target = nearest_clicked(cursor, candidates);
}

fn nearest_clicked(
    cursor: Vec2,
    candidates: impl Iterator<Item = (PersistId, Vec2)>,
) -> Option<PersistId> {
    const CLICK_RADIUS_PX: f32 = 32.0;
    candidates
        .filter_map(|(entity, screen)| {
            let distance = cursor.distance_squared(screen);
            (distance <= CLICK_RADIUS_PX * CLICK_RADIUS_PX).then_some((entity, distance))
        })
        .min_by(|left, right| left.1.total_cmp(&right.1))
        .map(|(entity, _)| entity)
}

fn ensure_rock_bodies(
    session: Res<ActiveSession>,
    existing: Query<(Entity, &CoreEntity), With<RockBody>>,
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
) {
    let mut rendered = BTreeSet::new();
    for (body, entity) in &existing {
        if matches!(
            session.executor().state(entity.0),
            Some(RegolithState::Rock(rock)) if rock.hull > 0
        ) {
            rendered.insert(entity.0);
        } else {
            commands.entity(body).despawn();
        }
    }
    for entity in session.executor().entities().copied() {
        let Some(RegolithState::Rock(rock)) = session.executor().state(entity) else {
            continue;
        };
        if rock.hull <= 0 || rendered.contains(&entity) {
            continue;
        }
        let radius_m = rock.tier.limits().radius_mm as f32 / 1_000.0;
        commands.spawn((
            CoreEntity(entity),
            RockBody,
            Mesh3d(meshes.add(Sphere::new(radius_m))),
            MeshMaterial3d(materials.add(StandardMaterial {
                base_color: hud::MINING_AMBER.with_alpha(0.72),
                metallic: 0.18,
                perceptual_roughness: 0.92,
                ..Default::default()
            })),
            Transform::default(),
        ));
    }
}

/// Spawns the remote duel body the moment a joined session learns which
/// craft to follow. Local sessions always know both seats at startup and
/// never trigger this.
/// Spawns the body for the seat the player drives, and retires craft bodies
/// whose entity the current session does not know.
///
/// `setup_scene` runs once at `Startup`, when the session is always
/// `ActiveSession::Local`, so the only craft body it can spawn carries
/// `CoreEntity(PLAYER)` — entity 1. Joining a campaign replaces the session:
/// the player's craft becomes the slot-derived id (`slot + 1`, `campaign.rs`),
/// and entity 1 is not in the campaign executor at all. `sync_rendered_state`
/// then finds no state for the one body in the scene and skips it, so the ship
/// never moves — while the HUD, which reads through `local_entity()`, shows
/// speed changing. That is exactly the shape the bug was reported in.
fn ensure_local_body(
    session: Res<ActiveSession>,
    asset_server: Res<AssetServer>,
    paths: Res<VisualAssetPaths>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    bodies: Query<(Entity, &CoreEntity)>,
    mut commands: Commands,
) {
    let local = session.local_entity();
    let mut have_local = false;
    for (body, core) in &bodies {
        if core.0 == local {
            have_local = true;
        } else if session.executor().state(core.0).is_none() {
            // A body the session no longer knows: the pre-join player craft
            // after a campaign join. Left in place it is a frozen ghost.
            commands.entity(body).despawn();
        }
    }
    if have_local || session.executor().state(local).is_none() {
        return;
    }
    spawn_craft_body(
        &mut commands,
        &asset_server,
        &paths,
        session.executor(),
        local,
        craft::Seat::Player,
        Transform::from_scale(Vec3::splat(craft::CRAFT_DISPLAY_SCALE)),
        &mut meshes,
        &mut materials,
    );
}

fn ensure_focus_body(
    session: Res<ActiveSession>,
    asset_server: Res<AssetServer>,
    paths: Res<VisualAssetPaths>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    bodies: Query<(Entity, &CoreEntity)>,
    mut commands: Commands,
) {
    let Some(focus) = session.focus_entity() else {
        return;
    };
    if bodies.iter().any(|(_, core)| core.0 == focus) {
        return;
    }
    spawn_craft_body(
        &mut commands,
        &asset_server,
        &paths,
        session.executor(),
        focus,
        craft::Seat::Bot,
        Transform::from_scale(Vec3::splat(craft::CRAFT_DISPLAY_SCALE)),
        &mut meshes,
        &mut materials,
    );
}

/// Rebuilds a speculative craft body once replicated state names a different
/// archetype.
///
/// A joined session can create the focus body before that craft's state has
/// arrived, so initial composition has to use `Archetype::for_slot`.  We keep
/// that immediate visual feedback, accepting possible visual flicker during
/// replacement when the authoritative state disagrees. Replacing the whole root
/// deliberately rebuilds every archetype-derived child together: hull, firing
/// arcs, and any future visual keyed on the same reading.
#[allow(clippy::too_many_arguments)]
fn recompose_craft_bodies(
    session: Res<ActiveSession>,
    asset_server: Res<AssetServer>,
    paths: Res<VisualAssetPaths>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    bodies: Query<(Entity, &CoreEntity, &CraftBodyComposition, &Transform)>,
    mut commands: Commands,
) {
    for (body, core, composition, transform) in &bodies {
        let Some(archetype) = archetype_of(session.executor(), core.0) else {
            continue;
        };
        if archetype == composition.archetype {
            continue;
        }
        commands.entity(body).despawn();
        spawn_craft_body(
            &mut commands,
            &asset_server,
            &paths,
            session.executor(),
            core.0,
            composition.seat,
            *transform,
            &mut meshes,
            &mut materials,
        );
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
    session: Res<ActiveSession>,
    mut rendered: Query<(&CoreEntity, &mut Transform)>,
) {
    for (entity, mut transform) in &mut rendered {
        let Some(state) = session.executor().state(entity.0) else {
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
    session: Res<ActiveSession>,
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
        // The joined-session line: state machine position and the counters
        // behind every measured number above.
        if let ActiveSession::Campaign(runtime) = &*session {
            let accumulator = runtime.accumulator();
            text.push_str(&format!(
                "\n{} | session {}\nuplink shed {} | downlink undecodable {} | afk capped {}",
                runtime.summary_line(),
                accumulator.session_id(),
                runtime.uplink_shed(),
                runtime.undecodable(),
                accumulator.progress().afk_capped,
            ));
        } else {
            text.push_str("\noffline local session — not a campaign path, banks nothing");
        }
    }
}

fn stream_metrics(
    mut metrics: ResMut<OverlayMetrics>,
    mut window: ResMut<MetricWindow>,
    mut sink: ResMut<JsonlTelemetry>,
    session: Res<ActiveSession>,
) {
    metrics.intents_per_second = std::mem::take(&mut window.intents);
    match &*session {
        ActiveSession::Local(_) => {
            metrics.idle_minutes = window.idle_ticks as f64 / (orrery_core::TICK_HZ as f64 * 60.0);
        }
        ActiveSession::Campaign(runtime) => {
            // Every number below is a measurement of the live link or the
            // live accumulator — never a configuration echo. Loss comes from
            // Dropped uplink acks (#393) plus downlink replication-tick gaps;
            // jitter from arrival-interval deviations; banked/idle minutes
            // from the same ticks `observe_tick` accounted.
            metrics.observed_loss_pct = runtime.observed_loss_pct();
            metrics.observed_jitter_p50_ms = runtime.observed_jitter_p50_ms();
            metrics.observed_jitter_p99_ms = runtime.observed_jitter_p99_ms();
            metrics.cell_id = runtime.latest_cell().map(CellId::to_bits);
            let progress = runtime.accumulator().progress();
            metrics.banked_minutes = progress.banked_minutes;
            metrics.idle_minutes = progress.idle_minutes;
            let configured = &runtime.config().configured;
            metrics.configured_loss_pct = configured.loss_pct;
            metrics.configured_jitter_ms = configured.jitter_p50_ms;
        }
    }
    if let Err(error) = sink.append(&metrics) {
        error!("cannot append Regolith telemetry: {error}");
    }
}

/// Writes the finished banking row once, on app exit.
///
/// The row is produced by the joined-session accumulator only; an offline
/// [`ActiveSession::Local`] writes nothing because it measured nothing.
fn write_campaign_record_on_exit(
    mut exited: MessageReader<AppExit>,
    mut session: ResMut<ActiveSession>,
    metrics: Res<OverlayMetrics>,
    upload: Option<Res<admission::UploadManager>>,
) {
    for exit in exited.read() {
        let ActiveSession::Campaign(runtime) = &mut *session else {
            return;
        };
        let Some(record) = runtime.shutdown() else {
            return;
        };
        let path: &Path = metrics.session_record_path.as_path();
        let record_path = path
            .parent()
            .unwrap_or(Path::new("."))
            .join("campaign-records.jsonl");
        let write = || -> std::io::Result<()> {
            if let Some(parent) = record_path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&record_path)?;
            let mut writer = std::io::BufWriter::new(file);
            crate::session::CampaignSession::write_record(&mut writer, &record)?;
            use std::io::Write as _;
            writer.flush()?;
            writer.get_ref().sync_all()
        };
        match write() {
            Ok(()) => {
                info!(
                    "campaign session {} recorded to {} ({} banked min)",
                    record.session_id,
                    record_path.display(),
                    record.banked_minutes
                );
                if let Some(upload) = &upload {
                    admission::upload_finished_session(
                        upload,
                        &record,
                        &record_path,
                        &metrics.session_record_path,
                    );
                }
            }
            Err(error) => error!(
                "cannot write campaign record {}: {error}; upload not attempted so local evidence remains authoritative",
                record_path.display()
            ),
        }
        let _ = exit;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bevy::asset::AssetPlugin;
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

    /// #445's arcs, checked against a ship whose heading comes from the
    /// ruleset rather than from this test.
    ///
    /// The trap is #377's, one level up: `heading_rotation` is right, and an
    /// arc could still be drawn on the wrong axis. So this spawns the real
    /// duel, reads each craft's own `yaw_urad` out of its hashed state, and
    /// requires every arc's world centreline to be the ruleset's own
    /// `(cos(yaw + bearing), 0, sin(yaw + bearing))` — the same expression
    /// `step_craft` thrusts along, evaluated at the arc's bearing.
    ///
    /// It also requires the whole fan to stay in the deck plane after the
    /// world rotation, which is the assertion a `+Y` cone would fail.
    #[test]
    fn firing_arcs_sit_on_the_rulesets_bearings_for_a_known_heading() {
        use bevy::render::mesh::VertexAttributeValues;

        let game = Regolith::honest();
        let mut executor = Executor::new(game, SEED);
        executor.insert(PLAYER, game.spawn(PLAYER, 0));
        executor.insert(OPPONENT, game.spawn(OPPONENT, 1));

        let mut checked = 0usize;
        for entity in [PLAYER, OPPONENT] {
            let RegolithState::Craft(craft) = executor.state(entity).expect("installed") else {
                panic!("both seats are craft");
            };
            // The heading is the ruleset's, not a constant this test chose.
            let yaw_urad = craft.yaw_urad;
            let rotation = heading_rotation(yaw_urad);
            for arc in craft::firing_arcs(craft.archetype) {
                let bearing = (yaw_urad as f64 + f64::from(arc.centre_urad)) / 1_000_000.0;
                let expected = Vec3::new(bearing.cos() as f32, 0.0, bearing.sin() as f32);
                let drawn = rotation * craft::arc_centre_direction(*arc);
                assert!(
                    drawn.distance(expected) < 1e-5,
                    "{} on {:?} at yaw {yaw_urad}: the arc points {drawn}, \
                     the ruleset's bearing is {expected}",
                    arc.name,
                    craft.archetype
                );

                // And the fan itself, once rotated into the world.
                let mesh = craft::arc_mesh(*arc, 30.0);
                let VertexAttributeValues::Float32x3(points) = mesh
                    .attribute(Mesh::ATTRIBUTE_POSITION)
                    .expect("the fan has positions")
                else {
                    panic!("arc positions are Float32x3");
                };
                for point in points {
                    let world = rotation * Vec3::new(point[0], point[1], point[2]);
                    assert!(
                        world.y.abs() < 1e-4,
                        "{}: world vertex {world} left the deck plane",
                        arc.name
                    );
                }
                checked += 1;
            }
        }
        assert_eq!(
            checked, 3,
            "one interceptor front arc and two cruiser side arcs were expected"
        );
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

    /// Replication can fill a remote state only after its body was made from
    /// the slot fallback.  Seed the already-composed body explicitly instead
    /// of relying on today's slot table: this remains a regression test even
    /// if a future table happens to guess this fixture's slot correctly.
    ///
    /// Deleting `recompose_craft_bodies` leaves the cruiser marker and its two
    /// side arcs in place, so this named test fails rather than passing on a
    /// convenient slot-number coincidence.
    #[test]
    fn authoritative_archetype_recomposes_a_speculative_remote_body() {
        let remote = PersistId::new(99);
        let game = Regolith::honest();
        let mut local = LocalSession::default();
        let RegolithState::Craft(mut craft) = game.spawn(remote, 0) else {
            panic!("the remote state is a craft");
        };
        craft.archetype = Archetype::Interceptor;
        local.executor.insert(remote, RegolithState::Craft(craft));

        let mut app = App::new();
        app.add_plugins(AssetPlugin::default())
            .init_resource::<VisualAssetPaths>()
            .init_resource::<Assets<Mesh>>()
            .init_resource::<Assets<StandardMaterial>>()
            .insert_resource(ActiveSession::Local(Box::new(local)))
            .add_systems(Update, recompose_craft_bodies);
        let speculative_body = app
            .world_mut()
            .spawn((
                CoreEntity(remote),
                CraftBodyComposition {
                    archetype: Archetype::Cruiser,
                    seat: craft::Seat::Bot,
                },
                Transform::from_scale(Vec3::splat(craft::CRAFT_DISPLAY_SCALE)),
                Visibility::Inherited,
            ))
            .id();
        for _ in 0..2 {
            app.world_mut()
                .spawn((FiringArcFan(remote), ChildOf(speculative_body)));
        }

        app.update();

        let mut bodies = app
            .world_mut()
            .query::<(&CoreEntity, &CraftBodyComposition)>();
        let compositions: Vec<_> = bodies
            .iter(app.world())
            .filter(|(core, _)| core.0 == remote)
            .map(|(_, composition)| composition.archetype)
            .collect();
        assert_eq!(compositions, vec![Archetype::Interceptor]);

        let mut arcs = app.world_mut().query::<&FiringArcFan>();
        assert_eq!(
            arcs.iter(app.world()).filter(|arc| arc.0 == remote).count(),
            1,
            "the interceptor's one front arc must replace the cruiser's two side arcs"
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
            craft.lock_class = Some(orrery_games::regolith::state::LockClass::Ship);
            craft.lock_progress = progress;
            craft.locks_acquired = 4;
            executor.insert(PLAYER, RegolithState::Craft(craft));
            executor.insert(OPPONENT, game.spawn(OPPONENT, 1));
            let view = CombatView::read(&executor, PLAYER);
            assert_eq!(view.lock.progress, progress);
            assert_eq!(view.lock.target, Some(OPPONENT));
            assert_eq!(
                view.lock.class,
                Some(orrery_games::regolith::state::LockClass::Ship)
            );
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

    #[test]
    fn clicking_selects_the_nearest_lockable_body_inside_the_pick_radius() {
        let chosen = nearest_clicked(
            Vec2::new(100.0, 100.0),
            [
                (PersistId::new(2), Vec2::new(125.0, 100.0)),
                (PersistId::new(3), Vec2::new(104.0, 103.0)),
                (PersistId::new(4), Vec2::new(200.0, 200.0)),
            ]
            .into_iter(),
        );
        assert_eq!(chosen, Some(PersistId::new(3)));
        assert_eq!(
            nearest_clicked(
                Vec2::ZERO,
                [(PersistId::new(2), Vec2::splat(40.0))].into_iter()
            ),
            None
        );
    }
}
