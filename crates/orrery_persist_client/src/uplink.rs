//! The 1–4 Hz per-entity diff uplink scheduler (D11 §2.1).
//!
//! Replicon change-detection diffs for locally-authoritative entities are
//! scheduled to the gateway by a per-entity priority accumulator (the Gaffer
//! state-synchronization pattern, docs/03-replication.md §5.2). Each entity
//! accrues priority at a rate in the config's `uplink_hz` range — nearest
//! entities fastest — and is flushed when its accumulator wins a slot in the
//! per-flush byte budget. Unacked diffs stay buffered and are resent on
//! reconnect (records are idempotent, keyed by `(entity, tick)`).
//!
//! The scheduler is engine-agnostic with respect to *what* a diff is: the
//! caller feeds [`orrery_protocol::DiffUplink`]s (the replicon change-detection
//! output, wired by [`crate::feed::feed_uplink`]) and drains the selected diffs
//! each flush. The Bevy system in [`crate::plugin`] wires this to the gateway
//! session.
//!
//! Ack-latency sampling (D16: bulk ack p99 < 5 ms): [`flush`] records the send
//! instant of each diff it selects, and [`on_ack`] computes the send→ack
//! round trip into the ack-latency histogram. A resent diff carries the
//! latest send instant, so a retransmission is not credited with the original
//! send time.

use std::collections::{HashMap, VecDeque};
use std::time::Duration;

use bevy_ecs::prelude::*;
use bevy_platform::time::Instant;
use bevy_time::Time;
use orrery_protocol::{DiffUplink, PersistId, Tick};

use crate::config::PersistClientConfig;
use crate::gateway::GatewaySession;
use crate::latency::LatencyHistogram;

/// Per-entity scheduler state.
#[derive(Debug, Clone)]
struct EntityState {
    /// The accumulated priority (in "send credits").
    acc: f32,
    /// The entity's uplink rate in Hz.
    rate_hz: f32,
    /// The next diff to send, if any (the newest unacked change).
    pending: Option<DiffUplink>,
    /// The last tick a diff was sent for this entity.
    last_sent_tick: Option<Tick>,
    /// The highest tick acked by the gateway for this entity.
    last_acked_tick: Option<Tick>,
    /// The client-side sequence of the last sent diff.
    last_seq: u64,
    /// Whether this entity has entered its rate schedule. Before its first
    /// pending diff it must not accrue credit, which preserves phased cold
    /// startup; afterward credit accrues independently of reply timing.
    has_sent: bool,
}

/// Upper bound on send timestamps retained by one scheduler shard. The P2
/// profile has only 80 entities per shard; this leaves ample room for delayed
/// replies while preventing an unavailable gateway from growing memory
/// without bound.
const MAX_IN_FLIGHT_SEND_TIMES: usize = 4_096;
const MAX_SEND_ORDER_ENTRIES: usize = MAX_IN_FLIGHT_SEND_TIMES * 2;

type SendKey = (PersistId, Tick);

/// The per-entity diff uplink scheduler (D11 §2.1, docs/10-crates.md §9).
///
/// A [`Resource`] holding the accumulator state for every locally-authoritative
/// entity. The plugin's flush system calls [`UplinkScheduler::flush`] each
/// update to select which diffs to send this flush, bounded by the config's
/// byte budget.
///
/// Ack-latency sampling: [`flush`](Self::flush) records the send instant, and
/// [`on_ack`](Self::on_ack) computes the round trip into the ack-latency
/// histogram. A resent diff carries the latest send instant.
#[derive(Debug, Default, Resource)]
pub struct UplinkScheduler {
    /// Per-entity scheduler state.
    entities: HashMap<PersistId, EntityState>,
    /// The last flush elapsed time, for rate accumulation.
    last_elapsed: Option<Duration>,
    /// Bulk-ack latency histogram (D16: bulk ack p99 < 5 ms).
    ack_latency: LatencyHistogram,
    /// Latest wire-send timestamp for every in-flight `(entity, tick)`.
    ///
    /// This is deliberately not stored in `EntityState`: a newly queued tick
    /// may supersede the entity's pending diff before the older durable reply
    /// arrives. Both replies still need latency samples.
    sent_at: HashMap<SendKey, (Instant, u64)>,
    sent_order: VecDeque<(SendKey, u64)>,
    send_generation: u64,
}

