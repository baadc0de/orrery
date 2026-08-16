//! The transport loop: authored frames reach a witness, and a dropped one is
//! repaired rather than accused.
//!
//! Two Bevy apps with a hand-driven link between them, so a packet can be
//! *deliberately* dropped. That is the case the design turns on — logs ride the
//! unreliable lane, and a witness that treated a hole as fabrication would
//! strike honest peers on a lossy link (D17 risk 3). A test that never dropped
//! anything would prove only the happy path, which is not the path the
//! rationale is about.
//!
//! No network here on purpose: PR #8's `coordinator_session.rs` already proves
//! the iroh path. What is unproven is whether the repair loop *closes*, and a
//! real socket makes drops incidental instead of chosen.

use bevy::prelude::*;
use bevy_ecs::message::Messages;

use orrery_core::log::{claim_hash, sign_claim, sign_frame, HeadTransition};
use orrery_core::{
    state_hash, CodecError, CoreCodec, Executor, OrderedInputs, QPos, QVel, Quantized, Ruleset,
    StateView, StepOutput, TickRng,
};
use orrery_net::channels::Channel;
use orrery_net::{IslandMembership, PeerPacket, SendPacket};
use orrery_protocol::coord::{IslandId, PeerEntry, TopologyRegime};
use orrery_protocol::{
    ChainHash, EntitySlice, InputRecord, LogFrame, NodeId, PersistId, RecordSource, RulesetId,
    StateClaim, Tick, UniverseSeed,
};
use orrery_witness::plugin::{PublishFrame, WitnessLinkCounters, WitnessState};
use orrery_witness::{Watch, Witness, WitnessConfig, WitnessPlugin, WitnessSignal, Witnessed};

// ── A ruleset small enough to keep this test about transport ──────────────

#[derive(Debug, Clone, PartialEq, Eq)]
struct Body {
    pos: QPos,
    vel: QVel,
}

impl CoreCodec for Body {
    fn encode(&self, out: &mut Vec<u8>) {
        for value in [
            self.pos.x, self.pos.y, self.pos.z, self.vel.x, self.vel.y, self.vel.z,
        ] {
            out.extend_from_slice(&value.to_le_bytes());
        }
    }
    fn decode(bytes: &[u8]) -> Result<Self, CodecError> {
        if bytes.len() != 48 {
            return Err(CodecError("body is 48 bytes"));
        }
        let read = |i: usize| i64::from_le_bytes(bytes[i * 8..i * 8 + 8].try_into().unwrap());
        Ok(Self {
            pos: QPos {
                x: read(0),
                y: read(1),
                z: read(2),
            },
            vel: QVel {
                x: read(3),
                y: read(4),
                z: read(5),
            },
        })
    }
}

impl Quantized for Body {
    fn quantize(&mut self) {}
}

#[derive(Debug, Clone, Copy)]
struct Move(i64);

impl CoreCodec for Move {
    fn encode(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.0.to_le_bytes());
    }
    fn decode(bytes: &[u8]) -> Result<Self, CodecError> {
        Ok(Self(i64::from_le_bytes(
            bytes.try_into().map_err(|_| CodecError("8 bytes"))?,
        )))
    }
}

struct Nothing;

impl CoreCodec for Nothing {
    fn encode(&self, _out: &mut Vec<u8>) {}
    fn decode(_bytes: &[u8]) -> Result<Self, CodecError> {
        Ok(Self)
    }
}

struct Kinematic;

const RULESET: RulesetId = RulesetId {
    version: 7,
    digest: [0x7E; 32],
};

impl Ruleset for Kinematic {
    type CoreState = Body;
    type CoreInput = Move;
    type CoreEvent = Nothing;

    fn id(&self) -> RulesetId {
        RULESET
    }

    fn step(
        &self,
        view: &mut StateView<'_, Body>,
        inputs: &OrderedInputs<'_, Move>,
        _rng: &mut TickRng,
    ) -> StepOutput<Nothing> {
        let mut requested = 0i64;
        for input in inputs.iter() {
            requested += input.0;
        }
        let state = view.own_mut();
        state.vel.x = requested;
        state.pos.x += state.vel.x;
        StepOutput::default()
    }
}

// ── Fixtures ─────────────────────────────────────────────────────────────

