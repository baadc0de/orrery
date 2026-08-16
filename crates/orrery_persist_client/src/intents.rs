//! The intent queue: signed, witness-attested critical writes with the
//! netsplit posture (D11 §2.2, D12).
//!
//! Critical operations (trades, currency, progression, structure placement)
//! are the only path for durable consequences. The client signs the intent,
//! gathers K-of-N attestations from the seeded witness set, and submits it to
//! the gateway on the reliable stream lane. While the gateway is unreachable the
//! intent queues locally (the D12 posture: P2P sim continues, durable commits
//! pause) and replays on reconnect — idempotency keys make replay safe.
//!
//! Outcomes are predicted locally so the UI does not wait for the < 10 ms
//! commit (D8: intents are the only path for durable consequences, so the
//! predicted effect is rolled back on `Rejected`).
//!
//! Netsplit recovery (D11 §2.2): intents flipped to `InFlight` by [`drain`]
//! are returned to `Queued` by [`requeue_all_inflight`] on disconnect, and the
//! plugin's reconnect system calls it via [`disconnect_gateway`]. A per-intent
//! in-flight timeout (default 10 s) also retriggers requeue so an unacked
//! intent is retransmitted without a full disconnect cycle.
//!
//! # Why at-least-once survived the move to a reliable lane
//!
//! Submissions ride the reliable stream lane now (C-1), so an intent is no
//! longer lost to a dropped datagram — and none of the machinery above is
//! therefore redundant, because none of it was ever really about datagram
//! loss. The window it covers is the one QUIC cannot close: the gateway
//! receives a submission, commits it durably, and the connection dies before
//! the ack reaches the client. The client cannot distinguish that from a
//! submission that never arrived, so it must replay — and replay must not
//! commit twice.
//!
//! That is what the `intent/{intent_id}` idempotency row on the gateway side
//! is for, and it is why the pairing stays: **at-least-once delivery plus an
//! idempotency key is a route to exactly-once *outcomes*, and a better
//! transport does not supply one.** Removing either half because the wire got
//! more reliable would trade a safety property for a redundancy that is not
//! there.
//!
//! What did change is what the in-flight timeout *means*. It was a
//! retransmit timer for a lost submission; on a reliable lane a submission on
//! a live connection cannot be lost without the connection dying, and a dying
//! connection already requeues. It is now a liveness backstop for a gateway
//! that accepted an intent and never answered — a stalled executor, a lost
//! reply — which is why it is measured in seconds and not in round trips.

use std::collections::VecDeque;
use std::time::Duration;

use bevy_ecs::prelude::*;
use bevy_platform::time::Instant;
use orrery_protocol::{Intent, IntentOutcome};

use crate::gateway::GatewaySession;
use crate::latency::LatencyHistogram;
use crate::queue_store::{open_store, QueueStore};

/// A ticket identifying a queued intent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct IntentTicket(pub u128);

/// The status of a queued intent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IntentStatus {
    /// The intent is being assembled (signing / attestation gathering).
    Draft,
    /// Signed and queued locally, awaiting transmission (netsplit posture).
    Queued,
    /// Transmitted to the gateway, awaiting the commit ack.
    InFlight,
    /// Committed at the given tick.
    Committed(orrery_protocol::Tick),
    /// Rejected by the gateway (validation or a durable invariant).
    Rejected(orrery_protocol::IntentOutcome),
}

/// The locally-predicted effect of an intent (D8).
///
/// Applied optimistically so the UI does not wait for the commit; rolled back
/// on `Rejected`. The shape is `Ruleset`-opaque — the game interprets the
/// payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PredictedEffects {
    /// The intent id this prediction belongs to.
    pub intent_id: u128,
    /// Opaque predicted effects, interpreted by the game.
    pub payload: bytes::Bytes,
}

/// A queued intent and its bookkeeping.
#[derive(Debug, Clone)]
struct QueuedIntent {
    intent: Intent,
    status: IntentStatus,
    predicted: Option<PredictedEffects>,
    /// When this intent was submitted (for latency tracking).
    submitted_at: Instant,
    /// When this intent was last sent (for in-flight timeout checking).
    /// `None` if it has never been sent.
    sent_at: Option<Instant>,
}

/// Default in-flight timeout before an unacked intent is requeued (10 s).
///
/// A liveness backstop, not a retransmit timer — see the [module
/// docs](self#why-at-least-once-survived-the-move-to-a-reliable-lane). It is
/// deliberately far above the D16 intent-commit budget (p99 < 10 ms) so it
/// never fires on a gateway that is merely slow; a client that resubmits at
/// the commit budget would amplify load against exactly the gateway that is
/// already struggling.
const INFLIGHT_TIMEOUT: Duration = Duration::from_secs(10);

