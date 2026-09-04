//! The seam: a real host report, through the real assembler, into the real ledger (#960).
//!
//! Both sides of this join were already tested, and that is exactly why no
//! human hour could bank. `gates/p1-swarm` was tested to emit a `SwarmReport`;
//! `scripts/p4-attempt-accounting.py` and `scripts/p4-ledger.sh` were tested to
//! consume an `AttemptReport`. Every fixture on either side was written in the
//! shape that side expected, so the suite stayed green while the two shapes had
//! nothing in common — no `attempt_id`, no `bots`, no `valid_attempt_seconds`,
//! no `completed`, no per-seat `session_id` or `close`, and no
//! `per_link_impairment`. A full end-to-end run at HEAD produced a complete
//! signed client row and then died on
//! `refusing to derive: the attempt report carries no UUIDv7 attempt_id`.
//!
//! So this test writes no host fixture at all. It runs the real binary as a
//! reservation-backed standing host, dials it with two real second processes
//! over real QUIC, and hands the report the host actually wrote to
//! `scripts/p4-campaign-session.sh assemble` and then to
//! `scripts/p4-ledger.sh append`, unmodified. What banks at the end is derived
//! from that file and nothing else.
//!
//! **What is real here and what stands in.** The host half is real throughout:
//! the attempt id, the seat map, each seat's admitted node and invite id, its
//! connected span, its close reason, and the per-link loss the router actually
//! drew. The client half is a signed stand-in — a row per seat, signed with the
//! seat key the host QUIC-admitted, because seating the *shipped* regolith
//! client needs an admission service and a rendered process that no test
//! process owns. That stand-in is not free-floating: it must be signed by a key
//! this attempt admitted at exactly one seat, name a session id this attempt
//! seated, and claim no more minutes than the host measured that seat as
//! connected for. Every one of those bounds is read out of the real report, so
//! a host that stops emitting them fails this test rather than passing it
//! vacuously.
//!
//! **The full P1 criterion is enforced here.** This leg carried `--report-only`
//! while #961 was open, because the receiver charged every replicated `Rock` to
//! `total_bad_body` and no seated attempt could hold its own clauses. #962 fixed
//! that, and the flag is gone: the host is now held to every criterion clause on
//! a seated, witnessed, impaired run, and `total_bad_body == 0` is asserted
//! rather than merely printed.
//!
//! **This leg is what found #963, and the counters it prints are what diagnosed
//! it.** Two of nine runs were refused for observation coverage — 0.7489 and
//! 0.9224 — with `judged_ticks` identical in every run and only `shown_ticks`
//! moving. Neither failing run had a live join: both seats bound in the lobby,
//! exactly as the passing runs did. What differed was the gap between the two
//! seats' *goodbyes* — six ticks in a failing run against four in a passing one
//! — because a seat's release marks membership changed, and the refresh that
//! followed armed host-side watches against the seat that stayed using its
//! tick-zero lobby anchor. See `Swarm::refresh_live_witnesses` for the
//! mechanism and `swarm::tests` for the unit-level pin.

#![allow(missing_docs)]

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

/// Simulated seconds, which for a run with a connected exterior is also wall
/// clock. Long enough that each human seat's directed links carry more than
/// the 1,000-packet floor #572 §6.1's binomial band needs to mean anything.
///
/// **Two minutes rather than one, and #1053 is why.** `BANKED_MINUTES` is now
/// squeezed between a floor and a ceiling, and the ceiling is this constant.
/// The lobby closes the moment it is full rather than sitting out
/// `--lobby-seconds` (`main.rs`'s `while pending.len() < args.external_slots`),
/// so both seats are accepted a beat before tick zero and a seat's whole
/// bracket is the attempt itself: measured on this leg at `ATTEMPT_SECONDS =
/// 60`, seat 4 bracketed **1.0035 min** and seat 5 **1.0034 min** — 0.2 s of
/// room above the ledger's 1.0-minute floor, and less than the run-to-run
/// metronome drift the same run printed (391 ms and 290 ms). At one minute the
/// two bounds are not merely tight, they overlap inside the noise. Two minutes
/// puts half a minute on each side of the claim, which is ~75× the observed
/// drift and 30× the allowance `CLOCK_BOUNDARY_SLACK_MS` grants two clocks.
/// It costs this leg one more wall minute on a gate whose witnessed hour
/// already runs about ten.
const ATTEMPT_SECONDS: u64 = 120;
/// Bot seats. Slots `[0, 4)`; the human seats are 4 and 5.
const BOTS: usize = 4;
/// Minutes each stand-in row banks, held between two bounds that must not be
/// closed by narrowing either of them:
///
/// * **at least 1.0**, `p4-ledger.sh`'s `MIN_MEASURED_MINUTES` — a session that
///   ended seconds after `StartV1` is a failure to seat and not a measurement
///   (#1053). That floor is a safety guard on real volunteer hours; a fixture
///   that arranged to sit under it, or that was handed a way around it, would
///   stop exercising the clause that matters;
/// * **at most the seat's connected span**, which the derivation clamps to
///   (`p4-attempt-accounting.py`'s `WALL_BRACKET_BASIS` branch) and the ledger
///   re-checks off the file. A row over the bracket does not fail — it *banks
///   less*, silently, and the manifest assertions below would then be
///   measuring the clamp instead of the seam.
///
/// See `ATTEMPT_SECONDS` for why the span is what it is.
const BANKED_MINUTES: f64 = 1.5;

