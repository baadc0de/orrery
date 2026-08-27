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

fn slot_key(index: usize) -> iroh_base::SecretKey {
    let mut seed = [0u8; 32];
    seed[0..8].copy_from_slice(&(index as u64 + 1).to_le_bytes());
    seed[31] = 0xB0;
    iroh_base::SecretKey::from_bytes(&seed)
}

fn token_hex(issuer: &iroh_base::SecretKey, key_id: u32, slot: usize, now_ms: u64) -> String {
    let claims = orrery_protocol::SessionTokenClaimsV1::new(
        orrery_protocol::AccountId::new(slot as u64 + 1),
        slot_key(slot).public(),
        orrery_protocol::UnixMillis::new(now_ms.saturating_sub(1_000)),
        orrery_protocol::SessionTokenTtlMs::new(300_000),
        orrery_protocol::SessionStanding::Good,
        orrery_protocol::IssuerKeyId::new(key_id),
        false,
    );
    orrery_protocol::SessionTokenV1::sign(claims, issuer)
        .expect("token signs")
        .encode()
        .expect("token encodes")
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn wait_for_listening(path: &std::path::Path) -> (String, String) {
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        if let Ok(line) = std::fs::read_to_string(path) {
            let mut fields = line.split_whitespace();
            if let (Some(node), Some(socket)) = (fields.next(), fields.next()) {
                return (node.to_owned(), socket.replace("0.0.0.0", "127.0.0.1"));
            }
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the host never wrote its listening file"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
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
    let reservation = std::net::UdpSocket::bind("127.0.0.1:0").expect("reserve exterior port");
    let external_bind = reservation.local_addr().expect("reserved exterior address");
    drop(reservation);
    let debug_bridge = std::env::var_os("P1_SWARM_BRIDGE_DEBUG").is_some();
    let _ = debug_bridge;
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
        .arg("--external-bind")
        .arg(external_bind.to_string())
        .env(
            "P1_SWARM_BRIDGE_DEBUG",
            std::env::var("P1_SWARM_BRIDGE_DEBUG").unwrap_or_default(),
        )
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
    let expected_direct = external_bind.to_string();
    assert_eq!(
        direct.as_deref(),
        Some(expected_direct.as_str()),
        "the listening file reports the exact configured exterior bind"
    );

    // The runner: same peers/seconds/seed/witness so both sides derive the
    // same island from the seed alone.
    let mut remote = Command::new(bin())
        .env(
            "P1_SWARM_BRIDGE_DEBUG",
            std::env::var("P1_SWARM_BRIDGE_DEBUG").unwrap_or_default(),
        )
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
        .stderr(std::fs::File::create(dir.join("runner.err")).expect("runner err file"))
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
        .and_then(|e| e.as_array())
        .filter(|exteriors| exteriors.len() == 1)
        .and_then(|exteriors| exteriors.first())
        .and_then(|e| e.as_object())
        .expect("the report names the external participant");
    // A clean end-of-run close is fine: the runner exits after its last tick,
    // so by report time the connection may already be closed - as long as it
    // said goodbye first, the run was complete when it did.
    let connected = external.get("connected").and_then(|v| v.as_bool());
    let said_goodbye = external.get("said_goodbye").and_then(|v| v.as_bool());
    let clean_close = connected == Some(true) || said_goodbye == Some(true);
    assert!(clean_close, "the bridge reported a mid-run disconnect");
    assert!(
        external.get("uplink_frames").and_then(|v| v.as_u64()) > Some(0),
        "no frames arrived from the external peer"
    );
    assert!(
        external.get("connected_ticks").and_then(|v| v.as_u64()) > Some(0),
        "the report did not retain the external peer's connected span"
    );
    let uplink_delivered = external
        .get("uplink_delivered")
        .and_then(|v| v.as_u64())
        .expect("the report counts delivered uplink datagrams");
    let uplink_dropped = external
        .get("uplink_dropped")
        .and_then(|v| v.as_u64())
        .expect("the report counts dropped uplink datagrams");
    assert!(
        uplink_delivered + uplink_dropped > 0,
        "no uplink datagram reached an impairment decision"
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

#[test]
#[ignore = "three real processes at wall clock; run explicitly for the lobby proof"]
fn two_reserved_clients_join_in_reverse_reservation_order() {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("test clock")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("p1-lobby-{}-{nonce}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let report_path = dir.join("report.json");
    let listening_path = dir.join("listening.txt");
    let journal_path = dir.join("slots.json");
    let host_err_path = dir.join("host.err");
    let issuer = iroh_base::SecretKey::from_bytes(&[0x58; 32]);
    let key_id = 583;
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("wall clock")
        .as_millis() as u64;
    let sessions = [
        (4usize, "018f8f4e-5c90-7abc-8123-000000000104"),
        (5usize, "018f8f4e-5c90-7abc-8123-000000000105"),
    ];
    let rows = sessions
        .iter()
        .map(|(slot, session)| {
            serde_json::json!({
                "attempt_id": "attempt-reverse",
                "slot": slot,
                "session_id": session,
                "node": slot_key(*slot).public().to_string(),
                "expires_at": now_ms / 1_000 + 300,
            })
        })
        .collect::<Vec<_>>();
    std::fs::write(
        &journal_path,
        serde_json::to_vec(&rows).expect("journal serializes"),
    )
    .expect("journal written");

    let reservation = std::net::UdpSocket::bind("127.0.0.1:0").expect("reserve exterior port");
    let external_bind = reservation.local_addr().expect("reserved exterior address");
    drop(reservation);
    let mut host = Command::new(bin())
        .args([
            "--peers",
            "4",
            "--external-slots",
            "4",
            "--lobby-seconds",
            "2",
            "--seconds",
            "1",
            "--min-cells",
            "1",
            "--external-peer",
            "--report-only",
            "--attempt-id",
            "attempt-reverse",
            "--issuer-key",
            &format!("{key_id}:{}", issuer.public()),
            "--json",
        ])
        .arg(&report_path)
        .arg("--reservation-journal")
        .arg(&journal_path)
        .arg("--listening-file")
        .arg(&listening_path)
        .arg("--external-bind")
        .arg(external_bind.to_string())
        .stdout(Stdio::null())
        .stderr(std::fs::File::create(&host_err_path).expect("host err file"))
        .spawn()
        .expect("host process starts");
    let (host_node, host_direct) = wait_for_listening(&listening_path);

    // Slot 5 connects first even though admission reserved slot 4 first.
    let mut remotes = Vec::new();
    for (slot, session) in sessions.into_iter().rev() {
        remotes.push(
            Command::new(bin())
                .args([
                    "--external",
                    "--peers",
                    "4",
                    "--external-slots",
                    "4",
                    "--slot",
                    &slot.to_string(),
                    "--seconds",
                    "1",
                    "--host-node",
                    &host_node,
                    "--host-direct",
                    &host_direct,
                    "--session-id",
                    session,
                    "--session-token",
                    &token_hex(&issuer, key_id, slot, now_ms),
                ])
                .stdout(Stdio::null())
                .stderr(
                    std::fs::File::create(dir.join(format!("runner-{slot}.err")))
                        .expect("runner err file"),
                )
                .spawn()
                .expect("external runner starts"),
        );
        std::thread::sleep(Duration::from_millis(100));
    }

    for (offset, remote) in remotes.iter_mut().enumerate() {
        assert!(
            remote.wait().expect("runner wait").success(),
            "reverse-order runner {offset} failed"
        );
    }
    let host_status = host.wait().expect("host wait");
    if !host_status.success() {
        eprintln!(
            "{}",
            std::fs::read_to_string(&host_err_path).unwrap_or_default()
        );
    }
    assert!(host_status.success(), "the lobby host failed");

    let report: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&report_path).expect("report written"))
            .expect("report parses");
    assert_eq!(
        report["external"]
            .as_array()
            .expect("external report")
            .iter()
            .map(|row| row["index"].as_u64().expect("seat index"))
            .collect::<Vec<_>>(),
        vec![4, 5],
        "arrival order must not renumber reserved seats"
    );
}
