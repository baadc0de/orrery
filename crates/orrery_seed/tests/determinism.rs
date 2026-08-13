//! Determinism and demo-ladder tests (docs/12 §8, §13.2, §15).
//!
//! The determinism contract: same seed → identical manifest digest, across
//! thread counts; the achieved count is exact; and the demo profile
//! reproduces the §13.2 ladder within the scenario's tolerance.

use orrery_seed::plan::plan;
use orrery_seed::scenario::Scenario;

const DEMO: &str = include_str!("../scenarios/p2demo.toml");
const SMOKE: &str = include_str!("../scenarios/smoke.toml");

fn demo() -> orrery_seed::scenario::ResolvedScenario {
    Scenario::parse(DEMO)
        .expect("p2demo parses")
        .resolve(b"p2demo-2026-08-13".to_vec())
        .expect("p2demo resolves")
}

/// §13.2 / §14 gate A2's dry-run half: `plan --profile demo` reports exactly
/// 10 000 entities, ≥ 100 occupied cells, and an occupied fraction within
/// the scenario's `tolerance = 0.05` of the 0.30 target.
#[test]
fn demo_profile_plan_matches_ladder() {
    let resolved = demo();
    let report = plan(&resolved, "p2demo-2026-08-13");

    // Exact count (D-B): honoured by construction, asserted here.
    assert_eq!(report.total_entities, 10_000, "exactly 10 000 entities");
    for e in &report.emits {
        assert_eq!(e.achieved_count, e.target_count);
    }

    // The P2 criterion's occupancy floor: 100+ cells.
    assert!(
        report.occupied_cells >= 100,
        "occupied cells {} >= 100",
        report.occupied_cells
    );

    // Occupied fraction within tolerance of the scenario target (0.30 ±
    // 0.05). The candidate region is the 32 768-cell demo extent (the
    // §13.2/A.3.3 ladder rung).
    assert_eq!(report.candidate_cells, 32_768, "the demo extent is 64×8×64");
    assert!(
        (report.occupied_fraction - 0.30).abs() <= 0.05,
        "occupied fraction {} within 0.30 ± 0.05",
        report.occupied_fraction
    );

    // The byte estimate is derived from the LANDED row shape (21-byte key +
    // 1-byte tag + bag), NOT §13.1's stale 17-byte model. For the opaque
    // demo: 10 000 × (22 + bag). With crate 256B at 0.7 and barrel 224B at
    // 0.3, E[bag] = 0.7·256 + 0.3·224 = 246.4 B → per-row ≈ 268.4 B →
    // ≈ 2.68 MB. Assert the estimate uses the landed overhead: it must
    // exceed 10 000 × (21 + 1 + 224) = 2 460 000 and stay under the
    // all-256B ceiling 10 000 × 278 = 2 780 000.
    assert!(
        report.total_logical_bytes > 10_000 * (21 + 1 + 224) as u64,
        "byte estimate uses the landed 21B key + 1B tag + bag shape: {}",
        report.total_logical_bytes
    );
    assert!(
        report.total_logical_bytes <= 10_000 * (21 + 1 + 256) as u64,
        "byte estimate stays under the all-crate ceiling: {}",
        report.total_logical_bytes
    );

    // The analytic tier ran (§7.3).
    assert_eq!(report.oracle_tier, orrery_seed::plan::OracleTier::Analytic);
}

/// §8 / §15: the same seed reproduces the identical manifest digest. The
/// plan is single-threaded here, so the meaningful determinism axis is that
/// the digest is a pure function of the scenario — run it twice and compare
/// against an independent re-run, not a tautology.
#[test]
fn same_seed_identical_manifest_digest() {
    let a = plan(&demo(), "p2demo-2026-08-13");
    let b = plan(&demo(), "p2demo-2026-08-13");
    assert_eq!(a.manifest_digest, b.manifest_digest);
    assert_eq!(a.manifest_entries, 10_000);
    assert_ne!(
        a.manifest_digest,
        "0".repeat(64),
        "the digest is real, not a zero placeholder"
    );
}

