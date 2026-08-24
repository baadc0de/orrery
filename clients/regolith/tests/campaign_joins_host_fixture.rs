//! The joined campaign loop against an in-process host fixture (#386).
//!
//! The fixture is slice 1's host side, reduced to what one client needs and
//! nothing more: it verifies the dialler's transport identity the way
//! `bridge::host_accept` does, runs the join handshake from `exterior.rs`,
//! opens the downlink uni stream with its announce beacon, then pumps frames
//! both ways — impairing uplink datagrams with a deterministic pattern and
//! settling each decision with a #393 `UplinkAck`, exactly as the harness's
//! `ExteriorSlot::pump_uplink` does after the router decides.
//!
//! Everything asserted here is *cross-checked*: the fixture keeps ground
//! truth of what it dropped and skipped, and the client's measured numbers
//! must equal it. A measurement that echoed configuration could not do that.

#![allow(missing_docs)]

use std::sync::Arc;

use std::time::{Duration, Instant};
use tokio::io::AsyncWriteExt as _;

use bytes::Bytes;
use iroh_base::SecretKey;
use orrery_core::CoreCodec as _;
use orrery_games::Game;
use orrery_protocol::channels::encode_replication;
use orrery_protocol::{CellId, NodeId, PersistId};
use orrery_regolith_client::campaign::{CampaignConfig, CampaignRuntime, JoinState};
use orrery_regolith_client::intent::Controls;
use orrery_regolith_client::net;
use orrery_regolith_client::session::ConfiguredImpairment;
use orrery_regolith_client::telemetry::JsonlTelemetry;

/// The client slot under test (slot 0 is the fixture's virtual bot).
const CLIENT_SLOT: usize = 1;
/// The virtual bot broadcasts every three ticks, like a bot at 20 Hz on a
/// 60 Hz sim; its replication-tick field advances by this stride.
const STRIDE: u64 = 3;
/// Deterministic uplink impairment: sequence ≡ 3 (mod 4) is dropped.
const UPLINK_DROP_MOD: u64 = 4;
/// Deterministic downlink impairment: every 7th broadcast never leaves.
const DOWNLINK_SKIP_EVERY: u64 = 7;

fn configured() -> ConfiguredImpairment {
    // Deliberately wrong about the link: the row must flag the mismatch
    // rather than echo whatever the operator declared.
    ConfiguredImpairment {
        loss_pct: 25.0,
        jitter_p50_ms: 100,
        jitter_p99_ms: 100,
    }
}

/// What the fixture actually did — ground truth the client must measure.
#[derive(Default, Clone)]
struct GroundTruth {
    uplink_kept: u64,
    uplink_dropped: u64,
    downlink_broadcasts: u64,
    downlink_skipped: u64,
}

impl GroundTruth {
    fn uplink_total(&self) -> u64 {
        self.uplink_kept + self.uplink_dropped
    }
}

/// One host fixture: endpoint plus pump task plus its ground truth.
struct HostFixture {
    _runtime: Arc<tokio::runtime::Runtime>,
    _endpoint: iroh::Endpoint,
    node_hex: String,
    direct: String,
    truth: Arc<std::sync::Mutex<GroundTruth>>,
}

impl HostFixture {
    /// Bind and start pumping. `reject` refuses the handshake; `wrong_slot`
    /// accepts with a different index than the client derived.
    fn spawn(mode: Mode, session_id: &str) -> Self {
        let runtime = Arc::new(
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .expect("fixture runtime"),
        );
        // The host key namespace differs from slot keys by design (#385);
        // any fixed non-slot key works here.
        let mut seed = [0u8; 32];
        seed[0..8].copy_from_slice(&u64::MAX.to_le_bytes());
        seed[31] = 0xB1;
        let secret = SecretKey::from_bytes(&seed);
        let expected_client = net::slot_secret(CLIENT_SLOT).public();

        let truth: Arc<std::sync::Mutex<GroundTruth>> = Default::default();

        let endpoint = runtime.block_on(async { net::bind(secret).await.expect("bind") });
        let socket = endpoint.bound_sockets()[0];
        let node_hex = format!("{}", endpoint.id());

        let endpoint_for_task = endpoint.clone();
        let pump_truth = Arc::clone(&truth);
        let pump_runtime = Arc::clone(&runtime);
        let expected_session = session_id.to_owned();
        let _pump_thread = std::thread::spawn(move || {
            pump_runtime.block_on(async move {
                let _ = pump(
                    endpoint_for_task,
                    expected_client,
                    mode,
                    &expected_session,
                    pump_truth,
                )
                .await;
            });
        });

        Self {
            _runtime: runtime,
            _endpoint: endpoint,
            node_hex,
            direct: socket.to_string(),
            truth,
        }
    }

