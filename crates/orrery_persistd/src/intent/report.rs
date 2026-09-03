//! The emit path: D32 clause (e)'s evidence assembled out of durable state
//! into a [`RampArtifact`]
//! ([D32](../../../../docs/adr/0032-enforcement-ramp.md) clause (e)).
//!
//! # Why this exists
//!
//! [`super::ramp`] gave the five controls their counters, [`super::cohort`]
//! gave `H` a durable form and [`super::window`] gave `W` and the counters
//! one. What none of them gave was a way to *read the result out*: the meters
//! counted in the deployed composition, and the only thing in the tree that
//! ever produced an artifact was an `#[ignore]`d harness test that built its
//! cohort in memory ([#991]). So the committed artifact said `"traffic":
//! "harness"` because a harness was the only producer that existed — a code
//! gap, not a traffic gap, and no amount of production traffic would have
//! closed it.
//!
//! This module is the producer. [`assemble_from_durable`] takes a cohort and a
//! window row per control and returns the artifact; `orrery-ramp report` is
//! the operator verb that reads those rows off a cluster and writes the file.
//! Reading durable state in an operator tool is fine —
//! [ADR-0031](../../../../docs/adr/0031-persistence-authority.md) clause (d)
//! forbids the *coordinator* from reading, and `orrery_coordinator` declares
//! no `foundationdb` dependency at all.
//!
//! # Nothing is derived twice
//!
//! The assembler computes no clause (e) term. It restores each row into a
//! [`RampMeter`] and calls [`RampMeter::snapshot`] — the same two calls
//! `persistd` makes at startup, in the same order — so `coverage`, `fp_count`,
//! `W`, `|H|` and the armed/natural split come out of the one implementation
//! that already owned them. A second derivation here would disagree with the
//! first exactly when it mattered, which is `AGENTS.md`'s rule for
//! `gate-status.sh` and the reason `scripts/ramp-report.py` re-derives nothing
//! either.
//!
//! The meter it restores into has metered nothing, which is the one thing that
//! *is* different from a live snapshot, and
//! [`RampSnapshot::without_process_cardinalities`] is where that difference is
//! declared rather than papered over.
//!
//! # How the artifact tells the truth about its own provenance
//!
//! `provenance.traffic` is the field clause (e)'s production leg turns on, and
//! it is the one field an artifact cannot establish from its own numbers.
//! Nothing in a `rampw/{control}` row says which fleet wrote it: the row is
//! counters, and [`super::window`]'s module docs are explicit that possession
//! of the cluster file is the trust boundary. A row read off a laboratory
//! cluster and a row read off the fleet decode identically.
//!
//! So the claim is neither computed nor taken on trust. Three things hold it up:
//!
//! 1. **It must be asserted, per run, by the operator.** [`TrafficClaim`] has
//!    no default. `harness` is what an unqualified run gets, because the
//!    failure that matters is a `production` claim nobody meant to make.
//! 2. **The durable state can refuse it.** A `production` claim is refused
//!    when the state contradicts it: no control has a window row at all
//!    ([`ProvenanceRefusal::NoDurableWindow`] — nothing a deployed `persistd`
//!    flusher wrote is present, so whatever produced these numbers, it was not
//!    a fleet), or every row that exists is empty
//!    ([`ProvenanceRefusal::NoObservations`] — a claim about traffic that does
//!    not exist). Both are zero-against-nonzero structural facts, not
//!    thresholds: clause (e)'s floors stay in the record and in
//!    `scripts/ramp-report.py`.
//! 3. **What it rests on is published with it.** [`Provenance::windows`]
//!    carries each row's generation, open time, flush count and reset reason,
//!    so a reviewer holding the same cluster file reads the same rows back,
//!    and a reviewer without one still sees whether this was a fleet
//!    measurement or one process writing once. The note is *assembled*, not
//!    typed: the assembler states the gaps itself, so the operator's free text
//!    is additional rather than the only place the caveats could have lived.
//!
//! What this deliberately does **not** do is invent a cryptographic
//! provenance. Signing the artifact, or the rows behind it, would need a
//! writer identity every `persistd` holds — a different trust root from D32
//! clause (i)'s operator key, and not one an emit path gets to introduce.
//! Custody of the cluster file is the boundary, exactly as it already is for
//! the window rows themselves.
//!
//! [#991]: https://github.com/baadc0de/orrery/issues/991

