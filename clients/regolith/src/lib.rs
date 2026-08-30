//! Bevy rendering and keyboard input over Regolith's headless rules pipeline.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod admission;
pub mod aoi;
pub mod assets;
pub mod campaign;
pub mod combat;
mod contact_arrows;
pub mod craft;
pub mod grab;
mod hearsay;
pub mod hud;
pub mod identity;
pub mod intent;
pub mod join;
pub mod legend;
pub mod lobby;
pub mod net;
pub mod roster;
pub mod session;
pub mod starfield;
pub mod telemetry;

/// Commit revision embedded in this client binary at build time.
pub const BUILD_REV: &str = env!("ORRERY_BUILD_REV");

/// Public campaign-admission origin used by a no-argument volunteer launch.
pub const DEFAULT_ADMISSION_URL: &str = "https://campaigns.distopik.com";

/// The longest lobby this client will sit through before it abandons a join.
///
/// The host answers a join request only when its lobby closes and the initial
/// cohort is frozen, so every client-side join bound is really a bound on the
/// lobby, not on a round trip. Both bounds below were written against a
/// 90-second freeze and were never revisited when the standing campaign moved
/// to a 180-second lobby: admission accepted the seat, the client then gave up
/// mid-lobby, and the host lost the connection at `StartV1`. Deriving them from
/// one number is what stops that drifting apart again.
///
/// `lobby_seconds` in `scripts/p1-swarm-always-on.py` refuses to configure a
/// lobby longer than this, so a control-file edit cannot outrun shipped clients.
pub const CAMPAIGN_LOBBY_HOLD: Duration = Duration::from_secs(180);

/// Lobby hold plus the slack a join needs around it: the handshake read waits
/// out the lobby, and the outer deadline additionally covers dial and bind.
pub const JOIN_HANDSHAKE_READ_TIMEOUT: Duration =
    CAMPAIGN_LOBBY_HOLD.saturating_add(Duration::from_secs(30));
/// The whole join attempt's bound, which must outlast the handshake read so a
/// lobby that never closes is reported as the handshake timing out, not as an
/// unattributed dial failure.
pub const JOIN_DEADLINE: Duration =
    JOIN_HANDSHAKE_READ_TIMEOUT.saturating_add(Duration::from_secs(30));

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
#[derive(Component)]
struct LobbyPanel;

#[derive(Debug, Default, Resource)]
struct OverlayState {
    expanded: bool,
}

/// Starts the session with the F3 diagnostics pane already open.
///
/// The pane carries the rock census (#524) and the roster line (#523), which
/// are exactly the two numbers a live capture needs to read. Ordinary clients
/// never insert the resource and press F3 like everyone else.
#[derive(Debug, Resource)]
pub struct OverlayOpen;

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

/// Sweeps the camera zoom between its limits, for capturing evidence.
///
/// **An evidence affordance, and deliberately a thin one.** It does not set
/// [`CameraZoom`] — it writes the same `MouseWheel` message winit writes, so
/// the stage under observation is `zoom_camera` itself rather than a second
/// code path that only exists for captures. The existing
/// [`GeometryCapture::auto_drive`] is the same idea for shots.
///
/// Ordinary clients never insert the resource.
#[derive(Debug, Resource)]
pub struct ZoomSweep {
    /// Ticks between notches. One notch every `period` frames.
    period: u32,
    frame: u32,
    notches_left: i32,
    direction: f32,
}

impl Default for ZoomSweep {
    fn default() -> Self {
        Self {
            period: 6,
            frame: 0,
            // Enough notches to walk the documented range end to end.
            notches_left: 24,
            direction: -1.0,
        }
    }
}

/// Feeds one wheel notch at a time into the real input queue, reversing at the
/// ends so a capture can catch both extremes.
fn drive_zoom_sweep(
    mut sweep: ResMut<ZoomSweep>,
    zoom: Res<CameraZoom>,
    mut wheel: MessageWriter<bevy::input::mouse::MouseWheel>,
) {
    sweep.frame = sweep.frame.saturating_add(1);
    if !sweep.frame.is_multiple_of(sweep.period) {
        return;
    }
    if sweep.notches_left <= 0 {
        sweep.notches_left = 24;
        sweep.direction = -sweep.direction;
    }
    sweep.notches_left -= 1;
    let _ = zoom;
    wheel.write(bevy::input::mouse::MouseWheel {
        unit: bevy::input::mouse::MouseScrollUnit::Line,
        x: 0.0,
        y: sweep.direction,
        window: Entity::PLACEHOLDER,
        phase: bevy::input::touch::TouchPhase::Moved,
    });
}

/// Writes a PNG of the rendered frame at each end of the zoom range.
///
/// **The fallback, not the first move.** A measurement is reproducible in CI
/// and a screenshot is not, so anything a number can settle is settled by
/// [`RockCensus`] and by the tests that pin the pixel arithmetic. What a
/// number cannot settle is whether a lit body reads against the field behind
/// it — a rock's tint is a *ceiling* on how bright it renders, and only the
/// frame the GPU produced says what came off it. So this exists for #530 and
/// #531, and it captures at the two zoom extremes those issues name.
///
/// The frame comes from Bevy's own screenshot path — the swapchain texture the
/// renderer just presented — rather than from an X grab of the window, so it
/// is the pixels the GPU wrote and needs no compositor to be readable.
///
/// Ordinary clients never insert the resource; pair it with [`ZoomSweep`],
/// which is what walks the camera to the ends.
#[derive(Debug, Resource)]
pub struct FrameCapture {
    dir: PathBuf,
    taken: BTreeSet<&'static str>,
}

impl FrameCapture {
    /// Capture into `dir`, creating it if it does not exist.
    ///
    /// # Errors
    /// A rendered string when the directory cannot be created.
    pub fn into_dir(dir: PathBuf) -> Result<Self, String> {
        std::fs::create_dir_all(&dir)
            .map_err(|error| format!("cannot create {}: {error}", dir.display()))?;
        Ok(Self {
            dir,
            taken: BTreeSet::new(),
        })
    }
}

/// How far from a zoom limit still counts as "at that extreme" for a frame
/// that also has to catch a passing event.
///
/// The bare zoom frames are taken exactly at the clamp. An impact burst lives
/// for [`combat::IMPACT_BURST_TICKS`] and cannot be summoned, so waiting for
/// one to coincide with the exact clamp is waiting for two independent things;
/// a tenth of the range either side is still unambiguously "zoomed out" or
/// "zoomed in", and the captured line prints the height it actually had.
const ZOOM_EXTREME_TOLERANCE: f32 = 0.10;