    fn config(&self, session_id: &str) -> CampaignConfig {
        CampaignConfig {
            host_node_hex: self.node_hex.clone(),
            host_direct: Some(self.direct.clone()),
            slot: CLIENT_SLOT,
            session_id: session_id.to_owned(),
            wall_start_utc: "2026-08-24T00:00:00Z".to_owned(),
            configured: configured(),
        }
    }

    fn truth(&self) -> GroundTruth {
        self.truth.lock().expect("truth lock").clone()
    }
}

#[derive(Clone, Copy)]
enum Mode {
    Join,
    Reject,
    WrongSlot,
}

/// The fixture's pump: accept → verify identity → handshake → data path.
///
/// One writer owns the downlink stream; broadcasts and acks feed it through
/// a channel whose sends are synchronous on an unbounded queue, so no frame
/// can be lost to an unpolled future (slice 1's rule, kept on the fixture
/// side too).
async fn pump(
    endpoint: iroh::Endpoint,
    expected_client: NodeId,
    mode: Mode,
    expected_session_id: &str,
    truth: Arc<std::sync::Mutex<GroundTruth>>,
) -> Result<(), String> {
    let incoming = endpoint
        .accept()
        .await
        .ok_or_else(|| "endpoint closed".to_string())?;
    let connection = incoming
        .accept()
        .map_err(|_| "join failed to start".to_string())?
        .await
        .map_err(|error| format!("handshake failed: {error}"))?;
    // Identity first, exactly as bridge::host_accept does: the transport
    // layer authenticated the peer before any application byte moved.
    assert_eq!(
        connection.remote_id(),
        expected_client,
        "the dialler's transport id must be the slot key"
    );

    // The remote opened and drives the handshake stream, as host_accept
    // notes: accept_bi here, not open_bi.
    let (mut send, mut recv) = connection.accept_bi().await.map_err(|e| e.to_string())?;
    let request_bytes = read_message(&mut recv).await?;
    let request = net::JoinRequest::decode(&request_bytes).expect("client spoke v3");
    assert!(
        !request.client_rev.is_empty(),
        "the client names its build revision"
    );
    assert!(
        !request.ships_anchor,
        "the rendered client declares it ships no witness anchor yet (#387)"
    );
    assert_eq!(
        request.session_id, expected_session_id,
        "the client presents the session identity it was launched with"
    );

    if matches!(mode, Mode::Reject) {
        let reason = b"campaign closed";
        let mut reply = vec![1u8, reason.len() as u8];
        reply.extend_from_slice(reason.as_slice());
        write_message(&mut send, &reply).await?;
        // Hold the connection open long enough for the reply to be
        // delivered: dropping it now can reset the stream before the
        // client reads the refusal (the goodbye-marker grace period,
        // applied to a rejection).
        tokio::time::sleep(Duration::from_millis(300)).await;
        return Ok(());
    }
    let assigned = match mode {
        Mode::WrongSlot => CLIENT_SLOT + 7,
        _ => CLIENT_SLOT,
    };
    let mut reply = vec![0u8];
    reply.extend_from_slice(&(assigned as u64).to_le_bytes());
    write_message(&mut send, &reply).await?;

    // Data path mirrors the host side: downlink opened and announced before
    // accepting the client's announced uplink (#385).
    drop(send);
    drop(recv);
    let mut downlink_send = connection.open_uni().await.map_err(|e| e.to_string())?;
    let announce = net::Frame {
        peer: u32::MAX,
        lane: net::Lane::Meta,
        payload: Bytes::new(),
    };
    write_frame(&mut downlink_send, &announce).await?;
    let mut uplink_recv = connection.accept_uni().await.map_err(|e| e.to_string())?;

    let (feed_tx, mut feed_rx) = tokio::sync::mpsc::unbounded_channel::<net::Frame>();
    tokio::spawn(async move {
        while let Some(frame) = feed_rx.recv().await {
            if write_frame(&mut downlink_send, &frame).await.is_err() {
                break;
            }
        }
    });

    // The virtual bot at slot 0: broadcast canonical craft state on a fixed
    // cadence, skipping every DOWNLINK_SKIP_EVERY-th broadcast.
    {
        let truth_cloned = Arc::clone(&truth);
        let feed = feed_tx.clone();
        tokio::spawn(async move {
            let game = orrery_games::Regolith::honest();
            let bot_entity = PersistId::new(1); // slot 0's entity id
            let state = game.spawn(bot_entity, 0);
            let cell = CellId::from_coords(bevy::math::IVec3::ONE, orrery_protocol::INTEREST_LEVEL)
                .expect("representable cell");
            let mut broadcast_index = 0u64;
            let mut interval = tokio::time::interval(Duration::from_millis(15));
            loop {
                interval.tick().await;
                broadcast_index += 1;
                if broadcast_index.is_multiple_of(DOWNLINK_SKIP_EVERY) {
                    truth_cloned.lock().expect("truth lock").downlink_skipped += 1;
                    continue;
                }
                let encoded = state.to_canonical();
                let payload = encode_replication(&(
                    encoded,
                    cell,
                    bot_entity,
                    broadcast_index * STRIDE, // the sender's absolute tick
                ));
                let frame = net::Frame {
                    peer: 0, // the sender's slot, per the downlink rule
                    lane: net::Lane::Datagram,
                    payload: Bytes::from(payload),
                };
                if feed.send(frame).is_err() {
                    break; // the writer is gone: client left
                }
                truth_cloned.lock().expect("truth lock").downlink_broadcasts += 1;
            }
        });
    }

    // Uplink: settle every sequenced datagram with a router decision, acked
    // strictly after deciding (#393's load-bearing ordering).
    loop {
        let Some(frame) = read_frame(&mut uplink_recv).await else {
            break; // the client went away
        };
        match frame.lane {
            net::Lane::Meta => continue, // cell reports; nothing to settle
            net::Lane::Datagram => {
                let Some(sequence) = decode_uplink_sequence(&frame.payload) else {
                    continue;
                };
                let dropped = sequence % UPLINK_DROP_MOD == UPLINK_DROP_MOD - 1;
                {
                    let mut truth = truth.lock().expect("truth lock");
                    if dropped {
                        truth.uplink_dropped += 1;
                    } else {
                        truth.uplink_kept += 1;
                    }
                }
                let mut payload = Vec::with_capacity(10);
                payload.push(0xa1);
                payload.push(u8::from(dropped));
                payload.extend_from_slice(&sequence.to_le_bytes());
                let ack = net::Frame {
                    peer: u32::MAX,
                    lane: net::Lane::Meta,
                    payload: Bytes::from(payload),
                };
                if feed_tx.send(ack).is_err() {
                    break;
                }
            }
            net::Lane::StreamShared | net::Lane::StreamBulk => {}
        }
    }
    Ok(())
}