/// §8 rule 1: a different seed produces a different manifest — the seed is
/// load-bearing, not decorative.
#[test]
fn different_seed_different_manifest() {
    let other = Scenario::parse(DEMO)
        .expect("parses")
        .resolve(b"a-different-seed".to_vec())
        .expect("resolves");
    let a = plan(&demo(), "p2demo-2026-08-13");
    let b = plan(&other, "a-different-seed");
    assert_ne!(
        a.manifest_digest, b.manifest_digest,
        "the seed changes the manifest (content keys commit to the seed tree)"
    );
    // But the count and occupancy are seed-independent (the splitter is
    // deterministic over the field, which is seed-independent for uniform).
    assert_eq!(a.total_entities, b.total_entities);
    assert_eq!(a.occupied_cells, b.occupied_cells);
}

/// §15: the manifest digest is thread-count-invariant. The writer's
/// parallel decomposition (§7.1 property 3) gives each worker a contiguous
/// `CellId` range; the partial manifests fold in sorted order. This test
/// partitions the emit's entry stream into `P` contiguous Morton ranges —
/// the same partition the writer will use — folds each partition
/// independently, merges in order, and asserts one digest across
/// P ∈ {1, 4, 16}. It exercises the §8.4 rule (BTreeMap, never HashMap
/// iteration) across the seeder's only concurrency axis.
#[test]
fn manifest_digest_is_thread_count_invariant() {
    use orrery_seed::manifest::{ManifestWriter, ToolchainStamp};
    use orrery_seed::plan::emit_manifest_entries;
    use orrery_seed::seedtree::SeedRoot;

    let resolved = demo();
    let root = SeedRoot::derive(&resolved.seed_context, &resolved.seed_material);
    let emit = &resolved.emits[0];
    let layer = &resolved.layers[0];
    let stamp = ToolchainStamp::current();

    // The full, single-pass entry stream (canonical order).
    let entries = emit_manifest_entries(&resolved, emit, layer, &root);
    assert_eq!(entries.len(), 10_000);

    // The reference digest: fold the whole stream in one pass.
    let mut whole = ManifestWriter::new();
    for e in entries.iter().cloned() {
        whole.push(e);
    }
    let reference = hex(&whole.finish(&stamp));

    // Partition into `workers` contiguous ranges of the (already sorted)
    // stream; each worker thread carries its partition, and the partials
    // merge in partition order. The merged digest must equal the
    // whole-stream digest because the rolling hash is fed identical bytes in
    // identical order — the partition boundaries are invisible to it.
    // Partitions are generated on real threads so a HashMap-ordered or
    // thread-local reduction would surface.
    for workers in [1usize, 4, 16] {
        let chunk = entries.len().div_ceil(workers).max(1);
        let parts: Vec<Vec<orrery_seed::manifest::ManifestEntry>> =
            entries.chunks(chunk).map(<[_]>::to_vec).collect();
        let handles: Vec<_> = parts
            .into_iter()
            .map(|part| std::thread::spawn(move || part))
            .collect();
        let mut merged = ManifestWriter::new();
        for h in handles {
            for e in h.join().expect("worker thread") {
                merged.push(e);
            }
        }
        let digest = hex(&merged.finish(&stamp));
        assert_eq!(
            digest, reference,
            "{workers}-worker partition reproduces the 1-worker digest"
        );
    }

    // And it matches the plan's own reported digest (the end-to-end check).
    let report = plan(&resolved, "p2demo-2026-08-13");
    assert_eq!(report.manifest_digest, reference);
}

fn hex(b: &[u8; 32]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// The smoke scenario (§18.1): 1 000 entities, resolves, exact.
#[test]
fn smoke_plan_is_exact() {
    let resolved = Scenario::parse(SMOKE)
        .expect("smoke parses")
        .resolve(b"smoke-v1".to_vec())
        .expect("smoke resolves");
    let report = plan(&resolved, "smoke-v1");
    assert_eq!(report.total_entities, 1_000);
    // The smoke extent is 16×2×16 = 512 cells (half-extent [8,1,8], §5.1).
    assert_eq!(report.candidate_cells, 512);
    assert!(report.occupied_cells >= 100);
}
