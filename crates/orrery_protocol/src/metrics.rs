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

/// Server-side intent latency: gateway receipt of a `SubmitIntent` through the
/// send call that answers it, measured inside `persistd`.
///
/// The server-side half of [`SERIES_INTENT_COMMIT`], and deliberately **not**
/// that name. The gated series is a client round trip; this span is strictly
/// shorter, and a consumer that folds both into one histogram — which is
/// exactly what `p2-dashboard` does, by series name and with no source field —
/// would deflate the gated p99 rather than measure anything. Separate name,
/// separate histogram, no target.
pub const SERIES_GATEWAY_INTENT_SERVER: &str = "gateway_intent_server_ms";

/// Server-side area-load latency: gateway receipt of a `Subscribe` through the
/// send call carrying its **first** `AreaPage` frame, measured inside
/// `persistd`.
///
/// The server-side half of [`SERIES_AREA_FIRST_PAGE`], ungated for the same
/// reason as [`SERIES_GATEWAY_INTENT_SERVER`]. A subscribe that names no cell,
/// or whose every cell read fails, sends no page and contributes no sample —
/// the refusals are counted instead.
pub const SERIES_GATEWAY_AREA_FIRST_PAGE_SERVER: &str = "gateway_area_first_page_server_ms";

/// The four gated D16 series, in canonical report order.
pub const GATED_SERIES: [&str; 4] = [
    SERIES_JOURNAL_COMMIT,
    SERIES_BULK_ACK,
    SERIES_INTENT_COMMIT,
    SERIES_AREA_FIRST_PAGE,
];

/// Series carried by the P2 artifact that no D16 target gates.
///
/// Every member is a *server-internal* span produced by `persistd`, named so
/// it can never be folded into the gated series it attributes. Growing this
/// array is a deliberate cross-workspace change: `p2-dashboard`'s `SERIES_KEYS`
/// is fixed-length over `GATED_SERIES.len() + UNGATED_SERIES.len()`, so a new
/// member is a compile error there until the gate is taught to fold it.
pub const UNGATED_SERIES: [&str; 3] = [
    SERIES_GATEWAY_BULK_SERVER,
    SERIES_GATEWAY_INTENT_SERVER,
    SERIES_GATEWAY_AREA_FIRST_PAGE_SERVER,
];

/// Follower append latency: receipt of a chain batch through its durable
/// acknowledgement, measured inside the follower `persistd`.
pub const SERIES_CHAIN_FOLLOWER_APPEND: &str = "chain_follower_append_ms";
/// Primary-to-follower lag in journal bytes: `primary.committed()` minus the
/// highest origin LSN the follower has reported durable.
pub const SERIES_CHAIN_LAG_BYTES: &str = "chain_lag_bytes";
/// Primary-to-follower lag as age: how long the oldest unmirrored record has
/// been committed on the primary. This is the form D11's RPO target is stated
/// in; the byte gauge is what an LSN can measure directly.
pub const SERIES_CHAIN_LAG_AGE: &str = "chain_lag_age_ms";
/// Watermark probe latency: the reconnect round trip a primary performs before
/// it may resend a tail.
pub const SERIES_CHAIN_PROBE: &str = "chain_watermark_probe_ms";
/// Reconnects per stream: sessions opened on one durable chain identity.
pub const SERIES_CHAIN_RECONNECTS: &str = "chain_reconnects_total";
/// Duplicate batches: appends the follower deduped against its durable index
/// rather than storing again. Expected during reconnect, alarming when it
/// rises above reconnect noise.
pub const SERIES_CHAIN_DUPLICATE_BATCHES: &str = "chain_duplicate_batches_total";
/// Stream restarts per shard set: how often the transport had to rebuild a
/// stream, as distinct from how often a session was opened on a live one.
pub const SERIES_CHAIN_STREAM_RESTARTS: &str = "chain_stream_restarts_total";
/// Journal and fsync errors on either side of the chain.
pub const SERIES_CHAIN_JOURNAL_ERRORS: &str = "chain_journal_errors_total";

/// The chain signals `docs/13-chain-replication.md` §6 requires, in that
/// section's order. §6 lists eight; these are nine because its third entry,
/// "primary-to-follower lag in bytes and age", is two measurements.
///
/// The first entry is [`SERIES_JOURNAL_COMMIT`]: §6's "primary commit latency"
/// is the D16 series already contracted above, and giving it a second name
/// would be the drift this module exists to prevent.
///
/// These are deliberately **not** in [`UNGATED_SERIES`], and
/// [`is_known_series`] deliberately does not accept them. That predicate
/// answers a narrower question — whether a name may appear in the P2 latency
/// artifact `p2-dashboard` folds — and four of these are counters and one is a
/// byte gauge, none of which the [`LATENCY_BOUNDARIES_US`] lattice
/// describes. Naming them here is what stops the eventual producer and the
/// eventual consumer from inventing two spellings; promoting the latency
/// members into the artifact contract is a decision for whoever wires the
/// first one, not a side effect of writing them down.
pub const CHAIN_SERIES: [&str; 9] = [
    SERIES_JOURNAL_COMMIT,
    SERIES_CHAIN_FOLLOWER_APPEND,
    SERIES_CHAIN_LAG_BYTES,
    SERIES_CHAIN_LAG_AGE,
    SERIES_CHAIN_RECONNECTS,
    SERIES_CHAIN_DUPLICATE_BATCHES,
    SERIES_CHAIN_PROBE,
    SERIES_CHAIN_STREAM_RESTARTS,
    SERIES_CHAIN_JOURNAL_ERRORS,
];

/// The chain members measured in microseconds on the shared lattice, so a
/// producer knows which of them [`bucket_index`] applies to.
pub const CHAIN_LATENCY_SERIES: [&str; 3] = [
    SERIES_JOURNAL_COMMIT,
    SERIES_CHAIN_FOLLOWER_APPEND,
    SERIES_CHAIN_PROBE,
];

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
    fn chain_series_are_unique_and_stay_out_of_the_latency_artifact() {
        let mut names = CHAIN_SERIES.to_vec();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), CHAIN_SERIES.len(), "duplicate chain series");
        // `journal_commit_ms` is shared with D16 on purpose; everything else
        // is chain-only and must not be folded as a P2 latency series.
        for name in CHAIN_SERIES.iter().filter(|&&s| s != SERIES_JOURNAL_COMMIT) {
            assert!(!is_known_series(name), "{name} leaked into the P2 artifact");
        }
        assert!(CHAIN_LATENCY_SERIES
            .iter()
            .all(|name| CHAIN_SERIES.contains(name)));
    }

    #[test]
    fn no_ungated_series_shadows_a_gated_one() {
        // The whole point of a separate name: a server-internal span is
        // strictly shorter than the client round trip it attributes, and a
        // consumer folds by name alone.
        for ungated in UNGATED_SERIES {
            assert!(
                !GATED_SERIES.contains(&ungated),
                "{ungated} would deflate a gated p99"
            );
        }
        let mut names = GATED_SERIES.to_vec();
        names.extend_from_slice(&UNGATED_SERIES);
        let total = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), total, "duplicate series name");
    }

    #[test]
    fn known_series_covers_gated_and_ungated() {
        assert!(GATED_SERIES.iter().all(|&s| is_known_series(s)));
        assert!(UNGATED_SERIES.iter().all(|&s| is_known_series(s)));
        assert!(!is_known_series("bulk_ack_us"));
    }
}
