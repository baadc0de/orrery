//! The reconciliation-error monitor (docs/05-prediction-rollback.md §10, D10).
//!
//! Every rollback comparison already computes `|predicted − authoritative|` per
//! component per entity. Discarding that residual and then building a separate
//! validation apparatus to recompute it would be doing the work twice; this
//! module keeps it. A sustained band violation against one authority is exactly
//! D10's step-1 "prediction *is* the witness" trigger, and it costs nothing
//! that prediction was not already paying for.
//!
//! Three properties are load-bearing, and each exists to keep an honest player
//! from being accused:
//!
//! - **The arithmetic is integer, over the quantization lattice.** Residuals
//!   arrive in millimetres and millimetres per second, and the EWMA is an
//!   integer recurrence. A comparator that used floats could disagree between
//!   the peer that reports and the adjudicator that decides, which would make
//!   verdicts platform-dependent — the one property the tolerance bands exist
//!   to remove.
//! - **A violation must be sustained.** One noisy tick is packet loss or
//!   `libm` drift, not a cheat. The bands carry a sustain requirement (D16:
//!   250 ms, 15 ticks at 60 Hz) and an instantaneous escalation multiple for
//!   error too large to be either.
//! - **The monitor grades its own evidence.** A peer in the 250+ ms latency
//!   band, or one that just evicted predicted entities under the budget guard
//!   (docs/05 §3), is not in a position to say whose fault a residual is. It
//!   reports with reduced weight rather than staying silent, because the
//!   *pattern* across authorities is still informative.
//!
//! The bands are restated here rather than imported from `orrery_core`'s
//! `Tolerance`: docs/10-crates.md's layering rule 2 puts `orrery_core` below
//! only `orrery_witness`, `orrery_persistd`, `orrery_field_host` and games, and
//! `orrery_predict` is not on that list. Field names match `Tolerance`'s
//! exactly so `orrery_witness` — which depends on both — converts mechanically.

use std::collections::HashMap;
use std::ops::Range;

use bevy_ecs::prelude::*;
use orrery_protocol::{NodeId, PersistId, Tick};

/// Which authority's claim about which entity a residual is against.
///
/// Keyed by the *authority*, not just the entity, because the question the
/// monitor answers is "is this peer lying" and an entity that changes hands
/// mid-dispute must not carry its predecessor's error with it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TrackKey {
    /// The peer holding authority over `entity` when the residual was taken.
    pub authority: NodeId,
    /// The entity the residual is for.
    pub entity: PersistId,
}

/// D16's tolerance bands and the sustain rule, as the monitor applies them.
///
/// Mirrors `orrery_core::Tolerance` field for field; see the module docs for
/// why it is a mirror and not an import.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MonitorBands {
    /// Positional band, in millimetres. D16: 1 cm.
    pub eps_pos_mm: i64,
    /// Velocity band, in millimetres per second. D16: 1 cm/s.
    pub eps_vel_mms: i64,
    /// Ticks the error must exceed the band before it counts. D16: 250 ms,
    /// which is 15 ticks at 60 Hz.
    pub sustain_ticks: u32,
    /// Instantaneous escalation multiple: one tick this far outside the band
    /// is a violation with no sustain needed.
    pub hard_snap_multiple: i64,
}

impl Default for MonitorBands {
    fn default() -> Self {
        Self {
            eps_pos_mm: 10,
            eps_vel_mms: 10,
            sustain_ticks: 15,
            hard_snap_multiple: 8,
        }
    }
}

/// Why this peer's evidence carries less weight than a healthy peer's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DegradedReason {
    /// RTT to the authority is in the 250+ ms band (docs/05 §8): interaction
    /// prediction is off, so residuals reflect the link, not the authority.
    HighLatencyBand,
    /// The budget guard recently evicted predicted entities (docs/05 §3). A
    /// machine that cannot afford prediction cannot serve as a high-confidence
    /// witness.
    BudgetEviction,
}

/// How much weight the witness pipeline should give this peer's report.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WitnessConfidence {
    /// Healthy link, prediction running at full fidelity.
    #[default]
    Full,
    /// Reported, but discounted, for the given reason.
    Reduced(DegradedReason),
}

/// Per-`(authority, entity)` reconciliation statistics (docs/05 §10).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ErrorTrack {
    /// EWMA of positional residual, millimetres.
    pub pos_ewma_mm: i64,
    /// EWMA of velocity residual, millimetres per second.
    pub vel_ewma_mms: i64,
    /// The tick the current out-of-band run started, if one is open.
    pub violation_start: Option<Tick>,
    /// Ticks the current run has lasted.
    pub violation_ticks: u32,
    /// Rollbacks against this authority since the last [`ReconciliationMonitor::reset_counters`].
    pub rollbacks: u32,
    /// Snap-reconciles (updates that arrived past the rollback window).
    pub snaps: u32,
    /// The most recent tick a residual was recorded for.
    pub last_tick: Option<Tick>,
}

