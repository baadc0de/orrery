//! The tokio half of the exterior wire: one real iroh connection per external
//! peer, pumped into the queue pairs the synchronous swarm already speaks.
//!
//! # Lane mapping
//!
//! The frame grammar in [`crate::exterior`] is transport-agnostic; this module
//! decides how each lane rides iroh, preserving the semantics the bots get
//! from aeronet's session machinery:
//!
//! All three lanes ride **one long-lived bidirectional stream** — the same one
//! the handshake ran on — as self-delimiting frames, with the lane byte
//! preserving which lane each frame belongs to.
//!
//! # Why the lossy lane rides a reliable wire here, and why that is honest
//!
//! Aeronet gives bots a lossy datagram lane, and making the exterior leg lossy
//! at the *socket* would seem to match it. It would also measure nothing: the
//! impairment the criterion samples is injected inside the host's virtual
//! router, **before** the wire — a packet the router drops never reaches any
//! socket, reliable or not, and the client observes exactly that drop as a gap
//! in its expected sequence. What the wire itself loses additionally would be
//! indistinguishable from router loss anyway; carrying frames reliably only
//! means the two loss sources cannot be confounded. The lane bytes still
//! travel, so upstream behaviour (what sheds, what repairs) is unchanged.
//!
//! # Handshake order
//!
//! The remote opens the first bidirectional stream and drives it: join
//! request, then — when the run witnesses — its tick-zero anchor. The host
//! answers with the accept between the two reads. Only after that does either
//! side start pumping frames, so "the slot exists" and "the slot carries
//! traffic" cannot be reordered.
//!
//! # Liveness
//!
//! Every pump ends by clearing one shared flag; queue senders check it. The
//! swarm's criterion reads that flag at report time, so a mid-run disconnect
//! fails the run rather than quietly banking an hour against a dead link.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc as std_mpsc, Arc};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use bytes::{Bytes, BytesMut};
use iroh::endpoint::{Connection, RecvStream, SendStream};
use iroh::{Endpoint, EndpointAddr, RelayMode};
use orrery_protocol::NodeId;

use crate::exterior::{
    decode_frame, encode_frame, AnchorFrame, Frame, HostLink, JoinReply, JoinRequest, Lane,
    RemoteLink, LINK_QUEUE_DEPTH, MAX_FRAME_BYTES,
};

/// The connection's application protocol. A grammar change bumps this as well
/// as `JoinRequest::VERSION`; both sides must refuse what they do not speak.
pub const EXTERIOR_ALPN: &[u8] = b"orrery/exterior/1";

/// How long any single handshake read may take before the attempt is refused.
const HANDSHAKE_READ_TIMEOUT: Duration = Duration::from_secs(10);

/// Binds an endpoint for the exterior role.
///
/// Relays are disabled: the loopback proof needs no relay, and turning one on
/// is an operator decision about where cohort traffic may travel (#375), not a
/// harness default.
pub async fn bind(secret: iroh::SecretKey) -> Result<Endpoint> {
    let endpoint = iroh::endpoint::Builder::new(iroh::endpoint::presets::N0)
        .alpns(vec![EXTERIOR_ALPN.to_vec()])
        .relay_mode(RelayMode::Disabled)
        .secret_key(secret)
        .bind()
        .await
        .context("bind exterior endpoint")?;
    Ok(endpoint)
}

/// What an operator or test needs to dial the host.
#[derive(Debug, Clone)]
pub struct HostAddress {
    /// Transport identity of the host.
    pub node: NodeId,
    /// Sockets the host is bound on; the first reachable one wins.
    pub direct: Vec<SocketAddr>,
}

impl HostAddress {
    /// The dial address, preferring a given socket when supplied and present.
    #[must_use]
    pub fn to_addr(&self, prefer: Option<SocketAddr>) -> EndpointAddr {
        let socket = prefer.or_else(|| self.direct.first().copied());
        match socket {
            Some(socket) => {
                EndpointAddr::from_parts(self.node, [iroh::TransportAddr::Ip(socket)])
            }
            None => EndpointAddr::from_parts(self.node, []),
        }
    }
}

fn mark_dead(connected: &Arc<AtomicBool>) {
    connected.store(false, Ordering::Relaxed);
}

/// Length-prefix read of one handshake message.
async fn read_message(recv: &mut RecvStream) -> Result<Vec<u8>> {
    let mut header = [0u8; 4];
    tokio::time::timeout(HANDSHAKE_READ_TIMEOUT, recv.read_exact(&mut header))
        .await
        .context("handshake read timed out")?
        .context("handshake closed mid-length")?;
    let len = u32::from_le_bytes(header) as usize;
    if len > MAX_FRAME_BYTES as usize {
        bail!("handshake message exceeds the frame bound");
    }
    let mut body = vec![0u8; len];
    tokio::time::timeout(HANDSHAKE_READ_TIMEOUT, recv.read_exact(&mut body))
        .await
        .context("handshake read timed out")?
        .context("handshake closed mid-message")?;
    Ok(body)
}