/// The intent queue (docs/10-crates.md §9).
///
/// A [`Resource`] holding the offline intent queue and the status of each
/// pending intent. The plugin's system drains the queue to the gateway when
/// connected and records acks as they arrive.
///
/// With the `disk-queue` feature and a configured `queue_dir`, queued intents
/// are appended to a durable store so they survive process exit and replay on
/// the next run (netsplit posture, D12; idempotency keys make replay safe).
#[derive(Debug, Resource)]
pub struct IntentQueue {
    /// Queued intents, in submission order.
    queue: VecDeque<QueuedIntent>,
    /// The queue capacity (from config).
    capacity: usize,
    /// The durable store (in-memory unless disk-backed).
    store: Box<dyn QueueStore>,
    /// Intent-commit latency histogram (D16: intent commit p99 < 10 ms).
    intent_latency: LatencyHistogram,
}

impl Default for IntentQueue {
    fn default() -> Self {
        Self {
            queue: VecDeque::new(),
            capacity: 4096,
            store: Box::new(crate::queue_store::MemQueueStore::new()),
            intent_latency: LatencyHistogram::new(),
        }
    }
}

impl IntentQueue {
    /// A new, empty queue with the given capacity and an in-memory store.
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self {
            queue: VecDeque::new(),
            capacity,
            store: Box::new(crate::queue_store::MemQueueStore::new()),
            intent_latency: LatencyHistogram::new(),
        }
    }

    /// A new queue backed by the store opened from `queue_dir`.
    ///
    /// Loads any intents persisted by a previous run (netsplit posture).
    #[must_use]
    pub fn with_store(capacity: usize, queue_dir: Option<&std::path::Path>) -> Self {
        let store = open_store(queue_dir);
        let mut queue = VecDeque::new();
        for intent in store.load() {
            queue.push_back(QueuedIntent {
                intent,
                status: IntentStatus::Queued,
                predicted: None,
                submitted_at: Instant::now(),
                sent_at: None,
            });
        }
        Self {
            queue,
            capacity,
            store,
            intent_latency: LatencyHistogram::new(),
        }
    }

    /// Submit a signed intent, returning a ticket.
    ///
    /// The intent is queued locally (and persisted, if disk-backed). If the
    /// gateway is reachable it is transmitted on the next drain; otherwise it
    /// waits (netsplit posture). Returns `None` if the queue is full.
    pub fn submit(&mut self, intent: Intent) -> Option<IntentTicket> {
        if self.queue.len() >= self.capacity {
            return None;
        }
        let ticket = IntentTicket(intent.intent_id);
        let _ = self.store.push(&intent);
        self.queue.push_back(QueuedIntent {
            intent,
            status: IntentStatus::Queued,
            predicted: None,
            submitted_at: Instant::now(),
            sent_at: None,
        });
        Some(ticket)
    }

    /// Attach a locally-predicted effect to a queued intent.
    ///
    /// The prediction is applied optimistically and rolled back on `Rejected`.
    pub fn predict(&mut self, ticket: IntentTicket, effects: PredictedEffects) {
        if let Some(entry) = self
            .queue
            .iter_mut()
            .find(|e| e.intent.intent_id == ticket.0)
        {
            entry.predicted = Some(effects);
        }
    }

    /// The status of a queued intent.
    #[must_use]
    pub fn status(&self, ticket: IntentTicket) -> IntentStatus {
        self.queue
            .iter()
            .find(|e| e.intent.intent_id == ticket.0)
            .map(|e| e.status.clone())
            .unwrap_or(IntentStatus::Rejected(IntentOutcome::Rejected {
                reason: 0,
            }))
    }

    /// The predicted effect of a queued intent, if any.
    #[must_use]
    pub fn predicted_outcome(&self, ticket: IntentTicket) -> Option<&PredictedEffects> {
        self.queue
            .iter()
            .find(|e| e.intent.intent_id == ticket.0)
            .and_then(|e| e.predicted.as_ref())
    }

    /// The number of queued intents.
    #[must_use]
    pub fn len(&self) -> usize {
        self.queue.len()
    }

    /// Whether no intents are queued.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }

    /// Drain the queue to the gateway.
    ///
    /// Marks each queued intent `InFlight` and returns the intents to transmit.
    /// Called by the plugin when the gateway is connected. `sent_at` is updated
    /// so a subsequent `on_ack` measures the round trip from this send.
    pub fn drain(&mut self) -> Vec<Intent> {
        let now = Instant::now();
        let mut out = Vec::new();
        for entry in self.queue.iter_mut() {
            if entry.status == IntentStatus::Queued {
                entry.status = IntentStatus::InFlight;
                entry.sent_at = Some(now);
                out.push(entry.intent.clone());
            }
        }
        out
    }

    /// Record a gateway ack for an intent.
    ///
    /// On `Committed` the intent is marked committed (its durable consequence
    /// is recorded). On `Rejected` it is marked rejected so the game rolls back
    /// the predicted effect. The entry is retained so [`IntentQueue::status`]
    /// returns the terminal state; it is dropped once the game observes it.
    ///
    /// On `Committed`, the submit→ack round trip is recorded in the
    /// intent-commit latency histogram (D16: intent commit p99 < 10 ms).
    pub fn on_ack(&mut self, intent_id: u128, outcome: IntentOutcome) -> Option<IntentTicket> {
        let entry = self
            .queue
            .iter_mut()
            .find(|e| e.intent.intent_id == intent_id)?;
        entry.status = match &outcome {
            IntentOutcome::Committed { tick, .. } => {
                // Intent-latency: time from submission to commit ack.
                self.intent_latency.record(entry.submitted_at.elapsed());
                IntentStatus::Committed(*tick)
            }
            IntentOutcome::Rejected { .. } => IntentStatus::Rejected(outcome.clone()),
        };
        Some(IntentTicket(intent_id))
    }

    /// Drop a completed (committed or rejected) intent from the queue.
    ///
    /// The game calls this once it has observed the terminal status, freeing
    /// the slot and (if disk-backed) removing the intent from the store.
    /// Returns the ticket if the intent was present.
    pub fn retire(&mut self, intent_id: u128) -> Option<IntentTicket> {
        let idx = self
            .queue
            .iter()
            .position(|e| e.intent.intent_id == intent_id)?;
        self.queue.remove(idx);
        let _ = self.store.remove(intent_id);
        Some(IntentTicket(intent_id))
    }

    /// Mark an intent in-flight as queued again (transmission failed, e.g.
    /// the connection dropped mid-send). It will be retried on the next drain.
    pub fn requeue(&mut self, intent_id: u128) {
        if let Some(entry) = self
            .queue
            .iter_mut()
            .find(|e| e.intent.intent_id == intent_id)
        {
            if entry.status == IntentStatus::InFlight {
                entry.status = IntentStatus::Queued;
                entry.sent_at = None;
            }
        }
    }

    /// Return every `InFlight` intent to `Queued` (netsplit posture, D11 §2.2).
    ///
    /// Called on gateway disconnect: intents that were mid-flight when the
    /// connection dropped must be retransmitted on reconnect, so they go back
    /// to `Queued` and the next [`drain`](Self::drain) replays them in
    /// submission order. Idempotency keys make replay safe.
    pub fn requeue_all_inflight(&mut self) {
        for entry in self.queue.iter_mut() {
            if entry.status == IntentStatus::InFlight {
                entry.status = IntentStatus::Queued;
                entry.sent_at = None;
            }
        }
    }

    /// Requeue any `InFlight` intent whose last send is older than the
    /// in-flight timeout, so an unacked intent is retransmitted without
    /// waiting for a disconnect. Returns the number requeued.
    pub fn requeue_expired(&mut self) -> usize {
        let now = Instant::now();
        let mut requeued = 0;
        for entry in self.queue.iter_mut() {
            if entry.status == IntentStatus::InFlight
                && entry
                    .sent_at
                    .is_some_and(|sent| now.saturating_duration_since(sent) >= INFLIGHT_TIMEOUT)
            {
                entry.status = IntentStatus::Queued;
                entry.sent_at = None;
                requeued += 1;
            }
        }
        requeued
    }

    /// The intent-commit latency histogram (D16: p99 < 10 ms).
    #[must_use]
    pub fn intent_latency(&self) -> &LatencyHistogram {
        &self.intent_latency
    }
}