/// `p4-ledger.sh`'s `MIN_MEASURED_MINUTES`, restated here so that a
/// `BANKED_MINUTES` narrowed back under the floor fails to *compile* rather
/// than failing two wall minutes into a nightly with the ledger's refusal —
/// which is how #1053's floor landing was found, three legs downstream of the
/// fixture that was actually wrong.
const LEDGER_MIN_MEASURED_MINUTES: f64 = 1.0;
const _: () = assert!(
    BANKED_MINUTES >= LEDGER_MIN_MEASURED_MINUTES,
    "BANKED_MINUTES is under p4-ledger.sh's MIN_MEASURED_MINUTES; the ledger will refuse it (#1053)"
);
/// The upper bound restated in the same place. A seat is connected for about
/// `ATTEMPT_SECONDS`, and the derivation clamps a claim to that bracket
/// *silently*, so an over-claim shows up as a manifest mismatch rather than as
/// a refusal. Half a minute of margin is deliberate: the measured
/// bracket-versus-nominal drift on this leg is a few hundred milliseconds.
const _: () = assert!(
    BANKED_MINUTES + 0.5 <= ATTEMPT_SECONDS as f64 / 60.0,
    "BANKED_MINUTES leaves no margin under the seat's connected span; raise ATTEMPT_SECONDS"
);

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_p1-swarm")
}

fn repo_root() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("gates/p1-swarm sits two levels below the repo root")
        .to_path_buf()
}

/// The 32-byte Ed25519 seed `gates/p1-swarm` derives every slot identity from.
///
/// Reproduced rather than imported because the seam needs the *seed bytes*: the
/// client row is signed outside this process, by
/// `scripts/sign-campaign-measurement-fixture.py --secret-hex`, exactly as a
/// real client signs with its own transport secret.
fn slot_seed(index: usize) -> [u8; 32] {
    let mut seed = [0u8; 32];
    seed[0..8].copy_from_slice(&(index as u64 + 1).to_le_bytes());
    seed[31] = 0xB0;
    seed
}

