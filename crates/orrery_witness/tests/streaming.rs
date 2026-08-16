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
use orrery_witness::{
    Watch, Witness, WitnessConfig, WitnessPlugin, WitnessSet, WitnessSignal, Witnessed,
};

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
        let mut hashes = Vec::new();

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
            let outcome = self
                .executor
                .step_entity(ENTITY, Tick::new(tick), &inputs)
                .expect("entity present");
            hashes.push(outcome.state_hash);
        }

        let transitions = vec![HeadTransition {
            entity: ENTITY,
            prev_head,
            head: self.head,
        }];
        PublishFrame {
            tick_hashes: vec![(ENTITY, hashes)],
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
            payload: bytes::Bytes::from(orrery_net::channels::encode_witness(
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
fn the_log_goes_to_the_witness_set_not_the_whole_island() {
    // docs/03-replication.md §5.3 bounds witness traffic to the cell-epoch
    // witness set — never the interest set. Streaming to everyone is what makes
    // "bounded by construction" untrue: ~20–30 kb/s per link is negligible
    // across seven and ruinous across thirty-one.
    use orrery_witness::plugin::{witness_links, MAX_WITNESS_LINKS};

    let node = |n: u8| {
        let mut seed = [0u8; 32];
        seed[0] = n;
        iroh_base::SecretKey::from_bytes(&seed).public()
    };
    let crowd: Vec<PeerEntry> = (1..=20u8)
        .map(|n| PeerEntry {
            node: node(n),
            cells: Vec::new(),
        })
        .collect();
    let island = IslandMembership {
        island: Some(IslandId::new(1)),
        epoch: 1,
        peers: crowd,
        regime: TopologyRegime::Mesh,
        source: orrery_net::IslandSource::Coordinator,
    };

    let links = witness_links(&WitnessSet::default(), &island);
    assert_eq!(links.len(), MAX_WITNESS_LINKS, "the fallback is capped");

    // Deterministic: island rosters arrive in whatever order the coordinator
    // built them, and an unsorted truncation would change who witnesses this
    // peer every time the manifest reordered.
    let mut shuffled = island.peers.clone();
    shuffled.reverse();
    let reordered = IslandMembership {
        peers: shuffled,
        ..island.clone()
    };
    assert_eq!(links, witness_links(&WitnessSet::default(), &reordered));

    // A configured set wins outright — that is what the coordinator will seed.
    let seeded = WitnessSet {
        members: vec![node(3), node(9)],
    };
    assert_eq!(
        witness_links(&seeded, &island),
        vec![node(3), node(9)],
        "a seeded witness set is used verbatim, uncapped and unsorted"
    );
}

#[test]
fn a_catching_up_witness_defers_judgement_rather_than_accusing() {
    // The rule the whole gap path rests on: a witness whose own re-execution
    // stopped at a hole has an incomplete trajectory, and comparing a claim
    // against ticks it never ran is exactly how an honest peer that dropped a
    // packet gets accused.
    let (mut authority, mut witness, mut source) = pair();
    let subject = subject_key().public();

    for round in 0..3u64 {
        authority
            .world_mut()
            .resource_mut::<Messages<PublishFrame>>()
            .write(source.send(T0 + round * 3));
        authority.update();
        pump(&mut authority, &mut witness, subject, |_| round == 1);
        witness.update();
    }

    let state = witness.world().resource::<WitnessState<Kinematic>>();
    let catchup = state
        .0
        .catching_up(ENTITY)
        .expect("the witness knows it is behind");
    assert_eq!(catchup.attempts, 1);
    assert!(!catchup.reported, "one hole is not yet a stall");
    assert_eq!(
        state.0.counters().claim_mismatches,
        0,
        "nothing is judged while the trajectory has a hole in it"
    );
}

#[test]
fn a_hole_that_never_closes_escalates_once_and_only_once() {
    // The floor under "defer judgement": without it, stalling forever would be
    // a way to stay unjudged. With it, refusing to answer is itself the
    // finding — and it is a statement about the subject, not about each packet
    // that reveals it.
    let (mut authority, mut witness, mut source) = pair();
    let subject = subject_key().public();

    let mut stalls = 0;
    let mut gaps = 0;
    // The backoff is linear in attempts, so reaching the escalation threshold
    // takes a few hundred ticks of the subject timeline — that patience is the
    // point, not an accident of the test length.
    for round in 0..200u64 {
        authority
            .world_mut()
            .resource_mut::<Messages<PublishFrame>>()
            .write(source.send(T0 + round * 3));
        authority.update();
        // Rounds 1..4 are lost, opening a hole; everything after arrives and
        // fails to chain across it. Repairs are never pumped back, so the hole
        // never closes — the shape of a peer that will not answer.
        pump(&mut authority, &mut witness, subject, |_| {
            (1..4).contains(&round)
        });
        witness.update();
        for signal in signals(&mut witness) {
            match signal.signal {
                WitnessSignal::Gap(_) => gaps += 1,
                WitnessSignal::Stalled { .. } => stalls += 1,
                _ => {}
            }
        }
    }
    assert!(gaps > 0, "the hole is noticed");
    assert_eq!(
        stalls, 1,
        "escalated exactly once for one stuck subject, not once per frame"
    );
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
fn a_repair_goes_to_the_authority_not_to_whoever_relayed_the_frame() {
    // Watching is per subject, and the engine checks every frame against the
    // watched authority's key — so a forged frame cannot enter a chain however
    // it arrives. What arrival *can* do is open a hole: a third party replaying
    // the subject's own genuine frames out of order makes the chain fail to
    // fold. If the resulting range request went back to whoever delivered the
    // frame, that peer would collect the repair traffic it provoked and simply
    // never answer, until the witness escalated `Stalled` against an authority
    // that was never asked. The same mistake in the other direction stamps every
    // signal, report and counter downstream with the wrong peer.
    let (mut authority, mut witness, mut source) = pair();
    let subject = subject_key().public();
    let stranger = iroh_base::SecretKey::from_bytes(&[99; 32]).public();

    authority
        .world_mut()
        .resource_mut::<Messages<PublishFrame>>()
        .write(source.send(T0));
    authority.update();
    pump(&mut authority, &mut witness, subject, |_| false);
    witness.update();
    let _ = signals(&mut witness);
    witness
        .world_mut()
        .resource_mut::<Messages<SendPacket>>()
        .clear();

    // One frame lost, and the next relayed by a stranger — all it takes.
    let _lost = source.send(T0 + 3);
    authority
        .world_mut()
        .resource_mut::<Messages<PublishFrame>>()
        .write(source.send(T0 + 6));
    authority.update();
    pump(&mut authority, &mut witness, stranger, |_| false);
    witness.update();

    let raised = signals(&mut witness);
    assert!(
        raised
            .iter()
            .any(|s| matches!(s.signal, WitnessSignal::Gap(_))),
        "the out-of-order relay opens a hole, got {raised:?}"
    );
    assert!(
        raised.iter().all(|s| s.subject == subject),
        "a signal names the authority it is about, not the carrier: {raised:?}"
    );

    let outbound: Vec<SendPacket> = witness
        .world_mut()
        .resource_mut::<Messages<SendPacket>>()
        .drain()
        .collect();
    assert!(!outbound.is_empty(), "the hole must be asked about");
    assert!(
        outbound.iter().all(|packet| packet.to == subject),
        "a repair addressed to the relayer is one nobody will answer"
    );
}

#[test]
fn the_authored_log_is_bounded_and_carries_the_hashes_a_bundle_needs() {
    // Retention is "the last three seconds", not "everything since launch".
    // Nothing pruned this log, so it grew for the whole session — and
    // `serve_range` scans that deque linearly on every repair, so the cost of
    // answering one grew with it. The repair budget caps the bandwidth; it
    // cannot cap the scan.
    let mut solo = App::new();
    solo.add_plugins(MinimalPlugins)
        .add_message::<PeerPacket>()
        .add_message::<SendPacket>()
        .init_resource::<IslandMembership>()
        .add_plugins(WitnessPlugin::<Kinematic>::new());

    let mut source = Authority::new();
    let _ = source.anchor();
    for round in 0..300u64 {
        solo.world_mut()
            .resource_mut::<Messages<PublishFrame>>()
            .write(source.send(T0 + round * 3));
        solo.update();
    }

    let log = &solo.world().resource::<orrery_witness::AuthoredLog>().0;
    let range = |from: u64, to: u64| orrery_protocol::LogRangeRequest {
        entity: ENTITY,
        chain_epoch: 0,
        from_tick: Tick::new(from),
        to_tick: Tick::new(to),
    };
    assert!(
        log.serve_range(&range(T0, T0 + 3), 64 * 1024)
            .response
            .frames
            .is_empty(),
        "the first frame of a 900-tick session is long past retention"
    );
    let recent = T0 + 297 * 3;
    assert!(
        !log.serve_range(&range(recent, recent + 3), 64 * 1024)
            .response
            .frames
            .is_empty(),
        "...but the window a dispute would actually open on is kept"
    );

    // And the per-tick hashes came with the frames. Without them every window
    // this peer authored fails `IncompleteHashes`, so it can serve everyone
    // else's repairs and still be unable to answer for itself.
    assert!(
        log.claimed_hashes(ENTITY, (Tick::new(recent), Tick::new(recent + 3)))
            .is_some(),
        "an authority has to be able to assemble its own window"
    );
}

#[test]
fn one_peer_asking_hard_cannot_evict_every_other_witnesss_repair() {
    // The queue drops its oldest entry when full, which is right *within* a
    // requester — its own backoff has superseded the older ask. Across
    // requesters it is backwards: queueing is not metered, so one peer asking
    // hard enough pushes every other witness out and keeps them out. An
    // unfilled hole escalates to `Stalled`, so that turns one noisy peer into
    // false findings against a third party.
    use orrery_net::budget::{Bandwidth, UploadBudget};
    use orrery_witness::plugin::RepairRequest;
    use orrery_witness::{PendingRepairs, RepairBudget};

    let mut app = App::new();
    app.add_plugins(MinimalPlugins)
        .add_message::<PeerPacket>()
        .add_message::<SendPacket>()
        .init_resource::<IslandMembership>()
        .add_plugins(WitnessPlugin::<Kinematic>::new());
    // No allowance, so everything queues and nothing drains — this is about who
    // holds the queue, not who gets served.
    app.insert_resource(UploadBudget {
        sustained: Bandwidth::from_bits_per_sec(0),
        ..UploadBudget::default()
    });

    let greedy = iroh_base::SecretKey::from_bytes(&[77; 32]).public();
    let honest = witness_key().public();
    let ask = |from: NodeId, tick: u64| RepairRequest {
        from,
        request: orrery_protocol::LogRangeRequest {
            entity: ENTITY,
            chain_epoch: 0,
            from_tick: Tick::new(tick),
            to_tick: Tick::new(tick + 3),
        },
    };

    {
        let mut requests = app.world_mut().resource_mut::<Messages<RepairRequest>>();
        for tick in 0..200u64 {
            requests.write(ask(greedy, tick));
        }
        requests.write(ask(honest, 1_000));
    }
    app.update();

    let limit = app.world().resource::<RepairBudget>().per_peer_limit;
    let pending = app.world().resource::<PendingRepairs>();
    assert_eq!(
        pending.queued_for(greedy),
        limit,
        "a peer holds its own share and no more"
    );
    assert_eq!(
        pending.queued_for(honest),
        1,
        "and the one honest request is still there to be served"
    );
    assert!(
        pending.overflowed >= 190,
        "the excess was dropped, not queued"
    );
}
