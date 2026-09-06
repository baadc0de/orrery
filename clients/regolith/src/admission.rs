//! Campaign discovery, admission UI, and durable client-evidence upload.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Mutex};
use std::time::{Duration, Instant};

use bevy::input_focus::AutoFocus;
use bevy::prelude::*;
use bevy::text::{EditableText, TextCursorStyle};
use bevy::ui::InteractionDisabled;
use bevy::ui_widgets::{Activate, Button, ScrollArea};
use orrery_games::regolith::REGOLITH_RULESET;
use orrery_protocol::CampaignJoinFileV1;
use serde::{Deserialize, Serialize};

use crate::campaign::{CampaignConfig, CampaignRuntime};
use crate::session::{ConfiguredImpairment, SessionRecord, CONSENT_NOTICE};
use crate::{ActiveSession, BUILD_REV, DEFAULT_ADMISSION_URL};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(45);
/// The whole exit path's upload budget, across every body a seat owes.
///
/// A quit is not the moment to spend [`REQUEST_TIMEOUT`] per body on the main
/// thread. Since #1048 a seat owes one body per five-minute increment, so an
/// hour-long seat against an origin that has gone away spent 13 x 45 s = ~9m45s
/// in a frozen window, which a tester reports as a crash (#1145). The exit
/// attempt has always been a courtesy -- every body is durable on disk and
/// swept at the next launch (#1051) -- so it is bounded here and the rest is
/// the next launch's job, exactly as older bodies already were.
const EXIT_UPLOAD_BUDGET: Duration = Duration::from_secs(5);
/// How many launches a `404 unknown_session` is re-attempted before retiring.
///
/// A 404 is the one refusal the client cannot classify from the status alone,
/// so it is bounded rather than called permanent or transient. See
/// [`UploadRefusal::Unknown`].
const UNKNOWN_SESSION_ATTEMPTS: u32 = 5;
const PANEL: Color = Color::srgb(0.035, 0.05, 0.075);
const ROW: Color = Color::srgb(0.08, 0.11, 0.16);
const ACTIVE: Color = Color::srgb(0.10, 0.35, 0.48);
const DIM: Color = Color::srgb(0.48, 0.53, 0.60);

/// Resolve the service origin as flag, environment, then baked default.
#[must_use]
pub fn resolve_admission_url(args: &[OsString], environment: Option<String>) -> String {
    flag_value(args, "--admission-url")
        .or(environment)
        .unwrap_or_else(|| DEFAULT_ADMISSION_URL.to_owned())
        .trim_end_matches('/')
        .to_owned()
}

fn flag_value(args: &[OsString], flag: &str) -> Option<String> {
    args.windows(2)
        .find(|pair| pair[0] == flag)
        .map(|pair| pair[1].to_string_lossy().into_owned())
}

/// One campaign returned by `GET /v1/campaigns`.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct CampaignListing {
    /// Stable service-side campaign identifier.
    pub id: String,
    /// Volunteer-facing title.
    pub title: String,
    /// `open`, `busy`, `closed`, or `paused`.
    pub state: String,
    /// Host peer count; human seats extend it rather than consuming it.
    pub peers: usize,
    /// How many of the island's seats are for people, when the service says.
    ///
    /// Absent from every service older than #573, which ran one human in the
    /// seat after the bots. `peers + humans` is the island size the host
    /// computes every spawn pose against, so this is what stops the client
    /// guessing that number (`docs/plans/multi-human-campaign.md` §3.2).
    #[serde(default)]
    pub humans: Option<u16>,
    /// How many human seats are still reservable, when the service says.
    #[serde(default)]
    pub slots_free: Option<u16>,
    /// Planned session duration.
    pub seconds: u64,
    /// Configured packet loss percentage.
    pub loss_pct: u64,
    /// Configured jitter in milliseconds.
    pub jitter_ms: u64,
    /// Required client revision, when pinned.
    pub client_rev: Option<String>,
    /// Service alias for the required client revision.
    #[serde(default)]
    pub server_rev: Option<String>,
}

impl CampaignListing {
    /// The island size this campaign runs, when the service published enough
    /// to say. `None` leaves the pre-#573 derivation in place rather than
    /// guessing a number the host never stated.
    #[must_use]
    pub fn island_seats(&self) -> Option<u16> {
        let peers = u16::try_from(self.peers).ok()?;
        peers.checked_add(self.humans?)
    }
}

#[derive(Debug, Clone, Deserialize)]
struct CampaignsResponse {
    campaigns: Vec<CampaignListing>,
    operator_note: Option<String>,
}

/// The seat material an admission service grants, as the join artifact stores it.
///
/// Public so the read-only-launch-directory proof in
/// `tests/read_only_launch_directory.rs` can drive the real
/// [`write_join_artifact`] rather than a copy of it.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct JoinObject {
    /// Hex node id of the hosting process.
    pub host_node: String,
    /// The swarm slot this seat occupies.
    pub slot: usize,
    /// Coordinator-issued session identity.
    pub session_id: String,
    /// Hex-encoded session token presented at join.
    pub session_token: String,
}

#[derive(Debug, Clone, Deserialize)]
struct JoinResponse {
    join: JoinObject,
    host_direct: String,
    /// The display label admission granted with this seat. Older services did
    /// not echo it; absence must stay absence rather than being backfilled from
    /// the public roster.
    #[serde(default)]
    nickname: Option<String>,
    configured: ConfiguredResponse,
}

#[derive(Debug, Clone, Copy, Deserialize)]
struct ConfiguredResponse {
    loss_pct: f64,
    jitter_p50_ms: u64,
    jitter_p99_ms: u64,
}

/// A refusal body: `{"error": code, "detail": text, ...}`.
///
/// `error` is the machine-readable condition #573 names (`campaign_full`,
/// `session_started`, `host_failed`); `detail` is the service's own sentence;
/// `retry_after_s` is the wait it computed. All three go to
/// [`crate::lobby::refusal_sentence`], which is the only place that decides
/// what the player reads.
#[derive(Debug, Deserialize)]
struct ErrorResponse {
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    detail: Option<String>,
    #[serde(default)]
    retry_after_s: Option<u64>,
}

#[derive(Debug)]
struct AdmissionRefusal {
    code: Option<String>,
    detail: String,
    retry_after_s: Option<u64>,
}

impl AdmissionRefusal {
    fn sentence(&self) -> String {
        crate::lobby::refusal_sentence(self.code.as_deref(), Some(&self.detail), self.retry_after_s)
    }
}

/// Result of the non-interactive admission path.
#[derive(Debug)]
pub enum HeadlessAdmission {
    /// Admission minted a seat and returned the launch material.
    Admitted(Box<CampaignConfig>),
    /// Admission returned the exact refusal the caller asked to probe.
    ExpectedRefusal,
}

/// The boot gate states from the accepted campaign-admission design.
#[derive(Debug, Clone, Resource)]
pub enum JoinGate {
    /// A listing request is in flight.
    FetchingCampaigns,
    /// The live listing is visible. An empty vector is an ordinary quiet period.
    Browsing {
        /// Campaign rows returned by the live service.
        campaigns: Vec<CampaignListing>,
        /// Optional operator note from the service.
        operator_note: Option<String>,
        /// A join refusal dialog, dismissed back to this same listing.
        dialog: Option<String>,
    },
    /// A campaign was selected and the nickname/consent form is visible.
    NicknameEntry {
        /// Selected open campaign.
        campaign: CampaignListing,
        /// Current editable nickname.
        nickname: String,
        /// Whether the volunteer accepted the recording notice.
        consented: bool,
    },
    /// A join request is in flight.
    Admitting {
        /// Campaign being started.
        campaign: CampaignListing,
        /// Nickname sent to the service.
        nickname: String,
    },
    /// The permanent service could not be reached.
    Unreachable {
        /// Transport or HTTP failure shown to the volunteer.
        detail: String,
    },
}

#[derive(Resource)]
struct AdmissionSettings {
    origin: String,
    telemetry_path: PathBuf,
    transport_secret: iroh_base::SecretKey,
}

#[derive(Resource, Default)]
struct AdmissionTask(Mutex<Option<mpsc::Receiver<WorkerReply>>>);

#[derive(Resource)]
struct UiDirty(bool);

#[derive(Resource, Default)]
struct CampaignCatalog {
    campaigns: Vec<CampaignListing>,
    operator_note: Option<String>,
}

enum WorkerReply {
    Campaigns(Result<CampaignsResponse, String>),
    Joined(Result<JoinResponse, String>),
}

#[derive(Component)]
struct JoinUiRoot;

#[derive(Component)]
struct CampaignChoice(CampaignListing);

#[derive(Component)]
struct NicknameEditor;

#[derive(Component)]
struct Retry;

#[derive(Component)]
struct Back;

#[derive(Component)]
struct Consent;

#[derive(Component)]
struct SubmitJoin;

#[derive(Component)]
struct DismissDialog;

/// Installs the no-argument campaign boot experience.
pub struct AdmissionPlugin {
    origin: String,
    telemetry_path: PathBuf,
    transport_secret: iroh_base::SecretKey,
}

impl AdmissionPlugin {
    /// Configure the admission origin and the session telemetry path.
    #[must_use]
    pub fn new(
        origin: String,
        telemetry_path: PathBuf,
        transport_secret: iroh_base::SecretKey,
    ) -> Self {
        Self {
            origin,
            telemetry_path,
            transport_secret,
        }
    }
}

impl Plugin for AdmissionPlugin {
    fn build(&self, app: &mut App) {
        app.insert_resource(AdmissionSettings {
            origin: self.origin.clone(),
            telemetry_path: self.telemetry_path.clone(),
            transport_secret: self.transport_secret.clone(),
        })
        .insert_resource(JoinGate::FetchingCampaigns)
        .insert_resource(AdmissionTask::default())
        .insert_resource(CampaignCatalog::default())
        .insert_resource(UiDirty(true))
        .add_systems(Startup, begin_fetch)
        // `poll_worker` removes `JoinGate` the moment a join succeeds, and the
        // two UI systems are chained behind it *in the same tick*. A
        // `run_if(resource_exists)` on the chain does not help: the condition
        // is evaluated once, before `poll_worker` deletes the resource, so all
        // three still run. The systems therefore take the gate optionally and
        // return when it is gone (#491).
        .add_systems(Update, (poll_worker, sync_nickname, rebuild_ui).chain())
        .add_observer(choose_campaign)
        .add_observer(retry_fetch)
        .add_observer(go_back)
        .add_observer(toggle_consent)
        .add_observer(submit_join)
        .add_observer(dismiss_dialog);
    }
}

fn begin_fetch(settings: Res<AdmissionSettings>, task: Res<AdmissionTask>) {
    start_fetch(&settings.origin, &task);
}

fn start_fetch(origin: &str, task: &AdmissionTask) {
    let (sender, receiver) = mpsc::channel();
    let url = format!("{origin}/v1/campaigns");
    std::thread::spawn(move || {
        let answer = get_campaigns(&url);
        let _ = sender.send(WorkerReply::Campaigns(answer));
    });
    *task.0.lock().expect("admission task lock") = Some(receiver);
}

fn start_join(
    origin: &str,
    campaign: &CampaignListing,
    nickname: &str,
    transport_secret: &iroh_base::SecretKey,
    task: &AdmissionTask,
) {
    let (sender, receiver) = mpsc::channel();
    let url = format!("{origin}/v1/campaigns/{}/join", campaign.id);
    let node = transport_secret.public().to_string();
    let nickname = nickname.to_owned();
    std::thread::spawn(move || {
        let answer = post_join(&url, &nickname, &node);
        let _ = sender.send(WorkerReply::Joined(answer));
    });
    *task.0.lock().expect("admission task lock") = Some(receiver);
}

fn client(timeout: Duration) -> Result<reqwest::blocking::Client, String> {
    reqwest::blocking::Client::builder()
        .timeout(timeout)
        .build()
        .map_err(|error| error.to_string())
}

fn get_campaigns(url: &str) -> Result<CampaignsResponse, String> {
    let response = client(REQUEST_TIMEOUT)?
        .get(url)
        .send()
        .map_err(|error| error.to_string())?;
    if response.status().is_success() {
        response.json().map_err(|error| error.to_string())
    } else {
        Err(format!("service answered HTTP {}", response.status()))
    }
}

fn post_join_detailed(
    url: &str,
    nickname: &str,
    node: &str,
) -> Result<JoinResponse, AdmissionRefusal> {
    let response = client(REQUEST_TIMEOUT)
        .map_err(|detail| AdmissionRefusal {
            code: None,
            detail,
            retry_after_s: None,
        })?
        .post(url)
        .json(&serde_json::json!({
            "nickname": nickname,
            "node": node,
            "client_rev": BUILD_REV,
            "ruleset_version": REGOLITH_RULESET.version,
        }))
        .send()
        .map_err(|error| AdmissionRefusal {
            code: None,
            detail: error.to_string(),
            retry_after_s: None,
        })?;
    if response.status().is_success() {
        response.json().map_err(|error| AdmissionRefusal {
            code: None,
            detail: error.to_string(),
            retry_after_s: None,
        })
    } else {
        let status = response.status();
        Err(response.json::<ErrorResponse>().map_or_else(
            |_| AdmissionRefusal {
                code: None,
                detail: format!("The campaign service answered HTTP {status}."),
                retry_after_s: None,
            },
            |body| AdmissionRefusal {
                code: body.error,
                detail: body
                    .detail
                    .unwrap_or_else(|| format!("The campaign service answered HTTP {status}.")),
                retry_after_s: body.retry_after_s,
            },
        ))
    }
}

fn post_join(url: &str, nickname: &str, node: &str) -> Result<JoinResponse, String> {
    post_join_detailed(url, nickname, node).map_err(|refusal| refusal.sentence())
}

fn poll_worker(
    mut commands: Commands,
    service: (
        Res<AdmissionSettings>,
        Res<AdmissionTask>,
        ResMut<CampaignCatalog>,
    ),
    gate: Option<ResMut<JoinGate>>,
    dirty: Option<ResMut<UiDirty>>,
    mut session: ResMut<ActiveSession>,
    mut roster: ResMut<crate::roster::ShipRoster>,
    roots: Query<Entity, With<JoinUiRoot>>,
) {
    // This system removes both resources itself on a successful join, so from
    // the very next tick they are gone and a mandatory `ResMut` would fail
    // validation and panic. The join has already happened at that point —
    // there is nothing left to poll (#491).
    let (Some(mut gate), Some(mut dirty)) = (gate, dirty) else {
        return;
    };
    let (settings, task, mut catalog) = service;
    let reply = {
        let mut guard = task.0.lock().expect("admission task lock");
        let Some(receiver) = guard.as_ref() else {
            return;
        };
        match receiver.try_recv() {
            Ok(reply) => {
                *guard = None;
                reply
            }
            Err(mpsc::TryRecvError::Empty) => return,
            Err(mpsc::TryRecvError::Disconnected) => {
                *guard = None;
                if matches!(&*gate, JoinGate::Admitting { .. }) {
                    WorkerReply::Joined(Err("campaign request stopped unexpectedly".to_owned()))
                } else {
                    WorkerReply::Campaigns(Err("campaign request stopped unexpectedly".to_owned()))
                }
            }
        }
    };

    match reply {
        WorkerReply::Campaigns(Ok(answer)) => {
            catalog.campaigns.clone_from(&answer.campaigns);
            catalog.operator_note.clone_from(&answer.operator_note);
            *gate = browsing_from_live_response(answer);
            dirty.0 = true;
        }
        WorkerReply::Campaigns(Err(detail)) => {
            *gate = JoinGate::Unreachable { detail };
            dirty.0 = true;
        }
        WorkerReply::Joined(Err(detail)) => {
            let JoinGate::Admitting { .. } = &*gate else {
                return;
            };
            *gate = JoinGate::Browsing {
                campaigns: catalog.campaigns.clone(),
                operator_note: catalog.operator_note.clone(),
                dialog: Some(detail),
            };
            dirty.0 = true;
        }
        WorkerReply::Joined(Ok(answer)) => {
            // The campaign id is only knowable from the gate that asked; the
            // join answer does not echo it, and the roster URL needs it.
            let (campaign_id, island_seats) = match &*gate {
                JoinGate::Admitting { campaign, .. } => {
                    (Some(campaign.id.clone()), campaign.island_seats())
                }
                _ => (None, None),
            };
            let artifact = write_join_artifact(&settings.telemetry_path, &answer.join);
            match artifact {
                Ok(path) => info!("campaign join file written to {}", path.display()),
                Err(error) => {
                    *gate = JoinGate::Browsing {
                        campaigns: catalog.campaigns.clone(),
                        operator_note: catalog.operator_note.clone(),
                        dialog: Some(format!("Could not save the join file: {error}")),
                    };
                    dirty.0 = true;
                    return;
                }
            }
            let own_label = answer.nickname;
            roster.set_own(crate::roster::OwnLabelGrant {
                slot: answer.join.slot,
                nickname: own_label.as_deref(),
            });
            let config = CampaignConfig {
                host_node_hex: answer.join.host_node,
                host_direct: Some(answer.host_direct),
                slot: answer.join.slot,
                own_label,
                session_id: answer.join.session_id,
                session_token_hex: Some(answer.join.session_token),
                wall_start_utc: crate::campaign::utc_now_iso8601(),
                configured: ConfiguredImpairment {
                    loss_pct: answer.configured.loss_pct,
                    jitter_p50_ms: answer.configured.jitter_p50_ms,
                    jitter_p99_ms: answer.configured.jitter_p99_ms,
                },
                transport_secret: settings.transport_secret.clone(),
                island_seats,
                // The one place a roster URL can come from: the origin this
                // client actually joined through.
                roster_url: campaign_id
                    .map(|id| format!("{}/v1/campaigns/{}/roster", settings.origin, id)),
            };
            let mut runtime = CampaignRuntime::launch(config, crate::SEED);
            // See the plugin-build site: the finished row is written by the
            // call that mints it, so this session needs its path up front
            // (#947).
            runtime.set_record_path(crate::campaign_record_path(&settings.telemetry_path));
            // And its upload destination, for the same reason one step on: the
            // call that mints the row queues the upload, so no teardown path
            // can persist evidence nothing will send (#1051). The session
            // starts now, so its telemetry starts at the current end of the
            // stream -- the same offset `JsonlTelemetry` took when it opened.
            // Absent is a first launch and starts at zero. Any other stat
            // failure also has to start somewhere, and zero is the safe end --
            // a span that repeats bytes is recoverable, one that skips them is
            // not -- but it re-opens #735's whole-history upload, so it is
            // said rather than defaulted silently (#1148).
            let telemetry_start = match std::fs::metadata(&settings.telemetry_path) {
                Ok(telemetry) => telemetry.len(),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => 0,
                Err(error) => {
                    error!(
                        "cannot measure {}: {error}; this session's evidence will carry the \
                         telemetry stream from its beginning",
                        settings.telemetry_path.display()
                    );
                    0
                }
            };
            runtime.set_upload_queue(UploadQueue::new(
                settings.origin.clone(),
                &settings.telemetry_path,
                telemetry_start,
            ));
            *session = ActiveSession::Campaign(Box::new(runtime));
            commands.insert_resource(UploadManager::for_telemetry(&settings.telemetry_path));
            for root in &roots {
                commands.entity(root).despawn();
            }
            commands.remove_resource::<JoinGate>();
            commands.remove_resource::<UiDirty>();
        }
    }
}

