//! The intent queue: signed, witness-attested critical writes with the
//! netsplit posture (D11 §2.2, D12).
//!
//! Critical operations (trades, currency, progression, structure placement)
//! are the only path for durable consequences. The client signs the intent,
//! gathers K-of-N attestations from the seeded witness set, and submits it to
//! the gateway over a reliable stream. While the gateway is unreachable the
//! intent queues locally (the D12 posture: P2P sim continues, durable commits
//! pause) and replays on reconnect — idempotency keys make replay safe.
//!
//! Outcomes are predicted locally so the UI does not wait for the < 10 ms
//! commit (D8: intents are the only path for durable consequences, so the
//! predicted effect is rolled back on `Rejected`).

use std::collections::VecDeque;

use bevy_ecs::prelude::*;
use orrery_protocol::{Intent, IntentOutcome};

use crate::gateway::GatewaySession;
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
}

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
}

impl Default for IntentQueue {
    fn default() -> Self {
        Self {
            queue: VecDeque::new(),
            capacity: 4096,
            store: Box::new(crate::queue_store::MemQueueStore::new()),
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
            });
        }
        Self {
            queue,
            capacity,
            store,
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
    /// Called by the plugin when the gateway is connected.
    pub fn drain(&mut self) -> Vec<Intent> {
        let mut out = Vec::new();
        for entry in self.queue.iter_mut() {
            if entry.status == IntentStatus::Queued {
                entry.status = IntentStatus::InFlight;
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
    pub fn on_ack(&mut self, intent_id: u128, outcome: IntentOutcome) -> Option<IntentTicket> {
        let entry = self
            .queue
            .iter_mut()
            .find(|e| e.intent.intent_id == intent_id)?;
        entry.status = match &outcome {
            IntentOutcome::Committed { tick, .. } => IntentStatus::Committed(*tick),
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
            }
        }
    }
}

/// The Bevy system that drains the intent queue to the gateway.
///
/// When connected, queued intents are transmitted over the reliable stream.
pub fn drain_intents(
    session: Res<GatewaySession>,
    mut queue: ResMut<IntentQueue>,
    mut sessions: Query<&mut aeronet_io::Session>,
) {
    if !session.is_connected() {
        return;
    }
    let Some(entity) = session.session else {
        return;
    };
    let Ok(mut io) = sessions.get_mut(entity) else {
        return;
    };
    for intent in queue.drain() {
        let msg = orrery_protocol::GatewayMsg::SubmitIntent { intent };
        io.send.push(GatewaySession::encode_stream(&msg));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use orrery_protocol::{Attestation, Epoch, IntentOp, NodeId, Tick};

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
            cell_epoch: Epoch::new(0),
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
}
