//! (See the module-level documentation on `crate` for the topology and the
//! phase definitions; this module is the harness itself.)

extern crate alloc;

use std::collections::HashMap;
use std::net::{TcpListener, TcpStream};
use std::sync::{Condvar, Mutex};

use alloc::collections::VecDeque;
use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use core::time::Duration;

use orrery_ipc::{
    EntityFrame, EntityInput, FrameBatch, InputBatch, QuantizedTransform, SidecarToEngine,
};
use orrery_protocol::{InterpBasis, LatticePoint, PersistId, QuantizedDir, Tick};
use serde::Serialize;

use crate::{
    clock_name, decode_envelope, encode_envelope, monotonic_now_ns, set_nodelay, sleep_until_ns,
    FrameReader, FrameWriter, SeqEvent, SeqWatcher, TimerResolution,
};

/// Sequence values at or above this bit belong to harness control traffic.
///
/// Control frames never pass through a [`SeqWatcher`], so they cannot pollute
/// the data stream's drop accounting.
pub const CONTROL_SEQ: u64 = 1 << 63;

/// The one-byte payload tags harness control frames use. All are below
/// `b'O'` (`0x4F`), so a control frame can never be mistaken for an
/// `orrery_ipc` message and vice versa.
mod tag {
    /// Observer → sidecar: 8-byte payload, echoed verbatim.
    pub const PROBE: u8 = 0x01;
    /// Sidecar → observer: `[u64 probe_seq][u64 t_pa][u64 orig_t_send][8 B]`.
    pub const ECHO: u8 = 0x02;
    /// Sidecar → observer: `[u32 entities][u32 hz]`.
    pub const HELLO: u8 = 0x10;
    /// Observer → sidecar: `[u64 start_ns][u64 ticks_total]`.
    pub const READY: u8 = 0x11;
    /// Observer → sidecar: run complete, send the timing table.
    pub const DONE: u8 = 0x20;
    /// Sidecar → observer: `[u32 count][count × (u64 seq, u64 t1, u64 t2)]`.
    pub const TABLE: u8 = 0x21;
    /// Sidecar → observer: the sidecar's own counters, one `u64` each.
    pub const COUNTERS: u8 = 0x22;
    /// How many table entries ride in one chunk, keeping the frame under the
    /// 1 MiB cap (16 384 × 24 B + header ≈ 393 KB).
    pub const TABLE_CHUNK: usize = 16_384;
    /// Ticks between a churn spawn and the matching despawn.
    pub const CHURN_PERIOD: u64 = 600;
    /// Offset of the despawn inside the churn period.
    pub const CHURN_DESPAWN_OFFSET: u64 = 300;
}

fn put_u64(out: &mut Vec<u8>, value: u64) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn put_u32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_le_bytes());
}

/// A cursor over a control-frame body; the first index is the tag byte.
struct Body<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Body<'a> {
    const fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, at: 1 }
    }

    fn u64(&mut self) -> u64 {
        let end = self.at + 8;
        let value = u64::from_le_bytes(
            self.bytes[self.at..end]
                .try_into()
                .expect("control body is well-formed by construction"),
        );
        self.at = end;
        value
    }

    fn u32(&mut self) -> u32 {
        let end = self.at + 4;
        let value = u32::from_le_bytes(
            self.bytes[self.at..end]
                .try_into()
                .expect("control body is well-formed by construction"),
        );
        self.at = end;
        value
    }
}

/// Outgoing messages, tagged by the lane that carries them.
enum OutMsg {
    /// An encoded `EngineToSidecar::Input`, once per tick. Reliable FIFO.
    Input { seq: u64, body: Vec<u8> },
    /// An 8-byte null probe. Expendable, own latest-wins lane, so
    /// measurement traffic can never delay the input stream.
    Probe { seq: u64 },
    /// An encoded `SidecarToEngine::Frames`. Latest-wins: a newer frame
    /// supersedes an older one, and the supersession is counted, never
    /// hidden.
    Frame { seq: u64, body: Vec<u8> },
    /// The answer to a probe, with the shared-clock stamps the observer
    /// needs for both one-way legs. Reliable — it is measurement.
    EchoReply {
        probe_seq: u64,
        t_arrive: u64,
        orig_t_send: u64,
        payload: Vec<u8>,
    },
    /// A churn spawn batch. Reliable FIFO, never droppable: the schema is
    /// push-only and a lost spawn is an entity that never appears.
    Spawn { body: Vec<u8> },
    /// A churn despawn batch. Reliable FIFO.
    Despawn { body: Vec<u8> },
    /// A chunk of the sidecar's per-sequence `(t1, t2)` table, sent at the
    /// end of the run so the return path is never perturbed by it.
    Table { body: Vec<u8> },
    /// The sidecar's counters, sent at the end of the run.
    Counters { body: Vec<u8> },
    /// Handshake: sidecar announces its shape.
    Hello { body: Vec<u8> },
    /// Observer → sidecar: the run is over.
    Done,
    /// Writer-thread shutdown, after everything queued ahead of it.
    Stop,
}

/// A shared wake-up for the writer thread, so a push to *either* lane wakes
/// it immediately. Polling intervals would otherwise land inside the
/// measured columns — the smoke run showed a 174 µs `encode` p50 that was
/// really a 1 ms poll sleeping, which is exactly the kind of instrument
/// artifact #920's lie list warns about.
struct WriterWaker {
    dirty: Mutex<bool>,
    signal: Condvar,
}

impl WriterWaker {
    const fn new() -> Self {
        Self {
            dirty: Mutex::new(false),
            signal: Condvar::new(),
        }
    }

    fn wake(&self) {
        let mut dirty = self.dirty.lock().expect("waker lock");
        *dirty = true;
        drop(dirty);
        self.signal.notify_one();
    }

    /// Block until something is pushed or the timeout elapses. A push that
    /// lands before the wait sets the dirty flag, so no wake-up is lost.
    fn wait(&self, timeout: Duration) {
        let mut dirty = self.dirty.lock().expect("waker lock");
        if !*dirty {
            let (guard, _timed_out) = self
                .signal
                .wait_timeout(dirty, timeout)
                .expect("waker lock");
            dirty = guard;
        }
        *dirty = false;
    }
}

/// FIFO that never drops: inputs, spawn batches, echo replies, control.
///
/// The bound exists so a wedged consumer cannot grow memory without limit;
/// tripping it is a run failure counted in the report, not a policy.
struct ReliableLane {
    queue: Mutex<VecDeque<OutMsg>>,
    waker: Arc<WriterWaker>,
    dropped: AtomicU64,
}

const RELIABLE_CAP: usize = 4096;

impl ReliableLane {
    // Arc wiring keeps this off the const path.
    #[allow(clippy::missing_const_for_fn)]
    fn new(waker: Arc<WriterWaker>) -> Self {
        Self {
            queue: Mutex::new(VecDeque::new()),
            waker,
            dropped: AtomicU64::new(0),
        }
    }