/// A signal the witness pipeline consumes (docs/05 §10; D10 step 1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MonitorSignal {
    /// ε_pos / ε_vel exceeded continuously for at least the sustain window.
    SustainedToleranceViolation {
        /// Whose claim is disputed, and about which entity.
        key: TrackKey,
        /// The disputed tick range: from where the trajectories parted to the
        /// tick the run was confirmed at. The *start* is the useful end — an
        /// adjudicator quoting the moment a counter happened to reach its
        /// threshold would point at a tick where nothing began.
        window: Range<Tick>,
        /// How much this peer's report should be weighted.
        confidence: WitnessConfidence,
    },
    /// A rollback storm against one authority while the others stay clean —
    /// evidence that the mispredict cause is remote rather than local jitter.
    AnomalousCorrectionPattern {
        /// The authority whose corrections stand out.
        authority: NodeId,
        /// Rollbacks attributed to it this epoch.
        rollbacks: u32,
        /// Mean rollbacks across the peer's *other* authorities, the baseline
        /// the storm stands out against.
        baseline: u32,
        /// How much this peer's report should be weighted.
        confidence: WitnessConfidence,
    },
}

/// Per-entity reconciliation error statistics (D10, docs/05 §10).
#[derive(Debug, Resource)]
pub struct ReconciliationMonitor {
    tracks: HashMap<TrackKey, ErrorTrack>,
    bands: MonitorBands,
    confidence: WitnessConfidence,
    /// Smoothing divisor for the residual EWMAs. Integer for the reason in the
    /// module docs.
    smoothing: i64,
    /// A storm is this many times the baseline…
    storm_factor: u32,
    /// …and at least this many rollbacks, so that 2-vs-0 is not a storm.
    storm_floor: u32,
}

impl ReconciliationMonitor {
    /// A monitor with explicit bands (a game retuning per docs/05 §12).
    #[must_use]
    pub fn with_bands(bands: MonitorBands) -> Self {
        Self {
            bands,
            ..Self::default()
        }
    }

    /// The bands in force.
    #[must_use]
    pub const fn bands(&self) -> MonitorBands {
        self.bands
    }

    /// The weight this peer's reports currently carry.
    #[must_use]
    pub const fn confidence(&self) -> WitnessConfidence {
        self.confidence
    }

    /// Mark this peer's evidence as degraded (docs/05 §10).
    ///
    /// Sticky until [`Self::restore_confidence`]: the point of the flag is
    /// that the adjudicator sees the peer's condition at report time, and a
    /// flag that cleared itself on the next healthy tick would routinely be
    /// clear by the time the report was assembled.
    pub fn degrade(&mut self, reason: DegradedReason) {
        self.confidence = WitnessConfidence::Reduced(reason);
    }

    /// Return this peer to full witness weight.
    pub fn restore_confidence(&mut self) {
        self.confidence = WitnessConfidence::Full;
    }

    /// Record one tick's residual against an authority's claim.
    ///
    /// `pos_err_mm` and `vel_err_mms` are magnitudes on the quantization
    /// lattice — the same bits the authority sent and this peer compared
    /// against, per docs/05 §9's quantize-both-sides rule. Feeding
    /// un-quantized state here would make every honest snapshot look like a
    /// small deviation.
    ///
    /// Returns a signal on the tick a violation first qualifies, and only on
    /// that tick: an open run does not re-fire every tick it stays open.
    pub fn record_residual(
        &mut self,
        key: TrackKey,
        tick: Tick,
        pos_err_mm: i64,
        vel_err_mms: i64,
    ) -> Option<MonitorSignal> {
        let smoothing = self.smoothing;
        let bands = self.bands;
        let confidence = self.confidence;
        let track = self.tracks.entry(key).or_default();

        if track.last_tick.is_none() {
            // Seed on the first sample rather than climbing to it from zero. A
            // fresh track under-reports for its first dozen samples otherwise,
            // and the case that matters most — a hard snap on the tick an
            // entity enters the predicted set — is exactly a first sample.
            track.pos_ewma_mm = pos_err_mm;
            track.vel_ewma_mms = vel_err_mms;
        } else {
            track.pos_ewma_mm = ewma(track.pos_ewma_mm, pos_err_mm, smoothing);
            track.vel_ewma_mms = ewma(track.vel_ewma_mms, vel_err_mms, smoothing);
        }
        track.last_tick = Some(tick);

        let out_of_band = pos_err_mm > bands.eps_pos_mm || vel_err_mms > bands.eps_vel_mms;
        let hard_snap = pos_err_mm > bands.eps_pos_mm.saturating_mul(bands.hard_snap_multiple)
            || vel_err_mms > bands.eps_vel_mms.saturating_mul(bands.hard_snap_multiple);

        if !out_of_band {
            track.violation_start = None;
            track.violation_ticks = 0;
            return None;
        }

        let start = *track.violation_start.get_or_insert(tick);
        // Already qualified on an earlier tick: the run is open, and the
        // witness has the report. Re-firing would turn one dispute into a
        // stream of duplicates for the adjudicator to deduplicate.
        let already_signalled = track.violation_ticks >= bands.sustain_ticks;
        track.violation_ticks = track.violation_ticks.saturating_add(1);

        if already_signalled {
            return None;
        }
        if hard_snap || track.violation_ticks >= bands.sustain_ticks {
            // Force the run to count as signalled even when a hard snap
            // short-circuits the sustain requirement.
            track.violation_ticks = track.violation_ticks.max(bands.sustain_ticks);
            return Some(MonitorSignal::SustainedToleranceViolation {
                key,
                window: start..Tick(tick.0.saturating_add(1)),
                confidence,
            });
        }
        None
    }

