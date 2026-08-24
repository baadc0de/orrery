//! An impaired link the peers cannot get around.
//!
//! The first version of this was a UDP proxy: bind a socket between the two
//! endpoints, drop and delay packets there, and dial the proxy instead of the
//! peer. It does not work, and finding out why is worth recording. iroh probes
//! for a better path and takes it — with no relay and no address lookup it is
//! never *told* the peer's real address, but it learns one anyway and moves.
//! The proxy carried 21 packets out of 1457. Every impairment figure would have
//! been a fiction, and a plausible-looking one.
//!
//! So the impairment goes *inside* the stack instead. [`ImpairedTransport`]
//! implements iroh's [`CustomTransport`], and the endpoints are built with
//! `clear_ip_transports()` — no IP sockets at all. There is then no path to
//! find: every packet either goes through this module's loss and delay model or
//! does not exist. That is a structural guarantee rather than a check that
//! could pass by luck.
//!
//! It is modelled on iroh's own `test_utils::test_transport`, which is the same
//! idea without the impairment.
//!
//! # What this does and does not change
//!
//! QUIC is entirely real: `noq`'s congestion control, loss detection,
//! retransmission, and — the subject here — its stream scheduling. What is
//! replaced is the wire, which becomes an in-process channel with a delay and a
//! seeded coin. Path selection and hole punching are gone, and they have no
//! bearing on which stream's loss blocks which message.

use std::collections::BTreeMap;
use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::task::Poll;
use std::time::Duration;

use bytes::Bytes;
use iroh::endpoint::transports::{
    CustomEndpoint, CustomSender, CustomTransport, RecvInfo, Transmit,
};
use iroh::EndpointId;
use iroh_base::CustomAddr;
use rand::{Rng, SeedableRng};
use rand_chacha::ChaCha8Rng;
use tokio::sync::mpsc;

/// This benchmark's transport id, distinct from iroh's own test transport
/// (`0x20`) so the two can never be confused for one another.
const TRANSPORT_ID: u64 = 0x9704;

/// How many packets may sit in a peer's inbox before the link starts dropping.
///
/// A real link has a finite queue too. Sized generously so that ordinary
/// bursts do not register as loss and confound the seeded loss rate; overflow
/// is counted separately either way.
const INBOX_CAPACITY: usize = 4_096;

/// Link conditions applied to every packet in both directions.
#[derive(Debug, Clone, Copy)]
pub struct Impairment {
    /// Fraction of packets dropped, 0.0–1.0.
    pub loss: f64,
    /// One-way delay. The round trip is twice this.
    ///
    /// A real RTT is what turns "one exchange or twenty" from a bookkeeping
    /// detail into the dominant term, which is the whole subject here.
    pub delay: Duration,
    /// Extra delay applied to a [`Self::jitter_rate`] share of packets.
    pub jitter: Duration,
    /// Fraction of packets that take the jitter delay.
    pub jitter_rate: f64,
}

impl Impairment {
    /// P4's criterion profile: 3% loss and 100 ms jitter spikes, on a 40 ms RTT.
    ///
    /// The loss and jitter are docs/11-roadmap.md §P4's. 40 ms is an
    /// unremarkable same-region consumer round trip, and the number the
    /// round-trip argument is worth stating against.
    #[must_use]
    pub const fn p4_profile() -> Self {
        Self {
            loss: 0.03,
            delay: Duration::from_millis(20),
            jitter: Duration::from_millis(100),
            jitter_rate: 0.10,
        }
    }
}

/// What the link carried.
#[derive(Debug, Default)]
pub struct LinkStats {
    /// Packets delivered.
    pub delivered: AtomicU64,
    /// Packets dropped by the loss model.
    pub dropped: AtomicU64,
    /// Packets dropped because a peer's inbox was full.
    pub overflowed: AtomicU64,
    /// Bytes offered to the link, delivered or not.
    pub bytes: AtomicU64,
}

impl LinkStats {
    /// `(delivered, dropped, overflowed, bytes)`.
    #[must_use]
    pub fn read(&self) -> (u64, u64, u64, u64) {
        (
            self.delivered.load(Ordering::Relaxed),
            self.dropped.load(Ordering::Relaxed),
            self.overflowed.load(Ordering::Relaxed),
            self.bytes.load(Ordering::Relaxed),
        )
    }
}

/// The shared link every endpoint on it sends through.
#[derive(Debug, Clone)]
pub struct Link {
    inner: Arc<LinkInner>,
}