    fn push(&self, message: OutMsg) {
        let mut queue = self.queue.lock().expect("reliable lane lock");
        if queue.len() >= RELIABLE_CAP {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        } else {
            queue.push_back(message);
            drop(queue);
            self.waker.wake();
        }
    }

    fn push_stop(&self) {
        let mut queue = self.queue.lock().expect("reliable lane lock");
        queue.push_back(OutMsg::Stop);
        drop(queue);
        self.waker.wake();
    }

    fn pop(&self) -> Option<OutMsg> {
        self.queue.lock().expect("reliable lane lock").pop_front()
    }

    fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

/// Latest-wins slot of capacity one. A newer value replaces the older one,
/// and the replacement is counted — #920 lie 4's optimistic trap (the reader
/// takes only the freshest frame and the drops hide) is answered by the
/// counter being in the report.
struct LatestWins {
    slot: Mutex<Option<OutMsg>>,
    waker: Arc<WriterWaker>,
    discarded: AtomicU64,
}

impl LatestWins {
    // Arc wiring keeps this off the const path.
    #[allow(clippy::missing_const_for_fn)]
    fn new(waker: Arc<WriterWaker>) -> Self {
        Self {
            slot: Mutex::new(None),
            waker,
            discarded: AtomicU64::new(0),
        }
    }

    fn push(&self, message: OutMsg) {
        let mut slot = self.slot.lock().expect("latest-wins lock");
        if slot.is_some() {
            self.discarded.fetch_add(1, Ordering::Relaxed);
        }
        *slot = Some(message);
        drop(slot);
        self.waker.wake();
    }

    fn take(&self) -> Option<OutMsg> {
        self.slot.lock().expect("latest-wins lock").take()
    }

    fn discarded(&self) -> u64 {
        self.discarded.load(Ordering::Relaxed)
    }
}

/// The simulated world both roles keep: N entities of integer state.
///
/// The step is O(N) integer work standing in for the rules; the extraction is
/// the shape the schema charges for — read per-entity state, build an
/// [`EntityFrame`] with an exact [`InterpBasis`]. Cost parity between the two
/// roles is what makes `extract_inproc` a fair baseline.
struct SimWorld {
    entities: Vec<(PersistId, i64, i64, i64)>,
}

impl SimWorld {
    fn new(entities: u32) -> Self {
        let rows = (1..=u64::from(entities))
            .map(|id| (PersistId::new(id), 0, 0, 0))
            .collect();
        Self { entities: rows }
    }

    fn step(&mut self, tick: u64) {
        let mut i: i64 = 0;
        for (_, x, y, z) in &mut self.entities {
            i += 1;
            *x += (tick % 7).cast_signed() * i - 3 * i;
            *y += (tick % 5).cast_signed() - 2;
            *z += 1;
        }
    }

    fn extract(&self, tick: Tick) -> FrameBatch {
        let predicted = self
            .entities
            .iter()
            .map(|(id, x, y, z)| EntityFrame {
                persist_id: *id,
                transform: QuantizedTransform {
                    translation: LatticePoint::new(*x, *y, *z),
                    forward: QuantizedDir::new(32_000, 0, -700),
                    up: QuantizedDir::new(0, 32_000, 0),
                },
                basis: InterpBasis::exact(tick),
            })
            .collect();
        FrameBatch {
            extracted_at: tick,
            predicted,
            interpolated: Vec::new(),
        }
    }
}

/// The input one tick carries: a single game input, the shape a prediction
/// sidecar actually receives — the local player's sample for this tick, not
/// an N-scaled payload. The N-scaling lives on the frame side.
fn input_batch(tick: u64) -> InputBatch {
    let mut payload = Vec::with_capacity(16);
    put_u64(&mut payload, tick);
    put_u64(&mut payload, tick.wrapping_mul(0x9E37_79B9_7F4A_7C15));
    InputBatch {
        tick: Tick::new(tick),
        inputs: vec![EntityInput {
            target: PersistId::new(1),
            sequence: u16::try_from(tick & 0xFFFF).unwrap_or(0),
            payload,
        }],
    }
}

/// Fold a decoded frame batch into a running checksum, so the apply step is
/// real work the optimizer cannot delete.
fn consume(batch: &FrameBatch, acc: &mut u64) {
    for frame in batch.predicted.iter().chain(batch.interpolated.iter()) {
        let x = u64::try_from(frame.transform.translation.x).unwrap_or(0);
        *acc = acc
            .wrapping_mul(0x100_0000_01B3)
            .wrapping_add(frame.persist_id.0)
            .wrapping_add(x)
            .wrapping_add(u64::from(frame.basis.alpha.0));
    }
}

fn read_loadavg() -> [f64; 3] {
    #[cfg(unix)]
    {
        let Ok(text) = std::fs::read_to_string("/proc/loadavg") else {
            return [0.0; 3];
        };
        let mut fields = text.split_whitespace();
        let mut out = [0.0; 3];
        for slot in &mut out {
            if let Some(value) = fields.next().and_then(|f| f.parse::<f64>().ok()) {
                *slot = value;
            }
        }
        out
    }
    #[cfg(not(unix))]
    {
        [0.0; 3]
    }
}

/// A percentile summary, nearest-rank over the sorted samples.
#[derive(Debug, Clone, Serialize)]
pub struct Summary {
    /// Samples the summary is taken over.
    pub n: usize,
    /// Arithmetic mean, nanoseconds.
    pub mean_ns: f64,
    /// Minimum, nanoseconds.
    pub min_ns: u64,
    /// 50th percentile, nanoseconds.
    pub p50_ns: u64,
    /// 99th percentile, nanoseconds.
    pub p99_ns: u64,
    /// 99.9th percentile, nanoseconds.
    pub p99_9_ns: u64,
    /// Maximum, nanoseconds.
    pub max_ns: u64,
}

impl Summary {
    fn of(mut samples: Vec<u64>) -> Option<Self> {
        if samples.is_empty() {
            return None;
        }
        samples.sort_unstable();
        let n = samples.len();
        #[allow(
            clippy::cast_precision_loss, // n is a sample count, far below 2^53
            clippy::cast_possible_truncation, // the rank index is clamped to n
            clippy::cast_sign_loss, // the rank index is clamped >= 1.0 first
        )]
        let rank = |pct: f64| -> u64 {
            let index = (pct / 100.0) * n as f64;
            let index = index.ceil();
            let index = index.max(1.0) as usize - 1;
            samples[index.min(n - 1)]
        };
        let total: u64 = samples.iter().sum();
        #[allow(clippy::cast_precision_loss)] // a mean over at most millions of ns readings
        let mean = total as f64 / n as f64;
        Some(Self {
            n,
            mean_ns: mean,
            min_ns: samples[0],
            p50_ns: rank(50.0),
            p99_ns: rank(99.0),
            p99_9_ns: rank(99.9),
            max_ns: samples[n - 1],
        })
    }
}

