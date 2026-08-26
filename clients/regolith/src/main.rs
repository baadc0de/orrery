use bevy::prelude::*;
use orrery_predict::OrreryPredictPlugin;
use orrery_regolith_client::{
    admission::{resolve_admission_url, retry_pending_uploads, AdmissionPlugin},
    campaign::CampaignConfig,
    identity::{load_or_create, resolve_identity_path},
    session::{require_campaign_consent, ConfiguredImpairment, CONSENT_NOTICE},
    RegolithSkinPlugin,
};
use std::path::PathBuf;

fn flag_value(args: &[std::ffi::OsString], flag: &str) -> Option<String> {
    args.windows(2)
        .find(|pair| pair[0] == flag)
        .map(|pair| pair[1].to_string_lossy().into_owned())
}

fn has_flag(args: &[std::ffi::OsString], flag: &str) -> bool {
    args.iter().any(|arg| arg == flag)
}

fn main() {
    let args: Vec<_> = std::env::args_os().collect();
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
    let campaign = has_flag(&args, "--campaign")
        || args
            .iter()
            .any(|arg| arg == "--host-node" || arg == "--join");
    let consented = has_flag(&args, "--campaign-consent");
    if campaign {
        eprintln!("{CONSENT_NOTICE}");
        if let Err(reason) = require_campaign_consent(consented) {
            eprintln!("{reason}");
            return;
        }
    }
    let campaign_input = match orrery_regolith_client::join::resolve_process_campaign_input(&args) {
        Ok(input) => input,
        Err(error) => {
            eprintln!("{error}");
            return;
        }
    };
    if smoke_test {
        run_smoke_test();
        return;
    }
    let transport_secret = match load_or_create(&identity_path) {
        Ok(key) => key,
        Err(error) => {
            eprintln!(
                "cannot load persistent identity {}: {error}",
                identity_path.display()
            );
            return;
        }
    };
    let telemetry_path = flag_value(&args, "--telemetry-jsonl")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("target/regolith-client/session.jsonl"));
    retry_pending_uploads(&telemetry_path);
    let admission_url = resolve_admission_url(&args, std::env::var("ORRERY_ADMISSION_URL").ok());

    let smoke_ticks =
        flag_value(&args, "--smoke-ticks").and_then(|value| value.parse::<u64>().ok());
    let mut app = App::new();
    app.add_plugins(DefaultPlugins.set(WindowPlugin {
        primary_window: Some(Window {
            title: "Orrery: Regolith".into(),
            // Headless proofs (`--smoke-ticks`) pop no window either.
            visible: smoke_ticks.is_none(),
            ..Default::default()
        }),
        ..Default::default()
    }))
    .add_plugins(OrreryPredictPlugin::default());

    let boot_ui = campaign_input.is_none();
    let mut skin = RegolithSkinPlugin::new(telemetry_path.clone());
    if let Some(input) = campaign_input {
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
            session_id: input.session_id,
            session_token_hex: input.session_token,
            wall_start_utc: orrery_regolith_client::campaign::utc_now_iso8601(),
            configured,
            transport_secret: transport_secret.clone(),
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
    app.run();
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
        .add_plugins(RegolithSkinPlugin::new(PathBuf::from(
            "target/regolith-client/smoke.jsonl",
        )));
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
    use super::run_smoke_test;

    #[test]
    fn smoke_test_assembles_the_regolith_skin() {
        run_smoke_test();
    }
}
