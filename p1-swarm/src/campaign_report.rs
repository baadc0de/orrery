//! Host-side assembly of a bankable campaign report (#387, slice 3).
//!
//! A bot hour's evidence is produced entirely inside one harness process. A
//! human hour is split across two processes: the rendered client measures its
//! own link and finishes a [`SessionRecord`](crate-shaped row); the hosting
//! harness produces everything around the run exactly as it does for pure-bot
//! runs (`witnessing`, coverage, false positives, deferral balance). This
//! module is where the two halves meet — the *only* place they meet — so the
//! assembled report banks through the ordinary `p4-ledger.sh append` path
//! with every existing refusal intact.
//!
//! # What the host asserts, outside tests
//!
//! Three things are checked here rather than trusted from the client, each
//! refusing loudly at assembly time:
//!
//! 1. **Identity**: the row's `session_id` must equal the invite-bound
//!    identity the host admitted at join (`--expected-session-id`). A row
//!    from some other session cannot ride this run's witnessing evidence.
//! 2. **Platform**: the row's `platform_triple` must equal the host's own
//!    build triple — the ledger folds hours per platform off this field.
//! 3. **Telemetry honesty**: the row's `impairment_mismatch` flag must agree
//!    with its own observed-vs-configured numbers, recomputed here. A row
//!    claiming verified impairment its measurements contradict — or flagging
//!    a mismatch they deny — refuses the whole assembly. This is the
//!    production assertion for #387's telemetry-honesty clause; the flag is
//!    computed client-side by `CampaignSession::finish`, and this is where a
//!    lie is caught.
//!
//! # What the host replaces, and why that is not tampering
//!
//! The client stamps `pipeline_digest` honestly as unavailable: a client-side
//! process cannot know the digest until the run's provenance exists. Assembly
//! fills it with the digest computed from the four hashed trees at this
//! commit — the same recipe `p4-ledger.sh pipeline-id` prints — because the
//! field is coordinator-supplied provenance by design. The ledger still
//! recomputes it from the report's own commit and refuses on disagreement,
//! so this value is a claim, never the authority.
//!
//! # Return path
//!
//! Manual collection is deliberately sufficient at shakedown scale: the
//! client's record file travels by hand (localhost runs share the disk;
//! cohort sessions can send the small JSONL), and either the hosting run or
//! `--assemble-campaign-report` merges it afterwards. An S3 upload path is
//! the named replacement when the campaign outgrows hand-carried files; it
//! is deliberately not built here.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;
use serde_json::Value;

use crate::exterior;

/// The trees the false-positive rate is a property of — restated from
/// `scripts/p4-ledger.sh`. The cross-check test below fails the moment either
/// copy drifts; the digest itself is verified downstream regardless, which is
/// what makes this copy a claim rather than an authority.
const PIPELINE_TREES: [&str; 4] = [
    "crates/orrery_witness",
    "crates/orrery_core",
    "crates/orrery_games",
    "p1-swarm",
];

/// One external participant's finished session row, read back from the JSONL
/// stream the client appends at exit.
///
/// Deliberately a [`serde_json::Value`] under a typed reader: unknown fields
/// survive assembly untouched, so the row the ledger sees is the row the
/// client wrote plus the one coordinator-supplied field.
#[derive(Debug, Clone)]
pub struct ParticipantRecord {
    value: Value,
}

#[derive(Deserialize)]
struct RecordView {
    session_id: String,
    actor: String,
    platform_triple: String,
    #[serde(rename = "configured_impairment_profile")]
    configured: ConfiguredProfile,
    observed_loss_pct: f64,
    observed_jitter_p50_ms: u64,
    observed_jitter_p99_ms: u64,
    impairment_mismatch: bool,
}

#[derive(Deserialize)]
struct ConfiguredProfile {
    loss_pct: f64,
    jitter_p50_ms: u64,
    jitter_p99_ms: u64,
}

