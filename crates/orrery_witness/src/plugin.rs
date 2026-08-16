//! The Bevy adapter (feature `bevy`).
//!
//! A thin drain over the engine in [`crate::witness`]: it moves bytes between
//! `orrery_net`'s peer lane and [`Witness`], and turns the engine's return
//! values into ECS messages. It holds no detection logic of its own — anything
//! that decides something belongs in the engine, where `orrery_persistd` and a
//! headless harness can reach it too.
//!
//! # What it wires
//!
//! ```text
//! authored frames ──▶ AuthorityLog ──▶ Channel::State ──▶ witness set (≤ 7)
//!  witnessed peers ──▶ Channel::State ──▶ Witness ──▶ signals
//!                                        └─ Gap ──▶ Channel::Control ──▶ authority
//!                                                     └─ frames back ──▶ Witness
//! ```
//!
//! The fan-out is the *witness set*, not the interest set — see [`WitnessSet`].
//! Streaming a log to everyone in an island is what makes the D9 traffic bounded
//! "by construction" untrue: §5.3 costs it at ~20–30 kb/s per link, negligible
//! across seven links and ruinous across thirty-one.
//!
//! # Why gap repair rides the reliable lane
//!
//! Frames and claims ride `Channel::State` with replication, where loss is
//! expected and cheap. A repair that could itself be dropped would turn one lost
//! datagram into a permanent hole in the chain — and an unfillable hole is the
//! one witness input that *is* reportable, so losing repairs would manufacture
//! accusations out of ordinary packet loss (D17 risk 3).

use bevy_app::prelude::*;
use bevy_ecs::prelude::*;
use bytes::Bytes;

use orrery_core::store::AuthorityLog;
use orrery_core::{log::HeadTransition, Ruleset};
use orrery_net::channels::{decode_witness, encode_witness, Channel};
use orrery_net::peer_link::payload_budget;
use orrery_net::{IslandMembership, PeerPacket, SendPacket};
use orrery_protocol::{FrameHead, LogFrame, LogRangeRequest, NodeId, StateClaim, Tick, WitnessMsg};

use crate::witness::{Witness, WitnessSignal};

/// The MTU to size a range response against when none is known.
///
/// Deliberately conservative: overshooting means the response is refused by the
/// peer lane and the gap never fills, while undershooting only costs an extra
/// round trip.
pub const ASSUMED_MTU: usize = 1200;

/// A frame this peer authored, on its way out.
///
/// The game produces these — it owns the `Ruleset` and the tick loop — and this
/// plugin retains and broadcasts them. Transitions come along because the log
/// keeps full heads while the wire carries truncated ones.
#[derive(Debug, Clone, Message)]
pub struct PublishFrame {
    /// The signed frame.
    pub frame: LogFrame,
    /// Full head transitions the frame's signature commits to.
    pub transitions: Vec<HeadTransition>,
}

/// A claim this peer authored, on its way out.
#[derive(Debug, Clone, Message)]
pub struct PublishClaim {
    /// The signed claim.
    pub claim: StateClaim,
    /// The quantized snapshot it commits to, retained for evidence assembly.
    ///
    /// Kept locally and never sent: a claim is a hash, and the snapshot is what
    /// an adjudicator eventually replays from.
    pub snapshot: Vec<u8>,
}

/// Something the witness engine decided, surfaced to the app.
///
/// Carries the peer it concerns, because a signal is only actionable against
/// the authority it came from.
#[derive(Debug, Clone, Message)]
pub struct Witnessed {
    /// The authority the signal is about.
    pub subject: NodeId,
    /// What the engine returned.
    pub signal: WitnessSignal,
}

/// The local authority's retained log (docs/06 §6).
#[derive(Debug, Default, Resource)]
pub struct AuthoredLog(pub AuthorityLog);

/// The most links a peer streams its log over (docs/03-replication.md §5.3).
///
/// Witness records ride the replication datagrams, but **only on links to
/// cell-epoch witness-set members** — never the whole interest set. The
/// difference is the whole bandwidth argument: §5.3 puts a typical sender at
/// ~20–30 kb/s of witness traffic *per link*, which is negligible across seven
/// and ruinous across thirty-one.
pub const MAX_WITNESS_LINKS: usize = 7;