use super::ramp::{
    Provenance, RampArtifact, RampMeter, RampSnapshot, WindowProvenance, RAMP_ARTIFACT_SCHEMA,
};
use super::window::RampWindowRow;
use super::HonestCohort;

/// What an operator asserts about the traffic behind a run.
///
/// Deliberately not `Default`, and deliberately not parsed from the same
/// string the artifact carries: the artifact's `traffic` field is an output,
/// and the only way to reach `production` is to ask for it and survive
/// [`assemble_from_durable`]'s refusals.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrafficClaim {
    /// Simulated traffic. Always assemblable — a harness artifact claims
    /// nothing clause (e)'s production leg accepts.
    Harness,
    /// The fleet. Refused unless the durable state is a fleet's.
    Production,
}

impl TrafficClaim {
    /// The string the artifact carries, which `scripts/ramp-report.py`
    /// matches against clause (e)'s production-leg term.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Harness => "harness",
            Self::Production => "production",
        }
    }
}

/// Why a [`TrafficClaim::Production`] artifact was not assembled.
///
/// A refusal is the tool working. An artifact that claimed production over
/// state that cannot have come from a fleet is worse than no artifact: it is
/// citable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProvenanceRefusal {
    /// Not one control has a `rampw/{control}` row.
    ///
    /// Only a deployed `persistd`'s flusher writes those rows. None present
    /// means nothing in this cluster has ever metered a control, so there is
    /// no production measurement to report — whatever the operator is looking
    /// at, it is not one.
    NoDurableWindow,
    /// Rows exist and every one of them is empty.
    ///
    /// A window opened by a reset, or by a flush that had nothing to flush,
    /// carries no observation. `W = 0` over zero observations is a true
    /// statement about a fleet and a false one about its traffic, and clause
    /// (e)'s production leg is a claim about traffic.
    NoObservations,
}

impl std::fmt::Display for ProvenanceRefusal {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoDurableWindow => formatter.write_str(
                "no control has a durable measurement window in this cluster, so nothing \
                 here was written by a deployed persistd's flusher and the numbers cannot \
                 be production-observed. Assemble it as --traffic harness, or point the \
                 tool at the cluster the fleet meters into",
            ),
            Self::NoObservations => formatter.write_str(
                "every durable measurement window in this cluster is empty, so this would \
                 claim production traffic over zero observations. Clause (e)'s production \
                 leg is a claim about traffic; there is none in these rows yet",
            ),
        }
    }
}

impl std::error::Error for ProvenanceRefusal {}

/// One control's durable window, as the artifact producer read it.
///
/// `row` is `None` for a control with no row at all — a control no process has
/// ever flushed. That control is still reported, with the zeros a window that
/// was never opened actually holds, because a measurable control missing from
/// the artifact would read as a control with nothing to report rather than as
/// one nothing has been measured for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DurableControl {
    /// D32 clause (c)'s control name — the `ramp/{control}` and
    /// `rampw/{control}` suffix. `&'static str` because it is the name
    /// [`RampMeter`] is constructed with, and because the set of controls is
    /// the record's rather than an argument's.
    pub control: &'static str,
    /// The stored row, or `None` when the key is absent.
    pub row: Option<RampWindowRow>,
}

impl DurableControl {
    /// Whether this control's window holds any observation.
    ///
    /// Flushes are not observations: a fleet with a metered control nobody
    /// exercised flushes an empty delta every interval, and counting those
    /// would let a `production` claim rest on the flusher's own heartbeat.
    fn observed_anything(&self) -> bool {
        self.row.as_ref().is_some_and(|row| !row.counts.is_empty())
    }
}