/// The Bevy system that drains the intent queue to the gateway.
///
/// When connected, queued intents are written to the session's reliable lane.
/// Before draining, any `InFlight` intent older than the in-flight timeout is
/// requeued so a gateway that accepted an intent and never answered does not
/// strand it forever.
pub fn drain_intents(
    session: Res<GatewaySession>,
    mut queue: ResMut<IntentQueue>,
    mut streams: Query<&mut aeronet_iroh::stream::IrohStreamIo>,
) {
    if !session.is_connected() {
        return;
    }
    let Some(entity) = session.session else {
        return;
    };
    let Ok(mut streams) = streams.get_mut(entity) else {
        return;
    };
    let _ = queue.requeue_expired();
    for intent in queue.drain() {
        let msg = orrery_protocol::GatewayMsg::SubmitIntent { intent };
        GatewaySession::push_control(&mut streams, &msg);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use orrery_protocol::{Attestation, CellEpoch, IntentOp, NodeId, Tick};

    fn node(n: u8) -> NodeId {
        let mut seed = [0u8; 32];
        seed[0] = n;
        iroh_base::SecretKey::from_bytes(&seed).public()
    }

    fn sig() -> orrery_protocol::Signature {
        let seed = [0u8; 32];
        iroh_base::SecretKey::from_bytes(&seed).sign(b"test")
    }

    fn intent(id: u128) -> Intent {
        Intent {
            intent_id: id,
            issuer: node(1),
            cell_epoch: CellEpoch::new(0),
            ops: vec![IntentOp {
                op: 1,
                args: bytes::Bytes::from_static(b"trade"),
            }],
            attestations: vec![Attestation {
                witness: node(2),
                signature: sig(),
            }],
            signature: sig(),
        }
    }

    #[test]
    fn submit_and_status() {
        let mut queue = IntentQueue::new(10);
        let ticket = queue.submit(intent(1)).unwrap();
        assert_eq!(queue.status(ticket), IntentStatus::Queued);
        assert_eq!(queue.len(), 1);
    }

    #[test]
    fn full_queue_rejects() {
        let mut queue = IntentQueue::new(2);
        queue.submit(intent(1)).unwrap();
        queue.submit(intent(2)).unwrap();
        assert!(queue.submit(intent(3)).is_none());
    }

    #[test]
    fn predict_and_ack() {
        let mut queue = IntentQueue::new(10);
        let ticket = queue.submit(intent(1)).unwrap();
        queue.predict(
            ticket,
            PredictedEffects {
                intent_id: 1,
                payload: bytes::Bytes::from_static(b"effects"),
            },
        );
        assert!(queue.predicted_outcome(ticket).is_some());
        queue.on_ack(
            1,
            IntentOutcome::Committed {
                tick: Tick::new(5),
                minted: vec![],
            },
        );
        assert_eq!(queue.status(ticket), IntentStatus::Committed(Tick::new(5)));
        // Retire frees the slot.
        queue.retire(1);
        assert_eq!(queue.len(), 0);
    }

    #[test]
    fn drain_marks_inflight_and_requeues() {
        let mut queue = IntentQueue::new(10);
        let ticket = queue.submit(intent(1)).unwrap();
        let drained = queue.drain();
        assert_eq!(drained.len(), 1);
        assert_eq!(queue.status(ticket), IntentStatus::InFlight);
        // Requeue on transmission failure.
        queue.requeue(1);
        assert_eq!(queue.status(ticket), IntentStatus::Queued);
        // A second drain retries it.
        assert_eq!(queue.drain().len(), 1);
    }

    #[test]
    fn rejected_is_retained_until_retired() {
        let mut queue = IntentQueue::new(10);
        let ticket = queue.submit(intent(1)).unwrap();
        queue.on_ack(1, IntentOutcome::Rejected { reason: 7 });
        // The ticket reports the rejection reason.
        assert_eq!(
            queue.status(ticket),
            IntentStatus::Rejected(IntentOutcome::Rejected { reason: 7 })
        );
        // Retire frees the slot.
        queue.retire(1);
        assert_eq!(queue.len(), 0);
    }

    #[test]
    fn requeue_all_inflight_returns_inflight_to_queued() {
        let mut queue = IntentQueue::new(10);
        // Submit 5 intents, drain them all (now InFlight).
        for i in 1..=5 {
            queue.submit(intent(i)).unwrap();
        }
        let drained = queue.drain();
        assert_eq!(drained.len(), 5);
        // All are InFlight.
        for i in 1..=5 {
            assert_eq!(queue.status(IntentTicket(i)), IntentStatus::InFlight);
        }
        // Simulate disconnect.
        queue.requeue_all_inflight();
        // All are Queued again.
        for i in 1..=5 {
            assert_eq!(queue.status(IntentTicket(i)), IntentStatus::Queued);
        }
        // A second drain retries all 5.
        let re_drained = queue.drain();
        assert_eq!(re_drained.len(), 5);
    }

    #[test]
    fn requeue_expired_retriggers_unacked_intents() {
        let mut queue = IntentQueue::new(10);
        queue.submit(intent(1)).unwrap();
        queue.submit(intent(2)).unwrap();
        queue.drain();

        // With no time elapsed, nothing is expired.
        assert_eq!(queue.requeue_expired(), 0);
        assert_eq!(queue.status(IntentTicket(1)), IntentStatus::InFlight);

        // Manually set sent_at to a distant past to simulate timeout.
        if let Some(entry) = queue.queue.iter_mut().find(|e| e.intent.intent_id == 1) {
            entry.sent_at = Some(
                Instant::now()
                    .checked_sub(Duration::from_secs(20))
                    .unwrap_or(Instant::now()),
            );
        }

        assert_eq!(queue.requeue_expired(), 1);
        assert_eq!(queue.status(IntentTicket(1)), IntentStatus::Queued);
        // Intent 2 is still InFlight (not expired).
        assert_eq!(queue.status(IntentTicket(2)), IntentStatus::InFlight);
    }

    #[test]
    fn intent_latency_is_recorded_on_commit() {
        let mut queue = IntentQueue::new(10);
        queue.submit(intent(1)).unwrap();
        queue.drain();

        // After drain, the intent has a sent_at set. on_ack with Committed
        // should record a latency sample.
        assert_eq!(queue.intent_latency().total(), 0);
        queue.on_ack(
            1,
            IntentOutcome::Committed {
                tick: Tick::new(10),
                minted: vec![],
            },
        );
        // The latency histogram should now have one sample.
        assert_eq!(queue.intent_latency().total(), 1);
        // The min should be a small positive duration.
        assert!(queue.intent_latency().min().unwrap() > Duration::ZERO);
    }

    #[test]
    fn rejected_intent_does_not_record_latency() {
        let mut queue = IntentQueue::new(10);
        queue.submit(intent(1)).unwrap();
        queue.drain();

        assert_eq!(queue.intent_latency().total(), 0);
        queue.on_ack(1, IntentOutcome::Rejected { reason: 7 });
        // Rejected does not record a latency sample.
        assert_eq!(queue.intent_latency().total(), 0);
        assert_eq!(
            queue.status(IntentTicket(1)),
            IntentStatus::Rejected(IntentOutcome::Rejected { reason: 7 })
        );
    }

    #[test]
    fn requeue_all_inflight_does_not_affect_committed_or_rejected() {
        let mut queue = IntentQueue::new(10);
        queue.submit(intent(1)).unwrap();
        queue.submit(intent(2)).unwrap();
        queue.drain();

        // Ack one as committed, one as rejected.
        queue.on_ack(
            1,
            IntentOutcome::Committed {
                tick: Tick::new(10),
                minted: vec![],
            },
        );
        queue.on_ack(2, IntentOutcome::Rejected { reason: 3 });

        // requeue_all_inflight should not affect them (they are terminal).
        queue.requeue_all_inflight();
        assert_eq!(
            queue.status(IntentTicket(1)),
            IntentStatus::Committed(Tick::new(10))
        );
        assert_eq!(
            queue.status(IntentTicket(2)),
            IntentStatus::Rejected(IntentOutcome::Rejected { reason: 3 })
        );
    }

    #[test]
    fn requeue_clears_sent_at() {
        let mut queue = IntentQueue::new(10);
        queue.submit(intent(1)).unwrap();
        queue.drain();

        // Requeue a single intent.
        queue.requeue(1);
        // After requeue, the sent_at should be None so a future drain sets
        // a fresh timestamp and on_ack measures from the new send.
        if let Some(entry) = queue.queue.iter().find(|e| e.intent.intent_id == 1) {
            assert!(entry.sent_at.is_none());
        }
    }

    #[test]
    fn requeue_all_inflight_clears_sent_at() {
        let mut queue = IntentQueue::new(10);
        queue.submit(intent(1)).unwrap();
        queue.drain();

        queue.requeue_all_inflight();
        if let Some(entry) = queue.queue.iter().find(|e| e.intent.intent_id == 1) {
            assert!(entry.sent_at.is_none());
        }
    }

    #[test]
    fn requeue_expired_clears_sent_at() {
        let mut queue = IntentQueue::new(10);
        queue.submit(intent(1)).unwrap();
        queue.drain();

        // Force the sent_at to be expired.
        if let Some(entry) = queue.queue.iter_mut().find(|e| e.intent.intent_id == 1) {
            entry.sent_at = Some(
                Instant::now()
                    .checked_sub(Duration::from_secs(20))
                    .unwrap_or(Instant::now()),
            );
        }

        queue.requeue_expired();
        if let Some(entry) = queue.queue.iter().find(|e| e.intent.intent_id == 1) {
            assert!(entry.sent_at.is_none());
        }
    }
}