async fn write_message(send: &mut SendStream, body: &[u8]) -> Result<()> {
    let len = u32::try_from(body.len()).context("handshake message too long")?;
    send.write_all(&len.to_le_bytes()).await?;
    send.write_all(body).await?;
    Ok(())
}

/// One frame read off a reliable stream: `[lane u8][peer u32][len u32][payload]`.
///
/// `Ok(None)` when the stream ended cleanly at a frame boundary. An error ends
/// the connection, not just this frame: after a desync the boundaries are
/// unknowable, which is what the length bound alone cannot fix.
async fn read_stream_frame(recv: &mut RecvStream) -> Result<Option<Frame>> {
    // A short read here is a clean end or a dead link; either way this pump's
    // work is done and the flag says so.
    let mut header = [0u8; 9];
    if recv.read_exact(&mut header).await.is_err() {
        if std::env::var_os("P1_SWARM_BRIDGE_DEBUG").is_some() {
            eprintln!("bridge[{}]: header read ended the stream", std::process::id());
        }
        return Ok(None);
    }
    if std::env::var_os("P1_SWARM_BRIDGE_DEBUG").is_some() {
        eprintln!("bridge[{}]: got a 9-byte header", std::process::id());
    }
    let Some(lane) = Lane::from_tag(header[0]) else {
        bail!("unknown lane byte on the exterior stream");
    };
    let peer = u32::from_le_bytes(header[1..5].try_into().expect("nine bytes read"));
    let len = u32::from_le_bytes(header[5..9].try_into().expect("nine bytes read")) as usize;
    if len > MAX_FRAME_BYTES as usize {
        bail!("frame length exceeds the bound");
    }
    let mut payload = vec![0u8; len];
    if recv.read_exact(&mut payload).await.is_err() {
        return Ok(None);
    }
    Ok(Some(Frame {
        peer,
        lane,
        payload: Bytes::from(payload),
    }))
}

async fn write_stream_frame(send: &mut SendStream, frame: &Frame) -> Result<()> {
    let mut wire = Vec::with_capacity(9 + frame.payload.len());
    if encode_frame(frame, &mut wire).is_err() {
        bail!("frame exceeds the wire bound");
    }
    send.write_all(&wire).await?;
    Ok(())
}

/// Sends one queued frame over whichever lane it names.
fn encode_to_bytes(frame: &Frame) -> Bytes {
    let mut wire = Vec::with_capacity(9 + frame.payload.len());
    // Infallible for in-range payloads; oversize frames are refused at the
    // queue boundary instead.
    let _ = encode_frame(frame, &mut wire);
    Bytes::from(wire)
}

/// Routes one inbound combat-lane frame: meta goes to its own channel so the
/// swarm can update rosters, everything else is traffic. A meta frame with a
/// body that is not one cell encoding is dropped, not guessed at.
fn route_inbound(
    frame: Frame,
    uplink_tx: &std_mpsc::SyncSender<Frame>,
    meta_tx: Option<&std_mpsc::SyncSender<u64>>,
) {
    match frame.lane {
        Lane::Meta => {
            if let (Some(meta_tx), Ok(raw)) =
                (meta_tx, <[u8; 8]>::try_from(frame.payload.as_ref()))
            {
                let _ = meta_tx.send(u64::from_le_bytes(raw));
            }
        }
        _ => {
            let _ = uplink_tx.send(frame);
        }
    }
}