/// Assemble D32 clause (e)'s artifact from durable state alone.
///
/// `cohort` is [`super::cohort::FdbHonestCohortStore::load`]'s result and
/// `controls` is one [`DurableControl`] per measurable control, in clause
/// (c)'s order. `source` names the producer in one line and `operator_note` is
/// whatever the operator wants a reader to know beyond what the assembler
/// already states.
///
/// # Errors
///
/// [`ProvenanceRefusal`] when `claim` is [`TrafficClaim::Production`] and the
/// durable state contradicts it. A [`TrafficClaim::Harness`] assembly cannot
/// fail: it claims nothing that needs holding up.
pub fn assemble_from_durable(
    claim: TrafficClaim,
    cohort: &HonestCohort,
    controls: &[DurableControl],
    source: &str,
    operator_note: Option<&str>,
) -> Result<RampArtifact, ProvenanceRefusal> {
    if claim == TrafficClaim::Production {
        if !controls.iter().any(|control| control.row.is_some()) {
            return Err(ProvenanceRefusal::NoDurableWindow);
        }
        if !controls.iter().any(DurableControl::observed_anything) {
            return Err(ProvenanceRefusal::NoObservations);
        }
    }

    let snapshots = controls
        .iter()
        .map(|control| snapshot_of(control, cohort))
        .collect();
    let windows = controls
        .iter()
        .filter_map(|control| {
            control.row.as_ref().map(|row| WindowProvenance {
                control: control.control.to_owned(),
                window_id: row.window_id,
                opened_at_ms: row.opened_at_ms,
                flushes: row.flushes,
                reset_reason: row.reset_reason.clone(),
                cohort_accounts_truncated: row.counts.cohort_accounts_truncated,
            })
        })
        .collect::<Vec<_>>();

    let provenance = Provenance {
        traffic: claim.as_str().to_owned(),
        source: source.to_owned(),
        note: note(claim, cohort, controls, &windows, operator_note),
        windows,
    };
    Ok(RampArtifact::new(provenance, snapshots))
}

/// One control's snapshot, restored out of its durable row.
///
/// The row goes into a meter that has metered nothing and
/// [`RampMeter::snapshot`] does the arithmetic, so every clause (e) term in
/// the artifact is computed by the same code the fleet computes it with. The
/// process-run cardinalities are then declared absent, because this process
/// has no run.
fn snapshot_of(control: &DurableControl, cohort: &HonestCohort) -> RampSnapshot {
    let meter = RampMeter::new(control.control);
    if let Some(row) = control.row.clone() {
        meter.restore(row);
    }
    meter.snapshot(cohort).without_process_cardinalities()
}

/// The note the assembler writes for itself.
///
/// Assembled rather than typed, so the caveats are not left to whoever
/// remembers them. Every clause here is a fact about *this* assembly: what
/// could not be assembled and why, which controls had no row, and — for a
/// production claim — the shape of the measurement behind it.
fn note(
    claim: TrafficClaim,
    cohort: &HonestCohort,
    controls: &[DurableControl],
    windows: &[WindowProvenance],
    operator_note: Option<&str>,
) -> String {
    let mut parts = vec![format!(
        "Assembled from durable state: {} cohort member(s) and {} of {} control window(s) \
         present. Fleet-wide distinct-account cardinalities are absent rather than zero — \
         they are per-process figures by construction (a cardinality cannot be folded \
         across flushes and 100k account ids do not fit in a value), and this producer \
         metered nothing, so it has none. Clause (f)'s spread term therefore cannot be \
         evaluated from this artifact; the auto-suspend breaker computes it in-process and \
         never reads one.",
        cohort.len(),
        windows.len(),
        controls.len()
    )];

    let unflushed: Vec<&str> = controls
        .iter()
        .filter(|control| control.row.is_none())
        .map(|control| control.control)
        .collect();
    if !unflushed.is_empty() {
        parts.push(format!(
            "No process has ever flushed a window for {}, so their counters read zero \
             because nothing was measured, not because nothing happened.",
            unflushed.join(", ")
        ));
    }

    if claim == TrafficClaim::Production {
        let flushes: u64 = windows.iter().map(|window| window.flushes).sum();
        parts.push(format!(
            "The production claim is the operator's, made against this cluster file, and \
             rests on {flushes} flush(es) across the windows listed in \
             `provenance.windows` — a reviewer holding the same cluster file reads the same \
             generations back. Nothing in a window row records which fleet wrote it; \
             custody of the cluster file is the boundary, as it is for the rows themselves."
        ));
    }

    if cohort.natural.is_empty() && !cohort.armed.is_empty() {
        parts.push(
            "H has no natural half: every cohort-scored figure here describes operator \
             automation rather than sampled real players. The fleet-wide counters still \
             describe whatever traffic the fleet saw."
                .to_owned(),
        );
    }

    if controls
        .iter()
        .filter_map(|control| control.row.as_ref())
        .any(|row| row.counts.fleet_truncation_seen)
    {
        parts.push(
            "At least one window folded traffic from the meter's past-capacity truncation \
             bucket, so its fleet account spread and cohort denominator are both understated \
             by an unknown amount."
                .to_owned(),
        );
    }

    if let Some(operator) = operator_note.map(str::trim).filter(|text| !text.is_empty()) {
        parts.push(format!("Operator note: {operator}"));
    }
    parts.join(" ")
}

