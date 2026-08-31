use bevy::prelude::*;
use orrery_games::regolith::REGOLITH_RULESET;
use orrery_predict::OrreryPredictPlugin;
use orrery_regolith_client::{
    admission::{
        admit_headless, resolve_admission_url, retry_pending_uploads, AdmissionPlugin,
        HeadlessAdmission,
    },
    campaign::CampaignConfig,
    identity::{load_or_create, resolve_identity_path},
    session::{require_campaign_consent, ConfiguredImpairment, CONSENT_NOTICE},
    RegolithSkinPlugin, BUILD_REV, DEFAULT_ADMISSION_URL,
};
use std::{path::PathBuf, time::Duration};

const HEADLESS_TIMEOUT_SECS: u64 = 1_020;

const USAGE: &str = "\
Orrery Regolith client

Usage:
  orrery_regolith_client [options]

Volunteer options:
  --admission-url <url>           Override the baked campaign-service origin
  --join <path>                   Join from an operator-provided join file
  --campaign-consent              Acknowledge campaign recording
  --telemetry-jsonl <path>        Session record and join file location
                                  (default: the per-user application data
                                  directory; ORRERY_TELEMETRY_JSONL also sets it)

Preflight and diagnostics:
  --headless-join <campaign>      Select, admit, and join a campaign without input
  --nickname <name>               Nickname used by --headless-join
  --expect-peer <name>            Exit after that peer's craft is replicated (repeatable)
  --headless-timeout-secs <n>     Bound discovery and joining (default: 1020)
  --expect-admission-refusal <id> Succeed only on that named admission refusal
  --build-info                    Print embedded revision, ruleset, and default URL
  --smoke-test                    Assemble the non-rendering client and exit
  --render-smoke                  Prove a rendered frame after 20 seconds
  --help, -h                      Print this usage and exit
";

fn flag_value(args: &[std::ffi::OsString], flag: &str) -> Option<String> {
    args.windows(2)
        .find(|pair| pair[0] == flag)
        .map(|pair| pair[1].to_string_lossy().into_owned())
}

fn flag_values(args: &[std::ffi::OsString], flag: &str) -> Vec<String> {
    args.windows(2)
        .filter(|pair| pair[0] == flag)
        .map(|pair| pair[1].to_string_lossy().into_owned())
        .collect()
}

fn has_flag(args: &[std::ffi::OsString], flag: &str) -> bool {
    args.iter().any(|arg| arg == flag)
}