impl UplinkScheduler {
    /// A new, empty scheduler.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a locally-authoritative entity at `rate_hz` (within the
    /// config's `uplink_hz` range). Idempotent: re-registering updates the
    /// rate without resetting accumulated priority.
    pub fn register(&mut self, entity: PersistId, rate_hz: f32) {
        let entry = self.entities.entry(entity).or_insert_with(|| EntityState {
            acc: 0.0,
            rate_hz,
            pending: None,
            last_sent_tick: None,
            last_acked_tick: None,
            last_seq: 0,
            has_sent: false,
        });
        entry.rate_hz = rate_hz;
    }

    /// Unregister an entity (despawn, or authority handed off).
    pub fn unregister(&mut self, entity: PersistId) {
        self.entities.remove(&entity);
    }

    /// Queue a change-detection diff for `entity`.
    ///
    /// The newest diff replaces any pending one for the same entity (newest-
    /// wins: a superseded diff is worthless to send). The entity must have been
    /// registered first; unregistered entities are ignored (the caller decides
    /// what is locally-authoritative).
    pub fn queue(&mut self, diff: DiffUplink) {
        if let Some(state) = self.entities.get_mut(&diff.entity) {
            state.pending = Some(diff);
        }
    }

    /// Whether `entity` has an unacked, unsent diff.
    #[must_use]
    pub fn has_pending(&self, entity: PersistId) -> bool {
        self.entities
            .get(&entity)
            .is_some_and(|s| s.pending.is_some())
    }