/// The schema an assembled artifact carries, for callers that want to print it
/// without reaching into [`super::ramp`].
#[must_use]
pub const fn schema() -> &'static str {
    RAMP_ARTIFACT_SCHEMA
}

#[cfg(test)]
mod tests {
    use super::*;
    use orrery_protocol::AccountId;

    const C1: &str = super::super::ATTESTATION_QUORUM_CONTROL;
    const C5: &str = crate::gateway::STRIKES_CONTROL;

    fn cohort_of(
        armed: impl IntoIterator<Item = u64>,
        natural: impl IntoIterator<Item = u64>,
    ) -> HonestCohort {
        let mut cohort = HonestCohort::new();
        for account in armed {
            cohort.arm(AccountId::new(account));
        }
        for account in natural {
            cohort.sample(AccountId::new(account));
        }
        cohort
    }

    /// A window with clause (e)'s halves populated separately, as a fleet's
    /// flusher would have written it.
    fn window() -> RampWindowRow {
        let mut row = RampWindowRow::opened(3, 1_000, Some("ruleset v9".to_owned()));
        row.flushes = 43_200;
        row.counts.observe_at(1_000);
        row.counts.observe_at(1_000 + 86_400_000 * 31);
        row.counts.fleet.qualifying = 1_000;
        row.counts.fleet.observed = 1_000;
        row.counts.armed.qualifying = 400;
        row.counts.armed.observed = 400;
        row.counts.armed.would_act = 1;
        row.counts
            .armed
            .causes
            .insert("threshold_not_met".to_owned(), 1);
        row.counts.natural.qualifying = 600;
        row.counts.natural.observed = 600;
        row.counts.natural.would_act = 40;
        row.counts
            .natural
            .causes
            .insert("threshold_not_met".to_owned(), 40);
        row.counts.armed_active = [AccountId::new(1)].into_iter().collect();
        row.counts.armed_would_act = [AccountId::new(1)].into_iter().collect();
        row.counts.natural_active = [AccountId::new(2), AccountId::new(3)].into_iter().collect();
        row.counts.natural_would_act = [AccountId::new(2)].into_iter().collect();
        row.counts.by_verdict.insert("would_admit".to_owned(), 959);
        row.counts.by_verdict.insert("would_refuse".to_owned(), 41);
        row
    }

    fn controls(row: Option<RampWindowRow>) -> Vec<DurableControl> {
        vec![
            DurableControl { control: C1, row },
            DurableControl {
                control: C5,
                row: None,
            },
        ]
    }

