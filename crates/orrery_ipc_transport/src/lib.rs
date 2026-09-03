//! The transport #920 specifies, beside the [`orrery_ipc`] codec, not inside
//! it.
//!
//! `orrery_ipc` is deliberately a codec: it defines messages and their bytes,
//! carries no outer length prefix (`decode` refuses trailing bytes), and
//! `MAGIC = b"ORIP"` can occur inside an opaque input payload. On a byte
//! stream without a prefix, one misaligned read is unrecoverable — the decoder
//! cannot resync, because a spawn that never decodes is an entity that never
//! appears and the schema is push-only. So the byte-stream transport adds the
//! prefix here: every frame is `[u32 len][body]`, `len` little-endian, capped
//! at [`MAX_FRAME_LEN`].
//!
//! The framing is generic over [`std::io::Read`] and [`std::io::Write`] so the
//! transport underneath is a one-line swap: TCP loopback today (the issue's
//! worst reasonable candidate on Windows), a named pipe or UDS later, without
//! touching a second call site. On top of the framing sits the #920 harness
//! envelope — `[u32 len][u64 seq][u64 t_send_ns][orrery_ipc bytes]` — whose
//! `seq` drives drop and reorder detection ([`SeqWatcher`]) and whose
//! `t_send_ns` comes from one system-wide monotonic clock
//! ([`monotonic_now_ns`]) read by both processes. Never a wall clock, never a
//! per-process epoch: the percentiles the decision is taken on subtract two
//! readings of the same clock in two processes.
//!
//! Unlike `orrery_ipc`, this crate does not carry `#![forbid(unsafe_code)]`.
//! It holds exactly two `unsafe` blocks, both inside [`monotonic_now_ns`]'s
//! helpers: reading `CLOCK_MONOTONIC` and `QueryPerformanceCounter`. There is
//! no safe equivalent that yields a cross-process-comparable reading —
//! `std::time::Instant` exposes no raw value, and calibrating two per-process
//! epochs over a loopback echo leaves a ~min-RTT/2 offset error that is the
//! same order as the quantity being measured. The codec's forbid stands; this
//! crate is where the boundary touches the OS, and it says so rather than
//! hiding the two calls behind a re-export.

#![warn(missing_docs)]

pub mod bench;

use std::io::{self, Read, Write};

/// Largest frame body the framing accepts, 1 MiB: the cap #920 puts on the
/// length-prefixed stream.
///
/// This bounds the body that follows the prefix — the envelope header and the
/// codec bytes — not the payload alone. A sidecar frame batch at the D6
/// ceiling (1024 entities, 62 bytes each) is ~64 KB, well inside it; the
/// harness's largest control frame is a timing-table chunk sized to fit.
pub const MAX_FRAME_LEN: usize = 1 << 20;

/// Width of the length prefix: one little-endian `u32`.
pub const PREFIX_LEN: usize = 4;

/// Width of the harness envelope header: `u64 seq` then `u64 t_send_ns`.
pub const ENVELOPE_HEADER: usize = 16;

/// Reject a frame body the cap forbids, before a byte is written or read.
fn check_cap(len: usize) -> io::Result<()> {
    if len > MAX_FRAME_LEN {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("frame body of {len} bytes exceeds the {MAX_FRAME_LEN}-byte cap"),
        ));
    }
    Ok(())
}

/// Writes length-prefixed frames to any [`Write`].
///
/// The prefix is written from the same buffer as the body in one
/// [`Write::write_all`] call, so a frame never tears between prefix and body
/// from this end.
pub struct FrameWriter<W: Write> {
    inner: W,
    buf: Vec<u8>,
}

impl<W: Write> FrameWriter<W> {
    /// Wrap a byte sink.
    pub const fn new(inner: W) -> Self {
        Self {
            inner,
            buf: Vec::new(),
        }
    }

    /// Write one frame: `[u32 len][body]`. Fails outright when `body` exceeds
    /// [`MAX_FRAME_LEN`] — a byte stream has no way to take it back.
    ///
    /// # Errors
    ///
    /// Returns the underlying I/O error, or [`io::ErrorKind::InvalidData`]
    /// when the body exceeds the cap.
    pub fn write_frame(&mut self, body: &[u8]) -> io::Result<()> {
        let len = u32::try_from(body.len()).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("frame body of {} bytes does not fit u32", body.len()),
            )
        })?;
        check_cap(body.len())?;

        self.buf.clear();
        self.buf.reserve(PREFIX_LEN + body.len());
        self.buf.extend_from_slice(&len.to_le_bytes());
        self.buf.extend_from_slice(body);
        self.inner.write_all(&self.buf)
    }

    /// Flush the underlying sink.
    ///
    /// # Errors
    ///
    /// Returns the underlying I/O error.
    pub fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// Reads length-prefixed frames from any [`Read`].