const ENTITY: PersistId = PersistId::new(4_242);
const SEED: UniverseSeed = UniverseSeed([0x77; 32]);
const T0: u64 = 3_000;
const STEP: i64 = 40;

fn subject_key() -> iroh_base::SecretKey {
    iroh_base::SecretKey::from_bytes(&[21; 32])
}

fn witness_key() -> iroh_base::SecretKey {
    iroh_base::SecretKey::from_bytes(&[22; 32])
}

fn body() -> Body {
    Body {
        pos: QPos::default(),
        vel: QVel::default(),
    }
}

/// An honest authority producing three-tick frames.
struct Authority {
    executor: Executor<Kinematic>,
    head: ChainHash,
    previous_claim: [u8; 32],
}

impl Authority {
    fn new() -> Self {
        let mut executor = Executor::new(Kinematic, SEED);
        executor.insert(ENTITY, body());
        Self {
            executor,
            head: ChainHash::EMPTY,
            previous_claim: [0; 32],
        }
    }

    /// The claim a witness anchors its re-execution at.
    fn anchor(&mut self) -> (StateClaim, Body) {
        let state = self.executor.state(ENTITY).expect("seeded").clone();
        let mut claim = StateClaim {
            entity: ENTITY,
            chain_epoch: 0,
            tick: Tick::new(T0),
            input_head: self.head,
            state_hash: state_hash(&state),
            prev_claim: self.previous_claim,
            ruleset: RULESET,
            sig: subject_key().sign(b"unsigned"),
        };
        sign_claim(&subject_key(), &mut claim);
        self.previous_claim = claim_hash(&claim);
        (claim, state)
    }

    fn send(&mut self, first_tick: u64) -> PublishFrame {
        let key = subject_key();
        let prev_head = self.head;
        let mut records = Vec::new();

        for offset in 0..3u64 {
            let tick = first_tick + offset;
            let inputs = vec![Move(STEP)];
            let record = InputRecord {
                tick_off: offset as u16,
                seq: 0,
                source: RecordSource::Player {
                    node: key.public(),
                    input_seq: tick as u32,
                },
                payload: bytes::Bytes::from(inputs[0].to_canonical()),
            };
            self.head = orrery_core::log::fold(self.head, &record);
            records.push(record);
            self.executor
                .step_entity(ENTITY, Tick::new(tick), &inputs)
                .expect("entity present");
        }

        let transitions = vec![HeadTransition {
            entity: ENTITY,
            prev_head,
            head: self.head,
        }];
        PublishFrame {
            frame: LogFrame {
                ruleset: RULESET,
                first_tick: Tick::new(first_tick),
                tick_count: 3,
                entities: vec![EntitySlice {
                    entity: ENTITY,
                    chain_epoch: 0,
                    prev_head: prev_head.rolling(),
                    records,
                    head: self.head.rolling(),
                }],
                sig: sign_frame(&key, RULESET, Tick::new(first_tick), 3, &transitions),
            },
            transitions,
        }
    }
}

// ── The two-app harness ──────────────────────────────────────────────────

/// An app with the witness adapter and the peer-lane messages it drains,
/// standing in for what `OrreryNetPlugin` would provide.
fn app(island_with: NodeId) -> App {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins)
        .add_message::<PeerPacket>()
        .add_message::<SendPacket>()
        .insert_resource(IslandMembership {
            island: Some(IslandId::new(1)),
            epoch: 1,
            peers: vec![PeerEntry {
                node: island_with,
                cells: Vec::new(),
            }],
            regime: TopologyRegime::Mesh,
            source: orrery_net::IslandSource::Coordinator,
        })
        .add_plugins(WitnessPlugin::<Kinematic>::new());
    app
}

/// Move everything `from` sent into `to`'s inbox, minus what `drop_it` refuses.
///
/// Returns how many packets were delivered, so a test can assert that a link it
/// believes is carrying traffic actually is.
fn pump(
    from: &mut App,
    to: &mut App,
    sender: NodeId,
    mut drop_it: impl FnMut(&SendPacket) -> bool,
) -> usize {
    let outbound: Vec<SendPacket> = from
        .world_mut()
        .resource_mut::<Messages<SendPacket>>()
        .drain()
        .collect();
    let mut delivered = 0;
    for packet in outbound {
        if drop_it(&packet) {
            continue;
        }
        delivered += 1;
        to.world_mut()
            .resource_mut::<Messages<PeerPacket>>()
            .write(PeerPacket {
                from: sender,
                channel: packet.channel,
                payload: packet.payload,
            });
    }
    delivered
}