/// Host side: accepts the exterior peer's connection and runs the handshake.
///
/// Returns the live host queues plus the anchor the peer shipped (`None` on
/// runs without witnessing). Pumps are spawned here, so the caller gets a
/// working link or an error — never a half-wired slot.
pub async fn host_accept(
    endpoint: &Endpoint,
    expected: NodeId,
    index: usize,
    wants_anchor: bool,
) -> Result<(HostLink, Option<AnchorFrame>)> {
    let incoming = endpoint
        .accept()
        .await
        .context("exterior endpoint closed while waiting for the join")?;
    let connection = incoming
        .accept()
        .context("join failed to start")?
        .await
        .context("join handshake failed")?;
    let remote = connection.remote_id();
    if remote != expected {
        bail!("a connection arrived from {remote}, but slot {index} belongs to {expected}");
    }

    // The remote opened the first bidirectional stream; it drives the talk.
    let (mut send, mut recv) = connection
        .accept_bi()
        .await
        .context("no handshake stream arrived")?;
    let request_bytes = read_message(&mut recv).await?;
    let _request =
        JoinRequest::decode(&request_bytes).map_err(|reason| anyhow::anyhow!("{reason}"))?;

    write_message(&mut send, &JoinReply::Accept { index }.encode()).await?;

    let anchor = if wants_anchor {
        Some(
            AnchorFrame::decode(&read_message(&mut recv).await?)
                .map_err(|reason| anyhow::anyhow!("{reason}"))?,
        )
    } else {
        None
    };

    // The same stream now carries the shared/meta lane in both directions.
    let connected = Arc::new(AtomicBool::new(true));
    let (uplink_tx, uplink_rx) = std_mpsc::sync_channel::<Frame>(LINK_QUEUE_DEPTH);
    let (downlink_tx, downlink_rx) = std_mpsc::sync_channel::<Frame>(LINK_QUEUE_DEPTH);
    let (meta_tx, meta_rx) = std_mpsc::sync_channel::<u64>(LINK_QUEUE_DEPTH);
    if std::env::var_os("P1_SWARM_BRIDGE_DEBUG").is_some() {
        eprintln!("bridge[host]: pumps armed");
    }

    pump_ordered_reader_to(Arc::clone(&connected), recv, uplink_tx, Some(meta_tx));
    pump_writer(Arc::clone(&connected), send, downlink_rx);

    // A freshly spawned task has not necessarily reached its first await, and
    // an unpolled datagram reader drops what arrives in that window. Yielding
    // here lets every pump park inside its socket read before the caller can
    // produce traffic (#385's flaky-first-frame lesson).
    tokio::time::sleep(Duration::from_millis(100)).await;

    Ok((
        HostLink {
            uplink: uplink_rx,
            downlink: downlink_tx,
            meta: meta_rx,
            connected,
        },
        anchor,
    ))
}

/// Remote side: dials the host and runs the client half of the handshake.
///
/// Returns the mirror queues. The assigned slot comes back verified against
/// what the caller derived from the seed — a host assigning a different slot
/// is a misconfiguration to refuse, not something to adapt to.
pub async fn remote_join(
    endpoint: &Endpoint,
    address: EndpointAddr,
    request: &JoinRequest,
    index: usize,
    anchor: Option<AnchorFrame>,
) -> Result<RemoteLink> {
    let connection = endpoint
        .connect(address, EXTERIOR_ALPN)
        .await
        .context("dial exterior host")?;

    // The remote opens and drives the handshake stream.
    let (mut send, mut recv) = connection
        .open_bi()
        .await
        .context("open handshake stream")?;
    write_message(&mut send, &request.encode()).await?;
    let reply_bytes = read_message(&mut recv).await?;
    match JoinReply::decode(&reply_bytes).map_err(|reason| anyhow::anyhow!("{reason}"))? {
        JoinReply::Accept { index: assigned } => {
            if assigned != index {
                bail!("the host assigned slot {assigned}; this peer derived {index}");
            }
        }
        JoinReply::Reject { reason } => bail!("the host refused the join: {reason}"),
    }
    if let Some(anchor) = &anchor {
        write_message(&mut send, &anchor.encode()).await?;
    }

    let connected = Arc::new(AtomicBool::new(true));
    let (outbound_tx, outbound_rx) = std_mpsc::sync_channel::<Frame>(LINK_QUEUE_DEPTH);
    if std::env::var_os("P1_SWARM_BRIDGE_DEBUG").is_some() {
        eprintln!("bridge[remote]: pumps armed");
    }
    let (inbound_tx, inbound_rx) = std_mpsc::sync_channel::<Frame>(LINK_QUEUE_DEPTH);

    // Everything arriving is this peer's inbound traffic; nothing arrives on
    // the meta lane because the host never sends it.
    pump_ordered_reader_to(Arc::clone(&connected), recv, inbound_tx, None);
    pump_writer(Arc::clone(&connected), send, outbound_rx);

    Ok(RemoteLink {
        downlink: inbound_rx,
        uplink: outbound_tx,
        connected,
    })
}

/// Frame reader over the ordered stream: everything arriving is routed by
/// lane. A mid-frame end or a bad length ends the connection — after a desync
/// there are no frame boundaries left to find.
fn pump_ordered_reader_to(
    connected: Arc<AtomicBool>,
    mut recv: RecvStream,
    uplink_tx: std_mpsc::SyncSender<Frame>,
    meta_tx: Option<std_mpsc::SyncSender<u64>>,
) {
    let debug = std::env::var_os("P1_SWARM_BRIDGE_DEBUG").is_some();
    let pid = std::process::id();
    tokio::spawn(async move {
        if debug {
            eprintln!("bridge[{}]: ordered reader armed", pid);
        }
        loop {
            match read_stream_frame(&mut recv).await {
                Ok(Some(frame)) => {
                    if debug {
                        eprintln!("bridge[{}]: got lane {:?} peer {}", pid, frame.lane, frame.peer);
                    }
                    route_inbound(frame, &uplink_tx, meta_tx.as_ref());
                }
                Ok(None) | Err(_) => break,
            }
        }
        mark_dead(&connected);
    });
}