/// The peers this authority streams its log to.
///
/// # Who chooses this
///
/// Not this peer. D10 requires the witness set to be seeded per cell-epoch by
/// the coordinator and **never self-chosen**, because a cheat that picks its own
/// witnesses picks friendly ones. That seeding is P5's (`orrery_coordinator`
/// witness-set seeding); until it exists this resource is left empty and
/// [`publish_authored`] falls back to the first [`MAX_WITNESS_LINKS`] island
/// peers in NodeId order.
///
/// That fallback is deterministic and bandwidth-correct, and it is **self-chosen
/// — which is only tolerable because P4 files nothing**. Shadow mode is what
/// makes an interim witness set safe; the moment reports carry consequences,
/// this must come from the coordinator.
#[derive(Debug, Default, Resource)]
pub struct WitnessSet {
    /// The peers to stream to. Empty means "fall back".
    pub members: Vec<NodeId>,
}

/// The witness engine, as a resource.
///
/// Generic over the ruleset because re-execution *is* the signal: a witness
/// without the game's rules can check signatures and chains but cannot tell
/// whether an outcome was legal.
#[derive(Resource)]
pub struct WitnessState<R: Ruleset>(pub Witness<R>)
where
    R::CoreState: Send + Sync,
    R::CoreInput: Send + Sync;

/// What the adapter moved, and what it could not.
#[derive(Debug, Default, Clone, Copy, Resource)]
pub struct WitnessLinkCounters {
    /// Frames broadcast to island peers.
    pub frames_published: u64,
    /// Claims broadcast to island peers.
    pub claims_published: u64,
    /// Range requests sent after a detected gap.
    pub repairs_requested: u64,
    /// Range requests answered from the local log.
    pub repairs_served: u64,
    /// Range requests this peer could not answer at all.
    pub repairs_unservable: u64,
    /// Frames delivered by a repair.
    pub repaired_frames: u64,
    /// Inbound payloads that did not decode as a [`WitnessMsg`].
    pub undecodable: u64,
    /// Inbound messages refused by the engine.
    pub refused: u64,
}

/// Streams the verifiable core between island peers and drives a [`Witness`].
///
/// `R` is the game's ruleset. The plugin does not create the witness — the app
/// inserts [`WitnessState`] once it knows the universe seed — so that a peer can
/// join a universe before it has one.
pub struct WitnessPlugin<R: Ruleset> {
    marker: core::marker::PhantomData<fn() -> R>,
}

impl<R: Ruleset> WitnessPlugin<R> {
    /// The adapter for a game whose rules are `R`.
    ///
    /// Tuning lives on the [`Witness`] the app inserts, not here: the engine is
    /// what honours [`crate::WitnessConfig::shadow_mode`], and a second copy on
    /// the plugin could disagree with it.
    #[must_use]
    pub fn new() -> Self {
        Self {
            marker: core::marker::PhantomData,
        }
    }
}

impl<R: Ruleset> Default for WitnessPlugin<R> {
    fn default() -> Self {
        Self::new()
    }
}

impl<R: Ruleset + Send + Sync + 'static> Plugin for WitnessPlugin<R>
where
    R::CoreState: Send + Sync,
    R::CoreInput: Send + Sync,
{
    fn build(&self, app: &mut App) {
        app.init_resource::<AuthoredLog>()
            .init_resource::<WitnessLinkCounters>()
            .init_resource::<WitnessSet>()
            .add_message::<PublishFrame>()
            .add_message::<PublishClaim>()
            .add_message::<Witnessed>()
            .add_message::<RepairRequest>()
            .add_systems(
                Update,
                (
                    publish_authored,
                    ingest_peer_traffic::<R>,
                    // Serving runs after ingest so a request that arrived this
                    // frame is answered in it: a repair that waits a frame per
                    // hop turns a 180-tick refill into seconds of round trips.
                    serve_repair_requests,
                )
                    .chain(),
            );
    }
}

