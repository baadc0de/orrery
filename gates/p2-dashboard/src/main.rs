//! P2 latency gate.
//!
//! Consumes the JSONL telemetry emitted by `gates/p2-load --json`
//! (docs/11-roadmap.md §P2) and reports the four D16 latency series against
//! the demo-criterion targets verbatim (docs/adr/0016-parameter-reference.md):
//!
//! | series               | D16 target (p99, in-region) |
//! |----------------------|-----------------------------|
//! | `journal_commit_ms`  | < 2 ms (server-internal)    |
//! | `bulk_ack_ms`        | < 5 ms (client-observed)    |
//! | `intent_commit_ms`   | < 10 ms                     |
//! | `area_first_page_ms` | < 50 ms                     |
//!
//! A fifth series, `gateway_bulk_server_ms`, is the server-side half of
//! `bulk_ack_ms` that persistd appends to the same artifact. D16 sets no
//! target for it, so it is folded and reported for attribution and never
//! contributes to the verdict — present or absent. It used to be silently
//! discarded while the report printed zero malformed records.
//!
//! Four more, `client_bulk_{queue,send,wire,dispatch}_ms`, are the *client*
//! side of the same attribution: the rig's own backlog, its send path, the
//! wire, and its ack handling. `bulk_ack_ms` covers `send + wire` only, so
//! the four decompose a bulk-ack tail into a part the server owns and three
//! parts it does not. Ungated, for the same reason and with the same
//! consequence: present or absent, they never change the verdict.
//!
//! The JSONL input carries raw µs samples (one `sample`, or a compact
//! `sample_batch` with an explicit count); this tool buckets them into the bounded-memory
//! [`LatencyHistogram`] from the client crate, exactly as the rig does —
//! percentiles come out of one code path on both sides of the wire, so the
//! gate's reading and the rig's live CPU-side reading mean the same thing.
//! The series names, the boundaries and the reconstruction rule are one
//! definition in `orrery_protocol::metrics`, reached here through the client
//! crate's re-export.
//!
//! `--json` carries the stable machine contract (a `Report` struct); `--gate`
//! makes the process exit non-zero when any series misses its threshold. The
//! P2 kill-9 harness also supplies D19's device qualification. On an
//! unqualified device the measurements remain visible, but no latency
//! comparison is rendered as a pass or a failure.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use serde::{Deserialize, Serialize};

use orrery_persist_client::latency::{
    is_byte_series, is_client_series, is_gateway_boundary_series, is_known_series,
    LatencyHistogram, CLIENT_UNGATED_SERIES, GATED_SERIES, GATEWAY_BOUNDARY_SERIES,
    SERIES_AREA_FIRST_PAGE, SERIES_BULK_ACK, SERIES_INTENT_COMMIT, SERIES_JOURNAL_COMMIT,
    UNGATED_SERIES,
};

/// Every series this gate folds, in canonical report order: the four gated
/// D16 keys first, then the ungated ones. The keys, the bucket boundaries and
/// the reconstruction rule all come from `orrery_protocol::metrics` through
/// the client crate's re-export — one definition, shared with the rig that
/// produces the stream and the persistd that appends to it.
///
/// The length is derived, so growing `UNGATED_SERIES` upstream is a compile
/// error here rather than a series silently counted as unknown. That is the
/// intended failure: this workspace is excluded from the root one, so nothing
/// else would notice.
const SERIES_KEYS: [&str;
    GATED_SERIES.len()
        + UNGATED_SERIES.len()
        + GATEWAY_BOUNDARY_SERIES.len()
        + CLIENT_UNGATED_SERIES.len()] = [
    GATED_SERIES[0],
    GATED_SERIES[1],
    GATED_SERIES[2],
    GATED_SERIES[3],
    UNGATED_SERIES[0],
    UNGATED_SERIES[1],
    UNGATED_SERIES[2],
    GATEWAY_BOUNDARY_SERIES[0],
    GATEWAY_BOUNDARY_SERIES[1],
    GATEWAY_BOUNDARY_SERIES[2],
    CLIENT_UNGATED_SERIES[0],
    CLIENT_UNGATED_SERIES[1],
    CLIENT_UNGATED_SERIES[2],
    CLIENT_UNGATED_SERIES[3],
    CLIENT_UNGATED_SERIES[4],
    CLIENT_UNGATED_SERIES[5],
];

/// How many of [`SERIES_KEYS`] carry a D16 threshold. The rest are folded and
/// reported, never gated.
const NUM_GATED: usize = GATED_SERIES.len();

/// Which gated series' spans strictly *contain* which other gated series'
/// spans, as `(outer, &[inner…])`.
///
/// # The problem this fixes
///
/// The four D16 series are presented as four independent gates, and three of
/// them are not independent of each other. `bulk_ack_ms` is, by construction,
/// a journal commit plus routing plus two trips over the wire: the client
/// stamps at flush selection and stops when the acknowledging datagram lands,
/// and that acknowledgement is only sent once the journal has reported the
/// record durable. `intent_commit_ms` contains a durable commit for the same
/// reason. So `bulk_ack_ms` (5 ms) cannot pass while `journal_commit_ms`
/// (2 ms) fails at 100 ms, and neither can `intent_commit_ms` (10 ms).
///
/// A run in that state printed three `FAIL` rows as three peers. A reader
/// with three failures and no stated relation between them opens three
/// investigations, two of which are the same investigation. Worse, the two
/// dependents carry no information at all in that state: they were going to
/// fail whatever else was true of them.
///
/// # Is the gate, as presented, fair and useful?
///
/// Fair, yes — every threshold is a real D16 target measured over the span it
/// names, and the durable ack really did take that long. Useful, no: a
/// verdict table's job is to say what to look at, and this one said "look at
/// three things" when there was one thing.
///
/// The fix is *not* to suppress or relax the dependents. A dependent that
/// fails still fails; the exit code, the per-series verdicts and every
/// threshold are byte-for-byte what they were, and
/// `containment_changes_no_verdict` is the guard on that. What changes is the
/// ordering the report hands the reader: each failing series is classified as
/// a **root** (nothing it contains is also failing, so its excess is its own)
/// or a **consequence** (something it contains is failing, so it cannot pass
/// until that does), and the roots are named first.
///
/// # What the containment claim does and does not assert
///
/// Per operation it is exact: the acknowledgement for one diff strictly
/// encloses the commit that diff waited on, so that diff's `bulk_ack_ms`
/// sample is greater than that commit's `journal_commit_ms` sample.
///
/// Across *percentiles* it is an implication, not an identity: the two series
/// count different populations — `journal_commit_ms` samples group commits,
/// `bulk_ack_ms` samples acknowledged diffs, and one group commit answers
/// many diffs. A committer that is slow on 1 % of its commits can be slow for
/// far more than 1 % of the diffs riding them, so the dependent's p99 is
/// pushed *up* by the root's tail, never held below it. The report says
/// "cannot pass while its inner series fails" on that basis, and never
/// asserts a numeric identity between the two p99s.
const CONTAINMENT: [(&str, &[&str]); 2] = [
    (SERIES_BULK_ACK, &[SERIES_JOURNAL_COMMIT]),
    (SERIES_INTENT_COMMIT, &[SERIES_JOURNAL_COMMIT]),
];

/// The gated series whose spans `key` strictly contains. Empty for a series
/// that contains no other gated span.
fn contained_by(key: &str) -> &'static [&'static str] {
    CONTAINMENT
        .iter()
        .find(|(outer, _)| *outer == key)
        .map_or(&[][..], |(_, inner)| *inner)
}

/// Where a failing series sits in the containment order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum FailureRole {
    /// This series fails and nothing it contains does: the excess is its own,
    /// and this is where the investigation starts.
    Root,
    /// This series fails and so does something it contains. It cannot pass
    /// until that one does, so on its own it carries no new information.
    Consequence {
        /// The failing inner series, in report order.
        of: Vec<&'static str>,
    },
}

/// The unit a series' numbers are in. The histogram lattice is a set of
/// integers and knows nothing about units; two attribution members are byte
/// gauges that share it because 50 B … 1 MiB is the range it covers. Printing
/// those under a "µs" header would be a lie told by a column title.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum SeriesUnit {
    /// Microseconds.
    Us,
    /// Bytes.
    Bytes,
}

impl SeriesUnit {
    fn of(key: &str) -> Self {
        if is_byte_series(key) {
            Self::Bytes
        } else {
            Self::Us
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Us => "µs",
            Self::Bytes => "B",
        }
    }
}