fn main() {
    let args: Vec<_> = std::env::args_os().collect();
    if has_flag(&args, "--help") || has_flag(&args, "-h") {
        print!("{USAGE}");
        return;
    }
    if has_flag(&args, "--build-info") {
        // This small, dependency-free JSON record is copied into every release
        // archive by package-client.yml.  The packaging step reads the binary,
        // rather than a second hand-maintained version constant, before it
        // publishes the archive.
        println!(
            r#"{{"client_rev":"{BUILD_REV}","ruleset_version":{},"admission_url":"{DEFAULT_ADMISSION_URL}"}}"#,
            REGOLITH_RULESET.version,
        );
        return;
    }

    let headless_campaign = flag_value(&args, "--headless-join");
    let headless_nickname = flag_value(&args, "--nickname");
    let expected_peers = flag_values(&args, "--expect-peer");
    let expected_refusal = flag_value(&args, "--expect-admission-refusal");
    if headless_campaign.is_some() && headless_nickname.is_none() {
        command_error("--headless-join needs --nickname <name>");
    }
    if headless_campaign.is_none()
        && (headless_nickname.is_some() || !expected_peers.is_empty() || expected_refusal.is_some())
    {
        command_error(
            "--nickname, --expect-peer, and --expect-admission-refusal require --headless-join",
        );
    }
    if !expected_peers.is_empty() && expected_refusal.is_some() {
        command_error("--expect-peer cannot be combined with --expect-admission-refusal");
    }
    let headless_timeout_value = flag_value(&args, "--headless-timeout-secs");
    if headless_campaign.is_none() && headless_timeout_value.is_some() {
        command_error("--headless-timeout-secs requires --headless-join");
    }
    let headless_timeout = headless_timeout_value.map_or_else(
        || Duration::from_secs(HEADLESS_TIMEOUT_SECS),
        |value| match value.parse::<u64>() {
            Ok(0) | Err(_) => command_error("--headless-timeout-secs needs a positive integer"),
            Ok(seconds) => Duration::from_secs(seconds),
        },
    );
    let identity_path = resolve_identity_path(&args, std::env::var_os("ORRERY_IDENTITY_FILE"));
    // Compatibility spelling retained for the operator runbook: the slot is
    // validated, but the printed NodeId now comes from the persistent client
    // key rather than from public slot arithmetic.
    if let Some(slot) = flag_value(&args, "--print-slot-key") {
        match slot.parse::<usize>() {
            Ok(_) => match load_or_create(&identity_path) {
                Ok(key) => println!("{}", key.public()),
                Err(error) => eprintln!(
                    "cannot load persistent identity {}: {error}",
                    identity_path.display()
                ),
            },
            Err(_) => eprintln!("--print-slot-key needs a slot number, got {slot:?}"),
        }
        return;
    }
    let smoke_test = has_flag(&args, "--smoke-test");
    let campaign = headless_campaign.is_some()
        || has_flag(&args, "--campaign")
        || args
            .iter()
            .any(|arg| arg == "--host-node" || arg == "--join");
    let consented = has_flag(&args, "--campaign-consent");
    if campaign {
        eprintln!("{CONSENT_NOTICE}");
        if let Err(reason) = require_campaign_consent(consented) {
            if headless_campaign.is_some() {
                preflight_error(reason);
            }
            eprintln!("{reason}");
            return;
        }
    }
    let campaign_input = match orrery_regolith_client::join::resolve_process_campaign_input(&args) {
        Ok(input) => input,
        Err(error) => {
            command_error(&error);
        }
    };
    if smoke_test {
        run_smoke_test();
        return;
    }
    let transport_secret = match load_or_create(&identity_path) {
        Ok(key) => key,
        Err(error) => {
            let detail = format!(
                "identity: cannot load persistent identity {}: {error}",
                identity_path.display()
            );
            if headless_campaign.is_some() {
                preflight_error(&detail);
            }
            eprintln!("{detail}");
            return;
        }
    };
    // Flag, then environment, then the platform's per-user data directory --
    // never a path relative to wherever the volunteer launched the binary
    // (#766). The join artifact and the upload-retry state are written beside
    // whatever this resolves to.
    let telemetry_path = orrery_regolith_client::paths::resolve_telemetry_path(
        &args,
        std::env::var_os("ORRERY_TELEMETRY_JSONL"),
    );
    retry_pending_uploads(&telemetry_path);
    let admission_url = resolve_admission_url(&args, std::env::var("ORRERY_ADMISSION_URL").ok());

    let headless_config = headless_campaign.as_deref().map(|campaign_id| {
        let nickname = headless_nickname
            .as_deref()
            .expect("validated with --headless-join");
        match admit_headless(
            &admission_url,
            campaign_id,
            nickname,
            &transport_secret,
            &telemetry_path,
            headless_timeout,
            expected_refusal.as_deref(),
        ) {
            Ok(HeadlessAdmission::Admitted(config)) => *config,
            Ok(HeadlessAdmission::ExpectedRefusal) => std::process::exit(0),
            Err(error) => preflight_error(&error),
        }
    });

    let smoke_ticks =
        flag_value(&args, "--smoke-ticks").and_then(|value| value.parse::<u64>().ok());
    let render_smoke = has_flag(&args, "--render-smoke");
    let mut app = App::new();
    app.add_plugins(DefaultPlugins.set(WindowPlugin {
        primary_window: Some(Window {
            title: "Orrery: Regolith".into(),
            // Headless proofs (`--smoke-ticks`) pop no window either.
            visible: smoke_ticks.is_none() && headless_campaign.is_none(),
            ..Default::default()
        }),
        ..Default::default()
    }))
    .add_plugins(OrreryPredictPlugin::default());

    let boot_ui = campaign_input.is_none() && headless_config.is_none();
    let mut skin = RegolithSkinPlugin::new(telemetry_path.clone());
    if let Some(config) = headless_config {
        skin = skin.with_campaign(config);
    } else if let Some(input) = campaign_input {
        // Operator-declared impairment. Shown beside the measurement in the
        // F3 pane and compared against it by the banking row; never
        // substituted for what the link actually did.
        let configured = ConfiguredImpairment {
            loss_pct: flag_value(&args, "--expect-loss")
                .and_then(|value| value.parse::<f64>().ok())
                .unwrap_or(0.0),
            jitter_p50_ms: flag_value(&args, "--expect-jitter-ms")
                .and_then(|value| value.parse::<u64>().ok())
                .unwrap_or(0),
            // The p99 expectation defaults to the p50 one: an operator who
            // declared one jitter figure declared it for the profile, and a
            // hardcoded zero here made the p99 half of the mismatch flag
            // unfalsifiable (observed 0 == configured 0 on an idle link).
            jitter_p99_ms: flag_value(&args, "--expect-jitter-p99-ms")
                .or_else(|| flag_value(&args, "--expect-jitter-ms"))
                .and_then(|value| value.parse::<u64>().ok())
                .unwrap_or(0),
        };
        skin = skin.with_campaign(CampaignConfig {
            host_node_hex: input.host_node,
            host_direct: flag_value(&args, "--host-direct"),
            slot: input.slot,
            // A join file grants transport/session material but carries no
            // host-asserted display label. The public roster is not allowed to
            // guess this craft's identity from the slot.
            own_label: None,
            session_id: input.session_id,
            session_token_hex: input.session_token,
            wall_start_utc: orrery_regolith_client::campaign::utc_now_iso8601(),
            configured,
            transport_secret: transport_secret.clone(),
            // The island size, when the operator states it. A join file names
            // a host and a seat, not the island's size, so without the flag
            // this keeps the pre-#573 `slot + 1` derivation — right for the
            // sole human in the last seat, which is what a join file has
            // always described.
            island_seats: flag_value(&args, "--island-seats")
                .and_then(|value| value.parse::<u16>().ok()),
            // A join file names a host, not a campaign, so this path cannot
            // work out on its own which roster to ask for. `--roster-campaign`
            // supplies the id when the operator knows it; without it every
            // ship stays unlabelled, which is the correct answer rather than a
            // degraded one. See `roster::ShipRoster`.
            roster_url: flag_value(&args, "--roster-campaign")
                .map(|id| format!("{admission_url}/v1/campaigns/{id}/roster")),
        });
    }
    app.add_plugins(skin);

    if has_flag(&args, "--overlay-open") {
        app.insert_resource(orrery_regolith_client::OverlayOpen);
    }
    if has_flag(&args, "--capture-zoom-sweep") {
        app.init_resource::<orrery_regolith_client::ZoomSweep>();
    }
    if has_flag(&args, "--capture-geometry") {
        app.insert_resource(orrery_regolith_client::GeometryCapture::auto_drive());
    }
    if let Some(dir) = flag_value(&args, "--capture-frames") {
        match orrery_regolith_client::FrameCapture::into_dir(PathBuf::from(dir)) {
            Ok(capture) => {
                app.insert_resource(capture);
            }
            Err(error) => {
                eprintln!("{error}");
                return;
            }
        }
    }

    if boot_ui {
        app.add_plugins(AdmissionPlugin::new(
            admission_url,
            telemetry_path,
            transport_secret,
        ));
    }

    if let Some(ticks) = smoke_ticks {
        // Headless campaign proof: join, run N joined ticks, record, exit.
        app.insert_resource(SmokeTicks(ticks));
        app.add_systems(Update, exit_smoke_after_joined_ticks);
    }
    if render_smoke {
        // This is intentionally a rendered proof, unlike `--smoke-test` and
        // `--smoke-ticks`: after the client has stayed alive for a while, the
        // screenshot callback only fires once the renderer has produced a
        // primary-window frame. The callback then exits cleanly.
        app.init_resource::<RenderSmoke>()
            .add_systems(Update, render_smoke_after_minimum_uptime);
    }
    if headless_campaign.is_some() {
        app.insert_resource(HeadlessJoinProbe {
            expected_peers,
            observed_peers: std::collections::BTreeSet::new(),
            all_observed_at: None,
            started_at: std::time::Instant::now(),
            timeout: headless_timeout,
            seated_reported: false,
        })
        .add_systems(Update, monitor_headless_join);
    }
    let outcome = app.run();
    if let AppExit::Error(code) = outcome {
        std::process::exit(i32::from(code.get()));
    }
}