/// The writer every side runs: takes its outbound queue and writes each frame
/// onto the stream in whatever order the queue holds.
fn pump_writer(
    connected: Arc<AtomicBool>,
    mut shared_send: SendStream,
    outbound_rx: std::sync::mpsc::Receiver<Frame>,
) {
    let debug = std::env::var_os("P1_SWARM_BRIDGE_DEBUG").is_some();
    let pid = std::process::id();
    tokio::spawn(async move {
        if debug {
            eprintln!("bridge[{}]: writer armed", pid);
        }
        loop {
            let frame = match outbound_rx.recv() {
                Ok(frame) => frame,
                Err(_) => break,
            };
            if debug {
                eprintln!(
                    "bridge[{}]: writing lane {:?} peer {}",
                    pid, frame.lane, frame.peer
                );
            }
            if write_stream_frame(&mut shared_send, &frame).await.is_err() {
                break;
            }
        }
        mark_dead(&connected);
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bot::{bot_key, host_key};
    use crate::exterior::{Frame, Lane, JoinRequest};

    /// The whole bridge over loopback iroh: real endpoints, the real
    /// handshake with an anchor, then frames pushed through both queue pairs.
    /// This is the seam #385's two-process proof rides on; if frames can lose
    /// here they can lose anywhere.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_bridge_carries_frames_both_ways() {
        let slot = 2usize;
        let expected = bot_key(slot).public();

        let host_ep = bind(host_key()).await.expect("host endpoint");
        let remote_ep = bind(bot_key(slot)).await.expect("remote endpoint");
        let socket = host_ep.bound_sockets()[0];
        let address = HostAddress {
            node: host_ep.id(),
            direct: vec![socket],
        };

        // Accept on a task so the dial can proceed concurrently. The
        // endpoint handle comes back out: dropping it closes every
        // connection, which is how a joined slot can go silently deaf.
        let host_task = {
            let host_ep = host_ep.clone();
            tokio::spawn(async move {
                let link_and_anchor = host_accept(&host_ep, expected, slot, true).await;
                (host_ep, link_and_anchor)
            })
        };
        let anchor_frame = AnchorFrame {
            claim_json: br#"{"entity":3}"#.to_vec(),
            state: vec![9, 9],
        };
        let remote_ep_keep = remote_ep.clone();
        let remote_link = remote_join(
            &remote_ep,
            HostAddress { node: address.node, direct: vec![socket] }.to_addr(Some(socket)),
            &JoinRequest {
                client_rev: "test".into(),
            },
            slot,
            Some(anchor_frame),
        )
        .await
        .expect("remote join completes");
        let _keep_endpoint = remote_ep_keep;
        let (_host_ep_back, joined) =
            host_task.await.expect("host task");
        let (host_link, anchor) = joined.expect("join ok");
        assert!(anchor.is_some(), "witnessing runs ship their anchor");

        // Uplink: two combat frames and one meta report.
        for peer in [0u32, 1] {
            remote_link
                .uplink
                .send(Frame {
                    peer,
                    lane: Lane::Datagram,
                    payload: bytes::Bytes::from_static(b"state"),
                })
                .expect("outbound queue accepts");
        }
        remote_link
            .uplink
            .send(Frame {
                peer: u32::MAX,
                lane: Lane::Meta,
                payload: bytes::Bytes::from(7u64.to_le_bytes().to_vec()),
            })
            .expect("outbound queue accepts");

        // Downlink: the host queues a frame for the remote.
        host_link
            .downlink
            .send(Frame {
                peer: 0,
                lane: Lane::StreamShared,
                payload: bytes::Bytes::from_static(b"replica"),
            })
            .expect("downlink queue accepts");

        // The pumps need a beat to move everything.
        tokio::time::sleep(Duration::from_secs(1)).await;

        let up1 = host_link.uplink.recv_timeout(Duration::from_secs(5));
        assert!(
            matches!(&up1, Ok(f) if f.lane == Lane::Datagram && f.peer == 0),
            "first uplink frame routed: {up1:?}"
        );
        let meta = host_link.meta.recv_timeout(Duration::from_secs(5));
        assert_eq!(meta.ok(), Some(7u64), "the cell report crossed");
        let down = remote_link
            .downlink
            .recv_timeout(Duration::from_secs(5))
            .expect("a downlink frame arrived at the remote");
        assert_eq!(down.peer, 0);
        assert_eq!(down.lane, Lane::StreamShared);
        assert_eq!(down.payload.as_ref(), b"replica");
    }
}