pub struct FrameReader<R: Read> {
    inner: R,
}

impl<R: Read> FrameReader<R> {
    /// Wrap a byte source.
    pub const fn new(inner: R) -> Self {
        Self { inner }
    }

    /// Read one frame body, blocking until it is complete.
    ///
    /// Returns `Ok(None)` on a clean end of stream at a frame boundary.
    /// Anything else — a truncated prefix, a body cut short, a declared
    /// length over the cap — is an error, and a fatal one: the stream has no
    /// resync, so the caller drops the connection. That is the same verdict
    /// the issue reaches for a codec decode error, and for the same reason.
    ///
    /// # Errors
    ///
    /// Returns the underlying I/O error; [`io::ErrorKind::InvalidData`] when
    /// the declared length exceeds the cap; [`io::ErrorKind::UnexpectedEof`]
    /// when the stream ends mid-frame.
    pub fn read_frame(&mut self) -> io::Result<Option<Vec<u8>>> {
        let mut prefix = [0u8; PREFIX_LEN];
        let mut filled = 0;
        while filled < PREFIX_LEN {
            let read = self.inner.read(&mut prefix[filled..])?;
            if read == 0 {
                if filled == 0 {
                    return Ok(None);
                }
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "stream ended inside a length prefix",
                ));
            }
            filled += read;
        }
        let len = u32::from_le_bytes(prefix) as usize;
        check_cap(len)?;

        let mut body = vec![0u8; len];
        self.inner.read_exact(&mut body)?;
        Ok(Some(body))
    }
}

/// Failure to read a harness envelope off a frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnvelopeError {
    /// The frame is shorter than the 16-byte header.
    Truncated,
}

impl core::fmt::Display for EnvelopeError {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Truncated => write!(formatter, "envelope shorter than its 16-byte header"),
        }
    }
}

impl core::error::Error for EnvelopeError {}

/// Encode one harness envelope: `[u64 seq][u64 t_send_ns][payload]`, both
/// integers little-endian. The framing layer prefixes the length.
///
/// The payload is the `orrery_ipc` bytes — or, for harness control traffic, a
/// one-byte tag that is never `'O'`, so a control frame can never be mistaken
/// for a codec message.
#[must_use]
pub fn encode_envelope(seq: u64, t_send_ns: u64, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(ENVELOPE_HEADER + payload.len());
    out.extend_from_slice(&seq.to_le_bytes());
    out.extend_from_slice(&t_send_ns.to_le_bytes());
    out.extend_from_slice(payload);
    out
}

/// Split one envelope into `(seq, t_send_ns, payload)`.
///
/// # Errors
///
/// Returns [`EnvelopeError::Truncated`] when the frame is shorter than
/// the header.
pub fn decode_envelope(frame: &[u8]) -> Result<(u64, u64, &[u8]), EnvelopeError> {
    if frame.len() < ENVELOPE_HEADER {
        return Err(EnvelopeError::Truncated);
    }
    let (header, payload) = frame.split_at(ENVELOPE_HEADER);
    let seq = u64::from_le_bytes(
        header[0..8]
            .try_into()
            .map_err(|_| EnvelopeError::Truncated)?,
    );
    let t_send_ns = u64::from_le_bytes(
        header[8..16]
            .try_into()
            .map_err(|_| EnvelopeError::Truncated)?,
    );
    Ok((seq, t_send_ns, payload))
}

/// What a [`SeqWatcher`] saw for one observed sequence number.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SeqEvent {
    /// The expected next sequence.
    InOrder,
    /// One or more sequences before this one never arrived.
    Gap {
        /// How many sequences are missing.
        missing: u64,
    },
    /// An older sequence arrived after the watcher had moved past it.
    Reorder {
        /// The sequence the watcher expected.
        expected: u64,
        /// The sequence that actually arrived.
        received: u64,
    },
    /// The most recent sequence arrived again.
    Duplicate,
}