    /// Record that a rollback was performed against this authority's update.
    pub fn record_rollback(&mut self, key: TrackKey) {
        let t = self.tracks.entry(key).or_default();
        t.rollbacks = t.rollbacks.saturating_add(1);
    }

    /// Record a snap-reconcile: an update that arrived past the rollback
    /// window and could not be replayed.
    pub fn record_snap(&mut self, key: TrackKey) {
        let t = self.tracks.entry(key).or_default();
        t.snaps = t.snaps.saturating_add(1);
    }

    /// The track for a key, if one exists.
    #[must_use]
    pub fn track(&self, key: &TrackKey) -> Option<&ErrorTrack> {
        self.tracks.get(key)
    }

    /// How many tracks are open.
    #[must_use]
    pub fn len(&self) -> usize {
        self.tracks.len()
    }

    /// Whether any track is open.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.tracks.is_empty()
    }

    /// Look for the second D10 pattern: one authority correcting this peer far
    /// more than every other authority is.
    ///
    /// The comparison is against the peer's *own* other links deliberately.
    /// Absolute rollback rates say more about this machine's network than
    /// about any authority; the ratio between links on the same machine at the
    /// same moment does not.
    #[must_use]
    pub fn scan_correction_pattern(&self) -> Option<MonitorSignal> {
        let mut per_authority: HashMap<NodeId, u32> = HashMap::new();
        for (key, track) in &self.tracks {
            *per_authority.entry(key.authority).or_default() += track.rollbacks + track.snaps;
        }
        if per_authority.len() < 2 {
            // With one authority there is no baseline, and calling a single
            // link's corrections anomalous would accuse whoever the player
            // happened to be standing next to.
            return None;
        }

        let total: u64 = per_authority.values().map(|v| u64::from(*v)).sum();
        let (worst, worst_count) = per_authority
            .iter()
            .max_by_key(|(_, v)| **v)
            .map(|(k, v)| (*k, *v))?;
        let others = per_authority.len() as u64 - 1;
        let baseline = ((total - u64::from(worst_count)) / others) as u32;

        if worst_count >= self.storm_floor
            && u64::from(worst_count) >= u64::from(baseline) * u64::from(self.storm_factor)
        {
            return Some(MonitorSignal::AnomalousCorrectionPattern {
                authority: worst,
                rollbacks: worst_count,
                baseline,
                confidence: self.confidence,
            });
        }
        None
    }

    /// Clear the rollback/snap counters, keeping the residual EWMAs and any
    /// open violation run.
    ///
    /// Called at the witness epoch boundary: correction *counts* are only
    /// meaningful within an epoch, while an open violation run spans whatever
    /// it spans.
    pub fn reset_counters(&mut self) {
        for track in self.tracks.values_mut() {
            track.rollbacks = 0;
            track.snaps = 0;
        }
    }

    /// Drop tracks with no residual since `before` — an entity that left the
    /// interest set, or an authority that is gone.
    pub fn retire_stale(&mut self, before: Tick) {
        self.tracks
            .retain(|_, t| t.last_tick.is_some_and(|last| last >= before));
    }
}

impl Default for ReconciliationMonitor {
    fn default() -> Self {
        Self {
            tracks: HashMap::new(),
            bands: MonitorBands::default(),
            confidence: WitnessConfidence::Full,
            smoothing: 8,
            storm_factor: 4,
            storm_floor: 8,
        }
    }
}