fn summary_map(entries: Vec<(&'static str, Vec<u64>)>) -> HashMap<String, Summary> {
    entries
        .into_iter()
        .filter_map(|(name, values)| Summary::of(values).map(|s| (name.to_owned(), s)))
        .collect()
}

/// Every counter the run can fail with. The threshold's accounting reads
/// these: zero dropped spawn/despawn/input, frame drops ≤ 0.1 %.
#[derive(Debug, Default, Serialize)]
pub struct DropsReport {
    /// Inputs the observer's reliable lane refused (capacity). Must be 0.
    pub input_dropped: u64,
    /// Sequences missing on the forward path (gap sizes summed).
    pub forward_seq_gaps: u64,
    /// Reorders the sidecar saw on the forward path.
    pub forward_seq_reorders: u64,
    /// Frames the sidecar's latest-wins lane superseded before writing.
    pub frame_discarded_sidecar: u64,
    /// Frames that arrived but were superseded in the observer's apply slot
    /// before a tick picked them up.
    pub frame_overwritten_observer: u64,
    /// Sequences missing on the return path (gap sizes summed).
    pub return_seq_gaps: u64,
    /// Reorders the observer saw on the return path.
    pub return_seq_reorders: u64,
    /// Churn spawns sent but never received (or received out of order).
    pub spawn_missing: u64,
    /// Churn despawns sent but never received (or received out of order).
    pub despawn_missing: u64,
    /// Applied frames whose sidecar `(t1, t2)` timing never arrived, so the
    /// sample could not be joined.
    pub samples_missing_sidecar_timing: usize,
    /// Probes with no echo reply at end of run.
    pub probe_replies_missing: usize,
    /// Ticks whose work ran past the next tick deadline.
    pub tick_overruns: u64,
}

/// The observer's report: the artifact `scripts/ipc-report.py` reads.
#[derive(Debug, Serialize)]
pub struct HarnessReport {
    /// Report shape identifier.
    pub schema: String,
    /// Which role wrote the report.
    pub role: String,
    /// `std::env::consts::OS` — the label the decision rides on. The #920
    /// bands are defined at N = 24 **on Windows**;
    /// `scripts/ipc-report.py` refuses a verdict for any other platform.
    pub platform: String,
    /// Machine architecture.
    pub arch: String,
    /// The system-wide monotonic clock both processes read.
    pub clock: String,
    /// The transport under the framing.
    pub transport: String,
    /// `TCP_NODELAY` was set on both ends before any sample. #920 lie 2.
    pub tcp_nodelay: bool,
    /// Whether `timeBeginPeriod(1)` was raised. #920 lie 1 asks for both.
    pub time_begin_period: bool,
    /// Entities per frame batch.
    pub entities: u32,
    /// Fixed tick rate.
    pub tick_hz: u32,
    /// Ticks run before sampling began.
    pub warmup_ticks: u64,
    /// Ticks sampled (each one input, one expected frame back).
    pub ticks: u64,
    /// Joined samples the phase percentiles are taken over.
    pub samples: usize,
    /// Wall duration of the sampled window, seconds.
    pub duration_s: f64,
    /// System load at run start (`/proc/loadavg`; zeros where unavailable).
    pub loadavg_start: [f64; 3],
    /// System load at run end.
    pub loadavg_end: [f64; 3],
    /// The phase columns, in nanoseconds.
    pub phases_ns: HashMap<String, Summary>,
    /// The baselines, in nanoseconds.
    pub baselines_ns: HashMap<String, Summary>,
    /// The failure counters.
    pub drops: DropsReport,
    /// Standing caveats the artifact carries with it.
    pub notes: Vec<String>,
}

/// The sidecar's own report: supporting evidence for the observer's.
#[derive(Debug, Serialize)]
pub struct SidecarReport {
    /// Report shape identifier.
    pub schema: String,
    /// Which role wrote the report.
    pub role: String,
    /// Platform label.
    pub platform: String,
    /// The shared clock.
    pub clock: String,
    /// Entities per frame batch.
    pub entities: u32,
    /// Inputs processed.
    pub inputs_processed: u64,
    /// Frames handed to the writer lane.
    pub frames_pushed: u64,
    /// The sidecar's counters.
    pub drops: DropsReport,
    /// Local `(t2 − t1)` percentiles, for cross-checking the observer's
    /// `extract` column.
    pub extract_local_ns: Option<Summary>,
}

/// One applied frame, with every stamp the join needs.
struct Arrival {
    seq: u64,
    /// The sidecar's writer-thread stamp (`t3`), from the envelope.
    t3: u64,
    /// Last byte arrived on the observer's reader thread (`t4'`).
    t_arrive: u64,
    /// Codec decode of the frame batch finished.
    t_decode: u64,
    /// The decoded batch itself, for the apply step's real work.
    batch: FrameBatch,
}

/// Configuration for [`run_observer`].
pub struct ObserverConfig {
    /// Interface to bind, e.g. `127.0.0.1`.
    pub bind: String,
    /// Port to bind; 0 lets the OS choose an ephemeral port (the default,
    /// and why the harness needs no port lease).
    pub port: u16,
    /// Entities per frame batch. #920's headline number is N = 24.
    pub entities: u32,
    /// Fixed tick rate.
    pub tick_hz: u32,
    /// Ticks sampled.
    pub ticks: u64,
    /// Ticks run before sampling begins.
    pub warmup: u64,
    /// Raise `timeBeginPeriod(1)` (Windows; a no-op elsewhere).
    pub time_begin_period: bool,
}

/// Configuration for [`run_sidecar`].
pub struct SidecarConfig {
    /// Observer address to connect to.
    pub addr: String,
    /// Entities per frame batch; must match the observer.
    pub entities: u32,
    /// Fixed tick rate; must match the observer.
    pub tick_hz: u32,
    /// Raise `timeBeginPeriod(1)` (Windows; a no-op elsewhere).
    pub time_begin_period: bool,
}

/// Shared state between the observer's threads.
struct ObserverShared {
    waker: Arc<WriterWaker>,
    reliable: ReliableLane,
    probe_lane: LatestWins,
    frame_slot: Mutex<Option<Arrival>>,
    frame_overwritten: AtomicU64,
    send_log: Mutex<Vec<Option<u64>>>,
    probe_outstanding: Mutex<VecDeque<(u64, u64)>>,
    null_out: Mutex<Vec<u64>>,
    null_back: Mutex<Vec<u64>>,
    null_rtt: Mutex<Vec<u64>>,
    return_gaps: AtomicU64,
    return_reorders: AtomicU64,
    table: Mutex<HashMap<u64, (u64, u64)>>,
    sidecar_counters: Mutex<Option<[u64; 6]>>,
    spawn_events: Mutex<Vec<u64>>,
    despawn_events: Mutex<Vec<u64>>,
    stopping: AtomicBool,
}

/// Run the observer role. Blocks until the run completes; returns the report.
///
/// # Errors
///
/// Returns any I/O failure that aborts the run before the measurement
/// starts; a failure mid-run aborts the process instead, since a partial
/// measurement is worse than none.
///
/// # Panics
///
/// Panics if the tick budget does not fit the platform's `usize`, which no
/// real run approaches.
pub fn run_observer(config: &ObserverConfig) -> std::io::Result<HarnessReport> {
    let loadavg_start = read_loadavg();
    let _timer = if config.time_begin_period {
        Some(TimerResolution::raise(1))
    } else {
        None
    };
    let listener = TcpListener::bind((config.bind.as_str(), config.port))?;
    let local = listener.local_addr()?;
    println!(
        "orrery-ipc-bench: observer listening on {local} (entities {})",
        config.entities
    );
    let (stream, peer) = listener.accept()?;
    set_nodelay(&stream)?;
    println!("orrery-ipc-bench: sidecar connected from {peer}");

    let period_ns: u64 = 1_000_000_000 / u64::from(config.tick_hz);
    let ticks_total = config.warmup + config.ticks;
    let (start_ns, handshake_writer) = observer_handshake(&stream, config, ticks_total)?;
    drop(handshake_writer);

    let waker = Arc::new(WriterWaker::new());
    let shared = Arc::new(ObserverShared {
        waker: Arc::clone(&waker),
        reliable: ReliableLane::new(Arc::clone(&waker)),
        probe_lane: LatestWins::new(Arc::clone(&waker)),
        frame_slot: Mutex::new(None),
        frame_overwritten: AtomicU64::new(0),
        send_log: Mutex::new(vec![
            None;
            usize::try_from(ticks_total).expect("ticks fit usize")
        ]),
        probe_outstanding: Mutex::new(VecDeque::new()),
        null_out: Mutex::new(Vec::new()),
        null_back: Mutex::new(Vec::new()),
        null_rtt: Mutex::new(Vec::new()),
        return_gaps: AtomicU64::new(0),
        return_reorders: AtomicU64::new(0),
        table: Mutex::new(HashMap::new()),
        sidecar_counters: Mutex::new(None),
        spawn_events: Mutex::new(Vec::new()),
        despawn_events: Mutex::new(Vec::new()),
        stopping: AtomicBool::new(false),
    });

    let writer_stream = stream.try_clone()?;
    let writer_shared = Arc::clone(&shared);
    let writer_thread = std::thread::Builder::new()
        .name("ipc-observer-writer".into())
        .spawn(move || observer_writer(writer_stream, writer_shared))?;

    let reader_stream = stream.try_clone()?;
    let reader_shared = Arc::clone(&shared);
    let reader_thread = std::thread::Builder::new()
        .name("ipc-observer-reader".into())
        .spawn(move || observer_reader(reader_stream, reader_shared))?;

    // ── The game thread (this thread) ─────────────────────────────────────
    let (records, inproc, overruns) =
        observer_game(&shared, config, start_ns, period_ns, ticks_total);

    // End of run: tell the sidecar, let it send its table, then stop.
    shared.reliable.push(OutMsg::Done);
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    while !shared.stopping.load(Ordering::SeqCst) && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(2));
    }
    shared.reliable.push_stop();

    let _ = writer_thread.join();
    let _ = reader_thread.join();

    // ── Join and report ───────────────────────────────────────────────────
    let (joined, missing_timing) = observer_join(&shared, records);
    let columns = phase_columns(&joined);
    let report = observer_report(
        &shared,
        config,
        &columns,
        joined.len(),
        inproc,
        overruns,
        start_ns,
        period_ns,
        loadavg_start,
        missing_timing,
    );
    Ok(report)
}

