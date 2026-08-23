use bevy::prelude::*;
use orrery_predict::OrreryPredictPlugin;
use orrery_regolith_client::{
    session::require_campaign_consent, session::CONSENT_NOTICE, RegolithSkinPlugin,
};
use std::path::PathBuf;

fn main() {
    let args: Vec<_> = std::env::args_os().collect();
    let smoke_test = args.iter().any(|arg| arg == "--smoke-test");
    let campaign = args.iter().any(|arg| arg == "--campaign");
    let consented = args.iter().any(|arg| arg == "--campaign-consent");
    if campaign {
        eprintln!("{CONSENT_NOTICE}");
        if let Err(reason) = require_campaign_consent(consented) {
            eprintln!("{reason}");
            return;
        }
    }
    let telemetry_path = args
        .windows(2)
        .find(|pair| pair[0] == "--telemetry-jsonl")
        .map(|pair| PathBuf::from(&pair[1]))
        .unwrap_or_else(|| PathBuf::from("target/regolith-client/session.jsonl"));
    let mut app = App::new();
    app.add_plugins(DefaultPlugins.set(WindowPlugin {
        primary_window: Some(Window {
            title: "Orrery: Regolith".into(),
            visible: !smoke_test,
            ..Default::default()
        }),
        ..Default::default()
    }))
    .add_plugins(OrreryPredictPlugin::default())
    .add_plugins(RegolithSkinPlugin::new(telemetry_path));
    if smoke_test {
        app.add_systems(Update, exit_smoke_after_frames);
    }
    app.run();
}

fn exit_smoke_after_frames(mut frames: Local<u8>, mut exit: MessageWriter<AppExit>) {
    *frames = frames.saturating_add(1);
    if *frames >= 3 {
        exit.write(AppExit::Success);
    }
}
