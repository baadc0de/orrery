//! The external peer's own loop (#385): one [`Bot`] driven at wall clock,
//! talking to the host over the bridge instead of into an in-process router.
//!
//! This is the process a rendered client replaces in #386. It is deliberately
//! the same machinery the swarm runs — same [`Bot`], same pilot, same witness
//! chain — with exactly two deltas, per #320 constraint 3: it steps in real
//! time (a human plays in real time), and its send/receive buffers are bridged
//! to a socket rather than to neighbouring slots in one process.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use orrery_protocol::{coord::PeerEntry, CellId, NodeId, PersistId, UniverseSeed};

use crate::bot::{
    bot_key, campaign_cell_edge_m, cell_of, grid_of, spawn_pose, Bot, BotSpec, TICK_HZ,
};
use crate::bridge::{self, HostAddress};

/// How many consecutive seconds of replicating to nobody are worth a line.
///
/// Ten, not one: a seat between two roster publishes can legitimately have an
/// empty audience for a second when every island-mate is genuinely out of
/// interest range. Ten seconds of it is either #1128 again or a seat that has
/// flown away from the whole island, and both are worth saying out loud while
/// the session is still running.
const EMPTY_AUDIENCE_WARN_S: u64 = 10;

/// Everything the runner needs: which slot is its (derived), where the host
/// is, and how long to play.
pub struct ExternalRun {
    /// Bots already on the host.
    pub peers: usize,
    /// Admission-reserved stable human seat.
    pub slot: usize,
    /// Full seat namespace, including inactive human seats.
    pub island_seats: usize,
    /// Simulated seconds — which here are wall-clock seconds by design.
    pub seconds: u64,
    /// The seed both processes share. Derives this slot's transport key, every
    /// sibling's key, the spawn poses and the universe. Slice 3 replaces the
    /// identity half of that with invite-bound material (#375).
    pub seed: u64,
    /// Run the witness pipeline, shipping a tick-zero anchor at join.
    pub witnessing: bool,
    /// Admission session id, when this is a campaign join.
    pub session_id: Option<String>,
    /// Encoded node-bound session token, when this is a campaign join.
    pub session_token: Option<Vec<u8>>,
    /// The host's address.
    pub host: HostAddress,
    /// A direct socket to prefer over discovery, for proofs without a relay.
    pub direct: Option<SocketAddr>,
}

