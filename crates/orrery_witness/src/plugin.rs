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
use bevy_time::{Real, Time};
use bytes::Bytes;

use orrery_core::store::AuthorityLog;
use orrery_core::{log::HeadTransition, Ruleset};
use orrery_net::budget::{Bandwidth, RateMeter, UploadBudget};
use orrery_net::channels::{decode_witness, encode_witness, Channel};
use orrery_net::peer_link::{control_payload_budget, payload_budget};
use orrery_net::{IslandMembership, PeerPacket, SendPacket, StreamMode};
use orrery_protocol::{
    FrameHead, LogFrame, LogRangeRequest, NodeId, PersistId, StateClaim, Tick, WitnessMsg,
};

use crate::witness::{Witness, WitnessSignal};

/// The MTU to size a range response against when none is known.
///
/// Deliberately conservative: overshooting means the response is refused by the
/// peer lane and the gap never fills, while undershooting only costs an extra
/// round trip.
///
/// Since the control lane became QUIC streams this is only the *floor* — see
/// [`repair_response_budget`], which sizes an answer against what the lane and
/// the budget will actually carry rather than against one packet.
pub const ASSUMED_MTU: usize = 1200;

/// How many bytes one range response may carry.
///
/// # Why this is not the MTU any more
///
/// It used to be. `Channel::Control` was a datagram with a different first byte
/// (`orrery_net`'s peer lane was datagram-only), so an answer had to fit one
/// packet: refilling a one-second hole took roughly twenty exchanges *per
/// witness*, and raising the repair share only put more of them in flight —
/// measured at eight peers, moving the share from 15% to 60% took stalled
/// subjects from 28 to 51. The MTU was the binding constraint, not the budget.
///
/// The lane now rides QUIC streams and has no MTU. What binds instead is the
/// share of the upload budget repairs may spend, so that is what an answer is
/// sized against — `allowance` bytes, floored at the old one-packet budget so
/// this can never serve *less* than it used to, and capped at what the lane
/// will carry.
///
/// The floor also carries a progress guarantee: an answer never exceeds one
/// window's allowance, so a response can always be paid for out of an empty
/// window. Sizing against the lane's full megabyte instead would build
/// responses no window could ever afford, and the queue would never drain.
///
/// The resume-from-here machinery stays either way — a range can still exceed
/// this — but at a 15% share of 1 Mbps it stops being the common path.
#[must_use]
pub fn repair_response_budget(allowance: &UploadBudget) -> usize {
    let per_window =
        usize::try_from(allowance.sustained.bytes_over(allowance.window)).unwrap_or(usize::MAX);
    per_window
        .max(payload_budget(ASSUMED_MTU))
        .min(control_payload_budget())
}

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
    /// Per-tick state hashes this frame's ticks produced, per entity, in tick
    /// order from `frame.first_tick`.
    ///
    /// Retained rather than sent. `EvidenceBundle::claimed_hashes` comes from
    /// here, so an authority that never supplies them can serve repairs but
    /// cannot assemble a bundle to answer for *itself* — every self-authored
    /// window fails `IncompleteHashes`, which is not a state a peer should be
    /// able to reach silently. Empty is allowed for a caller that has not
    /// wired it up yet.
    pub tick_hashes: Vec<(PersistId, Vec<[u8; 32]>)>,
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

/// The share of a peer's ≤ 1 Mbps upload budget the verifiable-core lane may
/// spend (docs/03-replication.md §5.3, §7).
///
/// Twenty percent — 0.2 Mbps — which is the upper end of the figure §5.3 has
/// always carried for this lane (`≈ 0.15–0.2 Mbps`, i.e. ≤ 7 links at 20–30
/// kb/s each). What is new is that it is now *enforced by construction* rather
/// than asserted: [`frame_interval_ticks`] derives the frame cadence that fits
/// inside it, so the lane cannot be the thing that exhausts the budget.
///
/// This is what makes the witness lane unsheddable
/// (`orrery_net::budget::is_sheddable`) coherent. A lane bounded at source may
/// keep what it has already spent; a lane that is not would have to be policed
/// by the backstop, and the backstop is exactly the wrong place — it discovers
/// the overrun after the frame exists, and dropping it there costs a
/// control-lane repair larger than the frame was.
///
/// Expressed in percent rather than as an `f32` so [`frame_interval_ticks`] can
/// be a `const fn` and the cadence can be a `const` in the authoring layer,
/// rather than a runtime value nothing checks.
pub const WITNESS_LANE_SHARE_PCT: u64 = 20;

