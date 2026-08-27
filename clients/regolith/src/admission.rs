//! Campaign discovery, admission UI, and durable client-evidence upload.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Mutex};
use std::time::Duration;

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

#[derive(Debug, Clone, Deserialize, Serialize)]
struct JoinObject {
    host_node: String,
    slot: usize,
    session_id: String,
    session_token: String,
}

#[derive(Debug, Clone, Deserialize)]
struct JoinResponse {
    join: JoinObject,
    host_direct: String,
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

fn client() -> Result<reqwest::blocking::Client, String> {
    reqwest::blocking::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .build()
        .map_err(|error| error.to_string())
}

fn get_campaigns(url: &str) -> Result<CampaignsResponse, String> {
    let response = client()?
        .get(url)
        .send()
        .map_err(|error| error.to_string())?;
    if response.status().is_success() {
        response.json().map_err(|error| error.to_string())
    } else {
        Err(format!("service answered HTTP {}", response.status()))
    }
}

fn post_join(url: &str, nickname: &str, node: &str) -> Result<JoinResponse, String> {
    let response = client()?
        .post(url)
        .json(&serde_json::json!({
            "nickname": nickname,
            "node": node,
            "client_rev": BUILD_REV,
            "ruleset_version": REGOLITH_RULESET.version,
        }))
        .send()
        .map_err(|error| error.to_string())?;
    if response.status().is_success() {
        response.json().map_err(|error| error.to_string())
    } else {
        let status = response.status();
        Err(response.json::<ErrorResponse>().map_or_else(
            |_| format!("The campaign service answered HTTP {status}."),
            |body| {
                crate::lobby::refusal_sentence(
                    body.error.as_deref(),
                    body.detail.as_deref(),
                    body.retry_after_s,
                )
            },
        ))
    }
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
            let config = CampaignConfig {
                host_node_hex: answer.join.host_node,
                host_direct: Some(answer.host_direct),
                slot: answer.join.slot,
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
            *session =
                ActiveSession::Campaign(Box::new(CampaignRuntime::launch(config, crate::SEED)));
            commands.insert_resource(UploadManager {
                origin: settings.origin.clone(),
                state_path: upload_state_path(&settings.telemetry_path),
            });
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
            GlobalZIndex(1000),
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

fn write_join_artifact(telemetry_path: &Path, join: &JoinObject) -> std::io::Result<PathBuf> {
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

/// Upload destination retained after a service-created join.
#[derive(Resource)]
pub struct UploadManager {
    origin: String,
    state_path: PathBuf,
}

#[derive(Debug, Default, Deserialize, Serialize)]
struct UploadState {
    sessions: BTreeMap<String, UploadEntry>,
}

#[derive(Debug, Deserialize, Serialize)]
struct UploadEntry {
    origin: String,
    body_path: PathBuf,
    acknowledged: bool,
}

fn upload_state_path(telemetry_path: &Path) -> PathBuf {
    telemetry_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("uploads.json")
}

/// Persist an exact upload body, attempt it, and leave a visible retry artifact on failure.
pub fn upload_finished_session(
    manager: &UploadManager,
    record: &SessionRecord,
    record_path: &Path,
    telemetry_path: &Path,
) {
    let telemetry = match std::fs::read_to_string(telemetry_path) {
        Ok(value) => value,
        Err(error) => {
            error!(
                "campaign upload not attempted: cannot read telemetry {}: {error}; records remain at {}",
                telemetry_path.display(),
                record_path.display()
            );
            return;
        }
    };
    let row = match serde_json::to_value(record) {
        Ok(row) => row,
        Err(error) => {
            error!("campaign upload not attempted: cannot serialize row: {error}");
            return;
        }
    };
    let upload = match build_upload_body(vec![row], telemetry) {
        Ok(upload) => upload,
        Err(error) => {
            error!("campaign upload not attempted: {error}");
            return;
        }
    };
    let directory = manager
        .state_path
        .parent()
        .unwrap_or_else(|| Path::new("."));
    let body_path = directory.join(format!("upload-{}.json", upload.session_id));
    if let Err(error) = durable_write(&body_path, &upload.body) {
        error!(
            "campaign upload not attempted: cannot preserve {}: {error}; records remain at {}",
            body_path.display(),
            record_path.display()
        );
        return;
    }
    let mut state = read_upload_state(&manager.state_path);
    state.sessions.insert(
        upload.session_id.clone(),
        UploadEntry {
            origin: manager.origin.clone(),
            body_path: body_path.clone(),
            acknowledged: false,
        },
    );
    if let Err(error) = write_upload_state(&manager.state_path, &state) {
        error!(
            "cannot write upload retry state: {error}; send {} to the operator",
            body_path.display()
        );
        return;
    }
    match post_upload(&manager.origin, &upload.session_id, &upload.body) {
        Ok(()) => {
            if let Some(entry) = state.sessions.get_mut(&upload.session_id) {
                entry.acknowledged = true;
            }
            if let Err(error) = write_upload_state(&manager.state_path, &state) {
                error!("upload succeeded but acknowledgement state could not be saved: {error}");
            } else {
                info!("campaign session {} uploaded", upload.session_id);
            }
        }
        Err(error) => error!(
            "campaign upload failed: {error}; it will retry next launch, and the volunteer can send {}",
            body_path.display()
        ),
    }
}

struct UploadBody {
    session_id: String,
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
    let body = serde_json::to_vec(&serde_json::json!({
        "records": rows,
        "telemetry_jsonl": telemetry_jsonl,
    }))
    .map_err(|error| error.to_string())?;
    Ok(UploadBody {
        session_id: session_id.to_owned(),
        body,
    })
}

fn post_upload(origin: &str, session_id: &str, body: &[u8]) -> Result<(), String> {
    let response = client()?
        .post(format!("{origin}/v1/sessions/{session_id}/upload"))
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body(body.to_vec())
        .send()
        .map_err(|error| error.to_string())?;
    if response.status() == reqwest::StatusCode::NO_CONTENT {
        Ok(())
    } else {
        let status = response.status();
        let detail = response
            .json::<ErrorResponse>()
            .map(|body| body.detail.unwrap_or_else(|| format!("HTTP {status}")))
            .unwrap_or_else(|_| format!("HTTP {status}"));
        Err(detail)
    }
}

/// Retry exact, unacknowledged upload bodies from earlier runs in the background.
pub fn retry_pending_uploads(telemetry_path: &Path) {
    let state_path = upload_state_path(telemetry_path);
    std::thread::spawn(move || {
        let mut state = read_upload_state(&state_path);
        let pending: Vec<_> = state
            .sessions
            .iter()
            .filter(|(_, entry)| !entry.acknowledged)
            .map(|(session, entry)| {
                (
                    session.clone(),
                    entry.origin.clone(),
                    entry.body_path.clone(),
                )
            })
            .collect();
        let mut changed = false;
        for (session, origin, body_path) in pending {
            let result = std::fs::read(&body_path)
                .map_err(|error| error.to_string())
                .and_then(|body| post_upload(&origin, &session, &body));
            match result {
                Ok(()) => {
                    if let Some(entry) = state.sessions.get_mut(&session) {
                        entry.acknowledged = true;
                        changed = true;
                    }
                    eprintln!("campaign session {session} upload retry succeeded");
                }
                Err(error) => eprintln!(
                    "campaign session {session} upload retry failed: {error}; send {} to the operator",
                    body_path.display()
                ),
            }
        }
        if changed {
            if let Err(error) = write_upload_state(&state_path, &state) {
                eprintln!("cannot save upload acknowledgement state: {error}");
            }
        }
    });
}

fn read_upload_state(path: &Path) -> UploadState {
    std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
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
    if let Some(parent) = path.parent() {
        std::fs::File::open(parent)?.sync_all()?;
    }
    Ok(())
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
        let accepted = r#"{"join":{"host_node":"node","slot":1,"session_id":"session","session_token":"token"},"host_direct":"127.0.0.1:1","configured":{"loss_pct":0.0,"jitter_p50_ms":0,"jitter_p99_ms":0}}"#;
        let (url, request) = join_test_server("200 OK", accepted);

        post_join(&url, "ada", "node").expect("current ruleset is admitted");

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
}