fn signals(app: &mut App) -> Vec<Witnessed> {
    app.world_mut()
        .resource_mut::<Messages<Witnessed>>()
        .drain()
        .collect()
}

fn counters(app: &App) -> WitnessLinkCounters {
    *app.world().resource::<WitnessLinkCounters>()
}

/// An authority app and a witness app already watching it.
fn pair() -> (App, App, Authority) {
    let subject = subject_key().public();
    let observer = witness_key().public();
    let mut authority = Authority::new();
    let (anchor, anchor_state) = authority.anchor();

    let authority_app = app(observer);
    let mut witness_app = app(subject);

    let mut witness = Witness::<Kinematic>::new(WitnessConfig::default(), SEED, || Kinematic);
    witness
        .watch(Watch {
            entity: ENTITY,
            subject,
            anchor,
            anchor_state,
        })
        .expect("watch");
    witness_app.insert_resource(WitnessState(witness));

    (authority_app, witness_app, authority)
}

// ── Tests ────────────────────────────────────────────────────────────────

#[test]
fn authored_frames_reach_a_witness_that_stays_quiet_about_them() {
    // The false-positive baseline for the transport layer: nothing about
    // *shipping* an honest chain may look like a discrepancy.
    let (mut authority, mut witness, mut source) = pair();
    let subject = subject_key().public();

    for round in 0..8 {
        authority
            .world_mut()
            .resource_mut::<Messages<PublishFrame>>()
            .write(source.send(T0 + round * 3));
        authority.update();
        assert_eq!(pump(&mut authority, &mut witness, subject, |_| false), 1);
        witness.update();
        assert!(
            signals(&mut witness).is_empty(),
            "an honest frame produced a signal in round {round}"
        );
    }

    let state = witness.world().resource::<WitnessState<Kinematic>>();
    assert_eq!(state.0.counters().frames_accepted, 8);
    assert_eq!(state.0.counters().gaps_detected, 0);
    assert_eq!(counters(&authority).frames_published, 8);
}

#[test]
fn a_dropped_frame_is_repaired_over_the_control_lane_and_never_accused() {
    // The case the whole design turns on. Logs ride the unreliable lane; a
    // witness that treated a hole as fabrication would strike honest peers on a
    // lossy link, which is D17 risk 3 arriving through the back door.
    let (mut authority, mut witness, mut source) = pair();
    let subject = subject_key().public();
    let observer = witness_key().public();

    // Three honest frames, with the middle one lost in transit.
    for round in 0..3u64 {
        authority
            .world_mut()
            .resource_mut::<Messages<PublishFrame>>()
            .write(source.send(T0 + round * 3));
        authority.update();
        pump(&mut authority, &mut witness, subject, |_| round == 1);
        witness.update();
    }

    // The witness noticed the hole and asked, rather than reporting.
    let raised = signals(&mut witness);
    assert!(
        raised
            .iter()
            .any(|s| matches!(s.signal, WitnessSignal::Gap(_))),
        "a hole must surface as a gap, got {raised:?}"
    );
    assert!(
        !raised
            .iter()
            .any(|s| matches!(s.signal, WitnessSignal::Report(_))),
        "a dropped datagram is not an accusation"
    );
    assert!(counters(&witness).repairs_requested >= 1);

    // The request reaches the authority, which serves it from its retained log.
    let asked = pump(&mut witness, &mut authority, observer, |packet| {
        assert_eq!(
            packet.channel,
            Channel::Control,
            "a repair that could itself be dropped would make the hole permanent"
        );
        false
    });
    assert_eq!(asked, 1, "exactly one repair request went out");
    authority.update();
    assert_eq!(counters(&authority).repairs_served, 1);

    // And the refill lands, closing the chain.
    pump(&mut authority, &mut witness, subject, |_| false);
    witness.update();
    assert!(counters(&witness).repaired_frames >= 1);
    assert!(
        !signals(&mut witness)
            .iter()
            .any(|s| matches!(s.signal, WitnessSignal::Report(_))),
        "the repaired chain is not a discrepancy"
    );
}

