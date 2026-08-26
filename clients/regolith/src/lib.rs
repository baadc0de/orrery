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
pub mod roster;
pub mod session;
pub mod starfield;
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
use campaign::{DeliveredOrder, JoinState};
use combat::{CombatView, LockBreak, ProjectileTracks, ShotFeedback};
use intent::{decode_packet, Controls, IntentPipeline};
use orrery_core::{Executor, TICK_NANOS};
use orrery_games::{
    regolith::archetype::Archetype,
    regolith::order::{Order, Outcome},
    regolith::state::{RegolithState, RockTier},
    Game, Regolith,
};
use orrery_protocol::{CellId, PersistId, Tick, UniverseSeed};
use telemetry::{JsonlTelemetry, OverlayMetrics, SessionScope};

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
struct SessionBanner;
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

/// Enables the live geometry capture used to compare rendered and adjudicated shots.
///
/// This is an operator diagnostic: ordinary clients never insert the resource.
#[derive(Debug, Resource)]
pub struct GeometryCapture {
    auto_drive: bool,
}

impl GeometryCapture {
    /// Capture geometry and continuously select, turn, and fire at the focus craft.
    #[must_use]
    pub const fn auto_drive() -> Self {
        Self { auto_drive: true }
    }
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

    fn join_state(&self) -> Option<&JoinState> {
        match self {
            Self::Local(_) => None,
            Self::Campaign(runtime) => Some(runtime.state()),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SessionPresentation {
    Local,
    Dialing,
    Live,
    Failed,
    Refused,
    Disconnected,
}

impl SessionPresentation {
    fn from_join_state(state: Option<&JoinState>) -> Self {
        match state {
            None => Self::Local,
            Some(JoinState::Dialing) => Self::Dialing,
            Some(JoinState::Joined) => Self::Live,
            Some(JoinState::Failed(_)) => Self::Failed,
            Some(JoinState::Refused(_)) => Self::Refused,
            Some(JoinState::Closed { .. }) => Self::Disconnected,
        }
    }

    const fn session_scope(self) -> SessionScope {
        match self {
            Self::Live => SessionScope::Campaign,
            Self::Local | Self::Dialing | Self::Failed | Self::Refused | Self::Disconnected => {
                SessionScope::Local
            }
        }
    }

    const fn label(self) -> &'static str {
        match self {
            Self::Local => "LOCAL SANDBOX - NOT CONNECTED TO CAMPAIGN",
            Self::Dialing => "LOCAL SANDBOX - CONNECTING TO CAMPAIGN...",
            Self::Live => "CAMPAIGN LIVE",
            Self::Failed => "LOCAL SANDBOX - CAMPAIGN DIAL FAILED",
            Self::Refused => "LOCAL SANDBOX - CAMPAIGN JOIN REFUSED",
            Self::Disconnected => "LOCAL SANDBOX - CAMPAIGN DISCONNECTED",
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
            .init_resource::<CameraZoom>()
            .init_resource::<roster::ShipRoster>()
            .init_resource::<roster::RosterTask>()
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
                    follow_camera.after(sync_rendered_state),
                    starfield::sync_starfield.after(follow_camera),
                    sync_ship_labels.after(follow_camera),
                    zoom_camera.before(follow_camera),
                    refresh_session_banner,
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
            .add_systems(Update, capture_tracer_geometry.after(hud::sync_tracers))
            .add_systems(Update, write_campaign_record_on_exit)
            .add_systems(
                Update,
                stream_metrics.run_if(on_timer(Duration::from_secs(1))),
            )
            .add_systems(
                Update,
                roster::refresh_roster.run_if(on_timer(roster::ROSTER_REFRESH)),
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
        Projection::from(chase_camera_projection()),
        ChaseCamera,
        chase_camera_transform(Vec3::ZERO, CAMERA_DEFAULT_HEIGHT_M),
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
    starfield::spawn_starfield(&mut commands, &mut meshes, &mut materials);
    let presentation = SessionPresentation::from_join_state(session.join_state());
    commands.spawn((
        Node {
            position_type: PositionType::Absolute,
            top: Val::Px(8.0),
            width: Val::Percent(100.0),
            justify_content: JustifyContent::Center,
            ..Default::default()
        },
        GlobalZIndex(100),
        children![(
            Text::new(presentation.label()),
            TextFont::from_font_size(14.0),
            TextColor(Color::WHITE),
            Node {
                padding: UiRect::axes(Val::Px(14.0), Val::Px(7.0)),
                border_radius: BorderRadius::all(Val::Px(8.0)),
                ..Default::default()
            },
            BackgroundColor(session_banner_color(presentation)),
            SessionBanner,
        )],
    ));
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
    geometry_capture: Option<Res<GeometryCapture>>,
) {
    if geometry_capture
        .as_ref()
        .is_some_and(|capture| capture.auto_drive)
    {
        if let ActiveSession::Campaign(runtime) = &*session {
            selected.target = runtime.focus();
        }
    }
    let mut controls = controls(&keys, selected.target);
    if geometry_capture
        .as_ref()
        .is_some_and(|capture| capture.auto_drive)
    {
        controls.right = true;
        controls.fire = true;
    }
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
            if let Err(error) = sink.append_orders(&packet, SessionScope::Local) {
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
            observe_skin_effects(
                &emitted,
                &[],
                PLAYER,
                &[],
                &mut tracks,
                &mut broken,
                &mut shots,
            );
            clear_refused_selection(&emitted, &[], PLAYER, &mut selected);
        }
        ActiveSession::Campaign(runtime) => {
            // The joined tick: same pipeline inside `advance`, plus the
            // wire leg, replicated-state application, link measurement and
            // the accumulator feed.
            let report = runtime.advance(controls, &mut sink);
            let presentation_targets: Vec<_> = report
                .events
                .iter()
                .filter_map(|event| match event {
                    Outcome::DamageDealt { target, .. } => {
                        match runtime.executor().state(*target) {
                            Some(RegolithState::Craft(craft)) => Some((*target, craft.pos)),
                            _ => None,
                        }
                    }
                    _ => None,
                })
                .collect();
            if geometry_capture.is_some() {
                capture_rendered_geometry(runtime);
                capture_client_geometry(runtime, &report.events);
            }
            window.intents = window.intents.saturating_add(report.intents as u64);
            observe_skin_effects(
                &report.events,
                &report.delivered,
                runtime.entity(),
                &presentation_targets,
                &mut tracks,
                &mut broken,
                &mut shots,
            );
            clear_refused_selection(
                &report.events,
                &report.delivered,
                runtime.entity(),
                &mut selected,
            );
        }
    }
}

fn capture_rendered_geometry(runtime: &campaign::CampaignRuntime) {
    use orrery_games::regolith::{distance_mm, firing_arc_measurement};

    let view = CombatView::read(runtime.executor(), runtime.entity());
    let (Some(attacker), Some(target)) = (view.own, view.target) else {
        return;
    };
    let measurement = firing_arc_measurement(
        attacker.archetype,
        attacker.yaw_urad,
        attacker.pos,
        target.pos,
    );
    let distance_mm = distance_mm(attacker.pos, target.pos);
    eprintln!(
        "geometry_capture side=client_render tick={} attacker={} target={} attacker_pos={:?} \
         target_pos={:?} attacker_yaw_urad={} archetype={:?} world_bearing_urad={:?} \
         relative_urad={:?} inside={} distance_mm={}",
        runtime.joined_ticks().saturating_sub(1),
        attacker.entity.0,
        target.entity.0,
        attacker.pos,
        target.pos,
        attacker.yaw_urad,
        attacker.archetype,
        measurement.world_bearing_urad,
        measurement.relative_urad,
        measurement.inside,
        distance_mm,
    );
}

fn capture_client_geometry(runtime: &campaign::CampaignRuntime, events: &[Outcome]) {
    use orrery_games::regolith::{distance_mm, firing_arc_measurement};

    for event in events {
        let Outcome::DamageDealt {
            attacker,
            target,
            attacker_pos,
            attacker_yaw_urad,
            attacker_archetype,
            attacker_weapon,
            flight_ticks: None,
            ..
        } = event
        else {
            continue;
        };
        let Some(RegolithState::Craft(rendered_attacker)) = runtime.executor().state(*attacker)
        else {
            continue;
        };
        let Some(RegolithState::Craft(rendered_target)) = runtime.executor().state(*target) else {
            continue;
        };
        let measurement = firing_arc_measurement(
            *attacker_archetype,
            *attacker_yaw_urad,
            *attacker_pos,
            rendered_target.pos,
        );
        let range_sq = rendered_target.pos.distance_squared(*attacker_pos);
        let distance_mm = distance_mm(rendered_target.pos, *attacker_pos);
        let reach_mm = attacker_weapon
            .weapon()
            .optimal_mm
            .saturating_add(attacker_weapon.weapon().falloff_mm)
            .saturating_add(rendered_target.archetype.limits().radius_mm);
        eprintln!(
            "geometry_capture side=client tick={} attacker={} target={} shot_pos={:?} \
             rendered_attacker_pos={:?} rendered_target_pos={:?} shot_yaw_urad={} \
             rendered_yaw_urad={} archetype={:?} world_bearing_urad={:?} \
             relative_urad={:?} inside={} distance_mm={} distance_sq_mm2={} reach_mm={}",
            runtime.joined_ticks().saturating_sub(1),
            attacker.0,
            target.0,
            attacker_pos,
            rendered_attacker.pos,
            rendered_target.pos,
            attacker_yaw_urad,
            rendered_attacker.yaw_urad,
            attacker_archetype,
            measurement.world_bearing_urad,
            measurement.relative_urad,
            measurement.inside,
            distance_mm,
            range_sq,
            reach_mm,
        );
    }
}

fn capture_tracer_geometry(
    capture: Option<Res<GeometryCapture>>,
    session: Res<ActiveSession>,
    tracks: Res<ProjectileTracks>,
    tracers: Query<(&hud::Tracer, &Transform, &Visibility)>,
) {
    if capture.is_none() {
        return;
    }
    let ActiveSession::Campaign(runtime) = &*session else {
        return;
    };
    for (tracer, transform, visibility) in &tracers {
        let Some(track) = tracks.tracks().get(tracer.0) else {
            continue;
        };
        if track.presented && track.travelled() > 0.0 && *visibility == Visibility::Inherited {
            eprintln!(
                "tracer_capture tick={} slot={} attacker={} target={} travelled={:.3} centre={:?} scale_x={:.3} visible=true",
                runtime.joined_ticks().saturating_sub(1),
                tracer.0,
                track.attacker.0,
                track.target.0,
                track.travelled(),
                transform.translation,
                transform.scale.x,
            );
        }
    }
}

fn clear_refused_selection(
    events: &[Outcome],
    delivered: &[DeliveredOrder],
    locker: PersistId,
    selected: &mut SelectedLock,
) {
    let local_refused = events.iter().any(|event| {
        matches!(
            event,
            Outcome::LockRefused { locker: who, target }
                if *who == locker && Some(*target) == selected.target
        )
    });
    let delivered_refused = delivered.iter().any(|input| {
        matches!(
            input.feedback_outcome(),
            Some(Outcome::LockRefused { locker: who, target })
                if who == locker && Some(target) == selected.target
        )
    });
    if local_refused || delivered_refused {
        selected.target = None;
    }
}

/// Copies this tick's authoritative combat statements into the skin.
///
/// `emitted` contains outcomes raised by the local step. `delivered` contains
/// accepted outcomes from another authority after their canonical conversion
/// to orders. This is a copy, not a simulation: the function keeps what those
/// two ruleset channels said this tick and derives nothing from authored
/// `Fire`, lock intent, or elapsed skin time.
pub fn observe_skin_effects(
    emitted: &[Outcome],
    delivered: &[DeliveredOrder],
    observer: PersistId,
    presentation_targets: &[(PersistId, orrery_core::QPos)],
    tracks: &mut ProjectileTracks,
    broken: &mut LockBreak,
    shots: &mut ShotFeedback,
) {
    let delivered_feedback: Vec<_> = delivered
        .iter()
        .filter_map(DeliveredOrder::feedback_outcome)
        .collect();
    // The skin's only source of truth for a shot in the air. `observe` is a
    // copy, not a simulation: it keeps the events this tick produced and
    // discards everything else, so a resolved shot loses its tracer on the
    // same tick the ruleset resolves it.
    tracks.observe_campaign(emitted, observer, presentation_targets);
    tracks.retire(emitted, observer);
    tracks.retire(&delivered_feedback, observer);
    broken.age();
    broken.observe(emitted, observer);
    broken.observe(&delivered_feedback, observer);
    // #383's two feedback layers, in event order: the provisional arrival
    // armed off this tick's last flight leg, then the target's authoritative
    // verdict — which arrives one delivery later and overrides the guess.
    shots.age();
    shots.arm_provisional(tracks, observer);
    shots.observe(emitted, observer);
    shots.observe(&delivered_feedback, observer);
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

/// The finish one rock tier wears: base tint and how faceted its shell is.
///
/// **Presentation only.** Size comes from the ruleset's own
/// `RockTier::limits().radius_mm`, which is the same number collision and
/// tracking read, so the drawn body is the ruleset's rock rather than a
/// decorative stand-in. Everything else here is a look, and drawing a rock
/// claims nothing whatsoever about mining, ownership or hull (#519) — a rock
/// the player can see is a rock the player may or may not be allowed to touch,
/// and only the ruleset says which.
///
/// ## Why the ramp runs the way it does
///
/// The tiers are 40 m, 20 m and 8 m in radius, so size alone separates them —
/// but a small rock is also the one that is hardest to notice, and the camera
/// now reaches 4 km (#521). So lightness runs *against* size: the small tier
/// is the brightest and the large tier the darkest, which keeps every tier
/// legible instead of making the already-faint one fainter. Facet count runs
/// *with* size, because a 40 m body has the screen area to show facets and an
/// 8 m one reads better as a single chunk.
///
/// The tints stay on a warm neutral ramp rather than taking
/// [`hud::MINING_AMBER`]: that amber is the mining *lock* colour, and a rock
/// wearing it unlocked would say the player has a mining lock they do not
/// have.
const fn rock_finish(tier: RockTier) -> (Color, u32) {
    match tier {
        RockTier::Large => (Color::srgb(0.40, 0.37, 0.34), 2),
        RockTier::Medium => (Color::srgb(0.55, 0.51, 0.46), 1),
        RockTier::Small => (Color::srgb(0.72, 0.67, 0.60), 0),
    }
}

/// A rock's resting attitude, derived from its own id.
///
/// Every rock body is an icosphere, so without this they all face the same way
/// and a field of them reads as a shop display. The id is the only stable
/// per-rock number the skin has, and turning it into a rotation is pure
/// presentation: nothing downstream reads a rock's drawn rotation, and
/// `sync_rendered_state` deliberately leaves non-craft rotations alone.
fn rock_attitude(entity: PersistId) -> Quat {
    let bits = entity.0.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    let angle = |shift: u32| {
        let slice = ((bits >> shift) & 0xFFFF) as f32 / 65_536.0;
        slice * std::f32::consts::TAU
    };
    Quat::from_euler(EulerRot::XYZ, angle(0), angle(16), angle(32))
}

/// Spawns and retires the rendered body for every rock the session knows.
///
/// #524: rocks were fully simulated — lockable, mineable, collidable, named in
/// the HUD as `LARGE ROCK` and friends — and nothing put one on screen, so the
/// HUD talked about objects the player could not see.
///
/// Rocks are replicated remote state and follow the same replica lifecycle as
/// craft. This system owns no lifetime of its own: a body exists exactly while
/// `executor().state(entity)` still yields a live `Rock`, so the staleness
/// expiry (#505) removing a replica removes the body on the same frame, and a
/// rock that leaves the interest set stops being drawn rather than freezing.
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
        let (tint, facets) = rock_finish(rock.tier);
        // `ico` only fails on a subdivision count far past anything here; the
        // uv sphere is a shape fallback, never a silent absence of a body.
        let mesh = Sphere::new(radius_m)
            .mesh()
            .ico(facets)
            .unwrap_or_else(|_| Sphere::new(radius_m).mesh().uv(12, 8));
        commands.spawn((
            CoreEntity(entity),
            RockBody,
            Mesh3d(meshes.add(mesh)),
            MeshMaterial3d(materials.add(StandardMaterial {
                base_color: tint,
                metallic: 0.05,
                perceptual_roughness: 0.95,
                ..Default::default()
            })),
            Transform::from_rotation(rock_attitude(entity)),
        ));
    }
}

/// `rocks 3 L / 5 M / 2 S in state | 10 drawn`, for the F3 pane.
///
/// This exists because #524's failure mode was invisible to the test suite:
/// rocks were fully simulated and simply never drawn, and no assertion about
/// state could tell the difference. `drawn` is counted from the `RockBody`
/// entities themselves, so the two halves of the line come from different
/// places and disagreeing is the signal.
#[must_use]
fn rock_census(counts: [usize; 3], drawn: usize) -> String {
    format!(
        "rocks {} L / {} M / {} S in state | {} drawn",
        counts[0], counts[1], counts[2], drawn
    )
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

/// Marks the one camera the follow system drives.
///
/// Public only so `starfield` can name it in a query filter; nothing outside
/// the skin has any reason to spawn one.
#[derive(Component)]
pub struct ChaseCamera;

/// The vertical field of view the chase camera is built with, in radians.
///
/// Spelled out rather than left to Bevy's default so the arithmetic in
/// [`CameraZoom`]'s documentation is a property of this file rather than of
/// whatever `PerspectiveProjection::default()` happens to be this release.
pub const CAMERA_FOV_Y: f32 = std::f32::consts::FRAC_PI_4;

/// The chase camera's far clip plane, in metres.
///
/// **This is not a tuning knob, it is a bug fix.** Bevy's default perspective
/// projection clips at 1000 m. The camera looks straight down from
/// `CameraZoom`'s height, so at any height past 1000 m the *deck plane itself*
/// is behind the far plane and the entire world stops being drawn — and the
/// old autozoom reached that height whenever the bodies it framed were more
/// than ~385 m apart, which is well inside one weapon envelope. Anything
/// beyond the plane simply vanished, with no error and nothing a test that
/// asserts on state could see.
///
/// The value has to clear [`CAMERA_MAX_HEIGHT_M`] plus the depth of the
/// deepest starfield layer plus that layer's own extent, and there is no cost
/// to headroom here: this is a reversed-Z depth buffer, whose precision is
/// governed by `near`, not by `far`.
pub const CAMERA_FAR_M: f32 = 120_000.0;

/// The chase camera's near clip plane, in metres. Well inside the closest the
/// camera is allowed to fly, and the number that actually governs depth
/// precision.
pub const CAMERA_NEAR_M: f32 = 1.0;

/// The projection the chase camera is built with.
#[must_use]
pub fn chase_camera_projection() -> PerspectiveProjection {
    PerspectiveProjection {
        fov: CAMERA_FOV_Y,
        near: CAMERA_NEAR_M,
        far: CAMERA_FAR_M,
        ..Default::default()
    }
}

/// Half the world height a top-down camera at `height_m` can see, in metres.
///
/// `tan(fov/2) * height` — the camera looks straight down, so the deck plane
/// sits exactly `height_m` away along the view axis.
#[must_use]
pub fn visible_half_height_m(height_m: f32) -> f32 {
    (CAMERA_FOV_Y / 2.0).tan() * height_m
}

/// How high the chase camera flies, in metres above the deck plane.
///
/// **Presentation only.** Nothing downstream of this resource reaches the
/// intent pipeline, the executor or any range/arc arithmetic: the ruleset's
/// numbers are in millimetres of lattice and are computed from replicated
/// state, never from a transform. Zooming changes what the player can see and
/// nothing about what the player can do (#519).
///
/// ## Why these limits
///
/// The reference length is the weapon envelope the rings draw: optimal is
/// 300 m with falloff past it, and a chassis hull is ~7 m (`craft::hull_length`
/// times [`craft::CRAFT_DISPLAY_SCALE`], so ~22 m on screen). With a
/// [`CAMERA_FOV_Y`] of 45 degrees the visible half-height is `0.414 * height`:
///
/// * [`CAMERA_MIN_HEIGHT_M`] = 150 m shows +/- 62 m — a 22 m hull is about a
///   sixth of the screen height, close enough to read facing and the arc
///   marking, and the closest useful framing before the ship fills the view.
/// * [`CAMERA_DEFAULT_HEIGHT_M`] = 900 m shows +/- 373 m, so the 300 m optimal
///   ring fits with margin. This is the framing the weapon is fought at.
/// * [`CAMERA_MAX_HEIGHT_M`] = 4000 m shows +/- 1657 m, enough to hold the
///   ~2.5 km campaign crowd orbit in view. Past that a 22 m hull is under two
///   pixels on a 1080-line window and the view stops being usable.
///
/// [`CAMERA_ZOOM_STEP`] is multiplicative rather than additive because a fixed
/// metre step is a huge jump at the near end and imperceptible at the far end.
/// At 1.15 per wheel notch the whole 150..4000 range is 24 notches, which is a
/// comfortable flick of a wheel rather than a grind.
#[derive(Debug, Clone, Copy, Resource, PartialEq)]
pub struct CameraZoom {
    height_m: f32,
}

/// The closest the chase camera is allowed to fly. See [`CameraZoom`].
pub const CAMERA_MIN_HEIGHT_M: f32 = 150.0;
/// The furthest the chase camera is allowed to fly. See [`CameraZoom`].
pub const CAMERA_MAX_HEIGHT_M: f32 = 4_000.0;
/// Where the chase camera starts. See [`CameraZoom`].
pub const CAMERA_DEFAULT_HEIGHT_M: f32 = 900.0;
/// Multiplicative zoom per wheel notch. See [`CameraZoom`].
pub const CAMERA_ZOOM_STEP: f32 = 1.15;
/// Pixels of a pixel-unit scroll event that count as one notch.
///
/// Trackpads and some mice report `MouseScrollUnit::Pixel`; 50 px per notch is
/// the conventional line height those devices are calibrated against.
pub const CAMERA_ZOOM_PIXELS_PER_NOTCH: f32 = 50.0;

impl Default for CameraZoom {
    fn default() -> Self {
        Self {
            height_m: CAMERA_DEFAULT_HEIGHT_M,
        }
    }
}

impl CameraZoom {
    /// The current camera height above the deck, in metres.
    #[must_use]
    pub fn height_m(self) -> f32 {
        self.height_m
    }

    /// The zoom after `notches` of wheel travel, clamped to the limits.
    ///
    /// Positive notches (wheel away from the player) zoom **in**, which is the
    /// convention every map and every other game uses.
    #[must_use]
    pub fn zoomed(self, notches: f32) -> Self {
        if !notches.is_finite() || notches == 0.0 {
            return self;
        }
        let height = self.height_m * CAMERA_ZOOM_STEP.powf(-notches);
        Self {
            height_m: height.clamp(CAMERA_MIN_HEIGHT_M, CAMERA_MAX_HEIGHT_M),
        }
    }

    /// How much a screen-anchored HUD glyph must be scaled to keep its
    /// apparent size at this zoom.
    ///
    /// World measurements — the range rings, the arc marking, the tracers —
    /// must **not** use this: they mean metres and have to shrink with
    /// everything else or they would lie about distance. The lock reticle is
    /// the one overlay that is a glyph rather than a measurement, so it is the
    /// one thing that holds its apparent size.
    #[must_use]
    pub fn glyph_scale(self) -> f32 {
        self.height_m / CAMERA_DEFAULT_HEIGHT_M
    }
}

/// Turns wheel events into zoom. Presentation only; see [`CameraZoom`].
fn zoom_camera(
    mut wheel: MessageReader<bevy::input::mouse::MouseWheel>,
    mut zoom: ResMut<CameraZoom>,
) {
    use bevy::input::mouse::MouseScrollUnit;
    let mut notches = 0.0f32;
    for event in wheel.read() {
        notches += match event.unit {
            MouseScrollUnit::Line => event.y,
            MouseScrollUnit::Pixel => event.y / CAMERA_ZOOM_PIXELS_PER_NOTCH,
        };
    }
    let next = zoom.zoomed(notches);
    if next != *zoom {
        *zoom = next;
    }
}

/// Where the chase camera sits and what it looks at, for a given centre.
///
/// A free function so the framing rule can be asserted without standing up a
/// window: the camera hangs straight above `centre` at `height_m` and looks
/// down, with `-Z` up the screen so world `+X` runs right.
#[must_use]
pub fn chase_camera_transform(centre: Vec3, height_m: f32) -> Transform {
    let eye = Vec3::new(centre.x, centre.y + height_m, centre.z);
    Transform::from_translation(eye).looking_at(centre, Vec3::NEG_Z)
}

/// Keeps the player's own craft in the centre of the screen, at the height the
/// player chose.
///
/// The previous behaviour framed *every* body — centre on their midpoint and
/// rise until the furthest one fit. In live play that meant the world scale
/// changed while the player was manoeuvring: the ship appeared to drift as the
/// midpoint moved, and distances stopped being readable as a contact entered
/// or left the set. #521 replaced it with a fixed centre and a player-owned
/// zoom, because a predictable view is worth more than optimal framing when
/// you are holding an arc on a target.
///
/// Following only. It reads rendered `Transform`s, which are already a pure
/// function of core state, and writes nothing back — constraint 3 forbids the
/// skin deciding anything the ruleset should.
fn follow_camera(
    session: Res<ActiveSession>,
    zoom: Res<CameraZoom>,
    bodies: Query<(&CoreEntity, &Transform), Without<ChaseCamera>>,
    mut camera: Query<&mut Transform, With<ChaseCamera>>,
) {
    let Ok(mut view) = camera.single_mut() else {
        return;
    };
    let own = session.local_entity();
    // Before the player's own body exists — the first frames of a campaign
    // join, where nothing has replicated yet — hold the last centre rather
    // than snapping to the origin, which would read as the ship teleporting.
    let centre = bodies
        .iter()
        .find_map(|(core, transform)| (core.0 == own).then_some(transform.translation))
        .unwrap_or(Vec3::new(view.translation.x, 0.0, view.translation.z));
    *view = chase_camera_transform(centre, zoom.height_m());
}

/// A screen-space nickname tag following one craft.
#[derive(Component, Debug, Clone, Copy)]
struct ShipLabel(PersistId);

/// Font size of a ship's nickname tag, in pixels.
const SHIP_LABEL_SIZE_PX: f32 = 12.0;
/// How far under a craft's centre the tag sits, in pixels.
const SHIP_LABEL_DROP_PX: f32 = 22.0;
/// Rough advance width of one character at [`SHIP_LABEL_SIZE_PX`], used only
/// to centre the tag under the ship. Bevy's layout knows the true width, but
/// only after the frame this system runs in; being a few pixels off-centre is
/// a better trade than a tag that lags the ship by a frame.
const SHIP_LABEL_CHAR_PX: f32 = 0.5 * SHIP_LABEL_SIZE_PX;

/// Which craft get a tag this frame, and where on screen it goes.
///
/// Split out from [`sync_ship_labels`] so the decision can be asserted without
/// a render device: a craft is tagged **only** when the roster knows it and it
/// projects onto the screen. A craft the roster has never heard of contributes
/// nothing — not an empty string, not a placeholder — because a made-up name
/// is worse than no name (#484, #523).
fn ship_label_placements(
    roster: &roster::ShipRoster,
    projected: impl Iterator<Item = (PersistId, Option<Vec2>)>,
) -> BTreeMap<PersistId, (String, Vec2)> {
    projected
        .filter_map(|(entity, screen)| {
            let label = roster.label(entity)?;
            Some((entity, (label.to_owned(), screen?)))
        })
        .collect()
}

/// Draws each craft's nickname under it, in screen space.
///
/// Screen space rather than world space on purpose: a label is a HUD element,
/// so it has to stay the same readable size across #521's whole zoom range,
/// where a world-space text mesh would be unreadable at either end.
///
/// **A label is only a label** (#484, and see [`roster::ShipRoster`]). This
/// system reads a `PersistId` and asks for a string; nothing anywhere asks the
/// other way round, and a craft the roster does not know is drawn with **no
/// tag at all** rather than a placeholder that could be mistaken for a name.
fn sync_ship_labels(
    roster: Res<roster::ShipRoster>,
    session: Res<ActiveSession>,
    camera: Query<(&Camera, &GlobalTransform), With<ChaseCamera>>,
    bodies: Query<(&CoreEntity, &GlobalTransform), With<CraftBodyComposition>>,
    mut labels: Query<(
        Entity,
        &ShipLabel,
        &mut Node,
        &mut Visibility,
        &mut Text,
        &mut TextColor,
    )>,
    mut commands: Commands,
) {
    let Ok((camera, camera_transform)) = camera.single() else {
        return;
    };
    let own = session.local_entity();
    let wanted = ship_label_placements(
        &roster,
        bodies.iter().map(|(core, transform)| {
            (
                core.0,
                camera
                    .world_to_viewport(camera_transform, transform.translation())
                    .ok(),
            )
        }),
    );

    let mut placed = BTreeSet::new();
    for (entity, tag, mut node, mut visibility, mut text, mut colour) in &mut labels {
        match wanted.get(&tag.0) {
            Some((label, screen)) => {
                placed.insert(tag.0);
                if **text != *label {
                    **text = label.clone();
                }
                let half = label.chars().count() as f32 * SHIP_LABEL_CHAR_PX / 2.0;
                node.left = Val::Px(screen.x - half);
                node.top = Val::Px(screen.y + SHIP_LABEL_DROP_PX);
                // "This one is mine" is the accent's job everywhere else in
                // the skin, so the player's own tag wears it and everyone
                // else stays on the neutral ramp.
                *colour = TextColor(if tag.0 == own {
                    hud::ACCENT_PALE
                } else {
                    hud::MUTED
                });
                *visibility = Visibility::Inherited;
            }
            None => commands.entity(entity).despawn(),
        }
    }
    for (entity, (label, screen)) in wanted {
        if placed.contains(&entity) {
            continue;
        }
        let half = label.chars().count() as f32 * SHIP_LABEL_CHAR_PX / 2.0;
        commands.spawn((
            ShipLabel(entity),
            Node {
                position_type: PositionType::Absolute,
                left: Val::Px(screen.x - half),
                top: Val::Px(screen.y + SHIP_LABEL_DROP_PX),
                ..Default::default()
            },
            GlobalZIndex(60),
            Text::new(label),
            TextFont::from_font_size(SHIP_LABEL_SIZE_PX),
            TextColor(if entity == own {
                hud::ACCENT_PALE
            } else {
                hud::MUTED
            }),
        ));
    }
}

fn sync_rendered_state(
    mut commands: Commands,
    session: Res<ActiveSession>,
    mut rendered: Query<(Entity, &CoreEntity, &mut Transform)>,
) {
    for (body, entity, mut transform) in &mut rendered {
        let Some(state) = session.executor().state(entity.0) else {
            commands.entity(body).despawn();
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

fn session_banner_color(presentation: SessionPresentation) -> Color {
    match presentation {
        SessionPresentation::Live => Color::srgb(0.08, 0.42, 0.28),
        SessionPresentation::Dialing => Color::srgb(0.35, 0.27, 0.08),
        SessionPresentation::Local
        | SessionPresentation::Failed
        | SessionPresentation::Refused
        | SessionPresentation::Disconnected => Color::srgb(0.62, 0.19, 0.10),
    }
}

fn refresh_session_banner(
    session: Res<ActiveSession>,
    mut banner: Query<(&mut Text, &mut BackgroundColor), With<SessionBanner>>,
) {
    let presentation = SessionPresentation::from_join_state(session.join_state());
    if let Ok((mut text, mut background)) = banner.single_mut() {
        **text = presentation.label().to_owned();
        background.0 = session_banner_color(presentation);
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
    rock_bodies: Query<(), With<RockBody>>,
    roster: Res<roster::ShipRoster>,
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
        let mut counts = [0usize; 3];
        for entity in session.executor().entities().copied() {
            if let Some(RegolithState::Rock(rock)) = session.executor().state(entity) {
                if rock.hull > 0 {
                    counts[match rock.tier {
                        RockTier::Large => 0,
                        RockTier::Medium => 1,
                        RockTier::Small => 2,
                    }] += 1;
                }
            }
        }
        text.push('\n');
        text.push_str(&rock_census(counts, rock_bodies.iter().count()));
        text.push('\n');
        text.push_str(&roster.summary_line());
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
            text.push_str("\noffline local session - not a campaign path, banks nothing");
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
    metrics.session_scope =
        SessionPresentation::from_join_state(session.join_state()).session_scope();
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

    /// #521: the camera is centred on the player, and nothing else in the
    /// world may move it. The old `frame_camera` averaged every body and rose
    /// until the furthest fit, so a distant contact rescaled the world under a
    /// manoeuvring player. The second body here is the one that used to do it.
    #[test]
    fn the_camera_centres_on_the_players_own_craft_and_ignores_every_other_body() {
        let mut app = App::new();
        app.insert_resource(ActiveSession::Local(Box::<LocalSession>::default()));
        app.init_resource::<CameraZoom>();
        let own = app.world().resource::<ActiveSession>().local_entity();
        let elsewhere = PersistId::new(own.0 + 7);

        app.world_mut()
            .spawn((CoreEntity(own), Transform::from_xyz(400.0, 0.0, -250.0)));
        app.world_mut().spawn((
            CoreEntity(elsewhere),
            Transform::from_xyz(9_000.0, 0.0, 9_000.0),
        ));
        let camera = app
            .world_mut()
            .spawn((ChaseCamera, Transform::default()))
            .id();

        app.add_systems(Update, follow_camera);
        app.update();

        let view = *app
            .world()
            .entity(camera)
            .get::<Transform>()
            .expect("camera");
        assert!(
            (view.translation.x - 400.0).abs() < 1e-3 && (view.translation.z + 250.0).abs() < 1e-3,
            "the camera must sit over the player's own craft, not over {:?}",
            view.translation
        );
        assert!(
            (view.translation.y - CAMERA_DEFAULT_HEIGHT_M).abs() < 1e-3,
            "a distant contact must not change the zoom: height was {}",
            view.translation.y
        );
    }

    /// #521: the wheel owns the zoom, and it stops where the documentation
    /// says it stops.
    #[test]
    fn the_wheel_zoom_moves_by_one_step_and_clamps_to_its_documented_limits() {
        let start = CameraZoom::default();
        assert!(
            (start.zoomed(1.0).height_m() - CAMERA_DEFAULT_HEIGHT_M / CAMERA_ZOOM_STEP).abs()
                < 1e-3,
            "one notch away from the player must be exactly one step closer in"
        );
        assert!(
            start.zoomed(-1.0).height_m() > start.height_m(),
            "one notch towards the player must zoom out"
        );

        let mut deep = start;
        for _ in 0..200 {
            deep = deep.zoomed(1.0);
        }
        assert!(
            (deep.height_m() - CAMERA_MIN_HEIGHT_M).abs() < 1e-3,
            "zooming in without end must stop at the near limit, got {}",
            deep.height_m()
        );

        let mut far = start;
        for _ in 0..200 {
            far = far.zoomed(-1.0);
        }
        assert!(
            (far.height_m() - CAMERA_MAX_HEIGHT_M).abs() < 1e-3,
            "zooming out without end must stop at the far limit, got {}",
            far.height_m()
        );
    }

    /// The default framing has to show the envelope the weapon is fought in,
    /// or the rings the HUD draws are off screen at the framing the player
    /// spends most of the session at. Optimal is 300 m.
    #[test]
    fn the_default_framing_holds_the_weapons_optimal_ring() {
        assert!(
            visible_half_height_m(CAMERA_DEFAULT_HEIGHT_M) > 300.0,
            "the 300 m optimal ring must fit at the default zoom, half-height was {}",
            visible_half_height_m(CAMERA_DEFAULT_HEIGHT_M)
        );
        assert!(
            visible_half_height_m(CAMERA_MIN_HEIGHT_M) > 30.0,
            "a 22 m hull must still fit at full zoom-in"
        );
    }

    /// #524: rocks were adjudicated, lockable, mineable and collidable, and
    /// nothing ever put a body on screen. This asserts on the bodies, because
    /// asserting on state is exactly what could not see the bug.
    #[test]
    fn every_live_rock_gets_a_body_sized_and_tinted_by_its_tier() {
        use orrery_core::{QPos, QVel};
        use orrery_games::regolith::state::Rock;

        let mut local = LocalSession::default();
        let tiers = [
            (PersistId::new(50), RockTier::Large),
            (PersistId::new(51), RockTier::Medium),
            (PersistId::new(52), RockTier::Small),
        ];
        for (index, (entity, tier)) in tiers.iter().enumerate() {
            local.executor.insert(
                *entity,
                RegolithState::Rock(Rock::spawned(
                    *tier,
                    0,
                    QPos::from_metres(120.0 * index as f64, 0.0, 0.0),
                    QVel::default(),
                )),
            );
        }

        let mut app = App::new();
        app.add_plugins(AssetPlugin::default())
            .init_asset::<Mesh>()
            .init_asset::<StandardMaterial>()
            .insert_resource(ActiveSession::Local(Box::new(local)))
            .add_systems(Update, ensure_rock_bodies);
        app.update();

        let drawn: Vec<PersistId> = app
            .world_mut()
            .query_filtered::<&CoreEntity, With<RockBody>>()
            .iter(app.world())
            .map(|core| core.0)
            .collect();
        for (entity, _) in tiers {
            assert!(
                drawn.contains(&entity),
                "rock {entity:?} is in state and must have a body; drawn: {drawn:?}"
            );
        }
        assert_eq!(drawn.len(), 3, "one body per live rock, no more");

        // Idempotent: a second frame must not spawn a duplicate shell.
        app.update();
        let again = app
            .world_mut()
            .query_filtered::<&CoreEntity, With<RockBody>>()
            .iter(app.world())
            .count();
        assert_eq!(again, 3, "rock bodies must not accumulate per frame");

        // A rock that leaves the session's state stops being drawn rather
        // than freezing — the replica lifecycle, not a lifetime of our own.
        {
            let ActiveSession::Local(local) = &mut *app.world_mut().resource_mut::<ActiveSession>()
            else {
                unreachable!("the fixture session is local")
            };
            local.executor.take_state(PersistId::new(51));
        }
        app.update();
        let survivors: Vec<PersistId> = app
            .world_mut()
            .query_filtered::<&CoreEntity, With<RockBody>>()
            .iter(app.world())
            .map(|core| core.0)
            .collect();
        assert!(
            !survivors.contains(&PersistId::new(51)) && survivors.len() == 2,
            "an expired replica must take its body with it; left {survivors:?}"
        );
    }

    /// The three tiers must not be confusable at a glance, and the ramp runs
    /// against size on purpose: the smallest rock is the hardest to see.
    #[test]
    fn the_rock_tiers_are_distinguishable_by_more_than_size() {
        let finishes = [
            rock_finish(RockTier::Large),
            rock_finish(RockTier::Medium),
            rock_finish(RockTier::Small),
        ];
        let luma = |colour: Color| {
            let rgba = colour.to_linear();
            rgba.red + rgba.green + rgba.blue
        };
        assert!(
            luma(finishes[0].0) < luma(finishes[1].0) && luma(finishes[1].0) < luma(finishes[2].0),
            "lightness must run against size so the smallest tier stays visible"
        );
        assert!(
            finishes[0].1 > finishes[1].1 && finishes[1].1 > finishes[2].1,
            "facet count must run with size"
        );
        let radii = [
            RockTier::Large.limits().radius_mm,
            RockTier::Medium.limits().radius_mm,
            RockTier::Small.limits().radius_mm,
        ];
        assert!(
            radii[0] > radii[1] && radii[1] > radii[2],
            "the drawn size is the ruleset's own radius and must separate the tiers"
        );
    }

    /// The camera looks straight down, so the deck plane is exactly the
    /// camera's height away. A far plane inside the zoom range clips the whole
    /// world away silently, which is what Bevy's 1000 m default did.
    #[test]
    fn the_far_plane_clears_the_whole_zoom_range() {
        let projection = chase_camera_projection();
        assert!(
            projection.far > CAMERA_MAX_HEIGHT_M,
            "the deck must stay inside the frustum at full zoom-out: far {} vs height {}",
            projection.far,
            CAMERA_MAX_HEIGHT_M
        );
        assert!(
            projection.near < CAMERA_MIN_HEIGHT_M,
            "the deck must stay inside the frustum at full zoom-in"
        );
    }

    /// #523/#484: a label is a label. A craft the roster knows gets its name
    /// under it; a craft it does not know gets nothing at all, and nothing
    /// here may invent a stand-in.
    #[test]
    fn only_a_craft_the_roster_knows_gets_a_tag() {
        use crate::roster::{entity_of_slot, RosterResponse, RosterRow, ShipRoster};

        let mut roster = ShipRoster::default();
        roster.accept(&RosterResponse {
            roster: vec![RosterRow {
                slot: 8,
                nickname: "ada".to_owned(),
            }],
        });
        let known = entity_of_slot(8);
        let stranger = entity_of_slot(3);

        let placements = ship_label_placements(
            &roster,
            [
                (known, Some(Vec2::new(400.0, 300.0))),
                (stranger, Some(Vec2::new(500.0, 300.0))),
            ]
            .into_iter(),
        );
        assert_eq!(
            placements.get(&known).map(|(label, _)| label.as_str()),
            Some("ada")
        );
        assert!(
            !placements.contains_key(&stranger),
            "a craft with no roster row must be drawn with no tag: {placements:?}"
        );

        // Off screen is not a tag either: a name floating at the edge of the
        // window belongs to a ship the player cannot see.
        let offscreen = ship_label_placements(&roster, [(known, None)].into_iter());
        assert!(offscreen.is_empty(), "an unprojectable craft gets no tag");

        // And an empty roster is a quiet screen, not a screen of placeholders.
        let silent = ship_label_placements(
            &ShipRoster::default(),
            [(known, Some(Vec2::ZERO)), (stranger, Some(Vec2::ZERO))].into_iter(),
        );
        assert!(silent.is_empty(), "no roster means no tags: {silent:?}");
    }

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

    /// A row is campaign evidence only while the transport state machine says
    /// it is joined. In particular, failed dials must remain visibly and
    /// mechanically local even though they still occupy the campaign runtime.
    #[test]
    fn local_fallbacks_cannot_present_or_serialize_as_live_campaigns() {
        let local = SessionPresentation::from_join_state(None);
        assert_eq!(local.label(), "LOCAL SANDBOX - NOT CONNECTED TO CAMPAIGN");
        assert_eq!(local.session_scope(), SessionScope::Local);

        let failed = JoinState::Failed("fixture dial failure".to_owned());
        let failed = SessionPresentation::from_join_state(Some(&failed));
        assert_eq!(failed.label(), "LOCAL SANDBOX - CAMPAIGN DIAL FAILED");
        assert_eq!(failed.session_scope(), SessionScope::Local);

        let joined = SessionPresentation::from_join_state(Some(&JoinState::Joined));
        assert_eq!(joined.label(), "CAMPAIGN LIVE");
        assert_eq!(joined.session_scope(), SessionScope::Campaign);
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

    #[test]
    fn body_despawns_when_its_replicated_state_expires() {
        let mut local = LocalSession::default();
        assert!(local.executor.take_state(OPPONENT).is_some());
        let mut app = App::new();
        app.insert_resource(ActiveSession::Local(Box::new(local)))
            .add_systems(Update, sync_rendered_state);
        let body = app
            .world_mut()
            .spawn((CoreEntity(OPPONENT), Transform::default()))
            .id();

        app.update();

        assert!(
            app.world().get_entity(body).is_err(),
            "an expired replica must not leave its last transform on screen"
        );
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
    fn delivered_lock_break_reaches_both_skin_consumers() {
        let mut tracks = ProjectileTracks::default();
        let mut broken = LockBreak::default();
        let mut shots = ShotFeedback {
            cue: Some(combat::ShotCue::Arrival { target: OPPONENT }),
            ticks_left: combat::SHOT_CUE_TICKS,
        };
        let delivered = [DeliveredOrder {
            from: OPPONENT,
            recipient: PLAYER,
            order: Order::LockBroken {
                target: OPPONENT,
                reason: orrery_games::regolith::order::LockBreakReason::RangeExceeded,
            },
        }];

        observe_skin_effects(
            &[],
            &delivered,
            PLAYER,
            &[],
            &mut tracks,
            &mut broken,
            &mut shots,
        );

        assert_eq!(broken.banner(), "LOCK BROKEN - RANGE EXCEEDED");
        assert!(
            shots.cue.is_none(),
            "the same delivered break must cancel a provisional shot"
        );
    }

    #[test]
    fn fire_without_a_ruleset_statement_invents_no_skin_feedback() {
        let mut tracks = ProjectileTracks::default();
        let mut broken = LockBreak::default();
        let mut shots = ShotFeedback::default();
        let muzzle = [Outcome::DamageDealt {
            attacker: PLAYER,
            target: OPPONENT,
            amount: 7,
            attacker_pos: orrery_core::QPos::default(),
            attacker_vel: orrery_core::QVel::default(),
            attacker_yaw_urad: 0,
            attacker_archetype: Archetype::Interceptor,
            attacker_weapon: orrery_games::regolith::weapon::WeaponKind::Stock,
            flight_ticks: None,
        }];
        observe_skin_effects(
            &muzzle,
            &[],
            PLAYER,
            &[],
            &mut tracks,
            &mut broken,
            &mut shots,
        );
        assert_eq!(tracks.tracks().len(), 1, "the ruleset statement is copied");

        let delivered = [DeliveredOrder {
            from: OPPONENT,
            recipient: PLAYER,
            order: Order::Fire,
        }];

        observe_skin_effects(
            &[],
            &delivered,
            PLAYER,
            &[],
            &mut tracks,
            &mut broken,
            &mut shots,
        );

        assert!(tracks.tracks().is_empty());
        assert!(broken.banner().is_empty());
        assert!(shots.cue.is_none());
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