impl ParticipantRecord {
    /// Parse one JSONL line into a participant row.
    ///
    /// # Errors
    /// When the line is not the JSON object a joined client writes, or any
    /// field the honesty assertions read below is missing or mistyped.
    pub fn parse(line: &str) -> Result<Self> {
        let value: Value =
            serde_json::from_str(line.trim()).context("session record is not JSON")?;
        if !value.is_object() {
            bail!("session record is not a JSON object");
        }
        // Typed view forces every asserted field to exist with the right
        // type; failures name themselves through serde's messages.
        let _view: RecordView = serde_json::from_value(value.clone())
            .context("session record is missing or mistyping required fields")?;
        Ok(Self { value })
    }

    fn view(&self) -> Result<RecordView> {
        serde_json::from_value(self.value.clone())
            .map_err(|error| anyhow!("session record unreadable: {error}"))
    }

    /// The raw JSON object, for embedding into the report.
    #[must_use]
    pub fn value(&self) -> &Value {
        &self.value
    }

    /// The session identity the row names.
    ///
    /// # Errors
    /// When the field is missing (already forced at parse; kept fallible so
    /// callers never index the raw value directly).
    pub fn session_id(&self) -> Result<String> {
        Ok(self.view()?.session_id)
    }

    /// Reads the last complete record from a client-side JSONL stream.
    ///
    /// # Errors
    /// When the file exists but holds no readable record.
    pub fn read_latest(path: &Path) -> Result<Option<Self>> {
        let contents = match std::fs::read_to_string(path) {
            Ok(contents) => contents,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error).context(format!("reading {}", path.display())),
        };
        let parsed = contents
            .lines()
            .rev()
            .find(|line| !line.trim().is_empty())
            .map(|line| Self::parse(line).context(format!("parsing {}", path.display())))
            .transpose()?;
        Ok(parsed)
    }
}

/// Asserts the row's mismatch flag agrees with its own numbers.
///
/// Recomputed here, at assembly time, outside tests: observation may disagree
/// with configuration (that is what the flag is *for*), but the flag may not
/// disagree with the observation.
///
/// # Errors
/// Naming the contradiction, in either direction.
pub fn assert_mismatch_consistency(record: &ParticipantRecord) -> Result<()> {
    let view = record.view()?;
    let measured_disagrees = (view.observed_loss_pct - view.configured.loss_pct).abs()
        > f64::EPSILON
        || view.observed_jitter_p50_ms != view.configured.jitter_p50_ms
        || view.observed_jitter_p99_ms != view.configured.jitter_p99_ms;
    if view.impairment_mismatch && !measured_disagrees {
        bail!(
            "telemetry honesty: the row flags an impairment mismatch its own numbers \
             deny (observed {:.2}%/{}/{}, configured {:.2}%/{}/{})",
            view.observed_loss_pct,
            view.observed_jitter_p50_ms,
            view.observed_jitter_p99_ms,
            view.configured.loss_pct,
            view.configured.jitter_p50_ms,
            view.configured.jitter_p99_ms,
        );
    }
    if !view.impairment_mismatch && measured_disagrees {
        bail!(
            "telemetry honesty: the row claims verified impairment while its own \
             measurement disagrees with configuration (observed {:.2}%/{}/{}, \
             configured {:.2}%/{}/{})",
            view.observed_loss_pct,
            view.observed_jitter_p50_ms,
            view.observed_jitter_p99_ms,
            view.configured.loss_pct,
            view.configured.jitter_p50_ms,
            view.configured.jitter_p99_ms,
        );
    }
    Ok(())
}

