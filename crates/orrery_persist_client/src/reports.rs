//! The report queue: signed discrepancy reports on their way to the cluster
//! adjudicator (docs/07-witnessing.md §3, stages 3 and 4).
//!
//! The client's third egress, beside the diff uplink and the intent queue, and
//! the only one that carries an accusation. It is deliberately the least
//! insistent of the three:
//!
//! - **No retry, no idempotency key.** An intent must commit exactly once and
//!   replays until it does; a report is evidence about a window that has
//!   already happened, and the same window can always be re-reported from the
//!   witness's own retained log. Replaying one blind would spend the reporter's
//!   own per-account rate limit (docs/07 §7) on a duplicate of an accusation
//!   the cluster may have already judged.
//! - **Bounded, oldest-first.** A client that cannot reach a gateway for
//!   minutes has no business holding minutes of accusations: the evidence ages
//!   past the adjudicator's retention anyway. The queue drops its oldest and
//!   counts what it dropped, because a silently shrinking accusation queue is
//!   exactly the failure P4 exists to measure.
//!
//! # Who fills it
//!
//! Not this crate. `orrery_persist_client` does not depend on
//! `orrery_witness` and must not — the dependency spine (D15) puts them side by
//! side, and only the `orrery` facade depends on both. So the queue is a
//! resource with a public push, and the facade owns the one system that moves
//! a filed report into it. That is the facade's second cross-crate system
//! ever, and its own docs say why the exception is narrow.

use std::collections::VecDeque;

use bevy_ecs::prelude::*;
use orrery_protocol::{DiscrepancyReport, NodeId, PersistId, Tick, Verdict};

use crate::gateway::GatewaySession;

/// Reports held while the gateway is unreachable, before the oldest is
/// dropped.
///
/// Sixteen is the same order as the escalation burst a witness can legitimately
/// produce (it watches at most seven subjects and escalates each divergence
/// episode once), so a client that reconnects promptly loses nothing, and one
/// that does not is holding evidence the adjudicator's 3 s window has already
/// outlived.
pub const DEFAULT_REPORT_QUEUE_CAPACITY: usize = 16;

/// Answers the cluster kept per queue, before the oldest is dropped.
///
/// Verdicts are read by the game (telemetry in P4, the strike ledger later),
/// and a game that never reads them must not grow the resource without bound.
const RETAINED_OUTCOMES: usize = 64;

/// What the cluster said about one filed report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReportOutcome {
    /// The accused peer the report named.
    pub subject: NodeId,
    /// The entity the bundle covered.
    pub entity: PersistId,
    /// The disputed claim tick the window closed at.
    pub window_end: Tick,
    /// The verdict, when the cluster judged the evidence at all.
    ///
    /// `None` is a refusal, not an exoneration — see `reason`. Collapsing the
    /// two would let a cluster with no rules build linked read as one that
    /// found nothing wrong.
    pub verdict: Option<Verdict>,
    /// `REPORT_ADJUDICATED` when `verdict` is present, otherwise why the
    /// cluster would not judge it (`orrery_protocol::REPORT_REFUSED_*`).
    pub reason: u16,
}

/// Signed discrepancy reports awaiting transmission, and the answers that came
/// back.
#[derive(Debug, Resource)]
pub struct ReportQueue {
    queue: VecDeque<Box<DiscrepancyReport>>,
    capacity: usize,
    outcomes: VecDeque<ReportOutcome>,
    /// Reports filed into this queue since startup.
    pub filed: u64,
    /// Reports sent to the gateway since startup.
    pub sent: u64,
    /// Reports dropped because the queue was full while disconnected.
    pub dropped: u64,
}

impl Default for ReportQueue {
    fn default() -> Self {
        Self::new(DEFAULT_REPORT_QUEUE_CAPACITY)
    }
}

impl ReportQueue {
    /// An empty queue holding at most `capacity` unsent reports.
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self {
            queue: VecDeque::new(),
            capacity: capacity.max(1),
            outcomes: VecDeque::new(),
            filed: 0,
            sent: 0,
            dropped: 0,
        }
    }

    /// Queue a signed report for the next drain.
    ///
    /// Drops the oldest when full: the newest accusation is the one whose
    /// evidence is still inside the adjudicator's retention.
    pub fn push(&mut self, report: Box<DiscrepancyReport>) {
        self.filed += 1;
        if self.queue.len() >= self.capacity {
            self.queue.pop_front();
            self.dropped += 1;
        }
        self.queue.push_back(report);
    }

    /// Reports waiting to be sent.
    #[must_use]
    pub fn pending(&self) -> usize {
        self.queue.len()
    }

    /// Take everything queued, for transmission.
    ///
    /// Unlike [`crate::IntentQueue::drain`] this does not retain the entries:
    /// there is nothing to correlate an answer against and nothing to retry.
    /// See the module docs for why a report is not an intent.
    pub fn drain(&mut self) -> Vec<Box<DiscrepancyReport>> {
        self.sent += self.queue.len() as u64;
        self.queue.drain(..).collect()
    }

    /// Record one gateway answer.
    pub fn record_outcome(&mut self, outcome: ReportOutcome) {
        if self.outcomes.len() >= RETAINED_OUTCOMES {
            self.outcomes.pop_front();
        }
        self.outcomes.push_back(outcome);
    }

    /// The answers held, oldest first.
    pub fn outcomes(&self) -> impl Iterator<Item = &ReportOutcome> {
        self.outcomes.iter()
    }

    /// Take the answers held, oldest first, clearing them.
    pub fn take_outcomes(&mut self) -> Vec<ReportOutcome> {
        self.outcomes.drain(..).collect()
    }
}