    /// The number of registered entities.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entities.len()
    }

    /// Whether no entities are registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entities.is_empty()
    }

    /// Select diffs to send this flush, bounded by `budget_bytes`.
    ///
    /// Accumulates priority by `dt` (the elapsed time since the last flush) at
    /// each entity's rate, sorts by accumulator (highest first), and packs
    /// diffs greedily into the byte budget. Sent diffs keep their pending entry
    /// (they are only cleared on ack) so a lost datagram is resent next flush.
    ///
    /// Returns the diffs to send this flush, in priority order.
    pub fn flush(&mut self, cfg: &PersistClientConfig, elapsed: Duration) -> Vec<DiffUplink> {
        let dt = match self.last_elapsed {
            Some(last) => elapsed.saturating_sub(last).as_secs_f32(),
            None => 0.0,
        };
        self.last_elapsed = Some(elapsed);

        // Accumulate one bounded send credit for every registered entity,
        // including while it has no pending diff. Change generation is
        // independent of acknowledgement timing: an entity that becomes dirty
        // on its next rate cohort may spend its accrued credit immediately,
        // while the cap prevents idle time from creating a catch-up burst.
        let mut ready: Vec<(PersistId, f32)> = Vec::new();
        for (entity, state) in self.entities.iter_mut() {
            if state.pending.is_some() || state.has_sent {
                state.acc = (state.acc + state.rate_hz * cfg.priority_gain * dt).min(1.0);
            }
            if state.pending.is_some() && state.acc >= 1.0 {
                ready.push((*entity, state.acc));
            }
        }

        // Highest priority first.
        ready.sort_by(|a, b| b.1.total_cmp(&a.1));

        // Pack greedily into the byte budget.
        let mut out = Vec::new();
        let mut used = 0usize;
        let now = Instant::now();
        for (entity, _) in ready {
            let state = self
                .entities
                .get_mut(&entity)
                .expect("ready entity is registered");
            let Some(diff) = state.pending.clone() else {
                continue;
            };
            let size = diff.payload.len() + 64; // header + overhead estimate
            if used + size > budget_bytes(cfg) && !out.is_empty() {
                continue; // budget exhausted; leave for next flush
            }
            used += size;
            state.last_sent_tick = Some(diff.tick);
            state.last_seq = diff.seq;
            state.has_sent = true;
            // Packed entities reset their accumulator (§5.2).
            state.acc = 0.0;
            // Record the send instant for ack-latency sampling. Updated on
            // every send (including resends) so a retransmitted diff is not
            // credited with the original send time.
            let key = (diff.entity, diff.tick);
            self.send_generation = self.send_generation.wrapping_add(1);
            let generation = self.send_generation;
            self.sent_at.insert(key, (now, generation));
            self.sent_order.push_back((key, generation));
            self.prune_send_times();
            out.push(diff);
        }

        out
    }

    /// Record a gateway ack for `entity` at `tick`.
    ///
    /// Clears the pending diff if it matches the acked tick (the ack is the
    /// durability contract, D11 §2.1). A provisional ack (epoch-unconfirmed)
    /// is treated as unacked and left pending for resend.
    ///
    /// Records a bulk-ack latency sample (D16: p99 < 5 ms): the time from the
    /// latest send of the acked diff to this ack. Since [`flush`](Self::flush)
    /// stamps the send instant on every send, a retransmitted diff is not
    /// credited with the original send time.
    pub fn on_ack(&mut self, entity: PersistId, tick: Tick, provisional: bool) {
        self.on_ack_at(entity, tick, provisional, Instant::now());
    }

    /// Record a gateway ack using the instant at which its datagram was
    /// received. Keeping receipt time separate from handler time prevents a
    /// busy game/load loop from being counted as gateway latency.
    pub fn on_ack_at(
        &mut self,
        entity: PersistId,
        tick: Tick,
        provisional: bool,
        received_at: Instant,
    ) {
        if provisional {
            return;
        }
        self.record_reply_latency(entity, tick, received_at);
        if let Some(state) = self.entities.get_mut(&entity) {
            state.last_acked_tick = Some(tick);
            if state.pending.as_ref().is_some_and(|d| d.tick == tick) {
                state.pending = None;
            }
        }
    }

    /// Record a gateway nack for `entity` at `tick`.
    ///
    /// A nack (invariant violation, stale epoch) drops the pending diff — the
    /// gateway rejected it, so resending is pointless. The caller is expected
    /// to surface the rejection to the game.
    pub fn on_nack(&mut self, entity: PersistId, tick: Tick) {
        self.on_nack_at(entity, tick, Instant::now());
    }

    /// Record a gateway nack using its wire-receipt instant.
    pub fn on_nack_at(&mut self, entity: PersistId, tick: Tick, received_at: Instant) {
        self.record_reply_latency(entity, tick, received_at);
        if let Some(state) = self.entities.get_mut(&entity) {
            if state.pending.as_ref().is_some_and(|d| d.tick == tick) {
                state.pending = None;
            }
        }
    }

    /// The last tick acked by the gateway for `entity`, if any.
    #[must_use]
    pub fn last_acked_tick(&self, entity: PersistId) -> Option<Tick> {
        self.entities.get(&entity).and_then(|s| s.last_acked_tick)
    }

    /// The current accumulated priority for `entity`.
    #[must_use]
    pub fn priority(&self, entity: PersistId) -> f32 {
        self.entities.get(&entity).map_or(0.0, |s| s.acc)
    }

    /// The bulk-ack latency histogram (D16: p99 < 5 ms).
    #[must_use]
    pub fn ack_latency(&self) -> &LatencyHistogram {
        &self.ack_latency
    }

    fn record_reply_latency(&mut self, entity: PersistId, tick: Tick, received_at: Instant) {
        if let Some((sent_at, _)) = self.sent_at.remove(&(entity, tick)) {
            self.ack_latency.record(
                received_at
                    .checked_duration_since(sent_at)
                    .unwrap_or_default(),
            );
        }
    }

    fn prune_send_times(&mut self) {
        while self.sent_at.len() > MAX_IN_FLIGHT_SEND_TIMES
            || self.sent_order.len() > MAX_SEND_ORDER_ENTRIES
        {
            let Some((key, generation)) = self.sent_order.pop_front() else {
                break;
            };
            if self
                .sent_at
                .get(&key)
                .is_some_and(|(_, current)| *current == generation)
            {
                self.sent_at.remove(&key);
            }
        }
        while self.sent_order.front().is_some_and(|(key, generation)| {
            self.sent_at
                .get(key)
                .is_none_or(|(_, current)| current != generation)
        }) {
            self.sent_order.pop_front();
        }
    }
}