/// Detects dropped and reordered messages on a numbered stream.
///
/// `seq` exists in the envelope precisely so this is detectable: the schema
/// is push-only and `SpawnBatch`/`DespawnBatch` are deltas with no
/// full-snapshot resync, so a lost spawn is an entity that never appears. On
/// TCP the watcher observes nothing but `InOrder` — its value is that it
/// keeps observing nothing, and that the same detection survives the
/// transport swap to a datagram candidate without new code.
///
/// The watcher is deliberately stateless beyond `next` and `highest`: it
/// never buffers, so it can sit on the hot read path.
#[derive(Debug, Default)]
pub struct SeqWatcher {
    next: Option<u64>,
    highest: u64,
}

impl SeqWatcher {
    #[must_use]
    /// A watcher that has seen nothing yet.
    pub fn new() -> Self {
        Self::default()
    }

    /// Observe one sequence number.
    #[must_use]
    pub const fn observe(&mut self, seq: u64) -> SeqEvent {
        let Some(next) = self.next else {
            self.next = Some(seq.wrapping_add(1));
            self.highest = seq;
            return SeqEvent::InOrder;
        };
        if seq == next {
            self.next = seq.checked_add(1);
            self.highest = seq;
            SeqEvent::InOrder
        } else if seq > next {
            let missing = seq - next;
            self.next = seq.checked_add(1);
            self.highest = seq;
            SeqEvent::Gap { missing }
        } else if seq == self.highest {
            SeqEvent::Duplicate
        } else {
            SeqEvent::Reorder {
                expected: next,
                received: seq,
            }
        }
    }
}

/// Nanoseconds on the system-wide monotonic clock, same epoch in every
/// process on the machine.
///
/// This is the only clock measurement timestamps come from. `CLOCK_MONOTONIC`
/// on Unix and `QueryPerformanceCounter` on Windows are both system-wide and
/// both non-adjusted, so `t_send_ns` written by one process subtracts cleanly
/// against a reading taken by another. What is never used: the wall clock,
/// which NTP steps, and any per-process epoch, which does not survive the
/// process boundary the measurement crosses twice per sample.
///
/// # Panics
///
/// Panics if the OS clock read fails, which leaves nothing measurable to
/// fall back to.
#[must_use]
pub fn monotonic_now_ns() -> u64 {
    #[cfg(unix)]
    {
        let mut ts = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        // SAFETY: `ts` is a valid, exclusively borrowed `timespec` for the
        // duration of the call; `clock_gettime` writes only through it.
        let status =
            unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, core::ptr::from_mut(&mut ts)) };
        // A release build takes this branch too: a failed clock read must
        // stop the measurement, not silently zero every timestamp.
        assert_eq!(
            status, 0,
            "clock_gettime(CLOCK_MONOTONIC) failed; no measurement is possible"
        );
        ts.tv_sec.cast_unsigned() * 1_000_000_000 + ts.tv_nsec.cast_unsigned()
    }
    #[cfg(windows)]
    {
        static FREQUENCY: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
        let frequency = *FREQUENCY.get_or_init(|| {
            let mut value: i64 = 0;
            // SAFETY: `value` is a valid, exclusively borrowed `i64` for the
            // duration of the call.
            let status = unsafe {
                windows_sys::Win32::System::Performance::QueryPerformanceFrequency(
                    core::ptr::from_mut(&mut value),
                )
            };
            assert_ne!(
                status, 0,
                "QueryPerformanceFrequency failed; no measurement is possible"
            );
            value.cast_unsigned()
        });
        let mut ticks: i64 = 0;
        // SAFETY: `ticks` is a valid, exclusively borrowed `i64` for the
        // duration of the call.
        let status = unsafe {
            windows_sys::Win32::System::Performance::QueryPerformanceCounter(core::ptr::from_mut(
                &mut ticks,
            ))
        };
        assert_ne!(
            status, 0,
            "QueryPerformanceCounter failed; no measurement is possible"
        );
        // QPC ticks are not nanoseconds; scale once through `u128`, which
        // cannot overflow at any real boot time and frequency pair.
        #[allow(clippy::cast_sign_loss)] // QPC ticks are non-negative by contract
        {
            let ticks = ticks.cast_unsigned();
            ((u128::from(ticks) * 1_000_000_000) / u128::from(frequency)) as u64
        }
    }
}

/// The name of the clock [`monotonic_now_ns`] reads, for the report.
#[must_use]
pub const fn clock_name() -> &'static str {
    #[cfg(unix)]
    {
        "CLOCK_MONOTONIC"
    }
    #[cfg(windows)]
    {
        "QueryPerformanceCounter"
    }
}

