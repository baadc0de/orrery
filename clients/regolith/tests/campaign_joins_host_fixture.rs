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
use orrery_core::{CoreCodec as _, Executor};
use orrery_games::regolith::archetype::Archetype;
use orrery_games::regolith::campaign_spawn_pose;
use orrery_games::regolith::order::{Order, Outcome};
use orrery_games::regolith::state::{Craft, RegolithState};
use orrery_games::{Game, Regolith};
use orrery_protocol::channels::encode_replication;
use orrery_protocol::{CellId, ChainHash, NodeId, PersistId, StateClaim, Tick, WitnessMsg};
use orrery_regolith_client::campaign::{CampaignConfig, CampaignRuntime, JoinState};
use orrery_regolith_client::combat::{LockBreak, ProjectileTracks, ShotCue, ShotFeedback};
use orrery_regolith_client::intent::Controls;
use orrery_regolith_client::net;
use orrery_regolith_client::observe_skin_effects;
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
    downlink_last_generated: u64,
    downlink_last_queued: u64,
    downlink_last_written: u64,
    downlink_skipped_indices: Vec<u64>,
    witness_frames_verified: u64,
    witness_claims_verified: u64,
    inbound_records_verified: u64,
    delivered_inputs_applied: u64,
    lock_confirmations_sent: u64,
    damage_inputs_applied: u64,
    shots_resolved: u64,
    lock_breaks_sent: u64,
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
    quiesce: tokio::sync::mpsc::UnboundedSender<QuiesceRequest>,
}

struct QuiesceRequest {
    flushed: std::sync::mpsc::SyncSender<u64>,
}

enum FeedItem {
    Frame {
        broadcast_index: Option<u64>,
        frame: net::Frame,
    },
    Barrier {
        terminal_index: u64,
        flushed: std::sync::mpsc::SyncSender<u64>,
    },
}

