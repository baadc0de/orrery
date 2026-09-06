//! What a seat does when its link dies and comes straight back (#1147).
//!
//! An *instrument*, not a criterion: it prints the reconnect blackout it
//! measures and asserts only the things that must hold for the measurement to
//! mean anything (the seat seated, and the seat eventually released). It is
//! `#[ignore]`d because it spends forty-odd seconds of wall clock on real
//! processes.
//!
//! It exists because the tree had no fixture at all for a seat that departs
//! *without* saying goodbye and then returns. `late_join_and_rejoin_after_
//! goodbye_reuse_the_released_slot` covers the graceful case, and
//! `transport_departure_releases_the_seat_in_observed_time` measures how long
//! a release takes — but neither asks what a peer redialling *during* that
//! release window is told. The answer, measured here, is
//! `reservation_slot_occupied` for 12.1 seconds, a hard `bail!` in
//! `bridge::remote_join`, and no retry anywhere.

#![allow(missing_docs)]

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

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
        orrery_protocol::SessionTokenTtlMs::new(3_600_000),
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
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Ok(line) = std::fs::read_to_string(path) {
            let mut fields = line.split_whitespace();
            if let (Some(node), Some(socket)) = (fields.next(), fields.next()) {
                return (node.to_owned(), socket.replace("0.0.0.0", "127.0.0.1"));
            }
        }
        assert!(
            Instant::now() < deadline,
            "the host never wrote its listening file"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[cfg(unix)]
#[test]
#[ignore = "real processes at wall clock; run explicitly for the reconnect measurement"]
fn a_hard_killed_seat_that_comes_straight_back() {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("hunt2-reconnect-{nonce}"));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let listening_path = dir.join("listening.txt");
    let active_seats_path = dir.join("active-seats.json");
    let journal_path = dir.join("slots.json");
    let host_err_path = dir.join("host.err");
    let report_path = dir.join("report.json");

    let issuer = iroh_base::SecretKey::from_bytes(&[0x77; 32]);
    let key_id = 777;
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_millis() as u64;

    let slot = 4usize;
    let session = "018f8f4e-5c90-7abc-8123-0000000000f4";
    let rows = vec![serde_json::json!({
        "attempt_id": "hunt2",
        "slot": slot,
        "session_id": session,
        "node": slot_key(slot).public().to_string(),
        "expires_at": now_ms / 1_000 + 3_600,
    })];
    std::fs::write(&journal_path, serde_json::to_vec(&rows).expect("journal")).expect("write");

    let reservation = std::net::UdpSocket::bind("127.0.0.1:0").expect("reserve port");
    let external_bind = reservation.local_addr().expect("addr");
    drop(reservation);

    let mut host = Command::new(bin())
        .args([
            "--peers",
            "4",
            "--external-slots",
            "2",
            "--lobby-seconds",
            "10",
            "--seconds",
            "240",
            "--min-cells",
            "1",
            "--external-peer",
            "--report-only",
            "--attempt-id",
            "hunt2",
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
        .stderr(std::fs::File::create(&host_err_path).expect("host err"))
        .spawn()
        .expect("host starts");
    let (host_node, host_direct) = wait_for_listening(&listening_path);

    let token = token_hex(&issuer, key_id, slot, now_ms);
    let launch = |tag: &str, seconds: u64| {
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
                &seconds.to_string(),
                "--min-cells",
                "1",
                "--host-node",
                &host_node,
                "--host-direct",
                &host_direct,
                "--session-id",
                session,
                "--session-token",
                &token,
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::from(
                std::fs::File::create(dir.join(format!("runner-{tag}.err"))).expect("err file"),
            ))
            .spawn()
            .expect("runner starts")
    };

    let active = || std::fs::read_to_string(&active_seats_path).unwrap_or_default();
    let wait_active = |want: bool, secs: u64| {
        let deadline = Instant::now() + Duration::from_secs(secs);
        loop {
            let seated = active().contains(&format!("\"active_slots\":[{slot}]"));
            if seated == want {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    };

    // First life. The lobby has to freeze before the live accept loop exists.
    let mut first = launch("first", 240);
    assert!(wait_active(true, 60), "seat never seated: {}", active());
    eprintln!("reconnect: seat {slot} seated; feed = {}", active());
    std::thread::sleep(Duration::from_secs(20));

    // The link dies without a goodbye — a laptop lid, a dropped Wi-Fi.
    let killed_at = Instant::now();
    Command::new("kill")
        .args(["-KILL", &first.id().to_string()])
        .status()
        .expect("kill");
    let _ = first.wait();
    eprintln!("reconnect: seat hard-killed at t=0");

    // The tester relaunches straight away, as a person does.
    std::thread::sleep(Duration::from_millis(500));
    let mut second = launch("immediate", 60);
    let status = second.wait().expect("immediate rejoin exits");
    eprintln!(
        "reconnect: immediate rejoin (t+0.5s) exited {:?} after {:.1}s",
        status.code(),
        killed_at.elapsed().as_secs_f64()
    );
    eprintln!(
        "reconnect: immediate rejoin stderr:\n{}",
        std::fs::read_to_string(dir.join("runner-immediate.err")).unwrap_or_default()
    );

    // Now wait past the 10s idle timeout + 2s grace and try again.
    assert!(
        wait_active(false, 40),
        "seat was never released after the kill: {}",
        active()
    );
    eprintln!(
        "reconnect: seat released at t+{:.1}s; feed = {}",
        killed_at.elapsed().as_secs_f64(),
        active()
    );
    let mut third = launch("after-release", 60);
    let reseated = wait_active(true, 30);
    eprintln!(
        "reconnect: rejoin after release reseated = {reseated}; feed = {}",
        active()
    );
    if !reseated {
        eprintln!(
            "reconnect: after-release stderr:\n{}",
            std::fs::read_to_string(dir.join("runner-after-release.err")).unwrap_or_default()
        );
    }
    let _ = third.kill();
    let _ = third.wait();
    let _ = host.kill();
    let _ = host.wait();
    let log = std::fs::read_to_string(&host_err_path).unwrap_or_default();
    eprintln!("reconnect: --- host stderr tail ---");
    for line in log.lines().rev().take(60).collect::<Vec<_>>().iter().rev() {
        eprintln!("{line}");
    }
}