fn command_error(message: &str) -> ! {
    eprintln!("error: {message}\n\n{USAGE}");
    std::process::exit(2)
}

fn preflight_error(message: &str) -> ! {
    eprintln!("PREFLIGHT FAIL {message}");
    std::process::exit(1)
}

/// State for one bounded non-interactive campaign proof.
#[derive(Resource)]
struct HeadlessJoinProbe {
    expected_peers: Vec<String>,
    observed_peers: std::collections::BTreeSet<String>,
    started_at: std::time::Instant,
    timeout: Duration,
    seated_reported: bool,
    /// When every expected peer had been observed, if that has happened.
    ///
    /// Observation is mutual and the two sides do not reach it at the same
    /// instant: a client that has been in the attempt since the lobby sees a
    /// late joiner within seconds of its arrival, while the joiner needs a
    /// little longer to accumulate the other side. Exiting the moment this
    /// client is satisfied therefore removes the very craft its peers are
    /// still trying to observe -- measured, the established clients left about
    /// nine seconds after the joiner arrived, before its window even opened,
    /// and the joiner then failed a clause about a peer that no longer
    /// existed. Linger so the proof is of the campaign, not of exit order.
    all_observed_at: Option<std::time::Instant>,
}

/// How long a satisfied probe stays in the attempt so its peers can finish
/// observing it. Comfortably longer than the few seconds a late joiner needs,
/// and far below any run's timeout.
const MUTUAL_OBSERVATION_LINGER: Duration = Duration::from_secs(30);

