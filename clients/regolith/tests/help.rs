use std::process::Command;

#[test]
fn help_prints_usage_without_launching_the_game() {
    let output = Command::new(env!("CARGO_BIN_EXE_orrery_regolith_client"))
        .arg("--help")
        .output()
        .expect("the Regolith client binary starts");

    assert!(output.status.success(), "--help exits successfully");
    let stdout = String::from_utf8(output.stdout).expect("help is UTF-8");
    assert!(stdout.contains("Usage:"));
    assert!(stdout.contains("--admission-url <url>"));
    assert!(stdout.contains("--headless-join <campaign>"));
    assert!(output.stderr.is_empty(), "help does not boot Bevy logging");
}