/// Assembles the participant's row into the run report.
///
/// Every check refuses the assembly rather than warning: a report that
/// should not bank must not exist to be appended by mistake.
///
/// # Errors
/// Identity, platform, or telemetry-honesty contradictions, named.
pub fn assemble(
    report: &mut Value,
    record: &ParticipantRecord,
    expected_session_id: &str,
    host_target: &str,
    pipeline_digest: &str,
) -> Result<()> {
    let Some(identity) = report.get_mut("identity").and_then(Value::as_object_mut) else {
        bail!("run report has no identity block to stamp");
    };

    let view = record.view()?;
    if view.actor != "human" {
        bail!(
            "session record actor is {:?}; a hosted campaign slot banks human rows",
            view.actor
        );
    }
    if view.session_id != expected_session_id {
        bail!(
            "session record {} does not name the invited session {expected_session_id}",
            view.session_id
        );
    }
    if view.platform_triple != host_target {
        bail!(
            "session record names platform {:?} but this host ran {:?}",
            view.platform_triple,
            host_target
        );
    }
    assert_mismatch_consistency(record)?;

    // Coordinator-supplied provenance (see module docs): the one field the
    // client cannot know, replaced here, verified downstream by the ledger.
    let mut row = record.value().clone();
    if let Some(row_object) = row.as_object_mut() {
        row_object.insert(
            "pipeline_digest".to_owned(),
            Value::String(pipeline_digest.to_owned()),
        );
    }
    identity.insert("actor".to_owned(), Value::String("human".to_owned()));
    identity.insert(
        "human_session_id".to_owned(),
        Value::String(expected_session_id.to_owned()),
    );

    let Some(object) = report.as_object_mut() else {
        bail!("run report is not a JSON object");
    };
    object.insert("session".to_owned(), row);
    Ok(())
}