/// The Bevy system that drains the report queue to the gateway.
///
/// Mirrors [`crate::intents::drain_intents`]: reports ride the reliable
/// control lane, because a bundle is far past a datagram and because an
/// accusation that arrives torn is worse than one that arrives late.
pub fn drain_reports(
    session: Res<GatewaySession>,
    mut queue: ResMut<ReportQueue>,
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
    for report in queue.drain() {
        let msg = orrery_protocol::GatewayMsg::Report { report };
        GatewaySession::push_control(&mut streams, &msg);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use orrery_protocol::{ChainHash, EvidenceBundle, RulesetId, StateClaim};

    fn report(entity: u64) -> Box<DiscrepancyReport> {
        let key = iroh_base::SecretKey::from_bytes(&[7; 32]);
        let ruleset = RulesetId {
            version: 1,
            digest: [1; 32],
        };
        let entity = PersistId::new(entity);
        Box::new(DiscrepancyReport {
            subject: key.public(),
            bundle: EvidenceBundle {
                ruleset,
                entity,
                window_start: Tick::new(0),
                window_end: Tick::new(30),
                t0_claim: StateClaim {
                    entity,
                    chain_epoch: 0,
                    tick: Tick::new(0),
                    input_head: ChainHash::EMPTY,
                    state_hash: [0; 32],
                    prev_claim: [0; 32],
                    ruleset,
                    sig: key.sign(b"claim"),
                },
                t0_snapshot: bytes::Bytes::new(),
                frames: Vec::new(),
                sibling_heads: Vec::new(),
                disputed_claims: Vec::new(),
                claimed_hashes: Vec::new(),
                computed_hashes: Vec::new(),
            },
            reporter: key.public(),
            reporter_sig: key.sign(b"report"),
        })
    }

    #[test]
    fn a_full_queue_drops_its_oldest_and_says_so() {
        // A disconnected client holds accusations whose evidence is ageing out
        // of the adjudicator's retention anyway, so the newest is the one
        // worth keeping — but a queue that shed silently would make the P4
        // report count measure the queue depth rather than the divergences.
        let mut queue = ReportQueue::new(2);
        for entity in 1..=4 {
            queue.push(report(entity));
        }
        assert_eq!(queue.pending(), 2);
        assert_eq!(queue.dropped, 2);
        assert_eq!(queue.filed, 4);

        let drained = queue.drain();
        assert_eq!(drained.len(), 2);
        assert_eq!(
            drained
                .iter()
                .map(|report| report.bundle.entity)
                .collect::<Vec<_>>(),
            vec![PersistId::new(3), PersistId::new(4)],
            "the two newest survived"
        );
        assert_eq!(queue.sent, 2);
        assert_eq!(queue.pending(), 0);
    }

    #[test]
    fn a_refusal_is_kept_distinct_from_an_exoneration() {
        // The one distinction the whole reply exists to preserve: a cluster
        // that cannot judge and a cluster that judged and found nothing are
        // opposite operational situations.
        let mut queue = ReportQueue::default();
        let key = iroh_base::SecretKey::from_bytes(&[7; 32]);
        queue.record_outcome(ReportOutcome {
            subject: key.public(),
            entity: PersistId::new(1),
            window_end: Tick::new(30),
            verdict: Some(Verdict::Exonerates),
            reason: orrery_protocol::REPORT_ADJUDICATED,
        });
        queue.record_outcome(ReportOutcome {
            subject: key.public(),
            entity: PersistId::new(2),
            window_end: Tick::new(30),
            verdict: None,
            reason: orrery_protocol::REPORT_REFUSED_NO_ADJUDICATOR,
        });
        let outcomes = queue.take_outcomes();
        assert_eq!(outcomes.len(), 2);
        assert_eq!(outcomes[0].verdict, Some(Verdict::Exonerates));
        assert_eq!(outcomes[1].verdict, None);
        assert_eq!(
            outcomes[1].reason,
            orrery_protocol::REPORT_REFUSED_NO_ADJUDICATOR
        );
        assert!(queue.take_outcomes().is_empty(), "taking clears");
    }

    #[test]
    fn retained_outcomes_are_bounded() {
        // A game that never reads its verdicts must not grow the resource
        // without bound; a witness watching seven subjects produces these
        // continuously once shadow mode is off.
        let mut queue = ReportQueue::default();
        let key = iroh_base::SecretKey::from_bytes(&[7; 32]);
        for entity in 0..(RETAINED_OUTCOMES as u64 * 3) {
            queue.record_outcome(ReportOutcome {
                subject: key.public(),
                entity: PersistId::new(entity + 1),
                window_end: Tick::new(30),
                verdict: Some(Verdict::Exonerates),
                reason: orrery_protocol::REPORT_ADJUDICATED,
            });
        }
        assert_eq!(queue.outcomes().count(), RETAINED_OUTCOMES);
    }
}
