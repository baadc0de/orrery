//! Changed-byte measurements for A19's replication delta design.
//!
//! This module observes canonical bodies at the existing send cadence. It does
//! not encode a delta or change anything sent on the wire: lane 1 exists to
//! measure the input distribution before later lanes choose that grammar.

use std::collections::BTreeMap;

use serde::Serialize;

use orrery_games::regolith::state::RegolithState;
use orrery_protocol::PersistId;

/// Histograms for every canonical body type observed during a run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DeltaStatsReport {
    /// Sender-clocked reference cadence modeled by this observation.
    pub keyframe_hz: u64,
    /// Body types actually present in this run. Ordinary nightly legs contain
    /// only `craft`; campaign runs can add the other canonical variants.
    pub body_types: BTreeMap<&'static str, BodyDeltaStatsReport>,
}

/// Both changed-byte comparisons for one canonical body type.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BodyDeltaStatsReport {
    /// Canonical body lengths seen for this type, including its variant tag.
    pub body_bytes: Vec<usize>,
    /// Changed bytes against this entity's preceding send.
    pub vs_previous_send: ChangedBytesHistogram,
    /// Changed bytes on non-keyframe sends against the most recent modeled
    /// 1 Hz keyframe.
    pub vs_keyframe: ChangedBytesHistogram,
}

/// A changed-byte distribution and its directly derived summary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ChangedBytesHistogram {
    /// Number of body comparisons in the histogram.
    pub samples: u64,
    /// Changed byte count to observations with that count.
    pub histogram: BTreeMap<usize, u64>,
    /// Nearest-rank 50th percentile.
    pub p50: usize,
    /// Nearest-rank 95th percentile.
    pub p95: usize,
    /// Largest observed changed byte count.
    pub max: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum BodyType {
    Craft,
    Rock,
    Pickup,
    BloomDirector,
}

impl BodyType {
    const fn of(state: &RegolithState) -> Self {
        match state {
            RegolithState::Craft(_) => Self::Craft,
            RegolithState::Rock(_) => Self::Rock,
            RegolithState::Pickup(_) => Self::Pickup,
            RegolithState::BloomDirector(_) => Self::BloomDirector,
        }
    }

    const fn name(self) -> &'static str {
        match self {
            Self::Craft => "craft",
            Self::Rock => "rock",
            Self::Pickup => "pickup",
            Self::BloomDirector => "bloom_director",
        }
    }
}

#[derive(Debug, Clone)]
struct EntityBaseline {
    body_type: BodyType,
    previous: Vec<u8>,
    keyframe: Vec<u8>,
    sends_since_keyframe: u64,
}

#[derive(Debug, Clone, Default)]
struct BodyHistograms {
    body_bytes: BTreeMap<usize, ()>,
    vs_previous_send: Vec<u64>,
    vs_keyframe: Vec<u64>,
}

/// Per-bot accumulator. Baselines stay local to an authority; only completed
/// histograms are merged when the swarm report is assembled.
#[derive(Debug, Clone)]
pub(crate) struct DeltaStats {
    keyframe_every_sends: u64,
    entities: BTreeMap<PersistId, EntityBaseline>,
    body_types: BTreeMap<BodyType, BodyHistograms>,
}

impl DeltaStats {
    pub(crate) fn new(send_hz: u64) -> Self {
        Self {
            keyframe_every_sends: send_hz.max(1),
            entities: BTreeMap::new(),
            body_types: BTreeMap::new(),
        }
    }

    /// Observe one canonical body at the point the existing send path encodes
    /// it. Keyframe sends remain absolute in A19's model, so they contribute to
    /// the previous-send distribution but not the keyframe-delta distribution.
    pub(crate) fn observe(&mut self, entity: PersistId, state: &RegolithState, canonical: &[u8]) {
        let body_type = BodyType::of(state);
        self.observe_bytes(entity, body_type, canonical);
    }

    fn observe_bytes(&mut self, entity: PersistId, body_type: BodyType, canonical: &[u8]) {
        let histograms = self.body_types.entry(body_type).or_default();
        histograms.body_bytes.insert(canonical.len(), ());

        let Some(baseline) = self.entities.get_mut(&entity) else {
            self.entities.insert(
                entity,
                EntityBaseline {
                    body_type,
                    previous: canonical.to_vec(),
                    keyframe: canonical.to_vec(),
                    sends_since_keyframe: 0,
                },
            );
            return;
        };

        // An entity changing canonical variant is a new body stream, not a
        // meaningful byte delta between two unrelated grammars.
        if baseline.body_type != body_type {
            *baseline = EntityBaseline {
                body_type,
                previous: canonical.to_vec(),
                keyframe: canonical.to_vec(),
                sends_since_keyframe: 0,
            };
            return;
        }

        record(
            &mut histograms.vs_previous_send,
            changed_bytes(&baseline.previous, canonical),
        );
        baseline.sends_since_keyframe += 1;
        if baseline.sends_since_keyframe == self.keyframe_every_sends {
            baseline.keyframe.clear();
            baseline.keyframe.extend_from_slice(canonical);
            baseline.sends_since_keyframe = 0;
        } else {
            record(
                &mut histograms.vs_keyframe,
                changed_bytes(&baseline.keyframe, canonical),
            );
        }
        baseline.previous.clear();
        baseline.previous.extend_from_slice(canonical);
    }