/// Build the observer's report from the joined samples and every counter the
/// threads accumulated.
///
/// # Panics
///
/// Panics on a poisoned shared-state lock, which can only mean a thread died
/// mid-measurement.
fn observer_report(
    shared: &Arc<ObserverShared>,
    config: &ObserverConfig,
    columns: &PhaseColumns,
    samples: usize,
    inproc: Vec<u64>,
    overruns: u64,
    start_ns: u64,
    period_ns: u64,
    loadavg_start: [f64; 3],
    missing_timing: usize,
) -> HarnessReport {
    let counters = shared
        .sidecar_counters
        .lock()
        .expect("sidecar counters")
        .unwrap_or([0; 6]);
    // Churn accounting: every sent spawn/despawn must be received, in id
    // order. A count shortfall or an ordering regression is a delivery
    // failure; the threshold reads both as missing.
    let churn_missing = |sent: u64, events: &Mutex<Vec<u64>>| -> u64 {
        let (received, regressions) = {
            let events = events.lock().expect("churn events");
            let stats = (
                events.len(),
                events.windows(2).filter(|pair| pair[1] <= pair[0]).count(),
            );
            drop(events);
            (stats.0, stats.1)
        };
        #[allow(clippy::cast_sign_loss)] // a Vec length and a count
        {
            sent.saturating_sub(received as u64) + regressions as u64
        }
    };
    let spawn_missing = churn_missing(counters[4], &shared.spawn_events);
    let despawn_missing = churn_missing(counters[5], &shared.despawn_events);

    let run_end = monotonic_now_ns();
    let sampled_start = start_ns + config.warmup * period_ns;
    #[allow(clippy::cast_precision_loss)] // a wall duration in seconds
    let duration_s = (run_end.saturating_sub(sampled_start)) as f64 / 1_000_000_000.0;
    let null_out = shared.null_out.lock().expect("null out").clone();
    let null_back = shared.null_back.lock().expect("null back").clone();
    let null_rtt = shared.null_rtt.lock().expect("null rtt").clone();
    let probe_missing = shared.probe_outstanding.lock().expect("probe log").len();

    let notes = vec![
        "phase is the wait for the next observer tick, reported separately and excluded from ipc_added: it exists in embedded too (issue #920, lie 6)".to_string(),
        "ipc_added = hop_in + extract + encode + hop_out + decode_out = t_decode - t0".to_string(),
        "the #920 stand/overturn bands are defined at N = 24 on WINDOWS; a report from any other platform is informational only".to_string(),
        "warmup ticks are excluded from every sample (allocator and branch warmth, issue #920 lie 9)".to_string(),
        "I/O is blocking std on dedicated threads; no async runtime is measured (issue #920 lie 8)".to_string(),
        "the sidecar is event-driven, so no scheduling wait is hidden inside its columns; the extraction contract is shape-faithful, not a Bevy App".to_string(),
    ];

    HarnessReport {
        schema: "orrery-ipc-harness/1".into(),
        role: "observer".into(),
        platform: std::env::consts::OS.into(),
        arch: std::env::consts::ARCH.into(),
        clock: clock_name().into(),
        transport: "tcp-loopback".into(),
        tcp_nodelay: true,
        time_begin_period: config.time_begin_period,
        entities: config.entities,
        tick_hz: config.tick_hz,
        warmup_ticks: config.warmup,
        ticks: config.ticks,
        samples,
        duration_s,
        loadavg_start,
        loadavg_end: read_loadavg(),
        phases_ns: summary_map(vec![
            ("hop_in", columns.hop_in.clone()),
            ("extract", columns.extract.clone()),
            ("encode", columns.encode.clone()),
            ("hop_out", columns.hop_out.clone()),
            ("decode_out", columns.decode_out.clone()),
            ("phase", columns.phase.clone()),
            ("phase_after_decode", columns.phase_after_decode.clone()),
            ("ipc_added", columns.ipc_added.clone()),
        ]),
        baselines_ns: summary_map(vec![
            ("extract_inproc", inproc),
            ("hop_null_out", null_out),
            ("hop_null_back", null_back),
            ("hop_null_rtt", null_rtt),
        ]),
        drops: DropsReport {
            input_dropped: shared.reliable.dropped(),
            forward_seq_gaps: counters[0],
            forward_seq_reorders: counters[1],
            frame_discarded_sidecar: counters[2],
            frame_overwritten_observer: shared.frame_overwritten.load(Ordering::Relaxed),
            return_seq_gaps: shared.return_gaps.load(Ordering::Relaxed),
            return_seq_reorders: shared.return_reorders.load(Ordering::Relaxed),
            spawn_missing,
            despawn_missing,
            samples_missing_sidecar_timing: missing_timing,
            probe_replies_missing: probe_missing,
            tick_overruns: overruns,
        },
        notes,
    }
}