/// Saves one frame the first time the camera reaches each end of its range,
/// and one at each end while a confirmed hit is bursting.
///
/// The census line is printed beside the path, from the same frame, so the
/// picture and the numbers can never be quoted against each other by mistake.
///
/// The `hit-` frames are #531's acceptance: the impact cue seen at both zoom
/// extremes. They are gated on [`ShotFeedback::impact_burst`] and therefore on
/// an adjudicated `Hit` — the skin may not manufacture one to be photographed.
#[allow(clippy::too_many_arguments)]
fn capture_zoom_extreme_frames(
    mut capture: ResMut<FrameCapture>,
    zoom: Res<CameraZoom>,
    session: Res<ActiveSession>,
    feedback: Res<ShotFeedback>,
    rock_bodies: RockBodyQuery,
    meshes: Res<Assets<Mesh>>,
    windows: Query<&Window, With<PrimaryWindow>>,
    mut commands: Commands,
) {
    use bevy::render::view::screenshot::{save_to_disk, Screenshot};

    let height_m = zoom.height_m();
    // Every label this frame satisfies, rarest first. A bursting frame at an
    // extreme is the thing that cannot be waited for, so it is claimed before
    // the bare one, which any later frame at the same extreme can still take.
    let mut labels: Vec<&'static str> = Vec::new();
    if feedback.impact_burst().is_some() {
        if height_m <= CAMERA_MIN_HEIGHT_M * (1.0 + ZOOM_EXTREME_TOLERANCE) {
            labels.push("hit-zoom-min");
        } else if height_m >= CAMERA_MAX_HEIGHT_M * (1.0 - ZOOM_EXTREME_TOLERANCE) {
            labels.push("hit-zoom-max");
        }
    }
    // Exact equality is what the clamp actually produces at either end, but a
    // hair of tolerance keeps this from depending on that.
    if height_m <= CAMERA_MIN_HEIGHT_M * 1.001 {
        labels.push("zoom-min");
    } else if height_m >= CAMERA_MAX_HEIGHT_M * 0.999 {
        labels.push("zoom-max");
    }
    let Some(label) = labels
        .into_iter()
        .find(|label| !capture.taken.contains(label))
    else {
        return;
    };
    capture.taken.insert(label);
    let path = capture.dir.join(format!("{label}.png"));
    let census = gather_rock_census(
        &session,
        &rock_bodies,
        &meshes,
        height_m,
        viewport_height_px(&windows),
    );
    println!(
        "frame_capture {label} {} | {}",
        path.display(),
        census.line()
    );
    commands
        .spawn(Screenshot::primary_window())
        .observe(save_to_disk(path));
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

    /// The interest cell edge this session's AOI boundary is built from, in
    /// metres, or `None` when the session has no interest set at all.
    ///
    /// The offline sandbox holds both seats in one local executor: there is no
    /// host, no replication, and therefore no boundary. Fading against an
    /// invented one there would have the skin drawing a limit the run does not
    /// have (#519). A campaign that has not finished dialling has not yet been
    /// told anything either, so it is `None` until [`JoinState::Joined`].
    ///
    /// The value itself is the campaign runtime's own `cell_edge_m` — the same
    /// field `committed_cell` divides by when telling the host which cell this
    /// craft is in — so the skin cannot drift from the host's definition of
    /// the edge the way #499 and #502 did.
    #[must_use]
    pub fn aoi_edge_m(&self) -> Option<f32> {
        match self {
            Self::Local(_) => None,
            Self::Campaign(runtime) => {
                matches!(runtime.state(), JoinState::Joined).then(|| runtime.cell_edge_m() as f32)
            }
        }
    }

    /// The campaign's current hearsay snapshot, for the rendering skin only.
    ///
    /// This is crate-private so input and ruleset crates cannot import a
    /// hearsay path. Its one consumer is the screen-edge-arrow skin,
    /// [`contact_arrows::sync_contact_arrows`] (#610).
    #[must_use]
    pub(crate) fn hearsay_view(
        &self,
        roster: &roster::ShipRoster,
    ) -> Option<hearsay::HearsayRenderView> {
        match self {
            Self::Local(_) => None,
            Self::Campaign(runtime) => Some(runtime.hearsay_view(roster)),
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
        let mut ship_roster = roster::ShipRoster::default();
        if let Some(config) = &self.campaign {
            ship_roster.set_own(roster::OwnLabelGrant {
                slot: config.slot,
                nickname: config.own_label.as_deref(),
            });
        }
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
            .init_resource::<legend::LegendState>()
            .init_resource::<MetricWindow>()
            .init_resource::<CameraZoom>()
            .init_resource::<starfield::StarDrift>()
            .init_resource::<aoi::AoiBoundary>()
            .init_resource::<aoi::AoiFadeCensus>()
            .insert_resource(ship_roster)
            .init_resource::<roster::RosterTask>()
            .init_resource::<lobby::LobbyView>()
            .init_resource::<SelectedLock>()
            .init_resource::<CombatView>()
            .init_resource::<grab::ReachView>()
            .init_resource::<grab::GrabLatch>()
            .init_resource::<ProjectileTracks>()
            .init_resource::<LockBreak>()
            .init_resource::<ShotFeedback>()
            .add_systems(Startup, (setup_scene, open_overlay_if_asked))
            .add_systems(FixedUpdate, drive_core)
            .add_systems(
                Update,
                (
                    toggle_overlay,
                    // The legend watches the same `ButtonInput` the intent
                    // path reads and never consumes a press, so noticing an
                    // input can never eat one.
                    legend::note_used_inputs,
                    legend::toggle_legend,
                    legend::sync_legend
                        .after(legend::note_used_inputs)
                        .after(legend::toggle_legend),
                    sync_rendered_state,
                    select_clicked_body.before(sync_rendered_state),
                    recompose_craft_bodies.after(sync_rendered_state),
                    ensure_local_body.after(sync_rendered_state),
                    ensure_focus_body.after(recompose_craft_bodies),
                    ensure_rock_bodies.after(sync_rendered_state),
                    // After every body-spawning system, so a craft that
                    // appears this frame is faded on the frame it appears
                    // rather than flashing at full opacity first.
                    aoi::read_aoi_boundary.after(sync_rendered_state),
                    aoi::sync_aoi_fade
                        .after(aoi::read_aoi_boundary)
                        .after(ensure_rock_bodies)
                        .after(ensure_focus_body)
                        .after(ensure_local_body),
                    // After `sync_rendered_state`: it frames the positions
                    // that system just wrote, not last frame's.
                    follow_camera.after(sync_rendered_state),
                    (
                        starfield::sync_starfield.after(follow_camera),
                        // After `read_combat_state`, so the smear is drawn
                        // from the velocity this frame's `CombatView` copied
                        // out of the executor rather than the previous
                        // frame's.
                        starfield::drive_star_smear.after(read_combat_state),
                    ),
                    sync_ship_labels.after(follow_camera),
                    // After `follow_camera` for the same reason the ship
                    // labels are: an edge arrow is a bearing taken from
                    // *this* frame's camera basis and own-craft position.
                    contact_arrows::sync_contact_arrows
                        .after(follow_camera)
                        .after(ensure_local_body),
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
                    hud::sync_grab_reach_ring,
                    hud::sync_tracers,
                    hud::sync_impact_flash,
                    hud::sync_gauges,
                    hud::refresh_combat_hud,
                )
                    .chain()
                    .after(sync_rendered_state)
                    // The glyph-sized overlays -- the lock reticle and the
                    // impact marker ring -- are scaled from `CameraZoom`, so
                    // they have to be placed against *this* frame's zoom the
                    // way `follow_camera` is. Without this the ring is sized
                    // from the previous frame's height during a zoom, which a
                    // live capture reads as a ring that is not quite holding
                    // its apparent size.
                    .after(zoom_camera),
            )
            .add_systems(Update, capture_tracer_geometry.after(hud::sync_tracers))
            .add_systems(
                Update,
                capture_impact_geometry
                    .after(hud::sync_impact_flash)
                    .run_if(resource_exists::<GeometryCapture>),
            )
            .add_systems(
                Update,
                (capture_aoi_census, capture_ship_label_census)
                    .after(aoi::sync_aoi_fade)
                    .after(sync_ship_labels)
                    .run_if(resource_exists::<GeometryCapture>)
                    .run_if(on_timer(Duration::from_secs(1))),
            )
            .add_systems(
                Update,
                stream_metrics.run_if(on_timer(Duration::from_secs(1))),
            )
            .add_systems(
                Update,
                roster::refresh_roster.run_if(on_timer(roster::ROSTER_REFRESH)),
            )
            .add_systems(Update, refresh_lobby_panel)
            .add_systems(
                Update,
                drive_zoom_sweep
                    .before(zoom_camera)
                    .run_if(resource_exists::<ZoomSweep>),
            )
            .add_systems(
                Update,
                capture_zoom_extreme_frames
                    .after(zoom_camera)
                    .run_if(resource_exists::<FrameCapture>),
            );
        install_campaign_finalization(app);
    }
}

/// Install the final session write after every frame's exit producers.
fn install_campaign_finalization(app: &mut App) {
    // `AppExit` is commonly emitted by an Update system (the headless
    // preflight does exactly that). The runner observes it immediately after
    // this frame, so another Update reader can lose the final record to
    // scheduler order. Last still belongs to the same frame, after every
    // Update exit producer and before the runner decides to tear the app
    // down.
    app.add_systems(
        Last,
        write_campaign_record_on_exit.in_set(CampaignFinalization),
    );
}

#[derive(SystemSet, Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct CampaignFinalization;

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
    legend::spawn_legend(&mut commands);
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
    // The waiting room. Top-centre, under the session banner: it must be the
    // first thing read, and it must not sit on the controls legend in the
    // bottom-right — the lobby is exactly when a first-time player should be
    // reading that legend, so the two are on screen together rather than in
    // sequence. Hidden until the host has actually described a lobby.
    commands.spawn((
        Node {
            position_type: PositionType::Absolute,
            top: Val::Px(LOBBY_PANEL_TOP_PX),
            width: Val::Percent(100.0),
            justify_content: JustifyContent::Center,
            ..Default::default()
        },
        GlobalZIndex(99),
        children![(
            Text::new(""),
            TextFont::from_font_size(LOBBY_ROW_FONT_PX),
            TextColor(Color::srgb(0.86, 0.90, 0.95)),
            Node {
                display: Display::None,
                padding: UiRect::axes(Val::Px(14.0), Val::Px(9.0)),
                border_radius: BorderRadius::all(Val::Px(8.0)),
                ..Default::default()
            },
            BackgroundColor(Color::srgba(0.035, 0.05, 0.075, 0.92)),
            LobbyPanel,
        )],
    ));
}