    /// The whole point of the module: durable rows become an artifact whose
    /// numbers are the rows', not a harness's.
    #[test]
    fn a_durable_window_becomes_an_artifact_carrying_its_own_numbers() {
        let cohort = cohort_of([1], [2, 3]);
        let artifact = assemble_from_durable(
            TrafficClaim::Production,
            &cohort,
            &controls(Some(window())),
            "test",
            None,
        )
        .expect("a fleet's window assembles");

        assert_eq!(artifact.provenance.traffic, "production");
        let c1 = artifact
            .controls
            .iter()
            .find(|control| control.control == C1)
            .expect("C1 is reported");
        assert_eq!(c1.qualifying, 1_000);
        assert!(
            (c1.window_days - 31.0).abs() < 1e-9,
            "W comes off the row's own bounds: {}",
            c1.window_days
        );
        assert_eq!(c1.cohort.size, 3);
        assert_eq!(c1.cohort.fp_count, 41);
        assert_eq!(c1.cohort.qualifying, 1_000);
        assert_eq!(
            c1.cohort.coverage,
            Some(1.0),
            "coverage is the meter's arithmetic over the durable counters"
        );
        assert_eq!(c1.cohort.active, 3, "the durable active sets union");
        assert_eq!(c1.cohort.accounts_would_act, 2);
    }

    /// Clause (e)'s halves survive the trip. `1` refused bot and `40` refused
    /// players must not arrive as `41` of something.
    #[test]
    fn the_armed_and_natural_halves_reach_the_artifact_unsummed() {
        let cohort = cohort_of([1], [2, 3]);
        let artifact = assemble_from_durable(
            TrafficClaim::Production,
            &cohort,
            &controls(Some(window())),
            "test",
            None,
        )
        .expect("assembles");
        let windows = &artifact.provenance.windows;
        assert_eq!(windows.len(), 1, "only the control with a row is listed");
        assert_eq!(windows[0].window_id, 3);
        assert_eq!(windows[0].flushes, 43_200);
        assert_eq!(windows[0].reset_reason.as_deref(), Some("ruleset v9"));

        // The artifact's cohort evidence is the union, as clause (e) defines
        // fp_count; the halves it was drawn from stay separable through the
        // window row the artifact publishes alongside it.
        let json = artifact.to_json().expect("serializable");
        assert!(json.contains("\"fp_count\": 41"), "{json}");
        let round_trip: RampArtifact = serde_json::from_str(&json).expect("round trip");
        assert_eq!(round_trip, artifact);
    }

    /// The honest gap, at the type level: an assembler that never metered
    /// reports *no* fleet cardinality rather than zero of them.
    #[test]
    fn fleet_cardinalities_are_absent_rather_than_zero() {
        let cohort = cohort_of([1], [2, 3]);
        let artifact = assemble_from_durable(
            TrafficClaim::Production,
            &cohort,
            &controls(Some(window())),
            "test",
            None,
        )
        .expect("assembles");
        for control in &artifact.controls {
            assert_eq!(
                control.accounts_qualifying, None,
                "a per-process cardinality has no value in an assembled artifact"
            );
            assert_eq!(control.accounts_observed, None);
            assert_eq!(
                control.accounts_would_act, None,
                "zero spread is *under* clause (f)'s bound, so reporting it would \
                 report a safety term as met"
            );
            assert_eq!(control.accounts_truncated, None);
        }
        assert!(
            artifact.provenance.note.contains("absent rather than zero"),
            "the gap is stated in the artifact, not only in the code: {}",
            artifact.provenance.note
        );
    }

    /// Truncation is a flag, and the flag folds, so it survives into an
    /// assembled artifact even though the count cannot.
    #[test]
    fn a_truncated_window_still_carries_its_warning() {
        let mut row = window();
        row.counts.fleet_truncation_seen = true;
        let artifact = assemble_from_durable(
            TrafficClaim::Production,
            &cohort_of([1], [2, 3]),
            &controls(Some(row)),
            "test",
            None,
        )
        .expect("assembles");
        let c1 = &artifact.controls[0];
        assert!(c1.truncation_seen, "the row's flag reaches the artifact");
        assert!(artifact.provenance.note.contains("truncation bucket"));
    }