#[derive(Debug)]
struct LinkInner {
    impairment: Impairment,
    stats: LinkStats,
    rng: Mutex<ChaCha8Rng>,
    inboxes: Mutex<BTreeMap<EndpointId, mpsc::Sender<Packet>>>,
    runtime: tokio::runtime::Handle,
}

/// One packet in flight.
#[derive(Debug)]
struct Packet {
    data: Bytes,
    from: CustomAddr,
}

impl Link {
    /// A link with the given conditions, seeded so a run can be repeated.
    #[must_use]
    pub fn new(runtime: tokio::runtime::Handle, impairment: Impairment, seed: u64) -> Self {
        Self {
            inner: Arc::new(LinkInner {
                impairment,
                stats: LinkStats::default(),
                rng: Mutex::new(ChaCha8Rng::seed_from_u64(seed)),
                inboxes: Mutex::new(BTreeMap::new()),
                runtime,
            }),
        }
    }

    /// What this link has carried.
    #[must_use]
    pub fn stats(&self) -> &LinkStats {
        &self.inner.stats
    }

    /// Attach an endpoint to this link.
    ///
    /// # Errors
    ///
    /// Fails if `id` is already attached — two endpoints under one identity
    /// would silently steal each other's packets.
    pub fn attach(&self, id: EndpointId) -> io::Result<Arc<ImpairedTransport>> {
        let (tx, rx) = mpsc::channel(INBOX_CAPACITY);
        {
            let mut inboxes = self
                .inner
                .inboxes
                .lock()
                .map_err(|_| io::Error::other("link poisoned"))?;
            if inboxes.contains_key(&id) {
                return Err(io::Error::other("endpoint already attached to this link"));
            }
            inboxes.insert(id, tx);
        }
        Ok(Arc::new(ImpairedTransport {
            id,
            addrs: n0_watcher::Watchable::new(vec![addr_of(id)]),
            link: self.clone(),
            inbox: Arc::new(Mutex::new(Some(rx))),
        }))
    }

    /// Decide one packet's fate and, if it survives, schedule its arrival.
    fn offer(&self, to: EndpointId, packet: Packet) -> io::Result<()> {
        self.inner
            .stats
            .bytes
            .fetch_add(packet.data.len() as u64, Ordering::Relaxed);

        let (dropped, delay) = {
            let mut rng = self
                .inner
                .rng
                .lock()
                .map_err(|_| io::Error::other("link poisoned"))?;
            let impairment = self.inner.impairment;
            if impairment.loss > 0.0 && rng.random_bool(impairment.loss) {
                (true, Duration::ZERO)
            } else {
                let mut delay = impairment.delay;
                if impairment.jitter_rate > 0.0 && rng.random_bool(impairment.jitter_rate) {
                    delay += impairment.jitter;
                }
                (false, delay)
            }
        };
        if dropped {
            self.inner.stats.dropped.fetch_add(1, Ordering::Relaxed);
            return Ok(());
        }

        let inbox = {
            let inboxes = self
                .inner
                .inboxes
                .lock()
                .map_err(|_| io::Error::other("link poisoned"))?;
            inboxes.get(&to).cloned()
        };
        let Some(inbox) = inbox else {
            return Err(io::Error::other("unknown endpoint on this link"));
        };

        // The delay is a timer rather than a queue this thread drains, so a
        // held packet never blocks the sender — which is what a link does, and
        // what a jittered packet being overtaken depends on.
        let stats = Arc::clone(&self.inner);
        self.inner.runtime.spawn(async move {
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
            match inbox.try_send(packet) {
                Ok(()) => stats.stats.delivered.fetch_add(1, Ordering::Relaxed),
                Err(_) => stats.stats.overflowed.fetch_add(1, Ordering::Relaxed),
            };
        });
        Ok(())
    }
}

/// The custom address for an endpoint on this link.
#[must_use]
pub fn addr_of(id: EndpointId) -> CustomAddr {
    CustomAddr::from((TRANSPORT_ID, &id.as_bytes()[..]))
}

fn id_of(addr: &CustomAddr) -> io::Result<EndpointId> {
    if addr.id() != TRANSPORT_ID {
        return Err(io::Error::other("not this link's transport"));
    }
    let bytes: &[u8; 32] = addr
        .data()
        .try_into()
        .map_err(|_| io::Error::other("wrong address length"))?;
    EndpointId::from_bytes(bytes).map_err(|_| io::Error::other("not an endpoint id"))
}