/// Computes the P4 pipeline digest for `commit`: sha256 over the four
/// hashed trees' git object ids, first sixteen hex characters — the exact
/// recipe `pipeline_id()` in `scripts/p4-ledger.sh` runs. Cross-checked
/// against the script by test.
///
/// # Errors
/// When git is unavailable, the commit is unknown, or a tree is missing.
pub fn compute_pipeline_digest(commit: &str, repo_root: &Path) -> Result<String> {
    let mut hashes = String::new();
    for tree in PIPELINE_TREES {
        let spec = format!("{commit}:{tree}");
        let output = Command::new("git")
            .args(["rev-parse", &spec])
            .current_dir(repo_root)
            .output()
            .context("running git rev-parse for the pipeline digest")?;
        if !output.status.success() {
            bail!(
                "no tree {tree} at {commit}: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        let hash = String::from_utf8(output.stdout)
            .context("git emitted non-utf8")?
            .trim()
            .to_owned();
        hashes.push_str(&format!("{tree}={hash}\n"));
    }
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(hashes.as_bytes());
    let hex: String = digest[..8]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    Ok(hex)
}

/// Where the repository root is found for digest computation: the checkout
/// this binary was built from (`p1-swarm/..`), overridable when assembling
/// elsewhere.
#[must_use]
pub fn repo_root() -> PathBuf {
    std::env::var_os("ORRERY_REPO_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(".."))
}

/// Waits for the participant's record file to appear and settles, then reads
/// the newest record from it.
///
/// The client writes its row *after* sending goodbye — the marker crosses
/// first — so the host that saw the goodbye typically waits only for the
/// write itself.
///
/// # Errors
/// On timeout (a hosted campaign hour without its participant row is not
/// evidence, and must fail loudly) or on an unreadable record.
pub fn await_record(path: &Path, timeout_secs: u64) -> Result<ParticipantRecord> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
    loop {
        match ParticipantRecord::read_latest(path)? {
            Some(record) => return Ok(record),
            None if std::time::Instant::now() >= deadline => {
                bail!(
                    "no session record appeared at {} within {timeout_secs}s; a hosted \
                     campaign hour without its participant row banks nothing",
                    path.display()
                )
            }
            None => std::thread::sleep(std::time::Duration::from_millis(250)),
        }
    }
}

/// Validates the invite-bound session id shape once, where the flag is
/// parsed, so a typo cannot silently disable join admission.
///
/// # Errors
/// When the value is not the ledger's UUIDv7 shape.
pub fn checked_session_id(value: &str) -> Result<()> {
    if exterior::is_uuid_v7(value) {
        Ok(())
    } else {
        bail!(
            "{value:?} is not a UUIDv7 session identity; mint one with \
             `orrery-invite` (see crates/orrery_identity/src/invite.rs)"
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A record shaped exactly like the client writes it — one line of JSON,
    /// which is what `read_latest` parses — parameterised so each test can
    /// override or break one field (later keys win, per serde).
    fn record_json(overrides: &str) -> String {
        let base = r#"{"session_id":"018f8f4e-5c90-7abc-8123-00000000abcd","wall_start":"2026-08-24T12:00:00Z","wall_end":"2026-08-24T13:00:00Z","distinct_play_minutes":60.0,"banked_minutes":60.0,"platform_triple":"x86_64-unknown-linux-gnu","client_rev":"test-rev","ruleset_id":"52","ruleset_version":2,"pipeline_digest":"unavailable-client-side","actor":"human","configured_impairment_profile":{"loss_pct":3.0,"jitter_p50_ms":100,"jitter_p99_ms":100},"observed_loss_pct":2.97,"observed_jitter_p50_ms":40,"observed_jitter_p99_ms":90,"afk_seconds":0,"afk_capped":false,"impairment_mismatch":true}"#;
        base.replace(
            "\"impairment_mismatch\":true}",
            &format!("\"impairment_mismatch\":true{overrides}}}"),
        )
    }

    fn good_record() -> ParticipantRecord {
        ParticipantRecord::parse(&record_json("")).expect("well-formed record")
    }

    fn run_report() -> Value {
        serde_json::json!({
            "identity": {
                "seed": 1,
                "impairment": { "loss": 0.03, "jitter_ticks": 6, "jitter_rate": 0.1 },
                "target": "x86_64-unknown-linux-gnu",
                "commit": "test-commit",
            },
            "peers": 3,
            "seconds": 120,
            "player_hours": 0.1,
            "witnessing": true,
            "total_false_positives": 0,
            "observation_coverage": 1.0,
            "deferral_ledger_balances": true,
        })
    }

    #[test]
    fn assembly_stamps_identity_attaches_the_row_and_fills_the_digest() {
        let mut report = run_report();
        let record = good_record();
        assemble(
            &mut report,
            &record,
            "018f8f4e-5c90-7abc-8123-00000000abcd",
            "x86_64-unknown-linux-gnu",
            "pipeline16chars",
        )
        .expect("assembly holds");
        assert_eq!(report["identity"]["actor"], "human");
        assert_eq!(
            report["identity"]["human_session_id"],
            "018f8f4e-5c90-7abc-8123-00000000abcd"
        );
        assert_eq!(report["session"]["pipeline_digest"], "pipeline16chars");
        // The rest of the row survives verbatim, unknown fields included.
        assert_eq!(report["session"]["afk_seconds"], 0);
        assert_eq!(report["session"]["observed_loss_pct"], 2.97);
    }

    #[test]
    fn a_row_from_another_session_or_actor_refuses() {
        let report = run_report();
        for (broken_fields, must_name) in [
            (
                r#", "session_id": "other""#,
                "does not name the invited session",
            ),
            (r#", "actor": "bot""#, "banks human rows"),
            (
                r#", "platform_triple": "aarch64-apple-darwin""#,
                "but this host ran",
            ),
        ] {
            let record = ParticipantRecord::parse(&record_json(broken_fields)).expect("parses");
            let error = assemble(
                &mut report.clone(),
                &record,
                "018f8f4e-5c90-7abc-8123-00000000abcd",
                "x86_64-unknown-linux-gnu",
                "pipeline16chars",
            )
            .expect_err("must refuse");
            assert!(
                error.to_string().contains(must_name),
                "{error} must name {must_name}"
            );
        }
    }

    /// The telemetry-honesty assertion, in both directions: the flag may
    /// disagree with configuration (that is its job), never with the row's
    /// own numbers.
    #[test]
    fn the_mismatch_flag_may_not_disagree_with_the_rows_own_numbers() {
        // Honest mismatch: measured ≠ configured, flag true — accepted above.
        assert!(assert_mismatch_consistency(&good_record()).is_ok());

        // A forged "verified impairment" row: numbers differ, flag denied.
        let forged = ParticipantRecord::parse(&record_json(r#", "impairment_mismatch": false"#))
            .expect("parses");
        let error = assert_mismatch_consistency(&forged).expect_err("forgery refused");
        assert!(
            error.to_string().contains("claims verified impairment"),
            "{error}"
        );

        // The opposite forgery: matching numbers flagged as mismatching.
        let false_alarm = ParticipantRecord::parse(
            &record_json(
                r#", "observed_loss_pct": 3.0, "observed_jitter_p50_ms": 100, "observed_jitter_p99_ms": 100"#,
            ),
        )
        .expect("parses");
        let error = assert_mismatch_consistency(&false_alarm).expect_err("false alarm refused");
        assert!(
            error.to_string().contains("mismatch its own numbers deny"),
            "{error}"
        );

        // And a self-consistent clean row (numbers equal, flag false) passes:
        // consistency is what is asserted here, not impairment.
        let consistent = ParticipantRecord::parse(&format!(
            "{}{}",
            r#"{"session_id":"s","actor":"human","platform_triple":"t","configured_impairment_profile":{"loss_pct":3.0,"jitter_p50_ms":10,"jitter_p99_ms":20},"observed_loss_pct":3.0,"observed_jitter_p50_ms":10,"observed_jitter_p99_ms":20,"impairment_mismatch":false,"wall_start":"w","wall_end":"e","distinct_play_minutes":1,"banked_minutes":1,"client_rev":"r","ruleset_id":"i","ruleset_version":1,"afk_seconds":0,"afk_capped":false}"#,
            ""
        ))
        .expect("parses");
        assert!(assert_mismatch_consistency(&consistent).is_ok());
    }

    /// The digest recipe agrees with `scripts/p4-ledger.sh pipeline-id`, run
    /// against this checkout at HEAD. This cross-check is why two copies of
    /// the recipe can exist: either side drifting fails here first.
    #[test]
    fn digest_matches_the_ledger_script_at_head() {
        let root = repo_root();
        let head = Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(&root)
            .output()
            .expect("git runs in this checkout");
        assert!(head.status.success(), "this test needs a git checkout");
        let commit = String::from_utf8(head.stdout)
            .expect("utf8")
            .trim()
            .to_owned();

        let script = Command::new("bash")
            .arg(root.join("scripts/p4-ledger.sh"))
            .arg("pipeline-id")
            .arg(&commit)
            .current_dir(&root)
            .output()
            .expect("the ledger script runs from the checkout");
        assert!(
            script.status.success(),
            "script failed: {}",
            String::from_utf8_lossy(&script.stderr)
        );
        let expected = String::from_utf8(script.stdout)
            .expect("utf8")
            .trim()
            .to_owned();
        assert!(!expected.is_empty(), "the script printed nothing");

        assert_eq!(
            compute_pipeline_digest(&commit, &root).expect("digest"),
            expected,
            "the Rust recipe and the ledger script disagree at {commit}"
        );
    }

    /// Reading back the newest line of a client-side JSONL stream.
    #[test]
    fn read_latest_takes_the_newest_record_and_names_garbage() {
        let dir = std::env::temp_dir().join(format!("oc387-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("records.jsonl");
        std::fs::write(
            &path,
            format!("{{\"garbage\": true}}\n{}\n", record_json("")),
        )
        .expect("write records");
        // A garbage line first does not stop the newest readable record…
        let latest = ParticipantRecord::read_latest(&path)
            .expect("reads")
            .expect("a record exists");
        assert_eq!(
            latest.session_id().expect("session id"),
            "018f8f4e-5c90-7abc-8123-00000000abcd"
        );
        // …but a file of only garbage refuses loudly rather than silently.
        std::fs::write(&path, "{\"garbage\": true}\n").expect("rewrite");
        assert!(
            ParticipantRecord::read_latest(&path).is_err(),
            "unreadable rows must refuse, not vanish"
        );
        // An absent file is None: the await loop distinguishes that from junk.
        assert!(ParticipantRecord::read_latest(&dir.join("absent.jsonl"))
            .expect("no error")
            .is_none());
        std::fs::remove_dir_all(&dir).expect("cleanup");
    }
}