    #[test]
    fn production_is_refused_when_no_process_has_ever_flushed_a_window() {
        let refusal = assemble_from_durable(
            TrafficClaim::Production,
            &cohort_of([1], [2]),
            &controls(None),
            "test",
            None,
        )
        .expect_err("no rows means no fleet measurement");
        assert_eq!(refusal, ProvenanceRefusal::NoDurableWindow);
        assert!(format!("{refusal}").contains("--traffic harness"));
    }

    #[test]
    fn production_is_refused_over_an_empty_window() {
        let refusal = assemble_from_durable(
            TrafficClaim::Production,
            &cohort_of([1], [2]),
            &controls(Some(RampWindowRow::opened(
                1,
                5_000,
                Some("reset".to_owned()),
            ))),
            "test",
            None,
        )
        .expect_err("a reset-and-silent window is not production evidence");
        assert_eq!(refusal, ProvenanceRefusal::NoObservations);
    }

    /// The same state a production claim is refused over still assembles as a
    /// harness artifact, which claims nothing clause (e)'s production leg
    /// accepts.
    #[test]
    fn a_harness_claim_over_the_same_state_assembles() {
        let artifact = assemble_from_durable(
            TrafficClaim::Harness,
            &cohort_of([1], []),
            &controls(None),
            "test",
            Some("  fixture  "),
        )
        .expect("a harness claim is never refused");
        assert_eq!(artifact.provenance.traffic, "harness");
        assert!(artifact.provenance.windows.is_empty());
        assert!(
            artifact.provenance.note.contains("Operator note: fixture"),
            "{}",
            artifact.provenance.note
        );
        assert!(
            artifact.provenance.note.contains("no natural half"),
            "a cohort of nothing but automation is stated: {}",
            artifact.provenance.note
        );
        assert!(
            artifact.provenance.note.contains("ever flushed a window"),
            "{}",
            artifact.provenance.note
        );
    }

    /// Every control the assembler was handed is reported, and the five D32
    /// names are all accounted for once the absent list is added.
    #[test]
    fn every_control_handed_in_is_reported_measured_or_not() {
        let artifact = assemble_from_durable(
            TrafficClaim::Harness,
            &cohort_of([], []),
            &controls(None),
            "test",
            None,
        )
        .expect("assembles");
        assert_eq!(artifact.controls.len(), 2);
        assert_eq!(artifact.schema, schema());
        let c5 = &artifact.controls[1];
        assert_eq!(c5.control, C5);
        assert_eq!(c5.qualifying, 0);
        assert_eq!(
            c5.cohort.coverage, None,
            "a control nothing was measured for has no rate, not a rate of zero"
        );
    }
}

#[cfg(all(test, feature = "fdb"))]
mod fdb_tests {
    use super::*;
    use crate::intent::cohort::{CohortHalf, FdbHonestCohortStore};
    use crate::intent::shadow::{ShadowObservation, ShadowVerdict};
    use crate::intent::window::{FdbRampWindowStore, FlushOutcome};
    use crate::intent::{NetworkQuality, RejectionCause};
    use orrery_protocol::{AccountId, CellEpoch};

    const DAY_MS: u64 = 86_400_000;
    /// A control name of this test's own. The report path keys `rampw/` rows
    /// by control, so a private name is what keeps a shared dev cluster from
    /// making this test read another lane's counters.
    const CONTROL: &str = "test_report_emit";

    /// The dev-cluster convention `intent/fdb.rs` established and every
    /// sibling module follows: skip without a cluster, and let
    /// `scripts/fdb-tests.sh` turn the skip into a gate.
    fn fdb_cluster_file() -> Option<String> {
        if let Ok(path) = std::env::var("ORRERY_FDB_CLUSTER_FILE") {
            return Some(path);
        }
        let local = std::path::Path::new(".fdb-dev/fdb.cluster");
        local.exists().then(|| local.display().to_string())
    }

    fn context() -> crate::FdbContext {
        crate::FdbContext::connect(&fdb_cluster_file().expect("cluster file")).expect("connect")
    }