fn transport_is_seated(state: &orrery_regolith_client::campaign::JoinState, ticks: u64) -> bool {
    matches!(state, orrery_regolith_client::campaign::JoinState::Joined) && ticks > 0
}

fn named_peer_is_observed(
    local: orrery_protocol::PersistId,
    peer: Option<orrery_protocol::PersistId>,
    peer_is_replicated_craft: bool,
    our_broadcast_to_peer_settled: bool,
) -> bool {
    peer.is_some_and(|peer| peer != local)
        && peer_is_replicated_craft
        && our_broadcast_to_peer_settled
}

fn monitor_headless_join(
    mut probe: ResMut<HeadlessJoinProbe>,
    session: Res<orrery_regolith_client::ActiveSession>,
    roster: Res<orrery_regolith_client::roster::ShipRoster>,
    mut exit: MessageWriter<AppExit>,
) {
    let orrery_regolith_client::ActiveSession::Campaign(runtime) = &*session else {
        eprintln!("PREFLIGHT FAIL handshake-seated: client is not in a campaign session");
        exit.write(error_exit());
        return;
    };

    match runtime.state() {
        orrery_regolith_client::campaign::JoinState::Dialing => {}
        orrery_regolith_client::campaign::JoinState::Joined => {
            if !transport_is_seated(runtime.state(), runtime.joined_ticks()) {
                return;
            }
            if !probe.seated_reported {
                println!(
                    "PREFLIGHT PASS handshake-seated slot={} entity={} ticks={}",
                    runtime.config().slot,
                    runtime.entity().0,
                    runtime.joined_ticks()
                );
                probe.seated_reported = true;
            }
            if probe.expected_peers.is_empty() {
                exit.write(AppExit::Success);
                return;
            }
            for expected in probe.expected_peers.clone() {
                if probe.observed_peers.contains(&expected) {
                    continue;
                }
                let peer = roster.entity_named(&expected);
                let peer_state = peer.and_then(|entity| runtime.executor().state(entity));
                let peer_is_replicated_craft = matches!(
                    peer_state,
                    Some(orrery_games::regolith::state::RegolithState::Craft(_))
                );
                let our_broadcast_to_peer_settled =
                    peer.is_some_and(|entity| runtime.replication_is_mutual_with(entity));
                if named_peer_is_observed(
                    runtime.entity(),
                    peer,
                    peer_is_replicated_craft,
                    our_broadcast_to_peer_settled,
                ) {
                    let entity = peer.expect("the observation predicate requires an entity");
                    println!(
                        "PREFLIGHT PASS peer-observed nickname={expected} entity={}",
                        entity.0
                    );
                    probe.observed_peers.insert(expected);
                }
            }
            if probe
                .expected_peers
                .iter()
                .all(|peer| probe.observed_peers.contains(peer))
            {
                let since = *probe
                    .all_observed_at
                    .get_or_insert_with(std::time::Instant::now);
                if since.elapsed() >= MUTUAL_OBSERVATION_LINGER {
                    exit.write(AppExit::Success);
                    return;
                }
            }
        }
        state => {
            eprintln!("PREFLIGHT FAIL handshake-seated: transport ended in {state:?}");
            exit.write(error_exit());
            return;
        }
    }

    if probe.started_at.elapsed() >= probe.timeout {
        if !probe.expected_peers.is_empty() {
            let missing = probe
                .expected_peers
                .iter()
                .filter(|peer| !probe.observed_peers.contains(*peer))
                .collect::<Vec<_>>();
            eprintln!(
                "PREFLIGHT FAIL peer-observed: seated={} peers={missing:?} were not present as mutually replicated remote craft before {} seconds",
                probe.seated_reported,
                probe.timeout.as_secs()
            );
        } else {
            eprintln!(
                "PREFLIGHT FAIL handshake-seated: transport did not seat the client before {} seconds",
                probe.timeout.as_secs()
            );
        }
        exit.write(error_exit());
    }
}