#[test]
fn an_authority_that_cannot_serve_a_gap_answers_rather_than_going_silent() {
    // Silence is indistinguishable from a dropped repair. A witness that could
    // not tell "I have nothing" from "you ignored me" would eventually report an
    // honest peer for the second when it was the first — and refusing a repair
    // *is* reportable, which is exactly why the distinction has to survive.
    let subject = subject_key().public();
    let observer = witness_key().public();
    // A real witness, and an authority that has authored nothing at all — so
    // the range is genuinely beyond anything it retains, the shape a request
    // arriving after retention has aged out takes.
    let (_, mut witness, _) = pair();
    let mut authority = app(observer);

    ask_for_range(&mut witness, T0, T0 + 180);
    assert_eq!(pump(&mut witness, &mut authority, observer, |_| false), 1);
    authority.update();
    assert_eq!(
        counters(&authority).repairs_unservable,
        1,
        "nothing retained for that range"
    );

    // The empty answer still goes out — that is the whole point — and the
    // witness records having been told, rather than waiting on a reply that
    // would never come.
    assert_eq!(
        pump(&mut authority, &mut witness, subject, |_| false),
        1,
        "an authority with nothing must still answer"
    );
    witness.update();
    assert_eq!(counters(&witness).repairs_unservable, 1);
}

/// Queue a raw range request from `app`, as a witness that found a hole would.
fn ask_for_range(app: &mut App, from: u64, to: u64) {
    app.world_mut()
        .resource_mut::<Messages<SendPacket>>()
        .write(SendPacket {
            to: subject_key().public(),
            channel: Channel::Control,
            payload: bytes::Bytes::from(orrery_net::channels::encode_datagram(
                &orrery_protocol::WitnessMsg::RangeRequest(orrery_protocol::LogRangeRequest {
                    entity: ENTITY,
                    chain_epoch: 0,
                    from_tick: Tick::new(from),
                    to_tick: Tick::new(to),
                }),
            )),
        });
}

#[test]
fn a_peer_alone_in_its_island_still_retains_what_it_authored() {
    // Retention is not a side effect of broadcasting. A peer with nobody
    // listening still has to be able to justify its last three seconds, because
    // the dispute arrives after the fact.
    let mut solo = App::new();
    solo.add_plugins(MinimalPlugins)
        .add_message::<PeerPacket>()
        .add_message::<SendPacket>()
        .init_resource::<IslandMembership>()
        .add_plugins(WitnessPlugin::<Kinematic>::new());

    let mut source = Authority::new();
    let _ = source.anchor();
    solo.world_mut()
        .resource_mut::<Messages<PublishFrame>>()
        .write(source.send(T0));
    solo.update();

    assert_eq!(
        counters(&solo).frames_published,
        0,
        "nobody to send it to..."
    );
    let request = orrery_protocol::LogRangeRequest {
        entity: ENTITY,
        chain_epoch: 0,
        from_tick: Tick::new(T0),
        to_tick: Tick::new(T0 + 3),
    };
    let served = solo
        .world()
        .resource::<orrery_witness::AuthoredLog>()
        .0
        .serve_range(&request, 64 * 1024);
    assert_eq!(
        served.response.frames.len(),
        1,
        "...but the frame is retained anyway"
    );
}

#[test]
fn a_frame_from_a_peer_the_witness_does_not_watch_is_refused_not_ingested() {
    // Watching is per subject. A frame from anyone else must not enter the
    // chain, however well-formed it is.
    let (mut authority, mut witness, mut source) = pair();
    let stranger = iroh_base::SecretKey::from_bytes(&[99; 32]).public();

    authority
        .world_mut()
        .resource_mut::<Messages<PublishFrame>>()
        .write(source.send(T0));
    authority.update();
    pump(&mut authority, &mut witness, stranger, |_| false);
    witness.update();

    // The frame names a watched entity, so it is ingested and checked against
    // the *watched* subject's key — where it fails, because the sender is not
    // who authored it. What must not happen is silent acceptance.
    let state = witness.world().resource::<WitnessState<Kinematic>>();
    assert_eq!(
        state.0.counters().frames_accepted,
        1,
        "the frame is genuinely signed by the subject it names"
    );
    assert!(
        signals(&mut witness).iter().all(|s| s.subject == stranger),
        "a signal is attributed to whoever handed the frame over"
    );
}