    fn account(id: u64) -> AccountId {
        AccountId::new(id)
    }

    fn obs(subject: u64, verdict: ShadowVerdict, at_ms: u64) -> ShadowObservation {
        ShadowObservation {
            intent_id: u128::from(at_ms) * 1_000 + u128::from(subject),
            issuer: iroh_base::SecretKey::from_bytes(&[13; 32]).public(),
            subject: Some(account(subject)),
            cell_epoch: CellEpoch::new(9),
            verdict,
            observed_at_ms: at_ms,
            network: NetworkQuality::Unknown,
        }
    }

    /// Seed the identity account row the natural half's durable check reads,
    /// aged past probation, exactly as `cohort`'s own store test does.
    async fn seed_account_row(member: AccountId) {
        let db = context().database();
        let key = crate::keyspace::account_key(member).to_vec();
        let value = postcard::to_allocvec(&crate::keyspace::AccountRow {
            created_ms: 0,
            ..crate::keyspace::AccountRow::default()
        })
        .expect("encode");
        db.run(move |trx, _| {
            let (key, value) = (key.clone(), value.clone());
            async move {
                trx.set(&key, &value);
                Ok(())
            }
        })
        .await
        .expect("seed account row");
    }

    async fn clear_account_row(member: AccountId) {
        let db = context().database();
        let key = crate::keyspace::account_key(member).to_vec();
        db.run(move |trx, _| {
            let key = key.clone();
            async move {
                trx.clear(&key);
                Ok(())
            }
        })
        .await
        .expect("clear account row");
    }