/// Wire bytes a log frame costs before its per-tick input records.
///
/// Measured on the P1 swarm's reference-ruleset frames: 33 B of `RulesetId` (a
/// 32-byte build digest, repeated because an evidence bundle has to be
/// self-describing for adjudication to route it), a 64-byte ed25519 signature,
/// a 67-byte full [`FrameHead`] pair, ~26 B of framing and tick base, and the
/// 60-byte IP+UDP+QUIC floor from `orrery_net::budget::DATAGRAM_OVERHEAD_BYTES`.
///
/// **This is the whole cadence argument.** Roughly 250 of a 316-byte frame is
/// paid per *frame*, not per tick, so a frame covering ten ticks costs a tenth
/// the fixed overhead per tick of audited timeline that one covering three
/// does. `a_frame_costs_what_the_cadence_arithmetic_assumes` fails if a change
/// to the wire types invalidates it.
pub const FRAME_FIXED_WIRE_BYTES: u64 = 250;

/// Wire bytes each covered tick adds to a frame: one sparse `InputRecord`.
///
/// The only genuinely per-tick part of a frame, and therefore the floor no
/// cadence goes below — at 60 Hz it is 1200 B/s per link on its own.
pub const FRAME_TICK_WIRE_BYTES: u64 = 20;

/// Wire bytes one `StateClaim` costs.
///
/// Claims are deliberately *not* on the cadence dial. A claim is the re-anchor
/// point a witness restarts from after a hole, so stretching that interval
/// lengthens exactly the window in which a witness is shown timeline it cannot
/// judge — the coverage number P1 measures. At 2 Hz they are ~4 kb/s per link,
/// about a sixth of the lane; there is nothing here worth buying with coverage.
pub const CLAIM_WIRE_BYTES: u64 = 261;