fn browsing_from_live_response(answer: CampaignsResponse) -> JoinGate {
    JoinGate::Browsing {
        campaigns: answer.campaigns,
        operator_note: answer.operator_note,
        dialog: None,
    }
}

fn choose_campaign(
    activate: On<Activate>,
    choices: Query<&CampaignChoice>,
    mut gate: ResMut<JoinGate>,
    mut dirty: ResMut<UiDirty>,
) {
    let Ok(choice) = choices.get(activate.entity) else {
        return;
    };
    *gate = JoinGate::NicknameEntry {
        campaign: choice.0.clone(),
        nickname: String::new(),
        consented: false,
    };
    dirty.0 = true;
}

fn retry_fetch(
    activate: On<Activate>,
    retries: Query<(), With<Retry>>,
    settings: Res<AdmissionSettings>,
    task: Res<AdmissionTask>,
    mut gate: ResMut<JoinGate>,
    mut dirty: ResMut<UiDirty>,
) {
    if retries.get(activate.entity).is_err() {
        return;
    }
    *gate = JoinGate::FetchingCampaigns;
    dirty.0 = true;
    start_fetch(&settings.origin, &task);
}

fn go_back(
    activate: On<Activate>,
    backs: Query<(), With<Back>>,
    catalog: Res<CampaignCatalog>,
    mut gate: ResMut<JoinGate>,
    mut dirty: ResMut<UiDirty>,
) {
    if backs.get(activate.entity).is_err() {
        return;
    }
    *gate = JoinGate::Browsing {
        campaigns: catalog.campaigns.clone(),
        operator_note: catalog.operator_note.clone(),
        dialog: None,
    };
    dirty.0 = true;
}

fn toggle_consent(
    activate: On<Activate>,
    toggles: Query<(), With<Consent>>,
    mut gate: ResMut<JoinGate>,
    mut dirty: ResMut<UiDirty>,
) {
    if toggles.get(activate.entity).is_err() {
        return;
    }
    if let JoinGate::NicknameEntry { consented, .. } = &mut *gate {
        *consented = !*consented;
        dirty.0 = true;
    }
}

fn submit_join(
    activate: On<Activate>,
    submits: Query<(), With<SubmitJoin>>,
    settings: Res<AdmissionSettings>,
    task: Res<AdmissionTask>,
    mut gate: ResMut<JoinGate>,
    mut dirty: ResMut<UiDirty>,
) {
    if submits.get(activate.entity).is_err() {
        return;
    }
    let JoinGate::NicknameEntry {
        campaign,
        nickname,
        consented,
    } = &*gate
    else {
        return;
    };
    if !*consented || !valid_nickname(nickname) {
        return;
    }
    let campaign = campaign.clone();
    let nickname = nickname.clone();
    start_join(
        &settings.origin,
        &campaign,
        &nickname,
        &settings.transport_secret,
        &task,
    );
    *gate = JoinGate::Admitting { campaign, nickname };
    dirty.0 = true;
}

fn dismiss_dialog(
    activate: On<Activate>,
    dismisses: Query<(), With<DismissDialog>>,
    mut gate: ResMut<JoinGate>,
    mut dirty: ResMut<UiDirty>,
) {
    if dismisses.get(activate.entity).is_err() {
        return;
    }
    if let JoinGate::Browsing { dialog, .. } = &mut *gate {
        *dialog = None;
        dirty.0 = true;
    }
}

fn sync_nickname(
    editors: Query<&EditableText, (With<NicknameEditor>, Changed<EditableText>)>,
    gate: Option<ResMut<JoinGate>>,
    dirty: Option<ResMut<UiDirty>>,
) {
    // The gate is gone once a join succeeds; this system is chained behind the
    // system that removes it, so it must tolerate that rather than panic.
    let (Some(mut gate), Some(mut dirty)) = (gate, dirty) else {
        return;
    };
    let Ok(editor) = editors.single() else {
        return;
    };
    let value = editor.value().into_iter().collect::<String>();
    if let JoinGate::NicknameEntry { nickname, .. } = &mut *gate {
        if *nickname != value {
            *nickname = value;
            dirty.0 = true;
        }
    }
}

fn valid_nickname(nickname: &str) -> bool {
    !nickname.trim().is_empty()
        && nickname.chars().count() <= 32
        && nickname
            .chars()
            .all(|glyph| glyph.is_ascii_graphic() || glyph == ' ')
}

fn rebuild_ui(
    mut commands: Commands,
    gate: Option<Res<JoinGate>>,
    dirty: Option<ResMut<UiDirty>>,
    roots: Query<Entity, With<JoinUiRoot>>,
) {
    let (Some(gate), Some(mut dirty)) = (gate, dirty) else {
        return;
    };
    if !dirty.0 {
        return;
    }
    dirty.0 = false;
    for root in &roots {
        commands.entity(root).despawn();
    }
    let snapshot = gate.clone();
    commands
        .spawn((
            JoinUiRoot,
            GlobalZIndex(JOIN_GATE_Z),
            Node {
                position_type: PositionType::Absolute,
                width: percent(100),
                height: percent(100),
                padding: px(36).all(),
                justify_content: JustifyContent::Center,
                align_items: AlignItems::Center,
                ..default()
            },
            BackgroundColor(Color::srgba(0.01, 0.015, 0.025, 0.97)),
        ))
        .with_children(|root| {
            root.spawn((
                Node {
                    width: px(720),
                    max_height: percent(92),
                    flex_direction: FlexDirection::Column,
                    row_gap: px(14),
                    padding: px(24).all(),
                    border_radius: BorderRadius::all(px(10)),
                    ..default()
                },
                BackgroundColor(PANEL),
            ))
            .with_children(|panel| render_gate(panel, &snapshot));
        });
}

fn render_gate(panel: &mut ChildSpawnerCommands, gate: &JoinGate) {
    spawn_text(panel, "ORRERY CAMPAIGNS", 28.0, Color::WHITE);
    match gate {
        JoinGate::FetchingCampaigns => {
            spawn_text(panel, "Finding current campaigns...", 18.0, DIM);
        }
        JoinGate::Unreachable { detail } => {
            spawn_text(
                panel,
                &format!("Can't reach the campaign service - {detail}"),
                18.0,
                Color::srgb(1.0, 0.55, 0.42),
            );
            spawn_text(panel, "If this persists, tell the operator.", 15.0, DIM);
            spawn_text(
                panel,
                "Have a join file? Start with --join <path>.",
                14.0,
                DIM,
            );
            spawn_button(panel, "Retry", Retry, true);
        }
        JoinGate::Browsing {
            campaigns,
            operator_note,
            dialog,
        } => {
            if campaigns.is_empty() {
                spawn_text(
                    panel,
                    "No campaigns right now - check back later.",
                    19.0,
                    Color::WHITE,
                );
            }
            if let Some(note) = operator_note {
                spawn_text(panel, note, 14.0, DIM);
            }
            panel
                .spawn((
                    ScrollArea,
                    Node {
                        width: percent(100),
                        max_height: px(500),
                        flex_direction: FlexDirection::Column,
                        row_gap: px(8),
                        overflow: Overflow::scroll_y(),
                        ..default()
                    },
                ))
                .with_children(|list| {
                    for campaign in campaigns {
                        let enabled = campaign_is_compatible_and_open(campaign);
                        let state = campaign_state_line(campaign);
                        spawn_button(
                            list,
                            &format!(
                                "{}\n{}% loss | {} ms jitter | {state}",
                                campaign.title, campaign.loss_pct, campaign.jitter_ms
                            ),
                            CampaignChoice(campaign.clone()),
                            enabled,
                        );
                    }
                });
            if let Some(detail) = dialog {
                spawn_text(panel, detail, 17.0, Color::srgb(1.0, 0.65, 0.4));
                spawn_button(panel, "Back to campaigns", DismissDialog, true);
            }
        }
        JoinGate::NicknameEntry {
            campaign,
            nickname,
            consented,
        } => {
            spawn_text(panel, &campaign.title, 21.0, Color::WHITE);
            spawn_text(panel, "Nickname (1-32 characters)", 14.0, DIM);
            panel.spawn((
                NicknameEditor,
                EditableText::new(nickname.clone()),
                TextCursorStyle::default(),
                Text::new(nickname),
                TextFont::from_font_size(20.0),
                TextColor(Color::WHITE),
                TextLayout::no_wrap(),
                AutoFocus,
                Node {
                    width: percent(100),
                    min_height: px(44),
                    padding: px(10).all(),
                    border: px(1).all(),
                    overflow: Overflow::clip_x(),
                    ..default()
                },
                BorderColor::all(if valid_nickname(nickname) {
                    Color::srgb(0.35, 0.8, 0.65)
                } else {
                    DIM
                }),
                BackgroundColor(ROW),
            ));
            spawn_text(panel, CONSENT_NOTICE, 14.0, DIM);
            spawn_button(
                panel,
                if *consented {
                    "[x] I consent to campaign recording"
                } else {
                    "[ ] I consent to campaign recording"
                },
                Consent,
                true,
            );
            spawn_button(
                panel,
                "Join campaign",
                SubmitJoin,
                *consented && valid_nickname(nickname),
            );
            spawn_button(panel, "Back", Back, true);
        }
        JoinGate::Admitting { .. } => {
            spawn_text(panel, "Starting your session...", 20.0, Color::WHITE);
            spawn_text(
                panel,
                "The host can take up to 30 seconds to bind.",
                14.0,
                DIM,
            );
        }
    }
}

fn campaign_state_line(campaign: &CampaignListing) -> String {
    let required_rev = campaign
        .server_rev
        .as_ref()
        .or(campaign.client_rev.as_ref());
    if required_rev.is_some_and(|revision| revision != BUILD_REV) {
        return format!(
            "needs build {} - download the current build",
            required_rev.expect("checked as some")
        );
    }
    match campaign.state.as_str() {
        "busy" => format!("busy - try again in ~{} min", campaign.seconds.div_ceil(60)),
        "paused" => "admissions paused - not your fault; try again later".to_owned(),
        "closed" => "closed".to_owned(),
        other => other.to_owned(),
    }
}

fn campaign_is_compatible_and_open(campaign: &CampaignListing) -> bool {
    let required_rev = campaign
        .server_rev
        .as_ref()
        .or(campaign.client_rev.as_ref());
    campaign.state == "open" && required_rev.is_none_or(|revision| revision == BUILD_REV)
}

/// Discover and admit one campaign without keyboard or pointer input.
///
/// This is the release preflight's entry point. It deliberately calls the
/// same listing decoder, compatibility/open predicate, and join request as the
/// interactive admission UI. A temporary non-open phase is retried because an
/// always-on campaign cycles through lobby, running, and restarting; a build
/// mismatch is terminal because waiting cannot make the embedded revision
/// change.
///
/// When `expected_refusal` is present, the request is sent even if the listing
/// says the build is incompatible. That negative probe succeeds only when
/// admission itself returns the named machine-readable refusal.
///
/// # Errors
/// A named stage and its detail when discovery, compatibility, admission, or
/// join-artifact persistence fails before the deadline.
pub fn admit_headless(
    origin: &str,
    campaign_id: &str,
    nickname: &str,
    transport_secret: &iroh_base::SecretKey,
    telemetry_path: &Path,
    timeout: Duration,
    expected_refusal: Option<&str>,
) -> Result<HeadlessAdmission, String> {
    if !valid_nickname(nickname) {
        return Err("nickname: needs 1-32 visible ASCII characters".to_owned());
    }
    let deadline = Instant::now() + timeout;
    let url = format!("{origin}/v1/campaigns");
    let mut origin_reported = false;
    loop {
        let answer = match get_campaigns(&url) {
            Ok(answer) => answer,
            Err(detail) if Instant::now() < deadline => {
                eprintln!("PREFLIGHT WAIT admission-origin origin={origin} detail={detail}");
                std::thread::sleep(Duration::from_secs(1));
                continue;
            }
            Err(detail) => return Err(format!("admission-origin: {detail}")),
        };
        if !origin_reported {
            println!("PREFLIGHT PASS admission-origin origin={origin}");
            origin_reported = true;
        }

        let Some(campaign) = answer
            .campaigns
            .iter()
            .find(|campaign| campaign.id == campaign_id)
        else {
            if Instant::now() >= deadline {
                return Err(format!(
                    "campaign-joinable: campaign {campaign_id:?} was not listed before the timeout"
                ));
            }
            eprintln!("PREFLIGHT WAIT campaign-joinable campaign={campaign_id} state=absent");
            std::thread::sleep(Duration::from_secs(1));
            continue;
        };

        let required_rev = campaign
            .server_rev
            .as_ref()
            .or(campaign.client_rev.as_ref());
        if expected_refusal.is_none() && required_rev.is_some_and(|revision| revision != BUILD_REV)
        {
            return Err(format!(
                "campaign-compatible: campaign {campaign_id:?} needs build {}, this binary is {BUILD_REV}",
                required_rev.expect("checked as some")
            ));
        }

        if expected_refusal.is_none() && !campaign_is_compatible_and_open(campaign) {
            if Instant::now() >= deadline {
                return Err(format!(
                    "campaign-joinable: campaign {campaign_id:?} stayed state={:?} before the timeout",
                    campaign.state
                ));
            }
            eprintln!(
                "PREFLIGHT WAIT campaign-joinable campaign={campaign_id} state={}",
                campaign.state
            );
            std::thread::sleep(Duration::from_secs(1));
            continue;
        }

        let join_url = format!("{origin}/v1/campaigns/{campaign_id}/join");
        let node = transport_secret.public().to_string();
        if let Some(expected) = expected_refusal {
            return match post_join_detailed(&join_url, nickname, &node) {
                Err(refusal) if refusal.code.as_deref() == Some(expected) => {
                    println!(
                        "PREFLIGHT PASS admission-refusal campaign={campaign_id} error={expected}"
                    );
                    Ok(HeadlessAdmission::ExpectedRefusal)
                }
                Err(refusal) => Err(format!(
                    "admission-refusal: expected {expected:?}, got {:?}: {}",
                    refusal.code, refusal.detail
                )),
                Ok(_) => Err(format!(
                    "admission-refusal: expected {expected:?}, but admission minted a seat"
                )),
            };
        }

        println!("PREFLIGHT PASS campaign-joinable campaign={campaign_id}");
        match post_join_detailed(&join_url, nickname, &node) {
            Ok(answer) => {
                let path = write_join_artifact(telemetry_path, &answer.join)
                    .map_err(|error| format!("join-artifact: {error}"))?;
                println!(
                    "PREFLIGHT PASS admission-accepted campaign={campaign_id} slot={} artifact={}",
                    answer.join.slot,
                    path.display()
                );
                return Ok(HeadlessAdmission::Admitted(Box::new(CampaignConfig {
                    host_node_hex: answer.join.host_node,
                    host_direct: Some(answer.host_direct),
                    slot: answer.join.slot,
                    own_label: answer.nickname,
                    session_id: answer.join.session_id,
                    session_token_hex: Some(answer.join.session_token),
                    wall_start_utc: crate::campaign::utc_now_iso8601(),
                    configured: ConfiguredImpairment {
                        loss_pct: answer.configured.loss_pct,
                        jitter_p50_ms: answer.configured.jitter_p50_ms,
                        jitter_p99_ms: answer.configured.jitter_p99_ms,
                    },
                    transport_secret: transport_secret.clone(),
                    island_seats: campaign.island_seats(),
                    roster_url: Some(format!("{origin}/v1/campaigns/{campaign_id}/roster")),
                })));
            }
            Err(refusal)
                if matches!(
                    refusal.code.as_deref(),
                    // `seat_held_for_reconnect` is as transient as a full
                    // campaign and shorter-lived: admission is holding the
                    // last seat for a volunteer whose lobby connection lapsed,
                    // and frees it when their reissue window closes (#1001).
                    Some(
                        "host_failed"
                            | "session_started"
                            | "campaign_full"
                            | "seat_held_for_reconnect"
                    )
                ) && Instant::now() < deadline =>
            {
                eprintln!(
                    "PREFLIGHT WAIT admission-accepted campaign={campaign_id} error={} detail={}",
                    refusal.code.as_deref().unwrap_or("unknown"),
                    refusal.detail
                );
                let wait = refusal.retry_after_s.unwrap_or(1).clamp(1, 5);
                std::thread::sleep(Duration::from_secs(wait));
            }
            Err(refusal) => {
                return Err(format!(
                    "admission-accepted: {} ({})",
                    refusal.code.as_deref().unwrap_or("unnamed_refusal"),
                    refusal.detail
                ));
            }
        }
    }
}

