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
use orrery_protocol::{coord::PeerEntry, NodeId, UniverseSeed};

use crate::bot::{
    bot_key, cell_of, default_cell_edge_m, grid_of, spawn_pose, Bot, BotSpec, TICK_HZ,
};
use crate::bridge::{self, HostAddress};

/// Everything the runner needs: which slot is its (derived), where the host
/// is, and how long to play.
pub struct ExternalRun {
    /// Bots already on the host; this process occupies slot `peers`.
    pub peers: usize,
    /// Simulated seconds — which here are wall-clock seconds by design.
    pub seconds: u64,
    /// The seed both processes share. Derives this slot's transport key, every
    /// sibling's key, the spawn poses and the universe. Slice 3 replaces the
    /// identity half of that with invite-bound material (#375).
    pub seed: u64,
    /// Run the witness pipeline, shipping a tick-zero anchor at join.
    pub witnessing: bool,
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
    let index = run.peers;
    let count = run.peers + 1;
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
        cell_edge_m: default_cell_edge_m(),
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
    let mut siblings = Vec::with_capacity(run.peers);
    let mut index_of = BTreeMap::new();
    let mut links = Vec::with_capacity(run.peers);
    for sibling in 0..run.peers {
        let node = bot_key(sibling).public();
        let (pos, _) = spawn_pose(sibling, count);
        let cell = cell_of(grid_of(&pos, default_cell_edge_m()));
        index_of.insert(node, sibling);
        links.push(node);
        siblings.push(PeerEntry {
            node,
            cells: vec![cell],
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
    let endpoint = rt.block_on(bridge::bind(secret))?;

    let request = crate::exterior::JoinRequest::plain(env!("P1_SWARM_COMMIT").to_owned());
    let remote_link = rt.block_on(bridge::remote_join(
        &endpoint,
        run.host.to_addr(run.direct),
        &request,
        index,
        anchor,
    ))?;
    eprintln!(
        "p1-swarm: external peer joined as slot {index} of {count}, {} simulated seconds",
        run.seconds
    );

    // Real time, because the host paces at wall clock when a peer is attached:
    // each side holding its own metronome keeps them within a tick of each
    // other without any lockstep protocol.
    let ticks = run.seconds * TICK_HZ;
    let send_every = (TICK_HZ / 20).max(1);
    let tick_duration = Duration::from_nanos(1_000_000_000 / TICK_HZ);

    let mut inbound_total = 0usize;
    let mut uplink_sequence = 0u64;
    rt.block_on(async move {
        for tick in 0..ticks {
            let tick_start = Instant::now();

            if run.witnessing {
                // Before the tick runs: a claim commits to pre-step state.
                bot.publish_claim(tick);
            }
            bot.step_core(tick, default_cell_edge_m());
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
                    peer: slot_of(&index_of, to),
                    lane,
                    payload,
                };
                if remote_link.uplink.send(frame).await.is_err() {
                    bail!("the uplink queue closed; the host is gone");
                }
            }
            // Once per simulated second, say where we are now (raw CellId bits).
            if tick % TICK_HZ == TICK_HZ - 1 {
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
                let from_slot = usize::try_from(frame.peer).unwrap_or(usize::MAX);
                if from_slot >= run.peers {
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
                if frame.lane != crate::exterior::Lane::Meta {
                    bot.receive_inbound(bot_key(from_slot).public(), stream, frame.payload);
                }
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
        if std::env::var_os("P1_SWARM_BRIDGE_DEBUG").is_some() {
            eprintln!(
                "bridge[remote]: {} inbound frames over the whole run; goodbye sent",
                inbound_total
            );
        }
        Ok(())
    })
}

fn slot_of(index_of: &BTreeMap<NodeId, usize>, node: NodeId) -> u32 {
    u32::try_from(index_of.get(&node).copied().unwrap_or(usize::MAX)).unwrap_or(u32::MAX)
}