    pub(crate) fn merge(&mut self, other: &Self) {
        debug_assert_eq!(self.keyframe_every_sends, other.keyframe_every_sends);
        for (body_type, other_histograms) in &other.body_types {
            let histograms = self.body_types.entry(*body_type).or_default();
            histograms.body_bytes.extend(
                other_histograms
                    .body_bytes
                    .keys()
                    .map(|length| (*length, ())),
            );
            merge_counts(
                &mut histograms.vs_previous_send,
                &other_histograms.vs_previous_send,
            );
            merge_counts(&mut histograms.vs_keyframe, &other_histograms.vs_keyframe);
        }
    }

    pub(crate) fn report(&self) -> DeltaStatsReport {
        DeltaStatsReport {
            keyframe_hz: 1,
            body_types: self
                .body_types
                .iter()
                .map(|(body_type, histograms)| {
                    (
                        body_type.name(),
                        BodyDeltaStatsReport {
                            body_bytes: histograms.body_bytes.keys().copied().collect(),
                            vs_previous_send: summarize(&histograms.vs_previous_send),
                            vs_keyframe: summarize(&histograms.vs_keyframe),
                        },
                    )
                })
                .collect(),
        }
    }
}

fn changed_bytes(baseline: &[u8], current: &[u8]) -> usize {
    baseline
        .iter()
        .zip(current)
        .filter(|(left, right)| left != right)
        .count()
        + baseline.len().abs_diff(current.len())
}

fn record(histogram: &mut Vec<u64>, changed: usize) {
    if histogram.len() <= changed {
        histogram.resize(changed + 1, 0);
    }
    histogram[changed] += 1;
}

fn merge_counts(total: &mut Vec<u64>, other: &[u64]) {
    if total.len() < other.len() {
        total.resize(other.len(), 0);
    }
    for (count, addend) in total.iter_mut().zip(other) {
        *count += addend;
    }
}

fn summarize(counts: &[u64]) -> ChangedBytesHistogram {
    let samples = counts.iter().sum();
    ChangedBytesHistogram {
        samples,
        histogram: counts
            .iter()
            .enumerate()
            .filter_map(|(changed, count)| (*count != 0).then_some((changed, *count)))
            .collect(),
        p50: percentile(counts, samples, 50),
        p95: percentile(counts, samples, 95),
        max: counts.iter().rposition(|count| *count != 0).unwrap_or(0),
    }
}

fn percentile(counts: &[u64], samples: u64, p: u64) -> usize {
    if samples == 0 {
        return 0;
    }
    let rank = (samples * p).div_ceil(100);
    let mut cumulative = 0;
    counts
        .iter()
        .position(|count| {
            cumulative += count;
            cumulative >= rank
        })
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    const CRAFT: BodyType = BodyType::Craft;

    #[test]
    fn changed_bytes_counts_differences_and_length_changes() {
        assert_eq!(changed_bytes(&[1, 2, 3], &[1, 4, 3]), 1);
        assert_eq!(changed_bytes(&[1, 2], &[1, 2, 3, 4]), 2);
        assert_eq!(changed_bytes(&[1, 2, 3, 4], &[1, 5]), 3);
    }

    #[test]
    fn keyframe_sends_are_not_counted_as_delta_samples() {
        let entity = PersistId::new(1);
        let mut stats = DeltaStats::new(2);
        stats.observe_bytes(entity, CRAFT, &[0, 0]);
        stats.observe_bytes(entity, CRAFT, &[1, 0]);
        stats.observe_bytes(entity, CRAFT, &[1, 1]);
        stats.observe_bytes(entity, CRAFT, &[2, 1]);

        let craft = &stats.report().body_types["craft"];
        assert_eq!(craft.vs_previous_send.histogram, BTreeMap::from([(1, 3)]));
        assert_eq!(craft.vs_keyframe.histogram, BTreeMap::from([(1, 2)]));
        assert_eq!(craft.vs_previous_send.samples, 3);
        assert_eq!(craft.vs_keyframe.samples, 2);
    }

    #[test]
    fn reports_nearest_rank_percentiles_and_observed_body_lengths() {
        let entity = PersistId::new(1);
        let mut stats = DeltaStats::new(20);
        stats.observe_bytes(entity, CRAFT, &[0, 0]);
        for changed in 1..=20u8 {
            stats.observe_bytes(entity, CRAFT, &[changed, u8::from(changed == 20)]);
        }

        let craft = &stats.report().body_types["craft"];
        assert_eq!(craft.body_bytes, vec![2]);
        assert_eq!(craft.vs_previous_send.p50, 1);
        assert_eq!(craft.vs_previous_send.p95, 1);
        assert_eq!(craft.vs_previous_send.max, 2);
        assert_eq!(craft.vs_keyframe.samples, 19);
        assert_eq!(craft.vs_keyframe.p50, 1);
    }

    #[test]
    fn merged_histograms_preserve_every_observation() {
        let mut left = DeltaStats::new(20);
        let mut right = DeltaStats::new(20);
        left.observe_bytes(PersistId::new(1), CRAFT, &[0]);
        left.observe_bytes(PersistId::new(1), CRAFT, &[1]);
        right.observe_bytes(PersistId::new(2), CRAFT, &[0, 0]);
        right.observe_bytes(PersistId::new(2), CRAFT, &[1, 1]);
        left.merge(&right);

        let craft = &left.report().body_types["craft"];
        assert_eq!(craft.body_bytes, vec![1, 2]);
        assert_eq!(
            craft.vs_previous_send.histogram,
            BTreeMap::from([(1, 1), (2, 1)])
        );
        assert_eq!(
            craft.vs_keyframe.histogram,
            BTreeMap::from([(1, 1), (2, 1)])
        );
    }
}