/// D16 defaults (docs/adr/0016-parameter-reference.md) as **µs ceilings** on
/// the p99. These
/// are the demo-criterion numbers; `--<series>-us` flags override them
/// individually for a sample-size- or posture-specific gate.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
struct ThresholdsUs {
    /// journal commit < 2 ms (server-internal).
    journal_commit_ms: u64,
    /// client-observed bulk ack p99 < 5 ms.
    bulk_ack_ms: u64,
    /// intent commit p99 < 10 ms.
    intent_commit_ms: u64,
    /// area first page-in < 50 ms.
    area_first_page_ms: u64,
}

impl ThresholdsUs {
    /// The D16 values verbatim.
    const D16: Self = Self {
        journal_commit_ms: 2_000,
        bulk_ack_ms: 5_000,
        intent_commit_ms: 10_000,
        area_first_page_ms: 50_000,
    };
}

/// One summary per series: bounded-memory percentiles from the shared D16
/// histogram (crates/orrery_persist_client/src/latency.rs). `p50_us`/`p99_us`
/// are the upper bound of the containing bucket (within one bucket width of
/// the true value by construction); `max_us` is exact. All are `None` when the
/// series has no samples.
#[derive(Debug, Clone, Serialize)]
struct SeriesSummary {
    /// Sample count folded into this series.
    n: u64,
    /// Approximate p50, µs.
    p50_us: Option<u64>,
    /// Approximate p99, µs; the gate compares this against the threshold.
    p99_us: Option<u64>,
    /// Exact max, µs.
    max_us: Option<u64>,
    /// The threshold this series is gated against (µs), or `None` for a
    /// series D16 sets no target for.
    threshold_us: Option<u64>,
    /// Whether this series met its threshold.
    gate: SeriesGate,
    /// The unit `p50`/`p99`/`max` are in. Microseconds for every latency
    /// series; bytes for the two send-buffer gauges that share the lattice.
    unit: SeriesUnit,
    /// The gated series whose spans this one strictly contains ([`CONTAINMENT`]).
    /// Empty for every ungated series and for the innermost gated one.
    #[serde(skip_serializing_if = "<[_]>::is_empty")]
    contains: &'static [&'static str],
    /// Set only on a failing series: whether this failure is a root cause or
    /// a consequence of a failing series it contains. Never affects
    /// [`SeriesSummary::gate`].
    #[serde(skip_serializing_if = "Option::is_none")]
    failure_role: Option<FailureRole>,
}

/// Per-series gate outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum SeriesGate {
    /// p99 at or below the threshold.
    Pass,
    /// p99 above the threshold.
    Fail,
    /// No samples were recorded for this series. A series the run never
    /// sampled cannot pass the D16 demo criterion by omission.
    MissingData,
    /// Samples exist, but D19's device qualification failed. The measurement
    /// is reported beside its D16 threshold without turning it into a verdict.
    Unqualified,
    /// D16 sets no target for this series: it is reported for attribution
    /// and never contributes to the verdict, present or absent.
    NotGated,
}

/// The overall gate verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum GateVerdict {
    /// Every series met its threshold.
    Pass,
    /// At least one series missed (or was missing).
    Fail,
    /// Every required series was measured, but the device failed D19's
    /// qualification, so the latency verdict was withheld.
    Unqualified,
}

/// D19's fio job-A requirement, copied into the run artifact by the harness.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct DeviceQualificationRequirement {
    /// Number of concurrent writers in fio job A.
    jobs: usize,
    /// Duration of the time-based job.
    runtime_seconds: u64,
    /// Size of each write followed by fdatasync.
    block_size_bytes: u64,
    /// Offered rate per writer.
    offered_rate_iops_per_job: f64,
    /// Minimum observed rate accepted per writer. This permits fio's timer
    /// accounting tolerance while still detecting a device that cannot hold
    /// the offered 470 barriers/s.
    minimum_rate_iops_per_job: f64,
    /// D19's binding qualification threshold.
    sync_max_ms_below: f64,
    /// Aggregate rate recorded by D19's reference run.
    reference_barriers_per_s: f64,
    /// D19's reference p99, for comparison rather than qualification.
    reference_sync_p99_ms: f64,
    /// D19's reference maximum.
    reference_sync_max_ms: f64,
}

/// One fio writer's observed job-A result.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct DeviceQualificationJob {
    /// Sustained write-and-fdatasync operations per second.
    iops: f64,
    /// 99th percentile fdatasync latency.
    sync_p99_ms: f64,
    /// Maximum fdatasync latency.
    sync_max_ms: f64,
}

/// Aggregate fio result for the filesystem carrying the journals.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct DeviceQualificationMeasurement {
    /// Sum of the two writers' observed rates.
    aggregate_barriers_per_s: f64,
    /// Worse of the two writers' p99s.
    worst_sync_p99_ms: f64,
    /// Worse of the two writers' maxima.
    worst_sync_max_ms: f64,
    /// Per-writer observations used to adjudicate qualification.
    jobs: Vec<DeviceQualificationJob>,
}

/// Device preflight emitted by `scripts/p2-kill9-gate.sh`.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct DeviceQualification {
    /// Stable artifact discriminator.
    kind: String,
    /// Measurement method (`fio_job_a` or an explicit unavailable result).
    method: String,
    /// Reproducible command shape. The journal path is represented by a
    /// placeholder and recorded separately in `data_path`.
    command: String,
    /// Filesystem path on which job A ran.
    data_path: String,
    /// fio version, when fio could run.
    #[serde(default)]
    fio_version: Option<String>,
    /// The exact qualification requirement and D19 reference figures.
    required: DeviceQualificationRequirement,
    /// Measurement, absent only when the preflight could not execute.
    #[serde(default)]
    measured: Option<DeviceQualificationMeasurement>,
    /// Whether the measured jobs met the requirement.
    qualified: bool,
    /// Why qualification was refused.
    #[serde(default)]
    reason: Option<String>,
}

/// The run context block the rig emits in its `run_header` record. All fields
/// optional on the wire; the report echoes them unchanged so a viewer knows
/// which run the numbers came from.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct RunContext {
    /// The gateway's NodeId (hex, `iroh` display form).
    #[serde(default)]
    gateway: Option<String>,
    /// The `--addr` the rig dialed.
    #[serde(default)]
    addr: Option<String>,
    /// Entities driven by the run.
    #[serde(default)]
    entities: Option<u64>,
    /// Cells covered by the entity inventory.
    #[serde(default)]
    cells: Option<u64>,
    /// Sessions (client fan-out) opened against the gateway.
    #[serde(default)]
    sessions: Option<u64>,
    /// Requested per-entity diff rate (Hz).
    #[serde(default)]
    diff_hz: Option<f64>,
    /// The requested intent mix, as `kind=fraction` pairs.
    #[serde(default)]
    intent_mix: Option<BTreeMap<String, f64>>,
    /// The full run duration, seconds.
    #[serde(default)]
    duration_secs: Option<u64>,
}

/// The `--json` report — the stable machine contract for the gate.
#[derive(Debug, Serialize)]
struct Report {
    /// Total non-empty JSONL lines read.
    records: usize,
    /// Lines that failed to parse.
    malformed: usize,
    /// Sample records naming a series this contract does not define. These
    /// are counted and reported but never gated: a producer that grows a new
    /// series should not fail a nightly run, while a *typo* in one of the
    /// gated names shows up here instead of vanishing.
    unknown_series: usize,
    /// The distinct series names behind `unknown_series`, sorted. A count
    /// alone does not tell an operator which producer drifted.
    unknown_series_names: Vec<String>,
    /// Whether the run's p99s all met their thresholds.
    gate: GateVerdict,
    /// D19 device preflight. Absent for standalone dashboard use; the kill-9
    /// gate always supplies it.
    #[serde(skip_serializing_if = "Option::is_none")]
    device_qualification: Option<DeviceQualification>,
    /// The run context echoed from the `run_header` record, when present.
    #[serde(skip_serializing_if = "Option::is_none")]
    run: Option<RunContext>,
    /// Per-series summaries, keyed by the wire series name.
    series: BTreeMap<&'static str, SeriesSummary>,
    /// The failing series that are not explained by another failing series
    /// they contain — what to look at first, in report order. Empty on a
    /// passing run. A reader who fixes every entry here has addressed every
    /// failure in the run, because the rest are downstream of these.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    root_causes: Vec<&'static str>,
}

/// One JSONL record (the subset of fields the dashboard needs). Unknown extra
/// fields are ignored so forward-compatible emitters do not break the gate.
#[derive(Debug, Deserialize)]
struct Record {
    /// The record kind: `run_header` | `sample` | `sample_batch` | `run_footer`.
    #[serde(rename = "type")]
    kind: String,
    /// For `run_header`: the run context.
    #[serde(default)]
    run: Option<RunContext>,
    /// For `sample`: which D16 series.
    #[serde(default)]
    series: Option<String>,
    /// For `sample`: the latency, µs.
    #[serde(default)]
    value_us: Option<u64>,
    /// For `sample_batch`: number of identical values represented.
    #[serde(default)]
    count: Option<u64>,
}