/// Retains what this peer authored and broadcasts it to its island.
///
/// Retention happens whether or not anyone is listening: the log is what makes
/// this peer able to *answer* a dispute, and a peer alone in an island still
/// has to be able to justify the last three seconds.
pub fn publish_authored(
    mut frames: MessageReader<PublishFrame>,
    mut claims: MessageReader<PublishClaim>,
    mut log: ResMut<AuthoredLog>,
    membership: Res<IslandMembership>,
    witnesses: Res<WitnessSet>,
    mut out: MessageWriter<SendPacket>,
    mut counters: ResMut<WitnessLinkCounters>,
) {
    let peers: Vec<NodeId> = witness_links(&witnesses, &membership);

    for published in frames.read() {
        let heads: Vec<FrameHead> = published
            .transitions
            .iter()
            .map(|transition| FrameHead {
                entity: transition.entity,
                prev_head: transition.prev_head,
                head: transition.head,
            })
            .collect();
        log.0
            .record_frame(published.frame.clone(), published.transitions.clone());

        let payload = Bytes::from(encode_witness(&WitnessMsg::Frame {
            frame: published.frame.clone(),
            heads,
        }));
        for peer in &peers {
            counters.frames_published += 1;
            out.write(SendPacket {
                to: *peer,
                channel: Channel::State,
                payload: payload.clone(),
            });
        }
    }

    for published in claims.read() {
        log.0
            .record_claim(published.claim.clone(), published.snapshot.clone());
        let payload = Bytes::from(encode_witness(&WitnessMsg::Claim(published.claim.clone())));
        for peer in &peers {
            counters.claims_published += 1;
            out.write(SendPacket {
                to: *peer,
                channel: Channel::State,
                payload: payload.clone(),
            });
        }
    }
}

/// The links this peer streams its log over.
///
/// The configured [`WitnessSet`] when there is one, otherwise the first
/// [`MAX_WITNESS_LINKS`] island peers in NodeId order — see [`WitnessSet`] for
/// why that fallback is interim and why shadow mode is what makes it safe.
///
/// Sorting matters: island rosters arrive in whatever order the coordinator
/// built them, and an unsorted truncation would silently change which peers
/// witness this one whenever the manifest reordered.
#[must_use]
pub fn witness_links(witnesses: &WitnessSet, membership: &IslandMembership) -> Vec<NodeId> {
    if !witnesses.members.is_empty() {
        return witnesses.members.clone();
    }
    let mut peers: Vec<NodeId> = membership.peer_ids().collect();
    peers.sort_by_key(|node| *node.as_bytes());
    peers.truncate(MAX_WITNESS_LINKS);
    peers
}

