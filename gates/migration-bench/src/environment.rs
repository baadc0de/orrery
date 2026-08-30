//! The environment manifest, and the fields the baseline-refusal rule keys on.
//!
//! A baseline is only a baseline for the environment it was captured in: same
//! toolchain, same target triple, same build profile, same CPU model (A10
//! §8.1's "same host class, same pinned toolchain"). [`EnvironmentManifest`]
//! records that environment next to every capture, and
//! [`EnvironmentManifest::refusals`] is the half a machine can hold honestly —
//! the differential harness refuses, by named field, to compare across
//! environments that differ in any of them.

use std::collections::BTreeMap;
use std::process::Command;

use serde::{Deserialize, Serialize};

use orrery_conformance::REFERENCE_RULESET;
use orrery_games::{Game, GameVisitor, for_each_game};

/// The environment a suite run happened in.
///
/// Everything here is recorded per capture. The `refuse_fields` are matched on
/// refusal; the rest is recorded so a reader can tell *what kind* of box
/// produced the numbers without having to trust tribal memory.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EnvironmentManifest {
    /// The commit whose tree was measured. Advisory: a differential comparison
    /// is a comparison *across* commits, so this field is reported, never
    /// matched.
    pub commit: String,
    /// Whether the working tree had uncommitted changes at capture time. A
    /// baseline captured dirty does not pin anything; the field exists so such
    /// a capture cannot pretend otherwise.
    pub tree_dirty: bool,
    /// `rustc --version` of the toolchain that built the harness.
    pub rustc_version: String,
    /// The target triple the harness binary was built for (cargo's `TARGET`,
    /// taken at compile time — the instruction set actually measured).
    pub target_triple: String,
    /// `release` or `debug`. A debug candidate against a release baseline
    /// measures the optimizer's absence, so this is matched on refusal.
    pub build_profile: String,
    /// The CPU the numbers came from.
    pub cpu: CpuInfo,
    /// Total RAM, GiB. Recorded, not matched — tick cost is single-threaded
    /// and the suite holds no meaningful pressure on memory bandwidth.
    pub ram_gib: f64,
    /// OS and kernel.
    pub os: String,
    /// blake3 over this workspace's committed `Cargo.lock`. Advisory: a
    /// dependency-graph change shifts measurements for reasons unrelated to
    /// the code under comparison, so a difference is warned about, loudly,
    /// but dependency bumps inside the D14 pins are not refusals.
    pub cargo_lock_blake3: String,
    /// The ruleset builds in force: the reference ruleset and every game in
    /// the catalogue, by name (A10 §8.3's "golden-table versions in force").
    pub rulesets: BTreeMap<String, RulesetStamp>,
    /// The harness version that captured this.
    pub captured_with: String,
}

/// The CPU half of the manifest.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CpuInfo {
    /// Marketing model string, e.g. "AMD Ryzen 9 9950X3D 16-Core Processor".
    /// "unknown" where the platform does not expose one. Matched on refusal.
    pub model: String,
    /// Logical cores.
    pub cores: u32,
}

/// One ruleset build's identity, for the manifest.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RulesetStamp {
    /// The ruleset's monotonic version.
    pub version: u32,
    /// Its build digest, lower-case hex.
    pub digest: String,
}

/// One field whose value differs between the baseline's environment and this
/// one — the content of an environment-mismatch refusal.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EnvironmentRefusal {
    /// The manifest field, dotted for nested values (`cpu.model`).
    pub field: String,
    /// What the baseline recorded.
    pub baseline: String,
    /// What this environment reports.
    pub current: String,
}

impl EnvironmentManifest {
    /// Capture the environment this binary is running in, right now.
    pub fn capture() -> Self {
        let (commit, tree_dirty) = git_state().unwrap_or_else(|| ("unknown".to_string(), true));
        Self {
            commit,
            tree_dirty,
            rustc_version: rustc_version(),
            target_triple: env!("MIGRATION_BENCH_TARGET").to_string(),
            build_profile: env!("MIGRATION_BENCH_PROFILE").to_string(),
            cpu: CpuInfo::capture(),
            ram_gib: ram_gib(),
            os: os_description(),
            cargo_lock_blake3: cargo_lock_digest(),
            rulesets: ruleset_stamps(),
            captured_with: env!("CARGO_PKG_VERSION").to_string(),
        }
    }