fn spawn_text(parent: &mut ChildSpawnerCommands, text: &str, size: f32, color: Color) {
    parent.spawn((
        Text::new(text),
        TextFont::from_font_size(size),
        TextColor(color),
    ));
}

fn spawn_button<M: Component>(
    parent: &mut ChildSpawnerCommands,
    label: &str,
    marker: M,
    enabled: bool,
) {
    let mut button = parent.spawn((
        Button,
        marker,
        Node {
            width: percent(100),
            min_height: px(42),
            padding: px(10).all(),
            border_radius: BorderRadius::all(px(6)),
            ..default()
        },
        BackgroundColor(if enabled { ACTIVE } else { ROW }),
        children![(
            Text::new(label),
            TextFont::from_font_size(16.0),
            TextColor(if enabled { Color::WHITE } else { DIM }),
        )],
    ));
    if !enabled {
        button.insert(InteractionDisabled);
    }
}

/// Save the granted seat beside the telemetry stream, and report where.
///
/// The directory is the telemetry path's, which since the 2026-09-02 owner
/// decision resolves to the directory holding the executable — the extracted
/// release folder a volunteer is already looking in (#942) — rather than to
/// `target/` under whatever her working directory happened to be, or to the
/// per-user application-data directory #766 chose first. Deliberately without
/// a fallback: this write failing is what stops a player at the door, so the
/// one thing that keeps it working must be resolving a writable directory in
/// the first place, and a rescue path here would hide that.
pub fn write_join_artifact(telemetry_path: &Path, join: &JoinObject) -> std::io::Result<PathBuf> {
    let directory = telemetry_path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(directory)?;
    let path = directory.join(format!("{}.join.json", join.session_id));
    let wire = CampaignJoinFileV1::new(
        join.host_node.clone(),
        join.slot,
        join.session_id.clone(),
        join.session_token.clone(),
    )
    .to_json()
    .map_err(std::io::Error::other)?;
    durable_write(&path, wire.as_bytes())?;
    Ok(path)
}

/// The `uploads.json` a joined session's bodies are owed from.
///
/// It used to carry the service origin as well, which nothing reads any more:
/// an entry in `uploads.json` records the origin it was queued for, so every
/// body already knows where it goes, and the only thing that ever asked the
/// manager was the second queueing #1125 removed. A field kept for its name
/// reads as evidence the code is using it.
#[derive(Resource)]
pub struct UploadManager {
    state_path: PathBuf,
}

impl UploadManager {
    /// Build the uploader for a session recording beside `telemetry_path`.
    #[must_use]
    pub fn for_telemetry(telemetry_path: &Path) -> Self {
        Self {
            state_path: upload_state_path(telemetry_path),
        }
    }
}

/// Depth of the full-screen join gate.
///
/// Named so the scope banner can be placed above it on purpose
/// (`SESSION_BANNER_Z`) rather than by two unrelated literals happening to
/// order correctly.
pub const JOIN_GATE_Z: i32 = 1000;

/// The campaign id a roster URL was built for.
///
/// The roster URL is `{origin}/v1/campaigns/{id}/roster`, and it is the one
/// thing a joined session carries that names the campaign rather than the
/// seat, so it is also the only honest source for what the scope banner calls
/// the campaign a player is in (#769).
#[must_use]
pub fn campaign_id_of_roster_url(roster_url: &str) -> Option<String> {
    let rest = roster_url.split_once("/v1/campaigns/")?.1;
    let id = rest.split('/').next()?;
    (!id.is_empty()).then(|| id.to_owned())
}

/// The service origin a roster URL was built from.
///
/// The roster URL is the one place a joined session records where it came
/// from, so it is also the only honest source for where its record goes back.
#[must_use]
pub fn origin_of_roster_url(roster_url: &str) -> Option<String> {
    let rest = roster_url.split_once("://")?.1;
    let authority_len = rest.find('/')?;
    Some(roster_url[..roster_url.len() - rest.len() + authority_len].to_owned())
}

#[cfg(test)]
impl UploadManager {
    pub(crate) fn for_test(telemetry_path: &Path) -> Self {
        Self {
            state_path: upload_state_path(telemetry_path),
        }
    }
}

/// Every body this installation owes, keyed by [`upload_key`].
///
/// The map used to be keyed by session id, which was the same thing until a
/// seat started banking in increments: a long session now owes several bodies
/// under one session id, and keying on the id alone made each increment
/// overwrite the last one's entry *and* its file (#1048).
#[derive(Debug, Default, Deserialize, Serialize)]
struct UploadState {
    sessions: BTreeMap<String, UploadEntry>,
    /// Whether this file has had the #1118 acknowledgement repair applied.
    ///
    /// Defaulted false for a file written before the repair existed, which is
    /// exactly the population that needs it. See
    /// [`repair_stolen_acknowledgements`]: without the flag the repair would
    /// re-open the same entries at every launch, which is the unbounded
    /// re-upload half of #1118 wearing the fix's clothes.
    #[serde(default)]
    increment_acks_repaired: bool,
}

#[derive(Debug, Deserialize, Serialize)]
struct UploadEntry {
    origin: String,
    body_path: PathBuf,
    acknowledged: bool,
    /// The session this body belongs to, which is what the POST path names.
    ///
    /// Defaulted for a state file written before #1048, whose keys *were*
    /// session ids; [`pending_uploads`] falls back to the key there.
    #[serde(default)]
    session_id: Option<String>,
    /// How many times this body has been POSTed and refused.
    ///
    /// Only [`UploadRefusal::Unknown`] is bounded by it, but every refusal
    /// counts: a body attempted twenty times and still called "transient" is a
    /// fact an operator reading `uploads.json` should be able to see (#1149).
    #[serde(default)]
    attempts: u32,
    /// Why this body will never be sent again, once something says so.
    ///
    /// A tombstone, not a deletion. `uploads.json` is also what stops
    /// [`sweep_unregistered_records`] re-queuing a row from
    /// `campaign-records.jsonl`, so an entry that is simply removed comes
    /// straight back at the next launch. The body itself is deliberately left
    /// on disk and named in the log: retiring is the client giving up on
    /// sending a body, not the client destroying a volunteer's signed
    /// evidence.
    #[serde(default)]
    retired: Option<String>,
}

/// Why one POST did not deliver a body, and what that means for the next one.
///
/// Before #1149 there was one arm: every refusal, from a TCP reset to a `413`,
/// came back as an opaque string meaning "not sent yet, try again next
/// launch". So a body the service will refuse forever was re-POSTed at every
/// launch for the life of the installation, on a 45-second-timeout blocking
/// client.
#[derive(Debug)]
enum UploadRefusal {
    /// Not delivered *yet*. Transport failures, timeouts, `5xx`, `429`, `408`.
    ///
    /// Never retired. A volunteer's evidence outlives an outage, and the cost
    /// of holding it is one POST per launch, not per second:
    /// [`retry_pending_uploads`] is one-shot.
    Transient(String),
    /// The service already holds this increment, under bytes that are not ours.
    ///
    /// `409` only ever means *differing* bytes for a stored increment (the
    /// conflict arm `scripts/admission.py` grew in #1119); identical bytes
    /// answer `204`. So the increment is banked and this body is not the copy
    /// of it that was banked -- re-POSTing can only earn the same `409`.
    Banked(String),
    /// The service will refuse these bytes at this address however often it is
    /// asked: `413` (over the service's upload cap, and a body does not
    /// shrink), `422 wrong_increment` (the increment is baked into both the
    /// body and the address), `403`, and every other `4xx` that is not `404`,
    /// `408` or `429`.
    Permanent(String),
    /// `404 unknown_session`, which is neither, and is therefore bounded.
    ///
    /// It looks permanent -- a session the service never admitted will never
    /// be admitted -- but two live causes make it spurious. #1151: one
    /// malformed line in `joins.jsonl` makes the service `404` *every* session
    /// admitted after it, cohort-wide and repairable. And the #1135 seam: a
    /// client posting per-increment addresses to a service that predates them
    /// is `404`ed on every increment after the first, which [`upload_path`]
    /// documents as deliberately survivable in either deployment order. Both
    /// are fixed by a deployment, on a scale of days, so the first `404` must
    /// not retire a body -- and the hundredth must not keep it.
    /// [`UNKNOWN_SESSION_ATTEMPTS`] launches is the compromise.
    Unknown(String),
}

impl UploadRefusal {
    /// The detail line, whatever the classification.
    fn detail(&self) -> &str {
        match self {
            Self::Transient(detail)
            | Self::Banked(detail)
            | Self::Permanent(detail)
            | Self::Unknown(detail) => detail,
        }
    }

    /// How this refusal reads in a log line.
    fn posture(&self) -> &'static str {
        match self {
            Self::Transient(_) => "transient",
            Self::Banked(_) => "already banked",
            Self::Permanent(_) => "permanently refused",
            Self::Unknown(_) => "refused as unknown",
        }
    }

    /// Why this body is retired now, or `None` to leave it queued.
    fn retirement(&self, attempts: u32) -> Option<String> {
        match self {
            Self::Transient(_) => None,
            Self::Banked(detail) => Some(format!(
                "the service already holds this increment: {detail}"
            )),
            Self::Permanent(detail) => Some(format!("permanently refused: {detail}")),
            Self::Unknown(detail) => (attempts >= UNKNOWN_SESSION_ATTEMPTS)
                .then(|| format!("refused as unknown {attempts} times: {detail}")),
        }
    }
}

/// Read one HTTP answer as a refusal.
fn classify_upload_refusal(status: reqwest::StatusCode, detail: String) -> UploadRefusal {
    use reqwest::StatusCode as Code;
    match status {
        Code::CONFLICT => UploadRefusal::Banked(detail),
        Code::NOT_FOUND => UploadRefusal::Unknown(detail),
        Code::REQUEST_TIMEOUT | Code::TOO_MANY_REQUESTS => UploadRefusal::Transient(detail),
        _ if status.is_server_error() => UploadRefusal::Transient(detail),
        _ if status.is_client_error() => UploadRefusal::Permanent(detail),
        // A `2xx` that is not `204`, or a `3xx` the redirect policy did not
        // follow: the service is answering, but not with the acknowledgement
        // this client understands. Retrying costs one POST a launch, and a
        // build that understands the answer may yet arrive.
        _ => UploadRefusal::Transient(detail),
    }
}

/// What `uploads.json` files one body under.
///
/// The session id for a seat's first row — which for a seat too short to bank
/// an increment is its only row, so a state file written before #1048 keeps
/// exactly the keys it had — and the id plus the increment's index for every
/// row after it.
fn upload_key(session_id: &str, increment_index: u64) -> String {
    if increment_index == 0 {
        session_id.to_owned()
    } else {
        format!("{session_id}.increment-{increment_index}")
    }
}

/// The increment an entry read back from `uploads.json` names.
///
/// [`upload_key`] is the only thing that has ever recorded which increment a
/// queued body is, so it is the only thing that can be asked. Parsing the key
/// rather than storing the index a second time keeps one source of truth: a
/// stored index could disagree with the key it sits under, and the key is
/// what the map, the body's filename and the skip-if-acknowledged guard all
/// already use.
fn increment_index_of_key(upload_key: &str, session_id: &str) -> u64 {
    upload_key
        .strip_prefix(session_id)
        .and_then(|rest| rest.strip_prefix(".increment-"))
        .and_then(|index| index.parse().ok())
        .unwrap_or(0)
}

/// Where one increment's body is POSTed, relative to the service origin.
///
/// The address is the *increment*, not the seat (#1119). Every increment of a
/// long seat used to go to `/v1/sessions/{id}/upload`, where the service
/// stores one pair of files per session id and refuses any second body whose
/// bytes differ with `409 conflict` -- so a session longer than
/// [`crate::session::INCREMENT_MINUTES`] banked its first five minutes and
/// nothing else, on that launch and on every later one.
///
/// Increment zero keeps the old path, exactly as [`upload_key`] keeps the
/// bare session id for it. That is not tidiness: it means a client carrying
/// this fix against a service that has not been updated yet still banks its
/// first increment as it does today, and the increments after it are refused
/// `404` and stay queued for a later launch, rather than the whole seat
/// failing on an unrouted path. The two halves of the seam can be deployed in
/// either order without losing evidence.
fn upload_path(session_id: &str, increment_index: u64) -> String {
    if increment_index == 0 {
        format!("v1/sessions/{session_id}/upload")
    } else {
        format!("v1/sessions/{session_id}/increments/{increment_index}/upload")
    }
}

/// The increment index a signed row carries, or zero for a row without one.
///
/// A row written before #1048 has no `increment` object and is a whole seat,
/// which is increment zero of a seat of one.
fn increment_index_of(row: &serde_json::Value) -> u64 {
    row.get("increment")
        .and_then(|increment| increment.get("index"))
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0)
}

fn upload_state_path(telemetry_path: &Path) -> PathBuf {
    telemetry_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("uploads.json")
}

/// Where a finished row's evidence goes, fixed before the session ends.
///
/// The row and its upload used to be minted in two different places. The row
/// is minted by [`crate::campaign::CampaignRuntime::finish_record`], which
/// every teardown path reaches, including through `Drop` during a panic
/// unwind (#947). The upload was attempted by a Bevy system in `Last`, which
/// only two of them reach: an `AppExit`, and the link leaving
/// `JoinState::Joined`. A macOS Cmd+Q raises `applicationWillTerminate:`,
/// which tears the world down without running another `Last` -- so the row
/// was minted, signed and persisted, and *nothing ever queued it*. Two
/// volunteers' sessions had to be hand-carried off their machines (#1051).
///
/// So the queue is handed to the runtime before its first tick, and the same
/// call that mints a row registers the upload for it. Whatever ends the
/// process after that, the body is on disk and `uploads.json` names it as
/// unacknowledged, which is all the next launch needs.
#[derive(Clone, Debug)]
pub struct UploadQueue {
    origin: String,
    state_path: PathBuf,
    telemetry_path: PathBuf,
    /// The stream offset the *next* body's telemetry starts at (#1125).
    ///
    /// A cursor and not a fixed offset. It used to be the seat's start for
    /// every body, so increment `k` embedded increments `0..=k`: the bytes a
    /// session uploaded grew quadratically in its length, and past about
    /// ninety minutes of campaign play the final body exceeded the service's
    /// 64 MiB cap and was refused `413 too_large` -- so the evidence a long
    /// session was banking stopped landing exactly when there was most of it.
    ///
    /// The address half of this is #1119's: each increment posts to
    /// `/v1/sessions/{id}/increments/{n}/upload` and is acknowledged under its
    /// own key. This is the body half, and it makes the two agree -- the
    /// telemetry under the increment's name is the telemetry of that
    /// increment. The spans of one seat partition its ticks
    /// (`crate::session::SeatIncrement`) and the cursor partitions the stream
    /// the same way: disjoint, abutting, and summing to the seat.
    telemetry_next: u64,
}

impl UploadQueue {
    /// Queue rows for `origin`, taking the first body's telemetry from
    /// `telemetry_start` -- see [`crate::telemetry::JsonlTelemetry::session_start`].
    ///
    /// Every body after it starts where the one before it ended; see
    /// [`Self::telemetry_next`].
    #[must_use]
    pub fn new(origin: String, telemetry_path: &Path, telemetry_start: u64) -> Self {
        Self {
            origin,
            state_path: upload_state_path(telemetry_path),
            telemetry_path: telemetry_path.to_owned(),
            telemetry_next: telemetry_start,
        }
    }
}

/// One upload body that is on disk and that the service has not acknowledged.
///
/// A named row rather than a bare tuple: the retry loop carries a session id,
/// an origin and a path, two of them `String`, and one transposition would
/// post every volunteer's evidence to a URL built from a session id.
#[derive(Clone, Debug)]
pub struct PendingUpload {
    session_id: String,
    upload_key: String,
    /// Which increment of the seat this body is, which is half of its address
    /// at the service (#1119) -- see [`upload_path`].
    increment_index: u64,
    origin: String,
    body_path: PathBuf,
}

impl PendingUpload {
    /// The exact bytes queued for this session, for a log line a volunteer
    /// can act on when the service never acknowledges them.
    #[must_use]
    pub fn body_path(&self) -> &Path {
        &self.body_path
    }
}