fn error_exit() -> AppExit {
    AppExit::Error(std::num::NonZeroU8::new(1).expect("one is non-zero"))
}

/// Build the client's non-graphics composition and report its outcome.
///
/// This deliberately does not construct a window, adapter, or render pipeline.
/// It proves that the client plugins and their schedules can be assembled; a
/// rendered run remains the coverage for graphics-device capability.
fn run_smoke_test() {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins)
        // `OrreryPredictPlugin` installs lightyear's state-backed resources;
        // unlike `DefaultPlugins`, `MinimalPlugins` does not provide this
        // schedule.
        .add_plugins(bevy::state::app::StatesPlugin)
        .add_plugins(OrreryPredictPlugin::default())
        .add_plugins(RegolithSkinPlugin::new(
            orrery_regolith_client::paths::default_smoke_path(),
        ));
    app.finish();

    // Keep this assertion beside the command's success message: it makes a
    // missing skin installation a named client failure rather than a green
    // process that only initialized Bevy's minimal runtime.
    assert!(
        app.world()
            .contains_resource::<orrery_regolith_client::ActiveSession>(),
        "smoke: client composition failed — RegolithSkinPlugin did not install ActiveSession"
    );
    eprintln!(
        "smoke: client composition passed; graphics were intentionally not initialized (no GPU pipeline coverage)"
    );
}

/// Tick budget for the headless campaign proof (`--smoke-ticks`).
#[derive(Resource)]
struct SmokeTicks(u64);

/// State for the bounded rendered-client smoke proof.
#[derive(Default, Resource)]
struct RenderSmoke {
    started_at: Option<Duration>,
    screenshot_requested: bool,
}

