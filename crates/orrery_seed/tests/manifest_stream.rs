//! The manifest writer's memory is bounded by one entry, not by the world
//! (docs/12-world-seeding.md §9.3: canonical order is generation order "so
//! the manifest streams out without a sort pass", at "470 MB at 10 M
//! entities").
//!
//! This is a *measurement*, not an assertion of style: a counting global
//! allocator records the live-bytes high-water mark across a run that writes
//! a large manifest to a discarding sink. A writer that materializes the
//! collection — the pre-fix `to_vec_pretty(&entries)` shape — has a peak that
//! grows with the entry count; the streaming sink's does not.
//!
//! The file holds exactly one `#[test]` on purpose. The allocator counters
//! are process-global, so a second test running concurrently in the same
//! binary would pollute the high-water mark.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

use orrery_protocol::{CellId, GridId, PersistId};
use orrery_seed::content::ContentKey;
use orrery_seed::manifest::{ManifestEntry, ManifestSink, ToolchainStamp};

/// Live bytes now, the high-water mark since [`reset_peak`], and the live
/// level that reset was taken at.
static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);
static BASE: AtomicUsize = AtomicUsize::new(0);

struct Counting;

// Relaxed ordering throughout: these are statistics, not a synchronization
// protocol, and the test's own reads happen after the measured work on the
// same thread.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { System.alloc(layout) };
        if !ptr.is_null() {
            bump(layout.size());
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let out = unsafe { System.realloc(ptr, layout, new_size) };
        if !out.is_null() {
            LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
            bump(new_size);
        }
        out
    }
}

fn bump(size: usize) {
    let live = LIVE.fetch_add(size, Ordering::Relaxed) + size;
    PEAK.fetch_max(live, Ordering::Relaxed);
}

#[global_allocator]
static ALLOC: Counting = Counting;

fn reset_peak() {
    let live = LIVE.load(Ordering::Relaxed);
    BASE.store(live, Ordering::Relaxed);
    PEAK.store(live, Ordering::Relaxed);
}

/// Peak live bytes above the level at the last [`reset_peak`].
///
/// Measured against the *baseline*, not against the live total at the end of
/// the run. Subtracting the final live level would hide exactly the failure
/// this test exists to catch: a writer that retains every entry ends with a
/// live total as high as its peak, so the difference would read as zero.
fn peak_growth() -> usize {
    PEAK.load(Ordering::Relaxed)
        .saturating_sub(BASE.load(Ordering::Relaxed))
}

/// A sink that keeps the bytes' shape and nothing else — it stands in for
/// the file, so what the measurement sees is the writer's own footprint and
/// not the output buffer's.
#[derive(Default, Clone, Copy)]
struct CountingSink {
    bytes: usize,
    lines: usize,
    longest_write: usize,
}

impl std::io::Write for CountingSink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.bytes += buf.len();
        self.lines += buf.iter().filter(|b| **b == b'\n').count();
        self.longest_write = self.longest_write.max(buf.len());
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// One entry, generated on demand. Successive `i` are ascending in
/// `(grid, cell, ContentKey)` — §9.3's canonical order, which the sink
/// enforces. Building them one at a time is the point: holding them in a
/// `Vec` first would be the very thing under test.
fn entry(i: u64) -> ManifestEntry {
    let mut key = [0u8; 16];
    key[..8].copy_from_slice(&i.to_be_bytes());
    ManifestEntry {
        content_key: ContentKey(key),
        persist_id: PersistId::new(i + 1),
        grid: GridId::ROOT,
        // One fixed cell: §9.3's order is lexicographic over the triple, so
        // a constant cell with an ascending `ContentKey` is canonical, and it
        // keeps the fixture independent of the cell-id bit layout.
        cell: CellId::from_bits(0xA924_9249_2492_4D65).expect("nonzero cell"),
        value_digest: [0xEE; 16],
        byte_len: 256,
        archetype: "crate".to_string(),
        layer: "world".to_string(),
        emit: "props".to_string(),
    }
}

/// Write `n` entries and report `(peak growth, sink counters)`.
fn measure(n: u64) -> (usize, CountingSink) {
    let mut sink = ManifestSink::new(CountingSink::default());
    // Warm the serializer's one-off allocations and the line buffer before
    // the baseline is taken, so the reset starts from a steady state.
    sink.push(&entry(0)).expect("write");
    reset_peak();
    for i in 1..n {
        sink.push(&entry(i)).expect("write");
    }
    let peak = peak_growth();
    let counters = *sink.get_ref();
    let digest = sink
        .finish_without_record(&ToolchainStamp::current())
        .expect("flush");
    assert_ne!(digest, [0u8; 32], "the digest is still produced");
    (peak, counters)
}

#[test]
fn manifest_writer_peak_memory_is_independent_of_entry_count() {
    // Two runs an order of magnitude apart. If the writer materialized the
    // collection, the 10x run's peak would be ~10x the 1x run's.
    let small = 4_096u64;
    let large = 40_960u64;

    let (small_peak, small_sink) = measure(small);
    let (large_peak, large_sink) = measure(large);

    // One line per entry, no fewer and no more: JSONL, not one array.
    assert_eq!(small_sink.lines as u64, small, "one JSON line per entry");
    assert_eq!(large_sink.lines as u64, large, "one JSON line per entry");
    assert!(
        large_sink.bytes > small_sink.bytes * 9,
        "the payload really did grow ~10x ({} vs {} bytes)",
        large_sink.bytes,
        small_sink.bytes
    );

    // The claim under test: the high-water mark is bounded by one entry, so
    // it does not move with the entry count. The bound is a small multiple of
    // the longest single line, not a fraction of the payload.
    let bound = large_sink.longest_write * 8 + 4_096;
    assert!(
        small_peak <= bound && large_peak <= bound,
        "peak live bytes must be bounded by one entry, not by the world: \
         {small} entries peaked at {small_peak} B, {large} entries at \
         {large_peak} B, bound {bound} B (payloads {} and {} B)",
        small_sink.bytes,
        large_sink.bytes
    );
    assert!(
        large_peak < large_sink.bytes / 4,
        "a peak of {large_peak} B against a {} B manifest is not streaming",
        large_sink.bytes
    );

    eprintln!(
        "manifest peak: {small} entries -> {small_peak} B (payload {} B); \
         {large} entries -> {large_peak} B (payload {} B)",
        small_sink.bytes, large_sink.bytes
    );
}
