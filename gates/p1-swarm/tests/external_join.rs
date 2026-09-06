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
fn an_empty_standing_lobby_stays_available() {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("test clock")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("p1-idle-lobby-{}-{nonce}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let listening_path = dir.join("listening.txt");
    let active_seats_path = dir.join("active-seats.json");
    let journal_path = dir.join("slots.json");
    let host_err_path = dir.join("host.err");
    std::fs::write(&journal_path, b"[]").expect("empty reservation journal");
    let issuer = iroh_base::SecretKey::from_bytes(&[0x59; 32]);
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
            "1",
            "--seconds",
            "1",
            "--external-peer",
            "--attempt-id",
            "attempt-idle",
            "--issuer-key",
            &format!("592:{}", issuer.public()),
        ])
        .arg("--reservation-journal")
        .arg(&journal_path)
        .arg("--listening-file")
        .arg(&listening_path)
        .arg("--active-seats-file")
        .arg(&active_seats_path)
        .arg("--external-bind")
        .arg(external_bind.to_string())
        .stdout(Stdio::null())
        .stderr(std::fs::File::create(&host_err_path).expect("host err file"))
        .spawn()
        .expect("standing host process starts");

    wait_for_listening(&listening_path);
    std::thread::sleep(Duration::from_millis(2_200));
    let status = host.try_wait().expect("standing host status");
    if status.is_some() {
        eprintln!(
            "{}",
            std::fs::read_to_string(&host_err_path).unwrap_or_default()
        );
    }
    assert!(
        status.is_none(),
        "a reservation-backed host must reopen after an empty lobby"
    );
    assert!(
        !active_seats_path.exists(),
        "an empty lobby must not claim that any human is active"
    );
    host.kill().expect("stop idle host after proof");
    host.wait().expect("reap idle host after proof");
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
    // #961. `--external-peer` is the only mode that seeds campaign rocks
    // (`main.rs`: `campaign: args.external_peer`), so this is the one test in
    // the tree that puts a non-craft body on the wire — and it asserted nothing
    // about the decode counters, which is how a receiver that dropped every
    // rock and charged it to `bad_body` stayed green for as long as it did.
    assert_eq!(
        report.get("total_bad_body").and_then(|v| v.as_u64()),
        Some(0),
        "a state packet decoded at the envelope but not as state; the receiver \
         is refusing a body the sender legitimately replicated"
    );
    assert_eq!(
        report.get("total_undecodable").and_then(|v| v.as_u64()),
        Some(0),
        "an inbound state packet did not decode at all"
    );
    // The anti-vacuity half: with the campaign's rocks in scope every bot holds
    // more replicas than there are peers, so a receiver that silently went back
    // to one-row-per-peer would fail here rather than pass a `bad_body` of zero
    // it earned by dropping the bodies quietly.
    let replicas = report
        .get("total_replicas")
        .and_then(|v| v.as_u64())
        .expect("the report counts replicas");
    let peers = report
        .get("peers")
        .and_then(|v| v.as_u64())
        .expect("the report counts peers");
    assert!(
        replicas > peers * peers,
        "{replicas} replicas across {peers} peers is at most one row per peer; the \
         campaign's hosted rocks are not being tracked as bodies (#961)"
    );
    assert_eq!(
        report.get("peers").and_then(|v| v.as_u64()),
        Some(4),
        "the clean goodbye releases the exterior before the final active-peer census"
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
    let active_seats_path = dir.join("active-seats.json");
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
        .arg("--active-seats-file")
        .arg(&active_seats_path)
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
    let active_seats: serde_json::Value = serde_json::from_slice(
        &std::fs::read(&active_seats_path).expect("live active seats written"),
    )
    .expect("active seats record parses");
    assert_eq!(active_seats["attempt_id"], "attempt-reverse");
    assert_eq!(
        active_seats["active_slots"],
        serde_json::json!([]),
        "both clean closes must leave both human seats unbound"
    );
    assert_eq!(
        active_seats["released_sessions"],
        serde_json::json!(sessions.map(|(_slot, session)| session)),
        "each clean close must release its allocator row"
    );
}