/// Runs one external peer against the host for the run's duration.
///
/// The tick order mirrors the host's `tick_once` for the single-bot slice of
/// it — claim before step, broadcast on the window's last tick, publish,
/// update, sample — because witnesses re-execute what this logs, and a
/// different order would be a different trajectory.
pub fn run(run: &ExternalRun) -> Result<()> {
    let index = run.slot;
    let count = run.island_seats;
    if index < run.peers || index >= count {
        bail!(
            "external slot {index} is outside human seats {}..{count}",
            run.peers
        );
    }
    let secret = bot_key(index);

    // The same universe derivation Swarm::new does, so both processes step the
    // same world from the same seed byte-for-byte.
    let mut universe = [0u8; 32];
    universe[0..8].copy_from_slice(&run.seed.to_le_bytes());
    let universe_seed = UniverseSeed(universe);

    let mut bot = Bot::new(BotSpec {
        index,
        count,
        seed: universe_seed,
        cell_edge_m: campaign_cell_edge_m(),
        witnessing: run.witnessing,
        cheat: None,
        enforcing: false,
    });
    // The scripted profiles (idle/burst/stall) exist to stress the witness
    // with awkward bots; a live external peer is never scripted to hitch. Slot
    // arithmetic would hand this one Cruise today and Stall after any reorder
    // of `Profile::ALL`, so the behaviour is pinned here instead of hoped for.
    bot.profile = crate::profile::Profile::Cruise;

    // Island formation from derived keys: every sibling's transport identity
    // is a function of the shared seed.
    //
    // **A bootstrap, and nothing more (#1128).** This roster is built from
    // spawn poses, and spawn poses are true for exactly as long as nobody
    // moves. It exists so the first second of the run has an audience at all;
    // from the host's first `refresh_rosters` publish it is replaced wholesale
    // by `IslandRoster` frames on the Meta lane. Before #1128 there was no
    // replacement: this was the roster for the whole session, each sibling
    // pinned to the single cell it spawned in, so the seat's audience emptied
    // the moment it crossed a cell boundary — about sixteen seconds in — and
    // it replicated to nobody for the rest of the run.
    //
    // The coverage is `neighbors27`, not the one cell the seat occupies, for
    // the same reason `Swarm::active_interest_coverage` publishes 27: a peer
    // declares the neighbourhood it wants, not the point it stands on.
    let mut siblings = Vec::with_capacity(run.peers);
    let mut slot_of: BTreeMap<NodeId, usize> = BTreeMap::new();
    let mut node_of: BTreeMap<usize, NodeId> = BTreeMap::new();
    let mut links = Vec::with_capacity(run.peers);
    for sibling in 0..run.peers {
        let node = bot_key(sibling).public();
        let (pos, _) = spawn_pose(sibling, count);
        let cell = cell_of(grid_of(&pos, campaign_cell_edge_m()));
        slot_of.insert(node, sibling);
        node_of.insert(sibling, node);
        links.push(node);
        siblings.push(PeerEntry {
            node,
            cells: cell.neighbors27(),
        });
    }
    for node in &links {
        bot.link(*node, 1_200);
    }
    bot.set_island(siblings);

    // The tick-zero anchor ships at join when witnessing: it is what arms this
    // slot's watchers, and there is no local state for the host to read.
    let anchor = if run.witnessing {
        let state = bot.state();
        let claim = bot
            .chain
            .as_mut()
            .expect("witnessing implies a chain")
            .anchor(0, &state);
        Some(crate::exterior::AnchorFrame {
            claim_json: serde_json::to_vec(&claim)?,
            state: orrery_core::CoreCodec::to_canonical(&state),
        })
    } else {
        None
    };

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("tokio runtime")?;
    let endpoint = rt.block_on(bridge::bind(secret, None))?;

    let request = crate::exterior::JoinRequest {
        client_rev: env!("P1_SWARM_COMMIT").to_owned(),
        session_id: run.session_id.clone(),
        token: run.session_token.clone(),
        slot: Some(index),
    };
    let remote_link = rt.block_on(bridge::remote_join(
        &endpoint,
        run.host.to_addr(run.direct),
        &request,
        index,
        anchor,
    ))?;
    eprintln!(
        "gates/p1-swarm: external peer joined as slot {index} of {count}, {} simulated seconds",
        run.seconds
    );

    // Real time, because the host paces at wall clock when a peer is attached:
    // each side holding its own metronome keeps them within a tick of each
    // other without any lockstep protocol.
    let ticks = run.seconds * TICK_HZ;
    let send_every = (TICK_HZ / 20).max(1);
    let tick_duration = Duration::from_nanos(1_000_000_000 / TICK_HZ);

    // The ring the host accepted this seat into, installed before the first
    // authored frame leaves — the frames published on tick zero must already be
    // addressed to the peers the host armed against this seat (#1130).
    let mut witness_ring_updates = 0u64;
    let mut witness_ring_size = 0usize;
    if run.witnessing {
        match &remote_link.manifest {
            Some(manifest) => {
                witness_ring_size =
                    apply_witness_ring(&mut bot, manifest, &mut slot_of, &mut node_of);
                witness_ring_updates += 1;
                eprintln!(
                    "gates/p1-swarm: external peer slot {index} witnesses host-chosen seats \
                     {:?} ({witness_ring_size} links)",
                    manifest.witness_recipients,
                );
            }
            None => {
                // Unconditional: with no ring the witness plugin falls back to
                // NodeId byte order, which is #1130 exactly. Say so rather than
                // bank an hour whose witnesses were chosen by key material.
                eprintln!(
                    "gates/p1-swarm: external peer slot {index} was accepted without a start \
                     manifest; it has no host-chosen witness ring and will fall back to NodeId \
                     order, which the host does not arm against (#1130)"
                );
            }
        }
    }

    let own_entity = bot.entity();
    let mut inbound_total = 0usize;
    let mut audience_seconds = 0u64;
    let mut empty_audience_seconds = 0u64;
    let mut consecutive_empty_seconds = 0u64;
    let mut meta_frames = 0u64;
    let mut roster_updates = 0u64;
    let mut accepted_from: BTreeMap<usize, u64> = BTreeMap::new();
    let mut uplink_sequence = 0u64;
    rt.block_on(async move {
        for tick in 0..ticks {
            let tick_start = Instant::now();

            if run.witnessing {
                // Before the tick runs: a claim commits to pre-step state.
                bot.publish_claim(tick);
            }
            bot.step_core(tick, campaign_cell_edge_m());
            if tick % send_every == send_every - 1 {
                bot.broadcast_state(tick);
            }
            if run.witnessing {
                bot.publish(tick);
            }
            bot.update();
            bot.sample();

            // Outbound: whatever the send path queued goes onto the wire addressed
            // by recipient slot. The stall profile never applies here — an
            // external peer is always live; a hitch is what impairment models.
            for (to, stream, payload) in bot.drain_outbound() {
                let lane = match stream {
                    None => crate::exterior::Lane::Datagram,
                    Some(aeronet_iroh::stream::StreamMode::Shared) => {
                        crate::exterior::Lane::StreamShared
                    }
                    Some(aeronet_iroh::stream::StreamMode::Bulk) => {
                        crate::exterior::Lane::StreamBulk
                    }
                };
                let payload = if lane == crate::exterior::Lane::Datagram {
                    let sequenced = crate::exterior::UplinkDatagram {
                        sequence: uplink_sequence,
                        payload,
                    };
                    uplink_sequence = uplink_sequence.wrapping_add(1);
                    sequenced.encode()
                } else {
                    payload
                };
                let frame = crate::exterior::Frame {
                    peer: recipient_slot(&slot_of, to),
                    lane,
                    payload,
                };
                if remote_link.uplink.send(frame).await.is_err() {
                    bail!("the uplink queue closed; the host is gone");
                }
            }
            // Once per simulated second, say where we are now (raw CellId bits).
            if tick % TICK_HZ == TICK_HZ - 1 {
                // And, in the same breath, count how many island-mates this
                // seat's craft is actually replicating to.
                //
                // **This is #1128's failing shape, at the only place it is
                // visible.** The audience is chosen here, from this seat's own
                // roster (`Bot::broadcast_state`), so a seat whose roster
                // froze at its spawn cell counts zero for every second after
                // it crosses a boundary — while every host-side number stays
                // healthy, because the host is still routing this seat's
                // repair and heartbeat traffic to everyone. Measured over a
                // 60-second campaign join: 48 of 60 seconds replicating to
                // nobody before the fix, 0 of 60 after.
                let audience = bot
                    .replication_audience_snapshot()
                    .into_iter()
                    .find(|(entity, _)| *entity == own_entity)
                    .map_or(0, |(_, recipients)| recipients.len());
                audience_seconds += 1;
                if audience == 0 {
                    empty_audience_seconds += 1;
                    consecutive_empty_seconds += 1;
                    // Live evidence, rate-limited: a tester's session log says
                    // "you are a ghost" while it is happening, rather than
                    // only in a report nobody reads until afterwards.
                    if consecutive_empty_seconds % EMPTY_AUDIENCE_WARN_S == 0 {
                        eprintln!(
                            "gates/p1-swarm: external peer slot {index} has replicated to nobody \
                             for {consecutive_empty_seconds} seconds; its island roster covers \
                             none of the cell it is in"
                        );
                    }
                } else {
                    consecutive_empty_seconds = 0;
                }
                let cell = bot.cell().context("external craft lost its cell")?;
                let frame = crate::exterior::Frame {
                    peer: u32::MAX,
                    lane: crate::exterior::Lane::Meta,
                    payload: bytes::Bytes::from(cell.to_bits().to_le_bytes().to_vec()),
                };
                if remote_link.uplink.send(frame).await.is_err() {
                    bail!("the uplink queue closed; the host is gone");
                }
            }

            // Inbound: everything the host delivered lands in the linked session
            // named by the sender's slot — the mirror image of the host's deliver.
            while let Ok(frame) = {
                let mut r = remote_link.downlink.lock().expect("downlink lock poisoned");
                r.try_recv()
            } {
                inbound_total += 1;
                // The Meta lane is not addressed by slot: its frames carry
                // `peer: u32::MAX` by construction (`ExteriorSlot::
                // acknowledge_uplink`, `fold_hearsay_contacts`,
                // `publish_live_manifests_for`, `publish_exterior_rosters`).
                // Classifying by lane *before* the slot check is what makes
                // the lane reachable at all: until #1129 the slot filter below
                // ran first and `u32::MAX >= run.peers` discarded every one of
                // them, which left the `Lane::Meta` guard that used to sit
                // eight lines further down as dead code.
                if frame.lane == crate::exterior::Lane::Meta {
                    meta_frames += 1;
                    if let Some(roster) = crate::exterior::IslandRoster::decode(&frame.payload) {
                        roster_updates += 1;
                        apply_roster(&mut bot, &roster, &mut slot_of, &mut node_of);
                        continue;
                    }
                    // Live membership: the host republishes `StartV1` on this
                    // lane whenever the roster changes
                    // (`Swarm::publish_live_manifests_for`), and it carries the
                    // recomputed witness ring. Reinstalling it here is what
                    // keeps the seat's stream and the host's arming in step
                    // across joins and releases, rather than only at accept.
                    //
                    // Discriminated by shape, not by a new tag: the binary
                    // records own `0xa1`-`0xa3` and this one is JSON, so a
                    // leading `{` cannot collide with any of them.
                    if run.witnessing && frame.payload.first() == Some(&b'{') {
                        if let Ok(manifest) =
                            serde_json::from_slice::<crate::exterior::StartManifest>(&frame.payload)
                        {
                            let size =
                                apply_witness_ring(&mut bot, &manifest, &mut slot_of, &mut node_of);
                            if size != witness_ring_size {
                                eprintln!(
                                    "gates/p1-swarm: external peer slot {index} witness ring \
                                     changed to seats {:?} ({size} links) at membership tick {}",
                                    manifest.witness_recipients, manifest.tick,
                                );
                                witness_ring_size = size;
                            }
                            witness_ring_updates += 1;
                        }
                    }
                    continue;
                }
                let from_slot = usize::try_from(frame.peer).unwrap_or(usize::MAX);
                // What this guard is for: `from_slot` names the seat whose
                // transport identity and entity id the ingest below is about
                // to assert, so it must be a seat of this island — and it must
                // not be this seat, whose own state is authored, not ingested.
                // It was written against `run.peers`, the *bot* count, when
                // bots were the only seats that had slots; every human seat is
                // numbered at or above it, so a second human's every frame was
                // dropped here for the whole session (#1129).
                if from_slot >= count || from_slot == index {
                    continue;
                }
                let stream = match frame.lane {
                    crate::exterior::Lane::Datagram | crate::exterior::Lane::Meta => None,
                    crate::exterior::Lane::StreamShared => {
                        Some(aeronet_iroh::stream::StreamMode::Shared)
                    }
                    crate::exterior::Lane::StreamBulk => {
                        Some(aeronet_iroh::stream::StreamMode::Bulk)
                    }
                };
                // The host's roster names each seat's real transport identity;
                // `bot_key` is only right for a seat this process derived, and
                // a live human's key is its own. Falling back to the derived
                // key keeps the bot cohort working before the first roster.
                let from_node = node_of
                    .get(&from_slot)
                    .copied()
                    .unwrap_or_else(|| bot_key(from_slot).public());
                *accepted_from.entry(from_slot).or_insert(0u64) += 1;
                bot.receive_inbound(
                    from_node,
                    PersistId::new(from_slot as u64 + 1),
                    stream,
                    frame.payload,
                );
            }

            let spent = tick_start.elapsed();
            if spent < tick_duration {
                std::thread::sleep(tick_duration - spent);
            }
        }

        if !remote_link
            .connected
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            bail!("the connection dropped before the run ended");
        }
        // Goodbye: one meta frame marking a CLEAN end of run. The host's
        // criterion accepts a disconnect only after this marker; anything else is
        // a mid-run drop and fails the run (#385).
        let goodbye = crate::exterior::Frame {
            peer: u32::MAX,
            lane: crate::exterior::Lane::Meta,
            payload: bytes::Bytes::from([0xFFu8].to_vec()),
        };
        if remote_link.uplink.send(goodbye).await.is_err() {
            bail!("could not send goodbye");
        }
        // Grace period: dropping the runtime right after the goodbye can kill the
        // transport before the frame hits the wire, leaving the host with a
        // disconnect instead of a clean close.
        tokio::time::sleep(Duration::from_millis(200)).await;
        remote_link.close_transport();
        // QUIC is implemented in userspace: give the endpoint driver a turn to
        // put CONNECTION_CLOSE on UDP before this process destroys its runtime.
        tokio::time::sleep(Duration::from_millis(200)).await;
        // Unconditional, and one line: this is the only place the *remote's*
        // own account of a run is written down. #1129 was a client-side drop —
        // the host's report showed every frame delivered, because they were,
        // to a runner that then threw them away. A host report cannot see
        // that; this line can, and the two-human regression reads it.
        eprintln!(
            "gates/p1-swarm: external peer slot {index} accepted {} of {inbound_total} inbound \
             frames from seats {}; {meta_frames} meta frames, {roster_updates} roster updates, \
             {witness_ring_updates} witness-ring updates ({witness_ring_size} links installed), \
             {empty_audience_seconds} of {audience_seconds} seconds replicating to nobody; \
             goodbye sent",
            accepted_from.values().sum::<u64>(),
            accepted_from
                .iter()
                .map(|(slot, frames)| format!("{slot}:{frames}"))
                .collect::<Vec<_>>()
                .join(","),
        );
        Ok(())
    })
}