#[derive(Parser)]
#[command(
    name = "p2-dashboard",
    about = "P2 latency gate: aggregate gates/p2-load --json telemetry against the D16 targets"
)]
struct Cli {
    /// One or more `.jsonl` telemetry files from `gates/p2-load --json`.
    #[arg(required = true)]
    files: Vec<PathBuf>,
    /// Emit the machine-readable JSON summary (the stable machine contract)
    /// instead of the human report.
    #[arg(long, env = "ORRERY_JSON")]
    json: bool,
    /// Exit non-zero when any series' p99 misses its threshold. The D16 demo
    /// criterion gates on this flag; a series with no samples fails the gate.
    #[arg(long, env = "ORRERY_GATE")]
    gate: bool,
    /// D19 device-qualification JSON emitted before the P2 load starts. If it
    /// says the device is unqualified, latency comparisons are withheld while
    /// missing telemetry still fails.
    #[arg(long, env = "ORRERY_DEVICE_QUALIFICATION")]
    device_qualification: Option<PathBuf>,
    /// Threshold override for `journal_commit_ms` (µs). Default: the D16 2 ms
    /// target.
    #[arg(long, default_value_t = ThresholdsUs::D16.journal_commit_ms, env = "ORRERY_JOURNAL_COMMIT_MS")]
    journal_commit_ms: u64,
    /// Threshold override for `bulk_ack_ms` (µs). Default: the D16 5 ms
    /// target.
    #[arg(long, default_value_t = ThresholdsUs::D16.bulk_ack_ms, env = "ORRERY_BULK_ACK_MS")]
    bulk_ack_ms: u64,
    /// Threshold override for `intent_commit_ms` (µs). Default: the D16 10 ms
    /// target.
    #[arg(long, default_value_t = ThresholdsUs::D16.intent_commit_ms, env = "ORRERY_INTENT_COMMIT_MS")]
    intent_commit_ms: u64,
    /// Threshold override for `area_first_page_ms` (µs). Default: the D16 50
    /// ms target.
    #[arg(long, default_value_t = ThresholdsUs::D16.area_first_page_ms, env = "ORRERY_AREA_FIRST_PAGE_MS")]
    area_first_page_ms: u64,
}

fn main() -> ExitCode {
    match run() {
        Ok(exit) => exit,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::from(2)
        }
    }
}

fn run() -> Result<ExitCode> {
    let cli = Cli::parse();
    let thresholds = ThresholdsUs {
        journal_commit_ms: cli.journal_commit_ms,
        bulk_ack_ms: cli.bulk_ack_ms,
        intent_commit_ms: cli.intent_commit_ms,
        area_first_page_ms: cli.area_first_page_ms,
    };
    let loaded = load(&cli.files)?;
    let qualification = cli
        .device_qualification
        .as_deref()
        .map(load_device_qualification)
        .transpose()?;
    Ok(report_and_maybe_gate(
        &cli,
        &thresholds,
        &loaded,
        qualification,
    ))
}

/// Read and minimally validate the harness-owned qualification artifact.
fn load_device_qualification(path: &std::path::Path) -> Result<DeviceQualification> {
    let file = File::open(path)
        .with_context(|| format!("failed to open device qualification {}", path.display()))?;
    let qualification: DeviceQualification = serde_json::from_reader(file)
        .with_context(|| format!("failed to parse device qualification {}", path.display()))?;
    anyhow::ensure!(
        qualification.kind == "d19_device_qualification",
        "device qualification {} has unexpected kind {:?}",
        path.display(),
        qualification.kind
    );
    Ok(qualification)
}

/// The fully-parsed input: per-series histograms plus the run context.
struct Loaded {
    /// One histogram per series, indexed by position in [`SERIES_KEYS`].
    histograms: [LatencyHistogram; SERIES_KEYS.len()],
    /// The run context from the `run_header` record, if one was present.
    run_ctx: Option<RunContext>,
    /// Total non-empty lines read.
    records: usize,
    /// Lines that failed to parse.
    malformed: usize,
    /// Sample records naming a series outside the shared contract.
    unknown_series: usize,
    /// The distinct names behind `unknown_series`.
    unknown_series_names: BTreeSet<String>,
}

/// Read every JSONL file once and fold it into the histogram set. Sample
/// values stream through constant memory (the bucket layout is fixed by
/// `orrery_protocol::metrics`), so a 30-minute soak at 10k entities × 4 Hz is
/// not a memory problem here either — the same argument as in the client
/// crate's `latency` module.
fn load(files: &[PathBuf]) -> Result<Loaded> {
    let mut histograms = std::array::from_fn(|_| LatencyHistogram::new());
    let mut run_ctx: Option<RunContext> = None;
    let mut records = 0usize;
    let mut malformed = 0usize;
    let mut unknown_series = 0usize;
    let mut unknown_series_names = BTreeSet::new();

    for path in files {
        let file =
            File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
        let reader = BufReader::new(file);
        for (lineno, line) in reader.lines().enumerate() {
            let line = line.with_context(|| format!("read {}:{}", path.display(), lineno + 1))?;
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            records += 1;
            match serde_json::from_str::<Record>(line) {
                Ok(record) => {
                    if let Some(name) = ingest(&mut histograms, &mut run_ctx, record) {
                        unknown_series += 1;
                        unknown_series_names.insert(name);
                    }
                }
                Err(e) => {
                    malformed += 1;
                    eprintln!(
                        "warning: {}:{}: malformed JSON: {e}",
                        path.display(),
                        lineno + 1
                    );
                }
            }
        }
    }

    Ok(Loaded {
        histograms,
        run_ctx,
        records,
        malformed,
        unknown_series,
        unknown_series_names,
    })
}

/// Fold one JSONL record into the live state. Returns the series name for a
/// sample record the shared contract does not define — the one case the
/// caller counts, since it is the only way a producer/consumer name mismatch
/// can be seen at all.
fn ingest(
    histograms: &mut [LatencyHistogram; SERIES_KEYS.len()],
    run_ctx: &mut Option<RunContext>,
    r: Record,
) -> Option<String> {
    match r.kind.as_str() {
        "run_header" => {
            if r.run.is_some() {
                *run_ctx = r.run;
            }
            None
        }
        "sample" | "sample_batch" => {
            let (Some(series), Some(value_us)) = (r.series, r.value_us) else {
                return None;
            };
            let Some(idx) = SERIES_KEYS.iter().position(|&k| k == series) else {
                debug_assert!(
                    !is_known_series(&series)
                        && !is_client_series(&series)
                        && !is_gateway_boundary_series(&series)
                );
                return Some(series);
            };
            let count = if r.kind == "sample_batch" {
                r.count.unwrap_or(0)
            } else {
                1
            };
            for _ in 0..count {
                histograms[idx].record(Duration::from_micros(value_us));
            }
            None
        }
        // run_footer and unknown kinds carry no latency data.
        _ => None,
    }
}

/// The D16 threshold for one series, by wire key, or `None` for a series D16
/// sets no target for.
fn threshold_for(t: &ThresholdsUs, key: &str) -> Option<u64> {
    match key {
        SERIES_JOURNAL_COMMIT => Some(t.journal_commit_ms),
        SERIES_BULK_ACK => Some(t.bulk_ack_ms),
        SERIES_INTENT_COMMIT => Some(t.intent_commit_ms),
        SERIES_AREA_FIRST_PAGE => Some(t.area_first_page_ms),
        _ => None,
    }
}

/// Build the report from the loaded input, print it, and return the process
/// exit code under `--gate`. This is the whole gate; `run()` is just this
/// plus file IO, so the tests exercise the exact binary path.
fn report_and_maybe_gate(
    cli: &Cli,
    thresholds: &ThresholdsUs,
    loaded: &Loaded,
    qualification: Option<DeviceQualification>,
) -> ExitCode {
    let report = build_report(thresholds, loaded, qualification);

    if cli.json {
        match serde_json::to_string_pretty(&report) {
            Ok(json) => println!("{json}"),
            Err(e) => {
                eprintln!("error: report serialization failed: {e}");
                return ExitCode::from(2);
            }
        }
    } else {
        print_human(&report);
    }

    if cli.gate {
        match report.gate {
            GateVerdict::Pass => ExitCode::SUCCESS,
            GateVerdict::Unqualified => {
                for line in gate_unqualified_report(&report) {
                    eprintln!("{line}");
                }
                ExitCode::SUCCESS
            }
            GateVerdict::Fail => {
                for line in gate_failure_report(&report) {
                    eprintln!("{line}");
                }
                ExitCode::FAILURE
            }
        }
    } else {
        ExitCode::SUCCESS
    }
}

