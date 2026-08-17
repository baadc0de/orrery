//! Stage 2 → stage 3: a detected mismatch becomes a *filed* report
//! (docs/07-witnessing.md §3).
//!
//! Detection is `detection.rs`'s subject and adjudication is
//! `orrery_persistd`'s. What is proven here is the step between them, which
//! nothing drove before: the adapter arms the audit window and calls
//! [`Witness::raise`] on its own, and the signed result leaves as a
//! [`ReportFiled`] message instead of being dropped on the floor.
//!
//! Three things have to stay distinguishable, and the tests below are arranged
//! around exactly that. A witness files nothing when it is in **shadow mode**
//! (the P4 posture, on by default), when it has **no signing identity** (a host
//! that never wired one up), and when it has **no provable window**. Only the
//! first is a considered decision, and a subsystem that reported all three as
//! "no reports" would make a misconfiguration look like a policy.

use bevy::prelude::*;
use bevy_ecs::message::Messages;

use orrery_core::log::{claim_hash, sign_claim, sign_frame, HeadTransition};
use orrery_core::{
    state_hash, CodecError, CoreCodec, Executor, OrderedInputs, QPos, QVel, Quantized, Ruleset,
    StateView, StepOutput, TickRng,
};
use orrery_net::channels::{encode_witness, Channel};
use orrery_net::{IslandMembership, PeerPacket, SendPacket};
use orrery_protocol::{
    ChainHash, EntitySlice, FrameHead, InputRecord, LogFrame, PersistId, RecordSource, RulesetId,
    StateClaim, Tick, UniverseSeed, Verdict, WitnessMsg,
};
use orrery_witness::plugin::{WitnessLinkCounters, WitnessState};
use orrery_witness::{
    ReportFiled, Watch, Witness, WitnessConfig, WitnessIdentity, WitnessPlugin, WitnessSignal,
    Witnessed,
};

// ── A ruleset with a speed cap it can be caught violating ────────────────
//
// The same shape `detection.rs` uses, and deliberately a copy rather than a
// shared module: that file is the detection lane's, and a fixture two lanes
// edit is a fixture neither owns.

#[derive(Debug, Clone, PartialEq, Eq)]
struct Body {
    pos: QPos,
    vel: QVel,
    entropy: u32,
}

impl CoreCodec for Body {
    fn encode(&self, out: &mut Vec<u8>) {
        for value in [
            self.pos.x, self.pos.y, self.pos.z, self.vel.x, self.vel.y, self.vel.z,
        ] {
            out.extend_from_slice(&value.to_le_bytes());
        }
        out.extend_from_slice(&self.entropy.to_le_bytes());
    }
    fn decode(bytes: &[u8]) -> Result<Self, CodecError> {
        if bytes.len() != 52 {
            return Err(CodecError("body is 52 bytes"));
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
            entropy: u32::from_le_bytes(bytes[48..52].try_into().unwrap()),
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

/// Millimetres per tick an honest body can move: 3 m/s at 60 Hz.
const SPEED_CAP: i64 = 50;

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
        rng: &mut TickRng,
    ) -> StepOutput<Nothing> {
        let mut requested = 0i64;
        for input in inputs.iter() {
            requested += input.0;
        }
        let state = view.own_mut();
        // The rules clamp. A cheat is a *claim* that this clamp did not apply.
        state.vel.x = requested.clamp(-SPEED_CAP, SPEED_CAP);
        state.pos.x += state.vel.x;
        state.entropy = state
            .entropy
            .wrapping_add(rand_chacha::rand_core::RngCore::next_u32(rng));
        StepOutput::default()
    }
}

// ── Fixtures ─────────────────────────────────────────────────────────────

const ENTITY: PersistId = PersistId::new(4_242);
const SEED: UniverseSeed = UniverseSeed([0x77; 32]);
const T0: u64 = 3_000;
const CLAIM_EVERY: u64 = 30;

/// Three-tick frames in one claim interval.
const FRAMES_PER_CLAIM: u64 = CLAIM_EVERY / 3;

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
        entropy: 0,
    }
}

/// One three-tick frame, and the claim that closes a window when one is due.
struct Sent {
    frame: LogFrame,
    heads: Vec<FrameHead>,
    claim: Option<StateClaim>,
}

/// An authority that logs honestly and claims a trajectory faster than its own
/// logged inputs can produce.
struct Authority {
    executor: Executor<Kinematic>,
    head: ChainHash,
    previous_claim: [u8; 32],
    cheat_multiplier: i64,
    cheat_state: Body,
}

impl Authority {
    fn new(cheat_multiplier: i64) -> Self {
        let mut executor = Executor::new(Kinematic, SEED);
        executor.insert(ENTITY, body());
        Self {
            executor,
            head: ChainHash::EMPTY,
            previous_claim: [0; 32],
            cheat_multiplier,
            cheat_state: body(),
        }
    }

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