fn recipient_slot(slot_of: &BTreeMap<NodeId, usize>, node: NodeId) -> u32 {
    u32::try_from(slot_of.get(&node).copied().unwrap_or(usize::MAX)).unwrap_or(u32::MAX)
}

/// The transport identities of this seat's host-chosen witness ring.
///
/// Split from [`apply_witness_ring`] so the resolution — the part that decides
/// *who* is streamed to — can be checked against
/// [`crate::swarm::witness_recipients`] without standing a `Bot` up.
///
/// Order is the manifest's, which is the host's ring order, not the map's:
/// `witness_links` truncates at `MAX_WITNESS_LINKS`, so a reordering here would
/// silently choose a different subset if the ring were ever longer than the
/// link budget.
fn witness_ring_members(manifest: &crate::exterior::StartManifest) -> Vec<NodeId> {
    let node_by_slot: BTreeMap<usize, NodeId> = manifest
        .active
        .iter()
        .filter_map(|seat| {
            seat.node
                .parse::<NodeId>()
                .ok()
                .map(|node| (seat.slot, node))
        })
        .collect();
    manifest
        .witness_recipients
        .iter()
        .filter_map(|slot| node_by_slot.get(slot).copied())
        .collect()
}

/// Install the host's witness ring for this seat, in place of a guess.
///
/// # What the witness set is a sample of, and why the host owns it (#1130)
///
/// A witness set is not "some peers": it is one side of a **reciprocal ring**.
/// `Swarm::seed_witnesses` and `Swarm::refresh_live_witnesses` assign peer *i*
/// the next [`MAX_WITNESS_LINKS`](orrery_witness::plugin::MAX_WITNESS_LINKS)
/// slots around the frozen active-seat ring, and then arm *exactly those* peers
/// as watchers of *i*. The set a peer streams to and the set the host arms
/// against it are therefore the same list, computed once by
/// `swarm::witness_recipients`, and the whole evidentiary value of the
/// arrangement is that they agree: a watch is armed because a stream is coming,
/// and a stream is sent because a watch is waiting.
///
/// The runner sent no such list. Nothing here ever called `set_witness_set`, so
/// `orrery_witness::plugin::witness_links` took its documented fallback — the
/// first seven island peers **in NodeId byte order**, from whatever
/// `IslandMembership` currently held. NodeId order is a function of key
/// material nobody coordinates, and after #1128 that membership is the
/// once-a-second *audience* roster, so the fallback also moved as the seat
/// flew. Neither has any relation to the host's slot ring. At eight seats the
/// two lists overlapped six of seven and looked correct; at the criterion's
/// thirty-two they overlapped **two of seven**, and the other five armed
/// watches were shown nothing for the whole hour.
///
/// So the host's list is the right one, and not merely by fiat: it is the list
/// the host **armed watches from**. A seat that chose its own would be choosing
/// who watches it, which is the one thing a witnessed subject must not do.
///
/// The ring travels in the accept and again in every live `StartV1` membership
/// frame, so it is reinstalled on every membership change for the same reason
/// the host recomputes it there — the ring's members change when the roster
/// does.
fn apply_witness_ring(
    bot: &mut Bot,
    manifest: &crate::exterior::StartManifest,
    slot_of: &mut BTreeMap<NodeId, usize>,
    node_of: &mut BTreeMap<usize, NodeId>,
) -> usize {
    for seat in &manifest.active {
        let Ok(node) = seat.node.parse::<NodeId>() else {
            continue;
        };
        // The manifest is also the authoritative slot/identity map, and the
        // seats it names include humans the audience roster may not cover.
        slot_of.insert(node, seat.slot);
        node_of.insert(seat.slot, node);
    }
    let members = witness_ring_members(manifest);
    if members.len() != manifest.witness_recipients.len() {
        // Loud, because the alternative is a quietly short ring: every slot the
        // host armed a watch from and this seat cannot address is a watch that
        // will be shown nothing, which is the whole of #1130.
        eprintln!(
            "gates/p1-swarm: witness ring names {} seats but only {} resolve to an identity; \
             the unresolved ones are armed host-side and will be shown nothing",
            manifest.witness_recipients.len(),
            members.len(),
        );
    }
    // An empty ring is *not* installed: `witness_links` reads empty as "use the
    // fallback", so writing one back would reinstate the defect rather than
    // clear it. A membership with nobody to witness this seat is a real state
    // (a lone seat), and the fallback over an empty island is also empty.
    let installed = members.len();
    if !members.is_empty() {
        for node in &members {
            bot.link(*node, 1_200);
        }
        bot.set_witness_set(members);
    }
    installed
}