/// Sleep until the monotonic clock reaches `deadline`.
///
/// The tick pacing primitive: sleep in ~1 ms quanta, then spin the last
/// ~1.5 ms, so a 60 Hz tick boundary is met to within microseconds without a
/// hot spin over the whole period. Pacing never produces a measurement
/// timestamp; it only decides when the game thread wakes.
pub fn sleep_until_ns(deadline: u64) {
    loop {
        let Some(remaining) = deadline.checked_sub(monotonic_now_ns()) else {
            return;
        };
        if remaining <= 1_500_000 {
            core::hint::spin_loop();
        } else {
            std::thread::sleep(core::time::Duration::from_nanos(remaining - 1_000_000));
        }
    }
}

// The two platform `unsafe` blocks above are the crate's entire FFI surface:
// one clock read each on Unix and Windows, both asserted on failure, both
// documented where they stand.

/// Raised timer resolution for the process, released on drop.
///
/// #920 lie 1: without `timeBeginPeriod(1)`, Windows quantizes blocking waits
/// to 15.6 ms, which would read as a sidecar verdict rather than a scheduler
/// artifact. The harness can run with and without this raised — both numbers
/// are asked for — so it is an explicit, droppable grant, not ambient state.
#[derive(Debug)]
pub struct TimerResolution {
    #[cfg(windows)]
    active: bool,
}

impl TimerResolution {
    /// Ask for `ms` millisecond timer resolution. On non-Windows this is a
    /// no-op that still records the request, so the report field means the
    /// same thing on every platform.
    #[must_use]
    // Const on Windows is impossible: the Windows branch calls an FFI
    // function. The lint sees only the non-Windows branch.
    #[allow(clippy::missing_const_for_fn)]
    pub fn raise(_ms: u32) -> Self {
        #[cfg(windows)]
        {
            // TIME_PERIOD-equivalent: 1 ms. `timeBeginPeriod` returns 0 on
            // success; a failure leaves `active` false and the report says so.
            let status = windows_sys::Win32::Media::timeBeginPeriod(_ms);
            Self {
                active: status == 0,
            }
        }
        #[cfg(not(windows))]
        {
            Self {}
        }
    }

    /// Whether the raised resolution is actually in effect (always true off
    /// Windows, where there is nothing to raise).
    #[must_use]
    pub const fn active(&self) -> bool {
        #[cfg(windows)]
        {
            self.active
        }
        #[cfg(not(windows))]
        {
            true
        }
    }
}

impl Drop for TimerResolution {
    fn drop(&mut self) {
        #[cfg(windows)]
        if self.active {
            windows_sys::Win32::Media::timeEndPeriod(1);
        }
    }
}

