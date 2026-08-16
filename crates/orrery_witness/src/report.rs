//! Signing and checking a [`DiscrepancyReport`] (docs/07 §3).
//!
//! The bundle inside a report is self-verifying, so this signature does not
//! make the *evidence* trustworthy — nothing can, and nothing needs to. What it
//! does is bind the accusation to an account, which is what makes
//! `EvidenceForged` a strike anyone can act on and what lets unadjudicable
//! submissions be rate-limited per reporter rather than per connection.

use orrery_protocol::{DiscrepancyReport, EvidenceBundle, NodeId, Signature};

/// Domain separator, so a report signature can never be replayed as a log
/// frame or state claim made with the same key.
const REPORT_DOMAIN: &[u8] = b"orrery/discrepancy-report/v1";

/// Why a report was not accepted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReportError {
    /// The reporter's signature does not verify under the reporter's key.
    ReporterSignatureInvalid,
}

impl core::fmt::Display for ReportError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::ReporterSignatureInvalid => f.write_str("reporter signature does not verify"),
        }
    }
}

impl core::error::Error for ReportError {}

/// The bytes a reporter signs.
///
/// Covers the accused, the reporter, and a hash of the bundle rather than the
/// bundle itself — a bundle is up to 180 ticks of frames, and re-serializing it
/// to verify one signature would make report checking cost proportional to the
/// evidence rather than constant.
///
/// # Panics
///
/// If the bundle does not serialize, which cannot happen for a type built
/// entirely from `Serialize` fields. Swallowing the error into an empty vector
/// would be far worse than a panic: every report would then digest to the hash
/// of the empty string, so any two reports would carry interchangeable
/// signatures and the binding this function exists to create would silently not
/// exist.
#[must_use]
fn preimage(subject: NodeId, reporter: NodeId, bundle: &EvidenceBundle) -> Vec<u8> {
    let encoded = postcard::to_stdvec(bundle).expect("an evidence bundle is serializable");
    let mut hasher = blake3::Hasher::new();
    hasher.update(&encoded);
    let digest = hasher.finalize();

    let mut out = Vec::with_capacity(REPORT_DOMAIN.len() + 96);
    out.extend_from_slice(REPORT_DOMAIN);
    out.extend_from_slice(subject.as_bytes());
    out.extend_from_slice(reporter.as_bytes());
    out.extend_from_slice(digest.as_bytes());
    out
}

/// File a report: bind the bundle to the accused and to this reporter.
#[must_use]
pub fn sign_report(
    key: &iroh_base::SecretKey,
    subject: NodeId,
    bundle: EvidenceBundle,
) -> DiscrepancyReport {
    let reporter = key.public();
    let sig: Signature = key.sign(&preimage(subject, reporter, &bundle));
    DiscrepancyReport {
        subject,
        bundle,
        reporter,
        reporter_sig: sig,
    }
}

/// Check that a report was signed by the reporter it names.
///
/// This is *not* a judgement on the evidence — that is `verify_bundle`'s job,
/// and it is deliberately a separate step so a malformed accusation and a
/// well-formed false one are distinguishable.
///
/// # Errors
///
/// [`ReportError::ReporterSignatureInvalid`].
pub fn verify_report(report: &DiscrepancyReport) -> Result<(), ReportError> {
    report
        .reporter
        .verify(
            &preimage(report.subject, report.reporter, &report.bundle),
            &report.reporter_sig,
        )
        .map_err(|_| ReportError::ReporterSignatureInvalid)
}

#[cfg(test)]
mod tests {
    use super::*;
    use orrery_protocol::{ChainHash, PersistId, RulesetId, StateClaim, Tick};

    fn key(seed: u8) -> iroh_base::SecretKey {
        iroh_base::SecretKey::from_bytes(&[seed; 32])
    }

    fn bundle() -> EvidenceBundle {
        let signer = key(9);
        EvidenceBundle {
            ruleset: RulesetId {
                version: 1,
                digest: [1; 32],
            },
            entity: PersistId::new(5),
            window_start: Tick::new(10),
            window_end: Tick::new(20),
            t0_claim: StateClaim {
                entity: PersistId::new(5),
                chain_epoch: 0,
                tick: Tick::new(10),
                input_head: ChainHash::EMPTY,
                state_hash: [2; 32],
                prev_claim: [0; 32],
                ruleset: RulesetId {
                    version: 1,
                    digest: [1; 32],
                },
                sig: signer.sign(b"claim"),
            },
            t0_snapshot: bytes::Bytes::from_static(b"state"),
            frames: Vec::new(),
            sibling_heads: Vec::new(),
            disputed_claims: Vec::new(),
            claimed_hashes: vec![[3; 32]; 10],
            computed_hashes: vec![[4; 32]; 10],
        }
    }

    #[test]
    fn a_signed_report_verifies() {
        let reporter = key(1);
        let report = sign_report(&reporter, key(2).public(), bundle());
        assert_eq!(verify_report(&report), Ok(()));
        assert_eq!(report.reporter, reporter.public());
    }

    #[test]
    fn editing_the_bundle_after_signing_breaks_the_report() {
        // The signature binds the accusation to an account. A reporter that
        // could swap the evidence afterwards could accuse anyone of anything
        // and disclaim the parts that did not hold up.
        let mut report = sign_report(&key(1), key(2).public(), bundle());
        report.bundle.claimed_hashes[0] = [0xFF; 32];
        assert_eq!(
            verify_report(&report),
            Err(ReportError::ReporterSignatureInvalid)
        );
    }

    #[test]
    fn reattributing_a_report_to_a_different_subject_breaks_it() {
        // Who is accused is inside the signature, so a genuine report about A
        // cannot be re-aimed at B.
        let mut report = sign_report(&key(1), key(2).public(), bundle());
        report.subject = key(3).public();
        assert_eq!(
            verify_report(&report),
            Err(ReportError::ReporterSignatureInvalid)
        );
    }

    #[test]
    fn claiming_someone_elses_report_breaks_it() {
        let mut report = sign_report(&key(1), key(2).public(), bundle());
        report.reporter = key(4).public();
        assert_eq!(
            verify_report(&report),
            Err(ReportError::ReporterSignatureInvalid)
        );
    }
}