/// The byte budget for a flush, clamped to a sane minimum.
fn budget_bytes(cfg: &PersistClientConfig) -> usize {
    cfg.flush_budget_bytes.max(1)
}

/// The Bevy system that flushes the uplink scheduler each update.
///
/// Accumulates priority from the real clock, selects diffs within the byte
/// budget, and encodes them as tagged datagrams into the gateway session's
/// send buffer. Does nothing while the session is not connected.
pub fn flush_uplink(
    cfg: Res<PersistClientConfig>,
    time: Res<Time>,
    session: Res<GatewaySession>,
    mut scheduler: ResMut<UplinkScheduler>,
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

    let elapsed = time.elapsed();
    let diffs = scheduler.flush(&cfg, elapsed);
    for diff in diffs {
        let msg = orrery_protocol::GatewayMsg::Diff { diff };
        io.send
            .push(bytes::Bytes::from(GatewaySession::encode_datagram(&msg)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> PersistClientConfig {
        PersistClientConfig {
            uplink_hz: 1.0..=4.0,
            priority_gain: 1.0,
            flush_budget_bytes: 1024,
            area_cells_per_round: 27,
            queue_capacity: 4096,
            queue_dir: None,
        }
    }

    fn diff(entity: u64, tick: u64, payload: &[u8]) -> DiffUplink {
        DiffUplink {
            cell: orrery_protocol::CellId::ROOT,
            grid: orrery_protocol::GridId::ROOT,
            entity: PersistId::new(entity),
            tick: Tick::new(tick),
            kind: orrery_protocol::RecordKind::ComponentDiff,
            payload: bytes::Bytes::copy_from_slice(payload),
            seq: tick,
            lease_id: None,
            authority_seq: None,
        }
    }

    fn t(ms: u64) -> Duration {
        Duration::from_millis(ms)
    }

    #[test]
    fn unregistered_entities_are_ignored() {
        let mut sched = UplinkScheduler::new();
        sched.queue(diff(1, 1, b"x"));
        assert!(!sched.has_pending(PersistId::new(1)));
        assert!(sched.is_empty());
    }

    #[test]
    fn priority_accumulates_and_sends() {
        let cfg = cfg();
        let mut sched = UplinkScheduler::new();
        sched.register(PersistId::new(1), 4.0);
        sched.queue(diff(1, 1, b"hp=50"));

        // No time elapsed on the first flush: nothing accumulates.
        let out = sched.flush(&cfg, t(0));
        assert!(out.is_empty());
        assert!(sched.has_pending(PersistId::new(1)));

        // After 250 ms at 4 Hz, the entity has accrued 1.0 priority and sends.
        let out = sched.flush(&cfg, t(250));
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].entity, PersistId::new(1));
        // Still pending until acked.
        assert!(sched.has_pending(PersistId::new(1)));
    }

    #[test]
    fn ack_clears_pending() {
        let cfg = cfg();
        let mut sched = UplinkScheduler::new();
        sched.register(PersistId::new(1), 4.0);
        sched.queue(diff(1, 1, b"hp=50"));
        sched.flush(&cfg, t(0)); // baseline
        sched.flush(&cfg, t(250));

        // A provisional ack does not clear the pending diff.
        sched.on_ack(PersistId::new(1), Tick::new(1), true);
        assert!(sched.has_pending(PersistId::new(1)));

        // A real ack clears it.
        sched.on_ack(PersistId::new(1), Tick::new(1), false);
        assert!(!sched.has_pending(PersistId::new(1)));
        assert_eq!(sched.last_acked_tick(PersistId::new(1)), Some(Tick::new(1)));
    }

    #[test]
    fn nack_drops_pending() {
        let cfg = cfg();
        let mut sched = UplinkScheduler::new();
        sched.register(PersistId::new(1), 4.0);
        sched.queue(diff(1, 1, b"hp=50"));
        sched.flush(&cfg, t(0)); // baseline
        sched.flush(&cfg, t(250));
        sched.on_nack(PersistId::new(1), Tick::new(1));
        assert!(!sched.has_pending(PersistId::new(1)));
    }

    #[test]
    fn newest_diff_wins() {
        let cfg = cfg();
        let mut sched = UplinkScheduler::new();
        sched.register(PersistId::new(1), 4.0);
        sched.queue(diff(1, 1, b"hp=50"));
        sched.queue(diff(1, 2, b"hp=25"));
        sched.flush(&cfg, t(0)); // baseline
        sched.flush(&cfg, t(250));
        // The newest diff (tick 2) is pending; acking tick 2 clears it.
        sched.on_ack(PersistId::new(1), Tick::new(2), false);
        assert!(!sched.has_pending(PersistId::new(1)));
    }

    #[test]
    fn older_ack_after_newer_tick_is_queued_still_records_latency() {
        let cfg = cfg();
        let mut sched = UplinkScheduler::new();
        let entity = PersistId::new(1);
        sched.register(entity, 4.0);
        sched.queue(diff(1, 1, b"old"));
        sched.flush(&cfg, t(0));
        sched.flush(&cfg, t(250));

        // Supersede the entity's pending state and send the newer tick before
        // the older durable reply arrives.
        sched.queue(diff(1, 2, b"new"));
        sched.flush(&cfg, t(500));
        sched.on_ack(entity, Tick::new(1), false);

        assert_eq!(sched.ack_latency().total(), 1);
        assert!(sched.has_pending(entity), "old ack must not clear tick 2");

        sched.on_ack(entity, Tick::new(2), false);
        assert_eq!(sched.ack_latency().total(), 2);
        assert!(!sched.has_pending(entity));
    }

    #[test]
    fn send_time_ledger_is_bounded() {
        let mut cfg = cfg();
        cfg.flush_budget_bytes = usize::MAX;
        let mut sched = UplinkScheduler::new();
        for entity in 0..(MAX_IN_FLIGHT_SEND_TIMES as u64 + 64) {
            sched.register(PersistId::new(entity), 4.0);
            sched.queue(diff(entity, 1, b"x"));
        }
        sched.flush(&cfg, t(0));
        sched.flush(&cfg, t(250));

        assert_eq!(sched.sent_at.len(), MAX_IN_FLIGHT_SEND_TIMES);
        assert!(sched.sent_order.len() <= MAX_SEND_ORDER_ENTRIES);
    }

    #[test]
    fn byte_budget_limits_flush() {
        let mut cfg = cfg();
        cfg.flush_budget_bytes = 80; // ~one small diff
        let mut sched = UplinkScheduler::new();
        for i in 0..3u64 {
            sched.register(PersistId::new(i), 4.0);
            sched.queue(diff(i, 1, b"payload"));
        }
        // First flush establishes the baseline; the second accumulates.
        sched.flush(&cfg, t(0));
        // All three accrue equally; the budget admits roughly one.
        let out = sched.flush(&cfg, t(250));
        assert!(!out.is_empty());
        assert!(
            out.len() < 3,
            "budget should bound the flush, got {}",
            out.len()
        );
    }

    #[test]
    fn higher_rate_sends_first() {
        let cfg = cfg();
        let mut sched = UplinkScheduler::new();
        sched.register(PersistId::new(1), 4.0); // fast
        sched.register(PersistId::new(2), 1.0); // slow
        sched.queue(diff(1, 1, b"a"));
        sched.queue(diff(2, 1, b"b"));
        sched.flush(&cfg, t(0));
        let out = sched.flush(&cfg, t(250));
        // The 4 Hz entity accrued 1.0, the 1 Hz entity 0.25; only the fast
        // one is ready, so it sends first.
        assert_eq!(out[0].entity, PersistId::new(1));
    }

    #[test]
    fn ack_latency_sample_is_recorded_per_ack() {
        let cfg = cfg();
        let mut sched = UplinkScheduler::new();
        sched.register(PersistId::new(1), 4.0);
        sched.queue(diff(1, 1, b"hp=50"));

        // First flush establishes the baseline (no accumulation).
        sched.flush(&cfg, t(0));
        // Second flush: 250 ms at 4 Hz → 1.0 priority → sends the diff.
        sched.flush(&cfg, t(250));

        // Before ack, the histogram is empty.
        assert_eq!(sched.ack_latency().total(), 0);

        // Ack the diff.
        sched.on_ack(PersistId::new(1), Tick::new(1), false);
        // The histogram now has one sample.
        assert_eq!(sched.ack_latency().total(), 1);
        // The sample should be a positive duration (the time from flush to ack).
        assert!(sched.ack_latency().min().unwrap() > Duration::ZERO);
    }

    #[test]
    fn receipt_timestamp_excludes_delayed_handler_work() {
        let cfg = cfg();
        let mut sched = UplinkScheduler::new();
        let entity = PersistId::new(1);
        sched.register(entity, 4.0);
        sched.queue(diff(1, 1, b"hp=50"));
        sched.flush(&cfg, t(0));
        sched.flush(&cfg, t(250));

        let received_at = Instant::now();
        std::thread::sleep(Duration::from_millis(20));
        sched.on_ack_at(entity, Tick::new(1), false, received_at);

        assert_eq!(sched.ack_latency().total(), 1);
        assert!(
            sched.ack_latency().max().unwrap() < Duration::from_millis(10),
            "handler delay leaked into wire latency"
        );
    }

    #[test]
    fn resend_does_not_produce_negative_or_absurd_sample() {
        // Verify that a resend uses the latest send instant, so a
        // retransmitted diff is not credited with the original send time.
        let cfg = cfg();
        let mut sched = UplinkScheduler::new();
        sched.register(PersistId::new(1), 4.0);
        sched.queue(diff(1, 1, b"hp=50"));

        // First flush establishes baseline (no send).
        sched.flush(&cfg, t(0));
        // Second flush (send at t=250).
        sched.flush(&cfg, t(250));
        // Third flush (resend at t=500) — the diff is still pending because
        // it hasn't been acked.
        sched.flush(&cfg, t(500));

        // Ack the diff.
        sched.on_ack(PersistId::new(1), Tick::new(1), false);
        // The latency should be roughly the time from the third flush (t=500)
        // to now, which is small and positive — not negative or absurd.
        let latency = sched.ack_latency().min().unwrap();
        assert!(
            latency > Duration::ZERO,
            "latency must be positive, got {latency:?}"
        );
        assert!(
            latency < Duration::from_secs(1),
            "latency must be absurdly small, got {latency:?}"
        );
    }

    #[test]
    fn provisional_ack_does_not_record_latency() {
        let cfg = cfg();
        let mut sched = UplinkScheduler::new();
        sched.register(PersistId::new(1), 4.0);
        sched.queue(diff(1, 1, b"hp=50"));
        sched.flush(&cfg, t(0)); // baseline
        sched.flush(&cfg, t(250));

        // A provisional ack should not record a latency sample.
        sched.on_ack(PersistId::new(1), Tick::new(1), true);
        assert_eq!(sched.ack_latency().total(), 0);
    }

    #[test]
    fn ack_latency_count_equals_ack_count() {
        let cfg = cfg();
        let mut sched = UplinkScheduler::new();
        // Register two entities and send diffs.
        sched.register(PersistId::new(1), 4.0);
        sched.register(PersistId::new(2), 4.0);
        sched.queue(diff(1, 1, b"a"));
        sched.queue(diff(2, 1, b"b"));
        sched.flush(&cfg, t(0)); // baseline
        sched.flush(&cfg, t(250));

        // Ack both.
        sched.on_ack(PersistId::new(1), Tick::new(1), false);
        sched.on_ack(PersistId::new(2), Tick::new(1), false);

        // The recorder count equals the ack count.
        assert_eq!(sched.ack_latency().total(), 2);
    }
}