/// Feeds inbound frames, claims and repairs into the witness.
pub fn ingest_peer_traffic<R>(
    mut packets: MessageReader<PeerPacket>,
    witness: Option<ResMut<WitnessState<R>>>,
    mut signals: MessageWriter<Witnessed>,
    mut out: MessageWriter<SendPacket>,
    mut counters: ResMut<WitnessLinkCounters>,
    mut requests: MessageWriter<RepairRequest>,
) where
    R: Ruleset + Send + Sync + 'static,
    R::CoreState: Send + Sync,
    R::CoreInput: Send + Sync,
{
    let mut witness = witness;
    for packet in packets.read() {
        // Not witness traffic at all — replication shares this lane. Skipped
        // before postcard sees it, and not counted as undecodable: a
        // replication datagram is not a malformed witness message.
        let Some(message) = decode_witness::<WitnessMsg>(&packet.payload) else {
            continue;
        };

        // Answering a repair needs a *log*, not a witness. Most peers watch
        // nobody and still have to justify their own chain on request, so this
        // is routed before the witness is even looked for — gating it behind
        // one would make every non-witnessing authority look like it was
        // refusing to answer, which is the one witness input that is
        // reportable.
        if let WitnessMsg::RangeRequest(request) = message {
            requests.write(RepairRequest {
                from: packet.from,
                request,
            });
            continue;
        }

        let Some(witness) = witness.as_mut() else {
            // Nothing is being watched here; the rest of the traffic is not
            // this peer's business.
            continue;
        };
        let witness = &mut witness.0;

        match message {
            WitnessMsg::Frame { frame, heads } => {
                deliver_frame(
                    witness,
                    packet.from,
                    &frame,
                    &heads,
                    &mut signals,
                    &mut out,
                    &mut counters,
                );
            }
            WitnessMsg::Claim(claim) => match witness.ingest_claim(&claim) {
                Ok(Some(signal)) => {
                    signals.write(Witnessed {
                        subject: packet.from,
                        signal,
                    });
                }
                Ok(None) => {}
                Err(_) => counters.refused += 1,
            },
            // Handled above, before the witness is consulted.
            WitnessMsg::RangeRequest(_) => unreachable!("routed before this match"),
            WitnessMsg::RangeResponse {
                response,
                heads,
                resume_from,
            } => {
                if response.frames.is_empty() {
                    // The authority cannot serve it. Not an accusation here:
                    // escalation is the app's call, and retention expiry is an
                    // ordinary reason to have nothing.
                    counters.repairs_unservable += 1;
                    continue;
                }
                for frame in &response.frames {
                    counters.repaired_frames += 1;
                    deliver_frame(
                        witness,
                        packet.from,
                        frame,
                        &heads,
                        &mut signals,
                        &mut out,
                        &mut counters,
                    );
                }
                if let Some(resume) = resume_from {
                    // A 180-tick window does not fit one datagram, so the
                    // authority serves what fits and says where to continue.
                    counters.repairs_requested += 1;
                    out.write(SendPacket {
                        to: packet.from,
                        channel: Channel::Control,
                        payload: Bytes::from(encode_witness(&WitnessMsg::RangeRequest(
                            LogRangeRequest {
                                entity: response.entity,
                                chain_epoch: 0,
                                from_tick: resume,
                                to_tick: Tick::new(
                                    resume.0 + orrery_protocol::MAX_ADJUDICATION_TICKS,
                                ),
                            },
                        ))),
                    });
                }
            }
            // A peer is not an adjudicator. Reports go to the cluster, and one
            // arriving here is either a misroute or a peer hoping to be
            // believed — neither is this adapter's business.
            WitnessMsg::Report(_) => counters.refused += 1,
        }
    }
}

/// Ingest one frame and route whatever the engine says about it.
#[allow(clippy::too_many_arguments)]
fn deliver_frame<R: Ruleset + Send + Sync + 'static>(
    witness: &mut Witness<R>,
    subject: NodeId,
    frame: &LogFrame,
    heads: &[FrameHead],
    signals: &mut MessageWriter<Witnessed>,
    out: &mut MessageWriter<SendPacket>,
    counters: &mut WitnessLinkCounters,
) {
    match witness.ingest_wire_frame(frame, heads) {
        Ok(produced) => {
            for signal in produced {
                if let WitnessSignal::Gap(request) = &signal {
                    counters.repairs_requested += 1;
                    out.write(SendPacket {
                        to: subject,
                        channel: Channel::Control,
                        payload: Bytes::from(encode_witness(&WitnessMsg::RangeRequest(
                            request.clone(),
                        ))),
                    });
                }
                signals.write(Witnessed { subject, signal });
            }
        }
        Err(_) => counters.refused += 1,
    }
}

/// A repair this peer has been asked to serve.
#[derive(Debug, Clone, Message)]
pub struct RepairRequest {
    /// Who asked.
    pub from: NodeId,
    /// What they are missing.
    pub request: LogRangeRequest,
}

/// Answers repair requests from the local authority log.
pub fn serve_repair_requests(
    mut requests: MessageReader<RepairRequest>,
    log: Res<AuthoredLog>,
    mut out: MessageWriter<SendPacket>,
    mut counters: ResMut<WitnessLinkCounters>,
) {
    for asked in requests.read() {
        let served = log
            .0
            .serve_range(&asked.request, payload_budget(ASSUMED_MTU));
        if served.response.frames.is_empty() {
            counters.repairs_unservable += 1;
        } else {
            counters.repairs_served += 1;
        }
        // An empty answer is still sent. Silence is indistinguishable from a
        // dropped repair, and a witness that cannot tell "I have nothing" from
        // "you ignored me" would eventually report an honest peer for the
        // second when it was the first.
        out.write(SendPacket {
            to: asked.from,
            channel: Channel::Control,
            payload: Bytes::from(encode_witness(&WitnessMsg::RangeResponse {
                response: served.response,
                heads: served.heads,
                resume_from: served.resume_from,
            })),
        });
    }
}