/// Integer EWMA: `ewma += (sample - ewma) / n`, with a floor of one lattice
/// unit per step.
///
/// The floor is not cosmetic. Plain integer division stalls as soon as the gap
/// is smaller than `n`, so an average tracking a steady 64 mm error would settle
/// at 57 mm and stay there — the monitor would under-report every sustained
/// deviation by up to `n − 1` millimetres, permanently, in the direction that
/// favours the accused. One unit per sample converges exactly and then stops.
fn ewma(current: i64, sample: i64, n: i64) -> i64 {
    let n = n.max(1);
    let diff = sample.saturating_sub(current);
    if diff == 0 {
        return current;
    }
    let step = diff / n;
    current.saturating_add(if step == 0 { diff.signum() } else { step })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(n: u8) -> NodeId {
        let mut bytes = [0u8; 32];
        bytes[0] = 1;
        bytes[31] = n;
        // Not every 32-byte string is a valid compressed Edwards point; walk
        // until one is, so the fixture never depends on a lucky constant.
        for candidate in 0..=u8::MAX {
            bytes[30] = candidate;
            if let Ok(key) = NodeId::from_bytes(&bytes) {
                return key;
            }
        }
        panic!("no valid key found for discriminant {n}");
    }

    fn key(authority: u8, entity: u64) -> TrackKey {
        TrackKey {
            authority: node(authority),
            entity: PersistId(entity),
        }
    }

    /// One tick outside the band is packet loss or float drift, and accusing
    /// on it is the failure mode that kills witness-based trust (R-6).
    #[test]
    fn single_out_of_band_tick_is_not_a_violation() {
        let mut m = ReconciliationMonitor::default();
        assert_eq!(m.record_residual(key(1, 7), Tick(100), 50, 0), None);
    }

    /// A run that reaches the sustain window fires once, and the window it
    /// reports starts where the trajectories parted — not where the counter
    /// tripped.
    #[test]
    fn sustained_run_fires_once_and_reports_the_start_tick() {
        let mut m = ReconciliationMonitor::default();
        let k = key(1, 7);
        let mut fired = Vec::new();
        for t in 0..40u64 {
            if let Some(sig) = m.record_residual(k, Tick(500 + t), 50, 0) {
                fired.push((t, sig));
            }
        }
        assert_eq!(fired.len(), 1, "one run, one signal");
        let (t, sig) = fired[0].clone();
        assert_eq!(t, 14, "15 ticks of sustain at D16's default");
        match sig {
            MonitorSignal::SustainedToleranceViolation {
                window, key: got, ..
            } => {
                assert_eq!(got, k);
                assert_eq!(window.start, Tick(500), "the tick the run began");
                assert_eq!(window.end, Tick(515));
            }
            other => panic!("expected a sustained violation, got {other:?}"),
        }
    }

    /// Error too large to be drift does not wait out the sustain window: an
    /// entity teleporting 8 band-widths in one tick is not noise.
    #[test]
    fn hard_snap_short_circuits_the_sustain_window() {
        let mut m = ReconciliationMonitor::default();
        let sig = m.record_residual(key(2, 3), Tick(9), 10_000, 0);
        assert!(matches!(
            sig,
            Some(MonitorSignal::SustainedToleranceViolation { .. })
        ));
    }

    /// Recovery must close the run: a peer that came back inside the band and
    /// later drifts out again is starting a new dispute, not continuing an old
    /// one, and the adjudication window is capped at 3 s for exactly this
    /// reason.
    #[test]
    fn returning_inside_the_band_resets_the_run() {
        let mut m = ReconciliationMonitor::default();
        let k = key(1, 7);
        for t in 0..10u64 {
            m.record_residual(k, Tick(t), 50, 0);
        }
        m.record_residual(k, Tick(10), 1, 0);
        assert_eq!(m.track(&k).unwrap().violation_ticks, 0);
        assert_eq!(m.track(&k).unwrap().violation_start, None);
        for t in 11..25u64 {
            assert_eq!(
                m.record_residual(k, Tick(t), 50, 0),
                None,
                "tick {t} came too early to re-qualify"
            );
        }
        assert!(m.record_residual(k, Tick(25), 50, 0).is_some());
    }

    /// Velocity is a band of its own: a peer whose position tracks but whose
    /// velocity does not is describing motion it is not performing.
    #[test]
    fn velocity_band_violates_independently_of_position() {
        let mut m = ReconciliationMonitor::default();
        let k = key(1, 7);
        let mut last = None;
        for t in 0..20u64 {
            last = m.record_residual(k, Tick(t), 0, 40);
        }
        assert!(last.is_none(), "already signalled");
        assert!(m.track(&k).unwrap().violation_ticks >= 15);
    }

    /// With a single authority there is no baseline; calling its corrections
    /// anomalous would accuse whoever the player happened to be next to.
    #[test]
    fn one_authority_is_never_a_storm() {
        let mut m = ReconciliationMonitor::default();
        for _ in 0..100 {
            m.record_rollback(key(1, 7));
        }
        assert_eq!(m.scan_correction_pattern(), None);
    }

    /// A storm is a ratio against the peer's other links at the same moment —
    /// which is what separates "that authority is misbehaving" from "this
    /// machine's network is bad".
    #[test]
    fn one_noisy_authority_among_clean_ones_is_a_storm() {
        let mut m = ReconciliationMonitor::default();
        for _ in 0..40 {
            m.record_rollback(key(1, 7));
        }
        for _ in 0..2 {
            m.record_rollback(key(2, 8));
            m.record_rollback(key(3, 9));
        }
        match m.scan_correction_pattern() {
            Some(MonitorSignal::AnomalousCorrectionPattern {
                authority,
                rollbacks,
                baseline,
                ..
            }) => {
                assert_eq!(authority, node(1));
                assert_eq!(rollbacks, 40);
                assert_eq!(baseline, 2);
            }
            other => panic!("expected a storm, got {other:?}"),
        }
    }

    /// Every link being equally noisy is a bad connection, not an accusation.
    #[test]
    fn uniformly_noisy_links_are_not_a_storm() {
        let mut m = ReconciliationMonitor::default();
        for i in 1..4u8 {
            for _ in 0..30 {
                m.record_rollback(key(i, u64::from(i)));
            }
        }
        assert_eq!(m.scan_correction_pattern(), None);
    }

    /// A degraded peer still reports — the pattern across authorities remains
    /// informative — but the signal must carry the discount, because an
    /// adjudicator that cannot see the reporter's condition will weigh a
    /// 300 ms link's residuals like a LAN peer's.
    #[test]
    fn degraded_confidence_rides_on_the_signal() {
        let mut m = ReconciliationMonitor::default();
        m.degrade(DegradedReason::BudgetEviction);
        let mut sig = None;
        for t in 0..20u64 {
            if let Some(s) = m.record_residual(key(1, 7), Tick(t), 50, 0) {
                sig = Some(s);
            }
        }
        match sig {
            Some(MonitorSignal::SustainedToleranceViolation { confidence, .. }) => assert_eq!(
                confidence,
                WitnessConfidence::Reduced(DegradedReason::BudgetEviction)
            ),
            other => panic!("expected a discounted violation, got {other:?}"),
        }
        m.restore_confidence();
        assert_eq!(m.confidence(), WitnessConfidence::Full);
    }

    /// The EWMA is what a report quotes as "how far off, typically"; it must
    /// converge on the residual rather than tracking the last sample.
    #[test]
    fn residual_ewma_converges_on_the_sustained_error() {
        let mut m = ReconciliationMonitor::default();
        let k = key(1, 7);
        for t in 0..200u64 {
            m.record_residual(k, Tick(t), 64, 32);
        }
        let track = *m.track(&k).unwrap();
        assert!((60..=64).contains(&track.pos_ewma_mm), "{track:?}");
        assert!((28..=32).contains(&track.vel_ewma_mms), "{track:?}");
    }

    /// Tracks for entities that left the interest set must not accumulate:
    /// the monitor lives for a session, and a per-entity map that never
    /// shrinks is a leak proportional to how far the player travelled.
    #[test]
    fn stale_tracks_are_retired() {
        let mut m = ReconciliationMonitor::default();
        m.record_residual(key(1, 7), Tick(10), 0, 0);
        m.record_residual(key(1, 8), Tick(400), 0, 0);
        m.retire_stale(Tick(100));
        assert_eq!(m.len(), 1);
        assert!(m.track(&key(1, 8)).is_some());
    }

    /// Epoch counter resets clear corrections without discarding an open
    /// dispute — the run spans whatever it spans.
    #[test]
    fn counter_reset_keeps_an_open_violation_run() {
        let mut m = ReconciliationMonitor::default();
        let k = key(1, 7);
        for t in 0..10u64 {
            m.record_residual(k, Tick(t), 50, 0);
        }
        m.record_rollback(k);
        m.reset_counters();
        assert_eq!(m.track(&k).unwrap().rollbacks, 0);
        assert_eq!(m.track(&k).unwrap().violation_ticks, 10);
    }
}