async fn write_frame(
    send: &mut iroh::endpoint::SendStream,
    frame: &net::Frame,
) -> Result<(), String> {
    let mut wire = Vec::with_capacity(9 + frame.payload.len());
    frame
        .encode(&mut wire)
        .map_err(|_| "frame exceeds the wire bound".to_string())?;
    send.write_all(&wire).await.map_err(|e| e.to_string())?;
    send.flush().await.map_err(|e| e.to_string())
}

async fn read_message(recv: &mut iroh::endpoint::RecvStream) -> Result<Vec<u8>, String> {
    let mut header = [0u8; 4];
    recv.read_exact(&mut header)
        .await
        .map_err(|e| e.to_string())?;
    let len = u32::from_le_bytes(header) as usize;
    let mut body = vec![0u8; len];
    recv.read_exact(&mut body)
        .await
        .map_err(|e| e.to_string())?;
    Ok(body)
}

async fn write_message(send: &mut iroh::endpoint::SendStream, body: &[u8]) -> Result<(), String> {
    send.write_all(&(body.len() as u32).to_le_bytes())
        .await
        .map_err(|e| e.to_string())?;
    send.write_all(body).await.map_err(|e| e.to_string())?;
    // quinn buffers written bytes per-stream; an unflushed handshake reply
    // sat unsent while the connection closed under it (#385's lesson).
    send.flush().await.map_err(|e| e.to_string())
}