/// The pre-measurement handshake, on the main thread, so the stream is clean
/// of control frames when the measurement starts. The sidecar says its shape;
/// the observer sets the shared start instant.
///
/// # Panics
///
/// Panics when the sidecar's `--entities`/`--hz` disagree with the observer's:
/// a mismatched pair would measure the wrong thing silently.
fn observer_handshake(
    stream: &TcpStream,
    config: &ObserverConfig,
    ticks_total: u64,
) -> std::io::Result<(u64, TcpStream)> {
    let mut handshake_writer = stream.try_clone()?;
    let mut hello_reader = FrameReader::new(stream.try_clone()?);
    let hello = hello_reader
        .read_frame()?
        .ok_or_else(|| std::io::Error::other("sidecar closed before hello"))?;
    drop(hello_reader);
    let (_, _, hello_payload) =
        decode_envelope(&hello).map_err(|e| std::io::Error::other(e.to_string()))?;
    assert_eq!(
        hello_payload.first(),
        Some(&tag::HELLO),
        "expected a hello frame from the sidecar"
    );
    let mut hello_body = Body::new(hello_payload);
    let sidecar_entities = hello_body.u32();
    let sidecar_hz = hello_body.u32();
    assert_eq!(
        sidecar_entities, config.entities,
        "sidecar was started with --entities {sidecar_entities}, observer with {}",
        config.entities
    );
    assert_eq!(
        sidecar_hz, config.tick_hz,
        "sidecar was started with --hz {sidecar_hz}, observer with {}",
        config.tick_hz
    );

    let start_ns = monotonic_now_ns() + 250_000_000;
    let mut ready = vec![tag::READY];
    put_u64(&mut ready, start_ns);
    put_u64(&mut ready, ticks_total);
    {
        let mut writer = FrameWriter::new(&mut handshake_writer);
        writer.write_frame(&encode_envelope(CONTROL_SEQ, monotonic_now_ns(), &ready))?;
        writer.flush()?;
    }
    Ok((start_ns, handshake_writer))
}

/// What one applied frame contributes to a phase column.
struct Record {
    seq: u64,
    t0: u64,
    t1: u64,
    t2: u64,
    t3: u64,
    t_arrive: u64,
    t_decode: u64,
    t_apply: u64,
}

/// The game thread's loop: apply, extract in-process, emit input and probe,
/// once per tick, then one final boundary so the last frame is applied like
/// every other sample. Returns the raw records, the `extract_inproc`
/// baseline, and the tick-overrun count.
fn observer_game(
    shared: &Arc<ObserverShared>,
    config: &ObserverConfig,
    start_ns: u64,
    period_ns: u64,
    ticks_total: u64,
) -> (Vec<Record>, Vec<u64>, u64) {
    let mut world = SimWorld::new(config.entities);
    let mut records: Vec<Record> = Vec::new();
    let mut inproc: Vec<u64> = Vec::new();
    let mut checksum: u64 = 0;
    let mut overruns: u64 = 0;

    let apply = |slot: &Mutex<Option<Arrival>>, records: &mut Vec<Record>, checksum: &mut u64| {
        let pending = slot.lock().expect("frame slot").take();
        if let Some(arrival) = pending {
            let t_apply = monotonic_now_ns();
            consume(&arrival.batch, checksum);
            if arrival.seq >= config.warmup {
                records.push(Record {
                    seq: arrival.seq,
                    t0: 0,
                    t1: 0,
                    t2: 0,
                    t3: arrival.t3,
                    t_arrive: arrival.t_arrive,
                    t_decode: arrival.t_decode,
                    t_apply,
                });
            }
        }
    };

    for tick in 0..ticks_total {
        sleep_until_ns(start_ns + tick * period_ns);
        let sampling = tick >= config.warmup;

        // 1. Apply whatever arrived since last tick, at the tick boundary.
        if sampling {
            apply(&shared.frame_slot, &mut records, &mut checksum);
        } else {
            let arrival = shared.frame_slot.lock().expect("frame slot").take();
            if let Some(arrival) = arrival {
                consume(&arrival.batch, &mut checksum);
            }
        }

        // 2. The in-process baseline: same extraction and step, no encode,
        //    consumed in-process. Also the tick's real per-frame work, so
        //    this is not a ping-pong endpoint.
        let t_start = monotonic_now_ns();
        world.step(tick);
        let frames = world.extract(Tick::new(tick));
        consume(&frames, &mut checksum);
        if sampling {
            inproc.push(monotonic_now_ns() - t_start);
        }

        // 3. Emit this tick's input through the reliable lane.
        let body = orrery_ipc::EngineToSidecar::Input(input_batch(tick))
            .encode()
            .expect("input batch always encodes");
        shared.reliable.push(OutMsg::Input { seq: tick, body });

        // 4. Emit the null probe through its own expendable lane. Control
        //    sequence numbers never touch the data watchers.
        shared.probe_lane.push(OutMsg::Probe {
            seq: CONTROL_SEQ | tick,
        });

        if monotonic_now_ns() > start_ns + (tick + 1) * period_ns {
            overruns += 1;
        }
    }

    // One more tick boundary so the last frame can arrive and be applied at
    // a real boundary, exactly like every other sample.
    sleep_until_ns(start_ns + ticks_total * period_ns);
    apply(&shared.frame_slot, &mut records, &mut checksum);

    (records, inproc, overruns)
}