fn slot_key(index: usize) -> iroh_base::SecretKey {
    iroh_base::SecretKey::from_bytes(&slot_seed(index))
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

fn wait_for_listening(path: &Path) -> (String, String) {
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
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

/// A UUIDv7 that is distinct per run.
///
/// The ledger dedups on a hash of the identity, and `attempt_id` is part of it,
/// so a constant here would make a second run of this test a re-dispatch of the
/// first rather than a new attempt — and the refusal that produced would be a
/// property of the fixture, not of the seam.
fn uuid_v7(tail: u16) -> String {
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("wall clock")
        .as_millis() as u64
        & 0xFFFF_FFFF_FFFF;
    let rand: u64 = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("wall clock")
        .subsec_nanos() as u64;
    format!(
        "{:08x}-{:04x}-7{:03x}-8{:03x}-{:08x}{:04x}",
        millis >> 16,
        millis & 0xFFFF,
        rand & 0xFFF,
        (rand >> 12) & 0xFFF,
        std::process::id(),
        tail,
    )
}

/// Runs one of the P4 scripts against a ledger private to this test.
///
/// `P4_LEDGER_FILE` is the whole isolation: without it `p4-ledger.sh append`
/// writes to `target/p4-ledger/hours.jsonl`, and a test that banked into the
/// operator's own ledger would both pollute it and dedup against it — the
/// second run of this test would then be refused as a re-dispatch of the first.
fn run_script(root: &Path, script: &str, args: &[&str], ledger: &Path) -> (bool, String, String) {
    let output = Command::new(root.join("scripts").join(script))
        .args(args)
        .env("P4_LEDGER_FILE", ledger)
        .output()
        .unwrap_or_else(|error| panic!("{script} runs: {error}"));
    (
        output.status.success(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

/// One client row, signed by the key the host admitted at `slot`.
fn signed_row(root: &Path, session_id: &str, slot: usize, target: &str) -> String {
    // The window the row says it played in, stated as the attempt's own length
    // rather than left as a constant. Nothing downstream parses these two
    // fields today — `p4-ledger.sh` asks only that they are non-empty strings —
    // so a row claiming `BANKED_MINUTES` of play inside a window shorter than
    // that would pass. It would also be false, and a fixture that is internally
    // false is a trap set for whichever clause reads these fields next.
    let wall_end = format!(
        "2026-09-03T12:{:02}:{:02}Z",
        ATTEMPT_SECONDS / 60,
        ATTEMPT_SECONDS % 60
    );
    let unsigned = serde_json::json!({
        "session_id": session_id,
        "wall_start": "2026-09-03T12:00:00Z",
        "wall_end": wall_end,
        "distinct_play_minutes": BANKED_MINUTES,
        "banked_minutes": BANKED_MINUTES,
        "platform_triple": target,
        "client_rev": "attempt-report-seam",
        "ruleset_id": "52",
        "ruleset_version": 16,
        "pipeline_digest": "unavailable-client-side",
        "actor": "human",
        "configured_impairment_profile": {
            "loss_pct": 3, "jitter_p50_ms": 100, "jitter_p99_ms": 100
        },
        "observed_loss_pct": 3,
        "observed_jitter_p50_ms": 100,
        "observed_jitter_p99_ms": 100,
        "afk_seconds": 0,
        "afk_capped": false,
        "impairment_mismatch": false,
    });
    let mut child = Command::new("python3")
        .arg(
            root.join("scripts")
                .join("sign-campaign-measurement-fixture.py"),
        )
        .arg("--secret-hex")
        .arg(
            slot_seed(slot)
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>(),
        )
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("the fixture signer starts");
    {
        use std::io::Write;
        child
            .stdin
            .as_mut()
            .expect("signer stdin")
            .write_all(unsigned.to_string().as_bytes())
            .expect("the unsigned row is written to the signer");
    }
    let output = child.wait_with_output().expect("signer finishes");
    assert!(output.status.success(), "the fixture signer failed");
    String::from_utf8(output.stdout).expect("the signed row is UTF-8")
}

#[test]
#[ignore = "three real processes at wall clock; run via --ignored or scripts/p1-swarm-gate.sh"]
fn a_real_host_report_banks_human_and_bot_hours_through_the_real_ledger() {
    let root = repo_root();
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("test clock")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("p1-seam-{}-{nonce}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let report_path = dir.join("report.json");
    let listening_path = dir.join("listening.txt");
    let active_seats_path = dir.join("active-seats.json");
    let journal_path = dir.join("slots.json");
    let host_err_path = dir.join("host.err");
    let records_path = dir.join("campaign-records.jsonl");
    let inputs_dir = dir.join("inputs");
    let ledger = dir.join("hours.jsonl");

    let attempt_id = uuid_v7(0);
    let sessions = [(4usize, uuid_v7(4)), (5usize, uuid_v7(5))];

    let issuer = iroh_base::SecretKey::from_bytes(&[0x5b; 32]);
    let key_id = 960;
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("wall clock")
        .as_millis() as u64;
    let rows = sessions
        .iter()
        .map(|(slot, session)| {
            serde_json::json!({
                "attempt_id": attempt_id,
                "slot": slot,
                "session_id": session,
                "node": slot_key(*slot).public().to_string(),
                "expires_at": now_ms / 1_000 + 600,
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

    // Impaired and witnessed, because those are the clauses the ledger refuses
    // without: an unwitnessed hour measured no false-positive rate, and a clean
    // link is a fine run and not one of #240's 500.
    let mut host = Command::new(bin())
        .args([
            "--peers",
            &BOTS.to_string(),
            // Exactly as many exterior slots as seats that will be filled, so
            // the island the bots are dealt into is the island that plays.
            //
            // An earlier note here blamed the coverage refusal on spare slots
            // arming watches. That does not hold at this commit: an unoccupied
            // slot never enters `Swarm::exteriors`, so nothing can be armed
            // against it. The refusal was #963 — a *released* seat triggering
            // a re-arm against a surviving one — and it reproduced at
            // `--external-slots 2` with every slot filled.
            "--external-slots",
            "2",
            // A *timeout*, not a duration: the host's lobby loop exits the
            // moment `pending.len() == args.external_slots`, so with both
            // runners dialled this closes in about a second and the 45 is only
            // the budget for two processes to boot and finish their handshakes. What it must
            // not be is short enough to close on one seat — a lobby that times
            // out mid-handshake seats a human partway into the attempt and
            // shrinks the connected span its row is held under, which is the
            // ceiling `BANKED_MINUTES` sits below.
            "--lobby-seconds",
            "45",
            "--seconds",
            &ATTEMPT_SECONDS.to_string(),
            "--min-cells",
            "1",
            "--impaired",
            "--witness",
            // #971: the seat's connected span is a wall-clock fact, and this is
            // the switch that lets the host stamp one. Without it the seam
            // below falls back to scaling a tick count at the nominal rate,
            // which is the arithmetic that refused seven honest attempts.
            "--stamp-wall-clock",
            "--external-peer",
            "--attempt-id",
            &attempt_id,
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
    for (slot, session) in &sessions {
        remotes.push(
            Command::new(bin())
                .args([
                    "--external",
                    "--peers",
                    &BOTS.to_string(),
                    "--external-slots",
                    "2",
                    "--slot",
                    &slot.to_string(),
                    "--seconds",
                    &ATTEMPT_SECONDS.to_string(),
                    "--witness",
                    "--host-node",
                    &host_node,
                    "--host-direct",
                    &host_direct,
                    "--session-id",
                    session,
                    "--session-token",
                    &token_hex(&issuer, key_id, *slot, now_ms),
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
            "seated runner {offset} did not survive its own run"
        );
    }
    let host_status = host.wait().expect("host wait");
    if !host_status.success() {
        eprintln!(
            "{}",
            std::fs::read_to_string(&host_err_path).unwrap_or_default()
        );
        // The report is written before the criterion is judged, so a failing
        // host still leaves the evidence that says *why*. Naming the clause
        // here is the difference between a diagnosis and "the host failed".
        //
        // #963 was the clause this leg tripped: a seat's release re-armed
        // host-side watches against the seat that stayed, using a lobby anchor
        // thousands of ticks stale, so each such watch was shown its subject's
        // whole timeline and judged none of it. It is fixed, and this assertion
        // stays as its live guard — the unit test pins the arming decision, and
        // only three real processes can prove the timing race is gone.
        if let Ok(raw) = std::fs::read(&report_path) {
            if let Ok(partial) = serde_json::from_slice::<serde_json::Value>(&raw) {
                let coverage = partial["observation_coverage"].as_f64().unwrap_or(1.0);
                assert!(
                    coverage >= 0.95,
                    "observation coverage {coverage:.4}: a host-side watch was \
                     shown timeline it never judged, which is #963's shape and not \
                     a failure of the accounting seam. Read the host's \
                     'watches never folded a frame' line beside its 'live seat N \
                     released at tick T' lines: if a watch was armed against a seat \
                     that was still connected when another left, #963 has \
                     regressed. total_bad_body {} and total_undecodable {} — \
                     neither #961 nor #964 is in play here.",
                    partial["total_bad_body"],
                    partial["total_undecodable"],
                );
            }
        }
    }
    assert!(
        host_status.success(),
        "the host failed; its words are above"
    );

    // ── The report the host actually wrote, read but never edited ───────────
    let report: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&report_path).expect("report written"))
            .expect("report parses");
    assert_eq!(
        report["attempt_id"], attempt_id,
        "the host must serialise the attempt id it was given; without it the \
         derivation has nothing to bind rows to (#960)"
    );
    assert_eq!(
        report["bots"].as_u64(),
        Some(BOTS as u64),
        "the report must say how many bot seats it ran, or a human row cannot be \
         shown to sit above them"
    );
    assert_eq!(
        report["valid_attempt_seconds"].as_u64(),
        Some(ATTEMPT_SECONDS),
        "the bot contribution's numerator is the attempt's own measured seconds"
    );
    assert_eq!(
        report["completed"].as_bool(),
        Some(true),
        "a partial attempt banks nothing, so the report must say it completed"
    );
    // The two receive-path counters, reported rather than assumed, because this
    // leg is the only place either is measured on a seated attempt.
    //
    // `total_bad_body` was #961 — the receiver charged every replicated `Rock`
    // to it, thousands per run — and #962 fixed it by re-keying `Replica` on
    // `PersistId`. `total_undecodable` is #964, still open: a host that strips
    // the channel tag in `receive_peer_packets` leaves every witness decoder
    // calling `untag` on a payload that no longer has one, discarding 100% of a
    // real regolith client's witness records. Both read **0** on this leg, and
    // the second is the load-bearing one: this test seats `p1-swarm --external`
    // runners rather than the shipped client, so #964's path is not on it. That
    // is a bound on what this test proves, not evidence that #964 is fixed.
    eprintln!(
        "attempt-report-seam: total_bad_body = {} (#961/#962), total_undecodable = {} (#964 \
         is not on this path: the seat is a p1-swarm --external runner, not the regolith client)",
        report["total_bad_body"], report["total_undecodable"]
    );
    assert_eq!(
        report["total_bad_body"].as_u64(),
        Some(0),
        "the harness must observe what it sends; #962 fixed this and a regression \
         here makes every seated attempt unbankable again (#961)"
    );
    // #963, named here rather than left to surface as a bare coverage refusal
    // three steps downstream. Roughly one seated run in four arms an exterior as
    // a host-side watcher of every bot; the host never receives that seat's
    // judgements, so every tick it is shown is shown-and-unjudged and coverage
    // lands at exactly 3/4. Both seats connect cleanly in that case, so nothing
    // earlier in this test distinguishes it.
    let coverage = report["observation_coverage"]
        .as_f64()
        .expect("the report measures its own observation coverage");
    assert!(
        coverage >= 0.95,
        "observation coverage {coverage:.4} — an exterior was armed as a watcher \
         that folded nothing (#963), not a failure of the accounting seam"
    );

    let host_target = report["identity"]["target"]
        .as_str()
        .expect("the report names the host target triple")
        .to_owned();

    let exteriors = report["external"]
        .as_array()
        .expect("the report names its exterior seats");
    assert_eq!(exteriors.len(), 2, "both reserved seats were seated");
    for (slot, session) in &sessions {
        let seat = exteriors
            .iter()
            .find(|seat| seat["index"].as_u64() == Some(*slot as u64))
            .unwrap_or_else(|| panic!("the report seats slot {slot}"));
        assert_eq!(
            seat["session_id"].as_str(),
            Some(session.as_str()),
            "the host must carry the invite id it seated; it is the binding the \
             contract prefers over the admitted node"
        );
        assert_eq!(
            seat["node"].as_str(),
            Some(slot_key(*slot).public().to_string().as_str()),
            "the seat names the QUIC-authenticated identity it admitted"
        );
        assert!(
            matches!(
                seat["close"].as_str(),
                Some("goodbye" | "attempt_end" | "disconnected")
            ),
            "slot {slot} closed as {:?}, which banks nothing",
            seat["close"]
        );
        let connected_ticks = seat["connected_ticks"]
            .as_u64()
            .expect("the seat's own connected span");
        let ticks = report["ticks"].as_u64().expect("the attempt's tick count");
        let seconds = report["seconds"].as_u64().expect("the attempt's seconds");
        let nominal_minutes = connected_ticks as f64 * (seconds as f64 / ticks as f64) / 60.0;

        // #971. The host's own wall bracket is the basis the accounting uses,
        // and this is the one place it is read off a *real* host rather than a
        // fixture — which is the whole point, because in a fixture the host's
        // tick rate and the client's agree by construction and the defect
        // cannot appear. The host's metronome sleeps out the remainder of a
        // tick and accumulates no deadline, so it runs at or below its nominal
        // rate and never makes an overrun back: the bracket is therefore at
        // least as long as the nominal scaling of the same seat, and the gap
        // between the two is this run's measured drift.
        let since = seat["connected_since_unix_millis"]
            .as_u64()
            .expect("the host stamps when it bound this seat");
        let until = seat["connected_until_unix_millis"]
            .as_u64()
            .expect("the host stamps when this seat stopped being connected");
        assert!(
            until >= since,
            "slot {slot} reports a bracket that runs backwards: {since} to {until}"
        );
        let connected_minutes = (until - since) as f64 / 60_000.0;
        assert!(
            connected_minutes + 1e-9 >= nominal_minutes,
            "slot {slot}: the host's wall bracket is {connected_minutes:.4} min but its \
             own tick count scales to {nominal_minutes:.4} min. A sleep-only metronome \
             cannot outrun its nominal rate, so this is a broken stamp, not fast ticking"
        );
        eprintln!(
            "seat {slot}: wall bracket {connected_minutes:.4} min, nominal tick scaling \
             {nominal_minutes:.4} min, drift {:.1} ms",
            (connected_minutes - nominal_minutes) * 60_000.0
        );
        assert!(
            connected_minutes >= BANKED_MINUTES,
            "slot {slot} was connected for {connected_minutes:.4} min, so a \
             {BANKED_MINUTES} min row would exceed its seat rather than test the seam"
        );
    }

    // ── The client half: one signed row per seat ────────────────────────────
    let records = sessions
        .iter()
        .map(|(slot, session)| signed_row(&root, session, *slot, &host_target))
        .collect::<Vec<_>>()
        .join("");
    std::fs::write(&records_path, records).expect("records written");

    // ── The seam: assemble, then bank ───────────────────────────────────────
    let (ok, stdout, stderr) = run_script(
        &root,
        "p4-campaign-session.sh",
        &[
            "assemble",
            report_path.to_str().expect("report path"),
            records_path.to_str().expect("records path"),
            inputs_dir.to_str().expect("inputs dir"),
            &sessions[0].1,
            &sessions[1].1,
        ],
        &ledger,
    );
    assert!(ok, "assemble refused the host's own report:\n{stderr}");
    let manifest: serde_json::Value =
        serde_json::from_str(&stdout).expect("assemble prints a manifest");

    let bot_hours = BOTS as f64 * ATTEMPT_SECONDS as f64 / 3600.0;
    let human_hours = 2.0 * BANKED_MINUTES / 60.0;
    assert!(
        (manifest["bot_hours"].as_f64().expect("bot hours") - bot_hours).abs() < 1e-9,
        "the bot contribution is B * valid_attempt_seconds / 3600"
    );
    assert!(
        (manifest["human_hours"].as_f64().expect("human hours") - human_hours).abs() < 1e-9,
        "each human banks its own signed interval"
    );
    let inputs = manifest["inputs"].as_array().expect("derived inputs");
    assert_eq!(
        inputs.len(),
        3,
        "one bot contribution and one row per human"
    );

    // #576's defect, asserted directly: the attempt's own cohort figure must
    // not reach a participant. One hour with four bots and two humans is 5.53
    // player-hours over three inputs, not 6.0 banked twice for sixteen.
    let cohort_total = report["player_hours"]
        .as_f64()
        .expect("cohort player_hours");
    for input in inputs {
        let derived: serde_json::Value = serde_json::from_slice(
            &std::fs::read(input.as_str().expect("input path")).expect("derived input readable"),
        )
        .expect("derived input parses");
        if derived["identity"]["actor"] == "human" {
            let hours = derived["player_hours"].as_f64().expect("row hours");
            assert!(
                (hours - BANKED_MINUTES / 60.0).abs() < 1e-9,
                "a human row banks banked_minutes / 60, not the cohort total"
            );
            assert!(
                (hours - cohort_total).abs() > 1e-9,
                "a human row must not carry the attempt's own player_hours"
            );
            assert_eq!(
                derived["binding"]["attempt_id"], attempt_id,
                "every derived row binds to the attempt the host reported"
            );
        }
    }

    // ── The real ledger, which refuses what the derivation did not earn ─────
    let mut appended = 0usize;
    for input in inputs {
        let path = input.as_str().expect("input path");
        let (ok, _, stderr) = run_script(&root, "p4-ledger.sh", &["append", path], &ledger);
        assert!(ok, "the ledger refused a derived row:\n{stderr}");
        appended += 1;
    }
    assert_eq!(appended, 3, "three rows appended");

    let banked_lines = std::fs::read_to_string(&ledger).expect("the ledger wrote hours");
    let total: f64 = banked_lines
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            serde_json::from_str::<serde_json::Value>(line).expect("a ledger line parses")
                ["player_hours"]
                .as_f64()
                .expect("a banked line carries its hours")
        })
        .sum();
    assert!(
        (total - (bot_hours + human_hours)).abs() < 1e-9,
        "the ledger banked {total} player-hours; the attempt derived \
         {} = {bot_hours} bot + {human_hours} human",
        bot_hours + human_hours
    );
    assert!(
        human_hours > 0.0 && total > bot_hours,
        "a human hour reached the ledger"
    );
}