/// Stay alive long enough to catch delayed compositor failures, then require a
/// completed renderer screenshot before declaring the rendered smoke proof a
/// success. A renderer that never reaches the screenshot callback is fatal
/// rather than leaving the packaging runner stuck indefinitely.
fn render_smoke_after_minimum_uptime(
    time: Res<Time>,
    mut smoke: ResMut<RenderSmoke>,
    mut commands: Commands,
    mut exit: MessageWriter<AppExit>,
) {
    use bevy::render::view::screenshot::{Screenshot, ScreenshotCaptured};

    // #550's Wayland teardown arrived 7--17 seconds after launch. Keep the
    // minimum above that observed range before asking the renderer to prove a
    // frame, so this mode would have rejected that failure.
    const MINIMUM_UPTIME: Duration = Duration::from_secs(20);
    const TIMEOUT: Duration = Duration::from_secs(60);

    let elapsed = time.elapsed();
    let started_at = smoke.started_at.get_or_insert(elapsed);
    let running_for = elapsed.saturating_sub(*started_at);
    if running_for >= TIMEOUT {
        error!(
            "smoke: renderer did not capture a frame within {} seconds",
            TIMEOUT.as_secs()
        );
        exit.write(AppExit::Error(
            std::num::NonZeroU8::new(1).expect("one is non-zero"),
        ));
        return;
    }
    if running_for < MINIMUM_UPTIME || smoke.screenshot_requested {
        return;
    }

    smoke.screenshot_requested = true;
    commands.spawn(Screenshot::primary_window()).observe(
        move |_captured: On<ScreenshotCaptured>, mut exit: MessageWriter<AppExit>| {
            info!(
                "smoke: renderer captured a primary-window frame after at least {} seconds; exiting successfully",
                MINIMUM_UPTIME.as_secs()
            );
            exit.write(AppExit::Success);
        },
    );
}

fn exit_smoke_after_joined_ticks(
    budget: Res<SmokeTicks>,
    session: Res<orrery_regolith_client::ActiveSession>,
    mut exit: MessageWriter<AppExit>,
) {
    // One write per process: after this frame the runner stops, so no
    // second-fire guard is needed — and holding a MessageReader here as well
    // would be a conflicting parameter pair on `Messages<AppExit>`.
    let orrery_regolith_client::ActiveSession::Campaign(runtime) = &*session else {
        return;
    };
    match runtime.state() {
        orrery_regolith_client::campaign::JoinState::Dialing => {}
        orrery_regolith_client::campaign::JoinState::Joined => {
            if runtime.joined_ticks() >= budget.0 {
                info!(
                    "smoke: {} joined ticks reached; recording and exiting",
                    budget.0
                );
                exit.write(AppExit::Success);
            }
        }
        state => {
            error!("smoke: campaign did not join — {state:?}");
            exit.write(AppExit::Error(
                std::num::NonZeroU8::new(1).expect("one is non-zero"),
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{flag_values, named_peer_is_observed, run_smoke_test, transport_is_seated};
    use orrery_protocol::PersistId;
    use orrery_regolith_client::campaign::JoinState;
    use std::ffi::OsString;

    #[test]
    fn repeated_expect_peer_values_are_preserved_in_order() {
        let args = [
            "client",
            "--expect-peer",
            "second-human",
            "--expect-peer",
            "third-human",
        ]
        .map(OsString::from);

        assert_eq!(
            flag_values(&args, "--expect-peer"),
            ["second-human", "third-human"]
        );
    }

    #[test]
    fn smoke_test_assembles_the_regolith_skin() {
        run_smoke_test();
    }

    #[test]
    fn headless_join_requires_the_transport_to_reach_joined() {
        assert!(!transport_is_seated(&JoinState::Dialing, 1));
        assert!(!transport_is_seated(&JoinState::Joined, 0));
        assert!(transport_is_seated(&JoinState::Joined, 1));
    }

    #[test]
    fn headless_join_requires_the_named_peer_to_be_a_replicated_remote_craft() {
        let local = PersistId::new(7);
        let remote = PersistId::new(8);
        assert!(!named_peer_is_observed(local, None, false, false));
        assert!(!named_peer_is_observed(local, Some(remote), false, true));
        assert!(!named_peer_is_observed(local, Some(local), true, true));
        assert!(named_peer_is_observed(local, Some(remote), true, true));
    }

    #[test]
    fn headless_join_waits_for_our_broadcast_to_the_observed_peer_to_settle() {
        let local = PersistId::new(6);
        let remote = PersistId::new(7);
        assert!(
            !named_peer_is_observed(local, Some(remote), true, false),
            "receiving the peer is only one half of mutual observation"
        );
    }
}