impl HostFixture {
    /// Bind and start pumping. `reject` refuses the handshake; `wrong_slot`
    /// accepts with a different index than the client derived.
    fn spawn(mode: Mode) -> Self {
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
        let (quiesce, quiesce_rx) = tokio::sync::mpsc::unbounded_channel();

        let endpoint = runtime.block_on(async { net::bind(secret).await.expect("bind") });
        let socket = endpoint.bound_sockets()[0];
        let node_hex = format!("{}", endpoint.id());

        let endpoint_for_task = endpoint.clone();
        let pump_truth = Arc::clone(&truth);
        let pump_runtime = Arc::clone(&runtime);
        let _pump_thread = std::thread::spawn(move || {
            pump_runtime.block_on(async move {
                let _ = pump(
                    endpoint_for_task,
                    expected_client,
                    mode,
                    pump_truth,
                    quiesce_rx,
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
            quiesce,
        }
    }

    fn config(&self, session_id: &str) -> CampaignConfig {
        CampaignConfig {
            host_node_hex: self.node_hex.clone(),
            host_direct: Some(self.direct.clone()),
            slot: CLIENT_SLOT,
            session_id: session_id.to_owned(),
            session_token_hex: None,
            wall_start_utc: "2026-08-24T00:00:00Z".to_owned(),
            configured: configured(),
            transport_secret: net::slot_secret(CLIENT_SLOT),
            roster_url: None,
        }
    }

    fn truth(&self) -> GroundTruth {
        self.truth.lock().expect("truth lock").clone()
    }

    /// Stop broadcast production at a delivered frame and wait until the
    /// fixture's ordered stream writer has flushed that terminal frame.
    fn quiesce(&self) -> u64 {
        let (flushed, receiver) = std::sync::mpsc::sync_channel(0);
        self.quiesce
            .send(QuiesceRequest { flushed })
            .expect("the fixture broadcaster is live");
        receiver
            .recv_timeout(Duration::from_secs(10))
            .expect("the fixture flushed its terminal broadcast")
    }
}

#[derive(Clone, Copy)]
enum Mode {
    Join,
    TargetDestroyedBeforeDamage,
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
    truth: Arc<std::sync::Mutex<GroundTruth>>,
    mut quiesce: tokio::sync::mpsc::UnboundedReceiver<QuiesceRequest>,
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

    if matches!(mode, Mode::Join | Mode::TargetDestroyedBeforeDamage) {
        let claim_bytes = read_message(&mut recv).await?;
        let state_bytes = read_message(&mut recv).await?;
        let claim: StateClaim =
            serde_json::from_slice(&claim_bytes).map_err(|error| error.to_string())?;
        let state = orrery_games::regolith::state::RegolithState::decode(&state_bytes)
            .map_err(|error| error.to_string())?;
        assert_eq!(claim.entity, PersistId::new(CLIENT_SLOT as u64 + 1));
        assert_eq!(claim.tick, Tick::new(0));
        orrery_core::log::verify_claim(&claim, expected_client)
            .expect("the rendered client signs its anchor with its transport identity");
        assert_eq!(
            orrery_core::state_hash(&state),
            claim.state_hash,
            "the rendered client's tick-zero state matches its signed claim"
        );
    }

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

    let (feed_tx, mut feed_rx) = tokio::sync::mpsc::unbounded_channel::<FeedItem>();
    let writer_truth = Arc::clone(&truth);
    tokio::spawn(async move {
        while let Some(item) = feed_rx.recv().await {
            match item {
                FeedItem::Frame {
                    broadcast_index,
                    frame,
                } => {
                    if write_frame(&mut downlink_send, &frame).await.is_err() {
                        break;
                    }
                    if let Some(index) = broadcast_index {
                        writer_truth
                            .lock()
                            .expect("truth lock")
                            .downlink_last_written = index;
                    }
                }
                FeedItem::Barrier {
                    terminal_index,
                    flushed,
                } => {
                    let _ = flushed.send(terminal_index);
                }
            }
        }
    });

    // The virtual bot at slot 0: broadcast canonical craft state on a fixed
    // cadence, skipping every DOWNLINK_SKIP_EVERY-th broadcast.
    {
        let truth_cloned = Arc::clone(&truth);
        let feed = feed_tx.clone();
        tokio::spawn(async move {
            let bot_entity = PersistId::new(1); // slot 0's entity id
            let state = campaign_craft(0, CLIENT_SLOT + 1);
            let cell = CellId::from_coords(bevy::math::IVec3::ONE, orrery_protocol::INTEREST_LEVEL)
                .expect("representable cell");
            let mut broadcast_index = 0u64;
            let mut interval = tokio::time::interval(Duration::from_millis(15));
            loop {
                let request = tokio::select! {
                    biased;
                    request = quiesce.recv() => request,
                    _ = interval.tick() => None,
                };

                // A quiescent cut must end in a delivered broadcast: a trailing
                // intentional skip is only measurable when a later arrival
                // closes its tick gap.
                loop {
                    broadcast_index += 1;
                    {
                        let mut truth = truth_cloned.lock().expect("truth lock");
                        truth.downlink_last_generated = broadcast_index;
                        if broadcast_index.is_multiple_of(DOWNLINK_SKIP_EVERY) {
                            truth.downlink_skipped += 1;
                            truth.downlink_skipped_indices.push(broadcast_index);
                            if request.is_none() {
                                break;
                            }
                            continue;
                        }
                    }

                    let encoded = state.to_canonical();
                    // The harness wire is double-tagged: `send_peer_packets`
                    // wraps `encode_replication`'s output in its own channel tag.
                    let payload = orrery_protocol::channels::tag(
                        orrery_protocol::channels::Channel::State,
                        &encode_replication(&(encoded, cell, bot_entity, broadcast_index * STRIDE)),
                    );
                    let frame = net::Frame {
                        peer: 0,
                        lane: net::Lane::Datagram,
                        payload: Bytes::from(payload),
                    };
                    if feed
                        .send(FeedItem::Frame {
                            broadcast_index: Some(broadcast_index),
                            frame,
                        })
                        .is_err()
                    {
                        return;
                    }
                    let mut truth = truth_cloned.lock().expect("truth lock");
                    truth.downlink_broadcasts += 1;
                    truth.downlink_last_queued = broadcast_index;
                    break;
                }

                if let Some(request) = request {
                    let _ = feed.send(FeedItem::Barrier {
                        terminal_index: broadcast_index,
                        flushed: request.flushed,
                    });
                    break;
                }
            }
        });
    }

    // Uplink: settle every sequenced datagram with a router decision, acked
    // strictly after deciding (#393's load-bearing ordering).
    let mut witness_head = ChainHash::EMPTY;
    let game = Regolith::honest();
    let bot_entity = PersistId::new(1);
    let client_entity = PersistId::new(CLIENT_SLOT as u64 + 1);
    let mut authority = Executor::new(game, crate_seed());
    authority.insert(bot_entity, campaign_craft(0, CLIENT_SLOT + 1));
    let mut authority_tick = 0u64;
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
                if feed_tx
                    .send(FeedItem::Frame {
                        broadcast_index: None,
                        frame: ack,
                    })
                    .is_err()
                {
                    break;
                }
            }
            net::Lane::StreamShared => {
                let delivered = orrery_protocol::channels::untag(&frame.payload)
                    .filter(|(channel, _)| *channel == orrery_protocol::channels::Channel::Control)
                    .and_then(|(_, inner)| {
                        orrery_protocol::channels::decode_delivered_input(inner)
                    });
                if let Some(delivered) = delivered {
                    assert_eq!(frame.peer, 0, "the client addressed the target's host slot");
                    assert_eq!(
                        delivered.from, client_entity,
                        "the delivery names the authority that emitted it"
                    );
                    assert_eq!(
                        delivered.recipient, bot_entity,
                        "the host applies only its own entity's delivery"
                    );
                    let order = Order::decode(&delivered.input)
                        .expect("the client sent canonical Regolith input bytes");
                    {
                        let mut observed = truth.lock().expect("truth lock");
                        observed.delivered_inputs_applied += 1;
                        observed.damage_inputs_applied +=
                            u64::from(matches!(order, Order::Damage { .. }));
                    }

                    if matches!(mode, Mode::TargetDestroyedBeforeDamage)
                        && matches!(order, Order::Damage { .. })
                    {
                        let RegolithState::Craft(mut target) = authority
                            .state(bot_entity)
                            .expect("the fixture host owns its target")
                            .clone()
                        else {
                            panic!("the fixture target is a craft");
                        };
                        target.hull = 0;
                        authority.insert(bot_entity, RegolithState::Craft(target));
                    }

                    // A real target authority step produces the reply. Self-
                    // addressed projectile continuations are stepped on
                    // successive host ticks until they resolve; no target
                    // state is guessed by the client or fixture.
                    let mut pending = vec![order];
                    while !pending.is_empty() {
                        let output = authority
                            .step_entity(bot_entity, Tick::new(authority_tick), &pending)
                            .expect("the fixture host owns its target");
                        authority_tick = authority_tick.saturating_add(1);
                        pending.clear();
                        for event in &output.events {
                            if matches!(event, Outcome::ShotResolved { .. }) {
                                truth.lock().expect("truth lock").shots_resolved += 1;
                            }
                            let Some((recipient, reply)) = authority.ruleset().deliver(event)
                            else {
                                continue;
                            };
                            if recipient == bot_entity {
                                pending.push(reply);
                                continue;
                            }
                            assert_eq!(
                                recipient, client_entity,
                                "the two-authority fixture has no third route"
                            );
                            if matches!(reply, Order::LockConfirmed { .. }) {
                                truth.lock().expect("truth lock").lock_confirmations_sent += 1;
                            }
                            if matches!(reply, Order::LockBroken { .. }) {
                                truth.lock().expect("truth lock").lock_breaks_sent += 1;
                            }
                            let inner = orrery_protocol::channels::encode_delivered_input(
                                bot_entity,
                                recipient,
                                &reply.to_canonical(),
                            );
                            let payload = orrery_protocol::channels::tag(
                                orrery_protocol::channels::Channel::Control,
                                &inner,
                            );
                            feed_tx
                                .send(FeedItem::Frame {
                                    broadcast_index: None,
                                    frame: net::Frame {
                                        peer: 0,
                                        lane: net::Lane::StreamShared,
                                        payload: Bytes::from(payload),
                                    },
                                })
                                .map_err(|_| "downlink writer stopped".to_string())?;
                        }
                    }
                    continue;
                }

                let message =
                    orrery_protocol::channels::decode_witness::<WitnessMsg>(&frame.payload)
                        .expect("the rendered client's shared stream carries a witness message");
                match message {
                    WitnessMsg::Frame { frame, heads } => {
                        assert_eq!(heads.len(), 1, "one authored entity per client frame");
                        let inbound = frame
                            .entities
                            .iter()
                            .flat_map(|slice| &slice.records)
                            .filter(|record| {
                                matches!(
                                    record.source,
                                    orrery_protocol::RecordSource::InboundEvent { from }
                                        if from == bot_entity
                                ) && matches!(
                                    Order::decode(&record.payload),
                                    Ok(Order::LockConfirmed { .. })
                                )
                            })
                            .count() as u64;
                        let transitions = orrery_core::log::verify_frame(
                            &frame,
                            expected_client,
                            &[witness_head],
                        )
                        .expect("the host verifies the rendered client's real log frame");
                        assert_eq!(heads[0].head, transitions[0].head);
                        witness_head = transitions[0].head;
                        let mut observed = truth.lock().expect("truth lock");
                        observed.witness_frames_verified += 1;
                        observed.inbound_records_verified += inbound;
                    }
                    WitnessMsg::Claim(claim) => {
                        orrery_core::log::verify_claim(&claim, expected_client)
                            .expect("the host verifies the rendered client's periodic claim");
                        truth.lock().expect("truth lock").witness_claims_verified += 1;
                    }
                    other => panic!("unexpected client-authored witness message: {other:?}"),
                }
            }
            net::Lane::StreamBulk => {}
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

fn campaign_craft(slot: usize, count: usize) -> RegolithState {
    let (pos, yaw_urad) = campaign_spawn_pose(slot, count);
    RegolithState::Craft(Craft::spawned(
        Archetype::for_slot(slot as u64),
        pos,
        yaw_urad,
    ))
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
        let report = runtime.advance(
            Controls {
                thrust: true,
                ..Controls::default()
            },
            sink,
        );
        let _ = report;
        // Faster than the sim rate for CI, slow enough that the pumps are
        // exercised across many wakeups.
        std::thread::sleep(Duration::from_millis(2));
    }
}

#[test]
fn a_human_campaign_lock_fire_round_trip_resolves_on_the_host() {
    let fixture = HostFixture::spawn(Mode::Join);
    let mut sink = sink_for("regolith-campaign-authority-loopback");
    let mut runtime = CampaignRuntime::launch(fixture.config("authority-loopback"), crate_seed());
    drive_until_joined(&mut runtime, &mut sink, 1);

    let target = PersistId::new(1);
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        let _ = runtime.advance(
            Controls {
                lock_target: Some(target),
                right: true,
                ..Controls::default()
            },
            &mut sink,
        );
        let ready = matches!(
            runtime.executor().state(runtime.entity()),
            Some(RegolithState::Craft(craft))
                if craft.lock_target == Some(target)
                    && craft.lock_class.is_some()
                    && craft.lock_progress >= orrery_games::regolith::LOCK_ACQUISITION_TICKS
        );
        if ready {
            break;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(
        matches!(
            runtime.executor().state(runtime.entity()),
            Some(RegolithState::Craft(craft))
                if craft.lock_target == Some(target)
                    && craft.lock_class.is_some()
                    && craft.lock_progress >= orrery_games::regolith::LOCK_ACQUISITION_TICKS
        ),
        "the host-authored LockConfirmed must mature the human's visible lock"
    );

    let mut tracks = ProjectileTracks::default();
    let mut broken = LockBreak::default();
    let mut shots = ShotFeedback::default();
    let fired = runtime.advance(
        Controls {
            fire: true,
            lock_target: Some(target),
            ..Controls::default()
        },
        &mut sink,
    );
    assert!(
        fired.events.iter().any(
            |event| matches!(event, Outcome::DamageDealt { target: hit, .. } if *hit == target)
        ),
        "a mature host-confirmed lock must pass the unchanged fire gate"
    );
    assert!(
        !fired.events.iter().any(|event| matches!(
            event,
            Outcome::ShotRefused {
                result: orrery_games::regolith::order::ShotResult::NoLock,
                ..
            }
        )),
        "the fully drawn, host-confirmed lock must not refuse NoLock"
    );
    observe_skin_effects(
        &fired.events,
        &fired.delivered,
        runtime.entity(),
        &[(
            target,
            match runtime.executor().state(target) {
                Some(RegolithState::Craft(craft)) => craft.pos,
                _ => panic!("the campaign target must have replicated state"),
            },
        )],
        &mut tracks,
        &mut broken,
        &mut shots,
    );
    assert_eq!(
        tracks.tracks().len(),
        1,
        "the human fire tick's DamageDealt must leave a muzzle tracer in the skin"
    );
    assert_eq!(tracks.tracks()[0].travelled(), 0.0);
    assert!(
        tracks.tracks()[0].presented,
        "the live campaign muzzle must arm a presentation flight"
    );

    let deadline = Instant::now() + Duration::from_secs(15);
    let mut saw_drawable_flight = false;
    while !matches!(shots.cue, Some(ShotCue::Resolved { .. })) && Instant::now() < deadline {
        let report = runtime.advance(
            Controls {
                lock_target: Some(target),
                ..Controls::default()
            },
            &mut sink,
        );
        observe_skin_effects(
            &report.events,
            &report.delivered,
            runtime.entity(),
            &[],
            &mut tracks,
            &mut broken,
            &mut shots,
        );
        saw_drawable_flight |= tracks
            .tracks()
            .iter()
            .any(|track| track.presented && track.travelled() > 0.0);
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(
        saw_drawable_flight,
        "a campaign muzzle must advance visibly without target-owned continuations"
    );
    let truth = fixture.truth();
    assert!(
        truth.lock_confirmations_sent > 0,
        "the target authored the lock reply"
    );
    assert!(
        truth.inbound_records_verified > 0,
        "the host-authored reply entered the signed witness log as an inbound event"
    );
    assert!(
        truth.damage_inputs_applied > 0,
        "the target authority received the shot"
    );
    assert!(
        truth.shots_resolved > 0,
        "the target authority resolved the shot"
    );
    assert!(
        matches!(shots.cue, Some(ShotCue::Resolved { target: hit, .. }) if hit == target),
        "the delivered target verdict must reach the skin's damage readout"
    );
    assert!(
        !matches!(
            shots.cue,
            Some(ShotCue::Resolved {
                result: orrery_games::regolith::order::ShotResult::OutOfArc,
                ..
            })
        ),
        "an in-arc campaign shot must not be refused by the target"
    );
    assert!(
        !shots.banner().is_empty(),
        "the authoritative shot result must remain visible to the player"
    );
}

#[test]
fn a_delivered_campaign_lock_break_reaches_both_skin_consumers() {
    let fixture = HostFixture::spawn(Mode::TargetDestroyedBeforeDamage);
    let mut sink = sink_for("regolith-campaign-delivered-lock-break");
    let mut runtime = CampaignRuntime::launch(fixture.config("delivered-lock-break"), crate_seed());
    drive_until_joined(&mut runtime, &mut sink, 1);

    let target = PersistId::new(1);
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        let _ = runtime.advance(
            Controls {
                lock_target: Some(target),
                ..Controls::default()
            },
            &mut sink,
        );
        if matches!(
            runtime.executor().state(runtime.entity()),
            Some(RegolithState::Craft(craft))
                if craft.lock_target == Some(target)
                    && craft.lock_class.is_some()
                    && craft.lock_progress >= orrery_games::regolith::LOCK_ACQUISITION_TICKS
        ) {
            break;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(
        matches!(
            runtime.executor().state(runtime.entity()),
            Some(RegolithState::Craft(craft))
                if craft.lock_target == Some(target)
                    && craft.lock_class.is_some()
                    && craft.lock_progress >= orrery_games::regolith::LOCK_ACQUISITION_TICKS
        ),
        "the live target must confirm the lock before the break scenario fires"
    );

    let mut tracks = ProjectileTracks::default();
    let mut broken = LockBreak::default();
    let mut shots = ShotFeedback::default();
    let fired = runtime.advance(
        Controls {
            fire: true,
            lock_target: Some(target),
            ..Controls::default()
        },
        &mut sink,
    );
    observe_skin_effects(
        &fired.events,
        &fired.delivered,
        runtime.entity(),
        &[],
        &mut tracks,
        &mut broken,
        &mut shots,
    );
    assert!(
        shots.cue.is_none(),
        "an untimed muzzle statement must not invent an arrival"
    );
    shots.cue = Some(ShotCue::Arrival { target });
    shots.ticks_left = orrery_regolith_client::combat::SHOT_CUE_TICKS;

    let deadline = Instant::now() + Duration::from_secs(15);
    while broken.banner().is_empty() && Instant::now() < deadline {
        let report = runtime.advance(
            Controls {
                lock_target: Some(target),
                ..Controls::default()
            },
            &mut sink,
        );
        observe_skin_effects(
            &report.events,
            &report.delivered,
            runtime.entity(),
            &[],
            &mut tracks,
            &mut broken,
            &mut shots,
        );
        std::thread::sleep(Duration::from_millis(2));
    }

    assert!(
        fixture.truth().lock_breaks_sent > 0,
        "the host ruleset must author the delivered break"
    );
    assert_eq!(
        broken.banner(),
        "LOCK BROKEN - TARGET DESTROYED",
        "the delivered reason must reach the visible break indicator"
    );
    assert!(
        shots.cue.is_none(),
        "the same delivered break must cancel the unadjudicated shot cue"
    );
}

#[test]
fn a_client_joins_measures_and_applies_replicated_state() {
    let fixture = HostFixture::spawn(Mode::Join);
    let mut sink = sink_for("regolith-campaign-it");
    let mut runtime = CampaignRuntime::launch(fixture.config("it-session"), crate_seed());

    drive_until_joined(&mut runtime, &mut sink, 150);

    // Let every outstanding datagram reach its settled decision.
    let deadline = Instant::now() + Duration::from_secs(10);
    while runtime.uplink_acks().0 < runtime.uplink_sent() && Instant::now() < deadline {
        let _ = runtime.advance(Controls::default(), &mut sink);
        std::thread::sleep(Duration::from_millis(2));
    }
    let terminal_broadcast = fixture.quiesce();
    let deadline = Instant::now() + Duration::from_secs(10);
    while runtime.downlink_last_tick(0) != Some(terminal_broadcast * STRIDE)
        && Instant::now() < deadline
    {
        let _ = runtime.advance(Controls::default(), &mut sink);
        std::thread::sleep(Duration::from_millis(2));
    }
    // Driving the client to the terminal downlink can produce another uplink;
    // settle that traffic too before taking the fixture's final ledger cut.
    let deadline = Instant::now() + Duration::from_secs(10);
    while runtime.uplink_acks().0 < runtime.uplink_sent() && Instant::now() < deadline {
        let _ = runtime.advance(Controls::default(), &mut sink);
        std::thread::sleep(Duration::from_millis(2));
    }
    let truth = fixture.truth();
    assert_eq!(
        runtime.downlink_last_tick(0),
        Some(terminal_broadcast * STRIDE),
        "downlink quiescence failed: terminal broadcast #{terminal_broadcast} was generated, \
         queued, and stream-flushed; client observed through broadcast #{:?}; sender stages: \
         generated through #{}, queued through #{}, stream-flushed through #{}",
        runtime.downlink_last_tick(0).map(|tick| tick / STRIDE),
        truth.downlink_last_generated,
        truth.downlink_last_queued,
        truth.downlink_last_written,
    );
    assert!(truth.uplink_total() > 10, "the fixture saw real traffic");
    assert!(
        truth.witness_frames_verified > 0,
        "the real client producer shipped frames the host verified"
    );
    assert!(
        truth.witness_claims_verified > 0,
        "the real client producer shipped periodic claims the host verified"
    );
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
    let first_unaccounted_skip = truth
        .downlink_skipped_indices
        .get(missing as usize)
        .map_or_else(|| "none".to_owned(), u64::to_string);
    assert!(arrivals > 10, "replication arrived");
    assert_eq!(
        arrivals + missing,
        truth.downlink_broadcasts + truth.downlink_skipped,
        "downlink conservation failed after broadcast #{terminal_broadcast} reached the client: \
         arrivals={arrivals}, accounted missing={missing}, sender-skipped broadcasts={:?}; \
         first unaccounted stage: sender-skipped broadcast #{first_unaccounted_skip}",
        truth.downlink_skipped_indices,
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
    let fixture = HostFixture::spawn(Mode::Reject);
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
    let fixture = HostFixture::spawn(Mode::WrongSlot);
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