/// Join the observer's applied records with the sidecar's end-of-run timing
/// table and the send log, producing the fully-timestamped samples.
fn observer_join(shared: &Arc<ObserverShared>, records: Vec<Record>) -> (Vec<Record>, usize) {
    let mut missing_timing = 0;
    let mut joined: Vec<Record> = Vec::with_capacity(records.len());
    for mut record in records {
        let index = usize::try_from(record.seq).expect("sequence numbers fit usize");
        let t0 = {
            let send_log = shared.send_log.lock().expect("send log");
            send_log.get(index).and_then(|slot| *slot)
        };
        let Some(t0) = t0 else {
            missing_timing += 1;
            continue;
        };
        record.t0 = t0;
        let timing = {
            let table = shared.table.lock().expect("timing table");
            table.get(&record.seq).copied()
        };
        match timing {
            Some((t1, t2)) => {
                record.t1 = t1;
                record.t2 = t2;
                joined.push(record);
            }
            None => missing_timing += 1,
        }
    }
    (joined, missing_timing)
}

/// The eight phase columns, computed once per joined sample.
struct PhaseColumns {
    hop_in: Vec<u64>,
    extract: Vec<u64>,
    encode: Vec<u64>,
    hop_out: Vec<u64>,
    decode_out: Vec<u64>,
    phase: Vec<u64>,
    phase_after_decode: Vec<u64>,
    ipc_added: Vec<u64>,
}

fn phase_columns(joined: &[Record]) -> PhaseColumns {
    let n = joined.len();
    let mut columns = PhaseColumns {
        hop_in: Vec::with_capacity(n),
        extract: Vec::with_capacity(n),
        encode: Vec::with_capacity(n),
        hop_out: Vec::with_capacity(n),
        decode_out: Vec::with_capacity(n),
        phase: Vec::with_capacity(n),
        phase_after_decode: Vec::with_capacity(n),
        ipc_added: Vec::with_capacity(n),
    };
    for r in joined {
        columns.hop_in.push(r.t1 - r.t0);
        columns.extract.push(r.t2 - r.t1);
        columns.encode.push(r.t3 - r.t2);
        columns.hop_out.push(r.t_arrive - r.t3);
        columns.decode_out.push(r.t_decode - r.t_arrive);
        columns.phase.push(r.t_apply - r.t_arrive);
        columns.phase_after_decode.push(r.t_apply - r.t_decode);
        columns.ipc_added.push(r.t_decode - r.t0);
    }
    columns
}

/// The observer's writer thread: reliable traffic first, then the probe lane.
fn observer_writer(mut stream: TcpStream, shared: Arc<ObserverShared>) {
    let mut writer = FrameWriter::new(&mut stream);
    loop {
        let message = shared.reliable.pop().or_else(|| shared.probe_lane.take());
        let Some(message) = message else {
            // Both lanes empty; the waker's dirty flag covers anything that
            // lands between the pops above and this wait.
            shared.waker.wait(Duration::from_millis(50));
            continue;
        };
        if matches!(message, OutMsg::Stop) {
            let _ = writer.flush();
            break;
        }
        if observer_send(&mut writer, &shared, message).is_err() {
            // The reader thread will hit EOF and end the run; nothing honest
            // can be measured past a broken transport.
            std::process::abort();
        }
    }
}

/// Stamp, envelope, and write one outgoing frame; record the stamps the
/// report needs.
fn observer_send(
    writer: &mut FrameWriter<&mut TcpStream>,
    shared: &ObserverShared,
    message: OutMsg,
) -> std::io::Result<()> {
    let t_send = monotonic_now_ns();
    match message {
        OutMsg::Input { seq, body } => {
            let index = usize::try_from(seq).expect("sequence numbers fit usize");
            if let Some(slot) = shared.send_log.lock().expect("send log").get_mut(index) {
                *slot = Some(t_send);
            }
            writer.write_frame(&encode_envelope(seq, t_send, &body))
        }
        OutMsg::Probe { seq } => {
            let payload = vec![tag::PROBE, 0x4F, 0x52, 0x49, 0x50, 0xAA, 0xBB, 0xCC, 0xDD];
            shared
                .probe_outstanding
                .lock()
                .expect("probe log")
                .push_back((seq, t_send));
            writer.write_frame(&encode_envelope(seq, t_send, &payload))
        }
        OutMsg::Done => writer.write_frame(&encode_envelope(CONTROL_SEQ, t_send, &[tag::DONE])),
        OutMsg::Frame { .. }
        | OutMsg::EchoReply { .. }
        | OutMsg::Spawn { .. }
        | OutMsg::Despawn { .. }
        | OutMsg::Table { .. }
        | OutMsg::Counters { .. }
        | OutMsg::Hello { .. }
        | OutMsg::Stop => unreachable!("the observer's writer sends only inputs, probes, done"),
    }
}

/// The observer's reader thread: every stamp is taken immediately after the
/// read returns, before any decode. Returns (and stops the run) on any
/// protocol violation, which a healthy sidecar never produces.
fn observer_reader(mut stream: TcpStream, shared: Arc<ObserverShared>) {
    let mut reader = FrameReader::new(&mut stream);
    let mut watcher = SeqWatcher::new();
    let mut checksum: u64 = 0;
    while let Some(frame) = reader.read_frame().ok().flatten() {
        let t_arrive = monotonic_now_ns();
        let Ok((seq, t_send, payload)) = decode_envelope(&frame) else {
            break;
        };
        match payload.first().copied() {
            Some(b'O') => {
                if !observer_on_codec_message(
                    &shared,
                    &mut watcher,
                    seq,
                    t_send,
                    t_arrive,
                    payload,
                    &mut checksum,
                ) {
                    break;
                }
            }
            Some(tag::ECHO) => observer_on_echo(&shared, payload, t_arrive, t_send),
            Some(tag::TABLE) => observer_on_table(&shared, payload),
            Some(tag::COUNTERS) => observer_on_counters(&shared, payload),
            _ => break,
        }
    }
    shared.stopping.store(true, Ordering::SeqCst);
}