    /// The fields the baseline-refusal rule matches on, with the values that
    /// differ. Empty means the environments agree and a comparison may run.
    ///
    /// Matching is deliberately narrow — toolchain, target triple, build
    /// profile, CPU model — because every field added here makes the next
    /// environment change a new baseline, and a rule that refuses too often
    /// stops being a rule and becomes a ritual. A10 §8.1's requirement is
    /// "same host class, same pinned toolchain"; these four are that.
    pub fn refusals(&self, current: &EnvironmentManifest) -> Vec<EnvironmentRefusal> {
        let mut out = Vec::new();
        let fields = [
            ("rustc_version", &self.rustc_version, &current.rustc_version),
            ("target_triple", &self.target_triple, &current.target_triple),
            ("build_profile", &self.build_profile, &current.build_profile),
            ("cpu.model", &self.cpu.model, &current.cpu.model),
        ];
        for (field, baseline, current) in fields {
            if baseline != current {
                out.push(EnvironmentRefusal {
                    field: field.to_string(),
                    baseline: baseline.clone(),
                    current: current.clone(),
                });
            }
        }
        out
    }
}

impl CpuInfo {
    /// Best-effort CPU identification. "unknown" is a legal answer: the
    /// manifest records what the platform volunteers, and an all-"unknown"
    /// manifest still matches itself.
    fn capture() -> Self {
        Self {
            model: cpu_model().unwrap_or_else(|| "unknown".to_string()),
            cores: std::thread::available_parallelism()
                .map(|n| n.get() as u32)
                .unwrap_or(0),
        }
    }
}

/// The ruleset builds in force: the reference ruleset plus every game in the
/// catalogue, so a baseline names the behaviour it timed.
fn ruleset_stamps() -> BTreeMap<String, RulesetStamp> {
    let mut out = BTreeMap::new();
    out.insert(
        "reference".to_string(),
        RulesetStamp {
            version: REFERENCE_RULESET.version,
            digest: orrery_conformance::corpus::hex(&REFERENCE_RULESET.digest),
        },
    );
    struct Stamps(BTreeMap<String, RulesetStamp>);
    impl GameVisitor for Stamps {
        fn visit<G: Game>(&mut self) {
            self.0.insert(
                G::META.name.to_string(),
                RulesetStamp {
                    version: G::META.ruleset.version,
                    digest: orrery_conformance::corpus::hex(&G::META.ruleset.digest),
                },
            );
        }
    }
    let mut stamps = Stamps(BTreeMap::new());
    for_each_game(&mut stamps);
    out.extend(stamps.0);
    out
}

/// The repo state the suite ran against: HEAD, and whether the tree was
/// clean. The harness is a tool of this repository, so the repo root is the
/// manifest directory's grandparent.
fn git_state() -> Option<(String, bool)> {
    let root = repo_root()?;
    let commit = Command::new("git")
        .args(["-C", &root, "rev-parse", "HEAD"])
        .output()
        .ok()
        .filter(|out| out.status.success())
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())?;
    let dirty = Command::new("git")
        .args(["-C", &root, "status", "--porcelain"])
        .output()
        .map(|out| !out.stdout.is_empty())
        .unwrap_or(true);
    Some((commit, dirty))
}

fn repo_root() -> Option<String> {
    let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let root = manifest.join("../..").canonicalize().ok()?;
    root.to_str().map(String::from)
}

/// `rustc --version` of the toolchain on PATH — the one cargo resolves to,
/// wrapper or not. The version line is what identifies the pinned toolchain.
fn rustc_version() -> String {
    Command::new("rustc")
        .arg("--version")
        .output()
        .ok()
        .filter(|out| out.status.success())
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

/// First `model name` in /proc/cpuinfo. None off Linux.
fn cpu_model() -> Option<String> {
    let info = std::fs::read_to_string("/proc/cpuinfo").ok()?;
    for line in info.lines() {
        if let Some(model) = line.strip_prefix("model name") {
            let model = model.trim_start_matches([':', '\t', ' ']).trim();
            if !model.is_empty() {
                return Some(model.to_string());
            }
        }
    }
    None
}

/// Total RAM in GiB, from /proc/meminfo's `MemTotal`. 0.0 where unknown.
fn ram_gib() -> f64 {
    let Ok(info) = std::fs::read_to_string("/proc/meminfo") else {
        return 0.0;
    };
    for line in info.lines() {
        if let Some(rest) = line.strip_prefix("MemTotal:") {
            let kib: f64 = rest
                .trim()
                .trim_end_matches("kB")
                .trim()
                .parse()
                .unwrap_or(0.0);
            return kib / (1024.0 * 1024.0);
        }
    }
    0.0
}

/// OS name and, on Linux, the kernel release — the part of "the box" that
/// moves under the measurements.
fn os_description() -> String {
    let base = std::env::consts::OS.to_string();
    match std::fs::read_to_string("/proc/sys/kernel/osrelease") {
        Ok(release) => format!("{} {}", base, release.trim()),
        Err(_) => base,
    }
}

/// blake3 over this workspace's committed `Cargo.lock`.
fn cargo_lock_digest() -> String {
    let lock = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.lock");
    match std::fs::read(&lock) {
        Ok(bytes) => blake3::hash(&bytes).to_string(),
        Err(_) => "unknown".to_string(),
    }
}
