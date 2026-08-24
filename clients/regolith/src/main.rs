use bevy::prelude::*;
use orrery_predict::OrreryPredictPlugin;
use orrery_regolith_client::{
    campaign::CampaignConfig,
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
    let smoke_test = has_flag(&args, "--smoke-test");
    let campaign = has_flag(&args, "--campaign") || args.iter().any(|arg| arg == "--host-node");
    let consented = has_flag(&args, "--campaign-consent");
    if campaign {
        eprintln!("{CONSENT_NOTICE}");
        if let Err(reason) = require_campaign_consent(consented) {
            eprintln!("{reason}");
            return;
        }
    }
    let telemetry_path = flag_value(&args, "--telemetry-jsonl")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("target/regolith-client/session.jsonl"));

    let smoke_ticks =
        flag_value(&args, "--smoke-ticks").and_then(|value| value.parse::<u64>().ok());
    let mut app = App::new();
    app.add_plugins(DefaultPlugins.set(WindowPlugin {
        primary_window: Some(Window {
            title: "Orrery: Regolith".into(),
            // Headless proofs (`--smoke-ticks`) pop no window either.
            visible: !smoke_test && smoke_ticks.is_none(),
            ..Default::default()
        }),
        ..Default::default()
    }))
    .add_plugins(OrreryPredictPlugin::default());

    let mut skin = RegolithSkinPlugin::new(telemetry_path.clone());
    if let Some(host_node) = flag_value(&args, "--host-node") {
        // Joining needs the slot this process occupies: the host derives the
        // slot's transport key from it and refuses a mismatched dialler, so
        // there is no safe default to guess.
        let Some(slot) = flag_value(&args, "--slot").and_then(|value| value.parse::<usize>().ok())
        else {
            eprintln!("--host-node needs --slot <n>: the slot derives your transport identity");
            return;
        };
        // Operator-declared impairment. Shown beside the measurement in the
        // F3 pane and compared against it by the banking row; never
        // substituted for what the link actually did.
        let expect_jitter_ms = flag_value(&args, "--expect-jitter-ms")
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(0);
        let configured = ConfiguredImpairment {
            loss_pct: flag_value(&args, "--expect-loss")
                .and_then(|value| value.parse::<f64>().ok())
                .unwrap_or(0.0),
            jitter_p50_ms: expect_jitter_ms,
            // The criterion's profile declares one spike magnitude, so an
            // unset p99 inherits the p50 declaration rather than claiming a
            // zero the operator never stated.
            jitter_p99_ms: flag_value(&args, "--expect-jitter-p99-ms")
                .and_then(|value| value.parse::<u64>().ok())
                .unwrap_or(expect_jitter_ms),
        };
        let session_id = flag_value(&args, "--session-id")
            .unwrap_or_else(|| format!("local-{}", orrery_regolith_client::BUILD_REV));
        skin = skin.with_campaign(CampaignConfig {
            host_node_hex: host_node,
            host_direct: flag_value(&args, "--host-direct"),
            slot,
            session_id,
            wall_start_utc: orrery_regolith_client::campaign::utc_now_iso8601(),
            configured,
        });
    }
    app.add_plugins(skin);

    if smoke_test {
        app.add_systems(Update, exit_smoke_after_frames);
    }
    if let Some(ticks) = smoke_ticks {
        // Headless campaign proof: join, run N joined ticks, record, exit.
        // Independent of --smoke-test (whose three-frame exitter would race
        // the join); a plain --smoke-test stays three local frames.
        app.insert_resource(SmokeTicks(ticks));
        app.add_systems(Update, exit_smoke_after_joined_ticks);
    }
    app.run();
}

/// Tick budget for the headless campaign proof (`--smoke-ticks`).
#[derive(Resource)]
struct SmokeTicks(u64);

fn exit_smoke_after_frames(mut frames: Local<u8>, mut exit: MessageWriter<AppExit>) {
    *frames = frames.saturating_add(1);
    if *frames >= 3 {
        exit.write(AppExit::Success);
    }
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