/// Write a finished row's exact upload body and register it as pending.
///
/// Registration happens *before* any POST, so a body that exists on disk is
/// always named by `uploads.json`. Nothing here talks to the network: a
/// queued upload is a promise the next launch can keep on its own.
pub fn queue_finished_session(
    queue: &mut UploadQueue,
    record: &SessionRecord,
) -> Result<PendingUpload, String> {
    // One of the three places a `proton-debug` build refuses (#1060). This is
    // the only call that puts a row's bytes on disk under `uploads.json`, so
    // returning before it means such a build has no evidence to send, this
    // launch or any later one. See `crate::BANKABLE`.
    //
    // A `#[cfg]` on the statement rather than `if !crate::BANKABLE`: the
    // constant folds, but the statement is still in the tree the shipped build
    // lowers. Cfg-stripping means the ordinary build has no such statement at
    // all, which is the difference between "optimized away" and "not there".
    #[cfg(proton_debug)]
    return Err(crate::NOT_BANKABLE_REASON.to_owned());
    // An unreadable stream costs the telemetry, not the row. The signed
    // record is the evidence; refusing to queue it because the JSONL beside
    // it could not be read is how a measured session goes missing, which is
    // the whole failure this function exists to end.
    let span = read_session_telemetry(&queue.telemetry_path, queue.telemetry_next).unwrap_or_else(
        |error| {
            eprintln!(
                "regolith: cannot read telemetry {} ({error}); the row is queued without it",
                queue.telemetry_path.display()
            );
            // The cursor stays where it was. Bytes this call could not read
            // are not this increment's loss to take: the next body starts at
            // the same offset and carries them instead.
            TelemetrySpan {
                jsonl: String::new(),
                end: queue.telemetry_next,
            }
        },
    );
    let row =
        serde_json::to_value(record).map_err(|error| format!("cannot serialize row: {error}"))?;
    let upload = build_upload_body(vec![row], span.jsonl)?;
    let pending = persist_and_register(&queue.state_path, &queue.origin, &upload)?;
    // Advanced only once the body is on disk and named by `uploads.json`.
    // A queueing that failed sent nothing, so its bytes are still owed, and
    // moving the cursor past them would be #735 in miniature -- telemetry
    // silently dropped on the floor between two increments.
    queue.telemetry_next = span.end;
    Ok(pending)
}

/// Put the body on disk and name it in `uploads.json` as unacknowledged.
fn persist_and_register(
    state_path: &Path,
    origin: &str,
    upload: &UploadBody,
) -> Result<PendingUpload, String> {
    let directory = state_path.parent().unwrap_or_else(|| Path::new("."));
    let body_path = directory.join(format!("upload-{}.json", upload.upload_key));
    durable_write(&body_path, &upload.body)
        .map_err(|error| format!("cannot preserve {}: {error}", body_path.display()))?;
    // An unreadable state file must not be defaulted to an empty one here:
    // registering into a fresh map would durably erase every acknowledgement
    // and every tombstone the install has (#1148). The body is already on
    // disk, and `recover_orphaned_bodies` re-registers it at the next launch.
    let mut state = read_upload_state(state_path).map_err(|error| {
        format!(
            "cannot register {} because the upload state is unreadable: {error}",
            body_path.display()
        )
    })?;
    let already_settled = state
        .sessions
        .get(&upload.upload_key)
        .is_some_and(|entry| entry.acknowledged || entry.retired.is_some());
    // Never walk an acknowledgement back. A row can be queued twice -- once by
    // the mint, once by the exit path that follows it -- and the second must
    // not re-open a session the service has already taken. A retirement is
    // settled for the same reason: re-queuing a body the service has refused
    // for good is the forever-retry #1149 is about.
    if !already_settled {
        state.sessions.insert(
            upload.upload_key.clone(),
            UploadEntry {
                origin: origin.to_owned(),
                body_path: body_path.clone(),
                acknowledged: false,
                session_id: Some(upload.session_id.clone()),
                attempts: 0,
                retired: None,
            },
        );
        write_upload_state(state_path, &state)
            .map_err(|error| format!("cannot write upload retry state: {error}"))?;
    }
    Ok(PendingUpload {
        session_id: upload.session_id.clone(),
        upload_key: upload.upload_key.clone(),
        increment_index: upload.increment_index,
        origin: origin.to_owned(),
        body_path,
    })
}

/// POST one queued body, and record the acknowledgement if it lands.
///
/// Returns whether this call is what delivered it. An entry the state file
/// already calls acknowledged is not posted again.
fn send_pending(state_path: &Path, pending: &PendingUpload, timeout: Duration) -> bool {
    // An unreadable state file stops the POST rather than defaulting to an
    // empty map: this call would otherwise re-send bodies the service already
    // holds and then write an acknowledgement into a map that has forgotten
    // every other one (#1148).
    let state = match read_upload_state(state_path) {
        Ok(state) => state,
        Err(error) => {
            eprintln!(
                "regolith: campaign session {} not sent: {error}; the body is at {} and nothing \
                 has been overwritten",
                pending.session_id,
                pending.body_path.display()
            );
            return false;
        }
    };
    if state
        .sessions
        .get(&pending.upload_key)
        .is_some_and(|entry| entry.acknowledged || entry.retired.is_some())
    {
        return false;
    }
    let result = std::fs::read(&pending.body_path)
        .map_err(|error| {
            // The body is gone and cannot be reconstructed, so no number of
            // launches will change the answer.
            UploadRefusal::Permanent(format!("{}: {error}", pending.body_path.display()))
        })
        .and_then(|body| {
            post_upload(
                &pending.origin,
                &pending.session_id,
                pending.increment_index,
                &body,
                timeout,
            )
        });
    match result {
        Ok(()) => {
            // Re-read rather than mutate a copy: the acknowledgement is
            // written per body, so a process that dies mid-flush keeps
            // every acknowledgement it had already earned.
            //
            // Keyed by the *upload key*, not the session id. `uploads.json`
            // has been keyed by upload key since #1048, and a seat longer
            // than `INCREMENT_MINUTES` owes several bodies under one session
            // id. Looking the entry up by session id found increment zero's
            // row for every increment of the seat: increment one onward was
            // never marked acknowledged and was re-posted at every later
            // launch, and a seat whose increment zero had failed had that
            // failure overwritten by increment one's success, so increment
            // zero's evidence was dropped and never retried.
            let mut state = match read_upload_state(state_path) {
                Ok(state) => state,
                Err(error) => {
                    eprintln!(
                        "regolith: campaign session {} uploaded but the acknowledgement cannot be \
                         recorded: {error}; nothing has been overwritten and the body at {} will \
                         be posted again",
                        pending.session_id,
                        pending.body_path.display()
                    );
                    return true;
                }
            };
            if let Some(entry) = state.sessions.get_mut(&pending.upload_key) {
                entry.acknowledged = true;
            } else {
                // An entry always exists: `send_pending` is only reached
                // through `pending_uploads`, which reads it out of this file.
                // Saying so is the difference between a lost acknowledgement
                // and a silent one (#1051).
                eprintln!(
                    "regolith: campaign session {} uploaded but {} names no entry for {}; \
                     it will be posted again next launch",
                    pending.session_id,
                    state_path.display(),
                    pending.upload_key
                );
            }
            if let Err(error) = write_upload_state(state_path, &state) {
                eprintln!(
                    "regolith: campaign session {} uploaded but the acknowledgement could not be saved: {error}",
                    pending.session_id
                );
            }
            true
        }
        Err(refusal) => {
            record_refusal(state_path, pending, &refusal);
            false
        }
    }
}

/// Count one refusal against a body, and retire it if that is what it means.
///
/// Everything about a refusal that outlives this launch is written here, and
/// it is the only place a body stops being owed. A retirement is durable
/// because the alternative -- deciding again from scratch at the next launch
/// -- is exactly the forever-retry #1149 is about.
fn record_refusal(state_path: &Path, pending: &PendingUpload, refusal: &UploadRefusal) {
    let mut state = match read_upload_state(state_path) {
        Ok(state) => state,
        Err(error) => {
            eprintln!(
                "regolith: campaign session {} upload failed ({}: {}) and the attempt cannot be \
                 recorded: {error}",
                pending.session_id,
                refusal.posture(),
                refusal.detail()
            );
            return;
        }
    };
    let Some(entry) = state.sessions.get_mut(&pending.upload_key) else {
        eprintln!(
            "regolith: campaign session {} upload failed ({}: {}) and {} names no entry for {}",
            pending.session_id,
            refusal.posture(),
            refusal.detail(),
            state_path.display(),
            pending.upload_key
        );
        return;
    };
    entry.attempts = entry.attempts.saturating_add(1);
    let retirement = refusal.retirement(entry.attempts);
    let attempts = entry.attempts;
    entry.retired.clone_from(&retirement);
    if let Err(error) = write_upload_state(state_path, &state) {
        eprintln!(
            "regolith: campaign session {} upload failed and the attempt could not be saved: {error}",
            pending.session_id
        );
        return;
    }
    match retirement {
        Some(reason) => eprintln!(
            "regolith: campaign session {} increment {} will not be sent again after {attempts} \
             attempt(s): {reason}; the body is kept at {} and can be sent by hand",
            pending.session_id,
            pending.increment_index,
            pending.body_path.display()
        ),
        None => eprintln!(
            "regolith: campaign session {} upload failed ({}: {}), attempt {attempts}; it will \
             retry next launch, and the volunteer can send {}",
            pending.session_id,
            refusal.posture(),
            refusal.detail(),
            pending.body_path.display()
        ),
    }
}

/// Everything `uploads.json` still owes the service.
fn pending_uploads(state: &UploadState) -> Vec<PendingUpload> {
    state
        .sessions
        .iter()
        // A retired entry is still named -- that is what keeps
        // `sweep_unregistered_records` from queuing its row again -- but it is
        // no longer owed (#1149).
        .filter(|(_, entry)| !entry.acknowledged && entry.retired.is_none())
        .map(|(upload_key, entry)| {
            let session_id = entry
                .session_id
                .clone()
                .unwrap_or_else(|| upload_key.clone());
            PendingUpload {
                increment_index: increment_index_of_key(upload_key, &session_id),
                session_id,
                upload_key: upload_key.clone(),
                origin: entry.origin.clone(),
                body_path: entry.body_path.clone(),
            }
        })
        .collect()
}

/// Everything one session still owes, oldest increment first.
///
/// `uploads.json` is a `BTreeMap`, so its keys already sort a session's bodies
/// into increment order: the bare session id (increment zero) sorts before
/// `<id>.increment-1`, and the suffixed keys sort lexically among themselves.
/// That is only a nicety — every body is independent evidence — but sending
/// them in the order they were flown makes a partial upload readable.
fn pending_uploads_for_session(state: &UploadState, session_id: &str) -> Vec<PendingUpload> {
    pending_uploads(state)
        .into_iter()
        .filter(|pending| pending.session_id == session_id)
        .collect()
}

/// Attempt every unacknowledged body, in this thread.
///
/// This runs on the launch sweep's own background thread, so it takes the full
/// [`REQUEST_TIMEOUT`]; the exit path is the one that is budgeted (#1145).
fn flush_pending(state_path: &Path) {
    let state = match read_upload_state(state_path) {
        Ok(state) => state,
        Err(error) => {
            eprintln!("regolith: nothing can be flushed: {error}");
            return;
        }
    };
    for pending in pending_uploads(&state) {
        if send_pending(state_path, &pending, REQUEST_TIMEOUT) {
            eprintln!("regolith: campaign session {} uploaded", pending.session_id);
        }
    }
}

/// Queue every recorded row that no `uploads.json` entry accounts for.
///
/// This is the #1051 failure exactly: `campaign-records.jsonl` held a signed
/// row for a session `uploads.json` had never heard of, because the only code
/// that registered a pending upload lived in a schedule that teardown never
/// ran. A row on disk with no entry beside it is evidence nobody is going to
/// send, so the launch that finds it queues it.
///
/// The telemetry for such a row cannot be recovered: the JSONL stream carries
/// no session id and is append-only across every session the binary played,
/// so there is no honest way to say which bytes were that session's. The row
/// is the signed evidence and goes up with empty telemetry rather than not at
/// all -- and the line printed here says that is what happened.
fn sweep_unregistered_records(state_path: &Path, records_path: &Path, origin: &str) {
    let Ok(records) = std::fs::read_to_string(records_path) else {
        return;
    };
    // Sweeping against a state file that could not be read would re-queue --
    // and re-POST -- every row of every session the install has ever played
    // (#1148). There is nothing to sweep until the read works.
    let state = match read_upload_state(state_path) {
        Ok(state) => state,
        Err(error) => {
            eprintln!(
                "regolith: {} cannot be swept for unqueued rows: {error}",
                records_path.display()
            );
            return;
        }
    };
    let mut queued = std::collections::BTreeSet::new();
    for line in records.lines() {
        let Ok(row) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let Some(session_id) = row
            .get("session_id")
            .and_then(serde_json::Value::as_str)
            .filter(|session_id| !session_id.is_empty())
            .map(str::to_owned)
        else {
            continue;
        };
        // Per increment, not per session (#1048): a session's rows are several
        // bodies, and a sweep that stopped at the session id would leave every
        // increment after the first unqueued -- #1051 again, one row along.
        let key = upload_key(&session_id, increment_index_of(&row));
        if state.sessions.contains_key(&key) || !queued.insert(key) {
            continue;
        }
        let upload = match build_upload_body(vec![row], String::new()) {
            Ok(upload) => upload,
            Err(error) => {
                eprintln!("regolith: campaign session {session_id} cannot be queued: {error}");
                continue;
            }
        };
        match persist_and_register(state_path, origin, &upload) {
            Ok(pending) => eprintln!(
                "regolith: campaign session {session_id} was recorded and never queued for \
                 upload; queued now as {} against {origin}, with empty telemetry because this \
                 run cannot tell which rows of {} were that session's",
                pending.body_path.display(),
                records_path.display()
            ),
            Err(error) => eprintln!(
                "regolith: campaign session {session_id} was recorded and cannot be queued: {error}"
            ),
        }
    }
}

/// Attempt every body this seat owes, and leave a visible retry artifact on failure.
///
/// This used to re-queue the closing row itself first, on the stated grounds
/// that repeating the mint's work was idempotent. It stopped being idempotent
/// the moment a body carried its own span rather than the whole session
/// (#1125): the queue that mints holds the cursor, and a second queue built
/// here from the seat's start would rewrite the tail's body with every byte
/// the seat ever wrote -- which is the very shape the cursor exists to end.
///
/// Nothing is lost by dropping it. Since #1051 the call that mints a row
/// queues its upload, on every teardown path including a `Drop` during a panic
/// unwind, and the two are installed together: `UploadManager` and
/// `UploadQueue` are both conditioned on the same resolved origin, at the
/// lobby's join gate (`join_gate_system`) and at the headless build site
/// (`crate::RegolithPlugin::build`). A row that reached this function has a
/// body on disk and an entry in `uploads.json`. What this adds, and all it
/// ever added, is the immediate attempt.
pub fn upload_finished_session(
    manager: &UploadManager,
    record: &SessionRecord,
    record_path: &Path,
) {
    // This session's bodies only. Exit is not the moment to spend a 45-second
    // request timeout on each *older* body as well; those are the next
    // launch's job, and a quit does not wait to be finished with.
    //
    // Bodies, plural, since #1048. The tail the mint just queued is one of
    // several the seat owes: the increments banked while it was flying are on
    // disk and named by `uploads.json`, and sending only the tail would leave
    // most of a normally-exiting long session waiting for a launch that may
    // not come. The set is bounded by this seat's own increments, and
    // `send_pending` skips anything already acknowledged.
    let state = match read_upload_state(&manager.state_path) {
        Ok(state) => state,
        Err(error) => {
            // Loud, and nothing written. The bodies are on disk and the next
            // launch quarantines the damaged file and recovers them (#1148).
            error!(
                "campaign session {} cannot be uploaded now: {error}; its evidence is on disk \
                 and the next launch will recover it",
                record.session_id
            );
            return;
        }
    };
    let owed = pending_uploads_for_session(&state, &record.session_id);
    if owed.is_empty() {
        // Two opposite facts, and `uploads.json` is what separates them. A
        // seat with entries and nothing owed has already landed; a seat with
        // no entry at all is the #1051 shape -- a signed row on the
        // volunteer's disk that nothing is going to send -- and the mint is
        // the side that failed, so say so where a log will show it.
        if state
            .sessions
            .values()
            .any(|entry| entry.session_id.as_deref() == Some(record.session_id.as_str()))
        {
            info!(
                "campaign session {} has already been acknowledged in full",
                record.session_id
            );
        } else {
            error!(
                "campaign session {} was never queued for upload; the row is on disk at {}                  and the next launch will sweep it",
                record.session_id,
                record_path.display()
            );
        }
        return;
    }
    // Budgeted, because this runs on the render thread (#1145). The systems
    // that reach here live in `Last` (`crate::write_campaign_record_on_exit`),
    // so every second spent below is a second of frozen, undrawn window. An
    // hour-long seat owes ~13 bodies, and against an origin that has gone away
    // each one used to take the full 45-second `REQUEST_TIMEOUT`: ~9m45s of a
    // window a tester reasonably reports as a crash, in exactly the case the
    // retry path exists to survive.
    //
    // Nothing is lost by stopping early. Every body is durable on disk, named
    // by `uploads.json`, and swept by the next launch's background thread,
    // which still takes the full timeout because nobody is waiting on it.
    let deadline = Instant::now() + EXIT_UPLOAD_BUDGET;
    let mut deferred = 0_usize;
    for owed in owed {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            deferred += 1;
            continue;
        }
        if send_pending(&manager.state_path, &owed, remaining) {
            info!(
                "campaign session {} uploaded ({})",
                owed.session_id, owed.upload_key
            );
        }
    }
    if deferred > 0 {
        info!(
            "campaign session {} has {deferred} body(ies) left after this quit's {}s upload \
             budget; they are on disk and the next launch sends them",
            record.session_id,
            EXIT_UPLOAD_BUDGET.as_secs()
        );
    }
}

/// The slice of the telemetry stream one upload body carries, and where it
/// ends.
///
/// A named row rather than a bare tuple: the end offset is what the queue's
/// cursor becomes, and a body paired with the wrong one either repeats a
/// span or skips it.
struct TelemetrySpan {
    jsonl: String,
    /// The offset one past the last byte read, which the next body starts at.
    end: u64,
}