#[test]
#[ignore = "four real processes at wall clock; run explicitly for late-join/rejoin proof"]
fn late_join_and_rejoin_after_goodbye_reuse_the_released_slot() {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("test clock")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("p1-live-join-{}-{nonce}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let listening_path = dir.join("listening.txt");
    let active_seats_path = dir.join("active-seats.json");
    let journal_path = dir.join("slots.json");
    let host_err_path = dir.join("host.err");
    let issuer = iroh_base::SecretKey::from_bytes(&[0x5a; 32]);
    let key_id = 681;
    let slot = 4usize;
    let sessions = [
        "018f8f4e-5c90-7abc-8123-000000000204",
        "018f8f4e-5c90-7abc-8123-000000000304",
    ];
    let write_reservation = |session: &str| {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("wall clock")
            .as_secs();
        let row = serde_json::json!([{
            "attempt_id": "attempt-live",
            "slot": slot,
            "session_id": session,
            "node": slot_key(slot).public().to_string(),
            "expires_at": now + 45,
        }]);
        std::fs::write(
            &journal_path,
            serde_json::to_vec(&row).expect("journal serializes"),
        )
        .expect("journal written");
    };
    write_reservation(sessions[0]);

    let reservation = std::net::UdpSocket::bind("127.0.0.1:0").expect("reserve port");
    let external_bind = reservation.local_addr().expect("reserved address");
    drop(reservation);
    let mut host = Command::new(bin())
        .args([
            "--peers",
            "4",
            "--external-slots",
            "4",
            "--lobby-seconds",
            "0",
            "--seconds",
            "8",
            "--min-cells",
            "1",
            "--external-peer",
            "--report-only",
            "--attempt-id",
            "attempt-live",
            "--issuer-key",
            &format!("{key_id}:{}", issuer.public()),
        ])
        .arg("--reservation-journal")
        .arg(&journal_path)
        .arg("--listening-file")
        .arg(&listening_path)
        .arg("--active-seats-file")
        .arg(&active_seats_path)
        .arg("--external-bind")
        .arg(external_bind.to_string())
        .stdout(Stdio::null())
        .stderr(std::fs::File::create(&host_err_path).expect("host err"))
        .spawn()
        .expect("host starts");
    let (host_node, host_direct) = wait_for_listening(&listening_path);

    let run = |session: &str| {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_millis() as u64;
        Command::new(bin())
            .args([
                "--external",
                "--peers",
                "4",
                "--external-slots",
                "4",
                "--slot",
                "4",
                "--seconds",
                "2",
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
            .stderr(Stdio::null())
            .spawn()
            .expect("client starts")
    };
    let wait_membership = |expected: serde_json::Value| {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            if let Ok(bytes) = std::fs::read(&active_seats_path) {
                if serde_json::from_slice::<serde_json::Value>(&bytes)
                    .is_ok_and(|value| value["active_slots"] == expected)
                {
                    break;
                }
            }
            assert!(
                std::time::Instant::now() < deadline,
                "active-seats.json never published active_slots={expected}"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    };

    let mut first = run(sessions[0]);
    wait_membership(serde_json::json!([4]));
    assert!(
        first.wait().expect("first wait").success(),
        "initial client failed"
    );
    wait_membership(serde_json::json!([]));

    write_reservation(sessions[1]);
    let mut late = run(sessions[1]);
    wait_membership(serde_json::json!([4]));
    assert!(
        late.wait().expect("late wait").success(),
        "late client failed"
    );
    wait_membership(serde_json::json!([]));

    let host_status = host.wait().expect("host wait");
    if !host_status.success() {
        eprintln!(
            "{}",
            std::fs::read_to_string(&host_err_path).unwrap_or_default()
        );
    }
    assert!(
        host_status.success(),
        "standing host failed the live join proof"
    );
    let published: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&active_seats_path).expect("active seats published"))
            .expect("active seats parse");
    assert_eq!(
        published["released_sessions"],
        serde_json::json!(sessions),
        "both explicit goodbyes must release their exact allocator rows"
    );
}

#[derive(Clone, Copy, Debug)]
enum DepartureMode {
    Graceful,
    Kill9,
    NetworkVanish,
}

impl DepartureMode {
    fn label(self) -> &'static str {
        match self {
            Self::Graceful => "graceful",
            Self::Kill9 => "kill-9",
            Self::NetworkVanish => "network-vanish",
        }
    }
}

fn signal(pid: u32, signal: &str) {
    let status = Command::new("kill")
        .args([signal, &pid.to_string()])
        .status()
        .expect("the Unix kill command runs");
    assert!(status.success(), "kill {signal} {pid} failed");
}

struct DepartureMeasurement {
    elapsed: Duration,
    host_log: String,
}

fn measure_seat_release(mode: DepartureMode) -> DepartureMeasurement {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("test clock")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "p1-departure-{}-{}-{nonce}",
        mode.label(),
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let listening_path = dir.join("listening.txt");
    let active_seats_path = dir.join("active-seats.json");
    let journal_path = dir.join("slots.json");
    let host_err_path = dir.join("host.err");
    let runner_err_path = dir.join("runner.err");
    let issuer = iroh_base::SecretKey::from_bytes(&[0x5b; 32]);
    let key_id = 682;
    let slot = 4usize;
    let session = format!("departure-{}-{nonce}", mode.label());
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("wall clock");
    let rows = serde_json::json!([{
        "attempt_id": "attempt-departure",
        "slot": slot,
        "session_id": session,
        "node": slot_key(slot).public().to_string(),
        "expires_at": now.as_secs() + 300,
    }]);
    std::fs::write(
        &journal_path,
        serde_json::to_vec(&rows).expect("journal serializes"),
    )
    .expect("journal written");

    let reservation = std::net::UdpSocket::bind("127.0.0.1:0").expect("reserve port");
    let external_bind = reservation.local_addr().expect("reserved address");
    drop(reservation);
    let mut host = Command::new(bin())
        .args([
            "--peers",
            "4",
            "--external-slots",
            "4",
            "--lobby-seconds",
            "0",
            "--seconds",
            "70",
            "--min-cells",
            "1",
            "--external-peer",
            "--report-only",
            "--attempt-id",
            "attempt-departure",
            "--issuer-key",
            &format!("{key_id}:{}", issuer.public()),
        ])
        .arg("--reservation-journal")
        .arg(&journal_path)
        .arg("--listening-file")
        .arg(&listening_path)
        .arg("--active-seats-file")
        .arg(&active_seats_path)
        .arg("--external-bind")
        .arg(external_bind.to_string())
        .stdout(Stdio::null())
        .stderr(std::fs::File::create(&host_err_path).expect("host err"))
        .spawn()
        .expect("host starts");
    let (host_node, host_direct) = wait_for_listening(&listening_path);

    let client_seconds = if matches!(mode, DepartureMode::Graceful) {
        2
    } else {
        60
    };
    let mut remote = Command::new(bin())
        .args([
            "--external",
            "--peers",
            "4",
            "--external-slots",
            "4",
            "--slot",
            "4",
            "--seconds",
            &client_seconds.to_string(),
            "--host-node",
            &host_node,
            "--host-direct",
            &host_direct,
            "--session-id",
            &session,
            "--session-token",
            &token_hex(&issuer, key_id, slot, now.as_millis() as u64),
        ])
        .stdout(Stdio::null())
        .stderr(std::fs::File::create(&runner_err_path).expect("runner err"))
        .spawn()
        .expect("client starts");

    let membership_is = |expected: serde_json::Value| {
        std::fs::read(&active_seats_path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
            .is_some_and(|value| value["active_slots"] == expected)
    };
    let occupied_deadline = std::time::Instant::now() + Duration::from_secs(15);
    while !membership_is(serde_json::json!([4])) {
        if std::time::Instant::now() >= occupied_deadline {
            let _ = remote.kill();
            let _ = host.kill();
            panic!(
                "{} client never occupied its seat; host: {}; client: {}",
                mode.label(),
                std::fs::read_to_string(&host_err_path).unwrap_or_default(),
                std::fs::read_to_string(&runner_err_path).unwrap_or_default(),
            );
        }
        std::thread::sleep(Duration::from_millis(20));
    }

    let departure_at = match mode {
        DepartureMode::Graceful => {
            let occupied_at = std::time::Instant::now();
            let status = remote.wait().expect("graceful client wait");
            assert!(
                status.success(),
                "graceful client failed: {}",
                std::fs::read_to_string(&runner_err_path).unwrap_or_default()
            );
            // The runner begins its goodbye after exactly `client_seconds` of
            // paced ticks. Measuring from occupancy and subtracting that run
            // duration includes the real wire scheduling while avoiding a
            // probe-only protocol message.
            occupied_at + Duration::from_secs(client_seconds)
        }
        DepartureMode::Kill9 => {
            let departure_at = std::time::Instant::now();
            signal(remote.id(), "-KILL");
            let _ = remote.wait();
            departure_at
        }
        DepartureMode::NetworkVanish => {
            let departure_at = std::time::Instant::now();
            signal(remote.id(), "-STOP");
            departure_at
        }
    };

    let release_deadline = std::time::Instant::now() + Duration::from_secs(50);
    while !membership_is(serde_json::json!([])) {
        if std::time::Instant::now() >= release_deadline {
            if matches!(mode, DepartureMode::NetworkVanish) {
                signal(remote.id(), "-CONT");
            }
            let _ = remote.kill();
            let _ = remote.wait();
            let _ = host.kill();
            let _ = host.wait();
            panic!(
                "{} seat was not released within 50 seconds; host: {}",
                mode.label(),
                std::fs::read_to_string(&host_err_path).unwrap_or_default(),
            );
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let elapsed = std::time::Instant::now().saturating_duration_since(departure_at);

    if matches!(mode, DepartureMode::Graceful) {
        let close_deadline = std::time::Instant::now() + Duration::from_secs(2);
        while !std::fs::read_to_string(&host_err_path)
            .unwrap_or_default()
            .contains("exterior QUIC closed")
            && std::time::Instant::now() < close_deadline
        {
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    if matches!(mode, DepartureMode::NetworkVanish) {
        signal(remote.id(), "-CONT");
        remote.kill().expect("kill resumed client");
        let _ = remote.wait();
    }
    host.kill().expect("stop measurement host");
    let _ = host.wait();
    let host_log = std::fs::read_to_string(&host_err_path).unwrap_or_default();
    eprintln!(
        "departure measurement {}: {:.3} seconds",
        mode.label(),
        elapsed.as_secs_f64()
    );
    DepartureMeasurement { elapsed, host_log }
}

#[cfg(unix)]
#[test]
#[ignore = "three real-process UDP departure modes at wall clock; run explicitly"]
fn transport_departure_releases_the_seat_in_observed_time() {
    let graceful = measure_seat_release(DepartureMode::Graceful);
    let kill9 = measure_seat_release(DepartureMode::Kill9);
    let network_vanish = measure_seat_release(DepartureMode::NetworkVanish);
    eprintln!(
        "departure measurements: graceful={:.3}s kill-9={:.3}s network-vanish={:.3}s",
        graceful.elapsed.as_secs_f64(),
        kill9.elapsed.as_secs_f64(),
        network_vanish.elapsed.as_secs_f64(),
    );
    assert!(
        graceful.host_log.contains("(explicit goodbye)"),
        "graceful release must name the explicit goodbye; host log:\n{}",
        graceful.host_log
    );
    assert!(
        graceful
            .host_log
            .contains("exterior QUIC closed (application close)"),
        "graceful shutdown must put CONNECTION_CLOSE on the wire; host log:\n{}",
        graceful.host_log
    );
    assert!(
        graceful.elapsed <= Duration::from_secs(1),
        "graceful goodbye seat release took {:.3}s; it must bypass the transport grace",
        graceful.elapsed.as_secs_f64(),
    );
    for (mode, measurement) in [("kill-9", &kill9), ("network-vanish", &network_vanish)] {
        assert!(
            measurement
                .host_log
                .contains("idle timeout; transport close grace elapsed"),
            "{mode} release must be classified as QUIC idle timeout; host log:\n{}",
            measurement.host_log
        );
        assert!(
            (Duration::from_secs(11)..=Duration::from_secs(15))
                .contains(&measurement.elapsed),
            "{mode} seat release took {:.3}s; the pinned 10s QUIC idle timeout plus the real 2s grace must release in 11..=15s",
            measurement.elapsed.as_secs_f64(),
        );
    }
}

/// Reads the runner's own end-of-run account of what it accepted.
///
/// The host's report cannot answer #1129: the host *delivered* every frame it
/// is asked about, and the runner threw them away after they arrived. This is
/// the only place the client's side of that is written down.
fn runner_summary(path: &std::path::Path) -> String {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    text.lines()
        .rev()
        .find(|line| line.contains("accepted"))
        .unwrap_or_else(|| panic!("the runner never summarised its run: {}", path.display()))
        .to_owned()
}

/// Frames the summary line says arrived from one seat, `0` when it names none.
fn accepted_from(summary: &str, slot: usize) -> u64 {
    let Some((_, seats)) = summary.split_once("from seats ") else {
        return 0;
    };
    let seats = seats.split(';').next().unwrap_or_default();
    seats
        .split(',')
        .filter_map(|entry| entry.split_once(':'))
        .find(|(seat, _)| seat.trim() == slot.to_string())
        .and_then(|(_, frames)| frames.trim().parse().ok())
        .unwrap_or(0)
}

/// The number after `label` in the runner's summary line.
fn summary_count(summary: &str, label: &str) -> u64 {
    summary
        .split_whitespace()
        .zip(summary.split_whitespace().skip(1))
        .find(|(_, next)| next.trim_end_matches(',') == label)
        .and_then(|(count, _)| count.parse().ok())
        .unwrap_or_else(|| panic!("the summary does not count {label}: {summary}"))
}

/// Seconds the runner spent replicating to nobody, from its summary line.
fn empty_audience_seconds(summary: &str) -> u64 {
    let tail = summary
        .split_once("roster updates, ")
        .map(|(_, tail)| tail)
        .unwrap_or_else(|| panic!("the summary does not count its audience: {summary}"));
    tail.split_whitespace()
        .next()
        .and_then(|count| count.parse().ok())
        .unwrap_or_else(|| panic!("the summary's audience count did not parse: {summary}"))
}

/// #1128: a human seat must still be replicating minutes after it leaves the
/// cell it spawned in.
///
/// **Why a minute and not eight seconds.** At the campaign's 512 m cell edge
/// and the pilot's 32 m/s cruise the craft clears its spawn cell in about
/// sixteen seconds. Every real-peer fixture in this file ran for eight, which
/// is why a defect that made the seat a permanent ghost twenty seconds in
/// survived: inside one cell the frozen spawn roster and a live one are the
/// same roster. Measured on this leg's parameters: 1,648 uplink frames by 20 s
/// and 2,928 by 60 s before the fix — 32.0 frames per second over the
/// intervening forty, the witness-lane floor — against 2,148 and 4,779 after.
///
/// **What is asserted, and why it is the runner's line rather than the
/// report.** The failing shape is the seat's *replication audience*, and only
/// the seat can see it: the host keeps routing the seat's repair and heartbeat
/// traffic to every linked bot whatever the audience is, so `state_deliveries`
/// falls by a third rather than to zero and no host-side counter separates the
/// two cases. The runner counts the audience of its own craft once a second
/// and reports it. Re-measured with the roster refresh disabled and everything
/// else in place: **48 of 60 seconds replicating to nobody**, against 0 of 60
/// with it. The cell count is the anti-vacuity half — a run that never crossed
/// a boundary is no evidence at all, and is what would let this test pass on
/// the broken code by accident.
#[test]
#[ignore = "two real processes for a minute of wall clock; run via scripts/p1-swarm-gate.sh or --ignored"]
fn an_external_peer_keeps_replicating_after_it_leaves_its_spawn_cell() {
    const SECONDS: u64 = 60;
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("test clock")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("p1-cruise-{}-{nonce}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let report_path = dir.join("report.json");
    let listening_path = dir.join("listening.txt");
    let host_err_path = dir.join("host.err");
    let runner_err_path = dir.join("runner.err");
    let reservation = std::net::UdpSocket::bind("127.0.0.1:0").expect("reserve exterior port");
    let external_bind = reservation.local_addr().expect("reserved exterior address");
    drop(reservation);

    let mut host = Command::new(bin())
        .args([
            "--peers",
            "4",
            "--seconds",
            &SECONDS.to_string(),
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
        .stdout(Stdio::null())
        .stderr(std::fs::File::create(&host_err_path).expect("host err file"))
        .spawn()
        .expect("host process starts");
    let (host_node, host_direct) = wait_for_listening(&listening_path);

    let mut remote = Command::new(bin())
        .args([
            "--external",
            "--peers",
            "4",
            "--seconds",
            &SECONDS.to_string(),
            "--seed",
            "7",
            "--witness",
            "--host-node",
            &host_node,
            "--host-direct",
            &host_direct,
        ])
        .stdout(Stdio::null())
        .stderr(std::fs::File::create(&runner_err_path).expect("runner err file"))
        .spawn()
        .expect("external runner starts");

    let deadline = std::time::Instant::now() + PROCESS_TIMEOUT + Duration::from_secs(SECONDS);
    loop {
        let done = host.try_wait().is_ok_and(|s| s.is_some())
            && remote.try_wait().is_ok_and(|s| s.is_some());
        if done {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the pair did not finish a {SECONDS}-second run in time"
        );
        std::thread::sleep(Duration::from_millis(200));
    }
    let host_status = host.wait().expect("host wait");
    let remote_status = remote.wait().expect("runner wait");
    if !host_status.success() {
        eprintln!(
            "{}",
            std::fs::read_to_string(&host_err_path).unwrap_or_default()
        );
    }
    assert!(
        remote_status.success(),
        "the external runner did not survive"
    );
    assert!(
        host_status.success(),
        "the host's criterion did not hold over a minute-long join; its own words are above"
    );

    let report: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&report_path).expect("report written"))
            .expect("report parses");
    let external = report["external"]
        .as_array()
        .and_then(|seats| seats.first())
        .expect("the report names the external seat")
        .clone();

    let reported_cells = external["reported_cells"].as_u64().expect("cells reported");
    assert!(
        reported_cells > 1,
        "the craft never left the cell it spawned in over {SECONDS} seconds, so this run says \
         nothing about #1128: {reported_cells} cell reported"
    );
    let deliveries = external["state_deliveries"].as_u64().expect("deliveries");
    assert!(
        deliveries > 1_200,
        "{deliveries} state deliveries over {SECONDS} seconds is below the rate one recipient \
         at the 20 Hz send cadence would produce for the run's second half alone"
    );
    let summary = runner_summary(&runner_err_path);
    assert!(
        summary_count(&summary, "roster") > 0,
        "the runner applied no host roster at all, so whatever it replicated to it chose on \
         its own: {summary}"
    );
    let empty = empty_audience_seconds(&summary);
    assert!(
        empty <= 2,
        "the seat replicated to nobody for {empty} of {SECONDS} seconds. Its roster froze and \
         its send audience emptied when it crossed out of its spawn cell — it is a ghost to \
         every other craft while its own screen looks normal (#1128): {summary}"
    );
}

/// #1129: two human seats must see each other, and must hear the host.
///
/// The tree's only other two-human test runs for one second and asserts seat
/// bookkeeping — which seat id each runner was granted — so it passed for as
/// long as the seats were mutually invisible. This one runs long enough to
/// cross a cell boundary and asserts the traffic itself, in both directions
/// and at both ends: the host must have delivered each human's frames to the
/// other, and each runner must say it accepted them.
///
/// Both ends are necessary. The host's report showed every frame delivered
/// while the defect was live, because they were — the runner then discarded
/// them, filtering inbound frames on the *bot* count. A test that read only
/// the host's report would have stayed green.
#[test]
#[ignore = "three real processes for most of a minute; run via scripts/p1-swarm-gate.sh or --ignored"]
fn two_human_seats_exchange_frames_and_read_the_meta_lane() {
    const SECONDS: u64 = 40;
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("test clock")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("p1-two-humans-{}-{nonce}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let report_path = dir.join("report.json");
    let listening_path = dir.join("listening.txt");
    let active_seats_path = dir.join("active-seats.json");
    let journal_path = dir.join("slots.json");
    let host_err_path = dir.join("host.err");
    let issuer = iroh_base::SecretKey::from_bytes(&[0x5b; 32]);
    let key_id = 594;
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("wall clock")
        .as_millis() as u64;
    let sessions = [
        (4usize, "018f8f4e-5c90-7abc-8123-000000000404"),
        (5usize, "018f8f4e-5c90-7abc-8123-000000000405"),
    ];
    let rows = sessions
        .iter()
        .map(|(slot, session)| {
            serde_json::json!({
                "attempt_id": "attempt-two-humans",
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
            "2",
            "--lobby-seconds",
            "20",
            "--seconds",
            &SECONDS.to_string(),
            "--min-cells",
            "1",
            "--seed",
            "7",
            "--impaired",
            "--external-peer",
            "--report-only",
            "--attempt-id",
            "attempt-two-humans",
            "--issuer-key",
            &format!("{key_id}:{}", issuer.public()),
            "--json",
        ])
        .arg(&report_path)
        .arg("--reservation-journal")
        .arg(&journal_path)
        .arg("--listening-file")
        .arg(&listening_path)
        .arg("--active-seats-file")
        .arg(&active_seats_path)
        .arg("--external-bind")
        .arg(external_bind.to_string())
        .stdout(Stdio::null())
        .stderr(std::fs::File::create(&host_err_path).expect("host err file"))
        .spawn()
        .expect("host process starts");
    let (host_node, host_direct) = wait_for_listening(&listening_path);

    let mut remotes = Vec::new();
    for (slot, session) in sessions {
        let err_path = dir.join(format!("runner-{slot}.err"));
        remotes.push((
            slot,
            err_path.clone(),
            Command::new(bin())
                .args([
                    "--external",
                    "--peers",
                    "4",
                    "--external-slots",
                    "2",
                    "--slot",
                    &slot.to_string(),
                    "--seconds",
                    &SECONDS.to_string(),
                    "--seed",
                    "7",
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
                .stderr(std::fs::File::create(&err_path).expect("runner err file"))
                .spawn()
                .expect("external runner starts"),
        ));
    }
    for (slot, _, remote) in &mut remotes {
        assert!(
            remote.wait().expect("runner wait").success(),
            "the runner on seat {slot} did not survive its own run"
        );
    }
    let host_status = host.wait().expect("host wait");
    if !host_status.success() {
        eprintln!(
            "{}",
            std::fs::read_to_string(&host_err_path).unwrap_or_default()
        );
    }
    assert!(host_status.success(), "the two-human host failed");

    let report: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&report_path).expect("report written"))
            .expect("report parses");
    let seats = report["external"].as_array().expect("external seats");
    assert_eq!(seats.len(), 2, "both human seats must appear in the report");

    for seat in seats {
        let index = seat["index"].as_u64().expect("seat index") as usize;
        let other = if index == 4 { 5 } else { 4 };
        let from_other = seat["deliveries_from_seats"]
            .as_array()
            .expect("per-seat deliveries")
            .iter()
            .find(|row| row["slot"].as_u64() == Some(other as u64))
            .and_then(|row| row["frames"].as_u64())
            .unwrap_or(0);
        assert!(
            from_other > 0,
            "the host delivered nothing from seat {other} to seat {index} over {SECONDS} \
             seconds: the two humans are not on the same island as far as routing is concerned"
        );
    }

    for (slot, err_path, _) in &remotes {
        let other = if *slot == 4 { 5 } else { 4 };
        let summary = runner_summary(err_path);
        assert!(
            accepted_from(&summary, other) > 0,
            "seat {slot} accepted no frame at all from seat {other}; the other human is \
             invisible to it for the whole session (#1129): {summary}"
        );
        assert!(
            summary_count(&summary, "meta") > 0,
            "seat {slot} read not one Meta frame: every host acknowledgement, hearsay fold, \
             membership manifest and island roster was discarded before it was classified \
             (#1129): {summary}"
        );
        assert!(
            summary_count(&summary, "roster") > 0,
            "seat {slot} applied no host roster: {summary}"
        );
        let empty = empty_audience_seconds(&summary);
        assert!(
            empty <= 2,
            "seat {slot} replicated to nobody for {empty} of {SECONDS} seconds (#1128 on a \
             human seat): {summary}"
        );
    }
}