/// One endpoint's attachment to an impaired [`Link`].
#[derive(Debug)]
pub struct ImpairedTransport {
    id: EndpointId,
    addrs: n0_watcher::Watchable<Vec<CustomAddr>>,
    link: Link,
    /// Taken by the first `bind`. A second bind of the same transport would
    /// otherwise get an endpoint with no way to receive.
    inbox: Arc<Mutex<Option<mpsc::Receiver<Packet>>>>,
}

impl CustomTransport for ImpairedTransport {
    fn bind(&self) -> io::Result<Box<dyn CustomEndpoint>> {
        let inbox = self
            .inbox
            .lock()
            .map_err(|_| io::Error::other("transport poisoned"))?
            .take()
            .ok_or_else(|| io::Error::other("this transport is already bound"))?;
        Ok(Box::new(BoundEndpoint {
            id: self.id,
            addrs: self.addrs.clone(),
            link: self.link.clone(),
            inbox,
        }))
    }
}

#[derive(Debug)]
struct BoundEndpoint {
    id: EndpointId,
    addrs: n0_watcher::Watchable<Vec<CustomAddr>>,
    link: Link,
    inbox: mpsc::Receiver<Packet>,
}

impl CustomEndpoint for BoundEndpoint {
    fn watch_local_addrs(&self) -> n0_watcher::Direct<Vec<CustomAddr>> {
        self.addrs.watch()
    }

    fn create_sender(&self) -> Arc<dyn CustomSender> {
        Arc::new(LinkSender {
            id: self.id,
            link: self.link.clone(),
        })
    }

    fn poll_recv(
        &mut self,
        cx: &mut std::task::Context,
        bufs: &mut [io::IoSliceMut<'_>],
        metas: &mut [noq_udp::RecvMeta],
        recv_infos: &mut [RecvInfo],
    ) -> Poll<io::Result<usize>> {
        let slots = bufs.len().min(metas.len()).min(recv_infos.len());
        if slots == 0 {
            return Poll::Ready(Ok(0));
        }
        let mut packets = Vec::with_capacity(slots);
        match self.inbox.poll_recv_many(cx, &mut packets, slots) {
            Poll::Pending => return Poll::Pending,
            Poll::Ready(0) => return Poll::Ready(Err(io::Error::other("link closed"))),
            Poll::Ready(_) => {}
        }

        let mut filled = 0;
        for packet in packets {
            if bufs[filled].len() < packet.data.len() {
                // The caller's buffer is smaller than the datagram. Dropping it
                // is what a real socket does with a short read, and QUIC treats
                // it as loss — which this link already models.
                continue;
            }
            bufs[filled][..packet.data.len()].copy_from_slice(&packet.data);
            metas[filled].len = packet.data.len();
            metas[filled].stride = packet.data.len();
            recv_infos[filled] = RecvInfo::new(packet.from, Some(addr_of(self.id)));
            filled += 1;
        }
        if filled == 0 {
            return Poll::Pending;
        }
        Poll::Ready(Ok(filled))
    }
}

#[derive(Debug)]
struct LinkSender {
    id: EndpointId,
    link: Link,
}

impl CustomSender for LinkSender {
    fn is_valid_send_addr(&self, addr: &CustomAddr) -> bool {
        addr.id() == TRANSPORT_ID
    }

    fn poll_send(
        &self,
        _cx: &mut std::task::Context,
        dst: &CustomAddr,
        _src: Option<&CustomAddr>,
        transmit: &Transmit<'_>,
    ) -> Poll<io::Result<()>> {
        let to = match id_of(dst) {
            Ok(id) => id,
            Err(err) => return Poll::Ready(Err(err)),
        };
        let from = addr_of(self.id);
        // A GSO transmit is several datagrams in one buffer. They are separate
        // packets on the wire and have to be separate here too, or a single
        // coin flip would drop a whole batch and the loss rate would be a lie.
        let segment = transmit.segment_size.unwrap_or(transmit.contents.len());
        for chunk in transmit.contents.chunks(segment.max(1)) {
            if let Err(err) = self.link.offer(
                to,
                Packet {
                    data: Bytes::copy_from_slice(chunk),
                    from: from.clone(),
                },
            ) {
                return Poll::Ready(Err(err));
            }
        }
        Poll::Ready(Ok(()))
    }
}