/// Where the waiting-room panel sits, in pixels from the top.
///
/// Clear of the session banner at 8 px and of nothing else: a full eight-seat
/// room is a heading, three summary lines and eight rows, which at
/// [`LOBBY_ROW_FONT_PX`] is roughly 190 px and finishes well above the
/// mid-screen crosshair even in the 720-line window #552 calls tight.
const LOBBY_PANEL_TOP_PX: f32 = 44.0;

/// Font size of a waiting-room line, in pixels.
const LOBBY_ROW_FONT_PX: f32 = 13.0;

/// Whether the waiting room belongs on screen at all.
///
/// Two conditions, both necessary. The player must still be waiting for
/// something — a dial in flight, a refusal to read, or a host that says the
/// attempt has not started — and the host must have said enough to draw. A
/// service older than #573 satisfies the first and never the second, so it
/// gets no panel rather than a room assembled out of craft labels.
fn lobby_panel_visible(
    presentation: SessionPresentation,
    lobby: &lobby::LobbyView,
    has_notice: bool,
) -> bool {
    let still_waiting = match (presentation, &lobby.phase) {
        // Nothing to wait for: this run never joined a campaign.
        (SessionPresentation::Local, _) => false,
        // Dialling, refused, failed or dropped: the room is where the player
        // finds out which, and it carries the notice saying so.
        (
            SessionPresentation::Dialing
            | SessionPresentation::Failed
            | SessionPresentation::Refused
            | SessionPresentation::Disconnected,
            _,
        ) => true,
        // Joined: keep the room up only while the host says the attempt has
        // not started. A waiting room over a live game is a lie about play.
        (SessionPresentation::Live, Some(phase)) => phase.is_waiting(),
        (SessionPresentation::Live, None) => false,
    };
    still_waiting && (lobby.is_describable() || has_notice)
}