/// How many ticks one log frame should cover, to keep the lane inside
/// [`WITNESS_LANE_SHARE_PCT`].
///
/// # The arithmetic
///
/// A peer streaming to `links` witnesses spends, per link and per second:
///
/// ```text
/// (tick_hz / n) * FRAME_FIXED_WIRE_BYTES          frames
/// + tick_hz * FRAME_TICK_WIRE_BYTES               the input records in them
/// + (tick_hz / claim_every) * CLAIM_WIRE_BYTES    claims
/// ```
///
/// Only the first term depends on the cadence `n`, so the answer is the
/// smallest `n` whose frame term fits in what the share leaves after the other
/// two are paid. At the D16 defaults — 1 Mbps, 7 links, 60 Hz sim, 2 Hz claims
/// — that is `n >= 8.1`, which the alignment rule below rounds to **10 ticks, a
/// 6 Hz frame cadence**.
///
/// # Why it may be slower than the 20 Hz send rate
///
/// docs/07-witnessing.md §3 arms an audit only on a violation *sustained past
/// the 250 ms window* — a single spike is packet loss, not a cheat. A frame
/// every 167 ms therefore lands strictly before the earliest tick at which any
/// signal it could carry becomes actionable, and the 3 s adjudication window is
/// twenty times longer again. What one-frame-per-send bought was never earlier
/// detection; it was 250 bytes of fixed cost every 50 ms.
///
/// # Why it aligns to the claim interval
///
/// The rounded-up `n` is raised to the next divisor of `claim_every`, so frame
/// boundaries fall on claim ticks. §3 requires an adjudication window to *end
/// at a claim tick*; a cadence coprime with the claim interval puts most claims
/// mid-frame, where a witness holding a partial fold has nothing to compare
/// against and defers instead of judging.
///
/// Returns at least 1: a link count or budget leaving no room for frames at all
/// still has to name a cadence, and the caller's own budget check is what
/// should object, not an interval of zero ticks.
#[must_use]
pub const fn frame_interval_ticks(
    budget_bits_per_sec: u64,
    links: usize,
    tick_hz: u64,
    claim_every: u64,
) -> u16 {
    let links = if links == 0 { 1 } else { links as u64 };
    let per_link = (budget_bits_per_sec / 8) * WITNESS_LANE_SHARE_PCT / 100 / links;
    let records = tick_hz * FRAME_TICK_WIRE_BYTES;
    let claims = match (tick_hz * CLAIM_WIRE_BYTES).checked_div(claim_every) {
        Some(bytes) => bytes,
        None => 0,
    };
    let longest = if claim_every == 0 { 1 } else { claim_every };
    let Some(for_frames) = per_link.checked_sub(records + claims) else {
        // The per-tick records alone do not fit. No cadence recovers that — it
        // is a link-count or a share question — so name the longest interval
        // the claim alignment allows and leave the objection to the caller.
        return longest as u16;
    };
    if for_frames == 0 {
        return longest as u16;
    }
    let ticks = {
        let exact = tick_hz * FRAME_FIXED_WIRE_BYTES;
        let ceil = exact.div_ceil(for_frames);
        if ceil == 0 {
            1
        } else {
            ceil
        }
    };
    if claim_every == 0 {
        return ticks as u16;
    }
    // Raise to the next divisor of the claim interval.
    let mut n = ticks;
    while n <= claim_every {
        if claim_every.is_multiple_of(n) {
            return n as u16;
        }
        n += 1;
    }
    claim_every as u16
}

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

/// The simulation tick the app is on, for repair timeouts.
///
/// Every other repair check hangs off a frame arriving, which only closes
/// "stall forever to stay unjudged" for a subject that keeps sending. A subject
/// that goes quiet has to be timed out against someone else's clock, and the
/// only clock with the right units is the app's own tick — the engine reasons in
/// subject ticks throughout, and wall time would need a conversion that a
/// hitching peer makes wrong exactly when it matters.
///
/// Left at zero the sweep never runs, so a host that does not set this keeps
/// the previous behaviour rather than getting timeouts against a clock that is
/// not advancing.
#[derive(Debug, Clone, Copy, Resource)]
pub struct WitnessClock(pub Tick);

impl Default for WitnessClock {
    fn default() -> Self {
        Self(Tick::new(0))
    }
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
            .init_resource::<RepairBudget>()
            .init_resource::<PendingRepairs>()
            // Declared rather than assumed: serving repairs meters against the
            // peer upload budget and needs a clock, and a host that adds this
            // plugin without the net stack should get working defaults instead
            // of a system that fails parameter validation at runtime.
            .init_resource::<UploadBudget>()
            .init_resource::<WitnessClock>()
            .init_resource::<Time<Real>>()
            .add_message::<PublishFrame>()
            .add_message::<PublishClaim>()
            .add_message::<Witnessed>()
            .add_message::<RepairRequest>()
            .add_systems(
                Update,
                (
                    publish_authored,
                    ingest_peer_traffic::<R>,
                    // After ingest, so a repair that landed this frame has
                    // already closed its hole and is not chased again.
                    sweep_repairs::<R>,
                    // Serving runs after ingest so a request that arrived this
                    // frame is answered in it: a repair that waits a frame per
                    // hop turns a 180-tick refill into seconds of round trips.
                    serve_repair_requests,
                )
                    .chain(),
            );
    }
}

/// Ticks of authored progress between retention sweeps on the local log.
///
/// Matches the witness engine's own cadence, and for the same reason: pruning
/// walks the retained frames, so doing it per frame is quadratic in session
/// length, while never doing it leaves the log growing for the whole session —
/// and `serve_range` scans that deque linearly on every repair, so an unpruned
/// log makes repair-serving cost grow without bound. The budget added in
/// [`RepairBudget`] caps the bandwidth, not the scan.
const AUTHORED_PRUNE_EVERY: u64 = 150;