/// The explicit non-verdict printed when D19's preflight refuses the device.
fn gate_unqualified_report(r: &Report) -> Vec<String> {
    let Some(q) = &r.device_qualification else {
        return vec!["LATENCY UNQUALIFIED: device qualification is absent".to_string()];
    };
    let required = &q.required;
    let mut lines = vec![
        "LATENCY UNQUALIFIED: D19 device qualification failed; D16 latency verdict withheld"
            .to_string(),
    ];
    if let Some(measured) = &q.measured {
        lines.push(format!(
            "  fio job A: {:.1} barriers/s measured vs {:.1} reference; p99 {:.3} ms vs {:.3} ms reference; max {:.3} ms vs required < {:.3} ms (reference {:.3} ms)",
            measured.aggregate_barriers_per_s,
            required.reference_barriers_per_s,
            measured.worst_sync_p99_ms,
            required.reference_sync_p99_ms,
            measured.worst_sync_max_ms,
            required.sync_max_ms_below,
            required.reference_sync_max_ms,
        ));
    } else {
        lines.push(format!(
            "  no fio measurement: {}",
            q.reason.as_deref().unwrap_or("preflight unavailable")
        ));
    }
    lines.push(format!("  journal path: {}", q.data_path));
    lines
}

/// The stderr text the `--gate` failure path prints, one entry per line.
///
/// # Why this exists
///
/// `--gate --json` sends the report to stdout, and the P2 kill-9 harness
/// redirects that into `latency-report.json`. So on a nightly failure the only
/// thing that reaches the job log is what this function returns. It used to
/// return one sentence — "one or more D16 series missed its p99 target" — which
/// names no series, no measurement and no target, and the nightly was red for
/// four consecutive nights with no reader able to say which budget was missed
/// or by how much. Reading the artifact is a 160 MB download; the log should
/// answer it.
///
/// Every number here comes off the [`Report`] that was already built. This is
/// rendering: it reads verdicts and thresholds and writes nothing back, so no
/// series' outcome and no exit code can move in here.
///
/// The gated series are walked in [`SERIES_KEYS`] order (the four D16 keys
/// first, innermost span first) rather than in the report's alphabetical map
/// order, so the root cause is printed above the failures it explains.
fn gate_failure_report(r: &Report) -> Vec<String> {
    let gated: Vec<(&&str, &SeriesSummary)> = SERIES_KEYS
        .iter()
        .filter_map(|key| r.series.get_key_value(key))
        .filter(|(_, s)| s.threshold_us.is_some())
        .collect();
    let missed: Vec<&(&&str, &SeriesSummary)> = gated
        .iter()
        .filter(|(_, s)| matches!(s.gate, SeriesGate::Fail | SeriesGate::MissingData))
        .collect();
    let passed: Vec<&str> = gated
        .iter()
        .filter(|(_, s)| s.gate == SeriesGate::Pass)
        .map(|(key, _)| **key)
        .collect();

    let mut lines = vec![format!(
        "GATE FAILED: {} of {} gated D16 series missed {}",
        missed.len(),
        gated.len(),
        if missed.len() == 1 {
            "its p99 target"
        } else {
            "their p99 targets"
        }
    )];
    for (key, s) in &missed {
        let unit = s.unit.label();
        // A gated series always carries a threshold; the filter above is what
        // makes that true, and the `—` is the honest reading if it ever stops
        // being true rather than a panic in the failure path.
        let target = s
            .threshold_us
            .map_or_else(|| "—".to_string(), |t| format!("{t} {unit}"));
        let measured = match (s.gate, s.p99_us) {
            (SeriesGate::MissingData, _) | (_, None) => {
                format!("p99 — (0 samples)   D16 target {target}   margin — (never measured)")
            }
            (_, Some(p99)) => {
                let t = s.threshold_us.unwrap_or(0);
                let over = p99.saturating_sub(t);
                let factor = if t == 0 {
                    String::new()
                } else {
                    format!(", {:.1}x the target", p99 as f64 / t as f64)
                };
                format!("p99 {p99} {unit}   D16 target {target}   margin +{over} {unit}{factor}")
            }
        };
        let role = match &s.failure_role {
            Some(FailureRole::Root) => "   ROOT CAUSE".to_string(),
            Some(FailureRole::Consequence { of }) => {
                format!("   consequence of {}", of.join(", "))
            }
            None => String::new(),
        };
        lines.push(format!("  {key:<22} {measured}{role}"));
    }
    // The exception is not the picture. A reader who sees only the misses
    // cannot tell a single regressed series from a run where everything is
    // slow, so the ones that met their budget are named too.
    if passed.is_empty() {
        lines.push(format!(
            "  0 of {} gated series met its target.",
            gated.len()
        ));
    } else {
        lines.push(format!(
            "  {} of {} gated series met its target: {}.",
            passed.len(),
            gated.len(),
            passed.join(", ")
        ));
    }
    if !r.root_causes.is_empty() {
        lines.push(format!(
            "  start at: {} — every other failure above is downstream of {}.",
            r.root_causes.join(", "),
            if r.root_causes.len() == 1 {
                "it"
            } else {
                "these"
            }
        ));
    }
    lines
}

/// Summarize every series and adjudicate the verdict. Split out of
/// [`report_and_maybe_gate`] so the containment tests can assert on the whole
/// report rather than on an exit code that compresses it to one bit.
fn build_report(
    thresholds: &ThresholdsUs,
    loaded: &Loaded,
    qualification: Option<DeviceQualification>,
) -> Report {
    let mut series = BTreeMap::new();
    let mut any_fail = false;
    let device_qualified = qualification
        .as_ref()
        .is_none_or(|qualification| qualification.qualified);
    for (i, key) in SERIES_KEYS.iter().enumerate() {
        let hist = &loaded.histograms[i];
        let (p50_us, p99_us, max_us) = if hist.total() == 0 {
            (None, None, None)
        } else {
            (
                Some(hist.p50().as_micros() as u64),
                Some(hist.p99().as_micros() as u64),
                hist.max().map(|d| d.as_micros() as u64),
            )
        };
        let threshold_us = threshold_for(thresholds, key);
        let gate = match (threshold_us, p99_us) {
            // Ungated series are reported for attribution only: a missing
            // `gateway_bulk_server_ms` must not fail a run the way a missing
            // D16 series does.
            (None, _) => SeriesGate::NotGated,
            (Some(_), None) => SeriesGate::MissingData,
            (Some(_), Some(_)) if !device_qualified => SeriesGate::Unqualified,
            (Some(t), Some(p99)) if p99 <= t => SeriesGate::Pass,
            (Some(_), Some(_)) => SeriesGate::Fail,
        };
        debug_assert_eq!(i < NUM_GATED, threshold_us.is_some());
        if matches!(gate, SeriesGate::Fail | SeriesGate::MissingData) {
            any_fail = true;
        }
        series.insert(
            *key,
            SeriesSummary {
                n: hist.total(),
                p50_us,
                p99_us,
                max_us,
                threshold_us,
                gate,
                unit: SeriesUnit::of(key),
                contains: contained_by(key),
                // Filled in below, once every series' own verdict is known: a
                // series' role depends on the verdicts of the series it
                // contains, which may sort after it.
                failure_role: None,
            },
        );
    }

    // Second pass: classify the failures against the containment order. This
    // reads verdicts and writes only `failure_role`, so no verdict, threshold
    // or exit code can move here — see `containment_changes_no_verdict`.
    let failing: BTreeSet<&str> = series
        .iter()
        .filter(|(_, s)| matches!(s.gate, SeriesGate::Fail | SeriesGate::MissingData))
        .map(|(key, _)| *key)
        .collect();
    let mut root_causes = Vec::new();
    for key in SERIES_KEYS {
        if !failing.contains(key) {
            continue;
        }
        let inner_failing: Vec<&'static str> = contained_by(key)
            .iter()
            .copied()
            .filter(|inner| failing.contains(inner))
            .collect();
        let role = if inner_failing.is_empty() {
            root_causes.push(key);
            FailureRole::Root
        } else {
            FailureRole::Consequence { of: inner_failing }
        };
        if let Some(summary) = series.get_mut(key) {
            summary.failure_role = Some(role);
        }
    }

    Report {
        records: loaded.records,
        malformed: loaded.malformed,
        unknown_series: loaded.unknown_series,
        unknown_series_names: loaded.unknown_series_names.iter().cloned().collect(),
        gate: if any_fail {
            GateVerdict::Fail
        } else if device_qualified {
            GateVerdict::Pass
        } else {
            GateVerdict::Unqualified
        },
        device_qualification: qualification,
        run: loaded.run_ctx.clone(),
        series,
        root_causes,
    }
}