/// A TCP stream configured for the loopback measurement: `TCP_NODELAY` set
/// before a single byte moves.
///
/// #920 lie 2: without `TCP_NODELAY`, Nagle plus delayed ACK stalls a
/// request/response loop 40–200 ms and falsely overturns the sidecar. Both
/// ends of both directions get this — the listener sets it on every accepted
/// stream too, so no measurement path exists where it was skipped.
///
/// # Errors
///
/// Returns the underlying I/O error if the socket option cannot be set.
pub fn set_nodelay(stream: &std::net::TcpStream) -> io::Result<()> {
    stream.set_nodelay(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn frame_bytes(stream: &[u8]) -> Vec<u8> {
        stream.to_vec()
    }

    #[test]
    fn a_codec_message_whose_payload_contains_magic_roundtrips_the_prefix() {
        // The reason the prefix must exist: MAGIC inside an opaque input
        // payload. Without the outer length prefix, `orrery_ipc::decode`
        // cannot find the message boundary here — it would read the payload's
        // ORIP as a header.
        let message = orrery_ipc::EngineToSidecar::Input(orrery_ipc::InputBatch {
            tick: orrery_protocol::Tick::new(7),
            inputs: vec![orrery_ipc::EntityInput {
                target: orrery_protocol::PersistId::new(1),
                sequence: 9,
                payload: vec![b'O', b'R', b'I', b'P', 0x00, 0xFF, b'O', b'R'],
            }],
        });
        let codec_bytes = message.clone().encode().expect("message encodes");

        let mut stream = Vec::new();
        {
            let mut writer = FrameWriter::new(&mut stream);
            let envelope = encode_envelope(41, 1_000_000, &codec_bytes);
            writer.write_frame(&envelope).expect("frame writes");
            writer.flush().expect("flushes");
        }

        // The prefix is really on the wire: u32 LE of the envelope length.
        #[allow(clippy::cast_possible_truncation)] // a test fixture, far under the cap
        let expected_len = (ENVELOPE_HEADER + codec_bytes.len()) as u32;
        assert_eq!(
            &stream[0..PREFIX_LEN],
            expected_len.to_le_bytes(),
            "first four bytes must be the little-endian u32 length prefix"
        );

        let mut reader = FrameReader::new(Cursor::new(frame_bytes(&stream)));
        let frame = reader
            .read_frame()
            .expect("frame reads")
            .expect("stream not at end");
        let (seq, t_send, payload) = decode_envelope(&frame).expect("envelope decodes");
        assert_eq!((seq, t_send), (41, 1_000_000));
        assert_eq!(
            orrery_ipc::EngineToSidecar::decode(payload),
            Ok(message),
            "the codec message must survive the framing round trip intact"
        );
        assert!(reader.read_frame().expect("eof check").is_none());
    }

    #[test]
    fn a_dropped_sequence_is_reported_as_a_gap() {
        let mut watcher = SeqWatcher::new();
        assert_eq!(watcher.observe(0), SeqEvent::InOrder);
        assert_eq!(watcher.observe(1), SeqEvent::InOrder);
        // Sequence 2 never arrived.
        assert_eq!(
            watcher.observe(3),
            SeqEvent::Gap { missing: 1 },
            "a missing sequence must be reported, not silently accepted"
        );
        assert_eq!(watcher.observe(4), SeqEvent::InOrder);
    }

    #[test]
    fn a_reordered_sequence_is_reported_as_a_reorder() {
        let mut watcher = SeqWatcher::new();
        assert_eq!(watcher.observe(10), SeqEvent::InOrder);
        assert_eq!(
            watcher.observe(12),
            SeqEvent::Gap { missing: 1 },
            "12 ahead of 10 is a gap until 11 shows up"
        );
        assert_eq!(
            watcher.observe(11),
            SeqEvent::Reorder {
                expected: 13,
                received: 11
            },
            "an out-of-order arrival must be distinguishable from a drop"
        );
    }

    #[test]
    fn a_frame_over_the_cap_is_refused_at_both_ends() {
        let mut stream = Vec::new();
        {
            let mut writer = FrameWriter::new(&mut stream);
            let too_big = vec![0u8; MAX_FRAME_LEN + 1];
            assert!(writer.write_frame(&too_big).is_err());
        }
        // A hostile or corrupted prefix claiming over the cap must also be
        // refused by the reader, before allocating.
        #[allow(clippy::cast_possible_truncation)] // the cap fits u32 by construction
        let claimed = (MAX_FRAME_LEN as u32 + 1).to_le_bytes();
        stream.extend_from_slice(&claimed);
        let mut reader = FrameReader::new(Cursor::new(stream));
        let error = reader
            .read_frame()
            .expect_err("an over-cap declared length must fail");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn a_truncated_body_is_a_fatal_error_and_a_clean_eof_is_none() {
        let mut stream = Vec::new();
        stream.extend_from_slice(&8u32.to_le_bytes());
        stream.extend_from_slice(&[1, 2, 3]); // says 8, carries 3
        let mut reader = FrameReader::new(Cursor::new(stream));
        let error = reader.read_frame().expect_err("truncation must fail");
        assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof);

        let mut reader = FrameReader::new(Cursor::new(Vec::new()));
        assert!(reader.read_frame().expect("clean eof").is_none());
    }

    #[test]
    fn an_empty_body_roundtrips() {
        let mut stream = Vec::new();
        {
            let mut writer = FrameWriter::new(&mut stream);
            writer.write_frame(&[]).expect("empty frame writes");
        }
        let mut reader = FrameReader::new(Cursor::new(stream));
        let frame = reader
            .read_frame()
            .expect("frame reads")
            .expect("stream not at end");
        assert!(frame.is_empty());
    }

    #[test]
    fn the_monotonic_clock_never_goes_backwards_and_moves() {
        let mut previous = monotonic_now_ns();
        for _ in 0..1_000 {
            let now = monotonic_now_ns();
            assert!(now >= previous, "the monotonic clock went backwards");
            previous = now;
        }
        std::thread::sleep(core::time::Duration::from_millis(2));
        assert!(
            monotonic_now_ns() >= previous + 1_000_000,
            "the monotonic clock did not advance across a 2 ms sleep"
        );
    }

    #[test]
    fn an_envelope_shorter_than_its_header_is_refused() {
        assert_eq!(decode_envelope(&[0u8; 15]), Err(EnvelopeError::Truncated));
    }
}