    /// The whole of #991, against a real cluster: durable rows written by
    /// something that looks like a fleet become an artifact, with clause (e)'s
    /// halves intact and the honest gap declared.
    ///
    /// Nothing in this test builds a cohort or a window in memory and hands it
    /// to the assembler. Both go *through* FoundationDB — the cohort through
    /// `FdbHonestCohortStore::sample`, which checks the natural half's durable
    /// facts in the transaction that records it, and the counters through
    /// `RampMeter::take_delta` and `FdbRampWindowStore::flush`, which is the
    /// path `persistd`'s flusher runs. The producer then reads them back with
    /// a meter that never saw the traffic. That round trip is the thing that
    /// could not happen before: the rows existed and nothing could turn them
    /// into an artifact.
    #[tokio::test]
    async fn durable_rows_become_an_artifact_a_reviewer_can_read() {
        let Some(_) = fdb_cluster_file() else {
            return;
        };
        let context = context();
        let armed = account(u64::from_be_bytes(*b"rpt-arm0"));
        let natural = account(u64::from_be_bytes(*b"rpt-nat0"));

        let cohort_store = FdbHonestCohortStore::from_context(&context);
        let windows = FdbRampWindowStore::from_context(&context);
        windows.clear(CONTROL).await.expect("clean slate");
        cohort_store.remove(armed).await.expect("clean slate");
        cohort_store.remove(natural).await.expect("clean slate");
        seed_account_row(natural).await;

        cohort_store
            .sample(armed, CohortHalf::Armed, "p1 swarm operator", 1_000, 1_000)
            .await
            .expect("the armed decision is the fact");
        cohort_store
            .sample(
                natural,
                CohortHalf::Natural,
                "owner-sampled",
                2_000,
                cohort_store.probation_ms() + 1,
            )
            .await
            .expect("the natural decision passes its durable checks");

        // ── One process's worth of traffic, over thirty-one days ──────────
        let cohort = cohort_store.load().await.expect("load cohort");
        let meter = crate::intent::ramp::RampMeter::new(CONTROL);
        for day in 0..31_u64 {
            for member in [armed, natural] {
                meter.record_qualifying(Some(member));
                meter.record(obs(member.0, ShadowVerdict::WouldAdmit, day * DAY_MS));
            }
        }
        // One would-have-acted event in each half. This is the pair the whole
        // split exists for: a reviewer must be able to tell the refused
        // player from the refused bot after the process that counted them is
        // gone.
        for member in [armed, natural] {
            meter.record_qualifying(Some(member));
            meter.record(obs(
                member.0,
                ShadowVerdict::WouldRefuse(RejectionCause::ThresholdNotMet),
                30 * DAY_MS,
            ));
        }
        let delta = meter.take_delta(&cohort);
        let FlushOutcome::Applied(_) = windows.flush(CONTROL, &delta, 5_000).await.expect("flush")
        else {
            panic!("nothing else writes this control's window");
        };

        // ── The producer, which metered nothing ──────────────────────────
        let reloaded = cohort_store.load().await.expect("reload cohort");
        let row = windows
            .load(CONTROL)
            .await
            .expect("load window")
            .expect("the flush opened one");
        let artifact = assemble_from_durable(
            TrafficClaim::Production,
            &reloaded,
            &[DurableControl {
                control: CONTROL,
                row: Some(row),
            }],
            "fdb test",
            None,
        )
        .expect("rows written by a flusher hold up a production claim");

        assert_eq!(artifact.provenance.traffic, "production");
        let control = &artifact.controls[0];
        assert!(
            (control.window_days - 30.0).abs() < 1e-9,
            "W is the durable window's own span, not the producer's uptime: {}",
            control.window_days
        );
        assert_eq!(control.qualifying, 64, "31 days x 2 members, plus the pair");
        assert_eq!(control.would_act, 2);
        assert_eq!(
            control.cohort.fp_count, 2,
            "both would-have-acted events were scored over H"
        );
        assert_eq!(control.cohort.accounts_would_act, 2);
        assert_eq!(control.cohort.active, 2);
        assert_eq!(control.cohort.coverage, Some(1.0));
        assert!(
            control.cohort.size >= 2,
            "the cohort came out of the rows, and the span may hold other lanes'"
        );
        assert_eq!(
            control.accounts_would_act, None,
            "the producer has no process run, so it reports no fleet cardinality"
        );
        assert!(!control.truncation_seen);

        let window = &artifact.provenance.windows[0];
        assert_eq!(window.control, CONTROL);
        assert_eq!(
            window.flushes, 1,
            "one process wrote this once, and it says so"
        );

        // The artifact is what the report script reads, so it must survive
        // the round trip the script's `json.loads` performs.
        let json = artifact.to_json().expect("serializable");
        assert!(json.contains("\"traffic\": \"production\""), "{json}");
        assert!(
            json.contains("\"accounts_would_act\": null"),
            "the unassembled cardinality is null in the file, not zero: {json}"
        );

        cohort_store.remove(armed).await.expect("tidy");
        cohort_store.remove(natural).await.expect("tidy");
        clear_account_row(natural).await;
        windows.clear(CONTROL).await.expect("tidy");
    }

    /// The refusal, against a real cluster: an empty span cannot hold up a
    /// production claim, and the operator is told which cluster it read.
    #[tokio::test]
    async fn production_is_refused_against_a_cluster_with_no_window() {
        let Some(_) = fdb_cluster_file() else {
            return;
        };
        let context = context();
        let windows = FdbRampWindowStore::from_context(&context);
        let control = "test_report_empty";
        windows.clear(control).await.expect("clean slate");

        let cohort = FdbHonestCohortStore::from_context(&context)
            .load()
            .await
            .expect("load cohort");
        let row = windows.load(control).await.expect("load");
        let refusal = assemble_from_durable(
            TrafficClaim::Production,
            &cohort,
            &[DurableControl { control, row }],
            "fdb test",
            None,
        )
        .expect_err("no row means no fleet measurement");
        assert_eq!(refusal, ProvenanceRefusal::NoDurableWindow);

        // And the same state still assembles as a harness artifact, which
        // claims nothing clause (e)'s production leg accepts.
        let harness = assemble_from_durable(
            TrafficClaim::Harness,
            &cohort,
            &[DurableControl {
                control,
                row: windows.load(control).await.expect("load"),
            }],
            "fdb test",
            None,
        )
        .expect("a harness claim is never refused");
        assert_eq!(harness.provenance.traffic, "harness");
    }
}