fn print_human(r: &Report) {
    println!("P2 latency dashboard");
    println!("====================");
    println!(
        "records: {} ({} malformed, {} unknown series)",
        r.records, r.malformed, r.unknown_series
    );
    if !r.unknown_series_names.is_empty() {
        println!("unknown series: {}", r.unknown_series_names.join(", "));
    }
    if let Some(run) = &r.run {
        println!();
        println!("run context:");
        if let Some(g) = &run.gateway {
            println!("  gateway:  {g}");
        }
        if let Some(a) = &run.addr {
            println!("  addr:     {a}");
        }
        if let (Some(e), Some(c), Some(s)) = (run.entities, run.cells, run.sessions) {
            println!("  entities: {e}   cells: {c}   sessions: {s}");
        }
        if let (Some(d), Some(hz)) = (run.duration_secs, run.diff_hz) {
            println!("  duration: {d} s   diff_hz: {hz}");
        }
    }
    println!();
    println!(
        "{:<34} {:>10} {:>9} {:>9} {:>9} {:>5} {:>11} {:>9}",
        "series", "n", "p50", "p99", "max", "unit", "threshold", "gate"
    );
    println!("{}", "-".repeat(100));
    for (key, s) in &r.series {
        let p50 = s.p50_us.map_or_else(|| "—".into(), |v| v.to_string());
        let p99 = s.p99_us.map_or_else(|| "—".into(), |v| v.to_string());
        let max = s.max_us.map_or_else(|| "—".into(), |v| v.to_string());
        let gate = match s.gate {
            SeriesGate::Pass => "PASS",
            SeriesGate::Fail => "FAIL",
            SeriesGate::MissingData => "MISSING",
            SeriesGate::Unqualified => "UNQUALIFIED",
            SeriesGate::NotGated => "—",
        };
        let threshold = s.threshold_us.map_or_else(|| "—".into(), |v| v.to_string());
        println!(
            "{:<34} {:>10} {:>9} {:>9} {:>9} {:>5} {:>11} {:>9}",
            key,
            s.n,
            p50,
            p99,
            max,
            s.unit.label(),
            threshold,
            gate
        );
    }
    println!();
    print_containment(r);
    // p50/p99 are histogram bucket *upper bounds*; max is the exact observed
    // value. So `max` legitimately reads below `p99` whenever every sample sat
    // low inside its bucket. Say so, or the table looks impossible.
    println!("p50/p99 are bucket upper bounds; max is exact, so max < p99 is normal.");
    println!();
    println!(
        "GATE: {}",
        match r.gate {
            GateVerdict::Pass => "PASS",
            GateVerdict::Fail => "FAIL",
            GateVerdict::Unqualified => "UNQUALIFIED (D19 device preflight failed)",
        }
    );
}

/// Print the containment structure of the verdict: which failures are root
/// causes and which are consequences of them.
///
/// Printed under the table rather than folded into it, because it is not a
/// per-series fact — it is the relation *between* the rows, and a table
/// column cannot say "this row cannot pass while that row fails".
///
/// Silent on a passing run: containment only ever answers "which of these
/// failures do I look at", and with no failures there is no question.
fn print_containment(r: &Report) {
    let failures: Vec<(&&str, &SeriesSummary)> = r
        .series
        .iter()
        .filter(|(_, s)| s.failure_role.is_some())
        .collect();
    if failures.is_empty() {
        return;
    }
    println!("verdict structure (these gates are nested, not independent):");
    for key in &r.root_causes {
        println!("  {key:<22} ROOT CAUSE — nothing it contains is also failing");
    }
    for (key, s) in &failures {
        if let Some(FailureRole::Consequence { of }) = &s.failure_role {
            println!(
                "  {:<22} consequence of {} — it contains that span, so it cannot pass while it fails",
                key,
                of.join(", ")
            );
        }
    }
    println!();
    let (n, roots) = (failures.len(), r.root_causes.len());
    println!(
        "{n} failing series, {roots} root cause(s): {}.",
        r.root_causes.join(", ")
    );
    println!("Every failing series still fails and every threshold is unchanged; this");
    println!("says which one to open first, not which one to excuse.");
    println!();
}

#[cfg(test)]
mod tests {
    use super::*;
    use orrery_persist_client::latency::{SERIES_CLIENT_SEND_BUFFER, SERIES_GATEWAY_BULK_SERVER};

