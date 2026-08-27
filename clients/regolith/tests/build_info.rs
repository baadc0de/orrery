use orrery_games::regolith::REGOLITH_RULESET;
use orrery_regolith_client::BUILD_REV;
use std::process::Command;

#[test]
fn build_info_reports_the_binarys_embedded_campaign_identity() {
    let output = Command::new(env!("CARGO_BIN_EXE_orrery_regolith_client"))
        .arg("--build-info")
        .output()
        .expect("the Regolith client binary starts");
    assert!(output.status.success(), "--build-info exits successfully");

    let info: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("--build-info emits JSON");
    assert_eq!(info["client_rev"], BUILD_REV);
    assert_eq!(info["ruleset_version"], REGOLITH_RULESET.version);
}