/// Retains what this peer authored and broadcasts it to its island.
///
/// Retention happens whether or not anyone is listening: the log is what makes
/// this peer able to *answer* a dispute, and a peer alone in an island still
/// has to be able to justify the last three seconds. It is also *bounded* —
/// "the last three seconds", not "everything since launch".
#[allow(clippy::too_many_arguments)]
pub fn publish_authored(
    mut frames: MessageReader<PublishFrame>,
    mut claims: MessageReader<PublishClaim>,
    mut log: ResMut<AuthoredLog>,
    membership: Res<IslandMembership>,
    witnesses: Res<WitnessSet>,
    mut out: MessageWriter<SendPacket>,
    mut counters: ResMut<WitnessLinkCounters>,
    mut last_pruned: Local<Option<u64>>,
) {
    let peers: Vec<NodeId> = witness_links(&witnesses, &membership);
    let mut newest: Option<u64> = None;

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
        for (entity, hashes) in &published.tick_hashes {
            for (offset, hash) in hashes.iter().enumerate() {
                log.0.record_tick_hash(
                    *entity,
                    Tick::new(published.frame.first_tick.0 + offset as u64),
                    *hash,
                );
            }
        }
        let last_tick =
            published.frame.first_tick.0 + u64::from(published.frame.tick_count).saturating_sub(1);
        newest = Some(newest.map_or(last_tick, |held: u64| held.max(last_tick)));

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
                mode: StreamMode::Shared,
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
                mode: StreamMode::Shared,
            });
        }
    }

    if let Some(now) = newest {
        if now
            >= last_pruned
                .unwrap_or(0)
                .saturating_add(AUTHORED_PRUNE_EVERY)
        {
            *last_pruned = Some(now);
            log.0.prune(Tick::new(now));
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
                let produced = witness.ingest_wire_frame(&frame, &heads);
                route(witness, produced, &mut signals, &mut out, &mut counters);
            }
            WitnessMsg::Claim(claim) => match witness.ingest_claim(&claim) {
                Ok(Some(signal)) => {
                    // Attributed to the authority the engine holds responsible,
                    // never to whoever handed the message over — see `route`.
                    let subject = witness.subject(claim.entity).unwrap_or(packet.from);
                    signals.write(Witnessed { subject, signal });
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
                counters.repaired_frames += response.frames.len() as u64;
                // The whole run at once, not frame by frame: the response
                // carries one head pair per entity for the *whole* answer, so
                // only the engine can thread them forward correctly. See
                // `Witness::ingest_wire_frames`.
                let produced = witness.ingest_wire_frames(&response.frames, &heads);
                route(witness, produced, &mut signals, &mut out, &mut counters);

                if let Some(resume) = resume_from {
                    // A 180-tick window does not fit one datagram, so the
                    // authority serves what fits and says where to continue.
                    let Some(subject) = witness.subject(response.entity) else {
                        counters.refused += 1;
                        continue;
                    };
                    counters.repairs_requested += 1;
                    out.write(SendPacket {
                        to: subject,
                        channel: Channel::Control,
                        payload: Bytes::from(encode_witness(&WitnessMsg::RangeRequest(
                            LogRangeRequest {
                                entity: response.entity,
                                // The epoch the gap is actually in. A hardcoded
                                // zero is right only until the first authority
                                // handoff increments one, and then it silently
                                // asks about a chain that no longer exists.
                                chain_epoch: witness.chain_epoch(response.entity).unwrap_or(0),
                                from_tick: resume,
                                to_tick: Tick::new(
                                    resume.0 + orrery_protocol::MAX_ADJUDICATION_TICKS,
                                ),
                            },
                        ))),
                        mode: StreamMode::Shared,
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

/// Chases repairs that have gone unanswered, on the app's own tick.
///
/// A subject that stops sending entirely never trips the frame-driven repair
/// check, because that check needs a frame to run on. Without this a peer can
/// go quiet inside an open hole and stay unjudged indefinitely — the cheap
/// version of the stall the escalation threshold exists to close.
///
/// Runs once per distinct tick: the engine's backoff is measured in ticks, so
/// sweeping several times on the same one would only re-ask questions it has
/// already asked.
pub fn sweep_repairs<R>(
    witness: Option<ResMut<WitnessState<R>>>,
    clock: Res<WitnessClock>,
    mut signals: MessageWriter<Witnessed>,
    mut out: MessageWriter<SendPacket>,
    mut counters: ResMut<WitnessLinkCounters>,
    mut swept: Local<Option<Tick>>,
) where
    R: Ruleset + Send + Sync + 'static,
    R::CoreState: Send + Sync,
    R::CoreInput: Send + Sync,
{
    let Some(mut witness) = witness else {
        return;
    };
    if clock.0 .0 == 0 || *swept == Some(clock.0) {
        return;
    }
    *swept = Some(clock.0);
    let produced = witness.0.sweep(clock.0);
    route(
        &mut witness.0,
        Ok(produced),
        &mut signals,
        &mut out,
        &mut counters,
    );
}

/// Route whatever the engine decided: repairs to the authority, signals to the
/// app.
///
/// # Why not `packet.from`
///
/// The engine verifies every frame against the key of the authority it was
/// asked to watch, so a forged frame cannot enter a chain however it arrives.
/// What arrival order *can* do is open a hole: a peer that replays the
/// subject's own genuine frames out of order makes the chain fail to fold, and
/// if the resulting `LogRangeRequest` went back to whoever delivered the frame,
/// that peer would collect the repair traffic it provoked and simply not answer
/// it — until the witness escalated `Stalled` against an authority that was
/// never asked. Attribution has the same problem in the other direction: a
/// signal stamped with the carrier names the wrong peer in every report and
/// counter downstream.
///
/// So both the address and the attribution come from the engine, which knows
/// which key it holds responsible. The carrier is a delivery detail.
fn route<R: Ruleset + Send + Sync + 'static>(
    witness: &mut Witness<R>,
    produced: Result<Vec<WitnessSignal>, crate::witness::WitnessError>,
    signals: &mut MessageWriter<Witnessed>,
    out: &mut MessageWriter<SendPacket>,
    counters: &mut WitnessLinkCounters,
) {
    let Ok(produced) = produced else {
        counters.refused += 1;
        return;
    };
    for signal in produced {
        let entity = match &signal {
            WitnessSignal::Gap(request) => request.entity,
            WitnessSignal::InvariantBreach { entity, .. }
            | WitnessSignal::ClaimMismatch { entity, .. }
            | WitnessSignal::Stalled { entity, .. } => *entity,
            // Only `raise` produces one, and the app calls that directly. There
            // is no entity to attribute it to here and nothing to route.
            WitnessSignal::Report(_) => continue,
        };
        let Some(subject) = witness.subject(entity) else {
            // A signal about an entity the engine no longer holds has nobody to
            // attribute it to, and guessing is exactly what this function
            // exists to avoid.
            counters.refused += 1;
            continue;
        };
        if let WitnessSignal::Gap(request) = &signal {
            counters.repairs_requested += 1;
            out.write(SendPacket {
                to: subject,
                channel: Channel::Control,
                payload: Bytes::from(encode_witness(&WitnessMsg::RangeRequest(request.clone()))),
                // The shared stream: a request is one packet and ordering is
                // free. Only the *response* is bulk enough to want a stream of
                // its own — see `serve_repair_requests`.
                mode: StreamMode::Shared,
            });
        }
        signals.write(Witnessed { subject, signal });
    }
}

/// The share of a peer's upload budget that serving repairs may spend.
///
/// Repairs ride the reliable lane and are never shed — a dropped repair turns
/// one lost datagram into a permanent hole. But "never shed" is not "never
/// bounded": measured at sixteen peers with a stalling quarter, unbounded
/// repair serving reached 8.7 Mbps against a 1 Mbps budget and shed 26 630
/// *replication* packets to pay for it. One peer's hitch became everyone's
/// problem, which is backwards.
///
/// Metering and queueing instead preserves the guarantee the design rests on —
/// a queued repair still arrives, just later — while capping what one stalling
/// peer costs its island. The requester side already tolerates the delay: it
/// holds a single outstanding repair with a backoff and escalates to
/// [`crate::WitnessSignal::Stalled`] only after several attempts.
///
/// Fifteen percent is a starting point, and raising it does **not** help:
/// measured at 8 peers, moving the share to 60% took stalled subjects from 28
/// to 51 and detected gaps from 328 to 726. More repair bandwidth puts more
/// repairs in flight, which crowds the state lane, which opens more holes. The
/// path amplifies under load, so the answer is fewer round trips (a stream
/// lane), not a larger slice. docs/03-replication.md §9.3 reserves twenty
/// percent for the proxy floor and this sits beside it.
#[derive(Debug, Clone, Copy, Resource)]
pub struct RepairBudget {
    /// Fraction of the peer upload budget repairs may spend.
    pub share: f32,
    /// Most requests held while waiting for budget.
    pub queue_limit: usize,
    /// Most requests any one peer may hold in that queue.
    ///
    /// A global limit alone is not a fairness policy. The queue drops its
    /// oldest entry when full, so without a per-peer cap one peer asking hard
    /// enough evicts every other witness's repair indefinitely — entirely
    /// inside the metered budget, because the cost of *queueing* is not
    /// metered. And since an unfilled hole escalates to
    /// [`crate::WitnessSignal::Stalled`], that turns one noisy peer into false
    /// findings against a third party, which is the failure this whole subsystem
    /// is arranged to avoid.
    ///
    /// Four is enough for a peer serving a multi-datagram refill under the
    /// resume protocol, which holds one request outstanding at a time.
    ///
    /// The loop this breaks is the expensive part. An evicted repair is a hole
    /// that stays open, and a hole that stays open is re-asked — so eviction
    /// manufactures the very traffic that causes the next eviction. Measured
    /// over `p1-swarm --peers 16 --seconds 180 --witness`, with the rest of this
    /// change in place: queue overflows fell from 3920 to 794, chain gaps from
    /// 6576 to 968, and `Stalled` escalations against honest bots — every one of
    /// them a false positive by construction — from 1004 to 92.
    pub per_peer_limit: usize,
}

impl Default for RepairBudget {
    fn default() -> Self {
        Self {
            share: 0.15,
            queue_limit: 64,
            per_peer_limit: 4,
        }
    }
}

/// Repairs accepted but not yet paid for.
#[derive(Debug, Default, Resource)]
pub struct PendingRepairs {
    queue: std::collections::VecDeque<RepairRequest>,
    /// Requests dropped because the queue was full.
    pub overflowed: u64,
    /// Requests waiting on budget right now.
    pub deferred: u64,
}

impl PendingRepairs {
    /// Requests queued on behalf of one peer.
    ///
    /// The number the per-peer cap exists to bound. A single total is not
    /// enough to tell a busy island from one peer crowding everyone out, and
    /// those want opposite responses.
    #[must_use]
    pub fn queued_for(&self, peer: NodeId) -> usize {
        self.queue.iter().filter(|held| held.from == peer).count()
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

/// Answers repair requests from the local authority log, within budget.
///
/// Requests that do not fit this frame's allowance are queued rather than
/// dropped: the reliable lane promises a repair *arrives*, not that it arrives
/// immediately, and the requester holds one outstanding repair on a backoff
/// precisely so it can wait.
///
/// # Why this needs so many round trips
///
/// Each answer is capped at one datagram, so refilling a one-second hole takes
/// roughly twenty exchanges *per witness*, and the same frames are served once
/// per witness. D3 puts control and bulk traffic on reliable **streams**, which
/// have no such cap — but `orrery_net`'s peer lane is datagram-only today, a
/// deferral noted when the witness transport landed. That, rather than this
/// budget, is the binding constraint on repair throughput.
#[allow(clippy::too_many_arguments)]
pub fn serve_repair_requests(
    mut requests: MessageReader<RepairRequest>,
    log: Res<AuthoredLog>,
    upload: Res<UploadBudget>,
    repair: Res<RepairBudget>,
    time: Res<Time<Real>>,
    mut pending: ResMut<PendingRepairs>,
    mut meter: Local<Option<RateMeter>>,
    mut out: MessageWriter<SendPacket>,
    mut counters: ResMut<WitnessLinkCounters>,
) {
    for asked in requests.read() {
        // Drop within the *asking* peer's own share first. A requester that
        // cannot be served is exactly the one that asks repeatedly, and its own
        // backoff has most likely superseded its older requests already — so
        // dropping its oldest costs it nothing. Dropping the queue's oldest
        // instead would make one peer's persistence everyone else's loss.
        let held = pending
            .queue
            .iter()
            .filter(|held| held.from == asked.from)
            .count();
        if held >= repair.per_peer_limit {
            if let Some(index) = pending
                .queue
                .iter()
                .position(|held| held.from == asked.from)
            {
                pending.queue.remove(index);
                pending.overflowed += 1;
            }
        } else if pending.queue.len() >= repair.queue_limit {
            pending.queue.pop_front();
            pending.overflowed += 1;
        }
        pending.queue.push_back(asked.clone());
    }

    let now = time.elapsed();
    let allowance = UploadBudget {
        sustained: Bandwidth::from_bits_per_sec(
            (upload.sustained.bits_per_sec() as f64 * f64::from(repair.share)) as u64,
        ),
        window: upload.window,
    };
    let meter = meter.get_or_insert_with(|| RateMeter::new(allowance.window));

    let response_budget = repair_response_budget(&allowance);

    while let Some(asked) = pending.queue.front().cloned() {
        let served = log.0.serve_range(&asked.request, response_budget);
        // An empty answer is still sent. Silence is indistinguishable from a
        // dropped repair, and a witness that cannot tell "I have nothing" from
        // "you ignored me" would eventually report an honest peer for the
        // second when it was the first.
        let payload = encode_witness(&WitnessMsg::RangeResponse {
            response: served.response.clone(),
            heads: served.heads.clone(),
            resume_from: served.resume_from,
        });

        if meter.would_exceed(now, payload.len() as u64, allowance) {
            // Out of allowance for this window. The request stays queued and is
            // served once budget frees up: the reliable lane promises a repair
            // *arrives*, not that it arrives immediately, and the requester is
            // sitting on a backoff rather than spinning.
            break;
        }
        pending.queue.pop_front();
        meter.record(now, payload.len() as u64);

        if served.response.frames.is_empty() {
            counters.repairs_unservable += 1;
        } else {
            counters.repairs_served += 1;
        }
        out.write(SendPacket {
            to: asked.from,
            channel: Channel::Control,
            // A stream of its own, not the shared control stream — the one
            // place in this crate where the distinction is worth spending.
            //
            // A range response is bulk; the shared stream also carries lease
            // traffic and handoff acks, which are small, latency-critical, and
            // have nothing to do with this peer's hole. `p4-streams-bench`
            // measures both halves of the trade over real QUIC at 3% loss on a
            // 40 ms link, across four seeds: mixing them costs sparse control
            // 2-5x its median and 3-6x its p95, and separating them costs this
            // response's own tail 1.4-2x.
            //
            // Paying the second to avoid the first is a judgement about *this*
            // crate rather than a benchmark result. A repair is already slow on
            // purpose — one outstanding at a time, on a backoff, with judgement
            // deferred while the witness catches up — so a longer tail lands on
            // machinery built to absorb it. A lease operation five times slower
            // has nothing to absorb it. The requests stay on the shared stream,
            // where they are one packet each and ordering is free.
            mode: StreamMode::Bulk,
            payload: Bytes::from(payload),
        });
    }
    pending.deferred = pending.queue.len() as u64;
}