/// Install the coordinator's roster in place of whatever this seat believed.
///
/// The host half of this is `Swarm::refresh_rosters`, which rebuilds every
/// bot's `IslandMembership` once a second from where the seats actually are.
/// A bot's copy is written straight into its world; this seat's lives in
/// another process, so the same fold arrives as an
/// [`IslandRoster`](crate::exterior::IslandRoster) and is written here. There
/// is deliberately no second source of truth: what the host published is what
/// this seat believes, cells included.
///
/// Links first, then the roster, for the reason `refresh_rosters` states in
/// its own body: a seat entered into the membership without a session has its
/// packets discarded against `no_session`, which nothing counts.
fn apply_roster(
    bot: &mut Bot,
    roster: &crate::exterior::IslandRoster,
    slot_of: &mut BTreeMap<NodeId, usize>,
    node_of: &mut BTreeMap<usize, NodeId>,
) {
    let mut peers = Vec::with_capacity(roster.seats.len());
    for seat in &roster.seats {
        let Ok(node) = NodeId::from_bytes(&seat.node) else {
            // A roster entry whose identity does not parse cannot be linked or
            // addressed. Dropping the entry keeps the rest of the fold usable.
            continue;
        };
        let slot = usize::from(seat.slot);
        slot_of.insert(node, slot);
        node_of.insert(slot, node);
        bot.link(node, 1_200);
        peers.push(PeerEntry {
            node,
            cells: seat
                .cells
                .iter()
                .filter_map(|bits| CellId::from_bits(*bits))
                .collect(),
        });
    }
    bot.set_island(peers);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exterior::{ActiveSeat, StartManifest};
    use crate::swarm::witness_recipients;
    use orrery_net::island::IslandMembership;
    use orrery_witness::plugin::{witness_links, WitnessSet, MAX_WITNESS_LINKS};

    /// The criterion's population: thirty-two bots and one human seat.
    const BOTS: usize = 32;
    const HUMAN_SLOT: usize = BOTS;

    fn manifest_for(subject: usize) -> StartManifest {
        let active = (0..=BOTS)
            .map(|slot| ActiveSeat {
                slot,
                node: bot_key(slot).public().to_string(),
                entity: slot as u64 + 1,
            })
            .collect::<Vec<_>>();
        let active_slots = active.iter().map(|seat| seat.slot).collect::<Vec<_>>();
        StartManifest {
            attempt_id: "witness-ring-test".to_owned(),
            seed: 7,
            tick: 0,
            island_seats: u16::try_from(BOTS + 1).expect("the test population fits u16"),
            witness_recipients: witness_recipients(&active_slots, subject),
            active,
            duration_ticks: 60 * TICK_HZ,
        }
    }

    /// The island the seat's witness plugin would fall back over: every other
    /// seat, in whatever order the roster was built.
    fn island_of_everyone_else(subject: usize, seats: usize) -> IslandMembership {
        IslandMembership {
            peers: (0..=seats)
                .filter(|slot| *slot != subject)
                .map(|slot| PeerEntry {
                    node: bot_key(slot).public(),
                    cells: Vec::new(),
                })
                .collect(),
            ..IslandMembership::default()
        }
    }

    fn slots_of(nodes: &[NodeId], seats: usize) -> Vec<usize> {
        let by_node: BTreeMap<NodeId, usize> = (0..=seats)
            .map(|slot| (bot_key(slot).public(), slot))
            .collect();
        nodes
            .iter()
            .map(|node| *by_node.get(node).expect("a seat of this island"))
            .collect()
    }

    /// The defect, stated as an inequality (#1130).
    ///
    /// This is what the old code fails. Nothing gave the runner a
    /// [`WitnessSet`], so `witness_links` took its NodeId-order fallback and
    /// the human seat streamed its authored log to the seven lowest **NodeIds**
    /// of the island, while the host armed the next seven **slots** after it.
    /// Both lists are computed here from the real functions, so the numbers are
    /// the code's rather than the issue's. Measured, at thirty-two bots and one
    /// human seat: the host armed slots `[0, 1, 2, 3, 4, 5, 6]` and the seat
    /// streamed to `[1, 18, 13, 22, 27, 30, 2]` — an overlap of **two**, so
    /// **five of seven armed watches were shown nothing at all**, for the whole
    /// hour, while the report printed `observation_coverage 1.0`.
    #[test]
    fn nodeid_order_and_slot_order_name_different_seats_at_the_criterion_population() {
        let manifest = manifest_for(HUMAN_SLOT);
        let armed = manifest.witness_recipients.clone();
        assert_eq!(
            armed.len(),
            MAX_WITNESS_LINKS,
            "the host arms seven watches against a seat at this population"
        );

        let fallback = witness_links(
            &WitnessSet::default(),
            &island_of_everyone_else(HUMAN_SLOT, BOTS),
        );
        let streamed = slots_of(&fallback, BOTS);

        let overlap = streamed.iter().filter(|slot| armed.contains(slot)).count();
        // An armed slot the seat does not stream to is a watch shown nothing at
        // all — not a degraded watch, a dark one.
        let dark = armed.iter().filter(|slot| !streamed.contains(slot)).count();
        assert_eq!(
            dark + overlap,
            MAX_WITNESS_LINKS,
            "every armed watch is either fed or dark"
        );
        assert!(
            dark >= 4,
            "the NodeId-order fallback left {dark} of {MAX_WITNESS_LINKS} armed watches dark; \
             armed {armed:?}, streamed {streamed:?}"
        );
    }

    /// The fix, stated as an equality.
    ///
    /// The seat streams to exactly the seats the host armed against it — not a
    /// superset, not the same set in another order, and not a set this process
    /// chose. A witnessed subject does not pick its own witnesses.
    #[test]
    fn the_installed_ring_is_exactly_what_the_host_armed() {
        for subject in [HUMAN_SLOT, 0, BOTS / 2] {
            let manifest = manifest_for(subject);
            let installed = slots_of(&witness_ring_members(&manifest), BOTS);
            assert_eq!(
                installed, manifest.witness_recipients,
                "slot {subject}: the ring installed from the manifest must be the host's own \
                 list, in the host's own order"
            );
        }
    }

    /// At eight seats the two orders nearly agree, which is why every fixture
    /// in this gate ran green while the criterion's own population did not.
    ///
    /// Measured: armed `[0, 1, 2, 3, 4, 5, 6]` against streamed
    /// `[1, 2, 0, 3, 5, 6, 7]` — six of seven, one dark. Worth stating exactly,
    /// because eight bots and one human seat is the population
    /// `scripts/p4-campaign-session.sh` documents for a volunteer session: the
    /// defect was never invisible there, only *small*, and one seventh of a
    /// human hour's witnesses is still a seventh.
    ///
    /// The gate's own exterior leg is smaller still and genuinely cannot see
    /// it: `gates/p1-swarm/tests/external_join.rs` runs `--peers 4`, where five
    /// active slots make the ring `min(7, 4) = 4` — every other seat — so both
    /// orders name the same set and the overlap is exact.
    #[test]
    fn the_gates_own_fixture_population_cannot_tell_the_two_orders_apart() {
        let small = 8usize;
        let active_slots = (0..=small).collect::<Vec<_>>();
        let armed = witness_recipients(&active_slots, small);
        let streamed = slots_of(
            &witness_links(
                &WitnessSet::default(),
                &island_of_everyone_else(small, small),
            ),
            small,
        );
        let overlap = streamed.iter().filter(|slot| armed.contains(slot)).count();
        assert!(
            overlap + 1 >= armed.len(),
            "at eight seats the orders were expected to be nearly indistinguishable, which is \
             the reason this defect survived every fixture: armed {armed:?}, streamed \
             {streamed:?}"
        );
    }
}