    fn send(&mut self, first_tick: u64) -> Sent {
        let key = subject_key();
        let prev_head = self.head;
        let mut records = Vec::new();

        for offset in 0..3u64 {
            let tick = first_tick + offset;
            let inputs = vec![Move(SPEED_CAP)];
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

            // The cheat: advance a private trajectory faster than the rules
            // allow, and claim *that*. The log stays honest because it is
            // signed and chained — lying there is caught immediately.
            self.cheat_state.vel.x = SPEED_CAP * self.cheat_multiplier;
            self.cheat_state.pos.x += self.cheat_state.vel.x;
            self.cheat_state.entropy = self.executor.state(ENTITY).expect("present").entropy;
        }

        let transitions = vec![HeadTransition {
            entity: ENTITY,
            prev_head,
            head: self.head,
        }];
        let heads = transitions
            .iter()
            .map(|transition| FrameHead {
                entity: transition.entity,
                prev_head: transition.prev_head,
                head: transition.head,
            })
            .collect();
        let frame = LogFrame {
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
        };

        let next_tick = first_tick + 3;
        let claim = (next_tick - T0).is_multiple_of(CLAIM_EVERY).then(|| {
            let claimed = if self.cheat_multiplier > 1 {
                &self.cheat_state
            } else {
                self.executor.state(ENTITY).expect("entity present")
            };
            let mut claim = StateClaim {
                entity: ENTITY,
                chain_epoch: 0,
                tick: Tick::new(next_tick),
                input_head: self.head,
                state_hash: state_hash(claimed),
                prev_claim: self.previous_claim,
                ruleset: RULESET,
                sig: key.sign(b"unsigned"),
            };
            sign_claim(&key, &mut claim);
            self.previous_claim = claim_hash(&claim);
            claim
        });

        Sent {
            frame,
            heads,
            claim,
        }
    }
}

fn watching(config: WitnessConfig, authority: &mut Authority) -> Witness<Kinematic> {
    let mut witness = Witness::new(config, SEED, || Kinematic);
    let (anchor, anchor_state) = authority.anchor();
    witness
        .watch(Watch {
            entity: ENTITY,
            subject: subject_key().public(),
            anchor,
            anchor_state,
        })
        .expect("the anchor is signed by the subject");
    witness
}

// ── The Bevy harness ─────────────────────────────────────────────────────

/// A witness app already watching the cheating authority.
///
/// The authority side is bytes on the peer lane rather than a second app: the
/// subject of this file is what the *witness* does with a mismatch, and a
/// second `WitnessPlugin` app would only re-prove `streaming.rs`.
fn witness_app(config: WitnessConfig, authority: &mut Authority) -> App {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins)
        .add_message::<PeerPacket>()
        .add_message::<SendPacket>()
        .init_resource::<IslandMembership>()
        .add_plugins(WitnessPlugin::<Kinematic>::new());
    app.insert_resource(WitnessState(watching(config, authority)));
    app
}

/// Deliver `frames` three-tick frames (and any claims among them) to the app.
///
/// Ten is the number the Bevy tests use: it is exactly one claim interval, so
/// exactly one claim is disputed and the counters below are unambiguous.
/// Running longer would file a second report — correctly, since the engine
/// de-duplicates per *disputed claim* and a second claim is a second signed
/// assertion — but would leave every assertion here reading as a rate.
fn deliver(app: &mut App, authority: &mut Authority, frames: u64) {
    for index in 0..frames {
        let sent = authority.send(T0 + index * 3);
        let mut packets = Vec::new();
        packets.push(encode_witness(&WitnessMsg::Frame {
            frame: sent.frame,
            heads: sent.heads,
        }));
        if let Some(claim) = sent.claim {
            packets.push(encode_witness(&WitnessMsg::Claim(claim)));
        }
        let mut inbox = app.world_mut().resource_mut::<Messages<PeerPacket>>();
        for payload in packets {
            inbox.write(PeerPacket {
                from: subject_key().public(),
                channel: Channel::State,
                payload: bytes::Bytes::from(payload),
            });
        }
        app.update();
    }
}

fn filed(app: &mut App) -> Vec<ReportFiled> {
    app.world_mut()
        .resource_mut::<Messages<ReportFiled>>()
        .drain()
        .collect()
}

fn mismatched(app: &mut App) -> bool {
    app.world_mut()
        .resource_mut::<Messages<Witnessed>>()
        .drain()
        .any(|witnessed| matches!(witnessed.signal, WitnessSignal::ClaimMismatch { .. }))
}

fn counters(app: &App) -> WitnessLinkCounters {
    *app.world().resource::<WitnessLinkCounters>()
}

fn enforcing() -> WitnessConfig {
    WitnessConfig {
        shadow_mode: false,
        ..WitnessConfig::default()
    }
}

// ── The tests ────────────────────────────────────────────────────────────