/// Draw the waiting room, or nothing at all.
///
/// The panel appears only when the host has actually described a lobby: a
/// phase it named, and at least one human seat. A service older than #573
/// sends neither, and the correct output there is no panel — inventing a seat
/// map out of whatever rows arrived would put a room on screen that no host
/// asserted (A12 §5.6, ADR-0050).
///
/// It also retires itself once the attempt is running and this client is in
/// it. A waiting room over a live game is a lie about the state of play, and
/// the seat map's job is done the moment the ships are the seat map.
fn refresh_lobby_panel(
    session: Res<ActiveSession>,
    lobby: Res<lobby::LobbyView>,
    mut panel: Query<(&mut Text, &mut Node), With<LobbyPanel>>,
) {
    let presentation = SessionPresentation::from_join_state(session.join_state());
    // What the host said about *this* client's join, in the room where the
    // player is waiting for it. `JoinState` already carries the host's own
    // words; nothing here paraphrases them.
    let notice = match session.join_state() {
        Some(JoinState::Refused(reason)) => Some(lobby::refusal_sentence(None, Some(reason), None)),
        Some(JoinState::Failed(reason)) => {
            lobby::plain_ascii(reason).map(|reason| format!("Could not join: {reason}"))
        }
        Some(JoinState::Closed { .. }) => {
            Some("The host ended this attempt. The next lobby opens shortly.".to_owned())
        }
        Some(JoinState::Dialing | JoinState::Joined) | None => None,
    };
    let visible = lobby_panel_visible(presentation, &lobby, notice.is_some());
    let Ok((mut text, mut node)) = panel.single_mut() else {
        return;
    };
    node.display = if visible {
        Display::Flex
    } else {
        Display::None
    };
    if visible {
        let mut view = lobby.clone();
        view.notice = notice.or_else(|| lobby.notice.clone());
        **text = view.text();
    }
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
    let arc_finish = hud::firing_arc_material(seat, accent);
    let arc_fade = aoi::fadeable(entity, &arc_finish);
    let arc_material = materials.add(arc_finish);
    spawned.with_children(|craft_root| {
        for arc in craft::firing_arcs(archetype) {
            craft_root.spawn((
                Name::new(arc.name),
                FiringArcFan(entity),
                arc_fade,
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
        //
        // **Known limit of #533's fade on this branch.** The scene's own
        // materials are owned by the glTF loader and are shared across every
        // body that loads the same asset, so tagging them `AoiFadeable` here
        // would fade every craft together the moment one of them neared the
        // boundary. Only the firing-arc fans above carry the tag on this
        // path, so a glTF hull fades its arc marking and not its plate. No
        // asset ships in this checkout, so no run today takes this branch;
        // doing it properly needs per-body material instances, which is a
        // change to the asset path rather than to the fade.
        spawned.insert(WorldAssetRoot(scene));
        return;
    }
    // Otherwise the chassis is composed from Bevy primitives, per archetype,
    // from the same reading the arcs above took.
    spawned.with_children(|craft_root| {
        for part in craft::parts(archetype) {
            let finish = craft::finish_material(part.finish, seat, accent);
            let fade = aoi::fadeable(entity, &finish);
            craft_root.spawn((
                Name::new(part.name),
                fade,
                Mesh3d(meshes.add(craft::mesh_for(part.shape))),
                MeshMaterial3d(materials.add(finish)),
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
        // Filled in by the proximity emitter below, not by a key: #568's
        // owner decision is that flying into a pickup collects it.
        grab: None,
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
    mut reach: ResMut<grab::ReachView>,
    mut latch: ResMut<grab::GrabLatch>,
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
    // The one place a grab is authored. It reads *this* tick's replicated
    // pickup state through the ruleset's own reach predicate and latches, so
    // an approach costs one order rather than one per tick inside 25 m.
    *reach = grab::ReachView::read(session.executor(), session.local_entity());
    controls.grab = latch.select(&reach);
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

/// Prints the interest-fade census once a second under `--capture-geometry`.
///
/// The evidence #533 needs is not "the arithmetic is right" — the unit tests
/// hold that — but "the fade reached a material in a live campaign, against
/// the host's own boundary". This prints what
/// [`aoi::sync_aoi_fade`] actually wrote.
fn capture_aoi_census(
    session: Res<ActiveSession>,
    census: Res<aoi::AoiFadeCensus>,
    rock_bodies: RockBodyQuery,
    meshes: Res<Assets<Mesh>>,
    windows: Query<&Window, With<PrimaryWindow>>,
    zoom: Res<CameraZoom>,
) {
    // The rock line rides along because #530 is judged at both zoom extremes
    // and there is no way to read a tint out of a running window; the camera
    // height says which extreme the frame was at, and the pixel figures say
    // what the tint was carried on.
    let rocks = gather_rock_census(
        &session,
        &rock_bodies,
        &meshes,
        zoom.height_m(),
        viewport_height_px(&windows),
    );
    println!(
        "aoi_census {} | camera {:.0} m | {}",
        census.line(session.aoi_edge_m()),
        zoom.height_m(),
        rocks.line(),
    );
}

/// Prints the impact cue's drawn size, in world metres and in pixels.
///
/// #531 asks whether an adjudicated hit is legible at both ends of the zoom
/// range, and the argument it was raised with was arithmetic about constants.
/// This reads the transforms [`hud::sync_impact_flash`] actually wrote — the
/// burst sphere's scale against its authored mesh radius, and the marker ring's
/// scale, which is its world radius outright — so the number is the one the
/// renderer used rather than one recomputed beside it.
///
/// One line per frame a burst is live, which is at most
/// [`combat::IMPACT_BURST_TICKS`] frames per confirmed hit.
fn capture_impact_geometry(
    feedback: Res<ShotFeedback>,
    zoom: Res<CameraZoom>,
    windows: Query<&Window, With<PrimaryWindow>>,
    flashes: hud::FlashReadQuery,
    markers: hud::MarkerReadQuery,
) {
    let Some((target, progress)) = feedback.impact_burst() else {
        return;
    };
    let height_m = zoom.height_m();
    let viewport_px = viewport_height_px(&windows);
    let drawn = |query_result: Option<(&Transform, &Visibility)>, mesh_radius_m: f32| {
        query_result.map_or((0.0, 0.0, false), |(transform, visibility)| {
            let radius_m = transform.scale.x * mesh_radius_m;
            (
                radius_m,
                apparent_diameter_px(radius_m, height_m, viewport_px),
                *visibility != Visibility::Hidden,
            )
        })
    };
    let (burst_m, burst_px, burst_shown) =
        drawn(flashes.iter().next(), hud::IMPACT_FLASH_MESH_RADIUS_M);
    // The marker's mesh is a unit torus, so its scale *is* its world radius.
    let (marker_m, marker_px, marker_shown) = drawn(markers.iter().next(), 1.0);
    println!(
        "impact_capture target={} progress={progress:.2} | burst {burst_m:.2} m = {burst_px:.1} px \
         shown={burst_shown} | marker {marker_m:.2} m = {marker_px:.1} px shown={marker_shown} | \
         cue {:.1} px | camera {height_m:.0} m / {viewport_px:.0} px",
        target.0,
        if marker_shown {
            marker_px.max(burst_px)
        } else {
            burst_px
        },
    );
}

/// Prints what the render world actually labelled, not merely what the last
/// roster response promised. This is the headless proof for #529: a body may be
/// present while its sideband label is absent or while it is off screen, and
/// those cases must remain visible as `unlabelled` rather than being inferred
/// away from the roster length.
fn capture_ship_label_census(
    bodies: Query<&CoreEntity, With<CraftBodyComposition>>,
    labels: Query<(&ShipLabel, &Text)>,
) {
    let craft: BTreeSet<PersistId> = bodies.iter().map(|body| body.0).collect();
    let mut names: Vec<(PersistId, &str)> = labels
        .iter()
        .filter_map(|(tag, text)| craft.contains(&tag.0).then_some((tag.0, text.as_str())))
        .collect();
    names.sort_unstable_by_key(|(entity, _)| *entity);
    let resolved = names
        .iter()
        .map(|(entity, name)| format!("{}={name}", entity.0))
        .collect::<Vec<_>>()
        .join(",");
    eprintln!(
        "ship_label_census craft={} labelled={} unlabelled={} resolved=[{}]",
        craft.len(),
        names.len(),
        craft.len().saturating_sub(names.len()),
        resolved,
    );
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
/// The tiers are 40 m, 20 m and 8 m in radius, so **size is the primary tier
/// cue and it is the ruleset's own number** — nothing here has to carry that
/// job. Facet count runs *with* size, because a 40 m body has the screen area
/// to show facets and an 8 m one reads better as a single chunk.
///
/// Lightness still tilts *against* size, for the reason #528 gave: the small
/// tier is the one hardest to notice, so it gets the most help. What #530
/// corrected is the **magnitude** of that tilt. It used to run from 0.40 to
/// 0.72 in sRGB, which made the 40 m tier the darkest object in the scene —
/// and a 40 m rock is the one you most need to see early, because collisions
/// apply real mutual force since #514. So the tilt survives and the ramp does
/// not invert; instead every tier is lifted onto a **contrast floor that does
/// not depend on tier at all** ([`ROCK_MIN_TINT_LUMA`]), and the whole ramp is
/// compressed to a nuance rather than a hierarchy
/// ([`ROCK_MAX_TINT_LUMA_RATIO`]).
///
/// The floor is set against the thing rocks are actually seen against. The
/// starfield (#525) draws `unlit` quads at up to 0.62 grey, while a rock is a
/// **lit** body: its rendered brightness is its tint multiplied by whatever
/// the one directional light gives it, so its tint is a ceiling on how light
/// it can ever appear. A tint below the star layers meant the largest rock
/// could render darker than the background it sat in front of and read as a
/// hole rather than an object.
///
/// The tints stay on a warm neutral ramp rather than taking
/// [`hud::MINING_AMBER`]: that amber is the mining *lock* colour, and a rock
/// wearing it unlocked would say the player has a mining lock they do not
/// have. The remaining per-tier separation is temperature — the large tier
/// warmest, the small tier closest to neutral — which reads at the zoom where
/// a rock fills enough pixels to have a colour at all, while size carries the
/// tier at the zoom where it does not.
const fn rock_finish(tier: RockTier) -> (Color, u32) {
    match tier {
        RockTier::Large => (Color::srgb(0.66, 0.61, 0.53), 2),
        RockTier::Medium => (Color::srgb(0.70, 0.66, 0.59), 1),
        RockTier::Small => (Color::srgb(0.74, 0.72, 0.70), 0),
    }
}

/// The lightness no rock tint may fall below, as linear Rec. 709 luma.
///
/// Set at the middle starfield layer's own grey (0.42 sRGB), so the dimmest
/// rock in the game still has a lighter surface than most of the field behind
/// it before the scene light has taken anything off it. See
/// [`rock_finish`].
pub const ROCK_MIN_TINT_LUMA: f32 = 0.144;

/// How much lighter the lightest tier may be than the darkest.
///
/// The old ramp ran 3.6:1, which is a hierarchy — it said "large rocks are
/// background". At 1.6:1 the tilt is still there and still helps the small
/// tier, but no tier is *the dark one*. See [`rock_finish`].
pub const ROCK_MAX_TINT_LUMA_RATIO: f32 = 1.6;

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
        let finish = StandardMaterial {
            base_color: tint,
            metallic: 0.05,
            perceptual_roughness: 0.95,
            ..Default::default()
        };
        // A rock is replicated remote state on the same interest set as a
        // craft, so it leaves the view through the same boundary and gets the
        // same fade (#533).
        let fade = aoi::fadeable(entity, &finish);
        commands.spawn((
            CoreEntity(entity),
            RockBody,
            fade,
            Mesh3d(meshes.add(mesh)),
            MeshMaterial3d(materials.add(finish)),
            Transform::from_rotation(rock_attitude(entity)),
        ));
    }
}

/// Every `RockBody` the render world holds, with what the renderer knows.
type RockBodyQuery<'w, 's> = Query<
    'w,
    's,
    (
        &'static CoreEntity,
        &'static Mesh3d,
        Option<&'static ViewVisibility>,
    ),
    With<RockBody>,
>;

/// What the rock census counted, stage by stage.
///
/// #524 has been misdiagnosed three times because "rocks are not on screen"
/// is not one question. It is a chain — seeded, replicated into the client's
/// world, given a body, kept by the renderer's visibility pass, and finally
/// large enough to see — and each link fails differently. Every field here is
/// read from a *different* place, so the line says which link broke instead of
/// leaving the reader to guess:
///
/// * `in_state` comes from the session's executor,
/// * `drawn` from the `RockBody` entities themselves,
/// * `in_view` from Bevy's own [`ViewVisibility`], the value the render pass
///   used,
/// * `smallest_px` from the drawn body's own mesh geometry, projected through
///   the live camera height and the live window.
///
/// Two of those agreeing proves nothing; the point is that they can disagree.
#[derive(Debug, Clone, Copy, PartialEq)]
struct RockCensus {
    /// Live rocks in the session's state: Large, Medium, Small.
    in_state: [usize; 3],
    /// `RockBody` entities in the render world.
    drawn: usize,
    /// How many of those the renderer's visibility pass kept for the view.
    ///
    /// Zero in an app that runs no visibility pass, which is every headless
    /// test — the number means something only in a rendered run.
    in_view: usize,
    /// Chase-camera height when the census was taken, in metres.
    camera_height_m: f32,
    /// Window height the pixel figures are against, in physical pixels.
    viewport_px: f32,
    /// The smallest drawn body's diameter, in pixels.
    smallest_px: Option<f32>,
}

impl RockCensus {
    /// `rocks 1 L / 2 M / 3 S in state | 6 drawn | 6 in view | ...`.
    ///
    /// ASCII only: this is drawn in the F3 pane, and Bevy renders anything
    /// else as an empty box.
    fn line(&self) -> String {
        let tier_px = |radius_mm: i64| {
            apparent_diameter_px(
                radius_mm as f32 / 1_000.0,
                self.camera_height_m,
                self.viewport_px,
            )
        };
        let smallest = match self.smallest_px {
            Some(px) if px < MIN_LEGIBLE_DIAMETER_PX => {
                format!("{px:.1} px BELOW THE {MIN_LEGIBLE_DIAMETER_PX:.0} px FLOOR")
            }
            Some(px) => format!("{px:.1} px"),
            None => "none".to_owned(),
        };
        format!(
            "rocks {} L / {} M / {} S in state | {} drawn | {} in view | \
             tier px L {:.1} / M {:.1} / S {:.1} | smallest drawn {} | camera {:.0} m / {:.0} px",
            self.in_state[0],
            self.in_state[1],
            self.in_state[2],
            self.drawn,
            self.in_view,
            tier_px(RockTier::Large.limits().radius_mm),
            tier_px(RockTier::Medium.limits().radius_mm),
            tier_px(RockTier::Small.limits().radius_mm),
            smallest,
            self.camera_height_m,
            self.viewport_px,
        )
    }
}

/// Counts the rock census from the session, the render world and the meshes.
///
/// See [`RockCensus`] for why each number is read from its own place.
fn gather_rock_census(
    session: &ActiveSession,
    bodies: &RockBodyQuery,
    meshes: &Assets<Mesh>,
    camera_height_m: f32,
    viewport_px: f32,
) -> RockCensus {
    let mut in_state = [0usize; 3];
    for entity in session.executor().entities().copied() {
        if let Some(RegolithState::Rock(rock)) = session.executor().state(entity) {
            if rock.hull > 0 {
                in_state[match rock.tier {
                    RockTier::Large => 0,
                    RockTier::Medium => 1,
                    RockTier::Small => 2,
                }] += 1;
            }
        }
    }
    let mut drawn = 0usize;
    let mut in_view = 0usize;
    let mut smallest_px: Option<f32> = None;
    for (_, mesh, visibility) in bodies.iter() {
        drawn += 1;
        if visibility.is_some_and(|seen| seen.get()) {
            in_view += 1;
        }
        // The mesh the renderer was handed, not the radius the spawn path
        // meant to use: a body scaled or meshed wrong has to be able to show
        // up here as a different number.
        let Some(aabb) = meshes
            .get(mesh)
            .and_then(bevy::camera::primitives::MeshAabb::compute_aabb)
        else {
            continue;
        };
        let radius_m = aabb.half_extents.max_element();
        let px = apparent_diameter_px(radius_m, camera_height_m, viewport_px);
        smallest_px = Some(smallest_px.map_or(px, |seen: f32| seen.min(px)));
    }
    RockCensus {
        in_state,
        drawn,
        in_view,
        camera_height_m,
        viewport_px,
        smallest_px,
    }
}

/// The primary window's height in physical pixels, or the reference height.
fn viewport_height_px(windows: &Query<&Window, With<PrimaryWindow>>) -> f32 {
    windows
        .single()
        .map_or(REFERENCE_VIEWPORT_PX, |window| {
            window.resolution.physical_height() as f32
        })
        .max(1.0)
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

/// The window height every published pixel figure is quoted against.
///
/// A pixel measurement is meaningless without the window it was taken in, and
/// a live capture reads the real window instead. This is only the number the
/// documented figures and the tests are stated at.
pub const REFERENCE_VIEWPORT_PX: f32 = 1080.0;

/// How many pixels one world metre spans on the deck plane.
///
/// The camera looks straight down, so the deck plane is exactly `height_m`
/// away and the whole visible world height is `2 * visible_half_height_m`.
/// Everything on that plane therefore shares one scale, which is what makes a
/// single number meaningful:
///
/// ```text
/// pixels_per_metre = viewport_px / (2 * tan(fov/2) * height_m)
/// ```
///
/// **This is the arithmetic #517, #524 and #536 kept getting caught by.** A
/// body can carry the right colour, the right opacity and the right position
/// and still be a fraction of a pixel across, which no assertion about state
/// can see. Measuring the extent is what makes that case fail loudly.
#[must_use]
pub fn pixels_per_metre(height_m: f32, viewport_px: f32) -> f32 {
    let visible_m = 2.0 * visible_half_height_m(height_m);
    if visible_m <= 0.0 {
        return 0.0;
    }
    viewport_px / visible_m
}

/// The on-screen diameter, in pixels, of a body of `radius_m` on the deck.
#[must_use]
pub fn apparent_diameter_px(radius_m: f32, height_m: f32, viewport_px: f32) -> f32 {
    2.0 * radius_m * pixels_per_metre(height_m, viewport_px)
}

/// The smallest drawn diameter that still reads as an object on screen.
///
/// Four pixels is deliberately a floor on *legibility*, not on rendering: one
/// pixel renders and cannot be recognised, and #536's marker ring nearly
/// shipped at a sub-pixel tube — correct colour, correct position, nothing a
/// player could see. Anything the skin draws as a body the player is expected
/// to notice must clear this at the far end of the zoom range.
pub const MIN_LEGIBLE_DIAMETER_PX: f32 = 4.0;

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
/// The reference length is the weapon envelope the rings draw: the widest
/// optimal in the table is Heavy's 300 m, with 60 m of falloff past it
/// (Stock is 240 + 80 since #545), and a chassis hull is ~7 m (`craft::hull_length`
/// times [`craft::CRAFT_DISPLAY_SCALE`], so ~22 m on screen). With a
/// [`CAMERA_FOV_Y`] of 45 degrees the visible half-height is `0.414 * height`:
///
/// * [`CAMERA_MIN_HEIGHT_M`] = 150 m shows +/- 62 m — a 22 m hull is about a
///   sixth of the screen height, close enough to read facing and the arc
///   marking, and the closest useful framing before the ship fills the view.
/// * [`CAMERA_DEFAULT_HEIGHT_M`] = 900 m shows +/- 373 m, so the widest ring
///   the table draws — Heavy's 360 m envelope — fits. This is the framing the
///   weapon is fought at.
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

fn open_overlay_if_asked(asked: Option<Res<OverlayOpen>>, mut state: ResMut<OverlayState>) {
    if asked.is_some() {
        state.expanded = true;
    }
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

#[allow(clippy::too_many_arguments)]
fn refresh_f3_pane(
    state: Res<OverlayState>,
    metrics: Res<OverlayMetrics>,
    view: Res<CombatView>,
    broken: Res<LockBreak>,
    tracks: Res<ProjectileTracks>,
    session: Res<ActiveSession>,
    rock_bodies: RockBodyQuery,
    meshes: Res<Assets<Mesh>>,
    windows: Query<&Window, With<PrimaryWindow>>,
    zoom: Res<CameraZoom>,
    roster: Res<roster::ShipRoster>,
    fade_census: Res<aoi::AoiFadeCensus>,
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
        let rocks = gather_rock_census(
            &session,
            &rock_bodies,
            &meshes,
            zoom.height_m(),
            viewport_height_px(&windows),
        );
        text.push('\n');
        text.push_str(&rocks.line());
        text.push('\n');
        text.push_str(&fade_census.line(session.aoi_edge_m()));
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
    use crate::session::ConfiguredImpairment;
    use bevy::asset::AssetPlugin;
    use orrery_core::state_hash;
    use orrery_games::scenario::{Entry, Play, Scenario, TickRecord};
    use std::io::{Read as _, Write as _};
    use std::net::TcpListener;
    use std::sync::mpsc::{self, Receiver};
    use std::time::Duration;

    struct UploadEndpoint {
        origin: String,
        received: Receiver<Vec<u8>>,
    }

    fn upload_endpoint() -> UploadEndpoint {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind upload service");
        let address = listener.local_addr().expect("upload service address");
        let (sent, received) = mpsc::channel();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept upload");
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .expect("set upload read timeout");
            let mut request = Vec::new();
            let mut buffer = [0_u8; 1024];
            let body_start = loop {
                let count = stream.read(&mut buffer).expect("read upload headers");
                assert_ne!(count, 0, "client closed upload before its headers");
                request.extend_from_slice(&buffer[..count]);
                if let Some(end) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
                    break end + 4;
                }
            };
            let headers = std::str::from_utf8(&request[..body_start]).expect("ASCII headers");
            let length = headers
                .lines()
                .find_map(|line| {
                    line.split_once(':').and_then(|(name, value)| {
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim())
                    })
                })
                .expect("content length")
                .parse::<usize>()
                .expect("numeric content length");
            while request.len() < body_start + length {
                let count = stream.read(&mut buffer).expect("read upload body");
                assert_ne!(count, 0, "client closed upload before its body");
                request.extend_from_slice(&buffer[..count]);
            }
            sent.send(request).expect("return upload request");
            stream
                .write_all(
                    b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .expect("acknowledge upload");
        });
        UploadEndpoint {
            origin: format!("http://{address}"),
            received,
        }
    }

    fn finish_campaign(mut exit: MessageWriter<AppExit>) {
        exit.write(AppExit::Success);
    }

    #[test]
    fn a_finished_campaign_session_reaches_the_service() {
        let temporary = tempfile::tempdir().expect("temporary client state");
        let telemetry_path = temporary.path().join("telemetry.jsonl");
        std::fs::write(&telemetry_path, "{\"kind\":\"overlay\"}\n").expect("write telemetry");
        let endpoint = upload_endpoint();
        let session_id = "01917f0e-2b9a-7c4d-8f21-6a0b3c9d1e2f".to_owned();
        let config = campaign::CampaignConfig {
            host_node_hex: String::new(),
            host_direct: None,
            slot: 0,
            own_label: Some("ada".to_owned()),
            session_id: session_id.clone(),
            session_token_hex: None,
            wall_start_utc: "2026-08-30T12:00:00Z".to_owned(),
            configured: ConfiguredImpairment {
                loss_pct: 0.0,
                jitter_p50_ms: 0,
                jitter_p99_ms: 0,
            },
            transport_secret: iroh_base::SecretKey::generate(),
            island_seats: Some(1),
            roster_url: None,
        };
        let runtime = campaign::CampaignRuntime::finished_for_test(config, SEED);
        let mut app = App::new();
        app.insert_resource(ActiveSession::Campaign(Box::new(runtime)))
            .insert_resource(OverlayMetrics::new(telemetry_path.clone()))
            .insert_resource(admission::UploadManager::for_test(
                endpoint.origin.clone(),
                &telemetry_path,
            ));
        install_campaign_finalization(&mut app);
        app.add_systems(Update, finish_campaign.after(CampaignFinalization));
        app.update();

        let request = endpoint
            .received
            .recv_timeout(Duration::from_secs(5))
            .expect("finished session reaches service");
        let body_start = request
            .windows(4)
            .position(|bytes| bytes == b"\r\n\r\n")
            .expect("request headers")
            + 4;
        let upload: serde_json::Value =
            serde_json::from_slice(&request[body_start..]).expect("valid upload JSON");
        assert_eq!(upload["records"][0]["session_id"], session_id);
        assert_eq!(upload["telemetry_jsonl"], "{\"kind\":\"overlay\"}\n");
    }

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

    /// The three tiers must not be confusable at a glance — and, since #530,
    /// no tier may be *the dark one*.
    ///
    /// The old ramp ran from 0.40 to 0.72 sRGB against size, which handed the
    /// 40 m tier the lowest contrast in the scene. That tier is the one you
    /// most need to see early, because a collision now applies real mutual
    /// force (#514). The tilt survives; the floor and the compression are what
    /// this pins.
    #[test]
    fn the_rock_tiers_are_distinguishable_by_more_than_size() {
        let finishes = [
            rock_finish(RockTier::Large),
            rock_finish(RockTier::Medium),
            rock_finish(RockTier::Small),
        ];
        // Rec. 709 luma over linear components: how light the surface is
        // before the scene light takes anything off it.
        let luma = |colour: Color| {
            let rgba = colour.to_linear();
            0.2126 * rgba.red + 0.7152 * rgba.green + 0.0722 * rgba.blue
        };
        let lumas = finishes.map(|(colour, _)| luma(colour));

        // The floor is what #530 is about, and it is tier-independent.
        for (tier, luma) in ["large", "medium", "small"].iter().zip(lumas) {
            assert!(
                luma >= ROCK_MIN_TINT_LUMA,
                "the {tier} tier must clear the contrast floor; {luma} < {ROCK_MIN_TINT_LUMA}"
            );
        }
        // A rock is a lit body, so its tint is a ceiling on how bright it can
        // ever render. Every tier must start above the starfield greys it is
        // seen against, or the largest rock reads as a hole in the field.
        let brightest_star = Color::srgb(
            starfield::STAR_LAYERS[0].grey,
            starfield::STAR_LAYERS[0].grey,
            starfield::STAR_LAYERS[0].grey,
        );
        assert!(
            ROCK_MIN_TINT_LUMA < luma(brightest_star),
            "the floor is set below the brightest star layer on purpose; \
             a rock is lit and a star is not"
        );

        // The ramp is a nuance, not a hierarchy.
        let ratio = lumas[2] / lumas[0];
        assert!(
            ratio <= ROCK_MAX_TINT_LUMA_RATIO,
            "the tier ramp must stay inside {ROCK_MAX_TINT_LUMA_RATIO}:1; got {ratio}"
        );
        assert!(
            lumas[0] < lumas[1] && lumas[1] < lumas[2],
            "the tilt still favours the smallest tier, which is the hardest to notice"
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

    /// #524, the last stage of it: a rock that is seeded, replicated, in the
    /// client's world and given a body can still be invisible, because a body
    /// can be a fraction of a pixel across.
    ///
    /// So this measures the mesh the spawn path actually handed the renderer,
    /// not the radius it meant to use, and converts it to pixels through the
    /// same arithmetic the live census uses. The three earlier misdiagnoses of
    /// #524 were all "the value is right" claims; the value being right is
    /// exactly what none of them could distinguish.
    ///
    /// Measured live on 2026-08-27 in a joined campaign session, 1280x720
    /// window: `rocks 1 L / 2 M / 3 S in state | 6 drawn | 6 in view |
    /// tier px L 17.4 / M 8.7 / S 3.5` at 4000 m, and `L 463.5` at 150 m.
    #[test]
    fn every_rock_tier_clears_the_legibility_floor_at_full_zoom_out() {
        use bevy::camera::primitives::MeshAabb;
        use orrery_core::{QPos, QVel};
        use orrery_games::regolith::state::Rock;

        let tiers = [RockTier::Large, RockTier::Medium, RockTier::Small];
        let mut local = LocalSession::default();
        for (index, tier) in tiers.iter().enumerate() {
            local.executor.insert(
                PersistId::new(70 + index as u64),
                RegolithState::Rock(Rock::spawned(
                    *tier,
                    0,
                    QPos::from_metres(400.0 * index as f64, 0.0, 0.0),
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

        // The drawn radius, taken from the mesh the renderer was handed.
        let handles: Vec<(PersistId, Mesh3d)> = app
            .world_mut()
            .query_filtered::<(&CoreEntity, &Mesh3d), With<RockBody>>()
            .iter(app.world())
            .map(|(core, mesh)| (core.0, mesh.clone()))
            .collect();
        let meshes = app.world().resource::<Assets<Mesh>>();
        let mut drawn: Vec<(PersistId, f32)> = handles
            .iter()
            .map(|(entity, mesh)| {
                let aabb = meshes
                    .get(mesh)
                    .and_then(MeshAabb::compute_aabb)
                    .expect("a rock body carries a mesh with vertices");
                (*entity, aabb.half_extents.max_element())
            })
            .collect();
        drawn.sort_by_key(|(entity, _)| entity.0);
        assert_eq!(drawn.len(), 3, "one body per tier: {drawn:?}");

        for (index, (entity, radius_m)) in drawn.iter().enumerate() {
            let ruleset_m = tiers[index].limits().radius_mm as f32 / 1_000.0;
            assert!(
                (radius_m - ruleset_m).abs() < 0.5,
                "rock {entity:?} is drawn at {radius_m} m against the ruleset's {ruleset_m} m: \
                 the skin may not restate the size the ruleset published"
            );
        }

        // Full zoom-out, the case the tiers have to survive.
        let far_px: Vec<f32> = drawn
            .iter()
            .map(|(_, radius_m)| {
                apparent_diameter_px(*radius_m, CAMERA_MAX_HEIGHT_M, REFERENCE_VIEWPORT_PX)
            })
            .collect();
        for (tier, px) in tiers.iter().zip(&far_px) {
            assert!(
                *px >= MIN_LEGIBLE_DIAMETER_PX,
                "the {tier:?} tier is {px} px across at {CAMERA_MAX_HEIGHT_M} m of camera \
                 height on a {REFERENCE_VIEWPORT_PX}-line window, under the \
                 {MIN_LEGIBLE_DIAMETER_PX} px floor"
            );
        }
        // Size is what carries the tier at the zoom where colour cannot, so
        // the tiers have to separate by more than antialiasing.
        assert!(
            far_px[0] >= far_px[1] * 1.8 && far_px[1] >= far_px[2] * 1.8,
            "the tiers must stay distinguishable by size alone: {far_px:?} px"
        );

        // Full zoom-in: the largest rock must still fit in the frame, or a
        // player flying into one sees an untextured wall instead of a rock.
        let near_px = apparent_diameter_px(drawn[0].1, CAMERA_MIN_HEIGHT_M, REFERENCE_VIEWPORT_PX);
        assert!(
            near_px > REFERENCE_VIEWPORT_PX * 0.25 && near_px < REFERENCE_VIEWPORT_PX,
            "a 40 m rock is {near_px} px across at {CAMERA_MIN_HEIGHT_M} m: it must read as a \
             body, and it must not fill the window"
        );

        // The window height below which the smallest tier stops clearing the
        // floor at full zoom-out. A live 1280x720 capture sits under it, at
        // 3.5 px, which is why the census says so out loud rather than
        // leaving the reader to work it out.
        let smallest_radius_m = drawn[2].1;
        let floor_lines =
            MIN_LEGIBLE_DIAMETER_PX * 2.0 * visible_half_height_m(CAMERA_MAX_HEIGHT_M)
                / (2.0 * smallest_radius_m);
        assert!(
            (820.0..=840.0).contains(&floor_lines),
            "the smallest tier clears {MIN_LEGIBLE_DIAMETER_PX} px only above {floor_lines} \
             lines of window at full zoom-out"
        );
    }

    /// The census has to be able to say *which* stage lost the rocks, and it
    /// has to say it in ASCII, because Bevy draws anything else as a box.
    ///
    /// #524 was diagnosed wrongly three times off a line that could only count
    /// state and bodies. This asserts the two numbers a body cannot fake: the
    /// renderer's own visibility verdict, and the drawn extent in pixels,
    /// including the floor warning that fires when a body is on screen and too
    /// small to see.
    #[test]
    fn the_rock_census_names_the_stage_and_stays_ascii() {
        use orrery_core::{QPos, QVel};
        use orrery_games::regolith::state::Rock;

        #[derive(Resource, Default)]
        struct Captured(String);

        fn capture(
            session: Res<ActiveSession>,
            bodies: RockBodyQuery,
            meshes: Res<Assets<Mesh>>,
            mut out: ResMut<Captured>,
        ) {
            out.0 = gather_rock_census(
                &session,
                &bodies,
                &meshes,
                CAMERA_MAX_HEIGHT_M,
                REFERENCE_VIEWPORT_PX,
            )
            .line();
        }

        let mut local = LocalSession::default();
        local.executor.insert(
            PersistId::new(80),
            RegolithState::Rock(Rock::spawned(
                RockTier::Small,
                0,
                QPos::from_metres(0.0, 0.0, 0.0),
                QVel::default(),
            )),
        );
        let mut app = App::new();
        app.add_plugins(AssetPlugin::default())
            .init_asset::<Mesh>()
            .init_asset::<StandardMaterial>()
            .init_resource::<Captured>()
            .insert_resource(ActiveSession::Local(Box::new(local)))
            .add_systems(Update, (ensure_rock_bodies, capture).chain());
        app.update();

        let line = app.world().resource::<Captured>().0.clone();
        assert!(line.is_ascii(), "the F3 pane is ASCII only: {line:?}");
        assert!(
            line.contains("0 L / 0 M / 1 S in state | 1 drawn"),
            "the census must count state and bodies separately: {line}"
        );
        assert!(
            line.contains("| 0 in view"),
            "a world with no visibility pass has drawn a body nothing has seen: {line}"
        );
        assert!(
            line.contains("smallest drawn 5.2 px"),
            "the census must measure the drawn body, not restate the tier: {line}"
        );

        // The same body on a window too short to carry it has to say so.
        let cramped = RockCensus {
            in_state: [0, 0, 1],
            drawn: 1,
            in_view: 1,
            camera_height_m: CAMERA_MAX_HEIGHT_M,
            viewport_px: 720.0,
            smallest_px: Some(3.5),
        };
        assert!(
            cramped.line().contains("BELOW THE 4 px FLOOR"),
            "a body under the legibility floor must be named, not merely counted: {}",
            cramped.line()
        );
    }

    /// #530: the tier you most need to see early is the 40 m one, because a
    /// collision now applies real mutual force (#514) — and the ramp made it
    /// the darkest thing in the scene.
    ///
    /// The tilt survives, so the largest tier still has the lowest *tint*.
    /// What this pins is that the tilt cannot make it the least noticeable
    /// body, because noticing is contrast times area and the ramp is a nuance
    /// while the size difference is a factor of five.
    ///
    /// Measured live on 2026-08-27 out of a captured frame at 4000 m of camera
    /// height: the Large tier rendered at 8.28:1 against the field over 208
    /// px2, the Medium at 8.85:1 over 49 px2, the Small at 4.76:1 over 9 px2.
    /// The rendered numbers are the ones that matter — a rock is lit, so its
    /// tint is only a ceiling — and they run the right way.
    #[test]
    fn no_rock_tier_is_the_least_noticeable_body_in_the_scene() {
        // WCAG relative luminance and contrast ratio, so the figures here are
        // comparable with the ones measured out of a captured frame.
        let luminance = |colour: Color| {
            let rgba = colour.to_linear();
            0.2126 * rgba.red + 0.7152 * rgba.green + 0.0722 * rgba.blue
        };
        let contrast = |a: f32, b: f32| (a.max(b) + 0.05) / (a.min(b) + 0.05);

        let tiers = [RockTier::Large, RockTier::Medium, RockTier::Small];
        // The deck has no fill of its own: what a rock is seen against is the
        // near-black clear colour, sparsely stippled with the starfield.
        let field = 0.0_f32;

        let mut weight = Vec::new();
        for tier in tiers {
            let (tint, _) = rock_finish(tier);
            let ratio = contrast(luminance(tint), field);
            assert!(
                ratio >= 4.5,
                "the {tier:?} tier renders at {ratio}:1 against the field, under the 4.5:1 \
                 floor a body the player must not fly into has to clear"
            );
            let diameter_px = apparent_diameter_px(
                tier.limits().radius_mm as f32 / 1_000.0,
                CAMERA_MAX_HEIGHT_M,
                REFERENCE_VIEWPORT_PX,
            );
            // Area times contrast: how much of the frame is arguing for this
            // body's presence at the zoom where it is hardest to see.
            weight.push((tier, ratio, diameter_px * diameter_px * ratio));
        }

        assert!(
            weight[0].2 > weight[1].2 && weight[1].2 > weight[2].2,
            "noticeability must run with size at full zoom-out, or the tier that can kill \
             you is the one you see last: {weight:?}"
        );
        // And the ramp itself stays a nuance rather than a hierarchy: no tier
        // may be more than this much better lit than another.
        let spread = weight[2].1 / weight[0].1;
        assert!(
            spread <= 1.5,
            "the contrast ramp runs {spread}:1 across the tiers, which is a hierarchy again"
        );

        // The starfield is drawn unlit, so its greys are a floor a lit rock's
        // tint has to clear before the scene light takes anything off it.
        let deepest_star = starfield::STAR_LAYERS[starfield::STAR_LAYERS.len() - 1].grey;
        for tier in tiers {
            let (tint, _) = rock_finish(tier);
            assert!(
                luminance(tint) > luminance(Color::srgb(deepest_star, deepest_star, deepest_star)),
                "the {tier:?} tier must start lighter than the deepest starfield layer"
            );
        }
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
        roster.accept(
            &RosterResponse {
                roster: vec![RosterRow::labelled(8, "ada")],
                ..Default::default()
            },
            None,
        );
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

    /// #575: the waiting room appears only where the host described one, and
    /// it gets out of the way once the attempt is running.
    #[test]
    fn the_waiting_room_is_drawn_only_while_the_host_says_there_is_one() {
        use crate::lobby::{LobbyPhase, LobbyView, Seat, SeatKind, SeatState};

        let room = LobbyView {
            seats: vec![
                Seat {
                    slot: 4,
                    kind: SeatKind::Human,
                    state: SeatState::Connected,
                    nickname: Some("ada".to_owned()),
                },
                Seat {
                    slot: 5,
                    kind: SeatKind::Human,
                    state: SeatState::Empty,
                    nickname: None,
                },
            ],
            phase: Some(LobbyPhase::Lobby),
            starts_in_s: None,
            own_slot: Some(4),
            notice: None,
        };
        assert!(lobby_panel_visible(
            SessionPresentation::Dialing,
            &room,
            false
        ));
        assert!(lobby_panel_visible(SessionPresentation::Live, &room, false));

        // The attempt started: the seat map's job is done, the ships are the
        // seat map now.
        let running = LobbyView {
            phase: Some(LobbyPhase::Running),
            ..room.clone()
        };
        assert!(!lobby_panel_visible(
            SessionPresentation::Live,
            &running,
            false
        ));

        // A service older than #573 describes no room, so none is drawn — not
        // one assembled from whatever craft labels arrived.
        let legacy = LobbyView {
            phase: None,
            ..room.clone()
        };
        assert!(!lobby_panel_visible(
            SessionPresentation::Live,
            &legacy,
            false
        ));
        assert!(!lobby_panel_visible(
            SessionPresentation::Dialing,
            &legacy,
            false
        ));
        // Unless there is something to tell the player about their own join.
        assert!(lobby_panel_visible(
            SessionPresentation::Refused,
            &legacy,
            true
        ));

        // The offline sandbox has no host and therefore no room.
        assert!(!lobby_panel_visible(
            SessionPresentation::Local,
            &room,
            true
        ));
    }

    /// The fit proof at the default window Bevy opens, 1280x720.
    ///
    /// #552 records that 720 lines is already tight, so this is numeric rather
    /// than a look, exactly as `legend::legend_fits_the_default_720_line_window`
    /// is: a full eight-seat room, with the longest names admission will hand
    /// over and a refusal sentence under it, measured against the window and
    /// against the controls legend it must not reach.
    #[test]
    fn the_waiting_room_fits_the_default_720_line_window() {
        use crate::legend;
        use crate::lobby::{LobbyPhase, LobbyView, Seat, SeatKind, SeatState};

        const WINDOW_W: f32 = 1280.0;
        const WINDOW_H: f32 = 720.0;
        // `legend::spawn_legend` puts its panel in the bottom-right corner.
        let legend_top = WINDOW_H - legend::MARGIN_PX - legend::height_px();

        let longest = "x".repeat(crate::roster::NICKNAME_MAX_CHARS);
        let room = LobbyView {
            seats: (0..8)
                .map(|slot| Seat {
                    slot,
                    kind: if slot < 4 {
                        SeatKind::Bot
                    } else {
                        SeatKind::Human
                    },
                    state: SeatState::Reserved,
                    nickname: Some(longest.clone()),
                })
                .collect(),
            phase: Some(LobbyPhase::Restarting),
            starts_in_s: Some(90),
            own_slot: Some(4),
            notice: Some(lobby::refusal_sentence(
                Some("campaign_full"),
                Some("All 4 player seats are full; try the next lobby."),
                Some(126),
            )),
        };

        let lines = room.lines();
        // Heading, phase, occupancy, countdown, eight seats, notice.
        assert_eq!(lines.len(), 13, "{lines:?}");
        assert!(
            lines.iter().all(|line| line.is_ascii()),
            "Bevy's built-in face draws boxes for anything else: {lines:?}"
        );

        let widest = lines
            .iter()
            .map(|line| legend::text_width_px(line, LOBBY_ROW_FONT_PX))
            .fold(0.0_f32, f32::max);
        assert!(
            widest < WINDOW_W,
            "the widest waiting-room line measures {widest:.1} px in a {WINDOW_W:.0} px window"
        );

        let height = lines.len() as f32 * LOBBY_ROW_FONT_PX * legend::LINE_HEIGHT_RATIO
            + 2.0 * legend::PADDING_PX;
        let bottom = LOBBY_PANEL_TOP_PX + height;
        assert!(
            bottom < legend_top,
            "the waiting room ends at {bottom:.1} px and the controls legend starts at \
             {legend_top:.1} px: a first-time player must be able to read both at once"
        );
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
