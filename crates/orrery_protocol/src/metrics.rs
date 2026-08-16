//! The D16 latency contract: series names and the shared histogram lattice.
//!
//! P2's acceptance gate is a chain of four processes — the journal recorder
//! inside `orrery_persistd`, the gateway's server-side bulk timer, the
//! client-side [`LatencyHistogram`] the `p2-load` rig measures with, and the
//! `p2-dashboard` gate that reconstructs percentiles from the JSONL artifact.
//! A percentile only means the same thing at both ends of that chain if every
//! link buckets a sample identically and spells the series the same way. This
//! module is that one definition.
//!
//! # Why this lives in `orrery_protocol`
//!
//! D15 casts this crate as the engine-agnostic *wire* surface, and a histogram
//! lattice is not a wire type. The placement is a deliberate stretch of that
//! charter, on two grounds. First, the D16 series names and `value_us` bucket
//! bounds *are* a wire format: they are the field values in the JSONL artifact
//! that `persistd` writes and `p2-dashboard` parses, and a producer and a
//! consumer in different processes agreeing on an encoding is exactly what
//! this crate exists to guarantee. Second, `orrery_protocol` is the only crate
//! all four participants already depend on — `orrery_persistd`,
//! `orrery_persist_client` and `p2-load` each name it directly, and
//! `p2-dashboard` reaches it through the client crate's re-export. Any other
//! home would mean a new dependency edge (D14), and the alternative to one
//! shared definition was four drifting copies, which is what this replaces.
//!
//! # This is a stepping stone toward D12, not a resolution of it
//!
//! [ADR-0012](https://github.com/baadc0de/orrery/blob/main/docs/adr/0012-backend-services.md)
//! makes OpenTelemetry the normative telemetry surface, and
//! `docs/09-services-and-ops.md` §Telemetry and `docs/10-crates.md` already
//! name an `otel` default feature and an `orrery_persistd::telemetry::init()`.
//! Neither exists, and neither is built here. What this module fixes is the
//! narrower, present-tense defect: the P2 gate could not distinguish a 1.05 ms
//! journal p99 from a 1.99 ms one, because the lattices disagreed. When the
//! OTel bridge lands, these names become the instrument names and these
//! boundaries the explicit bucket hints — the contract survives the transport
//! change, which is the point of writing it down once.
//!
//! [`LatencyHistogram`]: https://docs.rs/orrery_persist_client

/// Journal group-commit latency, server-internal (D16: < 2 ms).
pub const SERIES_JOURNAL_COMMIT: &str = "journal_commit_ms";
/// Client-observed bulk acknowledgement round trip (D16: p99 < 5 ms).
pub const SERIES_BULK_ACK: &str = "bulk_ack_ms";
/// Intent submit-through-commit round trip (D16: p99 < 10 ms).
pub const SERIES_INTENT_COMMIT: &str = "intent_commit_ms";
/// Subscribe through first `AreaPage` (D16: < 50 ms).
pub const SERIES_AREA_FIRST_PAGE: &str = "area_first_page_ms";

/// Server-side gateway bulk latency: receipt through send call, measured
/// inside `persistd` and emitted alongside the gated series.
///
/// It is **not** a D16 target. It exists as the server-side half of
/// `bulk_ack_ms`, so a bulk-ack regression can be attributed to server work
/// or to the wire without re-running. It is a known series — the gate folds
/// and reports it, and never fails on it — and naming it here is what keeps
/// `p2-dashboard` from having to choose between silently discarding it and
/// calling every nightly artifact malformed.
pub const SERIES_GATEWAY_BULK_SERVER: &str = "gateway_bulk_server_ms";

/// The four gated D16 series, in canonical report order.
pub const GATED_SERIES: [&str; 4] = [
    SERIES_JOURNAL_COMMIT,
    SERIES_BULK_ACK,
    SERIES_INTENT_COMMIT,
    SERIES_AREA_FIRST_PAGE,
];

/// Series carried by the P2 artifact that no D16 target gates.
pub const UNGATED_SERIES: [&str; 1] = [SERIES_GATEWAY_BULK_SERVER];