/// One frame off the ordered stream; `None` on a clean end mid-boundary.
async fn read_frame(recv: &mut iroh::endpoint::RecvStream) -> Option<net::Frame> {
    let mut header = [0u8; 9];
    recv.read_exact(&mut header).await.ok()?;
    let lane = match header[0] {
        0 => net::Lane::Datagram,
        1 => net::Lane::StreamShared,
        2 => net::Lane::StreamBulk,
        3 => net::Lane::Meta,
        other => panic!("fixture received an unknown lane byte {other}"),
    };
    let peer = u32::from_le_bytes(header[1..5].try_into().expect("nine bytes"));
    let len = u32::from_le_bytes(header[5..9].try_into().expect("nine bytes")) as usize;
    let mut payload = vec![0u8; len];
    recv.read_exact(&mut payload).await.ok()?;
    Some(net::Frame {
        peer,
        lane,
        payload: Bytes::from(payload),
    })
}

fn decode_uplink_sequence(payload: &[u8]) -> Option<u64> {
    if payload.len() < 8 {
        return None;
    }
    Some(u64::from_le_bytes(
        payload[..8].try_into().expect("eight bytes"),
    ))
}

fn crate_seed() -> orrery_protocol::UniverseSeed {
    orrery_protocol::UniverseSeed([0x61; 32])
}

fn sink_for(name: &str) -> JsonlTelemetry {
    let path = std::env::temp_dir().join(format!("{name}-{}.jsonl", std::process::id()));
    JsonlTelemetry::open(&path).expect("telemetry sink")
}

/// Drive `wanted` joined ticks over the live link.
fn drive_until_joined(runtime: &mut CampaignRuntime, sink: &mut JsonlTelemetry, wanted: u64) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        runtime.poll_join();
        match runtime.state() {
            JoinState::Joined => break,
            JoinState::Dialing => std::thread::sleep(Duration::from_millis(50)),
            other => panic!("the join ended before it began ticking: {other:?}"),
        }
    }
    assert_eq!(
        *runtime.state(),
        JoinState::Joined,
        "the client must reach Joined against a live fixture"
    );
    while runtime.joined_ticks() < wanted && Instant::now() < deadline {
        let report = runtime.advance(Controls::default(), sink);
        let _ = report;
        // Faster than the sim rate for CI, slow enough that the pumps are
        // exercised across many wakeups.
        std::thread::sleep(Duration::from_millis(2));
    }
}