/// Handle one codec message on the return path: frames (watched, decoded,
/// offered to the game thread), spawn and despawn batches (counted). Returns
/// `false` on a message this direction can never carry.
fn observer_on_codec_message(
    shared: &Arc<ObserverShared>,
    watcher: &mut SeqWatcher,
    seq: u64,
    t_send: u64,
    t_arrive: u64,
    payload: &[u8],
    checksum: &mut u64,
) -> bool {
    match SidecarToEngine::decode(payload) {
        Ok(SidecarToEngine::Frames(batch)) => {
            let t_decode = monotonic_now_ns();
            match watcher.observe(seq) {
                SeqEvent::Gap { missing } => {
                    shared.return_gaps.fetch_add(missing, Ordering::Relaxed);
                }
                SeqEvent::Reorder { .. } => {
                    shared.return_reorders.fetch_add(1, Ordering::Relaxed);
                }
                SeqEvent::InOrder | SeqEvent::Duplicate => {}
            }
            consume(&batch, checksum);
            let mut slot = shared.frame_slot.lock().expect("frame slot");
            if slot.is_some() {
                shared.frame_overwritten.fetch_add(1, Ordering::Relaxed);
            }
            *slot = Some(Arrival {
                seq,
                t3: t_send,
                t_arrive,
                t_decode,
                batch,
            });
            drop(slot);
            true
        }
        Ok(SidecarToEngine::Spawns(batch)) => {
            shared
                .spawn_events
                .lock()
                .expect("spawn events")
                .extend(batch.entities.iter().map(|id| id.0));
            true
        }
        Ok(SidecarToEngine::Despawns(batch)) => {
            shared
                .despawn_events
                .lock()
                .expect("despawn events")
                .extend(batch.entities.iter().map(|id| id.0));
            true
        }
        Ok(SidecarToEngine::Corrections(_)) | Err(_) => false,
    }
}

/// Handle one echo reply: both one-way legs and the round trip, from the
/// shared clock's stamps.
fn observer_on_echo(shared: &Arc<ObserverShared>, payload: &[u8], t_arrive: u64, t_send: u64) {
    let mut body = Body::new(payload);
    let probe_seq = body.u64();
    let t_arrive_sidecar = body.u64();
    let orig_t_send = body.u64();
    shared
        .null_out
        .lock()
        .expect("null out")
        .push(t_arrive_sidecar.saturating_sub(orig_t_send));
    shared
        .null_back
        .lock()
        .expect("null back")
        .push(t_arrive.saturating_sub(t_send));
    shared
        .null_rtt
        .lock()
        .expect("null rtt")
        .push(t_arrive.saturating_sub(orig_t_send));
    let mut outstanding = shared.probe_outstanding.lock().expect("probe log");
    if outstanding.front().map(|(s, _)| *s) == Some(probe_seq) {
        outstanding.pop_front();
    }
}

/// Ingest one chunk of the sidecar's end-of-run `(t1, t2)` table.
fn observer_on_table(shared: &Arc<ObserverShared>, payload: &[u8]) {
    let mut body = Body::new(payload);
    let count = body.u32();
    let mut table = shared.table.lock().expect("timing table");
    for _ in 0..count {
        let seq = body.u64();
        let t1 = body.u64();
        let t2 = body.u64();
        table.insert(seq, (t1, t2));
    }
}

/// Ingest the sidecar's end-of-run counters.
fn observer_on_counters(shared: &Arc<ObserverShared>, payload: &[u8]) {
    let mut body = Body::new(payload);
    let mut counters = [0u64; 6];
    for slot in &mut counters {
        *slot = body.u64();
    }
    *shared.sidecar_counters.lock().expect("sidecar counters") = Some(counters);
}

/// Shared state between the sidecar's threads.
struct SidecarShared {
    waker: Arc<WriterWaker>,
    reliable: ReliableLane,
    frames: LatestWins,
}

/// Run the sidecar role. Blocks until the observer ends the run.
///
/// # Errors
///
/// Returns any I/O failure that aborts the run before the measurement
/// starts; a failure mid-run aborts the process instead.
///
/// # Panics
///
/// Panics on a poisoned shared-state lock, which can only mean a thread died
/// mid-measurement.
pub fn run_sidecar(config: &SidecarConfig) -> std::io::Result<SidecarReport> {
    let _timer = if config.time_begin_period {
        Some(TimerResolution::raise(1))
    } else {
        None
    };
    let stream = TcpStream::connect(config.addr.as_str())?;
    set_nodelay(&stream)?;
    println!("orrery-ipc-bench: sidecar connected to {}", config.addr);

    let waker = Arc::new(WriterWaker::new());
    let shared = Arc::new(SidecarShared {
        waker: Arc::clone(&waker),
        reliable: ReliableLane::new(Arc::clone(&waker)),
        frames: LatestWins::new(Arc::clone(&waker)),
    });

    let writer_stream = stream.try_clone()?;
    let writer_shared = Arc::clone(&shared);
    let writer_thread = std::thread::Builder::new()
        .name("ipc-sidecar-writer".into())
        .spawn(move || sidecar_writer(writer_stream, writer_shared))?;

    let mut hello = vec![tag::HELLO];
    put_u32(&mut hello, config.entities);
    put_u32(&mut hello, config.tick_hz);
    shared.reliable.push(OutMsg::Hello { body: hello });

    // ── The reader thread (this thread): read, decode, step, extract ──────
    let mut reader = FrameReader::new(stream.try_clone()?);
    let mut watcher = SeqWatcher::new();
    let mut world = SimWorld::new(config.entities);
    let mut table: Vec<(u64, u64, u64)> = Vec::new();
    let mut extract_local: Vec<u64> = Vec::new();
    let mut checksum: u64 = 0;
    let mut inputs_processed: u64 = 0;
    let mut frames_pushed: u64 = 0;
    let mut spawns_sent: u64 = 0;
    let mut despawns_sent: u64 = 0;
    let mut forward_gaps: u64 = 0;
    let mut forward_reorders: u64 = 0;

    while let Some(frame) = reader.read_frame().ok().flatten() {
        let Ok((seq, t_send, payload)) = decode_envelope(&frame) else {
            break;
        };
        match payload.first().copied() {
            Some(b'O') => {
                let Ok(batch) = orrery_ipc::EngineToSidecar::decode(payload) else {
                    break;
                };
                match watcher.observe(seq) {
                    SeqEvent::Gap { missing } => forward_gaps += missing,
                    SeqEvent::Reorder { .. } => forward_reorders += 1,
                    SeqEvent::InOrder | SeqEvent::Duplicate => {}
                }
                // `EngineToSidecar` has exactly one kind, so this pattern is
                // irrefutable — the schema is push-only.
                let orrery_ipc::EngineToSidecar::Input(input) = batch;
                sidecar_process_input(
                    &shared,
                    &mut world,
                    seq,
                    &input,
                    &mut checksum,
                    &mut extract_local,
                    &mut table,
                    &mut frames_pushed,
                    &mut inputs_processed,
                    &mut spawns_sent,
                    &mut despawns_sent,
                );
            }
            Some(tag::PROBE) => {
                // t_pa: arrival on the sidecar, from the shared clock.
                let t_arrive = monotonic_now_ns();
                shared.reliable.push(OutMsg::EchoReply {
                    probe_seq: seq,
                    t_arrive,
                    orig_t_send: t_send,
                    payload: payload[1..].to_vec(),
                });
            }
            Some(tag::READY) => {
                // The observer's start instant and tick budget; the sidecar
                // is event-driven, so the values only bound the handshake.
            }
            Some(tag::DONE) => {
                sidecar_flush_table_and_counters(
                    &shared,
                    &table,
                    forward_gaps,
                    forward_reorders,
                    spawns_sent,
                    despawns_sent,
                );
                break;
            }
            _ => break,
        }
    }

    shared.reliable.push_stop();
    let _ = writer_thread.join();

    Ok(sidecar_report(
        config,
        &shared,
        inputs_processed,
        frames_pushed,
        forward_gaps,
        forward_reorders,
        Summary::of(extract_local),
    ))
}

