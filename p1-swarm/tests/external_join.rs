//! Two processes, one island (#385).
//!
//! The host runs a witnessed, impaired swarm with an external slot attached;
//! this test spawns it, reads the listening line off its stderr, dials with
//! the external runner as a second process, and holds the resulting report to
//! the same clauses the pure-bot legs must satisfy. A join that fails to
//! happen, a slot that moves no frames, or a disconnect mid-run each fail —
//! which is the whole point of the criterion clauses added with the exterior
//! slot.
//!
//! Ignored by default because it is real-time (`--seconds` here measures wall
//! clock); `scripts/p1-swarm-gate.sh` runs it explicitly.

#![allow(missing_docs)]

use std::process::{Command, Stdio};
use std::time::Duration;

/// Long enough for two Bevy apps to boot and one 8-second real-time run to
/// finish; short enough that a hang costs a CI minute, not ten.
const PROCESS_TIMEOUT: Duration = Duration::from_secs(120);

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_p1-swarm")
}

#[test]
#[ignore = "two real processes at wall clock; run via scripts/p1-swarm-gate.sh or --ignored"]
fn an_external_peer_joins_witnesses_and_moves_frames() {
    let dir = std::env::temp_dir().join(format!("p1-external-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let report_path = dir.join("report.json");

    // The host: impaired and witnessed like every other P4 leg, plus the
    // external slot. Its stderr is inherited (evidence lands in the test
    // log for free); the dial target comes from a listening file, which is
    // deterministic where stream parsing was not.
    let host_err = dir.join("host.err");
    let host_err = host_err.as_os_str().to_str().unwrap().to_owned();
    let listening_path = dir.join("listening.txt");
    let mut host = Command::new(bin())
        .args([
            "--peers",
            "4",
            "--seconds",
            "8",
            "--min-cells",
            "1",
            "--seed",
            "7",
            "--impaired",
            "--witness",
            "--external-peer",
            "--join-timeout-secs",
            "60",
            "--json",
        ])
        .arg(&report_path)
        .arg("--listening-file")
        .arg(&listening_path)
        .stdout(Stdio::null())
        .stderr(std::process::Stdio::from(
            std::fs::File::create(&host_err).expect("host err file"),
        ))
        .spawn()
        .expect("host process starts");

    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    let line = loop {
        match std::fs::read_to_string(&listening_path) {
            Ok(line) if !line.trim().is_empty() => break line,
            _ => {}
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the host never wrote its listening file"
        );
        std::thread::sleep(Duration::from_millis(100));
    };
    let mut parts = line.split_whitespace();
    let node_hex = parts.next().expect("node id").to_owned();
    let direct = parts.next().map(|d| d.replace("0.0.0.0", "127.0.0.1"));

    // The runner: same peers/seconds/seed/witness so both sides derive the
    // same island from the seed alone.
    let mut remote = Command::new(bin())
        .args([
            "--external",
            "--peers",
            "4",
            "--seconds",
            "8",
            "--seed",
            "7",
            "--witness",
            "--host-node",
            &node_hex,
        ])
        .args(
            direct
                .iter()
                .flat_map(|d| ["--host-direct".into(), d.clone()]),
        )
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("external runner starts");

    // Both must finish inside the budget; either failing is the proof working.
    loop {
        let host_done = host.try_wait().is_ok_and(|status| status.is_some());
        let remote_done = remote.try_wait().is_ok_and(|status| status.is_some());
        if host_done && remote_done {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the pair did not finish within {PROCESS_TIMEOUT:?}"
        );
        std::thread::sleep(Duration::from_millis(200));
    }

    let host_status = host.wait().expect("host wait");
    let remote_status = remote.wait().expect("runner wait");
    if !host_status.success() || !remote_status.success() {
        // Before any assertion: the dying words are the evidence.
        eprintln!("--- host stderr (tail) ---");
        if let Ok(lines) = std::fs::read_to_string(&host_err) {
            let collected: Vec<&str> = lines.lines().collect();
            for line in collected.iter().rev().take(40).rev() {
                eprintln!("{line}");
            }
        }
    }
    assert!(
        remote_status.success(),
        "the external runner did not survive its own run"
    );
    assert!(
        host_status.success(),
        "the host's criterion did not hold; its own words follow above"
    );

    let raw = std::fs::read_to_string(&report_path)
        .unwrap_or_else(|error| panic!("report written: {error}"));
    let report: serde_json::Value = serde_json::from_str(&raw).expect("report parses");

    let external = report
        .get("external")
        .and_then(|e| e.as_object())
        .expect("the report names the external participant");
    assert_eq!(
        external.get("connected").and_then(|v| v.as_bool()),
        Some(true),
        "the bridge reported a disconnect"
    );
    assert!(
        external.get("uplink_frames").and_then(|v| v.as_u64()) > Some(0),
        "no frames arrived from the external peer"
    );
    assert!(
        external.get("downlink_frames").and_then(|v| v.as_u64()) > Some(0),
        "no frames were delivered to the external peer"
    );
    assert_eq!(
        external.get("downlink_dropped").and_then(|v| v.as_u64()),
        Some(0),
        "backpressure dropped frames at criterion rates"
    );
    assert_eq!(
        report.get("total_false_positives").and_then(|v| v.as_u64()),
        Some(0),
        "an honest cohort filed signals against nobody"
    );
    assert_eq!(
        report.get("peers").and_then(|v| v.as_u64()),
        Some(5),
        "four bots plus the external peer"
    );
}