#[test]
fn a_client_joins_measures_and_applies_replicated_state() {
    let fixture = HostFixture::spawn(Mode::Join, "it-session");
    let mut sink = sink_for("regolith-campaign-it");
    let mut runtime = CampaignRuntime::launch(fixture.config("it-session"), crate_seed());

    drive_until_joined(&mut runtime, &mut sink, 150);

    // Let every outstanding datagram reach its settled decision.
    let deadline = Instant::now() + Duration::from_secs(10);
    while runtime.uplink_acks().0 < runtime.uplink_sent() && Instant::now() < deadline {
        let _ = runtime.advance(Controls::default(), &mut sink);
        std::thread::sleep(Duration::from_millis(2));
    }
    let truth = fixture.truth();
    assert!(truth.uplink_total() > 10, "the fixture saw real traffic");
    assert_eq!(
        runtime.uplink_sent(),
        truth.uplink_total(),
        "every queued datagram reached the router"
    );
    assert_eq!(
        runtime.uplink_acks(),
        (truth.uplink_total(), truth.uplink_dropped),
        "uplink loss is the router's settled decisions, ack for ack"
    );
    let (_, dropped) = runtime.uplink_acks();
    assert!(dropped > 0, "the impairment pattern actually bit");

    // Downlink: arrivals plus gaps reconcile against the sender's ledger.
    let (arrivals, missing) = runtime.downlink_accounting();
    assert!(arrivals > 10, "replication arrived");
    assert!(
        missing > 0,
        "the deterministic skip pattern must register as gaps"
    );
    assert_eq!(
        arrivals + missing,
        truth.downlink_broadcasts + truth.downlink_skipped,
        "gap accounting reconciles with what the sender actually sent"
    );

    // The replicated view holds the virtual bot, and the accumulator ran.
    assert_eq!(runtime.focus(), Some(PersistId::new(1)));
    assert!(runtime.executor().state(PersistId::new(1)).is_some());
    assert!(
        runtime.joined_ticks() >= 150,
        "every joined tick was accounted: {}",
        runtime.joined_ticks()
    );
    let progress = runtime.accumulator().progress();
    assert!(progress.banked_minutes > 0.0);
    assert!(progress.joined_session_ran);

    // The finished row carries the measurements and flags the declared
    // profile as wrong rather than echoing it.
    let record = runtime.shutdown().expect("one row per session");
    assert_eq!(record.session_id, "it-session");
    assert!(record.observed_loss_pct > 0.0);
    assert!(record.impairment_mismatch, "the declared profile was a lie");

    drop(sink);
}

#[test]
fn a_refused_join_never_proceeds_as_if_local() {
    let fixture = HostFixture::spawn(Mode::Reject, "refused");
    let mut sink = sink_for("regolith-campaign-reject");
    let mut runtime = CampaignRuntime::launch(fixture.config("refused"), crate_seed());

    let deadline = Instant::now() + Duration::from_secs(15);
    while matches!(runtime.state(), JoinState::Dialing) && Instant::now() < deadline {
        runtime.poll_join();
        std::thread::sleep(Duration::from_millis(20));
    }
    // The refusal is named, not swallowed into a local fallback.
    match runtime.state() {
        JoinState::Refused(reason) => assert_eq!(reason, "campaign closed"),
        other => panic!("expected Refused, got {other:?}"),
    }
    // Not a single tick of local play happened: no intents produced, no
    // accumulator progress, nothing bankable.
    let controls = Controls {
        thrust: true,
        fire: true,
        ..Controls::default()
    };
    let report = runtime.advance(controls, &mut sink);
    assert_eq!(report.intents, 0, "a refused campaign drives nothing");
    assert_eq!(report.events.len(), 0);
    assert_eq!(runtime.joined_ticks(), 0);
    let progress = runtime.accumulator().progress();
    assert_eq!(progress.banked_minutes, 0.0);
    assert!(runtime.shutdown().is_none(), "no record without a session");

    drop(sink);
}

#[test]
fn a_slot_mismatch_is_a_named_failure() {
    let fixture = HostFixture::spawn(Mode::WrongSlot, "slotless");
    let sink = sink_for("regolith-campaign-slot");
    let mut runtime = CampaignRuntime::launch(fixture.config("slotless"), crate_seed());

    let deadline = Instant::now() + Duration::from_secs(15);
    while matches!(runtime.state(), JoinState::Dialing) && Instant::now() < deadline {
        runtime.poll_join();
        std::thread::sleep(Duration::from_millis(20));
    }
    match runtime.state() {
        JoinState::Failed(reason) => assert!(
            reason.contains("assigned slot"),
            "the mismatch names itself: {reason}"
        ),
        other => panic!("expected Failed(slot mismatch), got {other:?}"),
    }
    assert_eq!(runtime.joined_ticks(), 0);

    drop(sink);
}