/// Assemble the sidecar's report from its counters.
fn sidecar_report(
    config: &SidecarConfig,
    shared: &Arc<SidecarShared>,
    inputs_processed: u64,
    frames_pushed: u64,
    forward_gaps: u64,
    forward_reorders: u64,
    extract_local_ns: Option<Summary>,
) -> SidecarReport {
    SidecarReport {
        schema: "orrery-ipc-harness/1".into(),
        role: "sidecar".into(),
        platform: std::env::consts::OS.into(),
        clock: clock_name().into(),
        entities: config.entities,
        inputs_processed,
        frames_pushed,
        drops: DropsReport {
            input_dropped: shared.reliable.dropped(),
            forward_seq_gaps: forward_gaps,
            forward_seq_reorders: forward_reorders,
            frame_discarded_sidecar: shared.frames.discarded(),
            ..DropsReport::default()
        },
        extract_local_ns,
    }
}

/// The sidecar's per-input work, exactly the pipeline the report's columns
/// name: decode already happened (`t1` on arrival thread), then step and
/// extract (`t2`), then encode and hand over (the writer stamps `t3`).
#[allow(clippy::too_many_arguments)] // the workspace allows this; the state is per-input
fn sidecar_process_input(
    shared: &Arc<SidecarShared>,
    world: &mut SimWorld,
    seq: u64,
    input: &orrery_ipc::InputBatch,
    checksum: &mut u64,
    extract_local: &mut Vec<u64>,
    table: &mut Vec<(u64, u64, u64)>,
    frames_pushed: &mut u64,
    inputs_processed: &mut u64,
    spawns_sent: &mut u64,
    despawns_sent: &mut u64,
) {
    // t1: decoded. The decode happened on this thread, the moment the frame
    // arrived — no queue wait is hidden here.
    let t1 = monotonic_now_ns();
    // t2: extraction and step done.
    world.step(input.tick.0);
    let frames = world.extract(input.tick);
    consume(&frames, checksum);
    let t2 = monotonic_now_ns();
    extract_local.push(t2 - t1);
    // Encode and hand over; the writer thread stamps t3 when the frame
    // reaches the transport, symmetric with t0.
    let body = SidecarToEngine::Frames(frames)
        .encode()
        .expect("frame batch always encodes");
    shared.frames.push(OutMsg::Frame { seq, body });
    *frames_pushed += 1;
    table.push((seq, t1, t2));
    *inputs_processed += 1;

    // Churn: a spawn every 600th tick, its despawn 300 later. Ids are
    // strictly increasing so the observer's ordering check means what it
    // says: a reused or regressing id can only be real misdelivery.
    if seq > 0 && seq.is_multiple_of(tag::CHURN_PERIOD) {
        let id = seq / tag::CHURN_PERIOD;
        let body = SidecarToEngine::Spawns(orrery_ipc::SpawnBatch {
            entities: vec![PersistId::new(id)],
        })
        .encode()
        .expect("spawn batch always encodes");
        shared.reliable.push(OutMsg::Spawn { body });
        *spawns_sent += 1;
    }
    if seq % tag::CHURN_PERIOD == tag::CHURN_DESPAWN_OFFSET {
        let id = seq / tag::CHURN_PERIOD;
        let body = SidecarToEngine::Despawns(orrery_ipc::DespawnBatch {
            entities: vec![PersistId::new(id)],
        })
        .encode()
        .expect("despawn batch always encodes");
        shared.reliable.push(OutMsg::Despawn { body });
        *despawns_sent += 1;
    }
}

/// Send the end-of-run timing table in cap-safe chunks, then the counters.
fn sidecar_flush_table_and_counters(
    shared: &Arc<SidecarShared>,
    table: &[(u64, u64, u64)],
    forward_gaps: u64,
    forward_reorders: u64,
    spawns_sent: u64,
    despawns_sent: u64,
) {
    for chunk in table.chunks(tag::TABLE_CHUNK) {
        let mut body = vec![tag::TABLE];
        put_u32(
            &mut body,
            u32::try_from(chunk.len()).expect("chunk fits u32"),
        );
        for (seq, t1, t2) in chunk {
            put_u64(&mut body, *seq);
            put_u64(&mut body, *t1);
            put_u64(&mut body, *t2);
        }
        shared.reliable.push(OutMsg::Table { body });
    }
    let mut body = vec![tag::COUNTERS];
    put_u64(&mut body, forward_gaps);
    put_u64(&mut body, forward_reorders);
    put_u64(&mut body, shared.frames.discarded());
    put_u64(&mut body, shared.reliable.dropped());
    put_u64(&mut body, spawns_sent);
    put_u64(&mut body, despawns_sent);
    shared.reliable.push(OutMsg::Counters { body });
}

/// The sidecar's writer thread: reliable traffic first, then the frame lane.
fn sidecar_writer(mut stream: TcpStream, shared: Arc<SidecarShared>) {
    let mut writer = FrameWriter::new(&mut stream);
    loop {
        let message = shared.reliable.pop().or_else(|| shared.frames.take());
        let Some(message) = message else {
            shared.waker.wait(Duration::from_millis(50));
            continue;
        };
        if matches!(message, OutMsg::Stop) {
            let _ = writer.flush();
            break;
        }
        if sidecar_send(&mut writer, message).is_err() {
            std::process::abort();
        }
    }
}

/// Stamp and write one sidecar frame; the stamp is `t3` — the moment the
/// frame reaches the transport.
fn sidecar_send(writer: &mut FrameWriter<&mut TcpStream>, message: OutMsg) -> std::io::Result<()> {
    let t_send = monotonic_now_ns();
    match message {
        OutMsg::Frame { seq, body } => writer.write_frame(&encode_envelope(seq, t_send, &body)),
        OutMsg::EchoReply {
            probe_seq,
            t_arrive,
            orig_t_send,
            payload,
        } => {
            let mut body = vec![tag::ECHO];
            put_u64(&mut body, probe_seq);
            put_u64(&mut body, t_arrive);
            put_u64(&mut body, orig_t_send);
            body.extend_from_slice(&payload);
            writer.write_frame(&encode_envelope(probe_seq, t_send, &body))
        }
        OutMsg::Spawn { body }
        | OutMsg::Despawn { body }
        | OutMsg::Table { body }
        | OutMsg::Counters { body }
        | OutMsg::Hello { body } => {
            writer.write_frame(&encode_envelope(CONTROL_SEQ, t_send, &body))
        }
        OutMsg::Input { .. } | OutMsg::Probe { .. } | OutMsg::Done | OutMsg::Stop => {
            unreachable!("the sidecar's writer sends only its own traffic")
        }
    }
}