#[test]
fn a_filed_report_confirms_under_independent_adjudication() {
    // The end of the chain the transport exists to complete: a witness files,
    // and a cluster that believes nothing the witness says re-runs the
    // evidence from the bundle alone and reaches the same verdict. Without
    // that, escalation would only be moving bytes.
    let mut authority = Authority::new(3);
    let mut witness = watching(enforcing(), &mut authority);

    let mut at = None;
    for index in 0..20u64 {
        let sent = authority.send(T0 + index * 3);
        witness
            .ingest_frame(&sent.frame, &[])
            .expect("frames are signed and chained");
        if let Some(claim) = sent.claim {
            if let Some(WitnessSignal::ClaimMismatch { at: tick, .. }) =
                witness.ingest_claim(&claim).expect("claim is signed")
            {
                at = Some(tick);
            }
        }
    }
    let at = at.expect("the cheat must be detected");

    let window = witness
        .audit_window(ENTITY, at)
        .expect("the anchor is still the last agreed point");
    let raised = witness
        .raise(&witness_key(), ENTITY, window)
        .expect("the window is servable");
    let WitnessSignal::Report(Some(report)) = raised else {
        panic!("expected a filed report, got {raised:?}");
    };
    assert!(orrery_witness::verify_report(&report).is_ok());

    let mut adjudicator = orrery_persistd::AdjudicationExecutor::new(SEED);
    adjudicator.register(|| Kinematic);
    assert!(
        matches!(adjudicator.adjudicate(&report), Verdict::Confirms { .. }),
        "the cluster must reach the same verdict from the evidence alone"
    );
}

#[test]
fn a_mismatch_files_a_report_when_the_adapter_has_an_identity() {
    // The wire this lane adds. Before it, `raise` had no non-test caller: the
    // adapter surfaced `ClaimMismatch` and discarded every `Report` signal, so
    // the artefact the whole subsystem exists to produce could not leave the
    // process however the host was configured.
    let mut authority = Authority::new(3);
    let mut app = witness_app(enforcing(), &mut authority);
    app.insert_resource(WitnessIdentity(witness_key()));

    deliver(&mut app, &mut authority, FRAMES_PER_CLAIM);

    assert!(mismatched(&mut app), "the cheat is still detected");
    let reports = filed(&mut app);
    assert_eq!(reports.len(), 1, "one divergence, one report");
    assert_eq!(reports[0].subject, subject_key().public());
    assert_eq!(reports[0].report.reporter, witness_key().public());
    assert_eq!(reports[0].report.bundle.entity, ENTITY);
    assert!(
        orrery_witness::verify_report(&reports[0].report).is_ok(),
        "what leaves the adapter is signed by the identity it was given"
    );
    let counters = counters(&app);
    assert_eq!(counters.escalations_filed, 1);
    assert_eq!(counters.escalations_unidentified, 0);
    assert_eq!(counters.escalations_shadowed, 0);
}

#[test]
fn without_an_identity_the_mismatch_is_detected_and_nothing_is_filed() {
    // A host that never inserted a signing key gets exactly what it had
    // before: detection, counters, no accusation. And it is counted under its
    // own name, so "filed nothing" is not silently the same number as shadow
    // mode's deliberate one.
    let mut authority = Authority::new(3);
    let mut app = witness_app(enforcing(), &mut authority);

    deliver(&mut app, &mut authority, FRAMES_PER_CLAIM);

    assert!(mismatched(&mut app), "detection does not depend on filing");
    assert!(
        filed(&mut app).is_empty(),
        "no identity, no signature, nothing to file"
    );
    let counters = counters(&app);
    assert_eq!(counters.escalations_unidentified, 1);
    assert_eq!(counters.escalations_filed, 0);
}

#[test]
fn shadow_mode_escalates_and_still_files_nothing() {
    // The default posture, and the reason this lane is safe to land: the
    // window is assembled and counted, and nothing leaves. D17 risk 3 —
    // enforcement waits on a measured false-positive rate, not on a missing
    // transport.
    let mut authority = Authority::new(3);
    let mut app = witness_app(WitnessConfig::default(), &mut authority);
    app.insert_resource(WitnessIdentity(witness_key()));

    deliver(&mut app, &mut authority, FRAMES_PER_CLAIM);

    assert!(mismatched(&mut app), "shadow mode detects everything");
    assert!(filed(&mut app).is_empty(), "and files nothing");
    let counters = counters(&app);
    assert_eq!(counters.escalations_shadowed, 1);
    assert_eq!(counters.escalations_filed, 0);
}

#[test]
fn an_honest_authority_produces_no_escalation_at_all() {
    // The baseline every accusation rests on. A transport that escalated on an
    // honest peer would be worse than no transport, and this is the assertion
    // that keeps the false-positive budget intact end to end.
    let mut authority = Authority::new(1);
    let mut app = witness_app(enforcing(), &mut authority);
    app.insert_resource(WitnessIdentity(witness_key()));

    deliver(&mut app, &mut authority, FRAMES_PER_CLAIM);

    assert!(
        !mismatched(&mut app),
        "an honest authority mismatches nothing"
    );
    assert!(filed(&mut app).is_empty());
    let counters = counters(&app);
    assert_eq!(counters.escalations_filed, 0);
    assert_eq!(counters.escalations_unservable, 0);
    assert_eq!(counters.escalations_unidentified, 0);
}
