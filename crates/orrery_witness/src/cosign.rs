//! Peer-side decisions for D27 intent co-signing.
//!
//! This module stays Bevy-free for the same reason as the detection engine: a
//! decision must not depend on which adapter moved its bytes. The adapter may
//! supply a game-specific plausibility result from its replicated view; this
//! layer owns the checks that are universal — signing identity, party
//! exclusion, issuer authentication, and D27's exact attestation signature.

use orrery_protocol::{
    AttestationRefusalReason, AttestationVerdict, IntentProposal, IntentResponse,
};

/// Decide one proposal and produce the explicit wire answer.
///
/// `plausibility` is the result of the game-specific checks against the
/// witness's replicated view. The generic witness crate cannot decode
/// `IntentOp::args`, so pretending it evaluated those rules would be weaker
/// than exposing the seam. A `u16` failure is returned as
/// [`AttestationRefusalReason::PlausibilityFailed`].
///
/// Party exclusion is evaluated before every other check and does not depend
/// on `plausibility`: an honest witness never signs an intent naming its own
/// NodeId, even if a caller accidentally reports that the game checks passed.
#[must_use]
pub fn decide_proposal(
    proposal: &IntentProposal,
    identity: Option<&iroh_base::SecretKey>,
    plausibility: Result<(), u16>,
) -> IntentResponse {
    let intent_id = proposal.intent.intent_id;
    let Some(identity) = identity else {
        return refusal(intent_id, AttestationRefusalReason::MissingSigningIdentity);
    };
    let witness = identity.public();

    if witness == proposal.intent.issuer || proposal.parties.contains(&witness) {
        return refusal(intent_id, AttestationRefusalReason::Party);
    }
    if !proposal.intent.verify_issuer() {
        return refusal(intent_id, AttestationRefusalReason::BadIssuerSignature);
    }
    if let Err(reason) = plausibility {
        return refusal(
            intent_id,
            AttestationRefusalReason::PlausibilityFailed(reason),
        );
    }

    IntentResponse {
        intent_id,
        verdict: AttestationVerdict::Attested(proposal.intent.attest(identity)),
    }
}

fn refusal(intent_id: u128, reason: AttestationRefusalReason) -> IntentResponse {
    IntentResponse {
        intent_id,
        verdict: AttestationVerdict::Refused(reason),
    }
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;
    use orrery_protocol::{CellEpoch, Intent, IntentOp};

    use super::*;

    fn key(seed: u8) -> iroh_base::SecretKey {
        iroh_base::SecretKey::from_bytes(&[seed; 32])
    }

    fn proposal() -> IntentProposal {
        let issuer = key(1);
        let mut intent = Intent {
            evidence: None,
            intent_id: 91,
            issuer: issuer.public(),
            cell_epoch: CellEpoch::new(5),
            ops: vec![IntentOp {
                op: 3,
                args: Bytes::from_static(b"trade"),
            }],
            attestations: Vec::new(),
            signature: key(9).sign(b"placeholder"),
        };
        intent.sign(&issuer);
        IntentProposal {
            intent,
            parties: vec![issuer.public()],
            context_refs: Vec::new(),
        }
    }

    #[test]
    fn a_party_refuses_without_signing() {
        let mut proposal = proposal();
        let witness = key(2);
        proposal.parties.push(witness.public());

        let response = decide_proposal(&proposal, Some(&witness), Ok(()));
        assert_eq!(
            response.verdict,
            AttestationVerdict::Refused(AttestationRefusalReason::Party)
        );
    }

    #[test]
    fn an_honest_non_party_signs_the_d27_preimage() {
        let proposal = proposal();
        let witness = key(2);

        let response = decide_proposal(&proposal, Some(&witness), Ok(()));
        let AttestationVerdict::Attested(attestation) = response.verdict else {
            panic!("a valid non-party proposal should be attested");
        };
        assert!(attestation.verify(&proposal.intent));
        assert!(
            witness
                .public()
                .verify(&proposal.intent.signing_preimage(), &attestation.signature,)
                .is_err(),
            "the same signature must fail in the issuer role"
        );
    }

    #[test]
    fn a_failed_game_precondition_is_an_explicit_refusal() {
        let proposal = proposal();
        let response = decide_proposal(&proposal, Some(&key(2)), Err(17));
        assert_eq!(
            response.verdict,
            AttestationVerdict::Refused(AttestationRefusalReason::PlausibilityFailed(17))
        );
    }
}