/// Shared bucket boundaries in microseconds, ascending.
///
/// A sample belongs to the first boundary greater than or equal to it; a
/// sample above the last boundary lands in the overflow bucket, which has no
/// upper bound and is serialized at the observed maximum.
///
/// Rationale, D16 targets in parentheses:
/// - 50, 100, 200, 500 µs: sub-millisecond ranges.
/// - 1, 1.25, 1.5, 1.75, 2 ms: the journal-commit band (< 2 ms). The gate
///   compares `p99 <= 2_000`, so a lattice that jumps 1 ms → 2 ms reports
///   every p99 in that whole band as exactly the threshold and passes on the
///   equality case. These four sub-2 ms boundaries are the reason this table
///   is 25 entries and not 22.
/// - 3, 5 ms: the bulk-ack band (p99 < 5 ms).
/// - 7, 10 ms: the intent-commit band (p99 < 10 ms).
/// - 15, 20, 30, 50 ms: the area first-page band (< 50 ms).
/// - 75 ms through 1 s: the wide tail.
pub const LATENCY_BOUNDARIES_US: [u64; 25] = [
    50, 100, 200, 500, 1_000, 1_250, 1_500, 1_750, 2_000, 3_000, 5_000, 7_000, 10_000, 15_000,
    20_000, 30_000, 50_000, 75_000, 100_000, 150_000, 200_000, 300_000, 500_000, 750_000,
    1_000_000,
];

/// Bucket count: one per boundary, plus the unbounded overflow bucket.
pub const NUM_LATENCY_BUCKETS: usize = LATENCY_BOUNDARIES_US.len() + 1;

/// The bucket index a sample of `micros` microseconds belongs to.
///
/// Always in `0..NUM_LATENCY_BUCKETS`; `LATENCY_BOUNDARIES_US.len()` is the
/// overflow bucket.
#[must_use]
pub fn bucket_index(micros: u64) -> usize {
    LATENCY_BOUNDARIES_US.partition_point(|&boundary| micros > boundary)
}

/// The microsecond value a bucket's samples are reported at: the bucket's
/// upper bound, or `observed_max_us` for the overflow bucket, which has none.
///
/// This is the single reconstruction rule. A producer draining bucket `index`
/// emits this value, and a consumer feeding that value back through
/// [`bucket_index`] lands in `index` again — so a percentile computed on
/// either side of the artifact means the same thing.
#[must_use]
pub fn bucket_upper_us(index: usize, observed_max_us: u64) -> u64 {
    LATENCY_BOUNDARIES_US
        .get(index)
        .copied()
        .unwrap_or(observed_max_us)
}

/// Whether `name` is a series this contract defines, gated or not.
///
/// A consumer uses this to separate "a series I do not fold" from "a record I
/// could not understand": an unknown name is a contract violation worth
/// reporting, an ungated known name is not.
#[must_use]
pub fn is_known_series(name: &str) -> bool {
    GATED_SERIES.contains(&name) || UNGATED_SERIES.contains(&name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn boundaries_are_ascending_and_unique() {
        assert!(LATENCY_BOUNDARIES_US.windows(2).all(|w| w[0] < w[1]));
    }

    #[test]
    fn bucket_index_is_inclusive_of_its_upper_bound() {
        assert_eq!(bucket_index(0), 0);
        assert_eq!(bucket_index(50), 0);
        assert_eq!(bucket_index(51), 1);
        // The band the D16 journal gate reads: 1 ms and 2 ms are no longer
        // adjacent, so a 1.5 ms p99 no longer reports as the 2 ms threshold.
        assert_ne!(bucket_index(1_001), bucket_index(1_999));
        assert_eq!(
            bucket_upper_us(bucket_index(1_500), 0),
            1_500,
            "a 1.5 ms sample must report as 1.5 ms, not as the 2 ms threshold"
        );
    }

    #[test]
    fn overflow_reports_the_observed_maximum() {
        let index = bucket_index(2_000_000);
        assert_eq!(index, LATENCY_BOUNDARIES_US.len());
        assert_eq!(bucket_upper_us(index, 2_000_000), 2_000_000);
    }

    #[test]
    fn reported_value_round_trips_to_the_same_bucket() {
        for micros in [0, 1, 49, 50, 999, 1_000, 1_249, 1_500, 4_999, 999_999] {
            let index = bucket_index(micros);
            let reported = bucket_upper_us(index, 0);
            assert_eq!(
                bucket_index(reported),
                index,
                "{micros} µs reported as {reported} µs must re-read as bucket {index}"
            );
        }
    }

    #[test]
    fn every_gated_band_has_boundaries_inside_it() {
        // At least two boundaries at or below each D16 target, so a p99 near
        // the target resolves rather than pinning to the target itself.
        for target_us in [2_000u64, 5_000, 10_000, 50_000] {
            let below = LATENCY_BOUNDARIES_US
                .iter()
                .filter(|&&b| b <= target_us)
                .count();
            assert!(
                below >= 2,
                "{target_us} µs has only {below} boundaries at or below it"
            );
        }
    }

    #[test]
    fn known_series_covers_gated_and_ungated() {
        assert!(GATED_SERIES.iter().all(|&s| is_known_series(s)));
        assert!(UNGATED_SERIES.iter().all(|&s| is_known_series(s)));
        assert!(!is_known_series("bulk_ack_us"));
    }
}