    /// The two server-side spans persistd added alongside the bulk one. Named
    /// through `UNGATED_SERIES` rather than imported: the client crate
    /// re-exports the array, and the array is the contract this gate folds.
    const SERIES_GATEWAY_INTENT_SERVER: &str = UNGATED_SERIES[1];
    const SERIES_GATEWAY_AREA_FIRST_PAGE_SERVER: &str = UNGATED_SERIES[2];
    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// Unique temp path per test invocation so parallel tests do not collide.
    fn tmp(name: &str) -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "p2-dashboard-test-{name}-{}-{n}.jsonl",
            std::process::id()
        ))
    }

    fn write_jsonl(path: &std::path::Path, lines: &[serde_json::Value]) {
        let mut f = File::create(path).expect("create temp jsonl");
        for line in lines {
            writeln!(f, "{}", serde_json::to_string(line).unwrap()).unwrap();
        }
    }

    fn sample(series: &str, value_us: u64) -> serde_json::Value {
        serde_json::json!({"type": "sample", "series": series, "value_us": value_us})
    }

    fn sample_batch(series: &str, value_us: u64, count: u64) -> serde_json::Value {
        serde_json::json!({"type": "sample_batch", "series": series, "value_us": value_us, "count": count})
    }

    fn run_header() -> serde_json::Value {
        serde_json::json!({
            "type": "run_header",
            "run": {
                "gateway": "ea4a6c63e29c520abef5507b132ec5f9954776aebebe7b92421eea691446d22c",
                "addr": "EndpointAddr { id: .., addrs: [] }",
                "entities": 1000u64,
                "cells": 128u64,
                "sessions": 6u64,
                "diff_hz": 2.0,
                "intent_mix": {"trade": 0.02, "craft": 0.01},
                "duration_secs": 30u64
            }
        })
    }

    /// Conforming test data: every series' samples sit below its D16 target
    /// (`gates/p2-dashboard/testdata/demo.jsonl` mirrors this shape).
    fn conforming() -> Vec<serde_json::Value> {
        let mut v = vec![run_header()];
        // journal_commit < 2 ms.
        for i in 0..100u64 {
            v.push(sample("journal_commit_ms", 400 + (i % 3) * 250));
        }
        // bulk_ack < 5 ms.
        for i in 0..100u64 {
            v.push(sample("bulk_ack_ms", 1_500 + (i % 2) * 1_500));
        }
        // intent_commit < 10 ms.
        for i in 0..100u64 {
            v.push(sample("intent_commit_ms", 4_500 + (i % 2) * 2_500));
        }
        // area_first_page < 50 ms.
        for i in 0..100u64 {
            v.push(sample("area_first_page_ms", 12_000 + (i % 2) * 16_000));
        }
        v
    }

    fn gate_cli(files: Vec<PathBuf>) -> Cli {
        Cli {
            files,
            json: false,
            gate: true,
            device_qualification: None,
            journal_commit_ms: ThresholdsUs::D16.journal_commit_ms,
            bulk_ack_ms: ThresholdsUs::D16.bulk_ack_ms,
            intent_commit_ms: ThresholdsUs::D16.intent_commit_ms,
            area_first_page_ms: ThresholdsUs::D16.area_first_page_ms,
        }
    }

    /// Run the exact binary gate path on `lines` and return the exit code.
    /// Errors are surfaced as exit code 2 here (no anyhow propagation) so the
    /// tests assert on the process-visible contract only.
    fn run_gate_on(lines: Vec<serde_json::Value>) -> ExitCode {
        let path = tmp("gate");
        write_jsonl(&path, &lines);
        let cli = gate_cli(vec![path.clone()]);
        let thresholds = ThresholdsUs::D16;
        let loaded = load(&cli.files).expect("test jsonl loads");
        let exit = report_and_maybe_gate(&cli, &thresholds, &loaded, None);
        let _ = std::fs::remove_file(&path);
        exit
    }

    /// Build the full report for `lines` the way the binary would.
    fn report_on(lines: Vec<serde_json::Value>) -> Report {
        report_on_with_qualification(lines, None)
    }

    fn report_on_with_qualification(
        lines: Vec<serde_json::Value>,
        qualification: Option<DeviceQualification>,
    ) -> Report {
        let path = tmp("report");
        write_jsonl(&path, &lines);
        let loaded = load(std::slice::from_ref(&path)).expect("test jsonl loads");
        let report = build_report(&ThresholdsUs::D16, &loaded, qualification);
        let _ = std::fs::remove_file(&path);
        report
    }

    fn qualification(qualified: bool) -> DeviceQualification {
        DeviceQualification {
            kind: "d19_device_qualification".to_string(),
            method: "fio_job_a".to_string(),
            command: "fio --name=jobA --directory=<journal-filesystem> --rw=write --bs=8k --fdatasync=1 --numjobs=2 --rate_iops=470 --runtime=120 --time_based --size=256m --output-format=json --unlink=1".to_string(),
            data_path: "/journal-device".to_string(),
            fio_version: Some("fio-3.42".to_string()),
            required: DeviceQualificationRequirement {
                jobs: 2,
                runtime_seconds: 120,
                block_size_bytes: 8192,
                offered_rate_iops_per_job: 470.0,
                minimum_rate_iops_per_job: 469.0,
                sync_max_ms_below: 1.0,
                reference_barriers_per_s: 940.0,
                reference_sync_p99_ms: 0.185,
                reference_sync_max_ms: 0.509,
            },
            measured: Some(DeviceQualificationMeasurement {
                aggregate_barriers_per_s: if qualified { 940.0 } else { 674.6 },
                worst_sync_p99_ms: if qualified { 0.185 } else { 7.045 },
                worst_sync_max_ms: if qualified { 0.509 } else { 104.120 },
                jobs: vec![
                    DeviceQualificationJob {
                        iops: if qualified { 470.0 } else { 337.3 },
                        sync_p99_ms: if qualified { 0.185 } else { 7.045 },
                        sync_max_ms: if qualified { 0.509 } else { 104.120 },
                    },
                    DeviceQualificationJob {
                        iops: if qualified { 470.0 } else { 337.3 },
                        sync_p99_ms: if qualified { 0.170 } else { 7.045 },
                        sync_max_ms: if qualified { 0.480 } else { 95.393 },
                    },
                ],
            }),
            qualified,
            reason: (!qualified).then(|| {
                "each job must sustain the offered rate and remain below the maximum".to_string()
            }),
        }
    }

    /// A run in which the journal committer is the thing that is broken: the
    /// commit p99 lands at 100 ms, and the round trips that contain a commit
    /// inherit it. This is the clean-box shape the D16 report used to present
    /// as three unrelated failures.
    fn slow_committer() -> Vec<serde_json::Value> {
        let mut v = vec![run_header()];
        for _ in 0..100u64 {
            v.push(sample(SERIES_JOURNAL_COMMIT, 100_000));
            v.push(sample(SERIES_BULK_ACK, 100_500));
            v.push(sample(SERIES_INTENT_COMMIT, 101_000));
            v.push(sample(SERIES_AREA_FIRST_PAGE, 3_000));
        }
        v
    }

    #[test]
    fn nested_failures_report_one_root_cause_and_two_consequences() {
        let r = report_on(slow_committer());
        assert_eq!(
            r.root_causes,
            vec![SERIES_JOURNAL_COMMIT],
            "the commit is the only failure that is not explained by another"
        );
        assert_eq!(
            r.series[SERIES_JOURNAL_COMMIT].failure_role,
            Some(FailureRole::Root)
        );
        for outer in [SERIES_BULK_ACK, SERIES_INTENT_COMMIT] {
            assert_eq!(
                r.series[outer].failure_role,
                Some(FailureRole::Consequence {
                    of: vec![SERIES_JOURNAL_COMMIT]
                }),
                "{outer} contains the commit that failed"
            );
        }
    }

    /// The whole point of the containment layer is that it is presentation.
    /// Every per-series verdict, every threshold and the overall gate are what
    /// they were before it existed — a consequence still FAILs, and the gate
    /// still exits non-zero.
    #[test]
    fn containment_changes_no_verdict() {
        let r = report_on(slow_committer());
        for key in [SERIES_JOURNAL_COMMIT, SERIES_BULK_ACK, SERIES_INTENT_COMMIT] {
            assert_eq!(
                r.series[key].gate,
                SeriesGate::Fail,
                "{key} is a consequence but must still fail"
            );
        }
        assert_eq!(r.series[SERIES_AREA_FIRST_PAGE].gate, SeriesGate::Pass);
        assert_eq!(
            r.series[SERIES_BULK_ACK].threshold_us,
            Some(ThresholdsUs::D16.bulk_ack_ms)
        );
        assert_eq!(r.gate, GateVerdict::Fail);
        assert_ne!(run_gate_on(slow_committer()), ExitCode::SUCCESS);
    }

    /// Containment classifies, it does not excuse: when the span a dependent
    /// contains is healthy, the dependent's own excess is its own, and it is
    /// reported as the root.
    #[test]
    fn a_dependent_failing_over_a_healthy_inner_span_is_its_own_root() {
        let mut lines = conforming();
        for _ in 0..300u64 {
            lines.push(sample(SERIES_BULK_ACK, 20_000));
        }
        let r = report_on(lines);
        assert_eq!(r.series[SERIES_JOURNAL_COMMIT].gate, SeriesGate::Pass);
        assert_eq!(r.root_causes, vec![SERIES_BULK_ACK]);
        assert_eq!(
            r.series[SERIES_BULK_ACK].failure_role,
            Some(FailureRole::Root)
        );
    }

    /// A passing run has no failures to order, so it makes no containment
    /// claim at all.
    #[test]
    fn a_passing_run_names_no_root_cause() {
        let r = report_on(conforming());
        assert!(r.root_causes.is_empty());
        assert!(r.series.values().all(|s| s.failure_role.is_none()));
    }

    /// The three transport-boundary spans the gateway emits are folded,
    /// counted and never gated — like every other attribution series.
    #[test]
    fn gateway_boundary_series_are_folded_and_never_gate() {
        let mut lines = conforming();
        for key in GATEWAY_BOUNDARY_SERIES {
            for _ in 0..50u64 {
                lines.push(sample_batch(key, 750_000, 1));
            }
        }
        let r = report_on(lines);
        assert_eq!(r.unknown_series, 0, "{:?}", r.unknown_series_names);
        for key in GATEWAY_BOUNDARY_SERIES {
            let summary = &r.series[key];
            assert_eq!(summary.n, 50, "{key} was not folded");
            assert_eq!(summary.gate, SeriesGate::NotGated, "{key} must not gate");
            assert!(summary.threshold_us.is_none());
        }
        assert_eq!(r.gate, GateVerdict::Pass, "attribution never fails a run");
    }

    /// The two send-buffer gauges are bytes on a lattice built for
    /// microseconds. The report has to say so, or a reader multiplies a byte
    /// count by 1000 in their head.
    #[test]
    fn send_buffer_gauges_are_reported_in_bytes_and_latencies_in_micros() {
        let mut lines = conforming();
        lines.push(sample_batch(SERIES_CLIENT_SEND_BUFFER, 500, 1));
        let r = report_on(lines);
        assert_eq!(r.series[SERIES_CLIENT_SEND_BUFFER].unit, SeriesUnit::Bytes);
        assert_eq!(
            r.series[GATEWAY_BOUNDARY_SERIES[2]].unit,
            SeriesUnit::Bytes,
            "gateway_send_buffer_bytes is the server-side gauge"
        );
        assert_eq!(r.series[SERIES_BULK_ACK].unit, SeriesUnit::Us);
        assert_eq!(r.series[GATEWAY_BOUNDARY_SERIES[0]].unit, SeriesUnit::Us);
    }

    #[test]
    fn gate_passes_on_conforming_testdata() {
        assert_eq!(run_gate_on(conforming()), ExitCode::SUCCESS);
    }

    #[test]
    fn unqualified_device_withholds_every_latency_verdict_but_keeps_measurements() {
        let r = report_on_with_qualification(slow_committer(), Some(qualification(false)));
        assert_eq!(r.gate, GateVerdict::Unqualified);
        assert!(
            r.root_causes.is_empty(),
            "an unjudged run has no latency root cause"
        );
        for key in GATED_SERIES {
            let summary = &r.series[key];
            assert_eq!(summary.gate, SeriesGate::Unqualified, "{key} was judged");
            assert!(summary.p99_us.is_some(), "{key}'s measurement disappeared");
            assert!(
                summary.threshold_us.is_some(),
                "{key}'s D16 target disappeared"
            );
        }
        let qualification = r
            .device_qualification
            .as_ref()
            .expect("the refusal must travel with the report");
        let measured = qualification.measured.as_ref().expect("fio did run");
        assert_eq!(measured.worst_sync_p99_ms, 7.045);
        assert_eq!(measured.worst_sync_max_ms, 104.120);
        assert_eq!(qualification.required.sync_max_ms_below, 1.0);
        let text = gate_unqualified_report(&r).join("\n");
        for needle in [
            "UNQUALIFIED",
            "7.045",
            "104.120",
            "< 1.000",
            "0.185",
            "0.509",
        ] {
            assert!(text.contains(needle), "{needle:?} absent from:\n{text}");
        }
    }

    #[test]
    fn injected_qualified_device_preserves_the_existing_failure_path() {
        let r = report_on_with_qualification(slow_committer(), Some(qualification(true)));
        assert_eq!(r.gate, GateVerdict::Fail);
        assert_eq!(r.series[SERIES_JOURNAL_COMMIT].gate, SeriesGate::Fail);
        let text = gate_failure_report(&r).join("\n");
        assert!(text.contains(SERIES_JOURNAL_COMMIT));
        assert!(text.contains("ROOT CAUSE"));
    }

    #[test]
    fn missing_latency_data_still_fails_on_an_unqualified_device() {
        let lines = conforming()
            .into_iter()
            .filter(|v| v.get("series").and_then(|s| s.as_str()) != Some(SERIES_INTENT_COMMIT))
            .collect::<Vec<_>>();
        let r = report_on_with_qualification(lines, Some(qualification(false)));
        assert_eq!(r.gate, GateVerdict::Fail);
        assert_eq!(r.series[SERIES_INTENT_COMMIT].gate, SeriesGate::MissingData);
        for key in [
            SERIES_JOURNAL_COMMIT,
            SERIES_BULK_ACK,
            SERIES_AREA_FIRST_PAGE,
        ] {
            assert_eq!(r.series[key].gate, SeriesGate::Unqualified);
        }
        let text = gate_failure_report(&r).join("\n");
        assert!(text.contains(SERIES_INTENT_COMMIT));
        for key in [
            SERIES_JOURNAL_COMMIT,
            SERIES_BULK_ACK,
            SERIES_AREA_FIRST_PAGE,
        ] {
            assert!(
                !text.contains(key),
                "unqualified {key} was rendered as a latency failure:\n{text}"
            );
        }
    }

    #[test]
    fn gate_fails_when_one_series_regresses() {
        // Mutate the bulk-ack series so its p99 lands above the 5 ms target.
        // 150 extra samples at 20 ms against a 100-sample base means p99 is
        // firmly in the 20 ms bucket — a real regression, not a tail spike.
        let mut lines = conforming();
        for _ in 0..150u64 {
            lines.push(sample("bulk_ack_ms", 20_000));
        }
        assert_ne!(run_gate_on(lines), ExitCode::SUCCESS);
    }

    #[test]
    fn gate_targets_p99_not_max() {
        // One 500 ms spike in a 100-sample series leaves p99 below threshold
        // (sample rank 99 of 101 is a conforming sample), so the run must
        // still pass. This pins the gate to p99 — reading max instead would
        // fail this test.
        let mut lines = conforming();
        lines.push(sample("bulk_ack_ms", 500_000));
        assert_eq!(run_gate_on(lines), ExitCode::SUCCESS);
    }

    #[test]
    fn missing_series_fails_the_gate() {
        // The D16 demo criterion requires all four series measured; a run
        // with no intent samples cannot pass by omission.
        let lines = conforming()
            .into_iter()
            .filter(|v| v.get("series").and_then(|s| s.as_str()) != Some("intent_commit_ms"))
            .collect::<Vec<_>>();
        assert_ne!(run_gate_on(lines), ExitCode::SUCCESS);
    }

    #[test]
    fn sample_batch_folds_exactly_like_repeated_samples() {
        // Rewritten with the shared lattice. This test used to assert that
        // 100 journal samples at 1 500 µs report a p99 of 2 000 µs — which is
        // exactly the D16 threshold this gate compares `p99 <= t` against, so
        // it encoded the collapse as correct. On the shared boundaries the
        // 1.5 ms band has its own bucket and the reported p99 is 1 500 µs.
        let path = tmp("batch");
        let lines = vec![
            sample_batch("journal_commit_ms", 1_500, 100),
            sample_batch("bulk_ack_ms", 3_000, 100),
            sample_batch("intent_commit_ms", 7_000, 100),
            sample_batch("area_first_page_ms", 28_000, 100),
        ];
        write_jsonl(&path, &lines);
        let loaded = load(std::slice::from_ref(&path)).unwrap();
        assert_eq!(loaded.histograms[0].total(), 100);
        assert_eq!(loaded.histograms[0].p99(), Duration::from_micros(1_500));
        assert!(
            loaded.histograms[0].p99() < Duration::from_micros(ThresholdsUs::D16.journal_commit_ms)
        );
        assert_eq!(loaded.histograms[3].total(), 100);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn the_journal_band_resolves_instead_of_pinning_to_the_threshold() {
        // Rewritten. The old version asserted that a 1 500 µs sample reports
        // p50 == p99 == 2 000 µs, the D16 journal threshold — the defect, not
        // the contract: with 1 ms and 2 ms adjacent, *every* journal p99
        // anywhere in the 1.0–2.0 ms band read out as the threshold and
        // passed on `p99 <= threshold`. Three points across that band must
        // now report three different numbers, all below the threshold.
        let readings: Vec<u64> = [1_100u64, 1_400, 1_900]
            .into_iter()
            .map(|micros| {
                let mut hist = LatencyHistogram::new();
                for _ in 0..100 {
                    hist.record(Duration::from_micros(micros));
                }
                assert_eq!(hist.max(), Some(Duration::from_micros(micros)));
                hist.p99().as_micros() as u64
            })
            .collect();
        assert_eq!(readings, vec![1_250, 1_500, 2_000]);
        assert!(readings[0] < readings[1] && readings[1] < readings[2]);
        assert_eq!(LatencyHistogram::new().p99(), Duration::ZERO);
    }

    #[test]
    fn gateway_bulk_server_series_is_folded_and_never_gates() {
        // The fifth series persistd emits. It must be recognized (not counted
        // as unknown), reported, and inert in the verdict.
        let path = tmp("fifth");
        let mut lines = conforming();
        for _ in 0..50u64 {
            lines.push(sample(SERIES_GATEWAY_BULK_SERVER, 300_000));
        }
        write_jsonl(&path, &lines);
        let cli = gate_cli(vec![path.clone()]);
        let loaded = load(&cli.files).expect("test jsonl loads");
        assert_eq!(loaded.unknown_series, 0);
        let idx = SERIES_KEYS
            .iter()
            .position(|&k| k == SERIES_GATEWAY_BULK_SERVER)
            .expect("the fifth series has a slot");
        assert_eq!(loaded.histograms[idx].total(), 50);
        // 300 ms would fail every D16 threshold; the run still passes.
        assert_eq!(
            report_and_maybe_gate(&cli, &ThresholdsUs::D16, &loaded, None),
            ExitCode::SUCCESS
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn server_side_spans_are_folded_separately_from_the_gated_round_trips() {
        // The failure this guards is silent and one-directional: a server
        // span is strictly shorter than the client round trip it attributes,
        // so folding it into the gated series would *lower* the p99 and pass
        // a gate that measured nothing. Same artifact, same fold, four
        // distinct histograms.
        let path = tmp("server-spans");
        let mut lines = conforming();
        for _ in 0..40u64 {
            lines.push(sample(SERIES_GATEWAY_INTENT_SERVER, 200));
            lines.push(sample(SERIES_GATEWAY_AREA_FIRST_PAGE_SERVER, 500));
        }
        write_jsonl(&path, &lines);
        let cli = gate_cli(vec![path.clone()]);
        let loaded = load(&cli.files).expect("test jsonl loads");
        assert_eq!(loaded.unknown_series, 0, "both names are in the contract");

        let slot = |key: &str| {
            SERIES_KEYS
                .iter()
                .position(|&k| k == key)
                .expect("every contract series has a slot")
        };
        assert_eq!(
            loaded.histograms[slot(SERIES_GATEWAY_INTENT_SERVER)].total(),
            40
        );
        assert_eq!(
            loaded.histograms[slot(SERIES_GATEWAY_AREA_FIRST_PAGE_SERVER)].total(),
            40
        );

        // The gated p99s are exactly what `conforming()` alone produces: the
        // server spans landed nowhere near them.
        let baseline = {
            let base = tmp("server-spans-baseline");
            write_jsonl(&base, &conforming());
            let loaded = load(std::slice::from_ref(&base)).expect("baseline loads");
            let p99s: Vec<_> = GATED_SERIES
                .iter()
                .map(|&key| loaded.histograms[slot(key)].p99())
                .collect();
            let _ = std::fs::remove_file(base);
            p99s
        };
        let observed: Vec<_> = GATED_SERIES
            .iter()
            .map(|&key| loaded.histograms[slot(key)].p99())
            .collect();
        assert_eq!(observed, baseline, "a server span moved a gated p99");

        assert_eq!(
            report_and_maybe_gate(&cli, &ThresholdsUs::D16, &loaded, None),
            ExitCode::SUCCESS
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn client_attribution_series_are_folded_counted_and_never_gated() {
        // The four client-side stage series ride the same artifact as the
        // gated ones. Three things have to hold at once, and each has broken
        // a gate run before:
        //   1. they must not be counted as unknown - the harness's python
        //      check fails the whole run on `unknown_series`;
        //   2. they must not acquire a threshold - a stage series failing the
        //      gate would fail a run the server is meeting;
        //   3. folding them must not move a gated p99.
        let mut lines = conforming();
        let slot = |key: &str| SERIES_KEYS.iter().position(|&k| k == key).unwrap();
        let gated_before = {
            let path = tmp("client-attr-base");
            write_jsonl(&path, &lines);
            let loaded = load(std::slice::from_ref(&path)).expect("baseline loads");
            let p99s: Vec<_> = GATED_SERIES
                .iter()
                .map(|&key| loaded.histograms[slot(key)].p99())
                .collect();
            let _ = std::fs::remove_file(&path);
            p99s
        };

        // Stage samples large enough to fail every D16 threshold if gated,
        // and to dominate any histogram they were wrongly folded into.
        for key in CLIENT_UNGATED_SERIES {
            lines.push(sample_batch(key, 300_000, 100));
        }
        let path = tmp("client-attr");
        write_jsonl(&path, &lines);
        let cli = gate_cli(vec![path.clone()]);
        let loaded = load(&cli.files).expect("test jsonl loads");

        assert_eq!(
            loaded.unknown_series, 0,
            "client attribution read as unknown ({:?}); the P2 harness fails the whole run on that",
            loaded.unknown_series_names
        );
        for key in CLIENT_UNGATED_SERIES {
            assert_eq!(
                loaded.histograms[slot(key)].total(),
                100,
                "{key} was not folded"
            );
            assert!(
                threshold_for(&ThresholdsUs::D16, key).is_none(),
                "{key} acquired a D16 threshold"
            );
        }
        let gated_after: Vec<_> = GATED_SERIES
            .iter()
            .map(|&key| loaded.histograms[slot(key)].p99())
            .collect();
        assert_eq!(
            gated_after, gated_before,
            "a client stage moved a gated p99"
        );

        assert_eq!(
            report_and_maybe_gate(&cli, &ThresholdsUs::D16, &loaded, None),
            ExitCode::SUCCESS,
            "300 ms of ungated client attribution failed a conforming run"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn every_ungated_series_is_reported_and_none_of_them_gates() {
        for key in UNGATED_SERIES {
            assert!(
                threshold_for(&ThresholdsUs::D16, key).is_none(),
                "{key} acquired a D16 threshold"
            );
            assert!(
                SERIES_KEYS.contains(&key),
                "{key} is in the contract but has no slot in the fold"
            );
        }
        assert_eq!(NUM_GATED, GATED_SERIES.len());
    }

    #[test]
    fn an_absent_fifth_series_does_not_fail_the_gate() {
        // The nightly artifact contains it only when persistd was configured
        // to export it, so absence must read as `not_gated`, not `missing`.
        assert_eq!(run_gate_on(conforming()), ExitCode::SUCCESS);
    }

    #[test]
    fn a_series_outside_the_contract_is_counted_rather_than_discarded() {
        // A typo in a gated name used to vanish into the default arm while
        // the report printed zero malformed records.
        let path = tmp("unknown");
        let mut lines = conforming();
        lines.push(sample("bulk_ack_us", 1_000));
        lines.push(sample_batch("journal_commit_millis", 1_000, 7));
        write_jsonl(&path, &lines);
        let loaded = load(std::slice::from_ref(&path)).expect("test jsonl loads");
        assert_eq!(loaded.unknown_series, 2, "both records are counted once");
        assert_eq!(loaded.malformed, 0, "they parse; they are just not ours");
        // Counting them is diagnostic, not gating.
        let cli = gate_cli(vec![path.clone()]);
        assert_eq!(
            report_and_maybe_gate(&cli, &ThresholdsUs::D16, &loaded, None),
            ExitCode::SUCCESS
        );
        let _ = std::fs::remove_file(path);
    }

    /// The failure path has to name the series, the measurement and the
    /// target. Under `--gate --json` the report itself goes to a file (the P2
    /// harness redirects stdout into `latency-report.json`), so these lines
    /// are the whole of what a nightly log gets to say — and for four
    /// consecutive nights they said "one or more D16 series missed its p99
    /// target" and nothing else.
    #[test]
    fn a_failing_gate_names_every_series_that_missed_with_its_numbers() {
        let r = report_on(slow_committer());
        let text = gate_failure_report(&r).join("\n");

        // The root cause, its measurement, its D16 target and the margin.
        assert!(
            text.contains(SERIES_JOURNAL_COMMIT),
            "the failing series is not named:\n{text}"
        );
        let s = &r.series[SERIES_JOURNAL_COMMIT];
        let p99 = s.p99_us.expect("the slow committer has samples");
        let target = s.threshold_us.expect("journal_commit_ms is gated");
        assert!(
            text.contains(&p99.to_string()),
            "the measured p99 ({p99}) is not printed:\n{text}"
        );
        assert!(
            text.contains(&target.to_string()),
            "the D16 target ({target}) is not printed:\n{text}"
        );
        assert!(
            text.contains(&format!("+{}", p99 - target)),
            "the margin (+{}) is not printed:\n{text}",
            p99 - target
        );

        // Both consequences are named too - a reader must see the whole
        // failing set, not just the root.
        for key in [SERIES_BULK_ACK, SERIES_INTENT_COMMIT] {
            assert!(text.contains(key), "{key} failed but is not named:\n{text}");
        }
        // And the series that met its budget, so "3 of 4" is legible as
        // three failures rather than as a total collapse.
        assert!(
            text.contains(SERIES_AREA_FIRST_PAGE),
            "the passing series is not accounted for:\n{text}"
        );
        assert!(
            text.contains("3 of 4"),
            "the count of missed series is wrong or absent:\n{text}"
        );
        // The root cause is still called out by name, and the exit code is
        // untouched by any of this.
        assert!(
            text.contains("ROOT CAUSE"),
            "the root cause is not marked:\n{text}"
        );
        assert_ne!(run_gate_on(slow_committer()), ExitCode::SUCCESS);
    }

    /// A series the run never sampled fails the gate (`missing_series_fails_the_gate`),
    /// and the failure text has to say which one and that it was never
    /// measured — a `p99` of `0` would read as a series that was fast.
    #[test]
    fn an_unmeasured_series_is_named_as_unmeasured_not_as_fast() {
        let lines = conforming()
            .into_iter()
            .filter(|v| v.get("series").and_then(|s| s.as_str()) != Some(SERIES_INTENT_COMMIT))
            .collect::<Vec<_>>();
        let r = report_on(lines);
        let text = gate_failure_report(&r).join("\n");
        assert!(
            text.contains(SERIES_INTENT_COMMIT),
            "the unmeasured series is not named:\n{text}"
        );
        assert!(
            text.contains("0 samples"),
            "an unmeasured series must not be reported as a measurement:\n{text}"
        );
        assert!(
            text.contains("1 of 4"),
            "one series missed; the header says otherwise:\n{text}"
        );
    }

    /// The failure text is rendering over a report that is already decided.
    /// It reads thresholds and verdicts and writes nothing, so a passing run
    /// has nothing for it to say and every verdict is what it was.
    #[test]
    fn the_failure_text_changes_no_verdict_and_a_passing_run_produces_none() {
        let before = report_on(slow_committer());
        let after = report_on(slow_committer());
        let _ = gate_failure_report(&after);
        for key in SERIES_KEYS {
            assert_eq!(
                before.series[key].gate, after.series[key].gate,
                "{key}'s verdict moved"
            );
            assert_eq!(
                before.series[key].threshold_us, after.series[key].threshold_us,
                "{key}'s threshold moved"
            );
        }
        assert_eq!(before.gate, after.gate);
        // A passing run never reaches the failure path at all.
        let passing = report_on(conforming());
        assert_eq!(passing.gate, GateVerdict::Pass);
        assert_eq!(run_gate_on(conforming()), ExitCode::SUCCESS);
    }
}