/// Read one body's slice of the telemetry stream, from `start` to end of file.
///
/// A `start` beyond the file's end means the player rotated or replaced the
/// stream under us; the honest answer then is the whole of what is there
/// rather than a silent nothing.
fn read_session_telemetry(path: &Path, start: u64) -> std::io::Result<TelemetrySpan> {
    use std::io::{Read as _, Seek as _, SeekFrom};
    let mut file = std::fs::File::open(path)?;
    let length = file.metadata()?.len();
    let from = if start <= length {
        file.seek(SeekFrom::Start(start))?;
        start
    } else {
        0
    };
    let mut jsonl = String::new();
    file.read_to_string(&mut jsonl)?;
    let end = from.saturating_add(jsonl.len() as u64);
    Ok(TelemetrySpan { jsonl, end })
}

struct UploadBody {
    session_id: String,
    upload_key: String,
    increment_index: u64,
    body: Vec<u8>,
}

fn build_upload_body(
    rows: Vec<serde_json::Value>,
    telemetry_jsonl: String,
) -> Result<UploadBody, String> {
    let session_id = rows
        .first()
        .and_then(|row| row.get("session_id"))
        .and_then(serde_json::Value::as_str)
        .filter(|session_id| !session_id.is_empty())
        .ok_or_else(|| "an upload must contain a row with a non-empty session id".to_owned())?;
    if rows
        .iter()
        .any(|row| row.get("session_id").and_then(serde_json::Value::as_str) != Some(session_id))
    {
        return Err("every upload row must name its own session id".to_owned());
    }
    // Every row of one body is one increment of one seat, so the body's key is
    // that increment's. `rows` is a single row on every path that reaches here.
    let increment_index = rows.first().map_or(0, increment_index_of);
    if rows
        .iter()
        .any(|row| increment_index_of(row) != increment_index)
    {
        return Err("every upload row must name its own seat increment".to_owned());
    }
    let body = serde_json::to_vec(&serde_json::json!({
        "records": rows,
        "telemetry_jsonl": telemetry_jsonl,
    }))
    .map_err(|error| error.to_string())?;
    Ok(UploadBody {
        session_id: session_id.to_owned(),
        upload_key: upload_key(session_id, increment_index),
        increment_index,
        body,
    })
}

/// POST one body and classify the answer (#1149).
///
/// The status used to be read once, formatted into a string, and thrown away,
/// so a `413` and a dropped packet were the same fact to every caller.
fn post_upload(
    origin: &str,
    session_id: &str,
    increment_index: u64,
    body: &[u8],
    timeout: Duration,
) -> Result<(), UploadRefusal> {
    let path = upload_path(session_id, increment_index);
    let response = client(timeout)
        .map_err(UploadRefusal::Transient)?
        .post(format!("{}/{path}", origin.trim_end_matches('/')))
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body(body.to_vec())
        .send()
        // A transport failure is the definition of transient: no service
        // formed an opinion about these bytes at all.
        .map_err(|error| UploadRefusal::Transient(error.to_string()))?;
    if response.status() == reqwest::StatusCode::NO_CONTENT {
        return Ok(());
    }
    let status = response.status();
    let detail = response
        .json::<ErrorResponse>()
        .map(|body| body.detail.unwrap_or_else(|| format!("HTTP {status}")))
        .unwrap_or_else(|_| format!("HTTP {status}"));
    Err(classify_upload_refusal(
        status,
        format!("HTTP {status}: {detail}"),
    ))
}

/// Sweep and retry every unsent session's evidence, in the background.
///
/// Two things can be owed at launch. A body already queued and not
/// acknowledged -- a failed POST, or a process that died between the mint and
/// the send. And a row in `campaign-records.jsonl` that no entry names at
/// all, which is what a teardown path with no upload trigger leaves behind
/// (#1051). Both are swept here, and both are registered before anything is
/// posted, so this call can itself be interrupted without losing ground.
///
/// `origin` is the service this launch would join through, and is the
/// destination for a swept row: a record with no entry has no remembered
/// origin, and the client has exactly one.
///
/// The handle is returned so a test can wait for the sweep; the client drops
/// it and lets the thread run alongside its first frames.
pub fn retry_pending_uploads(telemetry_path: &Path, origin: &str) -> std::thread::JoinHandle<()> {
    // The second refusal (#1060). `queue_finished_session` stops this build
    // minting a row; this stops it posting one an *ordinary* build left behind
    // in the same application-data directory, which is the way a Proton
    // session could otherwise bank somebody else's evidence for them.
    #[cfg(proton_debug)]
    {
        eprintln!("regolith: {}", crate::NOT_BANKABLE_REASON);
        return std::thread::spawn(|| {});
    }
    let state_path = upload_state_path(telemetry_path);
    let records_path = crate::campaign_record_path(telemetry_path);
    let origin = origin.to_owned();
    std::thread::spawn(move || {
        // Order matters. The quarantine is what turns an unreadable file into
        // an absent one, and only then may anything else write; the recovery
        // is what puts the bodies back into the map the quarantine emptied,
        // with the telemetry they were minted with -- which the record sweep
        // below cannot do, because the JSONL stream carries no session id.
        quarantine_damaged_state(&state_path);
        recover_orphaned_bodies(&state_path, &origin);
        repair_stolen_acknowledgements(&state_path);
        sweep_unregistered_records(&state_path, &records_path, &origin);
        flush_pending(&state_path);
    })
}

/// Move an unreadable `uploads.json` aside, loudly, so a launch can proceed.
///
/// The alternative to moving it is a client that refuses every upload for the
/// life of the installation, which is a different way to lose the same
/// evidence. The alternative to moving it *aside* is overwriting it, which is
/// #1148 itself. So it is renamed, kept, and named in the log, and
/// [`recover_orphaned_bodies`] then rebuilds the map from the bodies on disk.
fn quarantine_damaged_state(state_path: &Path) {
    let Err(error) = read_upload_state(state_path) else {
        return;
    };
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_millis());
    let aside = state_path.with_extension(format!("damaged-{stamp}.json"));
    match std::fs::rename(state_path, &aside) {
        Ok(()) => eprintln!(
            "regolith: {error}; it has been moved to {} and the queued evidence on disk will be \
             registered again -- no acknowledgement has been overwritten",
            aside.display()
        ),
        Err(rename) => eprintln!(
            "regolith: {error}; and it cannot be moved aside ({rename}). No evidence will be \
             uploaded or overwritten until {} is repaired or removed by hand",
            state_path.display()
        ),
    }
}

/// Register every `upload-*.json` body that no entry names.
///
/// The bodies are the evidence; `uploads.json` is only the index of them. Two
/// things leave a body with no entry: a crash between [`durable_write`] and
/// the registration in [`persist_and_register`], and a quarantine. Rebuilding
/// from the body preserves the telemetry span it was minted with, which
/// [`sweep_unregistered_records`] cannot -- that path re-mints from the record
/// row alone and says so.
///
/// A retired or acknowledged entry keeps its key, so its body is not orphaned
/// and is not resurrected here.
fn recover_orphaned_bodies(state_path: &Path, origin: &str) {
    let directory = state_path.parent().unwrap_or_else(|| Path::new("."));
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    let mut state = match read_upload_state(state_path) {
        Ok(state) => state,
        Err(error) => {
            eprintln!("regolith: queued evidence cannot be recovered: {error}");
            return;
        }
    };
    let mut recovered = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(upload_key) = path
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(|name| name.strip_prefix("upload-"))
            .and_then(|name| name.strip_suffix(".json"))
            .map(str::to_owned)
        else {
            continue;
        };
        if state.sessions.contains_key(&upload_key) {
            continue;
        }
        // The body names its own session; the key alone cannot, because a
        // session id is itself a key and carries no separator (#1048).
        let session_id = std::fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
            .and_then(|body| {
                body.get("records")?
                    .as_array()?
                    .first()?
                    .get("session_id")?
                    .as_str()
                    .map(str::to_owned)
            });
        let Some(session_id) = session_id.filter(|session_id| !session_id.is_empty()) else {
            eprintln!(
                "regolith: {} is a queued upload body naming no session; it cannot be sent and \
                 is left in place",
                path.display()
            );
            continue;
        };
        state.sessions.insert(
            upload_key.clone(),
            UploadEntry {
                origin: origin.to_owned(),
                body_path: path.clone(),
                acknowledged: false,
                session_id: Some(session_id.clone()),
                attempts: 0,
                retired: None,
            },
        );
        recovered.push(RecoveredBody {
            upload_key,
            session_id,
        });
    }
    if recovered.is_empty() {
        return;
    }
    if let Err(error) = write_upload_state(state_path, &state) {
        eprintln!("regolith: recovered evidence cannot be recorded: {error}");
        return;
    }
    for body in recovered {
        eprintln!(
            "regolith: campaign session {} has a queued body ({}) that {} did not name; it is \
             queued again against {origin}",
            body.session_id,
            body.upload_key,
            state_path.display()
        );
    }
}

/// One body put back into `uploads.json` by [`recover_orphaned_bodies`].
struct RecoveredBody {
    upload_key: String,
    session_id: String,
}

/// Re-open every acknowledgement #1118 could have stolen, once per file.
///
/// While the acknowledgement was keyed on the session id, a successful POST
/// of increment *n* >= 1 marked *increment zero's* entry acknowledged. So on
/// an installation that played a long seat, an increment-zero entry saying
/// `acknowledged` may be saying it about a body the service never took, and
/// nothing on disk distinguishes the two cases.
///
/// The affected population is exactly: an entry keyed by a bare session id
/// whose file still has a sibling under `<id>.increment-n`. Those are re-armed
/// so the sweep sends them again. A re-send of a body the service already
/// holds is free -- the bytes are identical, so admission stores nothing and
/// answers 204 -- while a re-send of one it never got is the five minutes of
/// signed evidence #1118 was dropping.
///
/// Once, and then never again: [`UploadState::increment_acks_repaired`] is
/// set in the same write, because a repair that ran at every launch would be
/// the unbounded re-upload this same issue is about.
fn repair_stolen_acknowledgements(state_path: &Path) {
    let mut state = match read_upload_state(state_path) {
        Ok(state) => state,
        Err(error) => {
            eprintln!("regolith: the #1118 upload repair cannot run: {error}");
            return;
        }
    };
    if state.increment_acks_repaired {
        return;
    }
    let seats_with_increments: std::collections::BTreeSet<String> = state
        .sessions
        .iter()
        .filter(|(upload_key, entry)| {
            let session_id = entry.session_id.as_deref().unwrap_or(upload_key.as_str());
            increment_index_of_key(upload_key, session_id) > 0
        })
        .map(|(upload_key, entry)| {
            entry
                .session_id
                .clone()
                .unwrap_or_else(|| upload_key.clone())
        })
        .collect();
    let mut reopened = Vec::new();
    for session_id in &seats_with_increments {
        let Some(entry) = state.sessions.get_mut(session_id) else {
            continue;
        };
        // A body that is no longer on disk cannot be re-sent, and re-arming it
        // would only leave a pending entry that fails at every launch.
        if !entry.acknowledged || !entry.body_path.exists() {
            continue;
        }
        entry.acknowledged = false;
        reopened.push(session_id.clone());
    }
    state.increment_acks_repaired = true;
    if let Err(error) = write_upload_state(state_path, &state) {
        eprintln!("regolith: cannot record the #1118 upload repair: {error}");
        return;
    }
    for session_id in reopened {
        eprintln!(
            "regolith: campaign session {session_id} increment 0 may have been marked delivered \
             by a later increment's upload (#1118); sending it again"
        );
    }
}

/// Read `uploads.json`, distinguishing *absent* from *unreadable* (#1148).
///
/// This used to be two `.ok()`s and an `unwrap_or_default()`, so a truncated
/// file, a short read, or a schema this build no longer parses all became an
/// **empty** `UploadState` -- indistinguishable from a first launch. The next
/// `write_upload_state` then durably committed that emptiness, orphaning every
/// `upload-*.json` body on disk with nothing left to name it, and saying
/// nothing.
///
/// Absent is a legitimate first launch and defaults. Everything else is an
/// error every caller has to decide about, and the one rule they all follow is
/// **never write state derived from a failed read**.
fn read_upload_state(path: &Path) -> Result<UploadState, String> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(UploadState::default())
        }
        Err(error) => return Err(format!("cannot read {}: {error}", path.display())),
    };
    serde_json::from_slice(&bytes)
        .map_err(|error| format!("cannot parse {}: {error}", path.display()))
}

fn write_upload_state(path: &Path, state: &UploadState) -> std::io::Result<()> {
    let bytes = serde_json::to_vec_pretty(state).map_err(std::io::Error::other)?;
    durable_write(path, &bytes)
}

fn durable_write(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let temporary = path.with_extension("tmp");
    {
        use std::io::Write as _;
        let mut file = std::fs::File::create(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    std::fs::rename(&temporary, path)?;
    sync_parent_directory(path);
    Ok(())
}

/// Flush the directory entry, on the platforms where that is a thing.
///
/// A rename is only durable once the *directory* is synced, so POSIX wants an
/// `fsync` on the parent. Windows has no equivalent: `File::open` on a
/// directory fails with `ERROR_ACCESS_DENIED` — os error 5 — unless the handle
/// is opened with `FILE_FLAG_BACKUP_SEMANTICS`, which `std` does not expose.
///
/// This used to be `File::open(parent)?.sync_all()?` on every platform. The
/// join file was written, synced and renamed successfully, and then that line
/// failed and took the whole write with it, so a Windows volunteer was stopped
/// at the door with **"Could not save the join file: Access is denied (os
/// error 5)"** — the same sentence #766 fixed, from a different cause, with
/// the artifact already on disk.
///
/// Best-effort by design: on POSIX a failure here means the rename may not
/// survive a power cut, which is worth a log line and is not worth failing a
/// join over. On Windows there is nothing to call.
fn sync_parent_directory(path: &Path) {
    #[cfg(not(windows))]
    if let Some(parent) = path.parent() {
        if let Err(error) = std::fs::File::open(parent).and_then(|dir| dir.sync_all()) {
            debug!(
                "could not flush the directory entry for {}: {error}; the write itself succeeded",
                parent.display()
            );
        }
    }
    #[cfg(windows)]
    let _ = path;
}

#[cfg(test)]
mod durable_write_tests {
    use super::*;

    /// A join artifact survives a write, and the write does not depend on
    /// being able to open its parent directory as a file.
    ///
    /// The bug this pins: `durable_write` ended with
    /// `File::open(parent)?.sync_all()?`, which on Windows fails with
    /// `ERROR_ACCESS_DENIED` (os error 5) because a directory cannot be opened
    /// that way. The bytes were already on disk; the `?` discarded that and
    /// returned the error, so a volunteer saw "Could not save the join file:
    /// Access is denied" with the file sitting beside them.
    ///
    /// A Linux-only assertion cannot reproduce the Windows failure, so this
    /// asserts the property that makes the platform difference irrelevant:
    /// the write reports success and the bytes are readable afterwards.
    #[test]
    fn a_durable_write_succeeds_and_leaves_the_bytes_readable() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("nested").join("artifact.json");
        durable_write(&path, b"{\"ok\":true}").expect("durable_write reports success");
        assert_eq!(
            std::fs::read(&path).expect("the artifact is readable"),
            b"{\"ok\":true}",
            "the bytes durable_write claimed to write must be on disk"
        );
    }

    /// The directory flush is advisory: it must not be able to fail a write.
    ///
    /// Calling it on a path whose parent does not exist exercises the error
    /// arm directly. Before the fix that arm was a `?`.
    #[test]
    fn the_directory_flush_cannot_fail_a_write() {
        sync_parent_directory(Path::new("/definitely/not/here/artifact.json"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read as _, Write as _};
    use std::net::TcpListener;
    use std::sync::mpsc::Receiver;

    fn join_test_server(status: &str, body: &str) -> (String, Receiver<Vec<u8>>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test service");
        let address = listener.local_addr().expect("test service address");
        let (sent, received) = mpsc::channel();
        let status = status.to_owned();
        let body = body.to_owned();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept client request");
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .expect("set read timeout");
            let mut request = Vec::new();
            let mut buffer = [0_u8; 1024];
            let body_end = loop {
                let count = stream.read(&mut buffer).expect("read client request");
                assert_ne!(count, 0, "client closed request before its headers");
                request.extend_from_slice(&buffer[..count]);
                if let Some(end) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
                    break end + 4;
                }
            };
            let headers = std::str::from_utf8(&request[..body_end]).expect("ASCII headers");
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
            while request.len() < body_end + length {
                let count = stream.read(&mut buffer).expect("read client body");
                assert_ne!(count, 0, "client closed request before its body");
                request.extend_from_slice(&buffer[..count]);
            }
            sent.send(request).expect("return captured request");
            write!(
                stream,
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .expect("respond to client");
        });
        (format!("http://{address}/v1/campaigns/test/join"), received)
    }

    /// One-shot service that captures a single upload and answers `status`.
    ///
    /// Returns the origin, not a join URL: the upload path builds
    /// `{origin}/v1/sessions/{id}/upload` itself, and the captured request
    /// line is how a test sees which session was posted.
    fn upload_test_server(status: &str) -> (String, Receiver<Vec<u8>>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test service");
        let address = listener.local_addr().expect("test service address");
        let (sent, received) = mpsc::channel();
        let status = status.to_owned();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept client request");
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .expect("set read timeout");
            let mut request = Vec::new();
            let mut buffer = [0_u8; 1024];
            let body_end = loop {
                let count = stream.read(&mut buffer).expect("read client request");
                assert_ne!(count, 0, "client closed request before its headers");
                request.extend_from_slice(&buffer[..count]);
                if let Some(end) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
                    break end + 4;
                }
            };
            let headers = std::str::from_utf8(&request[..body_end]).expect("ASCII headers");
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
            while request.len() < body_end + length {
                let count = stream.read(&mut buffer).expect("read client body");
                assert_ne!(count, 0, "client closed request before its body");
                request.extend_from_slice(&buffer[..count]);
            }
            sent.send(request).expect("return captured request");
            write!(
                stream,
                "HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            )
            .expect("respond to client");
        });
        (format!("http://{address}"), received)
    }

    /// One POST an increment-aware test service took.
    ///
    /// A named row rather than a bare tuple: a path and a body are both
    /// `String`, and the whole point of the #1119 assertions is *which* of the
    /// two a given increment went to.
    #[derive(Clone, Debug)]
    struct CapturedPost {
        path: String,
        body: String,
    }

    /// A standing service that records every POST and refuses chosen paths.
    ///
    /// The multi-increment shape needs three things the one-shot servers above
    /// cannot give: it must survive more than one request, it must say which
    /// *path* each body went to -- that is the whole of #1119 -- and it must be
    /// able to fail one specific increment while taking the next, which is the
    /// #1118 evidence-loss scenario.
    fn increment_test_service(
        refused_paths: &[String],
    ) -> (String, std::sync::Arc<std::sync::Mutex<Vec<CapturedPost>>>) {
        increment_test_service_capped(refused_paths, usize::MAX)
    }

    /// The service's own ceiling on one body, from `scripts/admission.py:35`.
    ///
    /// `_store_upload` refuses anything above it with `413 too_large`
    /// (`scripts/admission.py:949`), and #1002 made the service prove at
    /// startup that a body of exactly this size reaches it through the proxy.
    /// Spelled here because the client has no other copy of it: the point of
    /// #1125's test is that a real session's bodies stay under a number the
    /// client never gets to choose.
    const SERVICE_MAX_UPLOAD_BYTES: usize = 64 * 1024 * 1024;

    /// As [`increment_test_service`], and refusing a body over
    /// `max_body_bytes` with `413` the way the service does.
    fn increment_test_service_capped(
        refused_paths: &[String],
        max_body_bytes: usize,
    ) -> (String, std::sync::Arc<std::sync::Mutex<Vec<CapturedPost>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test service");
        let address = listener.local_addr().expect("test service address");
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let captured = std::sync::Arc::clone(&seen);
        let refused: std::collections::BTreeSet<String> = refused_paths.iter().cloned().collect();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .expect("set read timeout");
                let mut request = Vec::new();
                let mut buffer = [0_u8; 1024];
                let body_end = loop {
                    let Ok(count) = stream.read(&mut buffer) else {
                        return;
                    };
                    if count == 0 {
                        return;
                    }
                    request.extend_from_slice(&buffer[..count]);
                    if let Some(end) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
                        break end + 4;
                    }
                };
                let headers = std::str::from_utf8(&request[..body_end]).expect("ASCII headers");
                let path = headers
                    .lines()
                    .next()
                    .and_then(|line| line.split_whitespace().nth(1))
                    .unwrap_or_default()
                    .to_owned();
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
                while request.len() < body_end + length {
                    let Ok(count) = stream.read(&mut buffer) else {
                        return;
                    };
                    if count == 0 {
                        return;
                    }
                    request.extend_from_slice(&buffer[..count]);
                }
                let body = String::from_utf8_lossy(&request[body_end..]).into_owned();
                // Refused paths are recorded too: a test needs to see that the
                // failed body was attempted at the address it belongs to.
                let refuse = refused.contains(&path);
                let too_large = body.len() > max_body_bytes;
                captured
                    .lock()
                    .expect("capture lock")
                    .push(CapturedPost { path, body });
                let status = if too_large {
                    "413 Payload Too Large"
                } else if refuse {
                    "503 Service Unavailable"
                } else {
                    "204 No Content"
                };
                let _ = write!(
                    stream,
                    "HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                );
            }
        });
        (format!("http://{address}"), seen)
    }

    /// Bank `increments` six-minute increments of one seat and queue each body.
    ///
    /// Six minutes rather than five so every increment clears
    /// [`crate::session::INCREMENT_MINUTES`] with room to spare; the point is
    /// to cross the cadence several times, which is the only way a
    /// second-and-later body exists at all.
    ///
    /// Returns the seat's closing row, which is what the exit path is handed.
    fn queue_a_long_seat(
        queue: &mut UploadQueue,
        session_id: &str,
        increments: u64,
    ) -> SessionRecord {
        let mut session = crate::session::CampaignSession::new(
            session_id.to_owned(),
            "2026-09-06T12:00:00Z".to_owned(),
            crate::session::Actor::Human,
            ConfiguredImpairment {
                loss_pct: 0.0,
                jitter_p50_ms: 0,
                jitter_p99_ms: 0,
            },
        );
        let mut last = None;
        for index in 0..increments {
            for _ in 0..6 * 60 * u64::from(orrery_core::TICK_HZ) {
                session.observe_tick(crate::session::PlayerActivity::Active);
            }
            let record = session
                .finish_increment(
                    format!("2026-09-06T12:{:02}:00Z", (index + 1) * 6),
                    "aarch64-apple-darwin".to_owned(),
                    BUILD_REV.to_owned(),
                    "unavailable-client-side".to_owned(),
                    index + 1 == increments,
                )
                .expect("a six-minute increment covers a non-empty span");
            queue_finished_session(queue, &record).expect("the increment queues");
            last = Some(record);
        }
        last.expect("a seat banks at least one increment")
    }

    /// One `orders` row of the shape the driven-tick path appends, in bytes.
    ///
    /// Written through [`crate::telemetry::JsonlTelemetry::append_orders`]
    /// itself rather than hand-rolled, so the size the rest of this test
    /// reasons about is the client's own and not an estimate of it. The three
    /// orders are the campaign's per-tick set — Thrust, Lock, Fire — at the
    /// codec widths #1125 quoted (13, 9 and 1 bytes), which JSON carries as
    /// arrays of numbers.
    fn one_orders_row(increment: u64) -> Vec<u8> {
        let directory = tempfile::tempdir().expect("temp dir");
        let path = directory.path().join("row.jsonl");
        let packet = crate::intent::OrderPacket {
            // The tick names the increment that wrote the row, which is how
            // the assertion below tells one increment's telemetry from
            // another's.
            tick: increment,
            entity: 4_398_046_511_105,
            orders: vec![vec![0x51; 13], vec![0x27; 9], vec![0x01]],
        };
        {
            let mut sink =
                crate::telemetry::JsonlTelemetry::open(&path).expect("open telemetry stream");
            sink.append_orders(&packet, crate::telemetry::SessionScope::Campaign)
                .expect("append one orders row");
        }
        std::fs::read(&path).expect("read the row back")
    }

    /// Append one increment's worth of driven-tick telemetry to the stream.
    fn append_increment_telemetry(path: &Path, increment: u64, rows: u64) {
        use std::io::Write as _;
        let row = one_orders_row(increment);
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .expect("open telemetry stream for append");
        let mut writer = std::io::BufWriter::new(file);
        for _ in 0..rows {
            writer.write_all(&row).expect("append a telemetry row");
        }
        writer.flush().expect("flush telemetry");
    }

    /// #1125. A session past the service's size cap must still bank, and each
    /// body must carry its own increment's telemetry and no earlier one's.
    ///
    /// `UploadQueue` held a fixed `telemetry_start` — the offset the seat's
    /// rows began at — and `queue_finished_session` read from it to end of
    /// file for *every* body. Increment `k`'s body therefore embedded
    /// increments `0..=k`: total uploaded bytes grew quadratically in session
    /// length, and past roughly ninety minutes of campaign play the last body
    /// crossed `MAX_UPLOAD_BYTES` (`scripts/admission.py:35`) and the service
    /// refused it `413 too_large` — so the session's later evidence stopped
    /// landing exactly where there was most of it.
    ///
    /// A one-increment fixture cannot see this: "the whole session so far" and
    /// "this increment" are the same bytes. So this flies two hours at the
    /// stream's real rate — one `orders` row per driven tick at `TICK_HZ`,
    /// through the client's own writer — against a service that enforces the
    /// real ceiling.
    #[test]
    fn a_two_hour_seat_banks_every_increment_under_the_services_size_cap() {
        const INCREMENT_MINUTES: u64 = 6;
        const INCREMENTS: u64 = 20;
        let rows_per_increment = INCREMENT_MINUTES * 60 * u64::from(orrery_core::TICK_HZ);

        let directory = tempfile::tempdir().expect("temp dir");
        let telemetry_path = directory.path().join("telemetry.jsonl");
        let session_id = "01a06b05-f941-7000-8000-000000001125";
        let (origin, seen) = increment_test_service_capped(&[], SERVICE_MAX_UPLOAD_BYTES);
        let mut queue = UploadQueue::new(origin.clone(), &telemetry_path, 0);

        let mut session = crate::session::CampaignSession::new(
            session_id.to_owned(),
            "2026-09-06T12:00:00Z".to_owned(),
            crate::session::Actor::Human,
            ConfiguredImpairment {
                loss_pct: 0.0,
                jitter_p50_ms: 0,
                jitter_p99_ms: 0,
            },
        );
        for increment in 0..INCREMENTS {
            for _ in 0..rows_per_increment {
                session.observe_tick(crate::session::PlayerActivity::Active);
            }
            // The stream the ticks wrote, appended before the row that closes
            // the increment is minted — the order the driven-tick path uses.
            append_increment_telemetry(&telemetry_path, increment, rows_per_increment);
            let record = session
                .finish_increment(
                    format!(
                        "2026-09-06T{:02}:{:02}:00Z",
                        12 + (increment + 1) / 10,
                        ((increment + 1) * 6) % 60
                    ),
                    "aarch64-apple-darwin".to_owned(),
                    BUILD_REV.to_owned(),
                    "unavailable-client-side".to_owned(),
                    increment + 1 == INCREMENTS,
                )
                .expect("a six-minute increment covers a non-empty span");
            queue_finished_session(&mut queue, &record).expect("the increment queues");
        }

        let stream_bytes = std::fs::metadata(&telemetry_path)
            .expect("the stream exists")
            .len();
        retry_pending_uploads(&telemetry_path, &origin)
            .join()
            .expect("the sweep thread finishes");

        let posts = seen.lock().expect("capture lock").clone();
        let mut paths: Vec<String> = posts.iter().map(|post| post.path.clone()).collect();
        paths.sort();
        let mut wanted = expected_paths(session_id, INCREMENTS);
        wanted.sort();
        assert_eq!(
            paths, wanted,
            "a two-hour seat must post all {INCREMENTS} of its increments"
        );

        // Every body landed. Under the old shape the last one was 71 MB
        // against a 64 MiB ceiling and came back 413, and the state file kept
        // it unacknowledged for a relaunch that would fail the same way.
        let state =
            read_upload_state(&upload_state_path(&telemetry_path)).expect("read the upload state");
        for key in state.sessions.keys() {
            assert!(
                state.sessions[key].acknowledged,
                "{key} was not acknowledged: a two-hour session's evidence must land (#1125)"
            );
        }

        let mut uploaded_rows = 0_u64;
        let mut uploaded_bytes = 0_u64;
        let mut largest = 0_usize;
        for post in &posts {
            largest = largest.max(post.body.len());
            uploaded_bytes = uploaded_bytes.saturating_add(post.body.len() as u64);
            let body: serde_json::Value = serde_json::from_str(&post.body).expect("a body is JSON");
            let index = post
                .path
                .split("/increments/")
                .nth(1)
                .and_then(|rest| rest.split('/').next())
                .and_then(|index| index.parse::<u64>().ok())
                .unwrap_or(0);
            let telemetry = body["telemetry_jsonl"]
                .as_str()
                .expect("telemetry travels as text");
            for line in telemetry.lines() {
                let row: serde_json::Value =
                    serde_json::from_str(line).expect("a telemetry row is JSON");
                assert_eq!(
                    row["packet"]["tick"].as_u64(),
                    Some(index),
                    "increment {index} uploaded a row written by increment {}: a body \
                     must carry its own span and no increment already sent (#1125)",
                    row["packet"]["tick"]
                );
                uploaded_rows += 1;
            }
        }
        // Disjoint *and* complete: the spans of one seat partition its ticks
        // (`crate::session::SeatIncrement`), so the bodies must reconstruct
        // the stream rather than merely avoid repeating it.
        assert_eq!(
            uploaded_rows,
            rows_per_increment * INCREMENTS,
            "the increments' telemetry must sum to the seat's stream"
        );
        assert!(
            largest < SERVICE_MAX_UPLOAD_BYTES,
            "the largest body was {largest} bytes against the service's \
             {SERVICE_MAX_UPLOAD_BYTES}-byte ceiling (#1125)"
        );
        // Linear, not quadratic. Re-reading from the seat's start would have
        // uploaded the stream (INCREMENTS + 1) / 2 = 10.5 times over; carrying
        // its own span uploads it once, plus each body's record and JSON
        // escaping.
        println!(
            "#1125 two-hour seat: stream {stream_bytes} B, {} bodies, {uploaded_bytes} B uploaded \
             ({:.2}x the stream), largest body {largest} B",
            posts.len(),
            uploaded_bytes as f64 / stream_bytes as f64
        );
        assert!(
            uploaded_bytes < stream_bytes.saturating_mul(3),
            "a session uploaded {uploaded_bytes} bytes for a {stream_bytes}-byte stream: \
             the bytes a session uploads must be linear in its length (#1125)"
        );
    }

    /// The address every increment of a seat is posted to.
    ///
    /// Spelled out here rather than deferred to [`upload_path`]: an assertion
    /// that asks the code under test what it expects agrees with it by
    /// construction, and this is the exact claim -- the wire path per
    /// increment -- that #1119 is about.
    fn expected_paths(session_id: &str, increments: u64) -> Vec<String> {
        (0..increments)
            .map(|index| {
                if index == 0 {
                    format!("/v1/sessions/{session_id}/upload")
                } else {
                    format!("/v1/sessions/{session_id}/increments/{index}/upload")
                }
            })
            .collect()
    }

    /// #1119. A seat longer than five minutes must bank *all* of itself.
    ///
    /// Since #1048 a seat banks an increment every
    /// [`crate::session::INCREMENT_MINUTES`], and the client posts each one as
    /// its own body -- but every body went to `/v1/sessions/{id}/upload`, built
    /// from the session id rather than the upload key. The service stores one
    /// pair of files per session id and refuses any second body whose bytes
    /// differ with `409 conflict`, so a 60-minute playtest banked its first
    /// five minutes and nothing else, on that launch and on every later one.
    ///
    /// Five increments, not two: the bug is a cadence, and a fixture that
    /// crosses one boundary cannot tell a fix from an accident. This is the
    /// end-to-end observation `each_increment_of_one_seat_queues_its_own_body`
    /// could not make -- it points the queue at a dead port and asserts
    /// structure. Here the bodies are posted at a service that answers, and
    /// what is asserted is the address each one arrived at.
    #[test]
    fn every_increment_of_a_long_seat_is_posted_to_its_own_address_and_acknowledged() {
        let directory = tempfile::tempdir().expect("temp dir");
        let telemetry_path = directory.path().join("telemetry.jsonl");
        std::fs::write(&telemetry_path, b"{\"row\":1}\n").expect("write telemetry stream");
        let session_id = "01a06b05-f941-7000-8000-000000001119";
        let (origin, seen) = increment_test_service(&[]);
        let mut queue = UploadQueue::new(origin.clone(), &telemetry_path, 0);
        queue_a_long_seat(&mut queue, session_id, 5);

        let state_path = upload_state_path(&telemetry_path);
        retry_pending_uploads(&telemetry_path, &origin)
            .join()
            .expect("the sweep thread finishes");

        let posts = seen.lock().expect("capture lock").clone();
        let addresses = expected_paths(session_id, 5);
        let mut paths: Vec<String> = posts.iter().map(|post| post.path.clone()).collect();
        paths.sort();
        let mut wanted = addresses.clone();
        wanted.sort();
        assert_eq!(
            paths, wanted,
            "a thirty-minute seat must post five increments, each at its own address"
        );
        // Distinctly addressed *and* distinct evidence: five identical bodies
        // at five addresses would satisfy the paths and bank one increment.
        let bodies: std::collections::BTreeSet<&str> =
            posts.iter().map(|post| post.body.as_str()).collect();
        assert_eq!(
            bodies.len(),
            5,
            "five increments posted fewer than five distinct bodies"
        );
        for (index, address) in addresses.iter().enumerate() {
            assert!(
                posts.iter().any(|post| &post.path == address
                    && post.body.contains(&format!("\"index\":{index}"))),
                "no body naming increment {index} arrived at {address}"
            );
        }

        // Each acknowledged against its own key -- #1118's half of the seam.
        let state = read_upload_state(&state_path).expect("read the upload state");
        assert_eq!(state.sessions.len(), 5, "uploads.json lost an increment");
        for index in 0..5 {
            let key = upload_key(session_id, index);
            assert!(
                state
                    .sessions
                    .get(&key)
                    .is_some_and(|entry| entry.acknowledged),
                "increment {index} was not acknowledged under its own key {key}"
            );
        }

        // A relaunch re-posts nothing: the acknowledgements are what stop the
        // unbounded re-upload half of #1118.
        seen.lock().expect("capture lock").clear();
        retry_pending_uploads(&telemetry_path, &origin)
            .join()
            .expect("the second sweep finishes");
        assert!(
            seen.lock().expect("capture lock").is_empty(),
            "a relaunch re-posted evidence the service had already acknowledged"
        );
    }

    /// #1118's evidence loss, stated as the scenario that loses it.
    ///
    /// Increment zero's POST fails and increment one's succeeds. Keyed on the
    /// session id, increment one's success wrote `acknowledged` onto
    /// *increment zero's* entry -- `upload_key(id, 0) == id` -- so five
    /// minutes of measured, signed evidence was marked delivered without ever
    /// having been sent, and was never retried and never logged.
    #[test]
    fn a_landed_increment_never_acknowledges_the_increment_that_failed() {
        let directory = tempfile::tempdir().expect("temp dir");
        let telemetry_path = directory.path().join("telemetry.jsonl");
        std::fs::write(&telemetry_path, b"{\"row\":1}\n").expect("write telemetry stream");
        let session_id = "01a06b05-f941-7000-8000-000000001118";
        let addresses = expected_paths(session_id, 2);
        let zero = addresses[0].clone();
        let one = addresses[1].clone();
        let (origin, seen) = increment_test_service(std::slice::from_ref(&zero));
        let mut queue = UploadQueue::new(origin.clone(), &telemetry_path, 0);
        queue_a_long_seat(&mut queue, session_id, 2);

        let state_path = upload_state_path(&telemetry_path);
        retry_pending_uploads(&telemetry_path, &origin)
            .join()
            .expect("the sweep thread finishes");

        let attempted: Vec<String> = seen
            .lock()
            .expect("capture lock")
            .iter()
            .map(|post| post.path.clone())
            .collect();
        assert!(
            attempted.contains(&zero) && attempted.contains(&one),
            "both increments must be attempted; only one was: {attempted:?}"
        );

        let state = read_upload_state(&state_path).expect("read the upload state");
        assert!(
            state
                .sessions
                .get(&upload_key(session_id, 1))
                .is_some_and(|entry| entry.acknowledged),
            "increment one landed and must be acknowledged"
        );
        assert!(
            !state
                .sessions
                .get(&upload_key(session_id, 0))
                .expect("increment zero is still named")
                .acknowledged,
            "increment one's success marked increment zero delivered; those five signed minutes \
             would never be retried (#1118)"
        );

        // And the retry actually happens: a launch against a service that is
        // no longer refusing sends increment zero, and only increment zero.
        let (recovered_origin, recovered) = increment_test_service(&[]);
        // The entry remembers the origin it was queued against, so point the
        // failed body at the service that is answering now.
        let mut state = read_upload_state(&state_path).expect("read the upload state");
        for entry in state.sessions.values_mut() {
            entry.origin = recovered_origin.clone();
        }
        write_upload_state(&state_path, &state).expect("rewrite the retry state");
        retry_pending_uploads(&telemetry_path, &recovered_origin)
            .join()
            .expect("the recovery sweep finishes");
        let resent: Vec<String> = recovered
            .lock()
            .expect("capture lock")
            .iter()
            .map(|post| post.path.clone())
            .collect();
        assert_eq!(
            resent,
            vec![zero],
            "the next launch must resend the increment that failed, and nothing else"
        );
        assert!(
            read_upload_state(&state_path)
                .expect("read the upload state")
                .sessions
                .get(&upload_key(session_id, 0))
                .is_some_and(|entry| entry.acknowledged),
            "the resent increment zero is acknowledged"
        );
    }

    /// An acknowledgement #1118 could have stolen is re-armed exactly once.
    ///
    /// A state file written by the broken client cannot say whether increment
    /// zero's `acknowledged` was earned or was written by increment one's
    /// success. The repair re-sends it -- free when the service already holds
    /// it, five recovered minutes when it does not -- and then records that it
    /// has run, because a repair at every launch is the unbounded re-upload
    /// this same issue is about.
    #[test]
    fn an_acknowledgement_the_old_bug_could_have_stolen_is_resent_once() {
        let directory = tempfile::tempdir().expect("temp dir");
        let telemetry_path = directory.path().join("telemetry.jsonl");
        std::fs::write(&telemetry_path, b"{\"row\":1}\n").expect("write telemetry stream");
        let session_id = "01a06b05-f941-7000-8000-00000000dead";
        let (origin, seen) = increment_test_service(&[]);
        let mut queue = UploadQueue::new(origin.clone(), &telemetry_path, 0);
        queue_a_long_seat(&mut queue, session_id, 2);

        // Exactly what the broken client left behind: increment one is still
        // owed, and increment zero claims an acknowledgement it may never have
        // earned.
        let state_path = upload_state_path(&telemetry_path);
        let mut state = read_upload_state(&state_path).expect("read the upload state");
        state
            .sessions
            .get_mut(session_id)
            .expect("increment zero is named")
            .acknowledged = true;
        state.increment_acks_repaired = false;
        write_upload_state(&state_path, &state).expect("write the broken state");

        retry_pending_uploads(&telemetry_path, &origin)
            .join()
            .expect("the repairing sweep finishes");
        let mut paths: Vec<String> = seen
            .lock()
            .expect("capture lock")
            .iter()
            .map(|post| post.path.clone())
            .collect();
        paths.sort();
        let mut wanted = expected_paths(session_id, 2);
        wanted.sort();
        assert_eq!(
            paths, wanted,
            "the repair must resend the increment zero whose acknowledgement is in doubt"
        );

        seen.lock().expect("capture lock").clear();
        retry_pending_uploads(&telemetry_path, &origin)
            .join()
            .expect("the second sweep finishes");
        assert!(
            seen.lock().expect("capture lock").is_empty(),
            "the repair ran twice; that is the unbounded re-upload it exists to end"
        );
    }

    /// A multi-shot upload service that answers `status` to every POST.
    fn upload_test_service(
        status: &str,
    ) -> (String, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test service");
        let address = listener.local_addr().expect("test service address");
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let captured = std::sync::Arc::clone(&seen);
        let status = status.to_owned();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .expect("set read timeout");
                let mut request = Vec::new();
                let mut buffer = [0_u8; 1024];
                let body_end = loop {
                    let Ok(count) = stream.read(&mut buffer) else {
                        return;
                    };
                    if count == 0 {
                        return;
                    }
                    request.extend_from_slice(&buffer[..count]);
                    if let Some(end) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
                        break end + 4;
                    }
                };
                let headers = std::str::from_utf8(&request[..body_end]).expect("ASCII headers");
                let line = headers.lines().next().unwrap_or_default().to_owned();
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
                while request.len() < body_end + length {
                    let Ok(count) = stream.read(&mut buffer) else {
                        return;
                    };
                    if count == 0 {
                        return;
                    }
                    request.extend_from_slice(&buffer[..count]);
                }
                captured.lock().expect("capture lock").push(line);
                let _ = write!(
                    stream,
                    "HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                );
            }
        });
        (format!("http://{address}"), seen)
    }

    /// Every increment after a seat's first must record its own acknowledgement.
    #[test]
    fn a_banked_increment_records_its_own_acknowledgement() {
        let directory = tempfile::tempdir().expect("temp dir");
        let telemetry_path = directory.path().join("telemetry.jsonl");
        std::fs::write(&telemetry_path, b"{\"banked_minutes\":5.0}\n").expect("write telemetry");
        let session_id = "01a06b05-52e9-7000-8000-00000000beef";
        // A seat longer than INCREMENT_MINUTES banks increment 0, then 1.
        std::fs::write(
            crate::campaign_record_path(&telemetry_path),
            format!(
                "{{\"session_id\":\"{session_id}\",\"increment\":{{\"index\":0}}}}\n\
                 {{\"session_id\":\"{session_id}\",\"increment\":{{\"index\":1}}}}\n"
            ),
        )
        .expect("write the minted rows");
        let state_path = upload_state_path(&telemetry_path);

        let (origin, seen) = upload_test_service("204 No Content");
        retry_pending_uploads(&telemetry_path, &origin)
            .join()
            .expect("the sweep thread finishes");

        let posted = seen.lock().expect("capture lock").len();
        assert_eq!(posted, 2, "both increments are posted on the first launch");

        let state = read_upload_state(&state_path).expect("read the upload state");
        let first = state
            .sessions
            .get(session_id)
            .expect("increment zero is named");
        assert!(first.acknowledged, "increment zero is acknowledged");
        let second = state
            .sessions
            .get(&format!("{session_id}.increment-1"))
            .expect("increment one is named");
        assert!(
            second.acknowledged,
            "a 204 means the service holds increment one too; a later launch must not resend it"
        );
    }

    /// A minted row that `uploads.json` never heard of is sent on the next launch.
    ///
    /// This is #1051 in one assertion. Two macOS volunteers played, the
    /// client measured and signed their sessions and appended them to
    /// `campaign-records.jsonl`, and the rows were never uploaded and never
    /// even *queued*: `uploads.json` held no entry for them, so the retry
    /// that runs at every launch had nothing to retry and the evidence had to
    /// be hand-carried. The trigger lived in a `Last` system that a macOS
    /// Cmd+Q does not reach, while the row was minted in `Drop`, which every
    /// teardown reaches -- so the two could diverge, and did.
    ///
    /// The fix makes the record the thing that is swept, not the queue entry:
    /// a row on disk with nothing beside it is an upload owed.
    #[test]
    fn a_recorded_row_with_no_upload_entry_is_uploaded_on_the_next_launch() {
        let directory = tempfile::tempdir().expect("temp dir");
        let telemetry_path = directory.path().join("telemetry.jsonl");
        std::fs::write(&telemetry_path, b"{\"banked_minutes\":12.355}\n")
            .expect("write telemetry stream");
        let session_id = "01a06b05-52e9-7000-8000-00000000feed";
        std::fs::write(
            crate::campaign_record_path(&telemetry_path),
            format!("{{\"session_id\":\"{session_id}\",\"banked_minutes\":12.355}}\n"),
        )
        .expect("write the minted row");
        let state_path = upload_state_path(&telemetry_path);
        assert!(
            !state_path.exists(),
            "the failing case is a row with no upload state beside it at all"
        );

        let (origin, received) = upload_test_server("204 No Content");
        retry_pending_uploads(&telemetry_path, &origin)
            .join()
            .expect("the sweep thread finishes");

        let request = received
            .recv_timeout(Duration::from_secs(10))
            .expect("the swept row is posted");
        let request = String::from_utf8(request).expect("UTF-8 request");
        assert!(
            request.starts_with(&format!("POST /v1/sessions/{session_id}/upload ")),
            "the swept row must be posted as its own session: {request}"
        );
        assert!(
            request.contains(&format!("\"session_id\":\"{session_id}\"")),
            "the posted body must carry the recorded row verbatim: {request}"
        );

        let state = read_upload_state(&state_path).expect("read the upload state");
        let entry = state
            .sessions
            .get(session_id)
            .expect("the swept row is now named by uploads.json");
        assert!(
            entry.acknowledged,
            "a 204 means the service holds it; a second launch must not send it again"
        );
        assert!(
            entry.body_path.exists(),
            "the exact bytes posted stay on disk for the volunteer to hand over"
        );
    }

    /// A queued body is named as pending before anything is posted.
    ///
    /// The ordering is the durability: a body written and posted before it is
    /// registered is invisible to every later launch if the process dies in
    /// between, which is the same divergence #1051 was opened for from the
    /// other end.
    #[test]
    fn queueing_registers_the_pending_upload_without_touching_the_network() {
        let directory = tempfile::tempdir().expect("temp dir");
        let telemetry_path = directory.path().join("telemetry.jsonl");
        std::fs::write(&telemetry_path, b"{\"row\":1}\n").expect("write telemetry stream");
        // A port nothing is listening on: a queue that reached the network
        // would fail here rather than return.
        let mut queue = UploadQueue::new("http://127.0.0.1:1".to_owned(), &telemetry_path, 0);
        let record = crate::session::CampaignSession::new(
            "01a06b05-f941-7000-8000-0000000000ff".to_owned(),
            "2026-09-04T12:00:00Z".to_owned(),
            crate::session::Actor::Human,
            ConfiguredImpairment {
                loss_pct: 0.0,
                jitter_p50_ms: 0,
                jitter_p99_ms: 0,
            },
        )
        .finish(
            "2026-09-04T12:04:11Z".to_owned(),
            "aarch64-apple-darwin".to_owned(),
            BUILD_REV.to_owned(),
            "unavailable-client-side".to_owned(),
        );
        let pending = queue_finished_session(&mut queue, &record).expect("the row queues");
        assert!(pending.body_path().exists(), "the body is on disk");
        let state =
            read_upload_state(&upload_state_path(&telemetry_path)).expect("read the upload state");
        let entry = state
            .sessions
            .get(&record.session_id)
            .expect("the row is registered as pending before any POST");
        assert!(
            !entry.acknowledged,
            "nothing has been posted, so nothing is acknowledged"
        );
    }

    /// #1048. A seat that banks in increments owes several bodies under one
    /// session id. Keyed on the id alone, each increment overwrote the last
    /// one's `uploads.json` entry *and* its file on disk, so a crashed session
    /// would have uploaded only whichever increment happened to be last —
    /// which is #1051's loss with extra steps.
    #[test]
    fn each_increment_of_one_seat_queues_its_own_body() {
        let directory = tempfile::tempdir().expect("temp dir");
        let telemetry_path = directory.path().join("telemetry.jsonl");
        std::fs::write(&telemetry_path, b"{\"row\":1}\n").expect("write telemetry stream");
        let mut queue = UploadQueue::new("http://127.0.0.1:1".to_owned(), &telemetry_path, 0);
        let mut session = crate::session::CampaignSession::new(
            "01a06b05-f941-7000-8000-000000001048".to_owned(),
            "2026-09-05T12:00:00Z".to_owned(),
            crate::session::Actor::Human,
            ConfiguredImpairment {
                loss_pct: 0.0,
                jitter_p50_ms: 0,
                jitter_p99_ms: 0,
            },
        );
        let mut bodies = Vec::new();
        for index in 0..3 {
            for _ in 0..6 * 60 * u64::from(orrery_core::TICK_HZ) {
                session.observe_tick(crate::session::PlayerActivity::Active);
            }
            let record = session
                .finish_increment(
                    format!("2026-09-05T12:{:02}:00Z", (index + 1) * 6),
                    "aarch64-apple-darwin".to_owned(),
                    BUILD_REV.to_owned(),
                    "unavailable-client-side".to_owned(),
                    index == 2,
                )
                .expect("a six-minute increment covers a non-empty span");
            let pending =
                queue_finished_session(&mut queue, &record).expect("the increment queues");
            assert!(pending.body_path().exists(), "the body is on disk");
            bodies.push(pending.body_path().to_owned());
        }
        assert_eq!(
            bodies
                .iter()
                .collect::<std::collections::BTreeSet<_>>()
                .len(),
            3,
            "three increments wrote fewer than three distinct bodies"
        );
        let state =
            read_upload_state(&upload_state_path(&telemetry_path)).expect("read the upload state");
        assert_eq!(
            state.sessions.len(),
            3,
            "uploads.json owes {} bodies for a seat that banked three increments",
            state.sessions.len()
        );
        for entry in state.sessions.values() {
            assert_eq!(
                entry.session_id.as_deref(),
                Some("01a06b05-f941-7000-8000-000000001048"),
                "an entry lost the session id the POST path is built from"
            );
        }
    }

    #[test]
    fn the_join_gate_systems_do_not_run_once_the_gate_is_gone() {
        // A successful join removes `JoinGate` inside `poll_worker`, and the
        // UI systems chained behind it take `Res`/`ResMut<JoinGate>`. Without
        // the run condition they execute in that same tick and Bevy panics
        // with "Resource does not exist" — so the client died immediately
        // *after* admitting, which reads as a crash on success. Observed live
        // against the deployed service (#491).
        let mut app = App::new();
        // Exactly the real schedule, not a gated variant: the point is that
        // these systems survive the tick in which `poll_worker` deletes the
        // gate, and a run condition cannot express that because it is
        // evaluated before the deletion happens.
        // The post-join world for the two UI systems, which need nothing but
        // the resources `poll_worker` deletes. `poll_worker` itself takes the
        // same pair optionally and is exercised by running the real client
        // against the live service — constructing an `ActiveSession` here
        // would cost more than it proves.
        app.add_systems(Update, (sync_nickname, rebuild_ui).chain());
        app.update();
    }

    #[test]
    fn nickname_entry_accepts_only_text_the_default_font_can_draw() {
        assert!(valid_nickname("ada"));
        assert!(valid_nickname("Ada 7"));
        assert!(!valid_nickname("   "));
        assert!(!valid_nickname("Ren\u{e9}e"));
        assert!(!valid_nickname("a\u{7}b"));
    }

    fn args(values: &[&str]) -> Vec<OsString> {
        values.iter().map(OsString::from).collect()
    }

    #[test]
    fn admission_url_precedence_is_flag_then_environment_then_baked_default() {
        assert_eq!(
            resolve_admission_url(
                &args(&["regolith", "--admission-url", "https://flag.invalid/"]),
                Some("https://environment.invalid".to_owned()),
            ),
            "https://flag.invalid"
        );
        assert_eq!(
            resolve_admission_url(
                &args(&["regolith"]),
                Some("https://environment.invalid/".to_owned())
            ),
            "https://environment.invalid"
        );
        assert_eq!(
            resolve_admission_url(&args(&["regolith"]), None),
            DEFAULT_ADMISSION_URL
        );
    }

    #[test]
    fn a_live_empty_campaign_list_is_browsing_not_unreachable() {
        let gate = browsing_from_live_response(CampaignsResponse {
            campaigns: Vec::new(),
            operator_note: Some("between runs".to_owned()),
        });
        assert!(matches!(
            gate,
            JoinGate::Browsing {
                campaigns,
                operator_note: Some(_),
                dialog: None,
            } if campaigns.is_empty()
        ));
    }

    /// #711: a headless session installs its uploader from the roster URL it
    /// joined through. Every one of the service's 131 session directories was
    /// a headless client, and not one had uploaded, because only the lobby's
    /// join gate ever installed an `UploadManager`.
    #[test]
    fn a_roster_url_yields_the_origin_its_session_reports_back_to() {
        assert_eq!(
            origin_of_roster_url("https://campaigns.distopik.com/v1/campaigns/shakedown/roster"),
            Some("https://campaigns.distopik.com".to_owned())
        );
        assert_eq!(
            origin_of_roster_url("http://127.0.0.1:8323/v1/campaigns/x/roster"),
            Some("http://127.0.0.1:8323".to_owned())
        );
        // No path after the authority is not a roster URL, and guessing an
        // origin from one would send a player's record somewhere unintended.
        assert_eq!(origin_of_roster_url("https://campaigns.distopik.com"), None);
        assert_eq!(origin_of_roster_url("not-a-url"), None);
    }

    #[test]
    fn upload_body_derives_a_single_non_empty_session_id_from_its_rows() {
        let own = "01917f0e-2b9a-7c4d-8f21-6a0b3c9d1e2f";
        let upload = build_upload_body(
            vec![serde_json::json!({"session_id": own, "actor": "human"})],
            "telemetry\n".to_owned(),
        )
        .expect("matching row builds");
        assert_eq!(upload.session_id, own);
        let decoded: serde_json::Value = serde_json::from_slice(&upload.body).expect("upload JSON");
        assert_eq!(decoded["records"][0]["session_id"], own);
        assert!(build_upload_body(
            vec![
                serde_json::json!({"session_id": own}),
                serde_json::json!({"session_id": "another-session"}),
            ],
            String::new(),
        )
        .is_err());
        assert!(build_upload_body(Vec::new(), String::new()).is_err());
        assert!(
            build_upload_body(vec![serde_json::json!({"session_id": ""})], String::new(),).is_err()
        );
    }

    #[test]
    fn actual_service_listing_shape_deserializes_server_rev() {
        let wire = format!(
            r#"{{"campaigns":[{{"id":"test","title":"Test","state":"busy","peers":8,"seconds":2400,"loss_pct":3,"jitter_ms":100,"client_rev":"{BUILD_REV}","server_rev":"{BUILD_REV}"}}],"operator_note":null}}"#
        );
        let response: CampaignsResponse = serde_json::from_str(&wire).expect("service listing");
        assert_eq!(response.campaigns[0].server_rev.as_deref(), Some(BUILD_REV));
        assert_eq!(
            campaign_state_line(&response.campaigns[0]),
            "busy - try again in ~40 min"
        );
    }

    #[test]
    fn join_post_sends_the_current_regolith_ruleset_version() {
        let accepted = r#"{"join":{"host_node":"node","slot":1,"session_id":"session","session_token":"token"},"host_direct":"127.0.0.1:1","nickname":"ada","configured":{"loss_pct":0.0,"jitter_p50_ms":0,"jitter_p99_ms":0}}"#;
        let (url, request) = join_test_server("200 OK", accepted);

        let answer = post_join(&url, "ada", "node").expect("current ruleset is admitted");
        assert_eq!(answer.nickname.as_deref(), Some("ada"));

        let request = request.recv().expect("captured join request");
        let body_start = request
            .windows(4)
            .position(|bytes| bytes == b"\r\n\r\n")
            .expect("request headers")
            + 4;
        let body: serde_json::Value =
            serde_json::from_slice(&request[body_start..]).expect("join JSON");
        assert_eq!(body["client_rev"], BUILD_REV);
        assert_eq!(body["ruleset_version"], REGOLITH_RULESET.version);
    }

    #[test]
    fn ruleset_version_mismatch_reason_reaches_the_admission_dialog() {
        let reason = format!(
            "This campaign needs ruleset v{} - download the current build.",
            REGOLITH_RULESET.version + 1
        );
        let response = format!(r#"{{"error":"ruleset_version_mismatch","detail":"{reason}"}}"#);
        let (url, request) = join_test_server("403 Forbidden", &response);

        let refusal = match post_join(&url, "ada", "node") {
            Ok(_) => panic!("stale ruleset is refused"),
            Err(refusal) => refusal,
        };
        assert!(!request.recv().expect("captured join request").is_empty());
        let gate = JoinGate::Browsing {
            campaigns: Vec::new(),
            operator_note: None,
            dialog: Some(refusal),
        };
        assert!(matches!(
            gate,
            JoinGate::Browsing {
                dialog: Some(refusal),
                ..
            } if refusal == reason
        ));
    }

    #[test]
    fn headless_refusal_keeps_admissions_machine_readable_error_name() {
        let response = r#"{"error":"client_rev_mismatch","detail":"download the current build"}"#;
        let (url, request) = join_test_server("403 Forbidden", response);

        let refusal = post_join_detailed(&url, "ada", "node")
            .expect_err("a mismatched client revision is refused");
        assert!(!request.recv().expect("captured join request").is_empty());
        assert_eq!(refusal.code.as_deref(), Some("client_rev_mismatch"));
        assert_eq!(refusal.detail, "download the current build");
    }

    /// A damaged `uploads.json` is an error, not an empty map (#1148).
    ///
    /// The old read was `read().ok().and_then(from_slice ... .ok()).
    /// unwrap_or_default()`, so damaged bytes and a first launch were the same
    /// value. This test holds both readings side by side: the exact expression
    /// the old code used, which answers "nothing is queued", and the current
    /// one, which answers "this file cannot be read". Everything else in
    /// #1148 -- the acknowledgements erased, the next write committing the
    /// erasure, the bodies orphaned -- follows from that one collapse.
    #[test]
    fn a_damaged_upload_state_is_an_error_and_not_an_empty_map() {
        let directory = tempfile::tempdir().expect("temp dir");
        let telemetry_path = directory.path().join("telemetry.jsonl");
        std::fs::write(&telemetry_path, b"{\"row\":1}\n").expect("write telemetry stream");
        let session_id = "01a06b05-1148-7000-8000-0000000000aa";
        let (origin, _seen) = increment_test_service(&[]);
        let mut queue = UploadQueue::new(origin.clone(), &telemetry_path, 0);
        queue_a_long_seat(&mut queue, session_id, 2);
        let state_path = upload_state_path(&telemetry_path);

        // A file that was written and then truncated mid-write: the exact
        // shape a power cut or a full disk leaves.
        let good = std::fs::read(&state_path).expect("the state file exists");
        std::fs::write(&state_path, &good[..good.len() / 2]).expect("truncate the state file");

        let old_reading: UploadState = std::fs::read(&state_path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        assert!(
            old_reading.sessions.is_empty(),
            "the old read must be shown losing the queue; if it does not, this test proves nothing"
        );
        assert_eq!(
            pending_uploads(&old_reading).len(),
            0,
            "under the old read this install owed nothing, and the next write said so durably"
        );

        let error = read_upload_state(&state_path).expect_err("a truncated file cannot be read");
        assert!(
            error.contains("uploads.json"),
            "the failure must name the file it is about: {error}"
        );

        // And nothing writes through the failure. `send_pending` is the hot
        // path that used to re-read, re-post and re-write on every body.
        let pending = PendingUpload {
            session_id: session_id.to_owned(),
            upload_key: upload_key(session_id, 0),
            increment_index: 0,
            origin,
            body_path: directory.path().join(format!("upload-{session_id}.json")),
        };
        assert!(
            !send_pending(&state_path, &pending, Duration::from_secs(1)),
            "an unreadable state file must stop the send, not default around it"
        );
        assert_eq!(
            std::fs::read(&state_path).expect("the damaged file is still there"),
            good[..good.len() / 2],
            "the damaged file must not be overwritten: that write is what makes the loss permanent"
        );
    }

    /// The queued evidence survives a damaged `uploads.json` (#1148).
    ///
    /// The bodies are the evidence and `uploads.json` is only an index of
    /// them, so losing the index must cost a log line and not a volunteer's
    /// signed minutes. The launch sweep moves the damaged file aside and
    /// rebuilds the index from the `upload-*.json` bodies, which is what keeps
    /// each body's own telemetry span (#1125) -- the record sweep cannot,
    /// because the JSONL stream carries no session id.
    #[test]
    fn queued_evidence_survives_a_damaged_upload_state() {
        let directory = tempfile::tempdir().expect("temp dir");
        let telemetry_path = directory.path().join("telemetry.jsonl");
        std::fs::write(&telemetry_path, b"{\"row\":1}\n").expect("write telemetry stream");
        let session_id = "01a06b05-1148-7000-8000-0000000000bb";
        // Queued and never sent, which is the population that can be lost.
        let (dead_origin, _dead) = increment_test_service(&[]);
        let mut queue = UploadQueue::new(dead_origin, &telemetry_path, 0);
        queue_a_long_seat(&mut queue, session_id, 2);
        let state_path = upload_state_path(&telemetry_path);
        let bodies: Vec<PathBuf> = pending_uploads(
            &read_upload_state(&state_path).expect("the state file is readable while intact"),
        )
        .into_iter()
        .map(|pending| pending.body_path)
        .collect();
        assert_eq!(bodies.len(), 2, "a two-increment seat queues two bodies");

        std::fs::write(&state_path, b"{\"sessions\": {\"01a0").expect("damage the state file");

        let (origin, seen) = increment_test_service(&[]);
        retry_pending_uploads(&telemetry_path, &origin)
            .join()
            .expect("the recovering sweep finishes");

        let mut posted: Vec<String> = seen
            .lock()
            .expect("capture lock")
            .iter()
            .map(|post| post.path.clone())
            .collect();
        posted.sort();
        let mut wanted = expected_paths(session_id, 2);
        wanted.sort();
        assert_eq!(
            posted, wanted,
            "both queued bodies must reach the service after the index was damaged"
        );
        let state = read_upload_state(&state_path).expect("the rebuilt state file is readable");
        assert!(
            state.sessions.values().all(|entry| entry.acknowledged),
            "the rebuilt index must carry the acknowledgements the resend earned"
        );
        let damaged: Vec<PathBuf> = std::fs::read_dir(directory.path())
            .expect("read the state directory")
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.contains("damaged-"))
            })
            .collect();
        assert_eq!(
            damaged.len(),
            1,
            "the damaged file must be kept aside rather than overwritten: {damaged:?}"
        );
        for body in bodies {
            assert!(
                body.exists(),
                "{} was unlinked; the bodies are the evidence",
                body.display()
            );
        }
    }

    /// A permanent refusal is retired; a transient one is not (#1149).
    ///
    /// `413` is the clearest permanent answer the service has: the body is
    /// over the upload cap and a body does not shrink. `503` is the clearest
    /// transient one. Before this, `post_upload` discarded the status and both
    /// were the string "not sent yet", so the `413` body was re-POSTed at
    /// every launch for the life of the installation.
    #[test]
    fn a_permanent_refusal_is_retired_and_a_transient_one_is_not() {
        for answer in ["413 Payload Too Large", "503 Service Unavailable"] {
            let permanent = answer.starts_with("413");
            let directory = tempfile::tempdir().expect("temp dir");
            let telemetry_path = directory.path().join("telemetry.jsonl");
            std::fs::write(&telemetry_path, b"{\"row\":1}\n").expect("write telemetry stream");
            let session_id = "01a06b05-1149-7000-8000-0000000000cc";
            let (origin, seen) = upload_test_service(answer);
            let mut queue = UploadQueue::new(origin.clone(), &telemetry_path, 0);
            queue_a_long_seat(&mut queue, session_id, 1);

            for _ in 0..3 {
                retry_pending_uploads(&telemetry_path, &origin)
                    .join()
                    .expect("the sweep finishes");
            }

            let posted = seen.lock().expect("capture lock").len();
            let state = read_upload_state(&upload_state_path(&telemetry_path))
                .expect("read the upload state");
            let entry = state
                .sessions
                .get(&upload_key(session_id, 0))
                .expect("the body is still named");
            if permanent {
                assert_eq!(
                    posted, 1,
                    "a permanently refused body must be attempted once, not at every launch"
                );
                assert!(
                    entry.retired.is_some(),
                    "a {answer} body must be retired, and the reason recorded"
                );
                assert!(
                    entry.body_path.exists(),
                    "retiring gives up on sending the body, not on keeping it"
                );
            } else {
                assert_eq!(
                    posted, 3,
                    "a transient refusal must be retried every launch"
                );
                assert!(
                    entry.retired.is_none(),
                    "a {answer} body must stay queued: an outage is not a refusal"
                );
                assert_eq!(entry.attempts, 3, "every attempt is counted and durable");
            }
        }
    }

    /// A `404` is bounded rather than called permanent or transient (#1149).
    ///
    /// The status looks terminal, and for a session the service never admitted
    /// it is. But two live causes make it spurious for a session that *was*
    /// admitted: #1151, where one malformed line in the service's `joins.jsonl`
    /// `404`s every session after it, and the #1135 seam, where a client
    /// posting per-increment addresses to an older service is `404`ed on every
    /// increment past the first. Both are repaired by a deployment, so the
    /// first `404` must not retire the evidence -- and the hundredth must not
    /// keep re-POSTing it.
    #[test]
    fn a_body_refused_as_unknown_is_retried_a_bounded_number_of_times() {
        let directory = tempfile::tempdir().expect("temp dir");
        let telemetry_path = directory.path().join("telemetry.jsonl");
        std::fs::write(&telemetry_path, b"{\"row\":1}\n").expect("write telemetry stream");
        let session_id = "01a06b05-1149-7000-8000-0000000000dd";
        let (origin, seen) = upload_test_service("404 Not Found");
        let mut queue = UploadQueue::new(origin.clone(), &telemetry_path, 0);
        queue_a_long_seat(&mut queue, session_id, 1);
        let state_path = upload_state_path(&telemetry_path);
        let key = upload_key(session_id, 0);

        retry_pending_uploads(&telemetry_path, &origin)
            .join()
            .expect("the first sweep finishes");
        assert!(
            read_upload_state(&state_path)
                .expect("read the upload state")
                .sessions
                .get(&key)
                .is_some_and(|entry| entry.retired.is_none()),
            "one 404 must not retire a body: the service may be the side that is wrong"
        );

        // Enough launches to pass the bound, and then some.
        for _ in 1..UNKNOWN_SESSION_ATTEMPTS + 3 {
            retry_pending_uploads(&telemetry_path, &origin)
                .join()
                .expect("a later sweep finishes");
        }

        assert_eq!(
            seen.lock().expect("capture lock").len(),
            UNKNOWN_SESSION_ATTEMPTS as usize,
            "a body refused as unknown must stop being POSTed once the bound is spent"
        );
        let state = read_upload_state(&state_path).expect("read the upload state");
        let entry = state.sessions.get(&key).expect("the body is still named");
        assert!(
            entry
                .retired
                .as_ref()
                .is_some_and(|reason| reason.contains("unknown")),
            "the retirement must say what it was: {:?}",
            entry.retired
        );
        assert!(
            entry.body_path.exists(),
            "the volunteer's evidence stays on disk and can be sent by hand"
        );
    }

    /// A service that takes the connection and never answers.
    ///
    /// This is the link that dropped, from the client's side: the TCP
    /// handshake still completes (a NAT that has forgotten the flow, a host
    /// that is up but wedged), so nothing fails fast and the client waits out
    /// its whole request timeout. A refused connection would not exercise
    /// #1145 at all.
    fn stalling_test_service() -> String {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test service");
        let address = listener.local_addr().expect("test service address");
        std::thread::spawn(move || {
            // The streams are parked, not dropped: dropping one closes the
            // connection and the client would see an answer of sorts.
            let mut parked = Vec::new();
            for stream in listener.incoming() {
                let Ok(stream) = stream else { break };
                parked.push(stream);
            }
        });
        format!("http://{address}")
    }

    /// Quitting after a link drop returns promptly (#1145).
    ///
    /// The exit path runs in `Last`, on the render thread, so every second it
    /// spends is a frozen window. It used to POST each of the seat's bodies at
    /// the full 45-second `REQUEST_TIMEOUT`: for the three-increment seat
    /// below that is 135 s, and for the hour-long seat the arithmetic in #1145
    /// is about, 13 x 45 s = ~9m45s. The budget makes the whole exit path cost
    /// [`EXIT_UPLOAD_BUDGET`], and the bodies it did not reach stay queued for
    /// the next launch exactly as older bodies always did.
    #[test]
    fn quitting_after_a_link_drop_returns_promptly() {
        let directory = tempfile::tempdir().expect("temp dir");
        let telemetry_path = directory.path().join("telemetry.jsonl");
        std::fs::write(&telemetry_path, b"{\"row\":1}\n").expect("write telemetry stream");
        let session_id = "01a06b05-1145-7000-8000-0000000000ee";
        let origin = stalling_test_service();
        let mut queue = UploadQueue::new(origin, &telemetry_path, 0);
        let record = queue_a_long_seat(&mut queue, session_id, 3);
        let state_path = upload_state_path(&telemetry_path);
        assert_eq!(
            pending_uploads_for_session(
                &read_upload_state(&state_path).expect("read the upload state"),
                session_id
            )
            .len(),
            3,
            "the seat owes three bodies, each of which used to cost REQUEST_TIMEOUT"
        );

        let manager = UploadManager::for_test(&telemetry_path);
        let started = Instant::now();
        upload_finished_session(
            &manager,
            &record,
            &crate::campaign_record_path(&telemetry_path),
        );
        let elapsed = started.elapsed();

        assert!(
            elapsed < EXIT_UPLOAD_BUDGET * 2,
            "quitting took {elapsed:?}; the old path took 3 x {REQUEST_TIMEOUT:?} against this \
             same unanswering origin, and a one-hour seat took ~9m45s"
        );
        // And the evidence is untouched by the deadline: a timeout is
        // transport, which is transient, so nothing is retired and every body
        // is still owed.
        let state = read_upload_state(&state_path).expect("read the upload state");
        assert_eq!(
            pending_uploads_for_session(&state, session_id).len(),
            3,
            "a budgeted quit must defer bodies, never drop them"
        );
        assert!(
            state.sessions.values().all(|entry| entry.retired.is_none()),
            "a stalled origin is transient; retiring on it would lose the volunteer's evidence"
        );
    }
}
